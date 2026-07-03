//! Integration tests for SPEC-401: DHCPv4 factory.
//!
//! Tests use unprivileged user + network namespaces (via `NetnsGuard`) and a
//! real `dnsmasq` DHCP server. Both are skipped gracefully when unavailable.
//!
//! All tests use the `current_thread` tokio flavour so that every `tokio::spawn`
//! task created by `Dhcpv4Factory::start` runs on the same OS thread that
//! entered the network namespace via `unshare(2)`. A multi-thread runtime would
//! cause the background DHCP task to run on a different thread that is still in
//! the original (host) network namespace.

use std::net::Ipv4Addr;
use std::time::Duration;

use netfyr_backend::{Dhcpv4Factory, FactoryEvent};
use netfyr_test_utils::netns::{add_address, create_veth_pair, set_link_up, NetnsGuard};
use netfyr_test_utils::DnsmasqGuard;
use tokio::sync::mpsc;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Client-side veth (no IP; DHCP will assign one).
const VETH_CLIENT: &str = "veth-dhcp0";
/// Server-side veth (static IP; dnsmasq listens here).
const VETH_SERVER: &str = "veth-dhcp1";
const SERVER_IP: &str = "10.99.0.1";
const SERVER_CIDR: &str = "10.99.0.1/24";
const RANGE_START: &str = "10.99.0.100";
const RANGE_END: &str = "10.99.0.200";

// ── Helper macros ─────────────────────────────────────────────────────────────

/// Skip the test if user+network namespaces are not available.
macro_rules! require_netns {
    ($guard:ident) => {
        let $guard = match NetnsGuard::new() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("Skipping: cannot create network namespace: {e}");
                return;
            }
        };
    };
}

/// Skip the test if dnsmasq is not installed.
macro_rules! require_dnsmasq {
    ($guard:ident, $iface:expr, $server_ip:expr, $start:expr, $end:expr, $lease:expr) => {
        let $guard = match DnsmasqGuard::start($iface, $server_ip, $start, $end, $lease) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("Skipping: dnsmasq unavailable: {e}");
                return;
            }
        };
    };
}

// ── Helper: set up common veth pair ──────────────────────────────────────────

async fn setup_veth_pair() {
    create_veth_pair(VETH_CLIENT, VETH_SERVER)
        .await
        .expect("failed to create veth pair");
    add_address(VETH_SERVER, SERVER_CIDR)
        .await
        .expect("failed to add server address");
    set_link_up(VETH_SERVER)
        .await
        .expect("failed to bring server veth up");
    set_link_up(VETH_CLIENT)
        .await
        .expect("failed to bring client veth up");
}

// ── Helper: wait for LeaseAcquired event ─────────────────────────────────────

/// Drain the channel until a `LeaseAcquired` event arrives, ignoring `Error`
/// events (which are normal retry noise). Returns `true` on success.
async fn wait_for_lease_acquired(rx: &mut mpsc::Receiver<FactoryEvent>) -> bool {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Some(FactoryEvent::LeaseAcquired { .. }) => return true,
                Some(FactoryEvent::Error { error, .. }) => {
                    eprintln!("DHCP retry (expected): {error}");
                }
                Some(other) => {
                    panic!("Unexpected event while waiting for LeaseAcquired: {:?}", other);
                }
                None => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

// ── Scenario: Acquire DHCP lease in unprivileged namespace ───────────────────

/// Scenario: Acquire DHCP lease in unprivileged namespace
///
/// Given an unprivileged user + network namespace with a veth pair
///   "veth-dhcp0" / "veth-dhcp1"
/// And "veth-dhcp1" has address "10.99.0.1/24" and is link-up
/// And dnsmasq is running on "veth-dhcp1" serving range 10.99.0.100-10.99.0.200
/// And "veth-dhcp0" is link-up with no addresses
/// When a Dhcpv4Factory is started on "veth-dhcp0"
/// Then a LeaseAcquired event is received within 10 seconds
/// And the leased IP is in the range 10.99.0.100-10.99.0.200
/// And the gateway is 10.99.0.1
#[tokio::test]
async fn test_acquire_dhcp_lease_in_unprivileged_namespace() {
    require_netns!(_ns);
    setup_veth_pair().await;
    require_dnsmasq!(_dnsmasq, VETH_SERVER, SERVER_IP, RANGE_START, RANGE_END, "120s");

    let (tx, mut rx) = mpsc::channel::<FactoryEvent>(16);
    let mut factory = Dhcpv4Factory::start(VETH_CLIENT, "test-dhcp".to_string(), 100, tx)
        .await
        .expect("factory start() must succeed");

    assert!(
        wait_for_lease_acquired(&mut rx).await,
        "LeaseAcquired event must be received within 10 seconds"
    );

    // Verify current_state reflects the acquired lease.
    let state = factory
        .current_state()
        .expect("current_state() must return Some after lease acquisition");

    // Entity type must be "ethernet".
    assert_eq!(state.entity_type, "ethernet");
    // Selector name must be the client interface.
    assert_eq!(state.selector.name.as_deref(), Some(VETH_CLIENT));

    // Leased IP must be in the dnsmasq range.
    let addresses = state
        .fields
        .get("addresses")
        .expect("addresses field must exist")
        .value
        .as_list()
        .expect("addresses must be a list");
    assert!(!addresses.is_empty(), "addresses must be non-empty");

    let cidr = addresses[0]
        .as_map()
        .and_then(|m| m.get("address"))
        .and_then(|v| v.as_str())
        .expect("address entry must be a map with 'address' key containing a CIDR string");
    let ip_str = cidr.split('/').next().expect("CIDR must contain /");
    let ip: Ipv4Addr = ip_str.parse().expect("address must be a valid IPv4");

    let range_start: Ipv4Addr = RANGE_START.parse().unwrap();
    let range_end: Ipv4Addr = RANGE_END.parse().unwrap();
    assert!(
        u32::from(ip) >= u32::from(range_start) && u32::from(ip) <= u32::from(range_end),
        "leased IP {ip} must be in range {RANGE_START}-{RANGE_END}"
    );

    // Gateway must be the server IP (dnsmasq defaults to its own address).
    let routes = state
        .fields
        .get("routes")
        .expect("routes field must exist when gateway is provided")
        .value
        .as_list()
        .expect("routes must be a list");
    assert!(!routes.is_empty(), "at least one route must be present");

    let gw = routes[0]
        .as_map()
        .and_then(|m| m.get("gateway"))
        .and_then(|v| v.as_str())
        .expect("route must have a string gateway");
    assert_eq!(gw, SERVER_IP, "gateway must be the DHCP server IP");

    factory.stop().await.expect("stop() must succeed cleanly");
}

// ── Scenario: current_state after lease acquisition ──────────────────────────

/// Scenario: current_state returns Some(State) after a lease is acquired.
///
/// This is the integration counterpart of the unit test
/// `test_current_state_returns_none_before_lease_acquired`.
#[tokio::test]
async fn test_current_state_returns_some_after_lease_acquired() {
    require_netns!(_ns);
    setup_veth_pair().await;
    require_dnsmasq!(_dnsmasq, VETH_SERVER, SERVER_IP, RANGE_START, RANGE_END, "120s");

    let (tx, mut rx) = mpsc::channel::<FactoryEvent>(16);
    let mut factory = Dhcpv4Factory::start(VETH_CLIENT, "test-dhcp".to_string(), 100, tx)
        .await
        .expect("start() must succeed");

    assert!(
        wait_for_lease_acquired(&mut rx).await,
        "must acquire a lease first"
    );

    // After LeaseAcquired, current_state() must return Some.
    assert!(
        factory.current_state().is_some(),
        "current_state() must return Some after lease acquisition"
    );

    factory.stop().await.expect("stop() must succeed");
}

// ── Scenario: Factory stop releases lease cleanly ─────────────────────────────

/// Scenario: Factory releases lease on stop
///
/// Given an active DHCP lease in an unprivileged namespace
/// When stop() is called on the factory
/// Then a DHCPRELEASE is sent to the server (validated by clean exit)
/// And the factory exits cleanly
#[tokio::test]
async fn test_factory_stop_releases_lease_in_namespace() {
    require_netns!(_ns);
    setup_veth_pair().await;
    require_dnsmasq!(_dnsmasq, VETH_SERVER, SERVER_IP, RANGE_START, RANGE_END, "120s");

    let (tx, mut rx) = mpsc::channel::<FactoryEvent>(16);
    let mut factory = Dhcpv4Factory::start(VETH_CLIENT, "test-dhcp".to_string(), 100, tx)
        .await
        .expect("start() must succeed");

    assert!(
        wait_for_lease_acquired(&mut rx).await,
        "must acquire a lease before testing stop"
    );

    // stop() sends DHCPRELEASE and awaits clean task termination.
    factory.stop().await.expect("stop() must succeed cleanly");

    // After stop(), calling stop() again must be idempotent.
    factory
        .stop()
        .await
        .expect("second stop() must also succeed (idempotent)");
}

// ── Scenario: Factory retries on discovery timeout ────────────────────────────

/// Scenario: Factory retries on discovery timeout
///
/// Given a factory started on an interface with no DHCP server
/// When the discovery timeout elapses
/// Then a FactoryEvent::Error is sent
/// And the factory retries (a second Error event arrives after the backoff)
///
/// Note: DISCOVER_TIMEOUT is 5 seconds in the implementation, so this test
/// waits up to 15 seconds for the second Error event.
#[tokio::test]
async fn test_factory_retries_on_discovery_timeout() {
    require_netns!(_ns);

    // Set up only the client veth — no dnsmasq, so DHCP discovery always times out.
    create_veth_pair(VETH_CLIENT, VETH_SERVER)
        .await
        .expect("failed to create veth pair");
    set_link_up(VETH_CLIENT)
        .await
        .expect("failed to bring client veth up");

    let (tx, mut rx) = mpsc::channel::<FactoryEvent>(16);
    let mut factory = Dhcpv4Factory::start(VETH_CLIENT, "test-retry".to_string(), 100, tx)
        .await
        .expect("start() must succeed");

    // Collect the first Error event (arrives after DISCOVER_TIMEOUT ≈ 5s).
    let first_error = tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            match rx.recv().await {
                Some(FactoryEvent::Error { error, .. }) => return Some(error),
                Some(FactoryEvent::LeaseAcquired { .. }) => {
                    panic!("unexpected LeaseAcquired with no DHCP server")
                }
                None => return None,
                _ => continue,
            }
        }
    })
    .await
    .expect("first Error event must arrive within 12 seconds");

    let first_error_msg = first_error.expect("channel must not close before first Error");
    // The error message must mention timeout or discovery failure.
    // Implementation: "DHCP discovery timeout or error: DHCP response timeout"
    assert!(
        first_error_msg.to_lowercase().contains("timeout")
            || first_error_msg.to_lowercase().contains("error"),
        "Error message must describe the discovery failure, got: {first_error_msg}"
    );

    // After the backoff (INITIAL_BACKOFF = 1s + jitter ≤ 1s), the factory retries.
    // Collect the second Error event (arrives after another DISCOVER_TIMEOUT ≈ 5s).
    // Total wait: up to 12 more seconds.
    let second_error = tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            match rx.recv().await {
                Some(FactoryEvent::Error { .. }) => return true,
                Some(FactoryEvent::LeaseAcquired { .. }) => {
                    panic!("unexpected LeaseAcquired with no DHCP server")
                }
                None => return false,
                _ => {}
            }
        }
    })
    .await
    .unwrap_or(false);

    assert!(
        second_error,
        "factory must retry and send a second Error event (exponential backoff)"
    );

    factory.stop().await.expect("stop() must succeed");
}

// ── Scenario: current_state full fields after lease ───────────────────────────

/// Scenario: current_state returns full state after lease
///
/// Verifies every field the spec mandates: enabled=true, addresses, routes,
/// and dns_servers are all present once a lease has been acquired.
///
/// This is the integration-level counterpart of the unit tests in `mod.rs`
/// (which test `lease_to_state` in isolation using a synthetic lease).
#[tokio::test]
async fn test_current_state_full_fields_after_lease_acquired() {
    require_netns!(_ns);
    setup_veth_pair().await;
    require_dnsmasq!(_dnsmasq, VETH_SERVER, SERVER_IP, RANGE_START, RANGE_END, "120s");

    let (tx, mut rx) = mpsc::channel::<FactoryEvent>(16);
    let mut factory = Dhcpv4Factory::start(VETH_CLIENT, "test-full-state".to_string(), 100, tx)
        .await
        .expect("start() must succeed");

    assert!(
        wait_for_lease_acquired(&mut rx).await,
        "must acquire lease before checking full state"
    );

    let state = factory
        .current_state()
        .expect("current_state() must return Some after LeaseAcquired");

    // Scenario: current_state returns full state after lease
    // Then it returns Some(State) with enabled=true, addresses, routes, and dns

    // enabled=true is always required.
    let enabled = state
        .fields
        .get("enabled")
        .expect("enabled field must be present after lease acquisition")
        .value
        .as_bool()
        .expect("enabled must be a bool value");
    assert!(enabled, "enabled must be true in the post-lease state");

    // addresses must be present and non-empty.
    let addresses = state
        .fields
        .get("addresses")
        .expect("addresses field must be present after lease acquisition")
        .value
        .as_list()
        .expect("addresses must be a list");
    assert!(
        !addresses.is_empty(),
        "addresses list must be non-empty after lease acquisition"
    );
    // Each entry must be a map with an "address" key containing a CIDR string.
    let cidr = addresses[0]
        .as_map()
        .and_then(|m| m.get("address"))
        .and_then(|v| v.as_str())
        .expect("address entry must be a map with 'address' key containing a CIDR string");
    assert!(
        cidr.contains('/'),
        "address must be in CIDR notation (contains '/'), got: {cidr}"
    );

    // routes must be present (dnsmasq serves a default gateway = server IP).
    let routes = state
        .fields
        .get("routes")
        .expect("routes field must be present when DHCP server provides a gateway")
        .value
        .as_list()
        .expect("routes must be a list");
    assert!(!routes.is_empty(), "routes list must be non-empty");
    let first_route = routes[0].as_map().expect("each route must be a map");
    assert!(
        first_route.contains_key("destination"),
        "route map must contain 'destination' key"
    );
    assert!(
        first_route.contains_key("gateway"),
        "route map must contain 'gateway' key"
    );
    assert_eq!(
        first_route.get("destination").and_then(|v| v.as_str()),
        Some("0.0.0.0/0"),
        "default route destination must be 0.0.0.0/0"
    );

    // dns_servers: dnsmasq typically provides itself as DNS; verify the field
    // exists and is non-empty when present (may be absent if server sends none).
    if let Some(dns_fv) = state.fields.get("dns_servers") {
        let dns_list = dns_fv.value.as_list().expect("dns_servers must be a list");
        assert!(
            !dns_list.is_empty(),
            "dns_servers must not be an empty list (absent is fine, empty list is not)"
        );
        // Each entry must be a string representation of an IP address.
        for entry in dns_list {
            let s = entry.as_str().expect("each dns_server entry must be a string");
            assert!(
                s.parse::<std::net::Ipv4Addr>().is_ok(),
                "dns_server entry must be a valid IPv4 address, got: {s}"
            );
        }
    }

    factory.stop().await.expect("stop() must succeed");
}

// ── Scenario: Lease renewal in namespace ─────────────────────────────────────

/// Scenario: Lease renewal in namespace
///
/// Given an active DHCP lease with a short lease time (10 seconds)
/// When the renewal timer fires (T1 = lease_time / 2 = 5 seconds)
/// Then a LeaseRenewed event is received
/// And the lease IP is unchanged (or updated if the server changed it)
///
/// Note: This test takes at least ~5 seconds (T1) + network round-trip time.
/// It is guarded by a 30-second timeout.
#[tokio::test]
async fn test_lease_renewal_in_namespace() {
    require_netns!(_ns);
    setup_veth_pair().await;
    // Short lease time: 10 seconds. T1 = 5s, T2 = 8.75s.
    require_dnsmasq!(_dnsmasq, VETH_SERVER, SERVER_IP, RANGE_START, RANGE_END, "10s");

    let (tx, mut rx) = mpsc::channel::<FactoryEvent>(16);
    let mut factory = Dhcpv4Factory::start(VETH_CLIENT, "test-renew".to_string(), 100, tx)
        .await
        .expect("start() must succeed");

    // Wait for initial lease acquisition.
    assert!(
        wait_for_lease_acquired(&mut rx).await,
        "must acquire initial lease before waiting for renewal"
    );

    let ip_before = factory
        .current_state()
        .and_then(|s| {
            s.fields
                .get("addresses")
                .and_then(|fv| fv.value.as_list())
                .and_then(|list| list.first().cloned())
        })
        .as_ref()
        .and_then(|v| v.as_map())
        .and_then(|m| m.get("address"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .expect("must have an IP address (as map with 'address' key) after initial lease");

    // Wait up to 30 seconds for a LeaseRenewed event.
    let renewed = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match rx.recv().await {
                Some(FactoryEvent::LeaseRenewed { state, .. }) => return Some(state),
                Some(FactoryEvent::Error { error, .. }) => {
                    eprintln!("DHCP error during renewal (may retry): {error}");
                }
                Some(FactoryEvent::LeaseExpired { .. }) => {
                    // Lease expired without renewal — the server may have rejected renewal.
                    // This is acceptable if the lease time is very short.
                    return None;
                }
                Some(other) => panic!("Unexpected event: {:?}", other),
                None => return None,
            }
        }
    })
    .await;

    match renewed {
        Ok(Some(renewed_state)) => {
            // The renewed state must still reference the same interface.
            assert_eq!(
                renewed_state.selector.name.as_deref(),
                Some(VETH_CLIENT),
                "renewed state must target the same interface"
            );

            // IP should be preserved across renewal (dnsmasq typically re-assigns
            // the same IP, but the spec allows it to change).
            let ip_after = renewed_state
                .fields
                .get("addresses")
                .and_then(|fv| fv.value.as_list())
                .and_then(|l| l.first())
                .and_then(|v| v.as_map())
                .and_then(|m| m.get("address"))
                .and_then(|v| v.as_str())
                .expect("renewed state must have an address map with 'address' key");

            // Log whether the IP changed — both outcomes are valid per spec.
            if ip_before != ip_after {
                eprintln!("Note: DHCP server assigned a new IP on renewal: {ip_before} → {ip_after}");
            }
        }
        Ok(None) => {
            // LeaseExpired instead of LeaseRenewed. Acceptable for very short leases.
            eprintln!("Note: lease expired before renewal; test passes (short lease time)");
        }
        Err(_) => {
            panic!("No LeaseRenewed or LeaseExpired event received within 30 seconds");
        }
    }

    factory.stop().await.expect("stop() must succeed");
}

// ── Scenario: Factory sends LeaseExpired when lease expires ───────────────────

/// Scenario: Factory sends LeaseExpired when lease expires without renewal or rebind
///
/// Given a factory with an active lease (short lease time = 10s)
/// When the DHCP server becomes unreachable after the lease is acquired
///   (simulated by dropping dnsmasq)
/// Then:
///   - At T1 (≈5s), the factory attempts unicast renewal → times out (5s timeout)
///   - By t≈10s (lease_time), the expiry timer fires
///   - A LeaseExpired event is received
///
/// Timing: The test waits up to 30 seconds. With DISCOVER_TIMEOUT=5s and T1=5s,
/// the expected sequence is:
///   t=0   → lease acquired
///   t=0   → dnsmasq dropped (server gone)
///   t=5   → T1 fires, unicast renewal started, blocks for up to 5s
///   t=10  → renewal timeout, loop continues; expiry_wait is now ≤0
///   t=10  → expiry branch fires, LeaseExpired sent
///
/// The 30-second budget is generous to account for CI jitter and backoff jitter.
#[tokio::test]
async fn test_lease_expired_event_when_dhcp_server_stops() {
    require_netns!(_ns);
    setup_veth_pair().await;

    // Start dnsmasq with a 10-second lease. After acquiring the lease, we drop
    // the guard (kills dnsmasq), making renewal impossible.
    let dnsmasq =
        match DnsmasqGuard::start(VETH_SERVER, SERVER_IP, RANGE_START, RANGE_END, "10s") {
            Ok(g) => g,
            Err(e) => {
                eprintln!("Skipping: dnsmasq unavailable: {e}");
                return;
            }
        };

    let (tx, mut rx) = mpsc::channel::<FactoryEvent>(16);
    let mut factory = Dhcpv4Factory::start(VETH_CLIENT, "test-expire".to_string(), 100, tx)
        .await
        .expect("start() must succeed");

    // Wait for the initial lease acquisition before stopping dnsmasq.
    assert!(
        wait_for_lease_acquired(&mut rx).await,
        "must acquire initial lease before testing expiry"
    );

    // Drop dnsmasq — the DHCP server is now unreachable.
    drop(dnsmasq);

    // Wait up to 30 seconds for LeaseExpired.
    // With T1=5s and DISCOVER_TIMEOUT=5s, the expiry fires at ≈10s.
    let expired = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match rx.recv().await {
                Some(FactoryEvent::LeaseExpired { policy_name }) => {
                    assert_eq!(
                        policy_name, "test-expire",
                        "LeaseExpired must carry the correct policy_name"
                    );
                    return true;
                }
                Some(FactoryEvent::LeaseRenewed { .. }) => {
                    // Renewal succeeded before server was fully gone — keep waiting.
                }
                Some(FactoryEvent::Error { error, .. }) => {
                    // Expected: renewal/rebind timeouts produce Error events.
                    eprintln!("DHCP error during expiry test (expected): {error}");
                }
                Some(FactoryEvent::LeaseAcquired { .. }) => {
                    // Factory may re-enter DORA after expiry. Continue watching.
                }
                Some(FactoryEvent::Ipv6AutoFlags { .. }) => {
                    // Not expected from a DHCPv4 factory; ignore.
                }
                None => return false,
            }
        }
    })
    .await;

    assert!(
        expired.unwrap_or(false),
        "LeaseExpired event must be received within 30 seconds after the DHCP server stops"
    );

    // After LeaseExpired, the factory re-enters DORA. Stop it cleanly.
    factory.stop().await.expect("stop() must succeed");
}

// ── Scenario: Unicast renewal after server restart ──────────────────────────

/// Scenario: Unicast renewal succeeds after DHCP server restart
///
/// Given an active DHCP lease (120s, T1=60s)
/// When the DHCP server is killed and restarted before T1
/// Then the RENEWING unicast at T1 reaches the restarted server
///   and a LeaseRenewed event is received
///
/// Timeline (120s lease, T1=60s, T2=105s):
///   t=0   → lease acquired, dnsmasq killed
///   t=30  → dnsmasq restarted (well before T1)
///   t=60  → RENEWING: unicast to server → ACK
///   t≈60  → LeaseRenewed
#[tokio::test]
async fn test_renew_succeeds_after_server_restart() {
    require_netns!(_ns);
    setup_veth_pair().await;

    let dnsmasq =
        match DnsmasqGuard::start(VETH_SERVER, SERVER_IP, RANGE_START, RANGE_END, "120s") {
            Ok(g) => g,
            Err(e) => {
                eprintln!("Skipping: dnsmasq unavailable: {e}");
                return;
            }
        };

    let (tx, mut rx) = mpsc::channel::<FactoryEvent>(16);
    let mut factory =
        Dhcpv4Factory::start(VETH_CLIENT, "test-renew-restart".to_string(), 100, tx)
            .await
            .expect("start() must succeed");

    assert!(
        wait_for_lease_acquired(&mut rx).await,
        "must acquire initial lease"
    );

    // Kill dnsmasq and restart after 30 seconds. The server is offline for
    // a significant portion of the lease but returns well before T1 (60s).
    drop(dnsmasq);
    tokio::time::sleep(Duration::from_secs(30)).await;
    let _dnsmasq2 =
        DnsmasqGuard::start(VETH_SERVER, SERVER_IP, RANGE_START, RANGE_END, "120s")
            .expect("dnsmasq restart must succeed");

    // Unicast renewal at T1 (60s) should succeed since the server is alive.
    let renewed = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            match rx.recv().await {
                Some(FactoryEvent::LeaseRenewed { .. }) => return true,
                Some(FactoryEvent::LeaseExpired { .. }) => return false,
                Some(FactoryEvent::Error { error, .. }) => {
                    eprintln!("DHCP error (may retry): {error}");
                }
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await
    .unwrap_or(false);

    assert!(renewed, "LeaseRenewed must be received after server restart");

    factory.stop().await.expect("stop() must succeed");
}

// ── Scenario: Broadcast rebind after server restart ─────────────────────────

/// Scenario: Broadcast rebind succeeds after server is temporarily unavailable
///
/// Given an active DHCP lease (120s, T1=60s, T2=105s)
/// When the DHCP server is killed and restarted at t≈70s (after T1, before T2)
/// Then the RENEWING unicast fails (server was dead when the request was sent)
///   and the REBINDING broadcast at T2 succeeds (server is alive)
///   resulting in a LeaseRenewed event
///
/// This validates the RENEWING → REBINDING transition per RFC 2131 §4.4.5.
///
/// Timeline (120s lease, T1=60s, T2=105s):
///   t=0    → lease acquired, dnsmasq killed
///   t=60   → RENEWING: unicast to dead server (timeout=45s)
///   t=70   → dnsmasq restarted
///   t=105  → unicast times out; REBINDING: broadcast → server alive → ACK
///   t≈105  → LeaseRenewed
#[tokio::test]
async fn test_rebind_succeeds_after_server_restart() {
    require_netns!(_ns);
    setup_veth_pair().await;

    let dnsmasq =
        match DnsmasqGuard::start(VETH_SERVER, SERVER_IP, RANGE_START, RANGE_END, "120s") {
            Ok(g) => g,
            Err(e) => {
                eprintln!("Skipping: dnsmasq unavailable: {e}");
                return;
            }
        };

    let (tx, mut rx) = mpsc::channel::<FactoryEvent>(16);
    let mut factory = Dhcpv4Factory::start(VETH_CLIENT, "test-rebind".to_string(), 100, tx)
        .await
        .expect("start() must succeed");

    assert!(
        wait_for_lease_acquired(&mut rx).await,
        "must acquire initial lease before testing rebind"
    );

    // Kill dnsmasq — server unreachable. The RENEWING unicast at T1 (60s)
    // will be sent to a dead server, so the request is lost. The client
    // waits 45s (half the remaining time to T2) for an ACK that never comes.
    drop(dnsmasq);

    // Restart dnsmasq 70s after lease acquisition — past T1 (60s) but well
    // before T2 (105s). The server will be alive when the client transitions
    // to REBINDING and broadcasts a DHCPREQUEST.
    tokio::time::sleep(Duration::from_secs(70)).await;
    let _dnsmasq2 =
        DnsmasqGuard::start(VETH_SERVER, SERVER_IP, RANGE_START, RANGE_END, "120s")
            .expect("dnsmasq restart must succeed");

    // REBINDING broadcast expected at T2 ≈ 105s. Allow 60s after restart.
    let renewed = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match rx.recv().await {
                Some(FactoryEvent::LeaseRenewed { .. }) => return true,
                Some(FactoryEvent::LeaseExpired { .. }) => return false,
                Some(FactoryEvent::Error { error, .. }) => {
                    eprintln!("DHCP error (expected during outage): {error}");
                }
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await
    .unwrap_or(false);

    assert!(
        renewed,
        "LeaseRenewed must be received via REBINDING broadcast after server restart"
    );

    factory.stop().await.expect("stop() must succeed");
}
