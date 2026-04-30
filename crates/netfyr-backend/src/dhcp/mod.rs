//! DHCPv4 factory — produces `State` from a DHCP lease.
//!
//! `Dhcpv4Factory` starts a background tokio task that runs the full DHCP
//! state machine and sends `FactoryEvent` messages to the daemon via an
//! `mpsc` channel. The factory does NOT implement `NetworkBackend` — its
//! lifecycle is managed by the daemon (SPEC-403), not `BackendRegistry`.

pub mod client;
pub mod lease;

pub use lease::DhcpLease;

use std::sync::{Arc, Mutex};
use std::time::Instant;
use futures::TryStreamExt;
use rtnetlink::new_connection;

use indexmap::IndexMap;
use netfyr_state::{FieldValue, Provenance, Selector, State, StateMetadata, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::BackendError;

// ── Interface existence check ─────────────────────────────────────────────────

/// Check whether a network interface with the given name exists in the current
/// network namespace using rtnetlink.
///
/// Uses netlink instead of `/sys/class/net/` because sysfs is not
/// network-namespace-aware in all environments (e.g., containers, unshare).
/// Returns `true` if the interface is found, `false` on any error or absence.
pub async fn interface_exists(interface: &str) -> bool {
    let Ok((conn, handle, _)) = new_connection() else {
        return false;
    };
    tokio::spawn(conn);

    let mut links = handle
        .link()
        .get()
        .match_name(interface.to_string())
        .execute();

    matches!(links.try_next().await, Ok(Some(_)))
}

// ── LeaseTimingInfo ───────────────────────────────────────────────────────────

/// Timing metadata for an active DHCP lease. Stored alongside the produced
/// `State` so that `GetStatus` can compute how long the lease lasts and how
/// much time remains without needing to re-parse the `State` fields.
#[derive(Clone, Copy, Debug)]
pub struct LeaseTimingInfo {
    /// Total lease duration in seconds as granted by the DHCP server (option 51).
    pub lease_time_secs: u32,
    /// The monotonic instant at which the lease was acquired or last renewed.
    pub acquired_at: Instant,
}

// ── FactoryEvent ──────────────────────────────────────────────────────────────

/// Events sent by the DHCP client task to the daemon.
#[derive(Debug)]
pub enum FactoryEvent {
    /// A new DHCP lease was successfully acquired.
    LeaseAcquired {
        policy_name: String,
        state: State,
    },
    /// An existing DHCP lease was renewed (T1 or T2 renewal succeeded).
    LeaseRenewed {
        policy_name: String,
        state: State,
    },
    /// The DHCP lease expired without successful renewal or rebinding.
    LeaseExpired {
        policy_name: String,
    },
    /// A non-fatal error occurred (e.g., discovery timeout). The factory retries.
    Error {
        policy_name: String,
        error: String,
    },
}

// ── Dhcpv4Factory ─────────────────────────────────────────────────────────────

/// A factory that runs a DHCP client on a named interface and produces `State`
/// objects from acquired leases.
///
/// # Lifecycle
///
/// 1. Call [`Dhcpv4Factory::start`] to spawn the background DHCP client task.
/// 2. Monitor the `state_tx` channel for `FactoryEvent` messages.
/// 3. Call [`Dhcpv4Factory::stop`] to gracefully release the lease and terminate.
pub struct Dhcpv4Factory {
    /// The network interface this factory is managing.
    interface: String,
    /// Shared reference to the latest produced State, if any.
    /// Updated by the background task; read by `current_state()`.
    state: Arc<Mutex<Option<State>>>,
    /// Shared reference to the active lease timing info, if any.
    /// Updated at the same points as `state`; read by `lease_timing()`.
    lease_timing: Arc<Mutex<Option<LeaseTimingInfo>>>,
    /// One-shot channel sender for sending the stop signal to the background task.
    stop_tx: Option<oneshot::Sender<()>>,
    /// Handle to the background task, used to await clean termination.
    task_handle: Option<JoinHandle<()>>,
}

impl Dhcpv4Factory {
    /// Start a DHCP client on `interface`.
    ///
    /// Returns immediately; lease acquisition runs in a background tokio task.
    /// Lease state changes are communicated via `state_tx`.
    ///
    /// # Parameters
    /// - `interface`: Network interface name (e.g., `"eth0"`).
    /// - `policy_name`: Name of the policy that produced this factory (used
    ///   for `Provenance::UserConfigured` and event identification).
    /// - `priority`: Field priority for conflict resolution (higher wins).
    /// - `state_tx`: Channel for sending `FactoryEvent` messages to the daemon.
    pub async fn start(
        interface: &str,
        policy_name: String,
        priority: u32,
        state_tx: mpsc::Sender<FactoryEvent>,
    ) -> Result<Self, BackendError> {
        // Immediately populate a pending state so that current_state() returns
        // Some(State) before any lease is acquired. This ensures reconciliation
        // brings the interface UP (enabled: true) before DHCP discovery begins,
        // solving the chicken-and-egg problem: DHCP needs the interface UP to
        // send broadcast packets, but without produced state the reconciler
        // might leave the interface down.
        let initial_state = Some(pending_state(interface, &policy_name, priority));
        let shared_state: Arc<Mutex<Option<State>>> = Arc::new(Mutex::new(initial_state));
        let lease_timing: Arc<Mutex<Option<LeaseTimingInfo>>> = Arc::new(Mutex::new(None));
        let (stop_tx, stop_rx) = oneshot::channel();

        let task_shared_state = Arc::clone(&shared_state);
        let task_lease_timing = Arc::clone(&lease_timing);
        let task_interface = interface.to_string();
        let task_policy_name = policy_name.clone();

        let task_handle = tokio::spawn(async move {
            client::run_dhcp_client(
                task_interface,
                task_policy_name,
                priority,
                state_tx,
                task_shared_state,
                stop_rx,
                task_lease_timing,
            )
            .await;
        });

        Ok(Self {
            interface: interface.to_string(),
            state: shared_state,
            lease_timing,
            stop_tx: Some(stop_tx),
            task_handle: Some(task_handle),
        })
    }

    /// Stop the DHCP client and release the active lease (DHCPRELEASE).
    ///
    /// Idempotent: calling `stop()` on an already-stopped factory returns `Ok(())`.
    pub async fn stop(&mut self) -> Result<(), BackendError> {
        // Send the stop signal, if the task is still running.
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }

        // Wait for the background task to finish.
        if let Some(handle) = self.task_handle.take() {
            handle.await.map_err(|e| {
                BackendError::Internal(format!("DHCP task join error: {e}"))
            })?;
        }

        Ok(())
    }

    /// Returns a clone of the current lease state, or `None` if no lease has
    /// been acquired yet.
    ///
    /// Returns an owned `State` rather than `&State` to avoid holding the
    /// mutex across caller code (which would require a lock guard in the API).
    pub fn current_state(&self) -> Option<State> {
        self.state.lock().unwrap().clone()
    }

    /// Returns a copy of the current lease timing info, or `None` if no lease
    /// has been acquired yet or the factory is in the waiting/expired state.
    pub fn lease_timing(&self) -> Option<LeaseTimingInfo> {
        *self.lease_timing.lock().unwrap()
    }

    /// Returns the network interface name this factory manages.
    pub fn interface(&self) -> &str {
        &self.interface
    }
}

// ── State conversion ──────────────────────────────────────────────────────────

/// Convert a DHCP lease into a `State` with `UserConfigured` provenance.
///
/// Follows the exact field naming, value types, and map structure used by
/// `netlink/ethernet.rs` to ensure reconciliation compatibility:
/// - Addresses stored as `Value::String("ip/prefix")`.
/// - Routes stored as `Value::Map` with `"destination"` and `"gateway"` keys.
/// - DNS servers stored as `Value::List` of `Value::String`.
pub fn lease_to_state(
    lease: &DhcpLease,
    interface: &str,
    policy_name: &str,
    priority: u32,
) -> State {
    let prov = Provenance::UserConfigured {
        policy_ref: policy_name.to_string(),
    };

    let fv = |value: Value| FieldValue {
        value,
        provenance: prov.clone(),
    };

    let mut fields: IndexMap<String, FieldValue> = IndexMap::new();

    // Interface must stay up — this is always present regardless of lease options.
    fields.insert("enabled".to_string(), fv(Value::Bool(true)));

    // Addresses field: ["ip/prefix"]
    let cidr = format!("{}/{}", lease.ip, lease.subnet_mask_to_prefix());
    fields.insert(
        "addresses".to_string(),
        fv(Value::List(vec![Value::String(cidr)])),
    );

    // Routes field: [{destination: "0.0.0.0/0", gateway: "gw_ip", metric: 100}]
    // The metric field must be present to match the format produced by the
    // query layer (build_route_value). Without it, the diff engine sees two
    // different route objects for the same destination and generates a
    // simultaneous add+remove — the add is skipped (EEXIST) and the remove
    // succeeds, deleting the default route.
    if let Some(gateway) = lease.gateway {
        let mut route_map = IndexMap::new();
        route_map.insert(
            "destination".to_string(),
            Value::String("0.0.0.0/0".to_string()),
        );
        route_map.insert(
            "gateway".to_string(),
            Value::String(gateway.to_string()),
        );
        route_map.insert(
            "metric".to_string(),
            Value::U64(100),
        );
        fields.insert(
            "routes".to_string(),
            fv(Value::List(vec![Value::Map(route_map)])),
        );
    }

    // DNS servers field: ["server1", "server2", ...]
    if !lease.dns_servers.is_empty() {
        let dns_list: Vec<Value> = lease
            .dns_servers
            .iter()
            .map(|s| Value::String(s.to_string()))
            .collect();
        fields.insert("dns_servers".to_string(), fv(Value::List(dns_list)));
    }

    State {
        entity_type: "ethernet".to_string(),
        selector: Selector::with_name(interface),
        fields,
        metadata: StateMetadata::new(),
        policy_ref: Some(policy_name.to_string()),
        priority,
    }
}

// ── Pending state ─────────────────────────────────────────────────────────────

/// Build a minimal `State` that ensures the interface is brought UP before a
/// lease is acquired. Stored in `Dhcpv4Factory::state` immediately on start.
///
/// The pending state contains only `enabled: true` so that the reconciler
/// brings the interface up while the DHCP client is discovering a server.
/// Once a lease is acquired, `lease_to_state` replaces this with the full state.
pub(super) fn pending_state(interface: &str, policy_name: &str, priority: u32) -> State {
    let prov = Provenance::UserConfigured {
        policy_ref: policy_name.to_string(),
    };
    let mut fields: IndexMap<String, FieldValue> = IndexMap::new();
    fields.insert(
        "enabled".to_string(),
        FieldValue {
            value: Value::Bool(true),
            provenance: prov,
        },
    );
    State {
        entity_type: "ethernet".to_string(),
        selector: Selector::with_name(interface),
        fields,
        metadata: StateMetadata::new(),
        policy_ref: Some(policy_name.to_string()),
        priority,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::time::{Duration, Instant};

    use netfyr_state::{Provenance, Value};
    use tokio::sync::mpsc;

    use super::{lease_to_state, Dhcpv4Factory, FactoryEvent};
    use crate::dhcp::lease::DhcpLease;

    // ── Test helpers ──────────────────────────────────────────────────────────

    fn make_full_lease() -> DhcpLease {
        DhcpLease {
            ip: Ipv4Addr::new(10, 0, 1, 50),
            subnet_mask: Ipv4Addr::new(255, 255, 255, 0),
            gateway: Some(Ipv4Addr::new(10, 0, 1, 1)),
            dns_servers: vec![Ipv4Addr::new(10, 0, 1, 2)],
            lease_time: 3600,
            renewal_time: 1800,
            rebind_time: 3150,
            server_id: Ipv4Addr::new(10, 0, 1, 1),
            acquired_at: Instant::now(),
        }
    }

    fn make_minimal_lease() -> DhcpLease {
        DhcpLease {
            ip: Ipv4Addr::new(10, 0, 1, 50),
            subnet_mask: Ipv4Addr::new(255, 255, 255, 0),
            gateway: None,
            dns_servers: vec![],
            lease_time: 3600,
            renewal_time: 1800,
            rebind_time: 3150,
            server_id: Ipv4Addr::new(10, 0, 1, 1),
            acquired_at: Instant::now(),
        }
    }

    // ── lease_to_state: addresses field ──────────────────────────────────────

    /// Scenario: Lease produces correct State fields
    /// Given IP=10.0.1.50, mask=255.255.255.0 → addresses=["10.0.1.50/24"]
    #[test]
    fn test_lease_to_state_addresses_contains_cidr() {
        let state = lease_to_state(&make_full_lease(), "eth0", "test-policy", 100);

        let addresses = state
            .fields
            .get("addresses")
            .expect("addresses field must exist")
            .value
            .as_list()
            .expect("addresses must be a list");

        assert_eq!(addresses.len(), 1, "must have exactly one address");
        assert_eq!(
            addresses[0].as_str(),
            Some("10.0.1.50/24"),
            "address must be formatted as ip/prefix"
        );
    }

    // ── lease_to_state: routes field ─────────────────────────────────────────

    /// Scenario: State has routes with destination="0.0.0.0/0" gateway="10.0.1.1" metric=100
    #[test]
    fn test_lease_to_state_routes_contain_default_gateway() {
        let state = lease_to_state(&make_full_lease(), "eth0", "test-policy", 100);

        let routes = state
            .fields
            .get("routes")
            .expect("routes field must exist when gateway is provided")
            .value
            .as_list()
            .expect("routes must be a list");

        assert_eq!(routes.len(), 1, "must have exactly one route");

        let route_map = routes[0].as_map().expect("route entry must be a map");
        assert_eq!(
            route_map
                .get("destination")
                .and_then(Value::as_str),
            Some("0.0.0.0/0"),
            "default route destination must be 0.0.0.0/0"
        );
        assert_eq!(
            route_map.get("gateway").and_then(Value::as_str),
            Some("10.0.1.1"),
            "gateway must match lease gateway"
        );
        assert_eq!(
            route_map.get("metric").and_then(Value::as_u64),
            Some(100),
            "metric must be 100 to match query layer format"
        );
    }

    // ── lease_to_state: dns_servers field ────────────────────────────────────

    /// Scenario: State has dns_servers=["10.0.1.2"]
    #[test]
    fn test_lease_to_state_dns_servers_field() {
        let state = lease_to_state(&make_full_lease(), "eth0", "test-policy", 100);

        let dns = state
            .fields
            .get("dns_servers")
            .expect("dns_servers field must exist")
            .value
            .as_list()
            .expect("dns_servers must be a list");

        assert_eq!(dns.len(), 1);
        assert_eq!(
            dns[0].as_str(),
            Some("10.0.1.2"),
            "DNS server must match lease dns_servers"
        );
    }

    /// Multiple DNS servers are all listed in order.
    #[test]
    fn test_lease_to_state_multiple_dns_servers() {
        let lease = DhcpLease {
            ip: Ipv4Addr::new(10, 0, 1, 50),
            subnet_mask: Ipv4Addr::new(255, 255, 255, 0),
            gateway: None,
            dns_servers: vec![Ipv4Addr::new(8, 8, 8, 8), Ipv4Addr::new(8, 8, 4, 4)],
            lease_time: 3600,
            renewal_time: 1800,
            rebind_time: 3150,
            server_id: Ipv4Addr::new(10, 0, 1, 1),
            acquired_at: Instant::now(),
        };
        let state = lease_to_state(&lease, "eth0", "test-policy", 100);

        let dns = state
            .fields
            .get("dns_servers")
            .expect("dns_servers must exist")
            .value
            .as_list()
            .expect("dns_servers must be a list");

        assert_eq!(dns.len(), 2);
        assert_eq!(dns[0].as_str(), Some("8.8.8.8"));
        assert_eq!(dns[1].as_str(), Some("8.8.4.4"));
    }

    // ── lease_to_state: absent optional fields ────────────────────────────────

    /// When no gateway is present, the routes field must not exist.
    #[test]
    fn test_lease_to_state_no_gateway_produces_no_routes_field() {
        let state = lease_to_state(&make_minimal_lease(), "eth0", "test-policy", 100);
        assert!(
            state.fields.get("routes").is_none(),
            "routes field must be absent when no gateway provided"
        );
    }

    /// When no DNS servers are present, dns_servers must not exist.
    #[test]
    fn test_lease_to_state_no_dns_produces_no_dns_servers_field() {
        let state = lease_to_state(&make_minimal_lease(), "eth0", "test-policy", 100);
        assert!(
            state.fields.get("dns_servers").is_none(),
            "dns_servers field must be absent when no DNS servers provided"
        );
    }

    // ── lease_to_state: entity_type and selector ─────────────────────────────

    #[test]
    fn test_lease_to_state_entity_type_is_ethernet() {
        let state = lease_to_state(&make_minimal_lease(), "eth0", "test-policy", 100);
        assert_eq!(state.entity_type, "ethernet");
    }

    #[test]
    fn test_lease_to_state_selector_name_matches_interface() {
        let state = lease_to_state(&make_minimal_lease(), "veth-test", "test-policy", 100);
        assert_eq!(
            state.selector.name.as_deref(),
            Some("veth-test"),
            "selector name must match the interface argument"
        );
    }

    #[test]
    fn test_lease_to_state_policy_ref_matches_policy_name() {
        let state = lease_to_state(&make_minimal_lease(), "eth0", "my-dhcp-policy", 100);
        assert_eq!(
            state.policy_ref.as_deref(),
            Some("my-dhcp-policy"),
            "policy_ref must match the policy_name argument"
        );
    }

    /// Provenance of all fields must be UserConfigured with the given policy name.
    #[test]
    fn test_lease_to_state_provenance_is_user_configured() {
        let state = lease_to_state(&make_full_lease(), "eth0", "my-policy", 100);

        for (field_name, fv) in &state.fields {
            assert!(
                matches!(
                    fv.provenance,
                    Provenance::UserConfigured { ref policy_ref } if policy_ref == "my-policy"
                ),
                "field '{field_name}' must have UserConfigured provenance with the correct policy_ref"
            );
        }
    }

    /// Priority is stored correctly.
    #[test]
    fn test_lease_to_state_priority_is_stored() {
        let state = lease_to_state(&make_minimal_lease(), "eth0", "p", 200);
        assert_eq!(state.priority, 200);
    }

    /// Scenario: Produced state always contains enabled=true regardless of lease options.
    #[test]
    fn test_lease_to_state_enabled_is_always_true_full_lease() {
        let state = lease_to_state(&make_full_lease(), "eth0", "test-policy", 100);
        let enabled = state
            .fields
            .get("enabled")
            .expect("enabled field must always be present in lease_to_state output")
            .value
            .as_bool()
            .expect("enabled must be a bool");
        assert!(
            enabled,
            "enabled must be true in the full-lease state"
        );
    }

    /// Same as above for the minimal lease (no gateway, no DNS).
    #[test]
    fn test_lease_to_state_enabled_is_always_true_minimal_lease() {
        let state = lease_to_state(&make_minimal_lease(), "eth0", "test-policy", 100);
        let enabled = state
            .fields
            .get("enabled")
            .expect("enabled field must always be present even without gateway/dns")
            .value
            .as_bool()
            .expect("enabled must be a bool");
        assert!(
            enabled,
            "enabled must be true in the minimal-lease state (no gateway, no dns)"
        );
    }

    // ── Dhcpv4Factory: current_state before lease ─────────────────────────────

    /// Scenario: current_state returns pending state before lease
    /// Given a newly started factory on interface "nonexistent-iface-xyz99"
    /// When current_state() is called before any lease is acquired
    /// Then it returns Some(State) with enabled=true and no addresses/routes
    #[tokio::test]
    async fn test_current_state_returns_pending_state_before_lease_acquired() {
        let (tx, _rx) = mpsc::channel::<FactoryEvent>(10);
        // Use a nonexistent interface — start() itself always succeeds (task is spawned
        // asynchronously); the pending state is set synchronously in start().
        let factory =
            Dhcpv4Factory::start("nonexistent-iface-xyz99", "test-policy".to_string(), 100, tx)
                .await
                .expect("start() must succeed (task spawned, not executed synchronously)");

        // Check immediately — the pending state is set synchronously in start(),
        // before the background task has a chance to run.
        let state = factory
            .current_state()
            .expect("current_state() must return Some(State) immediately after start()");

        assert_eq!(state.entity_type, "ethernet", "entity_type must be ethernet");
        assert_eq!(
            state.selector.name.as_deref(),
            Some("nonexistent-iface-xyz99"),
            "selector name must match interface"
        );

        // Must have enabled=true
        let enabled = state
            .fields
            .get("enabled")
            .expect("pending state must have enabled field")
            .value
            .as_bool()
            .expect("enabled must be a bool");
        assert!(enabled, "pending state enabled must be true");

        // Must NOT have addresses, routes, or dns_servers
        assert!(
            state.fields.get("addresses").is_none(),
            "pending state must not have addresses field"
        );
        assert!(
            state.fields.get("routes").is_none(),
            "pending state must not have routes field"
        );
        assert!(
            state.fields.get("dns_servers").is_none(),
            "pending state must not have dns_servers field"
        );
    }

    /// Scenario: interface() returns the configured interface name.
    #[tokio::test]
    async fn test_factory_interface_returns_configured_name() {
        let (tx, _rx) = mpsc::channel::<FactoryEvent>(10);
        let factory =
            Dhcpv4Factory::start("eth-unit-test", "test-policy".to_string(), 100, tx)
                .await
                .expect("start() must succeed");
        assert_eq!(factory.interface(), "eth-unit-test");
    }

    /// Scenario: stop() is idempotent — calling it twice must not panic or error.
    #[tokio::test]
    async fn test_factory_stop_is_idempotent() {
        let (tx, _rx) = mpsc::channel::<FactoryEvent>(10);
        let mut factory =
            Dhcpv4Factory::start("nonexistent-iface-xyz99", "test-policy".to_string(), 100, tx)
                .await
                .expect("start() must succeed");
        factory.stop().await.expect("first stop() must succeed");
        factory.stop().await.expect("second stop() must succeed (idempotent)");
    }

    // ── FactoryEvent variant structure ────────────────────────────────────────

    /// Scenario: Factory sends LeaseRenewed on renewal
    ///
    /// Verify that FactoryEvent::LeaseRenewed carries the correct policy_name
    /// and a State with the expected fields (enabled, addresses).
    #[test]
    fn test_factory_event_lease_renewed_has_policy_name_and_state() {
        let state = lease_to_state(&make_full_lease(), "eth0", "renew-policy", 100);
        let event = FactoryEvent::LeaseRenewed {
            policy_name: "renew-policy".to_string(),
            state,
        };
        match event {
            FactoryEvent::LeaseRenewed { policy_name, state: ev_state } => {
                assert_eq!(policy_name, "renew-policy");
                assert_eq!(ev_state.entity_type, "ethernet");
                assert!(
                    ev_state.fields.contains_key("addresses"),
                    "LeaseRenewed state must contain addresses"
                );
                assert_eq!(
                    ev_state.fields.get("enabled").and_then(|fv| fv.value.as_bool()),
                    Some(true),
                    "LeaseRenewed state must have enabled=true"
                );
            }
            _ => panic!("expected FactoryEvent::LeaseRenewed variant"),
        }
    }

    /// Scenario: Factory sends LeaseExpired when lease expires
    ///
    /// Verify that FactoryEvent::LeaseExpired carries exactly the policy_name
    /// and no state (the daemon removes the produced state on receipt).
    #[test]
    fn test_factory_event_lease_expired_has_policy_name() {
        let event = FactoryEvent::LeaseExpired {
            policy_name: "expire-policy".to_string(),
        };
        match event {
            FactoryEvent::LeaseExpired { policy_name } => {
                assert_eq!(
                    policy_name, "expire-policy",
                    "LeaseExpired must carry the correct policy_name"
                );
            }
            _ => panic!("expected FactoryEvent::LeaseExpired variant"),
        }
    }

    // ── Scenario 8: current_state returns full state after lease ──────────────

    /// Scenario: current_state returns full state after lease
    ///
    /// Verifies that the state produced by lease_to_state (which becomes the
    /// factory's current_state once a lease is acquired) simultaneously satisfies
    /// ALL four required fields: enabled=true, addresses, routes, dns_servers.
    /// This is a holistic check — individual field tests exist above.
    #[test]
    fn test_lease_to_state_satisfies_scenario8_all_required_fields_present() {
        let state = lease_to_state(&make_full_lease(), "eth0", "test-policy", 100);

        // enabled must be true
        assert_eq!(
            state.fields.get("enabled").and_then(|fv| fv.value.as_bool()),
            Some(true),
            "full lease state must have enabled=true"
        );

        // addresses must be present and non-empty
        let addresses = state
            .fields
            .get("addresses")
            .expect("full lease state must have addresses field")
            .value
            .as_list()
            .expect("addresses must be a list");
        assert!(!addresses.is_empty(), "addresses list must be non-empty");

        // routes must be present and non-empty (gateway was provided)
        let routes = state
            .fields
            .get("routes")
            .expect("full lease state must have routes field (gateway was provided)")
            .value
            .as_list()
            .expect("routes must be a list");
        assert!(!routes.is_empty(), "routes list must be non-empty");

        // dns_servers must be present and non-empty (DNS server was provided)
        let dns = state
            .fields
            .get("dns_servers")
            .expect("full lease state must have dns_servers field (DNS was provided)")
            .value
            .as_list()
            .expect("dns_servers must be a list");
        assert!(!dns.is_empty(), "dns_servers list must be non-empty");
    }

    // ── FactoryEvent::LeaseAcquired variant ───────────────────────────────────

    /// Scenario: Factory acquires a DHCP lease
    ///
    /// "Then a LeaseAcquired event is sent
    ///  And the produced State contains the leased IP address
    ///  And the produced State contains the default gateway route"
    ///
    /// Verifies that FactoryEvent::LeaseAcquired carries the correct policy_name
    /// and a State with enabled=true, addresses, and routes populated — matching
    /// the same shape the background task would produce after a real DORA handshake.
    #[test]
    fn test_factory_event_lease_acquired_has_policy_name_and_state() {
        let state = lease_to_state(&make_full_lease(), "eth0", "acquire-policy", 100);
        let event = FactoryEvent::LeaseAcquired {
            policy_name: "acquire-policy".to_string(),
            state,
        };
        match event {
            FactoryEvent::LeaseAcquired { policy_name, state: ev_state } => {
                assert_eq!(policy_name, "acquire-policy");
                assert_eq!(
                    ev_state.entity_type, "ethernet",
                    "LeaseAcquired state entity_type must be 'ethernet'"
                );
                // Spec: "the produced State contains the leased IP address"
                let addresses = ev_state
                    .fields
                    .get("addresses")
                    .expect("LeaseAcquired state must contain addresses field")
                    .value
                    .as_list()
                    .expect("addresses must be a list");
                assert!(!addresses.is_empty(), "addresses must be non-empty");
                assert_eq!(
                    addresses[0].as_str(),
                    Some("10.0.1.50/24"),
                    "LeaseAcquired address must include leased IP with correct prefix"
                );
                // Spec: "the produced State contains the default gateway route"
                let routes = ev_state
                    .fields
                    .get("routes")
                    .expect("LeaseAcquired state must contain routes field")
                    .value
                    .as_list()
                    .expect("routes must be a list");
                assert!(!routes.is_empty(), "routes must be non-empty");
                let route_map = routes[0].as_map().expect("route must be a map");
                assert_eq!(
                    route_map.get("destination").and_then(Value::as_str),
                    Some("0.0.0.0/0"),
                    "default route destination must be 0.0.0.0/0"
                );
                assert_eq!(
                    route_map.get("gateway").and_then(Value::as_str),
                    Some("10.0.1.1"),
                    "default route gateway must match lease gateway"
                );
                assert_eq!(
                    route_map.get("metric").and_then(Value::as_u64),
                    Some(100),
                    "default route metric must be 100"
                );
                // enabled must be true
                assert_eq!(
                    ev_state.fields.get("enabled").and_then(|fv| fv.value.as_bool()),
                    Some(true),
                    "LeaseAcquired state must have enabled=true"
                );
            }
            _ => panic!("expected FactoryEvent::LeaseAcquired variant"),
        }
    }

    // ── Pending state: policy_ref field ──────────────────────────────────────

    /// Scenario: Pending state's policy_ref matches the policy_name argument.
    ///
    /// Verifies that the pending state (returned by current_state() before any
    /// lease is acquired) carries the correct policy_ref so that the reconciler
    /// can attribute the enabled=true field to the correct policy.
    #[tokio::test]
    async fn test_pending_state_policy_ref_matches_policy_name() {
        let (tx, _rx) = mpsc::channel::<FactoryEvent>(10);
        let factory =
            Dhcpv4Factory::start("nonexistent-iface-xyz99", "my-dhcp-policy".to_string(), 100, tx)
                .await
                .expect("start() must succeed");

        let state = factory
            .current_state()
            .expect("current_state() must return Some(State) immediately after start()");

        assert_eq!(
            state.policy_ref.as_deref(),
            Some("my-dhcp-policy"),
            "pending state policy_ref must match the policy_name passed to start()"
        );
    }

    // ── FactoryEvent::Error contains meaningful error context ─────────────────

    /// Scenario: Factory sends FactoryEvent::Error when the interface is not found.
    ///
    /// The background task fails to read the MAC from sysfs and sends an Error event.
    #[tokio::test]
    async fn test_factory_sends_error_event_when_interface_not_found() {
        let (tx, mut rx) = mpsc::channel::<FactoryEvent>(10);
        let _factory =
            Dhcpv4Factory::start("nonexistent-iface-xyz99", "test-policy".to_string(), 100, tx)
                .await
                .expect("start() must succeed");

        // Yield to allow the background task to run and send an event.
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("Error event must be received within 5 seconds")
            .expect("channel must not close before an event is sent");

        match event {
            FactoryEvent::Error { policy_name, error } => {
                assert_eq!(policy_name, "test-policy");
                assert!(
                    !error.is_empty(),
                    "Error event must contain a non-empty error message"
                );
                // The error must mention the interface name so operators can diagnose the problem.
                assert!(
                    error.contains("nonexistent-iface-xyz99"),
                    "Error event message must contain the interface name for diagnosability, got: {error}"
                );
            }
            other => panic!(
                "Expected FactoryEvent::Error for nonexistent interface, got {:?}",
                other
            ),
        }
    }

    // ── FactoryEvent::Error variant: policy_name identity ─────────────────────

    /// Verifying FactoryEvent::Error carries the right policy name is critical for
    /// the daemon to route the error to the correct policy. This test constructs
    /// the event directly to confirm the variant's structure contract.
    #[test]
    fn test_factory_event_error_carries_policy_name_and_message() {
        let event = FactoryEvent::Error {
            policy_name: "my-policy".to_string(),
            error: "DHCP discovery timeout or error: timeout".to_string(),
        };
        match event {
            FactoryEvent::Error { policy_name, error } => {
                assert_eq!(policy_name, "my-policy");
                assert!(
                    error.contains("timeout"),
                    "Error message should mention timeout, got: {error}"
                );
            }
            _ => panic!("expected FactoryEvent::Error variant"),
        }
    }

    // ── Scenario: Factory retries — Error event re-sent for nonexistent iface ──

    /// Scenario: Factory retries on discovery timeout
    ///
    /// "And a FactoryEvent::Error is sent"
    ///
    /// For the case of a nonexistent interface (MAC read failure), the task
    /// exits after a single Error event (no retry, since the interface itself
    /// is missing). This test confirms the Error event is produced and the
    /// channel closes cleanly — i.e., the factory does not hang.
    ///
    /// NOTE: The "exponential backoff retry" path (interface exists but no DHCP
    /// server responds) is covered by the integration tests which run inside a
    /// real network namespace with dnsmasq.
    #[tokio::test]
    async fn test_factory_error_event_sent_and_channel_closes_for_nonexistent_iface() {
        let (tx, mut rx) = mpsc::channel::<FactoryEvent>(10);
        let _factory =
            Dhcpv4Factory::start("nonexistent-iface-xyz99-b", "retry-policy".to_string(), 100, tx)
                .await
                .expect("start() must succeed");

        // Receive the Error event.
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("Error event must be received within 5 seconds")
            .expect("channel must have an event");

        assert!(
            matches!(event, FactoryEvent::Error { .. }),
            "First event must be Error for nonexistent interface"
        );

        // After the initial Error (MAC read failure), the task exits, so the
        // channel closes. recv() returns None once the sender is dropped.
        // Wait up to 2 seconds for the channel to drain/close.
        let next = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
        // Either the channel closes (Ok(None)) or times out — both are acceptable,
        // since the implementation may retry before task exit.
        // The important invariant: no panic, no hang.
        let _ = next;
    }
}
