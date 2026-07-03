//! Factory lifecycle manager for DHCPv4 and IPv6 SLAAC factories.
//!
//! `FactoryManager` owns the set of running `Dhcpv4Factory` and
//! `Ipv6AutoFactory` instances and maintains a single `mpsc` channel through
//! which all factory events flow back to the daemon event loop. The `sync`
//! method provides idempotent convergence: call it with the current policy set
//! after any `SubmitPolicies` request to start new factories and stop removed ones.

use std::collections::HashMap;

use netfyr_backend::{Dhcpv4Factory, FactoryEvent, LeaseTimingInfo};
use netfyr_policy::{FactoryType, Policy};
use netfyr_state::State;
use tokio::sync::mpsc;
use tracing::{error, warn};

use crate::ipv6auto::Ipv6AutoFactory;

// ── FactoryStatus ─────────────────────────────────────────────────────────────

/// Status of a single running factory, used for `GetStatus` responses.
pub struct FactoryStatus {
    pub policy_name: String,
    /// Factory type string: `"dhcpv4"` or `"ipv6auto"`.
    pub factory_type: String,
    pub interface: String,
    pub has_lease: bool,
    /// The acquired IP address (without prefix length), if a lease is active.
    pub lease_ip: Option<String>,
    /// Full CIDR address from the lease (e.g., `"192.168.122.63/24"`).
    pub lease_address: Option<String>,
    /// Total lease duration in seconds.
    pub lease_time_secs: Option<u32>,
    /// Seconds remaining until lease expiry, computed at query time.
    pub lease_remaining_secs: Option<u64>,
}

// ── FactoryManager ────────────────────────────────────────────────────────────

/// Manages the lifecycle of DHCPv4 and IPv6 SLAAC factories.
///
/// Factories are keyed by policy name. A single `mpsc` channel aggregates
/// events from all factories into the daemon's event loop via `next_event`.
pub struct FactoryManager {
    /// Running DHCPv4 factories, keyed by policy name.
    factories: HashMap<String, Dhcpv4Factory>,
    /// Running IPv6 SLAAC factories, keyed by policy name.
    ipv6auto_factories: HashMap<String, Ipv6AutoFactory>,
    /// Sender cloned into each factory on start.
    event_tx: mpsc::Sender<FactoryEvent>,
    /// Receiver polled by the daemon event loop.
    event_rx: mpsc::Receiver<FactoryEvent>,
}

impl FactoryManager {
    /// Creates a new `FactoryManager` with an empty factory set.
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::channel(64);
        Self {
            factories: HashMap::new(),
            ipv6auto_factories: HashMap::new(),
            event_tx,
            event_rx,
        }
    }

    /// Synchronise the running factory set to match `policies`.
    ///
    /// - Factories whose policy name is **not** in `policies` are stopped.
    /// - Factories whose policy name **is** in `policies` but not yet running
    ///   are started.
    /// - Already-running factories are left untouched (idempotent).
    ///
    /// Returns the names of policies whose factory could not be started (so the
    /// caller can include them in error reporting). Individual failures do not
    /// abort the sync — other policies are still processed.
    pub async fn sync(&mut self, policies: &[Policy]) -> anyhow::Result<Vec<String>> {
        let mut failed = Vec::new();

        // ── DHCPv4 factories ──────────────────────────────────────────────────

        let desired_dhcp: HashMap<String, &Policy> = policies
            .iter()
            .filter(|p| p.factory_type == FactoryType::Dhcpv4)
            .map(|p| (p.name.clone(), p))
            .collect();

        let to_stop_dhcp: Vec<String> = self
            .factories
            .keys()
            .filter(|name| !desired_dhcp.contains_key(*name))
            .cloned()
            .collect();
        for name in to_stop_dhcp {
            if let Some(mut factory) = self.factories.remove(&name) {
                if let Err(e) = factory.stop().await {
                    warn!(policy = %name, error = %e, "Failed to stop DHCP factory");
                }
            }
        }

        for (name, policy) in &desired_dhcp {
            if self.factories.contains_key(name.as_str()) {
                continue;
            }
            let interface = match policy.selector.as_ref().and_then(|s| s.name.as_deref()) {
                Some(iface) => iface.to_string(),
                None => {
                    warn!(
                        policy = %name,
                        "DHCPv4 policy has no interface name in selector; skipping factory start"
                    );
                    failed.push(name.clone());
                    continue;
                }
            };
            if !netfyr_backend::interface_exists(&interface).await {
                error!(
                    policy = %name,
                    interface = %interface,
                    "Interface does not exist; cannot start DHCP factory"
                );
                failed.push(name.clone());
                continue;
            }
            match Dhcpv4Factory::start(
                &interface,
                name.clone(),
                policy.priority,
                self.event_tx.clone(),
            )
            .await
            {
                Ok(factory) => {
                    self.factories.insert(name.clone(), factory);
                }
                Err(e) => {
                    error!(
                        policy = %name,
                        interface = %interface,
                        error = %e,
                        "Failed to start DHCP factory"
                    );
                    failed.push(name.clone());
                }
            }
        }

        // ── IPv6Auto factories ────────────────────────────────────────────────

        let desired_ipv6auto: HashMap<String, &Policy> = policies
            .iter()
            .filter(|p| p.factory_type == FactoryType::Ipv6Auto)
            .map(|p| (p.name.clone(), p))
            .collect();

        let to_stop_ipv6auto: Vec<String> = self
            .ipv6auto_factories
            .keys()
            .filter(|name| !desired_ipv6auto.contains_key(*name))
            .cloned()
            .collect();
        for name in to_stop_ipv6auto {
            if let Some(mut factory) = self.ipv6auto_factories.remove(&name) {
                if let Err(e) = factory.stop().await {
                    warn!(policy = %name, error = %e, "Failed to stop IPv6Auto factory");
                }
            }
        }

        for (name, policy) in &desired_ipv6auto {
            if self.ipv6auto_factories.contains_key(name.as_str()) {
                continue;
            }
            let interface = match policy.selector.as_ref().and_then(|s| s.name.as_deref()) {
                Some(iface) => iface.to_string(),
                None => {
                    warn!(
                        policy = %name,
                        "Ipv6Auto policy has no interface name in selector; skipping factory start"
                    );
                    failed.push(name.clone());
                    continue;
                }
            };
            if !netfyr_backend::interface_exists(&interface).await {
                error!(
                    policy = %name,
                    interface = %interface,
                    "Interface does not exist; cannot start IPv6Auto factory"
                );
                failed.push(name.clone());
                continue;
            }
            match Ipv6AutoFactory::start(
                &interface,
                name.clone(),
                policy.priority,
                self.event_tx.clone(),
            )
            .await
            {
                Ok(factory) => {
                    self.ipv6auto_factories.insert(name.clone(), factory);
                }
                Err(e) => {
                    error!(
                        policy = %name,
                        interface = %interface,
                        error = %e,
                        "Failed to start IPv6Auto factory"
                    );
                    failed.push(name.clone());
                }
            }
        }

        Ok(failed)
    }

    /// Returns the current state produced by all active factories.
    ///
    /// Factories that have not yet produced a state (i.e., `current_state()`
    /// returns `None`) are omitted.
    pub fn produced_states(&self) -> Vec<(String, State)> {
        let mut result: Vec<(String, State)> = self
            .factories
            .iter()
            .filter_map(|(name, factory)| {
                factory.current_state().map(|state| (name.clone(), state))
            })
            .collect();
        result.extend(self.ipv6auto_factories.iter().filter_map(|(name, factory)| {
            factory.current_state().map(|state| (name.clone(), state))
        }));
        result
    }

    /// Stops all running factories. Called on daemon shutdown.
    ///
    /// Individual stop failures are logged but do not abort the loop.
    pub async fn stop_all(&mut self) -> anyhow::Result<()> {
        let dhcp_names: Vec<String> = self.factories.keys().cloned().collect();
        for name in dhcp_names {
            if let Some(mut factory) = self.factories.remove(&name) {
                if let Err(e) = factory.stop().await {
                    warn!(
                        policy = %name,
                        error = %e,
                        "Failed to stop DHCP factory during shutdown"
                    );
                }
            }
        }
        let ipv6_names: Vec<String> = self.ipv6auto_factories.keys().cloned().collect();
        for name in ipv6_names {
            if let Some(mut factory) = self.ipv6auto_factories.remove(&name) {
                if let Err(e) = factory.stop().await {
                    warn!(
                        policy = %name,
                        error = %e,
                        "Failed to stop IPv6Auto factory during shutdown"
                    );
                }
            }
        }
        Ok(())
    }

    /// Extract the factory event receiver for use in the main event loop.
    ///
    /// Must be called once before wrapping `FactoryManager` in a mutex,
    /// because the receiver cannot be polled while the mutex is held across
    /// an async `.recv().await`.
    pub fn take_event_receiver(&mut self) -> mpsc::Receiver<FactoryEvent> {
        let (_, placeholder) = mpsc::channel(1);
        std::mem::replace(&mut self.event_rx, placeholder)
    }

    /// Returns status information for each running factory, for `GetStatus`.
    pub fn factory_statuses(&self) -> Vec<FactoryStatus> {
        let mut result: Vec<FactoryStatus> = self
            .factories
            .iter()
            .map(|(name, factory)| {
                let current = factory.current_state();
                // With pending state, current_state() returns Some(State) even before a
                // lease is acquired (containing only enabled: true). Check for the
                // "addresses" field to distinguish a real lease from a pending state.
                let has_lease = current
                    .as_ref()
                    .is_some_and(|s| s.fields.contains_key("addresses"));
                // Extract the bare IP (without /prefix) from the "addresses" field.
                let lease_ip = current.as_ref().and_then(|s| {
                    let addr_val = s
                        .fields
                        .get("addresses")?
                        .value
                        .as_list()?
                        .first()?
                        .as_map()?
                        .get("address")?;
                    let addr_str = addr_val.to_string();
                    Some(addr_str.split('/').next().unwrap_or(&addr_str).to_string())
                });
                let lease_address = current.and_then(|s| {
                    let addr_val = s
                        .fields
                        .get("addresses")?
                        .value
                        .as_list()?
                        .first()?
                        .as_map()?
                        .get("address")?;
                    Some(addr_val.to_string())
                });
                let timing: Option<LeaseTimingInfo> = factory.lease_timing();
                let (lease_time_secs, lease_remaining_secs) = match timing {
                    Some(info) => {
                        let remaining = (info.lease_time_secs as u64)
                            .saturating_sub(info.acquired_at.elapsed().as_secs());
                        (Some(info.lease_time_secs), Some(remaining))
                    }
                    None => (None, None),
                };
                FactoryStatus {
                    policy_name: name.clone(),
                    factory_type: "dhcpv4".to_string(),
                    interface: factory.interface().to_string(),
                    has_lease,
                    lease_ip,
                    lease_address,
                    lease_time_secs,
                    lease_remaining_secs,
                }
            })
            .collect();

        // IPv6Auto factories: no lease concept — has_lease reflects whether
        // at least one SLAAC address has been configured.
        result.extend(self.ipv6auto_factories.iter().map(|(name, factory)| {
            let current = factory.current_state();
            let has_lease = current.as_ref().is_some_and(|s| {
                s.fields
                    .get("ipv6")
                    .and_then(|fv| fv.value.as_map())
                    .and_then(|m| m.get("addresses"))
                    .and_then(|v| v.as_list())
                    .map(|l| !l.is_empty())
                    .unwrap_or(false)
            });
            FactoryStatus {
                policy_name: name.clone(),
                factory_type: "ipv6auto".to_string(),
                interface: factory.interface().to_string(),
                has_lease,
                lease_ip: None,
                lease_address: None,
                lease_time_secs: None,
                lease_remaining_secs: None,
            }
        }));

        result
    }
}

// ── FactoryManager tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use netfyr_policy::{FactoryType, Policy};
    use netfyr_state::Selector;

    // ── Test helpers ──────────────────────────────────────────────────────────

    /// Build a minimal static policy with a single ethernet state.
    fn make_static_policy(name: &str) -> Policy {
        let yaml = format!(
            "kind: policy\nname: {name}\nfactory: static\npriority: 100\n\
             state:\n  type: ethernet\n  name: eth0\n  mtu: 1500\n"
        );
        netfyr_policy::parse_policy_yaml(&yaml)
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
    }

    /// Build a DHCPv4 policy with NO selector (no interface name).
    fn make_dhcp_policy_no_selector(name: &str) -> Policy {
        Policy {
            name: name.to_string(),
            factory_type: FactoryType::Dhcpv4,
            priority: 100,
            state: None,
            states: None,
            selector: None,
        }
    }

    /// Build a DHCPv4 policy with a named interface selector.
    fn make_dhcp_policy_with_interface(name: &str, interface: &str) -> Policy {
        Policy {
            name: name.to_string(),
            factory_type: FactoryType::Dhcpv4,
            priority: 100,
            state: None,
            states: None,
            selector: Some(Selector::with_name(interface)),
        }
    }

    // ── Feature: FactoryManager initialization ─────────────────────────────────

    /// Scenario: Fresh FactoryManager has no produced states.
    #[tokio::test]
    async fn test_factory_manager_new_produced_states_is_empty() {
        let mgr = FactoryManager::new();
        assert!(
            mgr.produced_states().is_empty(),
            "a newly created FactoryManager must have no produced states"
        );
    }

    /// Scenario: Fresh FactoryManager has no factory statuses.
    #[tokio::test]
    async fn test_factory_manager_new_factory_statuses_is_empty() {
        let mgr = FactoryManager::new();
        assert!(
            mgr.factory_statuses().is_empty(),
            "a newly created FactoryManager must have no factory statuses"
        );
    }

    // ── Feature: Sync with empty policy set ────────────────────────────────────

    /// Scenario: Syncing with no policies succeeds.
    #[tokio::test]
    async fn test_factory_manager_sync_empty_policies_succeeds() {
        let mut mgr = FactoryManager::new();
        let result = mgr.sync(&[]).await;
        assert!(result.is_ok(), "sync with empty policy list must succeed");
    }

    /// Scenario: Syncing with no policies returns no failures.
    #[tokio::test]
    async fn test_factory_manager_sync_empty_policies_returns_no_failures() {
        let mut mgr = FactoryManager::new();
        let failed = mgr.sync(&[]).await.unwrap();
        assert!(
            failed.is_empty(),
            "sync with empty policies must not report any failures"
        );
    }

    // ── Feature: Sync with static-only policies ────────────────────────────────

    /// Scenario: Static policies do not start DHCP factories.
    #[tokio::test]
    async fn test_factory_manager_sync_static_policies_starts_no_dhcp_factories() {
        let mut mgr = FactoryManager::new();
        let policies = vec![make_static_policy("eth0-policy")];
        mgr.sync(&policies).await.unwrap();
        assert!(
            mgr.factory_statuses().is_empty(),
            "static policies must not create DHCP factories"
        );
    }

    /// Scenario: Static policies return no factory failures.
    #[tokio::test]
    async fn test_factory_manager_sync_static_policies_returns_no_failures() {
        let mut mgr = FactoryManager::new();
        let policies = vec![make_static_policy("eth0"), make_static_policy("eth1")];
        let failed = mgr.sync(&policies).await.unwrap();
        assert!(
            failed.is_empty(),
            "static-only policies must not cause factory failures"
        );
    }

    /// Scenario: Static policies leave produced_states empty (no factories running).
    #[tokio::test]
    async fn test_factory_manager_sync_static_policies_produced_states_remains_empty() {
        let mut mgr = FactoryManager::new();
        let policies = vec![make_static_policy("eth0")];
        mgr.sync(&policies).await.unwrap();
        assert!(
            mgr.produced_states().is_empty(),
            "no DHCP factories → no produced states"
        );
    }

    // ── Feature: Submit policies stops removed DHCP factories ─────────────────
    // (tested via sync: remove all policies after static-only set)

    /// Scenario: Syncing from static-only to empty set leaves no factories.
    #[tokio::test]
    async fn test_factory_manager_sync_to_empty_from_static_leaves_no_factories() {
        let mut mgr = FactoryManager::new();
        let policies = vec![make_static_policy("eth0")];
        mgr.sync(&policies).await.unwrap();
        // Remove all policies
        let failed = mgr.sync(&[]).await.unwrap();
        assert!(
            failed.is_empty(),
            "sync to empty from static-only should not report failures"
        );
        assert!(
            mgr.factory_statuses().is_empty(),
            "no factories should be running after syncing to empty set"
        );
    }

    // ── Feature: Sync is idempotent ────────────────────────────────────────────

    /// Scenario: Calling sync twice with the same policies is idempotent.
    #[tokio::test]
    async fn test_factory_manager_sync_idempotent_for_static_policies() {
        let mut mgr = FactoryManager::new();
        let policies = vec![make_static_policy("eth0")];
        mgr.sync(&policies).await.unwrap();
        mgr.sync(&policies).await.unwrap();
        assert!(
            mgr.factory_statuses().is_empty(),
            "repeated sync with static policies must remain factory-free"
        );
    }

    // ── Feature: DHCPv4 policy without interface selector ─────────────────────

    /// Scenario: Submit DHCPv4 policy with no interface — reported in failed list.
    #[tokio::test]
    async fn test_factory_manager_sync_dhcpv4_without_selector_reports_in_failed_list() {
        let mut mgr = FactoryManager::new();
        let policies = vec![make_dhcp_policy_no_selector("dhcp-no-iface")];
        let failed = mgr.sync(&policies).await.unwrap();
        assert!(
            failed.contains(&"dhcp-no-iface".to_string()),
            "DHCPv4 policy with no selector must appear in the failed list"
        );
    }

    /// Scenario: DHCPv4 policy without selector does not start a factory.
    #[tokio::test]
    async fn test_factory_manager_sync_dhcpv4_without_selector_starts_no_factory() {
        let mut mgr = FactoryManager::new();
        let policies = vec![make_dhcp_policy_no_selector("dhcp-no-iface")];
        mgr.sync(&policies).await.unwrap();
        assert!(
            mgr.factory_statuses().is_empty(),
            "DHCPv4 policy without selector must not start a factory"
        );
    }

    // ── Feature: DHCPv4 policy with nonexistent interface fails gracefully ─────

    /// Scenario: Submit DHCPv4 policy for nonexistent interface — fails gracefully.
    #[tokio::test]
    async fn test_factory_manager_sync_dhcpv4_nonexistent_interface_fails_gracefully() {
        let mut mgr = FactoryManager::new();
        // Use a highly unlikely interface name.
        let policies = vec![make_dhcp_policy_with_interface(
            "dhcp-eth99999",
            "eth99999-nonexistent-iface",
        )];
        let result = mgr.sync(&policies).await;
        // sync() itself must succeed (Result level) even when the factory cannot start.
        assert!(
            result.is_ok(),
            "sync must succeed at the Result level even when a factory fails to start"
        );
        let failed = result.unwrap();
        assert!(
            failed.contains(&"dhcp-eth99999".to_string()),
            "policy whose factory failed to start must be in the failed list"
        );
    }

    /// After a DHCPv4 factory fails to start, produced_states() and
    /// factory_statuses() must both be empty — no phantom factory state leaks
    /// into the reconciliation pipeline.
    #[tokio::test]
    async fn test_produced_states_and_statuses_empty_after_dhcp_factory_start_fails() {
        let mut mgr = FactoryManager::new();
        let policies = vec![make_dhcp_policy_with_interface(
            "dhcp-eth99999",
            "eth99999-nonexistent-iface",
        )];
        // sync() succeeds at the Result level but reports the factory in the failed list.
        let _ = mgr.sync(&policies).await;
        assert!(
            mgr.produced_states().is_empty(),
            "produced_states must be empty when the DHCPv4 factory failed to start"
        );
        assert!(
            mgr.factory_statuses().is_empty(),
            "factory_statuses must be empty when the DHCPv4 factory failed to start"
        );
    }

    // ── Feature: Stop all factories ────────────────────────────────────────────

    /// Scenario: stop_all on empty FactoryManager succeeds.
    #[tokio::test]
    async fn test_factory_manager_stop_all_when_empty_succeeds() {
        let mut mgr = FactoryManager::new();
        let result = mgr.stop_all().await;
        assert!(result.is_ok(), "stop_all on empty manager must succeed");
    }

    /// Scenario: stop_all leaves no running factories.
    #[tokio::test]
    async fn test_factory_manager_stop_all_leaves_no_running_factories() {
        let mut mgr = FactoryManager::new();
        mgr.stop_all().await.unwrap();
        assert!(
            mgr.factory_statuses().is_empty(),
            "factory_statuses must be empty after stop_all"
        );
    }

    // ── Feature: Mixed static and DHCP (without selector) ─────────────────────

    /// Scenario: Mix of static and DHCP-without-selector — no factories run.
    #[tokio::test]
    async fn test_factory_manager_sync_mix_static_and_dhcp_without_selector() {
        let mut mgr = FactoryManager::new();
        let policies = vec![
            make_static_policy("eth0"),
            make_dhcp_policy_no_selector("dhcp-no-iface"),
        ];
        mgr.sync(&policies).await.unwrap();
        // Static doesn't create factories; DHCP without selector is skipped.
        assert!(
            mgr.factory_statuses().is_empty(),
            "no DHCP factories should run: static never creates factories, \
             DHCP without selector is skipped"
        );
        assert!(
            mgr.produced_states().is_empty(),
            "no factories → no produced states"
        );
    }

    // ── Feature: SPEC-407 — DHCPv4 lease expiry handling ─────────────────────

    /// Scenario: "the produced state reverts to the pending state with only enabled: true"
    ///
    /// When a factory is in pending state (before initial lease or after
    /// LeaseExpired), produced_states() must include a state with only
    /// enabled=true — no addresses, routes, or dns_servers. This lets the
    /// reconciler compute a diff that removes any previously applied address.
    #[tokio::test]
    async fn test_produced_states_includes_pending_state_with_no_addresses() {
        let mut mgr = FactoryManager::new();
        // Loopback always exists on Linux; DHCP discovery never succeeds on it.
        // The factory enters pending state immediately (enabled=true, no addresses).
        let policies = vec![make_dhcp_policy_with_interface("dhcp-lo-spec407a", "lo")];
        mgr.sync(&policies).await.unwrap();

        let states = mgr.produced_states();
        assert!(
            !states.is_empty(),
            "produced_states must include the running factory's state even in pending mode"
        );

        let (_, state) = states
            .into_iter()
            .find(|(name, _)| name == "dhcp-lo-spec407a")
            .expect("pending state for dhcp-lo-spec407a must be in produced_states");

        assert_eq!(
            state.fields.get("enabled").and_then(|fv| fv.value.as_bool()),
            Some(true),
            "pending state must have enabled=true so the interface stays UP during re-discovery"
        );
        assert!(
            state.fields.get("addresses").is_none(),
            "pending state must not have 'addresses' — reconciler will Remove any existing address"
        );
        assert!(
            state.fields.get("routes").is_none(),
            "pending state must not have 'routes' — reconciler will Remove the expired gateway route"
        );

        mgr.stop_all().await.unwrap();
    }

    /// Scenario: GetStatus shows has_lease=false when factory is in pending state.
    ///
    /// After LeaseExpired, the factory's current_state() contains only
    /// enabled=true (no "addresses" field). factory_statuses() must reflect
    /// this as has_lease=false, communicating that no lease is currently held.
    #[tokio::test]
    async fn test_factory_statuses_reports_has_lease_false_in_pending_state() {
        let mut mgr = FactoryManager::new();
        let policies = vec![make_dhcp_policy_with_interface("dhcp-lo-spec407b", "lo")];
        mgr.sync(&policies).await.unwrap();

        let statuses = mgr.factory_statuses();
        assert!(!statuses.is_empty(), "factory_statuses must include the running factory");

        let status = statuses
            .into_iter()
            .find(|s| s.interface == "lo")
            .expect("factory status for the 'lo' interface must be present");

        assert!(
            !status.has_lease,
            "factory in pending state (after LeaseExpired or before initial lease) \
             must report has_lease=false — the 'addresses' field is absent"
        );
        assert!(
            status.lease_ip.is_none(),
            "factory in pending state must have no lease_ip"
        );
        assert!(
            status.lease_address.is_none(),
            "factory in pending state must have no lease_address"
        );
        assert!(
            status.lease_time_secs.is_none(),
            "factory in pending state must have no lease_time_secs"
        );

        mgr.stop_all().await.unwrap();
    }
}
