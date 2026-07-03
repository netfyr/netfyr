//! DHCPv6 protocol state machine (RFC 8415).
// SPEC-412 will call into this module; until then all items are unused.
#![allow(dead_code)]
//!
//! Implements both stateful (IA_NA: Solicit → Advertise → Request → Reply →
//! Renew/Rebind/Release) and stateless (Information-Request → Reply → refresh)
//! modes. Message encoding/decoding is done inline using TLV; no external
//! crate is needed because the wire format is a 4-byte header plus simple
//! option TLVs.

use std::net::{IpAddr, Ipv6Addr, SocketAddrV6};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::TryStreamExt;
use netlink_packet_route::address::{AddressAttribute, AddressFlags, AddressHeaderFlags};
use netlink_packet_route::AddressFamily;
use rand::Rng;
use rtnetlink::new_connection;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::duid::{encode_duid, DuidLlt};
use super::lease::{Dhcpv6Address, Dhcpv6Lease};
use super::Dhcpv6Result;

// ── Constants ────────────────────────────────────────────────────────────────

const DHCPV6_CLIENT_PORT: u16 = 546;
const DHCPV6_SERVER_PORT: u16 = 547;

/// All_DHCP_Relay_Agents_and_Servers multicast address (RFC 8415 §7.1).
const DHCPV6_ALL_SERVERS: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 1, 2);

const SOLICIT_TIMEOUT: Duration = Duration::from_secs(5);
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(120);
/// Default information refresh interval per RFC 8415 §7.6.
const DEFAULT_INFO_REFRESH: u32 = 1800;

// Message type codes per RFC 8415 §7.3.
const MSG_SOLICIT: u8 = 1;
const MSG_ADVERTISE: u8 = 2;
const MSG_REQUEST: u8 = 3;
const MSG_RENEW: u8 = 5;
const MSG_REBIND: u8 = 6;
const MSG_REPLY: u8 = 7;
const MSG_RELEASE: u8 = 8;
const MSG_INFORMATION_REQUEST: u8 = 11;

// Option codes per RFC 8415 §21 and RFC 3646.
const OPT_CLIENT_ID: u16 = 1;
const OPT_SERVER_ID: u16 = 2;
const OPT_IA_NA: u16 = 3;
const OPT_IA_ADDR: u16 = 5;
const OPT_OPTION_REQUEST: u16 = 6;
const OPT_ELAPSED_TIME: u16 = 8;
const OPT_DNS_SERVERS: u16 = 23;
const OPT_DNS_SEARCH: u16 = 24;
const OPT_INFO_REFRESH_TIME: u16 = 32;

// ── Context ──────────────────────────────────────────────────────────────────

/// Parameters shared across protocol functions.
pub(super) struct Dhcpv6Context {
    pub interface: String,
    pub stateful: bool,
    /// Interface Association Identifier derived from ifindex.
    pub iaid: u32,
    pub duid: DuidLlt,
    /// Interface's link-local address; used as UDP source.
    pub link_local: Ipv6Addr,
    /// Interface index; used as IPv6 scope_id for link-local socket.
    pub ifindex: u32,
    pub socket: UdpSocket,
    pub result_tx: mpsc::Sender<Dhcpv6Result>,
    pub lease: Arc<Mutex<Option<Dhcpv6Lease>>>,
}

// ── Parsed reply data (internal) ─────────────────────────────────────────────

struct ParsedReply {
    server_duid: Vec<u8>,
    server_addr: Ipv6Addr,
    addresses: Vec<Dhcpv6Address>,
    dns_servers: Vec<Ipv6Addr>,
    dns_search: Vec<String>,
    t1: u32,
    t2: u32,
    info_refresh_time: Option<u32>,
}

enum LeaseMaintOutcome {
    Renewed(Dhcpv6Lease),
    Expired,
    Stopped,
}

// ── Top-level task ───────────────────────────────────────────────────────────

/// Background task: runs the full DHCPv6 protocol state machine.
pub(super) async fn run_dhcpv6_client(ctx: Dhcpv6Context, mut stop_rx: oneshot::Receiver<()>) {
    if ctx.stateful {
        run_stateful(ctx, &mut stop_rx).await;
    } else {
        run_stateless(ctx, &mut stop_rx).await;
    }
}

// ── Stateful (IA_NA) state machine ──────────────────────────────────────────

async fn run_stateful(ctx: Dhcpv6Context, stop_rx: &mut oneshot::Receiver<()>) {
    let mut backoff = INITIAL_BACKOFF;

    loop {
        // ── Solicit/Advertise/Request/Reply ────────────────────────────────────

        let result = tokio::select! {
            biased;
            _ = &mut *stop_rx => return,
            r = do_stateful_acquire(&ctx, SOLICIT_TIMEOUT) => r,
        };

        let lease = match result {
            Ok(lease) => {
                backoff = INITIAL_BACKOFF;
                *ctx.lease.lock().unwrap() = Some(lease.clone());
                let _ = ctx.result_tx.send(Dhcpv6Result::Acquired(lease.clone())).await;
                info!(interface = ctx.interface, "dhcpv6: stateful lease acquired");
                lease
            }
            Err(e) => {
                warn!(interface = ctx.interface, error = %e, "dhcpv6: acquisition failed");
                let _ = ctx.result_tx.send(Dhcpv6Result::Error(e)).await;
                let jitter = Duration::from_millis(rand::rng().random_range(0..1000));
                let sleep_dur = (backoff + jitter).min(MAX_BACKOFF);
                backoff = (backoff * 2).min(MAX_BACKOFF);
                let sleep = tokio::time::sleep(sleep_dur);
                tokio::pin!(sleep);
                tokio::select! {
                    biased;
                    _ = &mut *stop_rx => return,
                    _ = sleep => {}
                }
                continue;
            }
        };

        // ── Lease maintenance: inner loop keeps renewing until expired/stopped ──

        let mut current_lease = lease;
        loop {
            match run_lease_maintenance(&ctx, stop_rx, current_lease).await {
                LeaseMaintOutcome::Renewed(new_lease) => {
                    *ctx.lease.lock().unwrap() = Some(new_lease.clone());
                    let _ = ctx.result_tx.send(Dhcpv6Result::Renewed(new_lease.clone())).await;
                    current_lease = new_lease; // continue maintenance on renewed lease
                }
                LeaseMaintOutcome::Expired => {
                    *ctx.lease.lock().unwrap() = None;
                    let _ = ctx.result_tx.send(Dhcpv6Result::Expired).await;
                    break; // restart from Solicit
                }
                LeaseMaintOutcome::Stopped => {
                    return;
                }
            }
        }
    }
}

/// Perform the four-message Solicit/Advertise/Request/Reply exchange.
async fn do_stateful_acquire(
    ctx: &Dhcpv6Context,
    timeout: Duration,
) -> Result<Dhcpv6Lease, String> {
    // 1. Send Solicit.
    let tx_id = random_tx_id();
    send_solicit(ctx, tx_id).await?;

    // 2. Receive Advertise.
    let advertise = recv_message(ctx, MSG_ADVERTISE, tx_id, timeout).await?;

    // 3. Send Request.
    let req_tx_id = random_tx_id();
    send_request(ctx, req_tx_id, &advertise.server_duid, &advertise.addresses).await?;

    // 4. Receive Reply.
    let reply = recv_message(ctx, MSG_REPLY, req_tx_id, timeout).await?;

    build_lease_from_reply(reply, advertise.server_addr)
}

async fn send_solicit(ctx: &Dhcpv6Context, tx_id: [u8; 3]) -> Result<(), String> {
    let duid_bytes = encode_duid(&ctx.duid);
    let options = vec![
        Dhcpv6Option::ClientId(duid_bytes),
        Dhcpv6Option::IaNa {
            iaid: ctx.iaid,
            t1: 0,
            t2: 0,
            addresses: vec![],
        },
        Dhcpv6Option::OptionRequest(vec![OPT_DNS_SERVERS, OPT_DNS_SEARCH]),
        Dhcpv6Option::ElapsedTime(0),
    ];
    let msg = encode_message(MSG_SOLICIT, tx_id, &options);
    send_to_all_servers(ctx, &msg).await
}

async fn send_request(
    ctx: &Dhcpv6Context,
    tx_id: [u8; 3],
    server_duid: &[u8],
    addresses: &[Dhcpv6Address],
) -> Result<(), String> {
    let duid_bytes = encode_duid(&ctx.duid);
    let options = vec![
        Dhcpv6Option::ClientId(duid_bytes),
        Dhcpv6Option::ServerId(server_duid.to_vec()),
        Dhcpv6Option::IaNa {
            iaid: ctx.iaid,
            t1: 0,
            t2: 0,
            addresses: addresses.to_vec(),
        },
        Dhcpv6Option::OptionRequest(vec![OPT_DNS_SERVERS, OPT_DNS_SEARCH]),
        Dhcpv6Option::ElapsedTime(0),
    ];
    let msg = encode_message(MSG_REQUEST, tx_id, &options);
    send_to_all_servers(ctx, &msg).await
}

/// Manage an active stateful lease: Renew at T1, Rebind at T2, Expired if
/// neither succeeds. Handles stop signals and sends Release before exiting.
async fn run_lease_maintenance(
    ctx: &Dhcpv6Context,
    stop_rx: &mut oneshot::Receiver<()>,
    lease: Dhcpv6Lease,
) -> LeaseMaintOutcome {
    let renewal_in = lease.time_until_renewal();
    let rebind_in = lease.time_until_rebind();
    let expiry_in = lease.time_until_expiry();

    if expiry_in.is_zero() {
        return LeaseMaintOutcome::Expired;
    }

    // Wait for T1 (or stop).
    if !renewal_in.is_zero() {
        let sleep = tokio::time::sleep(renewal_in);
        tokio::pin!(sleep);
        tokio::select! {
            biased;
            _ = &mut *stop_rx => {
                send_release(ctx, &lease).await;
                *ctx.lease.lock().unwrap() = None;
                return LeaseMaintOutcome::Stopped;
            }
            _ = sleep => {}
        }
    }

    // T1 reached: attempt Renew (unicast to server).
    match send_renew(ctx, &lease).await {
        Ok(new_lease) => return LeaseMaintOutcome::Renewed(new_lease),
        Err(e) => {
            warn!(interface = ctx.interface, error = %e, "dhcpv6: renew failed, waiting for T2");
        }
    }

    // Wait for T2 (or stop).
    let rebind_remaining = lease.time_until_rebind();
    if !rebind_in.is_zero() && !rebind_remaining.is_zero() {
        let sleep = tokio::time::sleep(rebind_remaining);
        tokio::pin!(sleep);
        tokio::select! {
            biased;
            _ = &mut *stop_rx => {
                send_release(ctx, &lease).await;
                *ctx.lease.lock().unwrap() = None;
                return LeaseMaintOutcome::Stopped;
            }
            _ = sleep => {}
        }
    }

    // T2 reached: attempt Rebind (multicast).
    match send_rebind(ctx, &lease).await {
        Ok(new_lease) => return LeaseMaintOutcome::Renewed(new_lease),
        Err(e) => {
            warn!(interface = ctx.interface, error = %e, "dhcpv6: rebind failed; lease expired");
        }
    }

    // Wait for valid_lft to expire (or stop), then report expired.
    let expiry_remaining = lease.time_until_expiry();
    if !expiry_remaining.is_zero() {
        let sleep = tokio::time::sleep(expiry_remaining);
        tokio::pin!(sleep);
        tokio::select! {
            biased;
            _ = &mut *stop_rx => {
                *ctx.lease.lock().unwrap() = None;
                return LeaseMaintOutcome::Stopped;
            }
            _ = sleep => {}
        }
    }

    LeaseMaintOutcome::Expired
}

async fn send_renew(ctx: &Dhcpv6Context, lease: &Dhcpv6Lease) -> Result<Dhcpv6Lease, String> {
    let tx_id = random_tx_id();
    let duid_bytes = encode_duid(&ctx.duid);
    let options = vec![
        Dhcpv6Option::ClientId(duid_bytes),
        Dhcpv6Option::ServerId(lease.server_duid.clone()),
        Dhcpv6Option::IaNa {
            iaid: ctx.iaid,
            t1: 0,
            t2: 0,
            addresses: lease.addresses.clone(),
        },
        Dhcpv6Option::OptionRequest(vec![OPT_DNS_SERVERS, OPT_DNS_SEARCH]),
        Dhcpv6Option::ElapsedTime(0),
    ];
    let msg = encode_message(MSG_RENEW, tx_id, &options);

    // Renew is unicast to the server (RFC 8415 §18.2.10).
    let server_addr = SocketAddrV6::new(lease.server_addr, DHCPV6_SERVER_PORT, 0, ctx.ifindex);
    ctx.socket
        .send_to(&msg, server_addr)
        .await
        .map_err(|e| format!("dhcpv6: renew send failed: {e}"))?;

    let reply = recv_message(ctx, MSG_REPLY, tx_id, SOLICIT_TIMEOUT).await?;
    build_lease_from_reply(reply, lease.server_addr)
}

async fn send_rebind(ctx: &Dhcpv6Context, lease: &Dhcpv6Lease) -> Result<Dhcpv6Lease, String> {
    let tx_id = random_tx_id();
    let duid_bytes = encode_duid(&ctx.duid);
    let options = vec![
        Dhcpv6Option::ClientId(duid_bytes),
        Dhcpv6Option::IaNa {
            iaid: ctx.iaid,
            t1: 0,
            t2: 0,
            addresses: lease.addresses.clone(),
        },
        Dhcpv6Option::OptionRequest(vec![OPT_DNS_SERVERS, OPT_DNS_SEARCH]),
        Dhcpv6Option::ElapsedTime(0),
    ];
    let msg = encode_message(MSG_REBIND, tx_id, &options);
    send_to_all_servers(ctx, &msg).await?;

    let reply = recv_message(ctx, MSG_REPLY, tx_id, SOLICIT_TIMEOUT).await?;
    build_lease_from_reply(reply, lease.server_addr)
}

/// Send Release message (best-effort; errors are ignored).
async fn send_release(ctx: &Dhcpv6Context, lease: &Dhcpv6Lease) {
    let tx_id = random_tx_id();
    let duid_bytes = encode_duid(&ctx.duid);
    let options = vec![
        Dhcpv6Option::ClientId(duid_bytes),
        Dhcpv6Option::ServerId(lease.server_duid.clone()),
        Dhcpv6Option::IaNa {
            iaid: ctx.iaid,
            t1: 0,
            t2: 0,
            addresses: lease.addresses.clone(),
        },
        Dhcpv6Option::ElapsedTime(0),
    ];
    let msg = encode_message(MSG_RELEASE, tx_id, &options);
    let server_addr = SocketAddrV6::new(lease.server_addr, DHCPV6_SERVER_PORT, 0, ctx.ifindex);
    let _ = ctx.socket.send_to(&msg, server_addr).await;
    debug!(interface = ctx.interface, "dhcpv6: release sent");
}

// ── Stateless (Information-Request) state machine ───────────────────────────

async fn run_stateless(ctx: Dhcpv6Context, stop_rx: &mut oneshot::Receiver<()>) {
    let mut backoff = INITIAL_BACKOFF;

    loop {
        let result = tokio::select! {
            biased;
            _ = &mut *stop_rx => return,
            r = do_stateless_acquire(&ctx, SOLICIT_TIMEOUT) => r,
        };

        let lease = match result {
            Ok(lease) => {
                backoff = INITIAL_BACKOFF;
                *ctx.lease.lock().unwrap() = Some(lease.clone());
                let _ = ctx.result_tx.send(Dhcpv6Result::Acquired(lease.clone())).await;
                info!(interface = ctx.interface, "dhcpv6: stateless options acquired");
                lease
            }
            Err(e) => {
                warn!(interface = ctx.interface, error = %e, "dhcpv6: stateless acquisition failed");
                let _ = ctx.result_tx.send(Dhcpv6Result::Error(e)).await;
                let jitter = Duration::from_millis(rand::rng().random_range(0..1000));
                let sleep_dur = (backoff + jitter).min(MAX_BACKOFF);
                backoff = (backoff * 2).min(MAX_BACKOFF);
                let sleep = tokio::time::sleep(sleep_dur);
                tokio::pin!(sleep);
                tokio::select! {
                    biased;
                    _ = &mut *stop_rx => return,
                    _ = sleep => {}
                }
                continue;
            }
        };

        // Inner refresh loop: all subsequent refreshes emit Renewed, not Acquired.
        let mut current_lease = lease;
        loop {
            let refresh_secs =
                current_lease.info_refresh_time.unwrap_or(DEFAULT_INFO_REFRESH) as u64;
            let sleep = tokio::time::sleep(Duration::from_secs(refresh_secs));
            tokio::pin!(sleep);
            tokio::select! {
                biased;
                _ = &mut *stop_rx => return,
                _ = sleep => {}
            }

            match do_stateless_acquire(&ctx, SOLICIT_TIMEOUT).await {
                Ok(new_lease) => {
                    *ctx.lease.lock().unwrap() = Some(new_lease.clone());
                    let _ = ctx.result_tx.send(Dhcpv6Result::Renewed(new_lease.clone())).await;
                    current_lease = new_lease;
                }
                Err(e) => {
                    warn!(interface = ctx.interface, error = %e, "dhcpv6: stateless refresh failed");
                    let _ = ctx.result_tx.send(Dhcpv6Result::Error(e)).await;
                    break; // retry initial acquisition with backoff from outer loop
                }
            }
        }
    }
}

async fn do_stateless_acquire(
    ctx: &Dhcpv6Context,
    timeout: Duration,
) -> Result<Dhcpv6Lease, String> {
    let tx_id = random_tx_id();
    send_information_request(ctx, tx_id).await?;
    let reply = recv_message(ctx, MSG_REPLY, tx_id, timeout).await?;
    build_stateless_lease_from_reply(reply)
}

async fn send_information_request(ctx: &Dhcpv6Context, tx_id: [u8; 3]) -> Result<(), String> {
    let duid_bytes = encode_duid(&ctx.duid);
    let options = vec![
        Dhcpv6Option::ClientId(duid_bytes),
        Dhcpv6Option::OptionRequest(vec![OPT_DNS_SERVERS, OPT_DNS_SEARCH, OPT_INFO_REFRESH_TIME]),
        Dhcpv6Option::ElapsedTime(0),
    ];
    let msg = encode_message(MSG_INFORMATION_REQUEST, tx_id, &options);
    send_to_all_servers(ctx, &msg).await
}

// ── Message send/receive helpers ─────────────────────────────────────────────

async fn send_to_all_servers(ctx: &Dhcpv6Context, msg: &[u8]) -> Result<(), String> {
    let dst = SocketAddrV6::new(DHCPV6_ALL_SERVERS, DHCPV6_SERVER_PORT, 0, ctx.ifindex);
    ctx.socket
        .send_to(msg, dst)
        .await
        .map_err(|e| format!("dhcpv6: send failed: {e}"))?;
    Ok(())
}

/// Wait for a DHCPv6 message of `expected_type` with matching `tx_id`.
async fn recv_message(
    ctx: &Dhcpv6Context,
    expected_type: u8,
    tx_id: [u8; 3],
    timeout: Duration,
) -> Result<ParsedReply, String> {
    let deadline = Instant::now() + timeout;
    let mut buf = vec![0u8; 1500];

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "dhcpv6: timeout waiting for message type {expected_type}"
            ));
        }

        let (n, src) = tokio::time::timeout(remaining, ctx.socket.recv_from(&mut buf))
            .await
            .map_err(|_| format!("dhcpv6: timeout waiting for message type {expected_type}"))?
            .map_err(|e| format!("dhcpv6: recv failed: {e}"))?;

        let data = &buf[..n];
        if data.len() < 4 {
            continue;
        }

        let msg_type = data[0];
        let msg_tx_id = [data[1], data[2], data[3]];

        if msg_type != expected_type || msg_tx_id != tx_id {
            continue;
        }

        let options = decode_options(&data[4..]);
        let src_ip = match src {
            std::net::SocketAddr::V6(v6) => *v6.ip(),
            _ => Ipv6Addr::UNSPECIFIED,
        };

        return parse_reply_from_options(options, src_ip);
    }
}

// ── Lease construction ───────────────────────────────────────────────────────

fn build_lease_from_reply(reply: ParsedReply, server_addr: Ipv6Addr) -> Result<Dhcpv6Lease, String> {
    if reply.addresses.is_empty() {
        return Err("dhcpv6: server reply contained no IA_NA addresses".to_string());
    }
    let (t1, t2) = Dhcpv6Lease::compute_t1_t2(&reply.addresses, reply.t1, reply.t2);
    Ok(Dhcpv6Lease {
        addresses: reply.addresses,
        dns_servers: reply.dns_servers,
        dns_search: reply.dns_search,
        t1,
        t2,
        server_duid: reply.server_duid,
        server_addr,
        info_refresh_time: None,
        acquired_at: std::time::Instant::now(),
    })
}

fn build_stateless_lease_from_reply(reply: ParsedReply) -> Result<Dhcpv6Lease, String> {
    Ok(Dhcpv6Lease {
        addresses: vec![],
        dns_servers: reply.dns_servers,
        dns_search: reply.dns_search,
        t1: 0,
        t2: 0,
        server_duid: reply.server_duid,
        server_addr: reply.server_addr,
        info_refresh_time: reply.info_refresh_time,
        acquired_at: std::time::Instant::now(),
    })
}

// ── Socket setup ─────────────────────────────────────────────────────────────

/// Create a DHCPv6 UDP socket bound to `(link_local, 546)` on `interface`.
pub(super) fn create_dhcpv6_socket(
    link_local: Ipv6Addr,
    interface: &str,
    ifindex: u32,
) -> Result<UdpSocket, String> {
    let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|e| format!("dhcpv6: socket create failed: {e}"))?;

    sock.set_reuse_address(true)
        .map_err(|e| format!("dhcpv6: SO_REUSEADDR failed: {e}"))?;

    sock.bind_device(Some(interface.as_bytes()))
        .map_err(|e| format!("dhcpv6: SO_BINDTODEVICE failed: {e}"))?;

    sock.set_multicast_if_v6(ifindex)
        .map_err(|e| format!("dhcpv6: IPV6_MULTICAST_IF failed: {e}"))?;

    let bind_addr = SocketAddrV6::new(link_local, DHCPV6_CLIENT_PORT, 0, ifindex);
    sock.bind(&bind_addr.into())
        .map_err(|e| format!("dhcpv6: bind to {bind_addr} failed: {e}"))?;

    sock.set_nonblocking(true)
        .map_err(|e| format!("dhcpv6: set_nonblocking failed: {e}"))?;

    let std_sock: std::net::UdpSocket = sock.into();
    UdpSocket::from_std(std_sock).map_err(|e| format!("dhcpv6: tokio UdpSocket failed: {e}"))
}

// ── Link-local address discovery ─────────────────────────────────────────────

/// Find a non-tentative link-local address on `interface` via rtnetlink.
///
/// Returns `(address, ifindex)`. Returns `Err` if no such address exists.
pub(super) async fn find_link_local(interface: &str) -> Result<(Ipv6Addr, u32), String> {
    let (conn, handle, _) = new_connection()
        .map_err(|e| format!("dhcpv6: netlink connection failed: {e}"))?;
    tokio::spawn(conn);

    // Get ifindex via link query.
    let mut links = handle
        .link()
        .get()
        .match_name(interface.to_string())
        .execute();
    let link = links
        .try_next()
        .await
        .map_err(|e| format!("dhcpv6: link query failed: {e}"))?
        .ok_or_else(|| format!("dhcpv6: interface not found: {interface}"))?;
    let ifindex = link.header.index;

    // Dump IPv6 addresses for this interface.
    let mut addrs = handle.address().get().execute();
    while let Some(msg) = addrs
        .try_next()
        .await
        .map_err(|e| format!("dhcpv6: address dump failed: {e}"))?
    {
        if msg.header.index != ifindex {
            continue;
        }
        if msg.header.family != AddressFamily::Inet6 {
            continue;
        }

        let mut addr_v6: Option<Ipv6Addr> = None;
        let mut is_tentative = false;
        let mut has_ifa_flags_attr = false;

        for attr in &msg.attributes {
            match attr {
                AddressAttribute::Address(IpAddr::V6(v6)) => {
                    addr_v6 = Some(*v6);
                }
                AddressAttribute::Flags(flags) => {
                    has_ifa_flags_attr = true;
                    if flags.contains(AddressFlags::Tentative) {
                        is_tentative = true;
                    }
                }
                _ => {}
            }
        }

        // Fall back to header flags on older kernels.
        if !has_ifa_flags_attr && msg.header.flags.contains(AddressHeaderFlags::Tentative) {
            is_tentative = true;
        }

        let Some(v6) = addr_v6 else { continue };

        // fe80::/10 check.
        if (v6.segments()[0] & 0xffc0) != 0xfe80 {
            continue;
        }

        if !is_tentative {
            return Ok((v6, ifindex));
        }
    }

    Err(format!("dhcpv6: no non-tentative link-local address on {interface}"))
}

// ── Message encoding ─────────────────────────────────────────────────────────

/// DHCPv6 option representation for encoding/decoding.
#[derive(Debug, Clone)]
pub(super) enum Dhcpv6Option {
    ClientId(Vec<u8>),
    ServerId(Vec<u8>),
    IaNa {
        iaid: u32,
        t1: u32,
        t2: u32,
        addresses: Vec<Dhcpv6Address>,
    },
    OptionRequest(Vec<u16>),
    ElapsedTime(u16),
    DnsServers(Vec<Ipv6Addr>),
    DnsSearchList(Vec<String>),
    InfoRefreshTime(u32),
    Unknown(u16, Vec<u8>),
}

/// Encode a DHCPv6 message: 1-byte type, 3-byte tx_id, TLV options.
pub(super) fn encode_message(
    msg_type: u8,
    tx_id: [u8; 3],
    options: &[Dhcpv6Option],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);
    buf.push(msg_type);
    buf.extend_from_slice(&tx_id);
    for opt in options {
        encode_option(opt, &mut buf);
    }
    buf
}

fn encode_option(opt: &Dhcpv6Option, buf: &mut Vec<u8>) {
    match opt {
        Dhcpv6Option::ClientId(data) => write_tlv(buf, OPT_CLIENT_ID, data),
        Dhcpv6Option::ServerId(data) => write_tlv(buf, OPT_SERVER_ID, data),
        Dhcpv6Option::IaNa { iaid, t1, t2, addresses } => {
            let mut body = Vec::with_capacity(12 + addresses.len() * 28);
            body.extend_from_slice(&iaid.to_be_bytes());
            body.extend_from_slice(&t1.to_be_bytes());
            body.extend_from_slice(&t2.to_be_bytes());
            for addr in addresses {
                encode_ia_address(addr, &mut body);
            }
            write_tlv(buf, OPT_IA_NA, &body);
        }
        Dhcpv6Option::OptionRequest(codes) => {
            let mut data = Vec::with_capacity(codes.len() * 2);
            for code in codes {
                data.extend_from_slice(&code.to_be_bytes());
            }
            write_tlv(buf, OPT_OPTION_REQUEST, &data);
        }
        Dhcpv6Option::ElapsedTime(t) => {
            write_tlv(buf, OPT_ELAPSED_TIME, &t.to_be_bytes());
        }
        Dhcpv6Option::DnsServers(addrs) => {
            let mut data = Vec::with_capacity(addrs.len() * 16);
            for addr in addrs {
                data.extend_from_slice(&addr.octets());
            }
            write_tlv(buf, OPT_DNS_SERVERS, &data);
        }
        Dhcpv6Option::DnsSearchList(domains) => {
            let mut data = Vec::new();
            for domain in domains {
                encode_dns_name(domain, &mut data);
            }
            write_tlv(buf, OPT_DNS_SEARCH, &data);
        }
        Dhcpv6Option::InfoRefreshTime(t) => {
            write_tlv(buf, OPT_INFO_REFRESH_TIME, &t.to_be_bytes());
        }
        Dhcpv6Option::Unknown(code, data) => {
            write_tlv(buf, *code, data);
        }
    }
}

fn encode_ia_address(addr: &Dhcpv6Address, buf: &mut Vec<u8>) {
    // IA Address sub-option: 16-byte IPv6 + 4-byte preferred_lft + 4-byte valid_lft.
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&addr.address.octets());
    body.extend_from_slice(&addr.preferred_lft.to_be_bytes());
    body.extend_from_slice(&addr.valid_lft.to_be_bytes());
    write_tlv(buf, OPT_IA_ADDR, &body);
}

fn write_tlv(buf: &mut Vec<u8>, code: u16, data: &[u8]) {
    buf.extend_from_slice(&code.to_be_bytes());
    buf.extend_from_slice(&(data.len() as u16).to_be_bytes());
    buf.extend_from_slice(data);
}

// ── Message decoding ─────────────────────────────────────────────────────────

/// Parse TLV options from bytes (starting after the 4-byte message header).
pub(super) fn decode_options(data: &[u8]) -> Vec<Dhcpv6Option> {
    let mut options = Vec::new();
    let mut pos = 0;
    while pos + 4 <= data.len() {
        let code = u16::from_be_bytes([data[pos], data[pos + 1]]);
        let len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        if pos + len > data.len() {
            break;
        }
        let opt_data = &data[pos..pos + len];
        pos += len;

        let opt = match code {
            OPT_CLIENT_ID => Dhcpv6Option::ClientId(opt_data.to_vec()),
            OPT_SERVER_ID => Dhcpv6Option::ServerId(opt_data.to_vec()),
            OPT_IA_NA => decode_ia_na(opt_data),
            OPT_ELAPSED_TIME if len >= 2 => {
                Dhcpv6Option::ElapsedTime(u16::from_be_bytes([opt_data[0], opt_data[1]]))
            }
            OPT_DNS_SERVERS => decode_dns_servers(opt_data),
            OPT_DNS_SEARCH => Dhcpv6Option::DnsSearchList(decode_dns_search_list(opt_data)),
            OPT_INFO_REFRESH_TIME if len >= 4 => Dhcpv6Option::InfoRefreshTime(
                u32::from_be_bytes([opt_data[0], opt_data[1], opt_data[2], opt_data[3]]),
            ),
            _ => Dhcpv6Option::Unknown(code, opt_data.to_vec()),
        };
        options.push(opt);
    }
    options
}

fn decode_ia_na(data: &[u8]) -> Dhcpv6Option {
    if data.len() < 12 {
        return Dhcpv6Option::Unknown(OPT_IA_NA, data.to_vec());
    }
    let iaid = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let t1 = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let t2 = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

    // Parse IA Address sub-options nested within IA_NA.
    let mut addresses = Vec::new();
    let sub = &data[12..];
    let mut pos = 0;
    while pos + 4 <= sub.len() {
        let sub_code = u16::from_be_bytes([sub[pos], sub[pos + 1]]);
        let sub_len = u16::from_be_bytes([sub[pos + 2], sub[pos + 3]]) as usize;
        pos += 4;
        if pos + sub_len > sub.len() {
            break;
        }
        if sub_code == OPT_IA_ADDR && sub_len >= 24 {
            let addr_bytes: [u8; 16] = sub[pos..pos + 16].try_into().unwrap();
            let address = Ipv6Addr::from(addr_bytes);
            let preferred_lft = u32::from_be_bytes([
                sub[pos + 16],
                sub[pos + 17],
                sub[pos + 18],
                sub[pos + 19],
            ]);
            let valid_lft = u32::from_be_bytes([
                sub[pos + 20],
                sub[pos + 21],
                sub[pos + 22],
                sub[pos + 23],
            ]);
            addresses.push(Dhcpv6Address {
                address,
                prefix_len: 128,
                preferred_lft,
                valid_lft,
            });
        }
        pos += sub_len;
    }

    Dhcpv6Option::IaNa { iaid, t1, t2, addresses }
}

fn decode_dns_servers(data: &[u8]) -> Dhcpv6Option {
    let mut addrs = Vec::new();
    let mut pos = 0;
    while pos + 16 <= data.len() {
        let arr: [u8; 16] = data[pos..pos + 16].try_into().unwrap();
        addrs.push(Ipv6Addr::from(arr));
        pos += 16;
    }
    Dhcpv6Option::DnsServers(addrs)
}

/// Decode a Domain Search List (option 24) from RFC 1035 wire format.
///
/// DHCPv6 prohibits DNS name compression (RFC 8415 §10), so no pointer
/// handling is needed. Labels are length-prefixed; a zero-length label
/// terminates each name.
pub(super) fn decode_dns_search_list(data: &[u8]) -> Vec<String> {
    let mut domains = Vec::new();
    let mut pos = 0;

    while pos < data.len() {
        let mut labels: Vec<String> = Vec::new();
        let mut ok = true;

        loop {
            if pos >= data.len() {
                ok = false;
                break;
            }
            let len = data[pos] as usize;
            pos += 1;
            if len == 0 {
                break;
            }
            if len > 63 {
                ok = false;
                break;
            }
            if pos + len > data.len() {
                ok = false;
                break;
            }
            match std::str::from_utf8(&data[pos..pos + len]) {
                Ok(label) => labels.push(label.to_string()),
                Err(_) => {
                    ok = false;
                }
            }
            pos += len;
        }

        if ok && !labels.is_empty() {
            domains.push(labels.join("."));
        }
    }

    domains
}

/// Encode a domain name to RFC 1035 wire format (for DnsSearchList encoding).
fn encode_dns_name(domain: &str, buf: &mut Vec<u8>) {
    for label in domain.split('.') {
        let bytes = label.as_bytes();
        buf.push(bytes.len() as u8);
        buf.extend_from_slice(bytes);
    }
    buf.push(0);
}

// ── Reply parsing ─────────────────────────────────────────────────────────────

fn parse_reply_from_options(
    options: Vec<Dhcpv6Option>,
    src_ip: Ipv6Addr,
) -> Result<ParsedReply, String> {
    let mut server_duid = Vec::new();
    let mut addresses = Vec::new();
    let mut dns_servers = Vec::new();
    let mut dns_search = Vec::new();
    let mut t1 = 0u32;
    let mut t2 = 0u32;
    let mut info_refresh_time = None;

    for opt in options {
        match opt {
            Dhcpv6Option::ServerId(duid) => {
                server_duid = duid;
            }
            Dhcpv6Option::IaNa { t1: ia_t1, t2: ia_t2, addresses: ia_addrs, .. } => {
                t1 = ia_t1;
                t2 = ia_t2;
                addresses.extend(ia_addrs);
            }
            Dhcpv6Option::DnsServers(addrs) => {
                dns_servers = addrs;
            }
            Dhcpv6Option::DnsSearchList(search) => {
                dns_search = search;
            }
            Dhcpv6Option::InfoRefreshTime(t) => {
                info_refresh_time = Some(t);
            }
            _ => {}
        }
    }

    Ok(ParsedReply {
        server_duid,
        server_addr: src_ip,
        addresses,
        dns_servers,
        dns_search,
        t1,
        t2,
        info_refresh_time,
    })
}

// ── Utilities ────────────────────────────────────────────────────────────────

/// Generate 3 random bytes for the DHCPv6 transaction ID.
pub(super) fn random_tx_id() -> [u8; 3] {
    let mut rng = rand::rng();
    [rng.random(), rng.random(), rng.random()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::net::Ipv6Addr;

    // ── Message header ────────────────────────────────────────────────────────

    // Scenario: Encoded message starts with correct type byte and transaction ID
    #[test]
    fn test_encode_message_has_correct_type_and_tx_id() {
        let tx_id = [0xAB, 0xCD, 0xEF];
        let msg = encode_message(MSG_SOLICIT, tx_id, &[]);
        assert_eq!(msg.len(), 4, "message with no options must be exactly 4 bytes");
        assert_eq!(msg[0], MSG_SOLICIT, "first byte must be the message type");
        assert_eq!(msg[1], 0xAB, "tx_id byte 0 must match");
        assert_eq!(msg[2], 0xCD, "tx_id byte 1 must match");
        assert_eq!(msg[3], 0xEF, "tx_id byte 2 must match");
    }

    // Scenario: All DHCPv6 message type constants are distinct
    #[test]
    fn test_message_type_constants_are_distinct() {
        let types = [
            MSG_SOLICIT,
            MSG_ADVERTISE,
            MSG_REQUEST,
            MSG_RENEW,
            MSG_REBIND,
            MSG_REPLY,
            MSG_RELEASE,
            MSG_INFORMATION_REQUEST,
        ];
        let unique: HashSet<u8> = types.iter().copied().collect();
        assert_eq!(
            unique.len(),
            types.len(),
            "all DHCPv6 message type constants must be distinct"
        );
    }

    // Scenario: random_tx_id produces exactly 3 bytes
    #[test]
    fn test_random_tx_id_produces_3_bytes() {
        let id = random_tx_id();
        assert_eq!(id.len(), 3, "transaction ID must be 3 bytes");
    }

    // Scenario: random_tx_id produces different values across calls
    #[test]
    fn test_random_tx_ids_are_different() {
        // With 2^24 = 16M possibilities, 10 samples will almost never collide.
        let ids: Vec<[u8; 3]> = (0..10).map(|_| random_tx_id()).collect();
        let unique: HashSet<[u8; 3]> = ids.into_iter().collect();
        assert!(unique.len() > 1, "random transaction IDs must not all be equal");
    }

    // ── ClientId / ServerId options ───────────────────────────────────────────

    // Scenario: Client Identifier encodes and decodes correctly
    #[test]
    fn test_encode_decode_client_id_roundtrip() {
        let duid_bytes = vec![
            0x00, 0x01, 0x00, 0x01, 0x12, 0x34, 0x56, 0x78, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
        ];
        let options = vec![Dhcpv6Option::ClientId(duid_bytes.clone())];
        let msg = encode_message(MSG_SOLICIT, [0, 0, 0], &options);
        let decoded = decode_options(&msg[4..]);
        assert_eq!(decoded.len(), 1);
        match &decoded[0] {
            Dhcpv6Option::ClientId(data) => assert_eq!(data, &duid_bytes),
            other => panic!("expected ClientId, got {:?}", other),
        }
    }

    // Scenario: Server Identifier encodes and decodes correctly
    #[test]
    fn test_encode_decode_server_id_roundtrip() {
        let server_duid = vec![
            0x00, 0x01, 0x00, 0x01, 0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55,
        ];
        let options = vec![Dhcpv6Option::ServerId(server_duid.clone())];
        let msg = encode_message(MSG_REQUEST, [1, 2, 3], &options);
        let decoded = decode_options(&msg[4..]);
        assert_eq!(decoded.len(), 1);
        match &decoded[0] {
            Dhcpv6Option::ServerId(data) => assert_eq!(data, &server_duid),
            other => panic!("expected ServerId, got {:?}", other),
        }
    }

    // ── DNS options ───────────────────────────────────────────────────────────

    // Scenario: A single DNS server address encodes and decodes correctly
    #[test]
    fn test_encode_decode_dns_servers_single() {
        let dns: Ipv6Addr = "2001:db8::53".parse().unwrap();
        let options = vec![Dhcpv6Option::DnsServers(vec![dns])];
        let msg = encode_message(MSG_REPLY, [0, 0, 0], &options);
        let decoded = decode_options(&msg[4..]);
        assert_eq!(decoded.len(), 1);
        match &decoded[0] {
            Dhcpv6Option::DnsServers(addrs) => {
                assert_eq!(addrs.len(), 1);
                assert_eq!(addrs[0], dns);
            }
            other => panic!("expected DnsServers, got {:?}", other),
        }
    }

    // Scenario: Multiple DNS server addresses encode and decode correctly
    #[test]
    fn test_encode_decode_dns_servers_multiple() {
        let dns1: Ipv6Addr = "2001:db8::53".parse().unwrap();
        let dns2: Ipv6Addr = "2001:db8::54".parse().unwrap();
        let options = vec![Dhcpv6Option::DnsServers(vec![dns1, dns2])];
        let msg = encode_message(MSG_REPLY, [0, 0, 0], &options);
        let decoded = decode_options(&msg[4..]);
        match &decoded[0] {
            Dhcpv6Option::DnsServers(addrs) => {
                assert_eq!(addrs.len(), 2, "must decode both DNS servers");
                assert_eq!(addrs[0], dns1);
                assert_eq!(addrs[1], dns2);
            }
            other => panic!("expected DnsServers, got {:?}", other),
        }
    }

    // Scenario: DNS Search List with a single domain encodes and decodes correctly
    #[test]
    fn test_encode_decode_dns_search_list_single_domain() {
        let options =
            vec![Dhcpv6Option::DnsSearchList(vec!["example.com".to_string()])];
        let msg = encode_message(MSG_REPLY, [0, 0, 0], &options);
        let decoded = decode_options(&msg[4..]);
        match &decoded[0] {
            Dhcpv6Option::DnsSearchList(domains) => {
                assert_eq!(domains, &["example.com"]);
            }
            other => panic!("expected DnsSearchList, got {:?}", other),
        }
    }

    // Scenario: DNS Search List with multiple domains round-trips correctly
    #[test]
    fn test_encode_decode_dns_search_list_multiple_domains() {
        let domains = vec!["example.com".to_string(), "test.net".to_string()];
        let options = vec![Dhcpv6Option::DnsSearchList(domains.clone())];
        let msg = encode_message(MSG_REPLY, [0, 0, 0], &options);
        let decoded = decode_options(&msg[4..]);
        match &decoded[0] {
            Dhcpv6Option::DnsSearchList(d) => assert_eq!(d, &domains),
            other => panic!("expected DnsSearchList, got {:?}", other),
        }
    }

    // Scenario: decode_dns_search_list handles RFC 1035 wire format for a single domain
    #[test]
    fn test_decode_dns_search_list_rfc1035_single_domain() {
        // "example.com" in RFC 1035 wire format: \x07example\x03com\x00
        let mut data = Vec::new();
        data.push(7u8);
        data.extend_from_slice(b"example");
        data.push(3u8);
        data.extend_from_slice(b"com");
        data.push(0u8);
        let domains = decode_dns_search_list(&data);
        assert_eq!(domains, vec!["example.com"]);
    }

    // Scenario: decode_dns_search_list handles multiple domains in RFC 1035 format
    #[test]
    fn test_decode_dns_search_list_rfc1035_multiple_domains() {
        let mut data = Vec::new();
        // "example.com"
        data.push(7u8);
        data.extend_from_slice(b"example");
        data.push(3u8);
        data.extend_from_slice(b"com");
        data.push(0u8);
        // "test.org"
        data.push(4u8);
        data.extend_from_slice(b"test");
        data.push(3u8);
        data.extend_from_slice(b"org");
        data.push(0u8);
        let domains = decode_dns_search_list(&data);
        assert_eq!(domains, vec!["example.com", "test.org"]);
    }

    // Scenario: decode_dns_search_list returns empty vec for empty input
    #[test]
    fn test_decode_dns_search_list_empty_data() {
        let domains = decode_dns_search_list(&[]);
        assert!(domains.is_empty(), "empty data must produce empty domain list");
    }

    // ── IA_NA option ──────────────────────────────────────────────────────────

    // Scenario: IA_NA with a single address encodes and decodes correctly
    #[test]
    fn test_encode_decode_ia_na_with_single_address() {
        let addr = Dhcpv6Address {
            address: "2001:db8::100".parse().unwrap(),
            prefix_len: 128,
            preferred_lft: 14400,
            valid_lft: 86400,
        };
        let options = vec![Dhcpv6Option::IaNa {
            iaid: 42,
            t1: 3600,
            t2: 5760,
            addresses: vec![addr],
        }];
        let msg = encode_message(MSG_REPLY, [0, 0, 0], &options);
        let decoded = decode_options(&msg[4..]);
        assert_eq!(decoded.len(), 1);
        match &decoded[0] {
            Dhcpv6Option::IaNa { iaid, t1, t2, addresses } => {
                assert_eq!(*iaid, 42);
                assert_eq!(*t1, 3600);
                assert_eq!(*t2, 5760);
                assert_eq!(addresses.len(), 1);
                assert_eq!(
                    addresses[0].address,
                    "2001:db8::100".parse::<Ipv6Addr>().unwrap()
                );
                assert_eq!(addresses[0].preferred_lft, 14400);
                assert_eq!(addresses[0].valid_lft, 86400);
                assert_eq!(addresses[0].prefix_len, 128);
            }
            other => panic!("expected IaNa, got {:?}", other),
        }
    }

    // Scenario: Multiple addresses in IA_NA — both addresses are decoded correctly
    #[test]
    fn test_encode_decode_ia_na_with_multiple_addresses() {
        let addr1 = Dhcpv6Address {
            address: "2001:db8::100".parse().unwrap(),
            prefix_len: 128,
            preferred_lft: 3600,
            valid_lft: 7200,
        };
        let addr2 = Dhcpv6Address {
            address: "2001:db8::200".parse().unwrap(),
            prefix_len: 128,
            preferred_lft: 7200,
            valid_lft: 14400,
        };
        let options = vec![Dhcpv6Option::IaNa {
            iaid: 1,
            t1: 1800,
            t2: 2880,
            addresses: vec![addr1, addr2],
        }];
        let msg = encode_message(MSG_REPLY, [0, 0, 0], &options);
        let decoded = decode_options(&msg[4..]);
        match &decoded[0] {
            Dhcpv6Option::IaNa { addresses, .. } => {
                assert_eq!(addresses.len(), 2, "must decode both addresses from IA_NA");
                assert_eq!(
                    addresses[0].address,
                    "2001:db8::100".parse::<Ipv6Addr>().unwrap()
                );
                assert_eq!(addresses[0].preferred_lft, 3600);
                assert_eq!(addresses[0].valid_lft, 7200);
                assert_eq!(
                    addresses[1].address,
                    "2001:db8::200".parse::<Ipv6Addr>().unwrap()
                );
                assert_eq!(addresses[1].preferred_lft, 7200);
                assert_eq!(addresses[1].valid_lft, 14400);
            }
            other => panic!("expected IaNa, got {:?}", other),
        }
    }

    // Scenario: IA_NA with no addresses encodes/decodes correctly
    #[test]
    fn test_encode_decode_ia_na_no_addresses() {
        let options = vec![Dhcpv6Option::IaNa {
            iaid: 99,
            t1: 0,
            t2: 0,
            addresses: vec![],
        }];
        let msg = encode_message(MSG_SOLICIT, [0, 0, 0], &options);
        let decoded = decode_options(&msg[4..]);
        match &decoded[0] {
            Dhcpv6Option::IaNa { iaid, addresses, .. } => {
                assert_eq!(*iaid, 99);
                assert!(addresses.is_empty(), "Solicit IA_NA must have no addresses");
            }
            other => panic!("expected IaNa, got {:?}", other),
        }
    }

    // ── Elapsed Time and Info Refresh Time ────────────────────────────────────

    // Scenario: Elapsed Time option encodes and decodes correctly
    #[test]
    fn test_encode_decode_elapsed_time() {
        let options = vec![Dhcpv6Option::ElapsedTime(1000)];
        let msg = encode_message(MSG_SOLICIT, [0, 0, 0], &options);
        let decoded = decode_options(&msg[4..]);
        match &decoded[0] {
            Dhcpv6Option::ElapsedTime(t) => assert_eq!(*t, 1000),
            other => panic!("expected ElapsedTime, got {:?}", other),
        }
    }

    // Scenario: Information Refresh Time option encodes and decodes correctly
    #[test]
    fn test_encode_decode_info_refresh_time() {
        let options = vec![Dhcpv6Option::InfoRefreshTime(900)];
        let msg = encode_message(MSG_REPLY, [0, 0, 0], &options);
        let decoded = decode_options(&msg[4..]);
        match &decoded[0] {
            Dhcpv6Option::InfoRefreshTime(t) => assert_eq!(*t, 900),
            other => panic!("expected InfoRefreshTime, got {:?}", other),
        }
    }

    // ── Unknown / truncated options ───────────────────────────────────────────

    // Scenario: Unknown option code is preserved as Unknown variant
    #[test]
    fn test_decode_unknown_option_preserved() {
        let mut data = vec![];
        data.extend_from_slice(&9999u16.to_be_bytes()); // unknown code
        data.extend_from_slice(&3u16.to_be_bytes()); // length=3
        data.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        let decoded = decode_options(&data);
        assert_eq!(decoded.len(), 1);
        match &decoded[0] {
            Dhcpv6Option::Unknown(code, bytes) => {
                assert_eq!(*code, 9999);
                assert_eq!(bytes, &[0xAA, 0xBB, 0xCC]);
            }
            other => panic!("expected Unknown option, got {:?}", other),
        }
    }

    // Scenario: Truncated option data is silently ignored (no panic)
    #[test]
    fn test_decode_options_truncated_option_ignored() {
        let mut data = vec![];
        data.extend_from_slice(&OPT_CLIENT_ID.to_be_bytes());
        data.extend_from_slice(&100u16.to_be_bytes()); // claims 100 bytes but provides none
        // No payload — decode_options must not panic
        let decoded = decode_options(&data);
        assert!(decoded.is_empty(), "truncated option must be silently dropped");
    }

    // ── Multi-option messages ─────────────────────────────────────────────────

    // Scenario: A Reply message with ClientId, ServerId, IA_NA, and DnsServers
    //           round-trips all options correctly
    #[test]
    fn test_encode_decode_full_reply_message() {
        let client_duid = vec![0x00, 0x01, 0x00, 0x01, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22, 0x33, 0x44];
        let server_duid = vec![0x00, 0x01, 0x00, 0x01, 0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA, 0x55, 0x44, 0x33, 0x22];
        let dns: Ipv6Addr = "2001:db8::53".parse().unwrap();
        let ia_addr = Dhcpv6Address {
            address: "2001:db8::100".parse().unwrap(),
            prefix_len: 128,
            preferred_lft: 14400,
            valid_lft: 86400,
        };

        let options = vec![
            Dhcpv6Option::ClientId(client_duid.clone()),
            Dhcpv6Option::ServerId(server_duid.clone()),
            Dhcpv6Option::IaNa {
                iaid: 1,
                t1: 7200,
                t2: 11520,
                addresses: vec![ia_addr],
            },
            Dhcpv6Option::DnsServers(vec![dns]),
            Dhcpv6Option::DnsSearchList(vec!["example.com".to_string()]),
        ];
        let tx_id = [0x01, 0x02, 0x03];
        let msg = encode_message(MSG_REPLY, tx_id, &options);

        // Header check
        assert_eq!(msg[0], MSG_REPLY);
        assert_eq!(&msg[1..4], &tx_id);

        // Options check
        let decoded = decode_options(&msg[4..]);
        assert_eq!(decoded.len(), 5, "must decode all 5 options");

        let has_client_id = decoded.iter().any(|o| matches!(o, Dhcpv6Option::ClientId(d) if d == &client_duid));
        let has_server_id = decoded.iter().any(|o| matches!(o, Dhcpv6Option::ServerId(d) if d == &server_duid));
        let has_ia_na = decoded.iter().any(|o| matches!(o, Dhcpv6Option::IaNa { addresses, .. } if addresses.len() == 1));
        let has_dns = decoded.iter().any(|o| matches!(o, Dhcpv6Option::DnsServers(a) if a.contains(&dns)));
        let has_search = decoded.iter().any(|o| matches!(o, Dhcpv6Option::DnsSearchList(d) if d.contains(&"example.com".to_string())));

        assert!(has_client_id, "decoded options must include ClientId");
        assert!(has_server_id, "decoded options must include ServerId");
        assert!(has_ia_na, "decoded options must include IaNa with one address");
        assert!(has_dns, "decoded options must include DnsServers");
        assert!(has_search, "decoded options must include DnsSearchList");
    }
}
