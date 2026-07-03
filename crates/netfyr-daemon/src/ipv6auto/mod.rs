//! IPv6 Stateless Address Autoconfiguration (SLAAC) factory.
//!
//! Processes Router Advertisements in userspace per RFC 4861/4862: opens a raw
//! ICMPv6 socket, sends a Router Solicitation, parses incoming RAs, generates
//! EUI-64 SLAAC addresses, manages their lifetimes, and produces a `State`
//! with an `ipv6` sub-object for the reconciler.
//!
//! Kernel SLAAC is disabled via procfs (`accept_ra=0`, `autoconf=0`) so that
//! the factory has exclusive ownership of SLAAC address lifecycle.

mod address;
pub mod dhcpv6;
mod ra;

use std::collections::HashMap;
use std::mem::MaybeUninit;
use std::net::Ipv6Addr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indexmap::IndexMap;
use ipnetwork::{IpNetwork, Ipv6Network};
use netfyr_backend::FactoryEvent;
use netfyr_state::{
    entity_types::ETHERNET, FieldValue, Provenance, Selector, State, StateMetadata, Value,
};
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use self::address::{
    add_ipv6_address, check_slaac_address_dad, generate_slaac_address, get_ifindex,
    get_interface_mac, remove_ipv6_address, update_address_lifetime, wait_for_link_local_dad,
    DadStatus,
};
use self::dhcpv6::lease::Dhcpv6Lease;
use self::dhcpv6::{Dhcpv6Client, Dhcpv6Result};
use self::ra::{open_ra_socket, parse_ra, send_router_solicitation, RaOption};

// ── Lifetime constants ────────────────────────────────────────────────────────

/// RFC 4861 §6.3.5: an "infinite" lifetime value (0xffffffff = ~136 years).
const LIFETIME_INFINITY: u32 = 0xffff_ffff;

/// Duration used when a lifetime is "infinite": far-future sentinel.
const INFINITE_DURATION: Duration = Duration::from_secs(0xffff_ffff);

// ── Internal state structs ────────────────────────────────────────────────────

struct PrefixState {
    /// Generated SLAAC address (EUI-64 from MAC + prefix).
    addr: Ipv6Addr,
    prefix_len: u8,
    valid_expires: Instant,
    preferred_expires: Instant,
    #[allow(dead_code)]
    on_link: bool,
    /// Whether the address has been installed via rtnetlink.
    installed: bool,
    /// DAD completed successfully; address is permanent and usable.
    dad_complete: bool,
    /// DAD failed; address was removed from the interface.
    dad_failed: bool,
    /// Preferred lifetime has passed; kernel was notified (preferred_lft=0).
    deprecated: bool,
}

struct RouterState {
    expires_at: Instant,
}

struct RdnssEntry {
    addresses: Vec<Ipv6Addr>,
    expires_at: Instant,
}

struct DnsslEntry {
    domains: Vec<String>,
    expires_at: Instant,
}

struct Pref64Entry {
    prefix: Ipv6Network,
    expires_at: Instant,
}

struct RouteInfoEntry {
    prefix: Ipv6Network,
    /// Link-local source of the RA that advertised this route.
    router: Ipv6Addr,
    /// RFC 4191 preference: 1=high, 0=medium, 3=low.
    preference: u8,
    expires_at: Instant,
}

#[derive(Default)]
struct Ipv6AutoState {
    /// Tracked SLAAC prefixes, keyed by the announced prefix network.
    prefixes: HashMap<Ipv6Network, PrefixState>,
    /// Active routers (default gateway candidates), keyed by link-local source.
    routers: HashMap<Ipv6Addr, RouterState>,
    /// Active RDNSS entries.
    rdnss: Vec<RdnssEntry>,
    /// Active DNSSL entries.
    dnssl: Vec<DnsslEntry>,
    /// Active PREF64/NAT64 prefix, if any.
    pref64: Option<Pref64Entry>,
    /// Additional routes from Route Information options (RFC 4191).
    route_info: Vec<RouteInfoEntry>,
    /// Last reported M/O flags (used to suppress duplicate events).
    last_m: Option<bool>,
    last_o: Option<bool>,
}

// ── Public factory struct ─────────────────────────────────────────────────────

/// Manages the lifecycle of a userspace IPv6 SLAAC process for one interface.
pub struct Ipv6AutoFactory {
    interface: String,
    /// Latest produced `State`, updated by the background task on every RA.
    state: Arc<Mutex<Option<State>>>,
    stop_tx: Option<oneshot::Sender<()>>,
    task_handle: Option<JoinHandle<()>>,
}

impl Ipv6AutoFactory {
    /// Start the SLAAC factory for `interface`.
    ///
    /// Sets the pending state immediately (enabling reconciliation that brings
    /// the interface UP), then spawns a background task that performs all
    /// blocking startup work: MAC lookup, procfs writes, socket open,
    /// link-local DAD wait, RS send, and the main RA-processing loop.
    pub async fn start(
        interface: &str,
        policy_name: String,
        priority: u32,
        event_tx: mpsc::Sender<FactoryEvent>,
    ) -> Result<Self, String> {
        // Set pending state immediately — this is what the reconciler uses to
        // bring the interface UP. Must happen before any blocking work so that
        // produced_states() always returns a value from this point on.
        let state_arc: Arc<Mutex<Option<State>>> = Arc::new(Mutex::new(Some(
            pending_state(interface, &policy_name, priority),
        )));

        let (stop_tx, stop_rx) = oneshot::channel();

        let task_interface = interface.to_string();
        let task_state_arc = Arc::clone(&state_arc);

        let handle = tokio::spawn(async move {
            run_ipv6auto(
                task_interface,
                policy_name,
                priority,
                task_state_arc,
                event_tx,
                stop_rx,
            )
            .await;
        });

        Ok(Self {
            interface: interface.to_string(),
            state: state_arc,
            stop_tx: Some(stop_tx),
            task_handle: Some(handle),
        })
    }

    /// Stop the factory and clean up the background task.
    pub async fn stop(&mut self) -> Result<(), String> {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.task_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }
        Ok(())
    }

    /// Return a snapshot of the current produced `State`, or `None` if not yet started.
    pub fn current_state(&self) -> Option<State> {
        self.state.lock().unwrap().clone()
    }

    /// Return the network interface name this factory manages.
    pub fn interface(&self) -> &str {
        &self.interface
    }
}

// ── Background task ───────────────────────────────────────────────────────────

/// Main event loop: startup, RA reception, lifetime expiry, DAD monitoring,
/// RS retry with exponential backoff, stop signal.
async fn run_ipv6auto(
    interface: String,
    policy_name: String,
    priority: u32,
    state_arc: Arc<Mutex<Option<State>>>,
    event_tx: mpsc::Sender<FactoryEvent>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    // ── Startup ───────────────────────────────────────────────────────────────

    // Get MAC address — works even when interface is DOWN.
    let mac = match get_interface_mac(&interface).await {
        Ok(m) => m,
        Err(e) => {
            let _ = event_tx
                .send(FactoryEvent::Error {
                    policy_name: policy_name.clone(),
                    error: format!("ipv6auto: {e}"),
                })
                .await;
            return;
        }
    };

    // Set addr_gen_mode=0 (EUI-64) before the interface comes UP so the kernel
    // generates the link-local address using EUI-64, matching the SLAAC addresses
    // the factory will generate for global prefixes.
    if let Err(e) = write_procfs_addr_gen_mode(&interface, 0) {
        warn!(interface, error = %e, "ipv6auto: failed to set addr_gen_mode=0");
    }

    // Disable kernel RA processing and SLAAC address generation so the daemon
    // has exclusive ownership of SLAAC lifecycle on this interface.
    if let Err(e) = write_procfs_accept_ra(&interface, 0) {
        warn!(interface, error = %e, "ipv6auto: failed to set accept_ra=0");
    }
    if let Err(e) = write_procfs_autoconf(&interface, 0) {
        warn!(interface, error = %e, "ipv6auto: failed to set autoconf=0");
    }

    // Open raw ICMPv6 socket for RA reception.
    let socket = match open_ra_socket(&interface) {
        Ok(s) => s,
        Err(e) => {
            let _ = event_tx
                .send(FactoryEvent::Error {
                    policy_name: policy_name.clone(),
                    error: format!("ipv6auto: failed to open ICMPv6 socket: {e}"),
                })
                .await;
            return;
        }
    };

    // Get interface index (required for Router Solicitation).
    let ifindex = match get_ifindex(&interface).await {
        Ok(i) => i,
        Err(e) => {
            let _ = event_tx
                .send(FactoryEvent::Error {
                    policy_name: policy_name.clone(),
                    error: format!("ipv6auto: {e}"),
                })
                .await;
            return;
        }
    };

    // Wait for the kernel-assigned link-local address to complete DAD.
    // The pending state set in start() causes the reconciler to bring the
    // interface UP; once UP the kernel assigns a link-local and runs DAD.
    // Retries on timeout (e.g. interface not yet UP) until stop is requested.
    loop {
        let result = tokio::select! {
            biased;
            _ = &mut stop_rx => return,
            r = wait_for_link_local_dad(&interface, Duration::from_secs(10)) => r,
        };
        match result {
            Ok(ll) => {
                debug!(interface, %ll, "Link-local DAD complete");
                break;
            }
            Err(e) => {
                let _ = event_tx
                    .send(FactoryEvent::Error {
                        policy_name: policy_name.clone(),
                        error: format!("ipv6auto: {e}"),
                    })
                    .await;
            }
        }
    }

    // Send initial Router Solicitation.
    if let Err(e) = send_router_solicitation(&socket, ifindex) {
        warn!(interface, error = %e, "ipv6auto: failed to send Router Solicitation");
    }

    let async_socket = match AsyncFd::new(socket) {
        Ok(fd) => fd,
        Err(e) => {
            warn!(interface, error = %e, "ipv6auto: Failed to create AsyncFd");
            return;
        }
    };

    // ── Main loop ──────────────────────────────────────────────────────────────

    let mut ra_state = Ipv6AutoState::default();
    let mut lease_acquired = false;
    let mut first_ra_received = false;
    // RS retry: exponential backoff starting at 4 s, doubling, capped at 3600 s
    // (RFC 4861 §6.3.7).
    let mut rs_interval = Duration::from_secs(4);
    let mut next_rs_time = Instant::now() + rs_interval;

    // DHCPv6 state: optional client started on M/O flag, its result channel,
    // the most recent lease, and IA_NA addresses we installed via netlink.
    let mut dhcpv6_client: Option<Dhcpv6Client> = None;
    let mut dhcpv6_result_rx: Option<mpsc::Receiver<Dhcpv6Result>> = None;
    let mut dhcpv6_lease: Option<Dhcpv6Lease> = None;
    let mut dhcpv6_installed_addrs: Vec<Ipv6Addr> = Vec::new();

    loop {
        // Compute earliest wakeup from all tracked timers.
        let has_pending_dad = ra_state
            .prefixes
            .values()
            .any(|ps| ps.installed && !ps.dad_complete && !ps.dad_failed);
        let dad_check_time =
            has_pending_dad.then(|| Instant::now() + Duration::from_millis(300));
        let rs_time = (!first_ra_received).then_some(next_rs_time);
        let next_expiry = compute_next_expiry(&ra_state);

        let next_wakeup: Option<Instant> = [next_expiry, dad_check_time, rs_time]
            .into_iter()
            .flatten()
            .min();

        tokio::select! {
            // ── RA received ───────────────────────────────────────────────────
            readable = async_socket.readable() => {
                let mut guard = match readable {
                    Ok(g) => g,
                    Err(e) => {
                        warn!(interface, error = %e, "RA socket poll error");
                        continue;
                    }
                };
                let result = guard.try_io(|inner| {
                    let mut buf = vec![MaybeUninit::<u8>::zeroed(); 1500];
                    let (n, addr) = inner.get_ref().recv_from(&mut buf)?;
                    let src = addr
                        .as_socket_ipv6()
                        .map(|s| *s.ip())
                        .unwrap_or(Ipv6Addr::UNSPECIFIED);
                    // Safety: recv_from initialises buf[..n].
                    let data: Vec<u8> = buf[..n]
                        .iter()
                        .map(|b| unsafe { b.assume_init() })
                        .collect();
                    Ok((data, src))
                });
                match result {
                    Ok(Ok((data, src))) => {
                        if let Some(ra) = parse_ra(&data, src) {
                            debug!(interface, %src, "RA received");
                            first_ra_received = true;

                            // Report M/O flag changes and start/stop DHCPv6.
                            let m_changed = ra_state.last_m != Some(ra.m_flag);
                            let o_changed = ra_state.last_o != Some(ra.o_flag);
                            if m_changed || o_changed {
                                ra_state.last_m = Some(ra.m_flag);
                                ra_state.last_o = Some(ra.o_flag);
                                let _ = event_tx
                                    .send(FactoryEvent::Ipv6AutoFlags {
                                        policy_name: policy_name.clone(),
                                        m: ra.m_flag,
                                        o: ra.o_flag,
                                    })
                                    .await;
                                manage_dhcpv6(
                                    ra.m_flag,
                                    ra.o_flag,
                                    &interface,
                                    &mut dhcpv6_client,
                                    &mut dhcpv6_result_rx,
                                    &mut dhcpv6_lease,
                                    &mut dhcpv6_installed_addrs,
                                )
                                .await;
                            }

                            handle_ra(&ra, &mut ra_state, &interface, mac).await;

                            let new_state = build_merged_state(
                                &ra_state,
                                &interface,
                                &policy_name,
                                priority,
                                dhcpv6_lease.as_ref(),
                            );
                            *state_arc.lock().unwrap() = Some(new_state.clone());
                            emit_lease_event_inner(
                                &ra_state,
                                &mut lease_acquired,
                                &policy_name,
                                new_state,
                                &event_tx,
                                dhcpv6_lease.is_some(),
                            )
                            .await;
                        }
                    }
                    Ok(Err(e)) => {
                        warn!(interface, error = %e, "RA recv error");
                    }
                    Err(_would_block) => {
                        // Try again when readable fires next time.
                    }
                }
            }

            // ── Timer: RS retry, DAD monitoring, lifetime expiry ──────────────
            _ = async {
                match next_wakeup {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let now = Instant::now();
                let mut state_changed = false;

                // RS retry with exponential backoff.
                if !first_ra_received && now >= next_rs_time {
                    if let Err(e) = send_router_solicitation(async_socket.get_ref(), ifindex) {
                        warn!(interface, error = %e, "ipv6auto: RS retry failed");
                    }
                    let _ = event_tx
                        .send(FactoryEvent::Error {
                            policy_name: policy_name.clone(),
                            error: "ipv6auto: no Router Advertisement received".to_string(),
                        })
                        .await;
                    rs_interval = (rs_interval * 2).min(Duration::from_secs(3600));
                    next_rs_time = now + rs_interval;
                }

                // DAD monitoring: poll addresses transitioning from tentative.
                if has_pending_dad {
                    let to_check: Vec<(Ipv6Network, Ipv6Addr, u8)> = ra_state
                        .prefixes
                        .iter()
                        .filter(|(_, ps)| ps.installed && !ps.dad_complete && !ps.dad_failed)
                        .map(|(net, ps)| (*net, ps.addr, ps.prefix_len))
                        .collect();

                    for (net, addr, plen) in to_check {
                        let status = check_slaac_address_dad(&interface, addr, plen).await;
                        match status {
                            DadStatus::Complete => {
                                if let Some(ps) = ra_state.prefixes.get_mut(&net) {
                                    ps.dad_complete = true;
                                    info!(interface, %addr, "SLAAC address DAD complete");
                                    state_changed = true;
                                }
                            }
                            DadStatus::Failed => {
                                if let Some(ps) = ra_state.prefixes.get_mut(&net) {
                                    ps.dad_failed = true;
                                    info!(interface, %addr, "SLAAC address DAD failed");
                                }
                                if let Err(e) =
                                    remove_ipv6_address(&interface, addr, plen).await
                                {
                                    warn!(interface, %addr, error = %e,
                                          "Failed to remove DAD-failed SLAAC address");
                                }
                                let _ = event_tx
                                    .send(FactoryEvent::Error {
                                        policy_name: policy_name.clone(),
                                        error: format!(
                                            "ipv6auto: SLAAC address DAD failed for {addr}"
                                        ),
                                    })
                                    .await;
                                state_changed = true;
                            }
                            DadStatus::Tentative => {}
                        }
                    }
                }

                // Lifetime management: deprecate preferred-expired addresses and
                // remove valid-expired entries.
                let changed = update_lifetimes(&mut ra_state, &interface).await;
                state_changed |= changed;

                if state_changed {
                    let new_state = build_merged_state(
                        &ra_state,
                        &interface,
                        &policy_name,
                        priority,
                        dhcpv6_lease.as_ref(),
                    );
                    *state_arc.lock().unwrap() = Some(new_state.clone());
                    emit_lease_event_inner(
                        &ra_state,
                        &mut lease_acquired,
                        &policy_name,
                        new_state,
                        &event_tx,
                        dhcpv6_lease.is_some(),
                    )
                    .await;
                }
            }

            // ── DHCPv6 result ─────────────────────────────────────────────────
            dhcpv6_msg = async {
                match dhcpv6_result_rx.as_mut() {
                    Some(rx) => match rx.recv().await {
                        Some(r) => r,
                        None => std::future::pending::<Dhcpv6Result>().await,
                    },
                    None => std::future::pending::<Dhcpv6Result>().await,
                }
            } => {
                match dhcpv6_msg {
                    Dhcpv6Result::Acquired(lease) | Dhcpv6Result::Renewed(lease) => {
                        let is_stateful =
                            dhcpv6_client.as_ref().is_some_and(|c| c.is_stateful());
                        if is_stateful {
                            install_dhcpv6_addresses(
                                &interface,
                                &lease,
                                &mut dhcpv6_installed_addrs,
                            )
                            .await;
                        }
                        dhcpv6_lease = Some(lease);
                        let new_state = build_merged_state(
                            &ra_state,
                            &interface,
                            &policy_name,
                            priority,
                            dhcpv6_lease.as_ref(),
                        );
                        *state_arc.lock().unwrap() = Some(new_state.clone());
                        emit_lease_event_inner(
                            &ra_state,
                            &mut lease_acquired,
                            &policy_name,
                            new_state,
                            &event_tx,
                            true,
                        )
                        .await;
                    }
                    Dhcpv6Result::Expired => {
                        remove_dhcpv6_addresses(&interface, &mut dhcpv6_installed_addrs).await;
                        dhcpv6_lease = None;
                        let new_state = build_merged_state(
                            &ra_state,
                            &interface,
                            &policy_name,
                            priority,
                            None,
                        );
                        *state_arc.lock().unwrap() = Some(new_state.clone());
                        emit_lease_event_inner(
                            &ra_state,
                            &mut lease_acquired,
                            &policy_name,
                            new_state,
                            &event_tx,
                            false,
                        )
                        .await;
                    }
                    Dhcpv6Result::Error(e) => {
                        warn!(interface, error = %e, "DHCPv6 transient error");
                        let _ = event_tx
                            .send(FactoryEvent::Error {
                                policy_name: policy_name.clone(),
                                error: format!("dhcpv6: {e}"),
                            })
                            .await;
                    }
                }
            }

            // ── Stop signal ───────────────────────────────────────────────────
            _ = &mut stop_rx => {
                debug!(interface, "IPv6 auto factory stopping");
                if let Some(mut c) = dhcpv6_client.take() {
                    let _ = c.stop().await;
                }
                remove_dhcpv6_addresses(&interface, &mut dhcpv6_installed_addrs).await;
                break;
            }
        }
    }
}

// ── RA processing ─────────────────────────────────────────────────────────────

/// Update `state` from a parsed RA message: router entry, prefixes, RDNSS,
/// DNSSL, and PREF64. M/O flag changes are handled by the caller.
async fn handle_ra(
    ra: &ra::RaMessage,
    state: &mut Ipv6AutoState,
    interface: &str,
    mac: [u8; 6],
) {
    let now = Instant::now();

    // Update router entry.
    if ra.router_lifetime == 0 {
        state.routers.remove(&ra.source);
    } else {
        let expires_at = now + Duration::from_secs(ra.router_lifetime as u64);
        state.routers.insert(ra.source, RouterState { expires_at });
    }

    // Process options.
    for opt in &ra.options {
        match opt {
            RaOption::PrefixInfo {
                prefix,
                preferred_lft,
                valid_lft,
                autonomous,
                on_link,
            } => {
                if !autonomous || prefix.prefix() != 64 {
                    continue;
                }
                let addr = generate_slaac_address(prefix.network(), prefix.prefix(), mac);
                let net = match Ipv6Network::new(prefix.network(), prefix.prefix()) {
                    Ok(n) => n,
                    Err(_) => continue,
                };

                if *valid_lft == 0 {
                    // Withdraw prefix: remove address if installed.
                    if let Some(ps) = state.prefixes.remove(&net) {
                        if ps.installed {
                            if let Err(e) =
                                remove_ipv6_address(interface, ps.addr, ps.prefix_len).await
                            {
                                warn!(interface, addr = %ps.addr, error = %e,
                                      "Failed to remove withdrawn SLAAC address");
                            }
                        }
                    }
                    continue;
                }

                let valid_dur = if *valid_lft == LIFETIME_INFINITY {
                    INFINITE_DURATION
                } else {
                    Duration::from_secs(*valid_lft as u64)
                };
                let pref_dur = if *preferred_lft == LIFETIME_INFINITY {
                    INFINITE_DURATION
                } else {
                    Duration::from_secs(*preferred_lft as u64)
                };
                let valid_expires = now + valid_dur;
                let preferred_expires = now + pref_dur;

                // If a previous DAD-failed entry exists for this prefix, clear it
                // so the address can be re-attempted when the router re-announces.
                if state.prefixes.get(&net).is_some_and(|ps| ps.dad_failed) {
                    state.prefixes.remove(&net);
                }

                if let Some(existing) = state.prefixes.get_mut(&net) {
                    // Update lifetimes in-place.
                    existing.valid_expires = valid_expires;
                    existing.preferred_expires = preferred_expires;
                    // Reset deprecated flag when the preferred lifetime is extended.
                    if existing.deprecated && preferred_expires > now {
                        existing.deprecated = false;
                    }
                    if existing.installed {
                        if let Err(e) = update_address_lifetime(
                            interface,
                            existing.addr,
                            existing.prefix_len,
                            *valid_lft,
                            *preferred_lft,
                        )
                        .await
                        {
                            warn!(interface, addr = %existing.addr, error = %e,
                                  "Failed to update SLAAC address lifetime");
                        }
                    }
                } else {
                    // New prefix: install address; DAD will start automatically.
                    let installed =
                        match add_ipv6_address(interface, addr, 64, *valid_lft, *preferred_lft)
                            .await
                        {
                            Ok(()) => {
                                info!(interface, %addr, "SLAAC address added, DAD pending");
                                true
                            }
                            Err(e) => {
                                warn!(interface, %addr, error = %e,
                                      "Failed to add SLAAC address");
                                false
                            }
                        };
                    state.prefixes.insert(
                        net,
                        PrefixState {
                            addr,
                            prefix_len: 64,
                            valid_expires,
                            preferred_expires,
                            on_link: *on_link,
                            installed,
                            dad_complete: false,
                            dad_failed: false,
                            deprecated: false,
                        },
                    );
                }
            }

            RaOption::Rdnss { addresses, lifetime } => {
                if *lifetime == 0 {
                    state.rdnss.retain(|e| e.addresses != *addresses);
                } else {
                    let expires_at = now
                        + if *lifetime == LIFETIME_INFINITY {
                            INFINITE_DURATION
                        } else {
                            Duration::from_secs(*lifetime as u64)
                        };
                    // Replace existing entry for this address set, or add new.
                    if let Some(e) = state.rdnss.iter_mut().find(|e| e.addresses == *addresses) {
                        e.expires_at = expires_at;
                    } else {
                        state.rdnss.push(RdnssEntry {
                            addresses: addresses.clone(),
                            expires_at,
                        });
                    }
                }
            }

            RaOption::Dnssl { domains, lifetime } => {
                if *lifetime == 0 {
                    state.dnssl.retain(|e| e.domains != *domains);
                } else {
                    let expires_at = now
                        + if *lifetime == LIFETIME_INFINITY {
                            INFINITE_DURATION
                        } else {
                            Duration::from_secs(*lifetime as u64)
                        };
                    if let Some(e) = state.dnssl.iter_mut().find(|e| e.domains == *domains) {
                        e.expires_at = expires_at;
                    } else {
                        state.dnssl.push(DnsslEntry {
                            domains: domains.clone(),
                            expires_at,
                        });
                    }
                }
            }

            RaOption::Pref64 { prefix, lifetime } => {
                if *lifetime == 0 {
                    state.pref64 = None;
                } else {
                    let expires_at = now + Duration::from_secs(*lifetime as u64);
                    state.pref64 = Some(Pref64Entry {
                        prefix: *prefix,
                        expires_at,
                    });
                }
            }

            RaOption::RouteInfo { prefix, preference, lifetime } => {
                if *lifetime == 0 {
                    state.route_info.retain(|e| !(e.prefix == *prefix && e.router == ra.source));
                } else {
                    let expires_at = now
                        + if *lifetime == LIFETIME_INFINITY {
                            INFINITE_DURATION
                        } else {
                            Duration::from_secs(*lifetime as u64)
                        };
                    if let Some(e) = state
                        .route_info
                        .iter_mut()
                        .find(|e| e.prefix == *prefix && e.router == ra.source)
                    {
                        e.preference = *preference;
                        e.expires_at = expires_at;
                    } else {
                        state.route_info.push(RouteInfoEntry {
                            prefix: *prefix,
                            router: ra.source,
                            preference: *preference,
                            expires_at,
                        });
                    }
                }
            }

            RaOption::Mtu(_) | RaOption::SourceLinkLayerAddress(_) => {
                // Informational; not used in this implementation.
            }
        }
    }
}

// ── Lease event emission ──────────────────────────────────────────────────────

/// Inner implementation shared by `emit_lease_event` and the DHCPv6 arm.
///
/// `has_dhcpv6_lease`: true when a DHCPv6 lease (stateful or stateless) is
/// currently held, so that `LeaseAcquired` can fire even when no SLAAC
/// address is DAD-complete (M-only scenario).
async fn emit_lease_event_inner(
    ra_state: &Ipv6AutoState,
    lease_acquired: &mut bool,
    policy_name: &str,
    state: State,
    event_tx: &mpsc::Sender<FactoryEvent>,
    has_dhcpv6_lease: bool,
) {
    let has_complete_addr = ra_state
        .prefixes
        .values()
        .any(|ps| ps.dad_complete && !ps.dad_failed);
    let all_gone =
        ra_state.prefixes.is_empty() && ra_state.routers.is_empty() && !has_dhcpv6_lease;

    let event = if all_gone && *lease_acquired {
        *lease_acquired = false;
        Some(FactoryEvent::LeaseExpired {
            policy_name: policy_name.to_string(),
        })
    } else if (has_complete_addr || has_dhcpv6_lease) && !*lease_acquired {
        *lease_acquired = true;
        Some(FactoryEvent::LeaseAcquired {
            policy_name: policy_name.to_string(),
            state,
        })
    } else if *lease_acquired {
        Some(FactoryEvent::LeaseRenewed {
            policy_name: policy_name.to_string(),
            state,
        })
    } else {
        None
    };

    if let Some(ev) = event {
        let _ = event_tx.send(ev).await;
    }
}

/// Emit the appropriate `FactoryEvent` based on the current RA state.
///
/// - `LeaseAcquired`: first time a DAD-complete address is available.
/// - `LeaseRenewed`: subsequent state updates while a lease is active.
/// - `LeaseExpired`: all prefixes and routers have expired.
/// - (no event): pre-acquisition, waiting for first DAD to complete.
///
/// This wrapper passes `has_dhcpv6_lease = false`; the run loop calls
/// `emit_lease_event_inner` directly with the real DHCPv6 lease status.
#[cfg(test)]
async fn emit_lease_event(
    ra_state: &Ipv6AutoState,
    lease_acquired: &mut bool,
    policy_name: &str,
    state: State,
    event_tx: &mpsc::Sender<FactoryEvent>,
) {
    emit_lease_event_inner(ra_state, lease_acquired, policy_name, state, event_tx, false).await;
}

// ── DHCPv6 integration helpers ────────────────────────────────────────────────

/// Start, stop, or leave the DHCPv6 client unchanged based on RA M/O flags.
///
/// - M=1: stateful DHCPv6 (IA_NA — acquires addresses and options).
/// - M=0, O=1: stateless DHCPv6 (Information-Request — options only).
/// - M=0, O=0: no DHCPv6.
///
/// When the required mode changes the running client is stopped first.
async fn manage_dhcpv6(
    m: bool,
    o: bool,
    interface: &str,
    client: &mut Option<Dhcpv6Client>,
    result_rx: &mut Option<mpsc::Receiver<Dhcpv6Result>>,
    lease: &mut Option<Dhcpv6Lease>,
    installed_addrs: &mut Vec<Ipv6Addr>,
) {
    let want_stateful = m;
    let want_dhcpv6 = m || o;

    // Stop the running client if the mode no longer matches or DHCPv6 is unneeded.
    if let Some(ref existing) = client {
        let mode_ok = existing.is_stateful() == want_stateful;
        if !want_dhcpv6 || !mode_ok {
            if let Some(mut c) = client.take() {
                let _ = c.stop().await;
            }
            *result_rx = None;
            remove_dhcpv6_addresses(interface, installed_addrs).await;
            *lease = None;
        }
    }

    // Start a new client if needed.
    if want_dhcpv6 && client.is_none() {
        let duid_path = PathBuf::from(
            std::env::var("NETFYR_DUID_PATH")
                .unwrap_or_else(|_| "/var/lib/netfyr/duid".to_string()),
        );
        let (tx, rx) = mpsc::channel(8);
        match Dhcpv6Client::start(interface, want_stateful, &duid_path, tx).await {
            Ok(c) => {
                *client = Some(c);
                *result_rx = Some(rx);
            }
            Err(e) => {
                warn!(interface, error = %e, "Failed to start DHCPv6 client");
            }
        }
    }
}

/// Install new DHCPv6 IA_NA addresses and remove stale ones from a prior lease.
async fn install_dhcpv6_addresses(
    interface: &str,
    lease: &Dhcpv6Lease,
    installed: &mut Vec<Ipv6Addr>,
) {
    let new_addrs: Vec<Ipv6Addr> = lease.addresses.iter().map(|a| a.address).collect();

    // Remove addresses from the previous lease that are not in the new one.
    let stale: Vec<Ipv6Addr> = installed
        .iter()
        .filter(|a| !new_addrs.contains(a))
        .copied()
        .collect();
    for addr in stale {
        if let Err(e) = remove_ipv6_address(interface, addr, 128).await {
            warn!(interface, %addr, error = %e, "Failed to remove stale DHCPv6 address");
        }
        installed.retain(|a| *a != addr);
    }

    // Install addresses that are new.
    for da in &lease.addresses {
        if !installed.contains(&da.address) {
            match add_ipv6_address(interface, da.address, da.prefix_len, da.valid_lft, da.preferred_lft).await {
                Ok(()) => {
                    info!(interface, addr = %da.address, "DHCPv6 address installed");
                    installed.push(da.address);
                }
                Err(e) => {
                    warn!(interface, addr = %da.address, error = %e, "Failed to install DHCPv6 address");
                }
            }
        }
    }
}

/// Remove all DHCPv6 addresses previously installed via `install_dhcpv6_addresses`.
async fn remove_dhcpv6_addresses(interface: &str, installed: &mut Vec<Ipv6Addr>) {
    for addr in installed.drain(..) {
        if let Err(e) = remove_ipv6_address(interface, addr, 128).await {
            warn!(interface, %addr, error = %e, "Failed to remove DHCPv6 address");
        }
    }
}

// ── Lifetime management ───────────────────────────────────────────────────────

/// Deprecate addresses whose preferred lifetime has passed and expire entries
/// whose valid lifetime has passed. Returns `true` if any state changed.
async fn update_lifetimes(state: &mut Ipv6AutoState, interface: &str) -> bool {
    let now = Instant::now();
    let mut changed = false;

    // Deprecate addresses past their preferred lifetime (but still valid).
    for ps in state.prefixes.values_mut() {
        if ps.installed
            && ps.dad_complete
            && !ps.dad_failed
            && !ps.deprecated
            && ps.preferred_expires <= now
            && ps.valid_expires > now
        {
            let valid_remaining = remaining_secs(ps.valid_expires, now) as u32;
            if let Err(e) =
                update_address_lifetime(interface, ps.addr, ps.prefix_len, valid_remaining, 0)
                    .await
            {
                warn!(interface, addr = %ps.addr, error = %e,
                      "Failed to deprecate SLAAC address");
            } else {
                info!(interface, addr = %ps.addr, "SLAAC address deprecated (preferred lifetime expired)");
            }
            ps.deprecated = true;
            changed = true;
        }
    }

    // Expire prefixes whose valid lifetime has passed.
    let expired_nets: Vec<Ipv6Network> = state
        .prefixes
        .iter()
        .filter(|(_, ps)| ps.valid_expires <= now)
        .map(|(net, _)| *net)
        .collect();

    for net in expired_nets {
        if let Some(ps) = state.prefixes.remove(&net) {
            if ps.installed {
                if let Err(e) = remove_ipv6_address(interface, ps.addr, ps.prefix_len).await {
                    warn!(interface, addr = %ps.addr, error = %e,
                          "Failed to remove expired SLAAC address");
                } else {
                    info!(interface, addr = %ps.addr, "SLAAC address valid lifetime expired, removed");
                }
            }
            changed = true;
        }
    }

    // Expire routers.
    let before = state.routers.len();
    state.routers.retain(|_, r| r.expires_at > now);
    if state.routers.len() != before {
        changed = true;
    }

    // Expire RDNSS.
    let before = state.rdnss.len();
    state.rdnss.retain(|e| e.expires_at > now);
    if state.rdnss.len() != before {
        changed = true;
    }

    // Expire DNSSL.
    let before = state.dnssl.len();
    state.dnssl.retain(|e| e.expires_at > now);
    if state.dnssl.len() != before {
        changed = true;
    }

    // Expire PREF64.
    if let Some(p) = &state.pref64 {
        if p.expires_at <= now {
            state.pref64 = None;
            changed = true;
        }
    }

    // Expire Route Information entries.
    let before = state.route_info.len();
    state.route_info.retain(|e| e.expires_at > now);
    if state.route_info.len() != before {
        changed = true;
    }

    changed
}

/// Return the next expiry instant across all tracked state, or `None` if
/// nothing will ever expire (all lifetimes are infinite or there is no state).
///
/// Includes `preferred_expires` for non-deprecated addresses so the timer
/// wakes up to notify the kernel when the preferred lifetime passes.
fn compute_next_expiry(state: &Ipv6AutoState) -> Option<Instant> {
    let mut min: Option<Instant> = None;
    let mut consider = |t: Instant| {
        min = Some(match min {
            Some(m) if m <= t => m,
            _ => t,
        });
    };
    for ps in state.prefixes.values() {
        consider(ps.valid_expires);
        // Wake at preferred_expires to deprecate the address in the kernel.
        if !ps.deprecated {
            consider(ps.preferred_expires);
        }
    }
    for r in state.routers.values() {
        consider(r.expires_at);
    }
    for e in &state.rdnss {
        consider(e.expires_at);
    }
    for e in &state.dnssl {
        consider(e.expires_at);
    }
    if let Some(p) = &state.pref64 {
        consider(p.expires_at);
    }
    for e in &state.route_info {
        consider(e.expires_at);
    }
    min
}

// ── State construction ────────────────────────────────────────────────────────

/// Build a minimal pending `State` with only `enabled: true`.
///
/// Stored immediately on factory start so that `current_state()` is never
/// `None` while the first RA is awaited.
fn pending_state(interface: &str, policy_name: &str, priority: u32) -> State {
    let prov = Provenance::UserConfigured {
        policy_ref: policy_name.to_string(),
    };
    let mut fields = IndexMap::new();
    fields.insert(
        "enabled".to_string(),
        FieldValue {
            value: Value::Bool(true),
            provenance: prov,
        },
    );
    State {
        entity_type: ETHERNET.to_string(),
        selector: Selector::with_name(interface),
        fields,
        metadata: StateMetadata::new(),
        policy_ref: Some(policy_name.to_string()),
        priority,
    }
}

/// Build the full `State` from the current RA-derived information.
///
/// Thin wrapper around `build_merged_state` with no DHCPv6 lease.
/// Tests call this directly and must not break.
#[cfg(test)]
fn build_ra_state(
    ra_state: &Ipv6AutoState,
    interface: &str,
    policy_name: &str,
    priority: u32,
) -> State {
    build_merged_state(ra_state, interface, policy_name, priority, None)
}

/// Build the full `State` merging RA-derived SLAAC data with an optional
/// DHCPv6 lease.
///
/// Addresses are only included when DAD has completed (`dad_complete = true`).
/// DHCPv6 IA_NA addresses (if any) are appended after SLAAC addresses.
/// DNS servers and search domains are merged from RDNSS/DNSSL and DHCPv6
/// option 23/24 without duplicates.
fn build_merged_state(
    ra_state: &Ipv6AutoState,
    interface: &str,
    policy_name: &str,
    priority: u32,
    dhcpv6_lease: Option<&Dhcpv6Lease>,
) -> State {
    let prov = Provenance::UserConfigured {
        policy_ref: policy_name.to_string(),
    };
    let fv = |value: Value| FieldValue {
        value,
        provenance: prov.clone(),
    };

    let now = Instant::now();
    let mut fields = IndexMap::new();
    fields.insert("enabled".to_string(), fv(Value::Bool(true)));

    let mut ipv6_map: IndexMap<String, Value> = IndexMap::new();

    // SLAAC addresses: DAD-complete, not-failed, not-yet-expired.
    // Collect as (Ipv6Addr, Value) pairs so DHCPv6 deduplication can remove overlaps.
    let mut slaac_addrs: Vec<(Ipv6Addr, Value)> = ra_state
        .prefixes
        .values()
        .filter(|ps| ps.dad_complete && !ps.dad_failed && ps.valid_expires > now)
        .map(|ps| {
            let cidr = format!("{}/{}", ps.addr, ps.prefix_len);
            let net: IpNetwork = cidr.parse().unwrap_or_else(|_| {
                IpNetwork::V6(Ipv6Network::new(ps.addr, ps.prefix_len).unwrap())
            });
            let valid_secs = remaining_secs(ps.valid_expires, now);
            let pref_secs = remaining_secs(ps.preferred_expires, now);
            let mut m = IndexMap::new();
            m.insert("address".to_string(), Value::IpNetwork(net));
            m.insert("valid_lft".to_string(), Value::U64(valid_secs));
            m.insert("preferred_lft".to_string(), Value::U64(pref_secs));
            (ps.addr, Value::Map(m))
        })
        .collect();

    // DHCPv6 IA_NA addresses (stateful only; stateless has empty addresses vec).
    // If both SLAAC and DHCPv6 produce the same address, prefer the DHCPv6 version
    // (it carries server-assigned lifetimes).
    let mut dhcpv6_addr_list: Vec<Value> = Vec::new();
    if let Some(lease) = dhcpv6_lease {
        for da in &lease.addresses {
            // Remove any SLAAC entry with the same address before appending the DHCPv6 version.
            slaac_addrs.retain(|(slaac_ip, _)| *slaac_ip != da.address);
            let net: IpNetwork = format!("{}/{}", da.address, da.prefix_len)
                .parse()
                .unwrap_or_else(|_| {
                    IpNetwork::V6(Ipv6Network::new(da.address, da.prefix_len).unwrap())
                });
            let mut m = IndexMap::new();
            m.insert("address".to_string(), Value::IpNetwork(net));
            m.insert("valid_lft".to_string(), Value::U64(da.valid_lft as u64));
            m.insert("preferred_lft".to_string(), Value::U64(da.preferred_lft as u64));
            dhcpv6_addr_list.push(Value::Map(m));
        }
    }
    let mut addr_list: Vec<Value> = slaac_addrs.into_iter().map(|(_, v)| v).collect();
    addr_list.extend(dhcpv6_addr_list);

    if !addr_list.is_empty() {
        ipv6_map.insert("addresses".to_string(), Value::List(addr_list));
    }

    // Routes: default routes from active routers (router_lifetime > 0).
    let mut route_list: Vec<Value> = ra_state
        .routers
        .iter()
        .filter(|(_, r)| r.expires_at > now)
        .map(|(router_addr, _)| {
            let mut m = IndexMap::new();
            m.insert(
                "destination".to_string(),
                Value::IpNetwork("::/0".parse().expect("valid default IPv6 route")),
            );
            m.insert(
                "gateway".to_string(),
                Value::IpAddr(std::net::IpAddr::V6(*router_addr)),
            );
            m.insert("metric".to_string(), Value::U64(100));
            Value::Map(m)
        })
        .collect();

    // Append additional routes from Route Information options (RFC 4191).
    for entry in ra_state.route_info.iter().filter(|e| e.expires_at > now) {
        let mut m = IndexMap::new();
        m.insert(
            "destination".to_string(),
            Value::IpNetwork(IpNetwork::V6(entry.prefix)),
        );
        m.insert(
            "gateway".to_string(),
            Value::IpAddr(std::net::IpAddr::V6(entry.router)),
        );
        m.insert("metric".to_string(), Value::U64(preference_to_metric(entry.preference)));
        route_list.push(Value::Map(m));
    }

    if !route_list.is_empty() {
        ipv6_map.insert("routes".to_string(), Value::List(route_list));
    }

    // DNS servers: RDNSS addresses first, then DHCPv6 option 23 without duplicates.
    let mut dns_seen: Vec<String> = Vec::new();
    let mut dns_servers: Vec<Value> = Vec::new();
    for addr in ra_state
        .rdnss
        .iter()
        .filter(|e| e.expires_at > now)
        .flat_map(|e| e.addresses.iter())
    {
        let s = addr.to_string();
        if !dns_seen.contains(&s) {
            dns_seen.push(s.clone());
            dns_servers.push(Value::String(s));
        }
    }
    if let Some(lease) = dhcpv6_lease {
        for addr in &lease.dns_servers {
            let s = addr.to_string();
            if !dns_seen.contains(&s) {
                dns_seen.push(s.clone());
                dns_servers.push(Value::String(s));
            }
        }
    }
    if !dns_servers.is_empty() {
        ipv6_map.insert("dns_servers".to_string(), Value::List(dns_servers));
    }

    // DNS search domains: DNSSL first, then DHCPv6 option 24 without duplicates.
    let mut search_seen: Vec<String> = Vec::new();
    let mut dns_search: Vec<Value> = Vec::new();
    for d in ra_state
        .dnssl
        .iter()
        .filter(|e| e.expires_at > now)
        .flat_map(|e| e.domains.iter())
    {
        if !search_seen.contains(d) {
            search_seen.push(d.clone());
            dns_search.push(Value::String(d.clone()));
        }
    }
    if let Some(lease) = dhcpv6_lease {
        for d in &lease.dns_search {
            if !search_seen.contains(d) {
                search_seen.push(d.clone());
                dns_search.push(Value::String(d.clone()));
            }
        }
    }
    if !dns_search.is_empty() {
        ipv6_map.insert("dns_search".to_string(), Value::List(dns_search));
    }

    // NAT64 prefix from PREF64 option.
    if let Some(p64) = &ra_state.pref64 {
        if p64.expires_at > now {
            ipv6_map.insert(
                "nat64_prefix".to_string(),
                Value::IpNetwork(IpNetwork::V6(p64.prefix)),
            );
        }
    }

    fields.insert("ipv6".to_string(), fv(Value::Map(ipv6_map)));

    State {
        entity_type: ETHERNET.to_string(),
        selector: Selector::with_name(interface),
        fields,
        metadata: StateMetadata::new(),
        policy_ref: Some(policy_name.to_string()),
        priority,
    }
}

/// Return seconds remaining until `expires_at`, or 0 if already past.
fn remaining_secs(expires_at: Instant, now: Instant) -> u64 {
    if expires_at > now {
        (expires_at - now).as_secs()
    } else {
        0
    }
}

// ── Procfs helpers ────────────────────────────────────────────────────────────

/// Write `value` to `/proc/sys/net/ipv6/conf/{name}/accept_ra`.
fn write_procfs_accept_ra(name: &str, value: u8) -> Result<(), String> {
    let path = format!("/proc/sys/net/ipv6/conf/{name}/accept_ra");
    std::fs::write(&path, format!("{value}\n"))
        .map_err(|e| format!("write {path}: {e}"))
}

/// Write `value` to `/proc/sys/net/ipv6/conf/{name}/autoconf`.
fn write_procfs_autoconf(name: &str, value: u8) -> Result<(), String> {
    let path = format!("/proc/sys/net/ipv6/conf/{name}/autoconf");
    std::fs::write(&path, format!("{value}\n"))
        .map_err(|e| format!("write {path}: {e}"))
}

/// Write `value` to `/proc/sys/net/ipv6/conf/{name}/addr_gen_mode`.
fn write_procfs_addr_gen_mode(name: &str, value: u8) -> Result<(), String> {
    let path = format!("/proc/sys/net/ipv6/conf/{name}/addr_gen_mode");
    std::fs::write(&path, format!("{value}\n"))
        .map_err(|e| format!("write {path}: {e}"))
}

/// Map RFC 4191 route preference to a kernel metric value.
///
/// High (1) → 50 (highest priority), medium (0) → 100 (default route metric),
/// low (3) → 200 (lowest priority).
fn preference_to_metric(preference: u8) -> u64 {
    match preference {
        1 => 50,  // high
        3 => 200, // low
        _ => 100, // medium (0) or unknown
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Build a PrefixState with DAD complete and a specified expiry.
    fn make_prefix_state(
        addr: Ipv6Addr,
        dad_complete: bool,
        dad_failed: bool,
        valid_expires: Instant,
        preferred_expires: Instant,
    ) -> PrefixState {
        PrefixState {
            addr,
            prefix_len: 64,
            valid_expires,
            preferred_expires,
            on_link: true,
            installed: true,
            dad_complete,
            dad_failed,
            deprecated: false,
        }
    }

    fn far_future() -> Instant {
        Instant::now() + Duration::from_secs(86400)
    }

    fn past() -> Instant {
        // Instant in the past; tokio::time::Instant cannot be constructed directly
        // as past — use now() and accept it as "just expired".
        Instant::now()
    }

    // ── pending_state ─────────────────────────────────────────────────────────

    /// Scenario: current_state returns pending state before RA
    /// Given a newly started factory, current_state() returns Some(State) with
    /// enabled=true and no ipv6 sub-object.
    #[test]
    fn test_pending_state_has_enabled_true() {
        let state = pending_state("eth0", "test-policy", 100);
        let enabled = state
            .fields
            .get("enabled")
            .and_then(|fv| fv.value.as_bool());
        assert_eq!(enabled, Some(true), "pending state must have enabled=true");
    }

    /// Scenario: Pending state has no ipv6 sub-object.
    #[test]
    fn test_pending_state_has_no_ipv6_field() {
        let state = pending_state("eth0", "test-policy", 100);
        assert!(
            state.fields.get("ipv6").is_none(),
            "pending state must not contain an ipv6 field"
        );
    }

    /// Scenario: Pending state entity_type is "ethernet".
    #[test]
    fn test_pending_state_entity_type_is_ethernet() {
        let state = pending_state("eth0", "test-policy", 100);
        assert_eq!(
            state.entity_type,
            netfyr_state::entity_types::ETHERNET,
            "pending state entity_type must be ethernet"
        );
    }

    /// Scenario: Pending state selector matches interface name.
    #[test]
    fn test_pending_state_selector_matches_interface() {
        let state = pending_state("veth-v6-0", "my-policy", 50);
        assert_eq!(
            state.selector.key(),
            "veth-v6-0",
            "pending state selector key must match interface name"
        );
    }

    /// Scenario: Pending state policy_ref matches policy name.
    #[test]
    fn test_pending_state_policy_ref_matches_policy_name() {
        let state = pending_state("eth0", "my-ipv6-policy", 100);
        assert_eq!(
            state.policy_ref.as_deref(),
            Some("my-ipv6-policy"),
            "pending state policy_ref must match policy name"
        );
    }

    // ── remaining_secs ────────────────────────────────────────────────────────

    /// Scenario: remaining_secs returns 0 for a past/equal instant.
    #[test]
    fn test_remaining_secs_past_returns_zero() {
        let now = Instant::now();
        // past() == now() is exactly at the boundary → remaining_secs returns 0
        // because the condition is `expires_at > now`.
        assert_eq!(
            remaining_secs(now, now),
            0,
            "remaining_secs at the exact expiry time must return 0"
        );
    }

    /// Scenario: remaining_secs returns positive value for future instant.
    #[test]
    fn test_remaining_secs_future_returns_positive() {
        let now = Instant::now();
        let future = now + Duration::from_secs(3600);
        let secs = remaining_secs(future, now);
        assert!(secs > 0, "remaining_secs for future instant must be positive");
        assert!(secs <= 3600, "remaining_secs must not exceed the duration");
    }

    // ── compute_next_expiry ───────────────────────────────────────────────────

    /// Scenario: compute_next_expiry returns None for empty state.
    #[test]
    fn test_compute_next_expiry_empty_state_returns_none() {
        let state = Ipv6AutoState::default();
        assert!(
            compute_next_expiry(&state).is_none(),
            "empty state must have no next expiry"
        );
    }

    /// Scenario: compute_next_expiry returns prefix valid_expires.
    #[test]
    fn test_compute_next_expiry_with_prefix_returns_valid_expires() {
        let mut state = Ipv6AutoState::default();
        let net: Ipv6Network = "2001:db8::/64".parse().unwrap();
        let valid_at = Instant::now() + Duration::from_secs(100);
        state.prefixes.insert(
            net,
            make_prefix_state(
                "2001:db8::1".parse().unwrap(),
                true,
                false,
                valid_at,
                Instant::now() + Duration::from_secs(50),
            ),
        );
        let next = compute_next_expiry(&state);
        assert!(next.is_some(), "state with a prefix must have a next expiry");
    }

    /// Scenario: compute_next_expiry returns minimum across prefix and router.
    #[test]
    fn test_compute_next_expiry_returns_minimum_instant() {
        let mut state = Ipv6AutoState::default();
        let prefix_net: Ipv6Network = "2001:db8::/64".parse().unwrap();
        let router_addr: Ipv6Addr = "fe80::1".parse().unwrap();

        let soon = Instant::now() + Duration::from_secs(10);
        let later = Instant::now() + Duration::from_secs(100);

        state.prefixes.insert(
            prefix_net,
            make_prefix_state(
                "2001:db8::1".parse().unwrap(),
                true,
                false,
                later,
                later,
            ),
        );
        state.routers.insert(router_addr, RouterState { expires_at: soon });

        let next = compute_next_expiry(&state).unwrap();
        // Should be the router's expiry (sooner)
        assert!(
            next <= soon + Duration::from_millis(1),
            "compute_next_expiry must return the earliest expiry"
        );
    }

    // ── build_ra_state ────────────────────────────────────────────────────────

    /// Scenario: build_ra_state with empty state produces ipv6 field but no addresses.
    #[test]
    fn test_build_ra_state_empty_has_no_addresses() {
        let state = Ipv6AutoState::default();
        let result = build_ra_state(&state, "eth0", "test-policy", 100);
        let ipv6 = result.fields.get("ipv6").expect("ipv6 field must be present");
        let map = ipv6.value.as_map().expect("ipv6 value must be a map");
        assert!(
            map.get("addresses").is_none(),
            "empty RA state must have no addresses"
        );
    }

    /// Scenario: Produced state contains correct ipv6 sub-object fields — addresses
    /// Given a DAD-complete prefix, build_ra_state includes that address.
    #[test]
    fn test_build_ra_state_dad_complete_address_included() {
        let mut state = Ipv6AutoState::default();
        let net: Ipv6Network = "2001:db8::/64".parse().unwrap();
        state.prefixes.insert(
            net,
            make_prefix_state(
                "2001:db8::a8bb:ccff:fedd:eeff".parse().unwrap(),
                true,  // dad_complete
                false, // dad_failed
                far_future(),
                far_future(),
            ),
        );

        let result = build_ra_state(&state, "eth0", "test-policy", 100);
        let ipv6 = result.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        let addrs = map.get("addresses").expect("addresses must be present");
        let list = addrs.as_list().expect("addresses must be a list");
        assert_eq!(list.len(), 1, "one DAD-complete address must appear");
    }

    /// Scenario: Produced state excludes address when DAD is still pending.
    #[test]
    fn test_build_ra_state_dad_pending_address_excluded() {
        let mut state = Ipv6AutoState::default();
        let net: Ipv6Network = "2001:db8::/64".parse().unwrap();
        state.prefixes.insert(
            net,
            make_prefix_state(
                "2001:db8::1".parse().unwrap(),
                false, // dad_complete = false (pending)
                false,
                far_future(),
                far_future(),
            ),
        );

        let result = build_ra_state(&state, "eth0", "test-policy", 100);
        let ipv6 = result.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        assert!(
            map.get("addresses").is_none(),
            "DAD-pending address must not appear in produced state"
        );
    }

    /// Scenario: DAD failure — address not included in produced state.
    #[test]
    fn test_build_ra_state_dad_failed_address_excluded() {
        let mut state = Ipv6AutoState::default();
        let net: Ipv6Network = "2001:db8::/64".parse().unwrap();
        state.prefixes.insert(
            net,
            make_prefix_state(
                "2001:db8::1".parse().unwrap(),
                true,  // dad_complete=true but also dad_failed=true
                true,  // dad_failed
                far_future(),
                far_future(),
            ),
        );

        let result = build_ra_state(&state, "eth0", "test-policy", 100);
        let ipv6 = result.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        assert!(
            map.get("addresses").is_none(),
            "DAD-failed address must not appear in produced state"
        );
    }

    /// Scenario: Produced state contains default route via active router.
    #[test]
    fn test_build_ra_state_active_router_included_as_route() {
        let mut state = Ipv6AutoState::default();
        let router: Ipv6Addr = "fe80::1".parse().unwrap();
        state.routers.insert(router, RouterState { expires_at: far_future() });

        let result = build_ra_state(&state, "eth0", "test-policy", 100);
        let ipv6 = result.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        let routes = map.get("routes").expect("routes must be present");
        let route_list = routes.as_list().unwrap();
        assert_eq!(route_list.len(), 1, "one router must produce one default route");

        let route = route_list[0].as_map().unwrap();
        let dest = route.get("destination").unwrap().to_string();
        assert!(dest.contains("::/0"), "default route destination must be ::/0");
    }

    /// Scenario: Expired router is not included in routes.
    #[test]
    fn test_build_ra_state_expired_router_excluded_from_routes() {
        let mut state = Ipv6AutoState::default();
        let router: Ipv6Addr = "fe80::1".parse().unwrap();
        // Expired router: expires_at = now (not > now)
        state.routers.insert(router, RouterState { expires_at: past() });

        let result = build_ra_state(&state, "eth0", "test-policy", 100);
        let ipv6 = result.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        assert!(
            map.get("routes").is_none(),
            "expired router must not produce a route"
        );
    }

    /// Scenario: Produced state contains dns_servers from RDNSS.
    #[test]
    fn test_build_ra_state_rdnss_included_as_dns_servers() {
        let mut state = Ipv6AutoState::default();
        let dns: Ipv6Addr = "2001:db8::53".parse().unwrap();
        state.rdnss.push(RdnssEntry {
            addresses: vec![dns],
            expires_at: far_future(),
        });

        let result = build_ra_state(&state, "eth0", "test-policy", 100);
        let ipv6 = result.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        let servers = map.get("dns_servers").expect("dns_servers must be present");
        let list = servers.as_list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].as_str(), Some("2001:db8::53"));
    }

    /// Scenario: RDNSS lifetime expiry removes DNS servers.
    #[test]
    fn test_build_ra_state_expired_rdnss_excluded() {
        let mut state = Ipv6AutoState::default();
        state.rdnss.push(RdnssEntry {
            addresses: vec!["2001:db8::53".parse().unwrap()],
            expires_at: past(), // expired
        });

        let result = build_ra_state(&state, "eth0", "test-policy", 100);
        let ipv6 = result.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        assert!(
            map.get("dns_servers").is_none(),
            "expired RDNSS must not appear in dns_servers"
        );
    }

    /// Scenario: Produced state contains dns_search from DNSSL.
    #[test]
    fn test_build_ra_state_dnssl_included_as_dns_search() {
        let mut state = Ipv6AutoState::default();
        state.dnssl.push(DnsslEntry {
            domains: vec!["example.com".to_string()],
            expires_at: far_future(),
        });

        let result = build_ra_state(&state, "eth0", "test-policy", 100);
        let ipv6 = result.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        let search = map.get("dns_search").expect("dns_search must be present");
        let list = search.as_list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].as_str(), Some("example.com"));
    }

    /// Scenario: Produced state contains nat64_prefix from PREF64.
    #[test]
    fn test_build_ra_state_pref64_included_as_nat64_prefix() {
        let mut state = Ipv6AutoState::default();
        let prefix: Ipv6Network = "64:ff9b::/96".parse().unwrap();
        state.pref64 = Some(Pref64Entry {
            prefix,
            expires_at: far_future(),
        });

        let result = build_ra_state(&state, "eth0", "test-policy", 100);
        let ipv6 = result.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        let nat64 = map.get("nat64_prefix").expect("nat64_prefix must be present");
        let nat64_str = nat64.to_string();
        assert!(
            nat64_str.contains("64:ff9b") && nat64_str.contains("/96"),
            "nat64_prefix must include 64:ff9b::/96, got: {nat64_str}"
        );
    }

    /// Scenario: Expired PREF64 is excluded from produced state.
    #[test]
    fn test_build_ra_state_expired_pref64_excluded() {
        let mut state = Ipv6AutoState::default();
        state.pref64 = Some(Pref64Entry {
            prefix: "64:ff9b::/96".parse().unwrap(),
            expires_at: past(), // expired
        });

        let result = build_ra_state(&state, "eth0", "test-policy", 100);
        let ipv6 = result.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        assert!(
            map.get("nat64_prefix").is_none(),
            "expired PREF64 must not appear in nat64_prefix"
        );
    }

    /// Scenario: build_ra_state always includes enabled=true.
    #[test]
    fn test_build_ra_state_always_has_enabled_true() {
        let state = Ipv6AutoState::default();
        let result = build_ra_state(&state, "eth0", "test-policy", 100);
        let enabled = result
            .fields
            .get("enabled")
            .and_then(|fv| fv.value.as_bool());
        assert_eq!(enabled, Some(true), "produced state must always have enabled=true");
    }

    /// Scenario: Multiple prefixes from same router — two addresses in ipv6.addresses.
    #[test]
    fn test_build_ra_state_multiple_prefixes_produce_multiple_addresses() {
        let mut state = Ipv6AutoState::default();
        let net1: Ipv6Network = "2001:db8:1::/64".parse().unwrap();
        let net2: Ipv6Network = "2001:db8:2::/64".parse().unwrap();
        state.prefixes.insert(
            net1,
            make_prefix_state(
                "2001:db8:1::a8bb:ccff:fedd:eeff".parse().unwrap(),
                true, false, far_future(), far_future(),
            ),
        );
        state.prefixes.insert(
            net2,
            make_prefix_state(
                "2001:db8:2::a8bb:ccff:fedd:eeff".parse().unwrap(),
                true, false, far_future(), far_future(),
            ),
        );

        let result = build_ra_state(&state, "eth0", "test-policy", 100);
        let ipv6 = result.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        let addrs = map.get("addresses").unwrap().as_list().unwrap();
        assert_eq!(addrs.len(), 2, "two DAD-complete prefixes must produce two addresses");
    }

    /// Scenario: Multiple routers on same link — both produce default routes.
    #[test]
    fn test_build_ra_state_multiple_routers_produce_multiple_routes() {
        let mut state = Ipv6AutoState::default();
        state.routers.insert(
            "fe80::1".parse().unwrap(),
            RouterState { expires_at: far_future() },
        );
        state.routers.insert(
            "fe80::2".parse().unwrap(),
            RouterState { expires_at: far_future() },
        );

        let result = build_ra_state(&state, "eth0", "test-policy", 100);
        let ipv6 = result.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        let routes = map.get("routes").unwrap().as_list().unwrap();
        assert_eq!(routes.len(), 2, "two active routers must produce two default routes");
    }

    // ── emit_lease_event ──────────────────────────────────────────────────────

    /// Scenario: emit_lease_event sends LeaseAcquired on first DAD-complete address.
    #[tokio::test]
    async fn test_emit_lease_event_sends_lease_acquired_on_first_dad_complete() {
        let mut ra_state = Ipv6AutoState::default();
        ra_state.prefixes.insert(
            "2001:db8::/64".parse().unwrap(),
            make_prefix_state(
                "2001:db8::1".parse().unwrap(),
                true, false, far_future(), far_future(),
            ),
        );

        let (tx, mut rx) = mpsc::channel(4);
        let placeholder_state = pending_state("eth0", "test", 100);
        let mut lease_acquired = false;

        emit_lease_event(&ra_state, &mut lease_acquired, "test", placeholder_state, &tx).await;

        let event = rx.try_recv().expect("LeaseAcquired must be sent");
        assert!(
            matches!(event, FactoryEvent::LeaseAcquired { .. }),
            "first DAD-complete address must emit LeaseAcquired"
        );
        assert!(lease_acquired, "lease_acquired flag must be set to true");
    }

    /// Scenario: emit_lease_event sends LeaseRenewed on subsequent updates.
    #[tokio::test]
    async fn test_emit_lease_event_sends_lease_renewed_when_already_acquired() {
        let mut ra_state = Ipv6AutoState::default();
        ra_state.prefixes.insert(
            "2001:db8::/64".parse().unwrap(),
            make_prefix_state(
                "2001:db8::1".parse().unwrap(),
                true, false, far_future(), far_future(),
            ),
        );

        let (tx, mut rx) = mpsc::channel(4);
        let mut lease_acquired = true; // Already acquired

        emit_lease_event(&ra_state, &mut lease_acquired, "test", pending_state("eth0", "test", 100), &tx).await;

        let event = rx.try_recv().expect("LeaseRenewed must be sent");
        assert!(
            matches!(event, FactoryEvent::LeaseRenewed { .. }),
            "subsequent update with active lease must emit LeaseRenewed"
        );
    }

    /// Scenario: All prefixes expired sends LeaseExpired.
    #[tokio::test]
    async fn test_emit_lease_event_sends_lease_expired_when_all_gone() {
        let ra_state = Ipv6AutoState::default(); // No prefixes, no routers

        let (tx, mut rx) = mpsc::channel(4);
        let mut lease_acquired = true; // Was previously acquired

        emit_lease_event(&ra_state, &mut lease_acquired, "test", pending_state("eth0", "test", 100), &tx).await;

        let event = rx.try_recv().expect("LeaseExpired must be sent");
        assert!(
            matches!(event, FactoryEvent::LeaseExpired { .. }),
            "all prefixes and routers expired must emit LeaseExpired"
        );
        assert!(!lease_acquired, "lease_acquired must be reset to false on LeaseExpired");
    }

    /// Scenario: No event sent before first DAD-complete address.
    #[tokio::test]
    async fn test_emit_lease_event_no_event_before_first_dad_complete() {
        let mut ra_state = Ipv6AutoState::default();
        // Prefix present but DAD not complete
        ra_state.prefixes.insert(
            "2001:db8::/64".parse().unwrap(),
            make_prefix_state(
                "2001:db8::1".parse().unwrap(),
                false, false, far_future(), far_future(), // dad_complete=false
            ),
        );

        let (tx, mut rx) = mpsc::channel(4);
        let mut lease_acquired = false;

        emit_lease_event(&ra_state, &mut lease_acquired, "test", pending_state("eth0", "test", 100), &tx).await;

        assert!(
            rx.try_recv().is_err(),
            "no event must be sent while DAD is pending"
        );
    }

    // ── update_lifetimes ──────────────────────────────────────────────────────

    /// Scenario: Router lifetime expiry removes default route.
    /// We test the state mutation without touching netlink (no installed prefixes).
    #[tokio::test]
    async fn test_update_lifetimes_expired_router_removed() {
        let mut state = Ipv6AutoState::default();
        let router: Ipv6Addr = "fe80::1".parse().unwrap();
        state.routers.insert(router, RouterState { expires_at: past() });

        let changed = update_lifetimes(&mut state, "nonexistent-iface-for-test").await;
        assert!(changed, "update_lifetimes must return true when router expires");
        assert!(
            state.routers.is_empty(),
            "expired router must be removed from state"
        );
    }

    /// Scenario: RDNSS lifetime expiry removes DNS servers.
    #[tokio::test]
    async fn test_update_lifetimes_expired_rdnss_removed() {
        let mut state = Ipv6AutoState::default();
        state.rdnss.push(RdnssEntry {
            addresses: vec!["2001:db8::53".parse().unwrap()],
            expires_at: past(),
        });

        let changed = update_lifetimes(&mut state, "nonexistent-iface-for-test").await;
        assert!(changed, "update_lifetimes must return true when RDNSS expires");
        assert!(state.rdnss.is_empty(), "expired RDNSS entry must be removed");
    }

    /// Scenario: DNSSL lifetime expiry removes search domains.
    #[tokio::test]
    async fn test_update_lifetimes_expired_dnssl_removed() {
        let mut state = Ipv6AutoState::default();
        state.dnssl.push(DnsslEntry {
            domains: vec!["example.com".to_string()],
            expires_at: past(),
        });

        let changed = update_lifetimes(&mut state, "nonexistent-iface-for-test").await;
        assert!(changed, "update_lifetimes must return true when DNSSL expires");
        assert!(state.dnssl.is_empty(), "expired DNSSL entry must be removed");
    }

    /// Scenario: PREF64 lifetime expiry removes NAT64 prefix.
    #[tokio::test]
    async fn test_update_lifetimes_expired_pref64_removed() {
        let mut state = Ipv6AutoState::default();
        state.pref64 = Some(Pref64Entry {
            prefix: "64:ff9b::/96".parse().unwrap(),
            expires_at: past(),
        });

        let changed = update_lifetimes(&mut state, "nonexistent-iface-for-test").await;
        assert!(changed, "update_lifetimes must return true when PREF64 expires");
        assert!(state.pref64.is_none(), "expired PREF64 must be removed");
    }

    /// Scenario: Non-expired state returns changed=false.
    #[tokio::test]
    async fn test_update_lifetimes_non_expired_state_returns_unchanged() {
        let mut state = Ipv6AutoState::default();
        let router: Ipv6Addr = "fe80::1".parse().unwrap();
        state.routers.insert(router, RouterState { expires_at: far_future() });

        let changed = update_lifetimes(&mut state, "nonexistent-iface-for-test").await;
        assert!(!changed, "non-expired state must return changed=false");
        assert!(!state.routers.is_empty(), "non-expired router must remain");
    }

    // ── FactoryManager integration: ipv6auto policy without selector ──────────

    /// Scenario: Ipv6Auto policy without selector is reported in failed list.
    #[tokio::test]
    async fn test_factory_manager_ipv6auto_without_selector_in_failed_list() {
        use crate::factory_manager::FactoryManager;
        use netfyr_policy::{FactoryType, Policy};

        let mut mgr = FactoryManager::new();
        let policy = Policy {
            name: "ipv6auto-no-selector".to_string(),
            factory_type: FactoryType::Ipv6Auto,
            priority: 100,
            state: None,
            states: None,
            selector: None,
        };
        let failed = mgr.sync(&[policy]).await.unwrap();
        assert!(
            failed.contains(&"ipv6auto-no-selector".to_string()),
            "Ipv6Auto policy with no selector must appear in the failed list"
        );
    }

    /// Scenario: Ipv6Auto policy with nonexistent interface fails gracefully.
    #[tokio::test]
    async fn test_factory_manager_ipv6auto_nonexistent_interface_fails_gracefully() {
        use crate::factory_manager::FactoryManager;
        use netfyr_policy::{FactoryType, Policy};
        use netfyr_state::Selector;

        let mut mgr = FactoryManager::new();
        let policy = Policy {
            name: "ipv6auto-no-iface".to_string(),
            factory_type: FactoryType::Ipv6Auto,
            priority: 100,
            state: None,
            states: None,
            selector: Some(Selector::with_name("eth99999-nonexistent-iface")),
        };
        let result = mgr.sync(&[policy]).await;
        assert!(result.is_ok(), "sync must succeed at Result level even when factory fails");
        let failed = result.unwrap();
        assert!(
            failed.contains(&"ipv6auto-no-iface".to_string()),
            "Ipv6Auto policy for nonexistent interface must appear in failed list"
        );
    }

    /// Scenario: FactoryManager status shows has_lease=false when no addresses yet.
    #[tokio::test]
    async fn test_factory_manager_ipv6auto_status_has_lease_false_before_address() {
        use crate::factory_manager::FactoryManager;
        use netfyr_policy::{FactoryType, Policy};
        use netfyr_state::Selector;

        let mut mgr = FactoryManager::new();
        // Loopback always exists; IPv6 SLAAC will not produce addresses there
        // (no RA), but the factory starts with pending state.
        let policy = Policy {
            name: "ipv6auto-lo".to_string(),
            factory_type: FactoryType::Ipv6Auto,
            priority: 100,
            state: None,
            states: None,
            selector: Some(Selector::with_name("lo")),
        };
        mgr.sync(&[policy]).await.unwrap();

        let statuses = mgr.factory_statuses();
        let status = statuses
            .iter()
            .find(|s| s.policy_name == "ipv6auto-lo");

        if let Some(s) = status {
            assert_eq!(s.factory_type, "ipv6auto");
            assert!(!s.has_lease, "ipv6auto factory before RA must report has_lease=false");
        }
        // The factory may have failed to start on lo (no MAC etc.); that's acceptable.
        // The key guarantee is that failed factories don't leak a has_lease=true status.

        mgr.stop_all().await.unwrap();
    }

    // ── M/O flag tracking logic ───────────────────────────────────────────────

    /// Replicates the M/O flag change detection logic from run_ipv6auto.
    /// This lets us unit-test the tracking without spawning the full background task.
    fn mo_flags_changed(state: &Ipv6AutoState, m: bool, o: bool) -> bool {
        state.last_m != Some(m) || state.last_o != Some(o)
    }

    /// Scenario: M/O flags are reported on first RA — initial state is None so any flags trigger.
    #[test]
    fn test_mo_flag_tracking_initial_state_is_none() {
        let state = Ipv6AutoState::default();
        assert!(state.last_m.is_none(), "initial last_m must be None");
        assert!(state.last_o.is_none(), "initial last_o must be None");
    }

    /// Scenario: M/O flags are reported to daemon via FactoryEvent — first RA always triggers.
    #[test]
    fn test_mo_flag_tracking_first_ra_always_triggers_change() {
        let state = Ipv6AutoState::default();
        assert!(mo_flags_changed(&state, false, false), "first RA M=0,O=0 must trigger change");
        assert!(mo_flags_changed(&state, true, false),  "first RA M=1,O=0 must trigger change");
        assert!(mo_flags_changed(&state, false, true),  "first RA M=0,O=1 must trigger change");
        assert!(mo_flags_changed(&state, true, true),   "first RA M=1,O=1 must trigger change");
    }

    /// Scenario: M/O flags unchanged does not send duplicate event.
    #[test]
    fn test_mo_flag_tracking_same_flags_not_a_change() {
        let mut state = Ipv6AutoState::default();
        state.last_m = Some(false);
        state.last_o = Some(true);
        assert!(
            !mo_flags_changed(&state, false, true),
            "identical M=0,O=1 must not be detected as a change"
        );
        state.last_m = Some(true);
        state.last_o = Some(true);
        assert!(
            !mo_flags_changed(&state, true, true),
            "identical M=1,O=1 must not be detected as a change"
        );
    }

    /// Scenario: M/O flag change triggers new event — O flag changes.
    #[test]
    fn test_mo_flag_tracking_o_flag_change_detected() {
        let mut state = Ipv6AutoState::default();
        state.last_m = Some(false);
        state.last_o = Some(false);
        assert!(
            mo_flags_changed(&state, false, true),
            "O flag change from false to true must be detected"
        );
    }

    /// Scenario: M/O flag change triggers new event — M flag changes.
    #[test]
    fn test_mo_flag_tracking_m_flag_change_detected() {
        let mut state = Ipv6AutoState::default();
        state.last_m = Some(false);
        state.last_o = Some(false);
        assert!(
            mo_flags_changed(&state, true, false),
            "M flag change from false to true must be detected"
        );
    }

    /// Scenario: FactoryEvent::Ipv6AutoFlags carries correct M and O values.
    #[test]
    fn test_factory_event_ipv6auto_flags_carries_correct_values() {
        let event = FactoryEvent::Ipv6AutoFlags {
            policy_name: "eth0-ipv6".to_string(),
            m: true,
            o: false,
        };
        match event {
            FactoryEvent::Ipv6AutoFlags { policy_name, m, o } => {
                assert_eq!(policy_name, "eth0-ipv6");
                assert!(m, "M flag must be true");
                assert!(!o, "O flag must be false");
            }
            _ => panic!("expected Ipv6AutoFlags variant"),
        }
    }

    /// Scenario: Ipv6AutoFlags with M=1 O=1 (both set).
    #[test]
    fn test_factory_event_ipv6auto_flags_both_set() {
        let event = FactoryEvent::Ipv6AutoFlags {
            policy_name: "my-policy".to_string(),
            m: true,
            o: true,
        };
        match event {
            FactoryEvent::Ipv6AutoFlags { m, o, .. } => {
                assert!(m && o, "both M and O must be true");
            }
            _ => panic!("expected Ipv6AutoFlags variant"),
        }
    }

    // ── Prefix deprecation ────────────────────────────────────────────────────

    /// Scenario: Prefix deprecation on preferred lifetime expiry.
    /// The address is still present in the state but with preferred_lft=0.
    #[test]
    fn test_build_ra_state_deprecated_address_has_preferred_lft_zero() {
        let mut state = Ipv6AutoState::default();
        let net: Ipv6Network = "2001:db8::/64".parse().unwrap();
        state.prefixes.insert(
            net,
            PrefixState {
                addr: "2001:db8::a8bb:ccff:fedd:eeff".parse().unwrap(),
                prefix_len: 64,
                valid_expires: far_future(), // still valid
                preferred_expires: past(),   // preferred lifetime expired
                on_link: true,
                installed: true,
                dad_complete: true,
                dad_failed: false,
                deprecated: true,
            },
        );
        let result = build_ra_state(&state, "eth0", "test-policy", 100);
        let ipv6 = result.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        let addrs = map
            .get("addresses")
            .expect("deprecated but still valid address must appear in state");
        let addr_list = addrs.as_list().unwrap();
        assert_eq!(addr_list.len(), 1, "one deprecated address must appear");
        let addr_map = addr_list[0].as_map().unwrap();
        let pref_lft = addr_map
            .get("preferred_lft")
            .expect("address map must have preferred_lft")
            .as_u64()
            .expect("preferred_lft must be u64");
        assert_eq!(pref_lft, 0, "deprecated address must have preferred_lft=0 in produced state");
    }

    // ── Address map field structure ───────────────────────────────────────────

    /// Scenario: Produced state contains correct ipv6 sub-object fields — address map structure.
    /// The address map must contain "address" (as IpNetwork CIDR), "valid_lft", "preferred_lft".
    #[test]
    fn test_build_ra_state_address_map_has_required_fields() {
        let mut state = Ipv6AutoState::default();
        let net: Ipv6Network = "2001:db8::/64".parse().unwrap();
        let now = Instant::now();
        state.prefixes.insert(
            net,
            PrefixState {
                addr: "2001:db8::a8bb:ccff:fedd:eeff".parse().unwrap(),
                prefix_len: 64,
                valid_expires: now + Duration::from_secs(86400),
                preferred_expires: now + Duration::from_secs(14400),
                on_link: true,
                installed: true,
                dad_complete: true,
                dad_failed: false,
                deprecated: false,
            },
        );
        let result = build_ra_state(&state, "eth0", "test-policy", 100);
        let addr_list = result
            .fields
            .get("ipv6")
            .unwrap()
            .value
            .as_map()
            .unwrap()
            .get("addresses")
            .unwrap()
            .as_list()
            .unwrap();
        let addr_map = addr_list[0].as_map().unwrap();
        assert!(addr_map.contains_key("address"), "address map must have 'address' field");
        assert!(addr_map.contains_key("valid_lft"), "address map must have 'valid_lft' field");
        assert!(addr_map.contains_key("preferred_lft"), "address map must have 'preferred_lft' field");
        let valid_lft = addr_map["valid_lft"].as_u64().expect("valid_lft must be u64");
        let pref_lft = addr_map["preferred_lft"].as_u64().expect("preferred_lft must be u64");
        assert!(valid_lft > 0, "valid_lft must be positive");
        assert!(pref_lft > 0, "preferred_lft must be positive");
        assert!(valid_lft >= pref_lft, "valid_lft must be >= preferred_lft");
    }

    // ── Route Information option ──────────────────────────────────────────────

    /// Scenario: Route Information option produces additional routes with correct fields.
    #[test]
    fn test_build_ra_state_route_info_entry_produces_additional_route() {
        let mut state = Ipv6AutoState::default();
        let router: Ipv6Addr = "fe80::1".parse().unwrap();
        let prefix: Ipv6Network = "2001:db8:beef::/48".parse().unwrap();
        state.route_info.push(RouteInfoEntry {
            prefix,
            router,
            preference: 1, // high → metric 50
            expires_at: far_future(),
        });
        let result = build_ra_state(&state, "eth0", "test-policy", 100);
        let ipv6 = result.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        let routes = map
            .get("routes")
            .expect("route info must produce routes entry")
            .as_list()
            .unwrap();
        assert_eq!(routes.len(), 1, "one route info entry must produce one route");
        let route = routes[0].as_map().unwrap();
        let metric = route["metric"].as_u64().expect("route must have metric");
        assert_eq!(metric, 50, "high preference route info must have metric=50");
    }

    /// Route info expired entries are excluded from routes.
    #[test]
    fn test_build_ra_state_expired_route_info_excluded() {
        let mut state = Ipv6AutoState::default();
        state.route_info.push(RouteInfoEntry {
            prefix: "2001:db8::/32".parse().unwrap(),
            router: "fe80::1".parse().unwrap(),
            preference: 0,
            expires_at: past(), // expired
        });
        let result = build_ra_state(&state, "eth0", "test-policy", 100);
        let ipv6 = result.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        assert!(
            map.get("routes").is_none(),
            "expired route info entry must not appear in routes"
        );
    }

    // ── preference_to_metric ──────────────────────────────────────────────────

    /// RFC 4191 preference 1 (high) maps to metric 50.
    #[test]
    fn test_preference_to_metric_high_gives_50() {
        assert_eq!(preference_to_metric(1), 50, "high preference (1) must map to metric 50");
    }

    /// RFC 4191 preference 0 (medium) maps to metric 100.
    #[test]
    fn test_preference_to_metric_medium_gives_100() {
        assert_eq!(preference_to_metric(0), 100, "medium preference (0) must map to metric 100");
    }

    /// RFC 4191 preference 3 (low) maps to metric 200.
    #[test]
    fn test_preference_to_metric_low_gives_200() {
        assert_eq!(preference_to_metric(3), 200, "low preference (3) must map to metric 200");
    }

    // ── handle_ra ─────────────────────────────────────────────────────────────

    /// Scenario: Prefix removal via valid_lft=0.
    /// Given a non-installed prefix, when RA sets valid_lft=0, the prefix is removed from state.
    /// Uses installed=false to avoid a netlink add call on a nonexistent interface.
    #[tokio::test]
    async fn test_handle_ra_valid_lft_zero_removes_prefix_from_state() {
        let mut state = Ipv6AutoState::default();
        let net: Ipv6Network = "2001:db8::/64".parse().unwrap();
        state.prefixes.insert(
            net,
            PrefixState {
                addr: "2001:db8::1".parse().unwrap(),
                prefix_len: 64,
                valid_expires: far_future(),
                preferred_expires: far_future(),
                on_link: true,
                installed: false, // not installed → no netlink remove call
                dad_complete: true,
                dad_failed: false,
                deprecated: false,
            },
        );

        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let pfx_net: Ipv6Network = "2001:db8::/64".parse().unwrap();
        let ra_msg = super::ra::RaMessage {
            hop_limit: 64,
            m_flag: false,
            o_flag: false,
            router_lifetime: 1800,
            source: "fe80::1".parse().unwrap(),
            options: vec![super::ra::RaOption::PrefixInfo {
                prefix: pfx_net,
                valid_lft: 0, // signal to remove the prefix
                preferred_lft: 0,
                on_link: true,
                autonomous: true,
            }],
        };

        handle_ra(&ra_msg, &mut state, "nonexistent-iface-for-test", mac).await;
        assert!(
            state.prefixes.get(&net).is_none(),
            "prefix with valid_lft=0 must be removed from factory state"
        );
    }

    /// Scenario: Factory acquires SLAAC address from Router Advertisement.
    /// handle_ra with a new /64 prefix generates the correct EUI-64 address.
    /// The add_ipv6_address call fails on the fake interface but state is still updated.
    #[tokio::test]
    async fn test_handle_ra_new_prefix_generates_eui64_address() {
        let mut state = Ipv6AutoState::default();
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let pfx_net: Ipv6Network = "2001:db8::/64".parse().unwrap();

        let ra_msg = super::ra::RaMessage {
            hop_limit: 64,
            m_flag: false,
            o_flag: false,
            router_lifetime: 1800,
            source: "fe80::1".parse().unwrap(),
            options: vec![super::ra::RaOption::PrefixInfo {
                prefix: pfx_net,
                valid_lft: 86400,
                preferred_lft: 14400,
                on_link: true,
                autonomous: true,
            }],
        };

        handle_ra(&ra_msg, &mut state, "nonexistent-iface-for-test", mac).await;

        let net: Ipv6Network = "2001:db8::/64".parse().unwrap();
        let ps = state
            .prefixes
            .get(&net)
            .expect("prefix must be tracked after RA with valid_lft > 0");
        let expected: Ipv6Addr = "2001:db8::a8bb:ccff:fedd:eeff".parse().unwrap();
        assert_eq!(
            ps.addr, expected,
            "SLAAC address for 2001:db8::/64 from MAC aa:bb:cc:dd:ee:ff must be \
             2001:db8::a8bb:ccff:fedd:eeff (EUI-64)"
        );
    }

    /// Scenario: handle_ra adds router to state when router_lifetime > 0.
    #[tokio::test]
    async fn test_handle_ra_router_added_when_lifetime_nonzero() {
        let mut state = Ipv6AutoState::default();
        let mac = [0x00u8; 6];
        let router: Ipv6Addr = "fe80::1".parse().unwrap();

        let ra_msg = super::ra::RaMessage {
            hop_limit: 64,
            m_flag: false,
            o_flag: false,
            router_lifetime: 1800,
            source: router,
            options: vec![],
        };

        handle_ra(&ra_msg, &mut state, "nonexistent-iface-for-test", mac).await;
        assert!(
            state.routers.contains_key(&router),
            "router with router_lifetime > 0 must be added to state"
        );
    }

    /// Scenario: handle_ra removes router from state when router_lifetime = 0.
    #[tokio::test]
    async fn test_handle_ra_router_removed_when_lifetime_zero() {
        let mut state = Ipv6AutoState::default();
        let router: Ipv6Addr = "fe80::1".parse().unwrap();
        state.routers.insert(router, RouterState { expires_at: far_future() });

        let mac = [0x00u8; 6];
        let ra_msg = super::ra::RaMessage {
            hop_limit: 64,
            m_flag: false,
            o_flag: false,
            router_lifetime: 0, // router withdraws default-router role
            source: router,
            options: vec![],
        };

        handle_ra(&ra_msg, &mut state, "nonexistent-iface-for-test", mac).await;
        assert!(
            !state.routers.contains_key(&router),
            "router with router_lifetime=0 must be removed from state"
        );
    }

    /// Scenario: handle_ra adds RDNSS DNS servers to state.
    #[tokio::test]
    async fn test_handle_ra_rdnss_added_to_state() {
        let mut state = Ipv6AutoState::default();
        let mac = [0x00u8; 6];
        let dns: Ipv6Addr = "2001:db8::53".parse().unwrap();

        let ra_msg = super::ra::RaMessage {
            hop_limit: 64,
            m_flag: false,
            o_flag: false,
            router_lifetime: 1800,
            source: "fe80::1".parse().unwrap(),
            options: vec![super::ra::RaOption::Rdnss {
                addresses: vec![dns],
                lifetime: 3600,
            }],
        };

        handle_ra(&ra_msg, &mut state, "nonexistent-iface-for-test", mac).await;
        assert!(!state.rdnss.is_empty(), "RDNSS option must add DNS servers to state");
        assert!(
            state.rdnss[0].addresses.contains(&dns),
            "DNS server address must be tracked"
        );
    }

    /// Scenario: handle_ra adds DNSSL search domains to state.
    #[tokio::test]
    async fn test_handle_ra_dnssl_added_to_state() {
        let mut state = Ipv6AutoState::default();
        let mac = [0x00u8; 6];

        let ra_msg = super::ra::RaMessage {
            hop_limit: 64,
            m_flag: false,
            o_flag: false,
            router_lifetime: 1800,
            source: "fe80::1".parse().unwrap(),
            options: vec![super::ra::RaOption::Dnssl {
                domains: vec!["example.com".to_string()],
                lifetime: 3600,
            }],
        };

        handle_ra(&ra_msg, &mut state, "nonexistent-iface-for-test", mac).await;
        assert!(!state.dnssl.is_empty(), "DNSSL option must add search domains to state");
        assert_eq!(
            state.dnssl[0].domains,
            vec!["example.com"],
            "search domain must be tracked correctly"
        );
    }

    /// Scenario: handle_ra adds PREF64 NAT64 prefix to state.
    #[tokio::test]
    async fn test_handle_ra_pref64_added_to_state() {
        let mut state = Ipv6AutoState::default();
        let mac = [0x00u8; 6];
        let nat64: Ipv6Network = "64:ff9b::/96".parse().unwrap();

        let ra_msg = super::ra::RaMessage {
            hop_limit: 64,
            m_flag: false,
            o_flag: false,
            router_lifetime: 1800,
            source: "fe80::1".parse().unwrap(),
            options: vec![super::ra::RaOption::Pref64 {
                prefix: nat64,
                lifetime: 3600,
            }],
        };

        handle_ra(&ra_msg, &mut state, "nonexistent-iface-for-test", mac).await;
        assert!(state.pref64.is_some(), "PREF64 option must set NAT64 prefix in state");
        assert_eq!(
            state.pref64.as_ref().unwrap().prefix,
            nat64,
            "NAT64 prefix must be stored correctly"
        );
    }

    // ── DHCPv6 test helpers ───────────────────────────────────────────────────

    /// Make a stateful Dhcpv6Lease for unit tests.
    fn make_test_dhcpv6_lease_stateful(
        addr_specs: Vec<(std::net::Ipv6Addr, u8, u32, u32)>,
        dns: Vec<std::net::Ipv6Addr>,
        search: Vec<String>,
    ) -> Dhcpv6Lease {
        use self::dhcpv6::lease::Dhcpv6Address;
        Dhcpv6Lease {
            addresses: addr_specs
                .into_iter()
                .map(|(address, prefix_len, preferred_lft, valid_lft)| Dhcpv6Address {
                    address,
                    prefix_len,
                    preferred_lft,
                    valid_lft,
                })
                .collect(),
            dns_servers: dns,
            dns_search: search,
            t1: 3600,
            t2: 5760,
            server_duid: vec![0, 1, 2, 3],
            server_addr: "fe80::1".parse().unwrap(),
            info_refresh_time: None,
            acquired_at: std::time::Instant::now(),
        }
    }

    /// Make a stateless Dhcpv6Lease (no addresses) for unit tests.
    fn make_test_dhcpv6_lease_stateless(
        dns: Vec<std::net::Ipv6Addr>,
        search: Vec<String>,
    ) -> Dhcpv6Lease {
        Dhcpv6Lease {
            addresses: vec![],
            dns_servers: dns,
            dns_search: search,
            t1: 0,
            t2: 0,
            server_duid: vec![0, 1, 2, 3],
            server_addr: "fe80::1".parse().unwrap(),
            info_refresh_time: Some(1800),
            acquired_at: std::time::Instant::now(),
        }
    }

    // ── build_merged_state with DHCPv6 lease ─────────────────────────────────

    /// Scenario: SLAAC and DHCPv6 stateful addresses are merged
    /// Given SLAAC provides one address and DHCPv6 stateful provides another,
    /// build_merged_state includes both in ipv6.addresses.
    #[test]
    fn test_build_merged_state_stateful_includes_both_slaac_and_dhcpv6_addresses() {
        let mut ra_state = Ipv6AutoState::default();
        let net: Ipv6Network = "2001:db8::/64".parse().unwrap();
        let slaac_addr: Ipv6Addr = "2001:db8::a8bb:ccff:fedd:eeff".parse().unwrap();
        ra_state
            .prefixes
            .insert(net, make_prefix_state(slaac_addr, true, false, far_future(), far_future()));

        let dhcpv6_addr: Ipv6Addr = "2001:db8::100".parse().unwrap();
        let lease = make_test_dhcpv6_lease_stateful(
            vec![(dhcpv6_addr, 128, 14400, 86400)],
            vec![],
            vec![],
        );

        let state = build_merged_state(&ra_state, "eth0", "test-policy", 100, Some(&lease));
        let ipv6 = state.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        let addrs = map
            .get("addresses")
            .expect("addresses must be present")
            .as_list()
            .unwrap();
        assert_eq!(
            addrs.len(),
            2,
            "both SLAAC and DHCPv6 stateful addresses must appear"
        );
        let addr_strs: Vec<String> = addrs
            .iter()
            .filter_map(|v| v.as_map())
            .filter_map(|m| m.get("address"))
            .map(|v| v.to_string())
            .collect();
        assert!(
            addr_strs.iter().any(|s| s.contains("a8bb:ccff:fedd:eeff")),
            "SLAAC address must be in addresses list"
        );
        assert!(
            addr_strs.iter().any(|s| s.contains("100")),
            "DHCPv6 address must be in addresses list"
        );
    }

    /// Scenario: O flag (stateless) — no DHCPv6 addresses in produced state
    /// Given a stateless DHCPv6 lease (empty address list) and a SLAAC address,
    /// only the SLAAC address appears in ipv6.addresses.
    #[test]
    fn test_build_merged_state_stateless_dhcpv6_adds_no_addresses() {
        let mut ra_state = Ipv6AutoState::default();
        let net: Ipv6Network = "2001:db8::/64".parse().unwrap();
        let slaac_addr: Ipv6Addr = "2001:db8::a8bb:ccff:fedd:eeff".parse().unwrap();
        ra_state
            .prefixes
            .insert(net, make_prefix_state(slaac_addr, true, false, far_future(), far_future()));

        let lease = make_test_dhcpv6_lease_stateless(
            vec!["2001:db8::53".parse().unwrap()],
            vec!["example.com".to_string()],
        );

        let state = build_merged_state(&ra_state, "eth0", "test-policy", 100, Some(&lease));
        let ipv6 = state.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        let addrs = map
            .get("addresses")
            .expect("SLAAC address must still appear")
            .as_list()
            .unwrap();
        assert_eq!(
            addrs.len(),
            1,
            "only the SLAAC address must appear; stateless DHCPv6 provides no addresses"
        );
    }

    /// Scenario: M flag implies O — DNS from both RDNSS and DHCPv6
    /// Given RA RDNSS provides [2001:db8::53] and a stateful DHCPv6 lease provides
    /// [2001:db8::54], both servers appear in ipv6.dns_servers.
    #[test]
    fn test_build_merged_state_dns_merged_from_rdnss_and_dhcpv6_stateful() {
        let mut ra_state = Ipv6AutoState::default();
        ra_state.rdnss.push(RdnssEntry {
            addresses: vec!["2001:db8::53".parse().unwrap()],
            expires_at: far_future(),
        });

        let lease = make_test_dhcpv6_lease_stateful(
            vec![],
            vec!["2001:db8::54".parse().unwrap()],
            vec![],
        );

        let state = build_merged_state(&ra_state, "eth0", "test-policy", 100, Some(&lease));
        let ipv6 = state.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        let dns = map
            .get("dns_servers")
            .expect("dns_servers must be present")
            .as_list()
            .unwrap();
        assert_eq!(dns.len(), 2, "RDNSS and DHCPv6 DNS servers must both appear");
        let dns_strs: Vec<&str> = dns.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            dns_strs.contains(&"2001:db8::53"),
            "RDNSS DNS server 2001:db8::53 must be present"
        );
        assert!(
            dns_strs.contains(&"2001:db8::54"),
            "DHCPv6 DNS server 2001:db8::54 must be present"
        );
    }

    /// Scenario: DNS servers from RDNSS and stateless DHCPv6 are merged.
    #[test]
    fn test_build_merged_state_dns_merged_from_rdnss_and_dhcpv6_stateless() {
        let mut ra_state = Ipv6AutoState::default();
        ra_state.rdnss.push(RdnssEntry {
            addresses: vec!["2001:db8::53".parse().unwrap()],
            expires_at: far_future(),
        });

        let lease = make_test_dhcpv6_lease_stateless(
            vec!["2001:db8::54".parse().unwrap()],
            vec![],
        );

        let state = build_merged_state(&ra_state, "eth0", "test-policy", 100, Some(&lease));
        let ipv6 = state.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        let dns = map
            .get("dns_servers")
            .expect("dns_servers must be present")
            .as_list()
            .unwrap();
        assert_eq!(
            dns.len(),
            2,
            "RDNSS and stateless DHCPv6 DNS servers must both appear"
        );
    }

    /// Scenario: Duplicate DNS servers are deduplicated
    /// Given RDNSS [2001:db8::53] and DHCPv6 [2001:db8::53, 2001:db8::54],
    /// result is [2001:db8::53, 2001:db8::54] with no duplicates.
    #[test]
    fn test_build_merged_state_duplicate_dns_servers_deduplicated() {
        let mut ra_state = Ipv6AutoState::default();
        ra_state.rdnss.push(RdnssEntry {
            addresses: vec!["2001:db8::53".parse().unwrap()],
            expires_at: far_future(),
        });

        let lease = make_test_dhcpv6_lease_stateless(
            vec!["2001:db8::53".parse().unwrap(), "2001:db8::54".parse().unwrap()],
            vec![],
        );

        let state = build_merged_state(&ra_state, "eth0", "test-policy", 100, Some(&lease));
        let ipv6 = state.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        let dns = map
            .get("dns_servers")
            .expect("dns_servers must be present")
            .as_list()
            .unwrap();
        assert_eq!(
            dns.len(),
            2,
            "duplicate DNS server 2001:db8::53 must not appear twice"
        );
        let dns_strs: Vec<&str> = dns.iter().filter_map(|v| v.as_str()).collect();
        assert!(dns_strs.contains(&"2001:db8::53"), "2001:db8::53 must appear once");
        assert!(dns_strs.contains(&"2001:db8::54"), "2001:db8::54 must appear");
    }

    /// Scenario: DNS search domains are merged from DNSSL and DHCPv6 without duplicates.
    #[test]
    fn test_build_merged_state_dns_search_merged_and_deduplicated() {
        let mut ra_state = Ipv6AutoState::default();
        ra_state.dnssl.push(DnsslEntry {
            domains: vec!["example.com".to_string()],
            expires_at: far_future(),
        });

        let lease = make_test_dhcpv6_lease_stateless(
            vec![],
            vec!["example.com".to_string(), "other.example.com".to_string()],
        );

        let state = build_merged_state(&ra_state, "eth0", "test-policy", 100, Some(&lease));
        let ipv6 = state.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        let search = map
            .get("dns_search")
            .expect("dns_search must be present")
            .as_list()
            .unwrap();
        assert_eq!(
            search.len(),
            2,
            "duplicate search domain must not appear twice; expected [example.com, other.example.com]"
        );
        let search_strs: Vec<&str> = search.iter().filter_map(|v| v.as_str()).collect();
        assert!(search_strs.contains(&"example.com"), "DNSSL domain must appear");
        assert!(
            search_strs.contains(&"other.example.com"),
            "DHCPv6-only domain must appear"
        );
    }

    /// Scenario: DHCPv6 address replaces SLAAC address when both produce the same IP.
    /// The DHCPv6 version (server-assigned lifetimes) takes precedence.
    #[test]
    fn test_build_merged_state_dhcpv6_address_deduplicates_slaac_duplicate() {
        let mut ra_state = Ipv6AutoState::default();
        let shared_addr: Ipv6Addr = "2001:db8::a8bb:ccff:fedd:eeff".parse().unwrap();
        let net: Ipv6Network = "2001:db8::/64".parse().unwrap();
        ra_state.prefixes.insert(
            net,
            make_prefix_state(shared_addr, true, false, far_future(), far_future()),
        );

        // DHCPv6 assigns the same address with distinct server-assigned lifetimes.
        let lease = make_test_dhcpv6_lease_stateful(
            vec![(shared_addr, 128, 7200, 43200)],
            vec![],
            vec![],
        );

        let state = build_merged_state(&ra_state, "eth0", "test-policy", 100, Some(&lease));
        let ipv6 = state.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        let addrs = map
            .get("addresses")
            .expect("addresses must be present")
            .as_list()
            .unwrap();
        assert_eq!(
            addrs.len(),
            1,
            "duplicate address from SLAAC and DHCPv6 must appear only once"
        );
        let addr_map = addrs[0].as_map().unwrap();
        let valid_lft = addr_map["valid_lft"]
            .as_u64()
            .expect("valid_lft must be u64");
        assert_eq!(
            valid_lft, 43200,
            "DHCPv6 version of duplicate address must take precedence (valid_lft=43200)"
        );
    }

    /// Scenario: routes come from SLAAC only (DHCPv6 does not provide routes).
    /// Given a DHCPv6 lease alongside an active router, routes still come from RA.
    #[test]
    fn test_build_merged_state_routes_from_slaac_not_dhcpv6() {
        let mut ra_state = Ipv6AutoState::default();
        ra_state
            .routers
            .insert("fe80::1".parse().unwrap(), RouterState { expires_at: far_future() });

        let lease = make_test_dhcpv6_lease_stateless(vec![], vec![]);

        let state = build_merged_state(&ra_state, "eth0", "test-policy", 100, Some(&lease));
        let ipv6 = state.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        let routes = map
            .get("routes")
            .expect("routes must be present from RA router")
            .as_list()
            .unwrap();
        assert_eq!(routes.len(), 1, "one default route must appear from the RA router");
        let route = routes[0].as_map().unwrap();
        let dest = route["destination"].to_string();
        assert!(dest.contains("::/0"), "route destination must be ::/0 (default route)");
    }

    /// Scenario: nat64_prefix from SLAAC PREF64 is preserved when DHCPv6 lease is present.
    #[test]
    fn test_build_merged_state_nat64_prefix_preserved_with_dhcpv6_lease() {
        let mut ra_state = Ipv6AutoState::default();
        let prefix: Ipv6Network = "64:ff9b::/96".parse().unwrap();
        ra_state.pref64 = Some(Pref64Entry {
            prefix,
            expires_at: far_future(),
        });

        let lease =
            make_test_dhcpv6_lease_stateless(vec!["2001:db8::53".parse().unwrap()], vec![]);

        let state = build_merged_state(&ra_state, "eth0", "test-policy", 100, Some(&lease));
        let ipv6 = state.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        assert!(
            map.get("nat64_prefix").is_some(),
            "nat64_prefix from PREF64 must be preserved alongside a DHCPv6 lease"
        );
    }

    /// Scenario: DHCPv6 error does not affect SLAAC state
    /// After a DHCPv6 error the factory calls build_merged_state with None lease.
    /// SLAAC addresses, routes, and DNS from RDNSS must remain unchanged.
    #[test]
    fn test_build_merged_state_slaac_state_intact_when_no_dhcpv6_lease() {
        let mut ra_state = Ipv6AutoState::default();
        let net: Ipv6Network = "2001:db8::/64".parse().unwrap();
        let slaac_addr: Ipv6Addr = "2001:db8::a8bb:ccff:fedd:eeff".parse().unwrap();
        ra_state
            .prefixes
            .insert(net, make_prefix_state(slaac_addr, true, false, far_future(), far_future()));
        ra_state
            .routers
            .insert("fe80::1".parse().unwrap(), RouterState { expires_at: far_future() });
        ra_state.rdnss.push(RdnssEntry {
            addresses: vec!["2001:db8::53".parse().unwrap()],
            expires_at: far_future(),
        });

        // None lease simulates DHCPv6 error / expiry clearing the lease.
        let state = build_merged_state(&ra_state, "eth0", "test-policy", 100, None);
        let ipv6 = state.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        assert!(
            map.get("addresses").is_some(),
            "SLAAC address must remain in produced state after DHCPv6 error"
        );
        assert!(
            map.get("routes").is_some(),
            "SLAAC routes must remain in produced state after DHCPv6 error"
        );
        assert!(
            map.get("dns_servers").is_some(),
            "RDNSS dns_servers must remain in produced state after DHCPv6 error"
        );
    }

    /// Scenario: DHCPv6 stateful lease provides addresses even with no DAD-complete SLAAC.
    /// This covers the M-only scenario where the interface gets addresses only via DHCPv6.
    #[test]
    fn test_build_merged_state_dhcpv6_only_no_slaac_addresses() {
        let ra_state = Ipv6AutoState::default(); // No SLAAC state
        let dhcpv6_addr: Ipv6Addr = "2001:db8::200".parse().unwrap();
        let lease = make_test_dhcpv6_lease_stateful(
            vec![(dhcpv6_addr, 128, 14400, 86400)],
            vec!["2001:db8::53".parse().unwrap()],
            vec![],
        );

        let state = build_merged_state(&ra_state, "eth0", "test-policy", 100, Some(&lease));
        let ipv6 = state.fields.get("ipv6").unwrap();
        let map = ipv6.value.as_map().unwrap();
        let addrs = map
            .get("addresses")
            .expect("DHCPv6 addresses must be present even with no SLAAC")
            .as_list()
            .unwrap();
        assert_eq!(addrs.len(), 1, "one DHCPv6 address must appear");
        let dns = map
            .get("dns_servers")
            .expect("dns_servers from DHCPv6 must be present")
            .as_list()
            .unwrap();
        assert_eq!(dns.len(), 1, "one DHCPv6 DNS server must appear");
    }

    // ── emit_lease_event_inner with DHCPv6 lease ──────────────────────────────

    /// Scenario: emit_lease_event_inner sends LeaseAcquired when DHCPv6 lease present
    /// but no DAD-complete SLAAC addresses (M-only scenario).
    #[tokio::test]
    async fn test_emit_lease_event_inner_lease_acquired_with_dhcpv6_only() {
        let ra_state = Ipv6AutoState::default(); // No SLAAC addresses
        let (tx, mut rx) = mpsc::channel(4);
        let mut lease_acquired = false;

        emit_lease_event_inner(
            &ra_state,
            &mut lease_acquired,
            "test",
            pending_state("eth0", "test", 100),
            &tx,
            true, // has_dhcpv6_lease = true
        )
        .await;

        let event =
            rx.try_recv().expect("LeaseAcquired must be sent when DHCPv6 lease is present");
        assert!(
            matches!(event, FactoryEvent::LeaseAcquired { .. }),
            "DHCPv6 lease alone (no SLAAC DAD) must trigger LeaseAcquired"
        );
        assert!(lease_acquired, "lease_acquired flag must be set to true");
    }

    /// Scenario: emit_lease_event_inner sends LeaseRenewed when DHCPv6 renews
    /// and lease_acquired is already true.
    #[tokio::test]
    async fn test_emit_lease_event_inner_lease_renewed_on_dhcpv6_renewal() {
        let ra_state = Ipv6AutoState::default();
        let (tx, mut rx) = mpsc::channel(4);
        let mut lease_acquired = true; // Already acquired from a prior event

        emit_lease_event_inner(
            &ra_state,
            &mut lease_acquired,
            "test",
            pending_state("eth0", "test", 100),
            &tx,
            true,
        )
        .await;

        let event = rx.try_recv().expect("LeaseRenewed must be sent on DHCPv6 renewal");
        assert!(
            matches!(event, FactoryEvent::LeaseRenewed { .. }),
            "DHCPv6 renewal with lease_acquired=true must emit LeaseRenewed"
        );
    }

    /// Scenario: emit_lease_event_inner sends LeaseExpired when DHCPv6 expires
    /// and there is no SLAAC state (M flag cleared, no more SLAAC).
    #[tokio::test]
    async fn test_emit_lease_event_inner_lease_expired_when_dhcpv6_clears_with_no_slaac() {
        let ra_state = Ipv6AutoState::default(); // No SLAAC prefixes or routers
        let (tx, mut rx) = mpsc::channel(4);
        let mut lease_acquired = true; // Was previously acquired

        emit_lease_event_inner(
            &ra_state,
            &mut lease_acquired,
            "test",
            pending_state("eth0", "test", 100),
            &tx,
            false, // has_dhcpv6_lease = false (expired)
        )
        .await;

        let event = rx
            .try_recv()
            .expect("LeaseExpired must be sent when DHCPv6 clears with no SLAAC");
        assert!(
            matches!(event, FactoryEvent::LeaseExpired { .. }),
            "DHCPv6 expiry with no SLAAC must emit LeaseExpired"
        );
        assert!(
            !lease_acquired,
            "lease_acquired must be reset to false on LeaseExpired"
        );
    }

    /// Scenario: emit_lease_event_inner does not send an event before first acquisition
    /// even with has_dhcpv6_lease=false and no SLAAC (initial state, nothing acquired yet).
    #[tokio::test]
    async fn test_emit_lease_event_inner_no_event_when_never_acquired_and_no_dhcpv6() {
        let ra_state = Ipv6AutoState::default();
        let (tx, mut rx) = mpsc::channel(4);
        let mut lease_acquired = false;

        emit_lease_event_inner(
            &ra_state,
            &mut lease_acquired,
            "test",
            pending_state("eth0", "test", 100),
            &tx,
            false,
        )
        .await;

        assert!(
            rx.try_recv().is_err(),
            "no event must be sent when neither SLAAC nor DHCPv6 has ever acquired"
        );
    }

    // ── manage_dhcpv6 M/O flag state machine ─────────────────────────────────

    /// Scenario: No M or O flags — no DHCPv6 client is started.
    /// manage_dhcpv6(M=0, O=0) with no existing client must leave client as None.
    #[tokio::test]
    async fn test_manage_dhcpv6_no_flags_does_not_start_client() {
        let mut client: Option<Dhcpv6Client> = None;
        let mut result_rx: Option<mpsc::Receiver<Dhcpv6Result>> = None;
        let mut lease: Option<Dhcpv6Lease> = None;
        let mut installed_addrs: Vec<Ipv6Addr> = Vec::new();

        manage_dhcpv6(
            false,
            false,
            "nonexistent-iface-for-test",
            &mut client,
            &mut result_rx,
            &mut lease,
            &mut installed_addrs,
        )
        .await;

        assert!(client.is_none(), "M=0, O=0 must not start a DHCPv6 client");
        assert!(
            result_rx.is_none(),
            "M=0, O=0 must not create a DHCPv6 result receiver"
        );
    }

    /// Scenario: manage_dhcpv6 keeps client and result_rx coherent.
    /// When M=1 on a nonexistent interface, the start attempt fails but client
    /// and result_rx remain coherent (both None or both Some).
    #[tokio::test]
    async fn test_manage_dhcpv6_client_and_rx_always_coherent_on_m_flag() {
        let mut client: Option<Dhcpv6Client> = None;
        let mut result_rx: Option<mpsc::Receiver<Dhcpv6Result>> = None;
        let mut lease: Option<Dhcpv6Lease> = None;
        let mut installed_addrs: Vec<Ipv6Addr> = Vec::new();

        // Start attempt will fail on nonexistent interface; must not panic.
        manage_dhcpv6(
            true,
            false,
            "nonexistent-iface-for-test",
            &mut client,
            &mut result_rx,
            &mut lease,
            &mut installed_addrs,
        )
        .await;

        assert_eq!(
            client.is_some(),
            result_rx.is_some(),
            "client and result_rx must always be set/unset together"
        );
    }

    /// Scenario: O flag (stateless) — manage_dhcpv6 attempts a stateless client start.
    /// On a nonexistent interface the start may fail; client/result_rx remain coherent.
    #[tokio::test]
    async fn test_manage_dhcpv6_o_flag_stateless_client_and_rx_coherent() {
        let mut client: Option<Dhcpv6Client> = None;
        let mut result_rx: Option<mpsc::Receiver<Dhcpv6Result>> = None;
        let mut lease: Option<Dhcpv6Lease> = None;
        let mut installed_addrs: Vec<Ipv6Addr> = Vec::new();

        manage_dhcpv6(
            false,
            true,
            "nonexistent-iface-for-test",
            &mut client,
            &mut result_rx,
            &mut lease,
            &mut installed_addrs,
        )
        .await;

        assert_eq!(
            client.is_some(),
            result_rx.is_some(),
            "client and result_rx must always be set/unset together for O flag"
        );
    }

    /// Scenario: manage_dhcpv6 with M=0, O=0 and no running client
    /// does not start a new client.
    #[tokio::test]
    async fn test_manage_dhcpv6_no_flags_no_existing_client_noop() {
        let mut client: Option<Dhcpv6Client> = None;
        let mut result_rx: Option<mpsc::Receiver<Dhcpv6Result>> = None;
        let mut lease: Option<Dhcpv6Lease> = None;
        let mut installed_addrs: Vec<Ipv6Addr> = Vec::new();

        manage_dhcpv6(
            false,
            false,
            "nonexistent-iface-for-test",
            &mut client,
            &mut result_rx,
            &mut lease,
            &mut installed_addrs,
        )
        .await;

        assert!(client.is_none(), "no client must be started with M=0, O=0");
        assert!(result_rx.is_none(), "no result_rx must be created with M=0, O=0");
        // installed_addrs must remain empty (no addresses to remove).
        assert!(
            installed_addrs.is_empty(),
            "installed_addrs must remain empty when no client ran"
        );
    }

    // ── Ipv6AutoFactory public API ────────────────────────────────────────────

    /// Scenario: current_state returns pending state before any RA.
    /// start() always returns Ok; the background task may fail asynchronously.
    #[tokio::test]
    async fn test_ipv6auto_factory_pending_state_available_immediately_after_start() {
        let (tx, _rx) = mpsc::channel::<FactoryEvent>(4);
        let mut factory = Ipv6AutoFactory::start("lo", "test-ipv6-pending".to_string(), 100, tx)
            .await
            .expect("start() must always succeed synchronously");
        let state = factory.current_state();
        assert!(
            state.is_some(),
            "current_state must return Some(pending_state) immediately after start"
        );
        let state = state.unwrap();
        assert_eq!(
            state.fields.get("enabled").and_then(|fv| fv.value.as_bool()),
            Some(true),
            "pending state must have enabled=true immediately after start"
        );
        let _ = factory.stop().await;
    }
}
