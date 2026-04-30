//! DHCPv4 client state machine.
//!
//! Uses a dual-socket architecture:
//! - DORA phase (before IP): AF_PACKET/SOCK_DGRAM packet socket with manual IP+UDP framing.
//!   The kernel does not deliver broadcast UDP to AF_INET sockets on interfaces with no IP,
//!   so a packet socket is required to receive DHCPOFFER/DHCPACK during initial acquisition.
//! - Renewal/Rebind/Release: AF_INET/SOCK_DGRAM UDP socket bound to the acquired client IP.

use std::ffi::CString;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dhcproto::v4::{DhcpOption, Flags, Message, MessageType, OptionCode};
use dhcproto::{Decodable, Decoder, Encodable, Encoder};
use futures::TryStreamExt;
use netlink_packet_route::link::LinkAttribute;
use rtnetlink::new_connection;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::unix::AsyncFd;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

use tracing::{debug, info};

use netfyr_state::State;

use crate::dhcp::lease::DhcpLease;
use crate::dhcp::{lease_to_state, FactoryEvent, LeaseTimingInfo};
use crate::BackendError;

// ── Constants ─────────────────────────────────────────────────────────────────

const DISCOVER_TIMEOUT: Duration = Duration::from_secs(5);
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const DHCP_CLIENT_PORT: u16 = 68;
const DHCP_SERVER_PORT: u16 = 67;

// RFC 2131 §4.4.5: minimum interval between retransmissions during
// RENEWING and REBINDING states.
const MIN_RETRY_INTERVAL: Duration = Duration::from_secs(60);

// Bounded drain after DHCPACK before dropping the packet socket.
const DRAIN_LIMIT: usize = 10;

// ── Main client task ──────────────────────────────────────────────────────────

/// Entry point for the background DHCP client task.
///
/// Runs the full DHCP state machine: DORA handshake, lease maintenance
/// (renew/rebind/expire), and DHCPRELEASE on stop signal.
pub(crate) async fn run_dhcp_client(
    interface: String,
    policy_name: String,
    priority: u32,
    state_tx: mpsc::Sender<FactoryEvent>,
    shared_state: Arc<Mutex<Option<State>>>,
    stop_rx: oneshot::Receiver<()>,
    lease_timing: Arc<Mutex<Option<LeaseTimingInfo>>>,
) {
    let mut stop_rx = stop_rx;

    // Read the interface MAC address for chaddr field via rtnetlink (not sysfs).
    let mac = match get_interface_mac(&interface).await {
        Ok(m) => m,
        Err(e) => {
            let _ = state_tx
                .send(FactoryEvent::Error {
                    policy_name: policy_name.clone(),
                    error: format!("failed to read MAC address for {interface}: {e}"),
                })
                .await;
            return;
        }
    };

    // Get interface index for the packet socket bind address.
    let ifindex = match get_ifindex(&interface) {
        Ok(idx) => idx,
        Err(e) => {
            let _ = state_tx
                .send(FactoryEvent::Error {
                    policy_name: policy_name.clone(),
                    error: format!("failed to get interface index for {interface}: {e}"),
                })
                .await;
            return;
        }
    };

    let ctx = DhcpContext {
        ifindex,
        mac,
        interface,
        policy_name,
        priority,
        state_tx,
        shared_state,
        lease_timing,
    };
    run_state_machine(ctx, &mut stop_rx).await;
}

// ── State machine ─────────────────────────────────────────────────────────────

/// Context passed to the DHCP state machine. Groups parameters to avoid
/// exceeding clippy's too_many_arguments limit.
struct DhcpContext {
    ifindex: i32,
    mac: [u8; 6],
    interface: String,
    policy_name: String,
    priority: u32,
    state_tx: mpsc::Sender<FactoryEvent>,
    shared_state: Arc<Mutex<Option<State>>>,
    lease_timing: Arc<Mutex<Option<LeaseTimingInfo>>>,
}

async fn run_state_machine(ctx: DhcpContext, stop_rx: &mut oneshot::Receiver<()>) {
    let DhcpContext {
        ifindex,
        mac,
        interface,
        policy_name,
        priority,
        state_tx,
        shared_state,
        lease_timing,
    } = ctx;
    let mut backoff = INITIAL_BACKOFF;

    loop {
        // Create a fresh packet socket for each DORA attempt.
        let pkt_sock = match create_packet_socket(ifindex) {
            Ok(s) => s,
            Err(e) => {
                let _ = state_tx
                    .send(FactoryEvent::Error {
                        policy_name: policy_name.clone(),
                        error: format!("failed to create packet socket: {e}"),
                    })
                    .await;
                return;
            }
        };

        // ── Discovery phase ───────────────────────────────────────────────────
        let xid: u32 = rand::random();
        let discover = build_discover(xid, mac);

        let encoded = match encode_message(&discover) {
            Ok(b) => b,
            Err(e) => {
                let _ = state_tx
                    .send(FactoryEvent::Error {
                        policy_name: policy_name.clone(),
                        error: format!("failed to encode DHCPDISCOVER: {e}"),
                    })
                    .await;
                return;
            }
        };

        let frame = build_ip_udp_frame(&encoded);
        debug!(%interface, "sending DHCPDISCOVER");
        if let Err(e) = send_via_packet_socket(&pkt_sock, ifindex, &frame).await {
            let _ = state_tx
                .send(FactoryEvent::Error {
                    policy_name: policy_name.clone(),
                    error: format!("failed to send DHCPDISCOVER: {e}"),
                })
                .await;
            return;
        }

        // Wait for DHCPOFFER.
        let offer_result = tokio::select! {
            biased;
            _ = &mut *stop_rx => return,
            r = recv_dhcp_from_packet(&pkt_sock, xid, MessageType::Offer, DISCOVER_TIMEOUT) => r,
        };

        let offer = match offer_result {
            Ok(msg) => {
                backoff = INITIAL_BACKOFF;
                msg
            }
            Err(e) => {
                let _ = state_tx
                    .send(FactoryEvent::Error {
                        policy_name: policy_name.clone(),
                        error: format!("DHCP discovery timeout or error: {e}"),
                    })
                    .await;
                let jitter = Duration::from_millis(u64::from(rand::random::<u16>()) % 1000);
                tokio::select! {
                    biased;
                    _ = &mut *stop_rx => return,
                    _ = tokio::time::sleep(backoff + jitter) => {},
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };

        let offered_ip = offer.yiaddr();
        let server_id = extract_server_id(offer.opts()).unwrap_or_else(|| offer.siaddr());
        debug!(%interface, %offered_ip, server = %server_id, "received DHCPOFFER");

        // ── Request phase ─────────────────────────────────────────────────────
        let request = build_request(xid, mac, offered_ip, server_id);
        let encoded = match encode_message(&request) {
            Ok(b) => b,
            Err(e) => {
                let _ = state_tx
                    .send(FactoryEvent::Error {
                        policy_name: policy_name.clone(),
                        error: format!("failed to encode DHCPREQUEST: {e}"),
                    })
                    .await;
                return;
            }
        };

        let frame = build_ip_udp_frame(&encoded);
        debug!(%interface, ip = %offered_ip, server = %server_id, "sending DHCPREQUEST");
        if let Err(e) = send_via_packet_socket(&pkt_sock, ifindex, &frame).await {
            let _ = state_tx
                .send(FactoryEvent::Error {
                    policy_name: policy_name.clone(),
                    error: format!("failed to send DHCPREQUEST: {e}"),
                })
                .await;
            return;
        }

        // Wait for DHCPACK.
        let ack_result = tokio::select! {
            biased;
            _ = &mut *stop_rx => return,
            r = recv_dhcp_from_packet(&pkt_sock, xid, MessageType::Ack, DISCOVER_TIMEOUT) => r,
        };

        let ack = match ack_result {
            Ok(msg) => msg,
            Err(e) => {
                let _ = state_tx
                    .send(FactoryEvent::Error {
                        policy_name: policy_name.clone(),
                        error: format!("DHCPACK not received: {e}"),
                    })
                    .await;
                let jitter = Duration::from_millis(u64::from(rand::random::<u16>()) % 1000);
                tokio::select! {
                    biased;
                    _ = &mut *stop_rx => return,
                    _ = tokio::time::sleep(backoff + jitter) => {},
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };

        let lease = match parse_ack(&ack) {
            Ok(l) => l,
            Err(e) => {
                let _ = state_tx
                    .send(FactoryEvent::Error {
                        policy_name: policy_name.clone(),
                        error: format!("failed to parse DHCPACK: {e}"),
                    })
                    .await;
                let jitter = Duration::from_millis(u64::from(rand::random::<u16>()) % 1000);
                tokio::select! {
                    biased;
                    _ = &mut *stop_rx => return,
                    _ = tokio::time::sleep(backoff + jitter) => {},
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        debug!(%interface, "received DHCPACK");
        log_lease_details(&interface, "acquired", &lease, &ack);

        // DHCPACK received — drain packet socket then transition to UDP socket.
        drain_packet_socket(&pkt_sock);
        drop(pkt_sock);

        let udp_socket = match create_udp_renewal_socket(lease.ip, &interface) {
            Ok(s) => s,
            Err(e) => {
                let _ = state_tx
                    .send(FactoryEvent::Error {
                        policy_name: policy_name.clone(),
                        error: format!("failed to create UDP renewal socket: {e}"),
                    })
                    .await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };

        // Build State and store it.
        let state = lease_to_state(&lease, &interface, &policy_name, priority);
        {
            let mut guard = shared_state.lock().unwrap();
            *guard = Some(state.clone());
        }
        {
            let mut timing_guard = lease_timing.lock().unwrap();
            *timing_guard = Some(LeaseTimingInfo {
                lease_time_secs: lease.lease_time,
                acquired_at: lease.acquired_at,
            });
        }

        if state_tx
            .send(FactoryEvent::LeaseAcquired {
                policy_name: policy_name.clone(),
                state,
            })
            .await
            .is_err()
        {
            return;
        }

        // ── Lease maintenance loop ────────────────────────────────────────────
        let outcome = run_lease_maintenance(
            &udp_socket,
            mac,
            &interface,
            &policy_name,
            priority,
            &state_tx,
            &shared_state,
            &lease_timing,
            stop_rx,
            lease,
        )
        .await;

        match outcome {
            LeaseMaintOutcome::Stop => {
                let mut guard = shared_state.lock().unwrap();
                *guard = None;
                let mut timing_guard = lease_timing.lock().unwrap();
                *timing_guard = None;
                return;
            }
            LeaseMaintOutcome::Expired => {
                {
                    let mut guard = shared_state.lock().unwrap();
                    *guard = Some(super::pending_state(&interface, &policy_name, priority));
                }
                {
                    let mut timing_guard = lease_timing.lock().unwrap();
                    *timing_guard = None;
                }
                let _ = state_tx
                    .send(FactoryEvent::LeaseExpired {
                        policy_name: policy_name.clone(),
                    })
                    .await;
                backoff = INITIAL_BACKOFF;
            }
        }
    }
}

// ── Lease maintenance ─────────────────────────────────────────────────────────

enum LeaseMaintOutcome {
    Stop,
    Expired,
}

#[allow(clippy::too_many_arguments)]
async fn run_lease_maintenance(
    socket: &UdpSocket,
    mac: [u8; 6],
    interface: &str,
    policy_name: &str,
    priority: u32,
    state_tx: &mpsc::Sender<FactoryEvent>,
    shared_state: &Arc<Mutex<Option<State>>>,
    lease_timing: &Arc<Mutex<Option<LeaseTimingInfo>>>,
    stop_rx: &mut oneshot::Receiver<()>,
    mut lease: DhcpLease,
) -> LeaseMaintOutcome {
    loop {
        let renewal_wait = lease.time_until_renewal();
        if !renewal_wait.is_zero() {
            tokio::select! {
                biased;
                _ = &mut *stop_rx => {
                    send_release(socket, mac, lease.ip, lease.server_id).await;
                    return LeaseMaintOutcome::Stop;
                }
                _ = tokio::time::sleep(renewal_wait) => {}
            }
        }

        // RFC 2131 §4.4.5: In RENEWING, retransmit DHCPREQUEST (unicast)
        // at one-half the remaining time until T2, min 60 seconds. On
        // reaching T2 without an ACK, transition to REBINDING and
        // retransmit (broadcast) at one-half the remaining lease time,
        // min 60 seconds, until the lease expires.
        debug!(%interface, ip = %lease.ip, server = %lease.server_id, "entering RENEWING state");
        let renewed = 'renewal: {
            // RENEWING: unicast to the original server.
            loop {
                let remaining = lease.time_until_rebind();
                if remaining.is_zero() {
                    break;
                }
                let timeout = (remaining / 2).max(MIN_RETRY_INTERVAL).min(remaining);
                let result = tokio::select! {
                    biased;
                    _ = &mut *stop_rx => {
                        send_release(socket, mac, lease.ip, lease.server_id).await;
                        return LeaseMaintOutcome::Stop;
                    }
                    r = attempt_renewal(socket, mac, &lease, false, timeout) => r,
                };
                if let Some(updated) = result {
                    lease = updated;
                    break 'renewal true;
                }
            }

            // REBINDING: broadcast to any server.
            debug!(%interface, ip = %lease.ip, "entering REBINDING state");
            loop {
                let remaining = lease.time_until_expiry();
                if remaining.is_zero() {
                    break;
                }
                let timeout = (remaining / 2).max(MIN_RETRY_INTERVAL).min(remaining);
                let result = tokio::select! {
                    biased;
                    _ = &mut *stop_rx => {
                        send_release(socket, mac, lease.ip, lease.server_id).await;
                        return LeaseMaintOutcome::Stop;
                    }
                    r = attempt_renewal(socket, mac, &lease, true, timeout) => r,
                };
                if let Some(updated) = result {
                    lease = updated;
                    break 'renewal true;
                }
            }

            false
        };

        if !renewed {
            info!(%interface, "lease expired, restarting discovery");
            return LeaseMaintOutcome::Expired;
        }

        info!(
            %interface,
            ip = %lease.ip,
            server = %lease.server_id,
            lease_time = lease.lease_time,
            t1 = lease.renewal_time,
            t2 = lease.rebind_time,
            "lease renewed",
        );
        let state = lease_to_state(&lease, interface, policy_name, priority);
        {
            let mut guard = shared_state.lock().unwrap();
            *guard = Some(state.clone());
        }
        {
            let mut timing_guard = lease_timing.lock().unwrap();
            *timing_guard = Some(LeaseTimingInfo {
                lease_time_secs: lease.lease_time,
                acquired_at: lease.acquired_at,
            });
        }
        let _ = state_tx
            .send(FactoryEvent::LeaseRenewed {
                policy_name: policy_name.to_string(),
                state,
            })
            .await;
    }
}

/// Attempt a DHCP renewal or rebinding.
///
/// `broadcast = false` → unicast DHCPREQUEST to `lease.server_id`.
/// `broadcast = true`  → broadcast DHCPREQUEST to 255.255.255.255.
async fn attempt_renewal(
    socket: &UdpSocket,
    mac: [u8; 6],
    lease: &DhcpLease,
    broadcast: bool,
    timeout: Duration,
) -> Option<DhcpLease> {
    let xid: u32 = rand::random();
    let request = build_renew_request(xid, mac, lease.ip, lease.server_id);
    let encoded = encode_message(&request).ok()?;

    let mode = if broadcast { "broadcast" } else { "unicast" };
    debug!(ip = %lease.ip, server = %lease.server_id, %mode, "sending renewal DHCPREQUEST");

    let dest: SocketAddr = if broadcast {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, DHCP_SERVER_PORT))
    } else {
        SocketAddr::V4(SocketAddrV4::new(lease.server_id, DHCP_SERVER_PORT))
    };

    socket.send_to(&encoded, dest).await.ok()?;

    recv_dhcp_response(socket, xid, MessageType::Ack, timeout)
        .await
        .ok()
        .and_then(|ack| parse_ack(&ack).ok())
}

/// Send a DHCPRELEASE to the server.
async fn send_release(socket: &UdpSocket, mac: [u8; 6], client_ip: Ipv4Addr, server_id: Ipv4Addr) {
    debug!(ip = %client_ip, server = %server_id, "sending DHCPRELEASE");
    let release = build_release(mac, client_ip, server_id);
    if let Ok(encoded) = encode_message(&release) {
        let dest: SocketAddr = SocketAddr::V4(SocketAddrV4::new(server_id, DHCP_SERVER_PORT));
        let _ = socket.send_to(&encoded, dest).await;
    }
}

// ── UDP receive helper (renewal phase) ───────────────────────────────────────

/// Receive and validate a DHCP response on a UDP socket (renewal/rebind phase).
async fn recv_dhcp_response(
    socket: &UdpSocket,
    xid: u32,
    expected_type: MessageType,
    timeout: Duration,
) -> Result<Message, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = vec![0u8; 1500];

    loop {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .ok_or_else(|| "DHCP response timeout".to_string())?;

        let n = tokio::time::timeout(remaining, socket.recv_from(&mut buf))
            .await
            .map_err(|_| "DHCP response timeout".to_string())?
            .map_err(|e| format!("socket recv error: {e}"))?
            .0;

        let msg = Message::decode(&mut Decoder::new(&buf[..n]))
            .map_err(|e| format!("failed to decode DHCP message: {e}"))?;

        if msg.xid() != xid {
            continue;
        }

        let msg_type = extract_msg_type(msg.opts());
        if msg_type == Some(MessageType::Nak) {
            return Err("received DHCPNAK from server".to_string());
        }

        if msg_type == Some(expected_type) {
            return Ok(msg);
        }
    }
}

// ── Packet socket receive helper (DORA phase) ─────────────────────────────────

/// Receive and validate a DHCP message from a packet socket (DORA phase).
///
/// Parses and filters IP+UDP+DHCP packets from the raw IP stream delivered by
/// `AF_PACKET/SOCK_DGRAM`. Drops non-UDP, fragmented, wrong-port, or non-BOOTREPLY packets.
async fn recv_dhcp_from_packet(
    async_fd: &AsyncFd<OwnedFd>,
    xid: u32,
    expected_type: MessageType,
    timeout: Duration,
) -> Result<Message, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = vec![0u8; 2048];

    loop {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .ok_or_else(|| "DHCP response timeout".to_string())?;

        let n = tokio::time::timeout(remaining, recv_from_packet_socket(async_fd, &mut buf))
            .await
            .map_err(|_| "DHCP response timeout".to_string())?
            .map_err(|e| format!("packet socket recv error: {e}"))?;

        if n < 28 {
            continue;
        }

        // IPv4 version check.
        if (buf[0] >> 4) != 4 {
            continue;
        }

        let ihl = (buf[0] & 0x0F) as usize * 4;
        if ihl < 20 || n < ihl + 8 {
            continue;
        }

        // Protocol must be UDP (17).
        if buf[9] != 17 {
            continue;
        }

        // Drop fragmented packets (MF flag or nonzero fragment offset).
        let flags_frag = u16::from_be_bytes([buf[6], buf[7]]);
        if (flags_frag & 0x1FFF) != 0 || (flags_frag & 0x2000) != 0 {
            continue;
        }

        // UDP destination port must be 68 (DHCP client).
        let udp_dst_port = u16::from_be_bytes([buf[ihl + 2], buf[ihl + 3]]);
        if udp_dst_port != DHCP_CLIENT_PORT {
            continue;
        }

        let dhcp_start = ihl + 8;
        if n < dhcp_start + 240 {
            continue;
        }

        let payload = &buf[dhcp_start..n];

        // DHCP op must be BOOTREPLY (2).
        if payload[0] != 2 {
            continue;
        }

        // Magic cookie must be 0x63825363.
        if payload[236..240] != [0x63, 0x82, 0x53, 0x63] {
            continue;
        }

        let msg = match Message::decode(&mut Decoder::new(payload)) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if msg.xid() != xid {
            continue;
        }

        let msg_type = extract_msg_type(msg.opts());
        if msg_type == Some(MessageType::Nak) {
            return Err("received DHCPNAK from server".to_string());
        }

        if msg_type == Some(expected_type) {
            return Ok(msg);
        }
    }
}

// ── Option extraction helpers ─────────────────────────────────────────────────

fn extract_msg_type(opts: &dhcproto::v4::DhcpOptions) -> Option<MessageType> {
    match opts.get(OptionCode::MessageType) {
        Some(DhcpOption::MessageType(mt)) => Some(*mt),
        _ => None,
    }
}

fn extract_server_id(opts: &dhcproto::v4::DhcpOptions) -> Option<Ipv4Addr> {
    match opts.get(OptionCode::ServerIdentifier) {
        Some(DhcpOption::ServerIdentifier(ip)) => Some(*ip),
        _ => None,
    }
}

// ── Lease logging ────────────────────────────────────────────────────────────

fn log_lease_details(interface: &str, event: &str, lease: &DhcpLease, ack: &Message) {
    let prefix = lease.subnet_mask_to_prefix();
    let gw = lease
        .gateway
        .map_or_else(|| "none".to_string(), |gw| gw.to_string());
    let dns = if lease.dns_servers.is_empty() {
        "none".to_string()
    } else {
        lease
            .dns_servers
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let search = match ack.opts().get(OptionCode::DomainName) {
        Some(DhcpOption::DomainName(name)) => name.clone(),
        _ => String::new(),
    };
    if search.is_empty() {
        info!(
            %interface,
            address = %format_args!("{}/{prefix}", lease.ip),
            server = %lease.server_id,
            lease_time = lease.lease_time,
            t1 = lease.renewal_time,
            t2 = lease.rebind_time,
            %gw,
            %dns,
            "lease {event}",
        );
    } else {
        info!(
            %interface,
            address = %format_args!("{}/{prefix}", lease.ip),
            server = %lease.server_id,
            lease_time = lease.lease_time,
            t1 = lease.renewal_time,
            t2 = lease.rebind_time,
            %gw,
            %dns,
            %search,
            "lease {event}",
        );
    }
}

// ── Packet builders ───────────────────────────────────────────────────────────

fn build_discover(xid: u32, mac: [u8; 6]) -> Message {
    let mut msg = Message::default();
    msg.set_xid(xid)
        .set_flags(Flags::default().set_broadcast())
        .set_chaddr(&mac);
    msg.opts_mut()
        .insert(DhcpOption::MessageType(MessageType::Discover));
    msg.opts_mut()
        .insert(DhcpOption::ClientIdentifier(mac.to_vec()));
    msg.opts_mut()
        .insert(DhcpOption::ParameterRequestList(vec![
            OptionCode::SubnetMask,
            OptionCode::Router,
            OptionCode::DomainNameServer,
            OptionCode::DomainName,
            OptionCode::AddressLeaseTime,
            OptionCode::ServerIdentifier,
            OptionCode::Renewal,
            OptionCode::Rebinding,
        ]));
    msg
}

fn build_request(
    xid: u32,
    mac: [u8; 6],
    requested_ip: Ipv4Addr,
    server_id: Ipv4Addr,
) -> Message {
    let mut msg = Message::default();
    msg.set_xid(xid)
        .set_flags(Flags::default().set_broadcast())
        .set_chaddr(&mac);
    msg.opts_mut()
        .insert(DhcpOption::MessageType(MessageType::Request));
    msg.opts_mut()
        .insert(DhcpOption::RequestedIpAddress(requested_ip));
    msg.opts_mut()
        .insert(DhcpOption::ServerIdentifier(server_id));
    msg.opts_mut()
        .insert(DhcpOption::ClientIdentifier(mac.to_vec()));
    msg.opts_mut()
        .insert(DhcpOption::ParameterRequestList(vec![
            OptionCode::SubnetMask,
            OptionCode::Router,
            OptionCode::DomainNameServer,
            OptionCode::DomainName,
            OptionCode::AddressLeaseTime,
            OptionCode::ServerIdentifier,
            OptionCode::Renewal,
            OptionCode::Rebinding,
        ]));
    msg
}

/// Build a DHCPREQUEST for renewal/rebinding. Sets `ciaddr` to the current IP.
fn build_renew_request(xid: u32, mac: [u8; 6], ciaddr: Ipv4Addr, server_id: Ipv4Addr) -> Message {
    let mut msg = Message::default();
    msg.set_xid(xid).set_ciaddr(ciaddr).set_chaddr(&mac);
    msg.opts_mut()
        .insert(DhcpOption::MessageType(MessageType::Request));
    msg.opts_mut()
        .insert(DhcpOption::ServerIdentifier(server_id));
    msg.opts_mut()
        .insert(DhcpOption::ClientIdentifier(mac.to_vec()));
    msg
}

fn build_release(mac: [u8; 6], client_ip: Ipv4Addr, server_id: Ipv4Addr) -> Message {
    let mut msg = Message::default();
    msg.set_ciaddr(client_ip).set_chaddr(&mac);
    msg.opts_mut()
        .insert(DhcpOption::MessageType(MessageType::Release));
    msg.opts_mut()
        .insert(DhcpOption::ServerIdentifier(server_id));
    msg.opts_mut()
        .insert(DhcpOption::ClientIdentifier(mac.to_vec()));
    msg
}

// ── Parsing helpers ───────────────────────────────────────────────────────────

fn parse_ack(msg: &Message) -> Result<DhcpLease, String> {
    let ip = msg.yiaddr();
    if ip.is_unspecified() {
        return Err("DHCPACK has no yiaddr (your IP)".to_string());
    }

    let opts = msg.opts();

    let lease_time = extract_u32(opts, OptionCode::AddressLeaseTime)
        .ok_or_else(|| "DHCPACK missing lease time (option 51)".to_string())?;

    let subnet_mask = extract_ipv4(opts, OptionCode::SubnetMask)
        .unwrap_or_else(|| Ipv4Addr::new(255, 255, 255, 0));

    let gateway = match opts.get(OptionCode::Router) {
        Some(DhcpOption::Router(routers)) => routers.first().copied(),
        _ => None,
    };

    let dns_servers = match opts.get(OptionCode::DomainNameServer) {
        Some(DhcpOption::DomainNameServer(servers)) => servers.clone(),
        _ => vec![],
    };

    let server_id = extract_server_id(opts).unwrap_or_else(|| msg.siaddr());

    let renewal_time = extract_u32(opts, OptionCode::Renewal).unwrap_or(lease_time / 2);
    let rebind_time = extract_u32(opts, OptionCode::Rebinding).unwrap_or(lease_time * 7 / 8);

    Ok(DhcpLease {
        ip,
        subnet_mask,
        gateway,
        dns_servers,
        lease_time,
        renewal_time,
        rebind_time,
        server_id,
        acquired_at: Instant::now(),
    })
}

fn extract_u32(opts: &dhcproto::v4::DhcpOptions, code: OptionCode) -> Option<u32> {
    match opts.get(code) {
        Some(DhcpOption::AddressLeaseTime(t)) => Some(*t),
        Some(DhcpOption::Renewal(t)) => Some(*t),
        Some(DhcpOption::Rebinding(t)) => Some(*t),
        _ => None,
    }
}

fn extract_ipv4(opts: &dhcproto::v4::DhcpOptions, code: OptionCode) -> Option<Ipv4Addr> {
    match opts.get(code) {
        Some(DhcpOption::SubnetMask(ip)) => Some(*ip),
        Some(DhcpOption::ServerIdentifier(ip)) => Some(*ip),
        Some(DhcpOption::RequestedIpAddress(ip)) => Some(*ip),
        _ => None,
    }
}

// ── IP/UDP framing ────────────────────────────────────────────────────────────

/// RFC 1071 one's-complement checksum over `data`.
fn ip_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u32::from(u16::from_be_bytes([data[i], data[i + 1]]));
        i += 2;
    }
    if i < data.len() {
        // Odd byte: treat as MSB of a zero-padded 16-bit word.
        sum += u32::from(data[i]) << 8;
    }
    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// UDP checksum over the pseudo-header + UDP header + payload.
fn udp_checksum(src_ip: Ipv4Addr, dst_ip: Ipv4Addr, udp_header_and_payload: &[u8]) -> u16 {
    let udp_len = udp_header_and_payload.len() as u16;
    let src = src_ip.octets();
    let dst = dst_ip.octets();
    let pseudo: [u8; 12] = [
        src[0], src[1], src[2], src[3],
        dst[0], dst[1], dst[2], dst[3],
        0, 17,
        (udp_len >> 8) as u8, (udp_len & 0xFF) as u8,
    ];
    let mut combined = Vec::with_capacity(12 + udp_header_and_payload.len());
    combined.extend_from_slice(&pseudo);
    combined.extend_from_slice(udp_header_and_payload);
    let csum = ip_checksum(&combined);
    // Per RFC 768: transmitted checksum of 0 means "no checksum"; encode as 0xFFFF.
    if csum == 0 { 0xFFFF } else { csum }
}

/// Build a raw IP+UDP frame carrying `dhcp_payload` (src 0.0.0.0:68 → dst 255.255.255.255:67).
fn build_ip_udp_frame(dhcp_payload: &[u8]) -> Vec<u8> {
    let udp_len = 8u16 + dhcp_payload.len() as u16;
    let ip_total_len = 20u16 + udp_len;
    let ident: u16 = rand::random();

    let mut frame = vec![0u8; 28 + dhcp_payload.len()];

    // IP header (20 bytes).
    frame[0] = 0x45; // version=4, IHL=5
    frame[1] = 0x00; // DSCP/ECN
    frame[2] = (ip_total_len >> 8) as u8;
    frame[3] = (ip_total_len & 0xFF) as u8;
    frame[4] = (ident >> 8) as u8;
    frame[5] = (ident & 0xFF) as u8;
    // frame[6..8] = 0x0000 (no flags, no fragmentation)
    frame[8] = 64;   // TTL
    frame[9] = 17;   // protocol = UDP
    // frame[10..12] = checksum placeholder (zero)
    // frame[12..16] = src IP 0.0.0.0 (already zero)
    frame[16] = 255; // dst IP 255.255.255.255
    frame[17] = 255;
    frame[18] = 255;
    frame[19] = 255;

    let ip_csum = ip_checksum(&frame[0..20]);
    frame[10] = (ip_csum >> 8) as u8;
    frame[11] = (ip_csum & 0xFF) as u8;

    // UDP header (8 bytes at offset 20).
    frame[20] = (DHCP_CLIENT_PORT >> 8) as u8;
    frame[21] = (DHCP_CLIENT_PORT & 0xFF) as u8;
    frame[22] = (DHCP_SERVER_PORT >> 8) as u8;
    frame[23] = (DHCP_SERVER_PORT & 0xFF) as u8;
    frame[24] = (udp_len >> 8) as u8;
    frame[25] = (udp_len & 0xFF) as u8;
    // frame[26..28] = UDP checksum placeholder (zero)

    // DHCP payload at offset 28.
    frame[28..].copy_from_slice(dhcp_payload);

    let udp_csum = udp_checksum(Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST, &frame[20..]);
    frame[26] = (udp_csum >> 8) as u8;
    frame[27] = (udp_csum & 0xFF) as u8;

    frame
}

// ── Socket creation ───────────────────────────────────────────────────────────

/// Get the interface index via `if_nametoindex()` (namespace-aware, unlike sysfs).
fn get_ifindex(interface: &str) -> Result<i32, BackendError> {
    let c_name = CString::new(interface)
        .map_err(|_| BackendError::Internal(format!("invalid interface name: {interface}")))?;
    let idx = unsafe { libc::if_nametoindex(c_name.as_ptr()) };
    if idx == 0 {
        Err(BackendError::Internal(format!(
            "interface not found: {interface}"
        )))
    } else {
        Ok(idx as i32)
    }
}

/// Create an `AF_PACKET/SOCK_DGRAM/ETH_P_IP` socket bound to `ifindex`.
///
/// Uses `SOCK_DGRAM` so the kernel strips/adds Ethernet headers; we only
/// construct IP+UDP headers. The socket is set non-blocking and wrapped in
/// `AsyncFd<OwnedFd>` for integration with tokio's event loop.
fn create_packet_socket(ifindex: i32) -> Result<AsyncFd<OwnedFd>, BackendError> {
    let proto = (libc::ETH_P_IP as u16).to_be() as i32;
    let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_DGRAM, proto) };
    if fd < 0 {
        return Err(BackendError::Internal(format!(
            "AF_PACKET socket creation failed: {}",
            io::Error::last_os_error()
        )));
    }

    let mut sockaddr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    sockaddr.sll_family = libc::AF_PACKET as u16;
    sockaddr.sll_protocol = (libc::ETH_P_IP as u16).to_be();
    sockaddr.sll_ifindex = ifindex;

    let ret = unsafe {
        libc::bind(
            fd,
            &sockaddr as *const libc::sockaddr_ll as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_ll>() as u32,
        )
    };
    if ret < 0 {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(BackendError::Internal(format!(
            "AF_PACKET bind failed: {err}"
        )));
    }

    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(BackendError::Internal(format!(
            "set O_NONBLOCK failed: {err}"
        )));
    }

    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    AsyncFd::new(owned)
        .map_err(|e| BackendError::Internal(format!("AsyncFd wrapping failed: {e}")))
}

/// Create a UDP socket bound to `client_ip:68` for renewal/rebind/release.
fn create_udp_renewal_socket(client_ip: Ipv4Addr, interface: &str) -> Result<UdpSocket, BackendError> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|e| BackendError::Internal(format!("UDP socket creation failed: {e}")))?;

    socket
        .set_reuse_address(true)
        .map_err(|e| BackendError::Internal(format!("SO_REUSEADDR failed: {e}")))?;

    socket
        .set_broadcast(true)
        .map_err(|e| BackendError::Internal(format!("SO_BROADCAST failed: {e}")))?;

    #[cfg(target_os = "linux")]
    socket
        .set_freebind(true)
        .map_err(|e| BackendError::Internal(format!("IP_FREEBIND failed: {e}")))?;

    #[cfg(target_os = "linux")]
    socket
        .bind_device(Some(interface.as_bytes()))
        .map_err(|e| BackendError::Internal(format!("SO_BINDTODEVICE failed: {e}")))?;

    let addr = std::net::SocketAddr::V4(std::net::SocketAddrV4::new(client_ip, DHCP_CLIENT_PORT));
    socket
        .bind(&addr.into())
        .map_err(|e| BackendError::Internal(format!("bind to {client_ip}:{DHCP_CLIENT_PORT} failed: {e}")))?;

    socket
        .set_nonblocking(true)
        .map_err(|e| BackendError::Internal(format!("set_nonblocking failed: {e}")))?;

    let std_socket = std::net::UdpSocket::from(socket);
    UdpSocket::from_std(std_socket)
        .map_err(|e| BackendError::Internal(format!("tokio UdpSocket conversion failed: {e}")))
}

// ── Packet socket I/O ─────────────────────────────────────────────────────────

/// Send `frame` to the broadcast MAC via the packet socket using `sendto()` with `sockaddr_ll`.
async fn send_via_packet_socket(
    async_fd: &AsyncFd<OwnedFd>,
    ifindex: i32,
    frame: &[u8],
) -> Result<(), BackendError> {
    let mut sockaddr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    sockaddr.sll_family = libc::AF_PACKET as u16;
    sockaddr.sll_protocol = (libc::ETH_P_IP as u16).to_be();
    sockaddr.sll_ifindex = ifindex;
    sockaddr.sll_halen = 6;
    sockaddr.sll_addr = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0];

    loop {
        let mut guard = async_fd.writable().await.map_err(|e| {
            BackendError::Internal(format!("packet socket writable() error: {e}"))
        })?;

        let result = guard.try_io(|inner| {
            let ret = unsafe {
                libc::sendto(
                    inner.get_ref().as_raw_fd(),
                    frame.as_ptr() as *const libc::c_void,
                    frame.len(),
                    0,
                    &sockaddr as *const libc::sockaddr_ll as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_ll>() as u32,
                )
            };
            if ret < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(ret)
            }
        });

        match result {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(e)) => {
                return Err(BackendError::Internal(format!("sendto failed: {e}")));
            }
            Err(_would_block) => continue,
        }
    }
}

/// Receive one packet from the packet socket asynchronously.
async fn recv_from_packet_socket(
    async_fd: &AsyncFd<OwnedFd>,
    buf: &mut [u8],
) -> io::Result<usize> {
    loop {
        let mut guard = async_fd.readable().await?;
        let result = guard.try_io(|inner| {
            let ret = unsafe {
                libc::recv(
                    inner.get_ref().as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    0,
                )
            };
            if ret < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(ret as usize)
            }
        });

        match result {
            Ok(Ok(n)) => return Ok(n),
            Ok(Err(e)) => return Err(e),
            Err(_would_block) => continue,
        }
    }
}

/// Drain up to `DRAIN_LIMIT` packets from the packet socket (non-blocking).
/// Called just before closing the packet socket after DHCPACK to discard in-flight data.
fn drain_packet_socket(async_fd: &AsyncFd<OwnedFd>) {
    let mut buf = [0u8; 2048];
    for _ in 0..DRAIN_LIMIT {
        let fd = async_fd.get_ref().as_raw_fd();
        let ret = unsafe {
            libc::recv(
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if ret <= 0 {
            break;
        }
    }
}

// ── MAC address discovery ─────────────────────────────────────────────────────

/// Read the interface's MAC address via rtnetlink.
///
/// Uses netlink instead of `/sys/class/net/` because sysfs is not
/// network-namespace-aware in all environments (e.g., containers, unshare).
async fn get_interface_mac(interface: &str) -> Result<[u8; 6], BackendError> {
    let (conn, handle, _) = new_connection()
        .map_err(|e| BackendError::Internal(format!("netlink connection failed: {e}")))?;
    tokio::spawn(conn);

    let mut links = handle
        .link()
        .get()
        .match_name(interface.to_string())
        .execute();

    let msg = links
        .try_next()
        .await
        .map_err(|e| {
            BackendError::Internal(format!("netlink query failed for {interface}: {e}"))
        })?
        .ok_or_else(|| BackendError::Internal(format!("interface not found: {interface}")))?;

    for attr in &msg.attributes {
        if let LinkAttribute::Address(bytes) = attr {
            if bytes.len() == 6 {
                let mut mac = [0u8; 6];
                mac.copy_from_slice(bytes);
                return Ok(mac);
            }
        }
    }

    Err(BackendError::Internal(format!(
        "no MAC address found for interface {interface}"
    )))
}

// ── Encoding helper ───────────────────────────────────────────────────────────

fn encode_message(msg: &Message) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let mut enc = Encoder::new(&mut buf);
    msg.encode(&mut enc)
        .map_err(|e| format!("DHCP encode error: {e}"))?;
    Ok(buf)
}
