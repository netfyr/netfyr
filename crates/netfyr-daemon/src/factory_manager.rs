//! DHCP factory lifecycle manager.
//!
//! `FactoryManager` owns the set of running `Dhcpv4Factory` instances and
//! maintains a single `mpsc` channel through which all factory events flow
//! back to the daemon event loop. The `sync` method provides idempotent
//! convergence: call it with the current policy set after any `SubmitPolicies`
//! request to start new factories and stop removed ones.

use std::collections::HashMap;

use netfyr_backend::{Dhcpv4Factory, FactoryEvent, LeaseTimingInfo};
use netfyr_policy::{FactoryType, Policy};
use netfyr_state::State;
use tokio::sync::mpsc;
use tracing::{error, warn};

// ── FactoryStatus ─────────────────────────────────────────────────────────────

/// Status of a single running factory, used for `GetStatus` responses.
pub struct FactoryStatus {
    pub policy_name: String,
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

/// Manages the lifecycle of all DHCPv4 factories.
///
/// Factories are keyed by policy name. A single `mpsc` channel aggregates
/// events from all factories into the daemon's event loop via `next_event`.
pub struct FactoryManager {
    /// Running factories, keyed by policy name.
    factories: HashMap<String, Dhcpv4Factory>,
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
        // Build the desired set: policy_name → Policy for all DHCPv4 policies.
        let desired: HashMap<String, &Policy> = policies
            .iter()
            .filter(|p| p.factory_type == FactoryType::Dhcpv4)
            .map(|p| (p.name.clone(), p))
            .collect();

        // ── Stop removed factories ────────────────────────────────────────────
        let to_stop: Vec<String> = self
            .factories
            .keys()
            .filter(|name| !desired.contains_key(*name))
            .cloned()
            .collect();

        for name in to_stop {
            if let Some(mut factory) = self.factories.remove(&name) {
                if let Err(e) = factory.stop().await {
                    warn!(policy = %name, error = %e, "Failed to stop DHCP factory");
                }
            }
        }

        // ── Start new factories ───────────────────────────────────────────────
        let mut failed = Vec::new();

        for (name, policy) in &desired {
            if self.factories.contains_key(name.as_str()) {
                // Already running — leave it alone.
                continue;
            }

            // Extract the interface name from the selector's name field.
            let interface = match policy
                .selector
                .as_ref()
                .and_then(|s| s.name.as_deref())
            {
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

            // Validate that the interface exists before spawning the factory.
            // Dhcpv4Factory::start() always succeeds (it spawns a background
            // task), so we must pre-check here to give the caller a synchronous
            // error signal for nonexistent interfaces.
            // Use rtnetlink (via netfyr_backend::interface_exists) rather than
            // /sys/class/net/ because sysfs is not network-namespace-aware in
            // all environments (e.g., containers, unshare --user --net).
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

        Ok(failed)
    }

    /// Returns the current lease state produced by all active factories.
    ///
    /// Factories that have not yet acquired a lease (i.e., `current_state()`
    /// returns `None`) are omitted.
    pub fn produced_states(&self) -> Vec<(String, State)> {
        self.factories
            .iter()
            .filter_map(|(name, factory)| {
                factory.current_state().map(|state| (name.clone(), state))
            })
            .collect()
    }

    /// Stops all running factories. Called on daemon shutdown.
    ///
    /// Individual stop failures are logged but do not abort the loop.
    pub async fn stop_all(&mut self) -> anyhow::Result<()> {
        let names: Vec<String> = self.factories.keys().cloned().collect();
        for name in names {
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
        Ok(())
    }

    /// Receive the next factory event (lease acquired, renewed, expired, or
    /// error). Returns `None` when all senders have been dropped (shutdown).
    pub async fn next_event(&mut self) -> Option<FactoryEvent> {
        self.event_rx.recv().await
    }

    /// Returns status information for each running factory, for `GetStatus`.
    pub fn factory_statuses(&self) -> Vec<FactoryStatus> {
        self.factories
            .iter()
            .map(|(name, factory)| {
                let current = factory.current_state();
                // With pending state, current_state() returns Some(State) even before a
                // lease is acquired (containing only operstate:up). Check for the
                // "addresses" field to distinguish a real lease from a pending state.
                let has_lease = current
                    .as_ref()
                    .is_some_and(|s| s.fields.contains_key("addresses"));
                // Extract the bare IP (without /prefix) from the "addresses" field.
                let lease_ip = current.as_ref().and_then(|s| {
                    let addr_list = s.fields.get("addresses")?.value.as_list()?.first()?.as_str()?;
                    // "10.0.1.50/24" → "10.0.1.50"
                    Some(addr_list.split('/').next().unwrap_or(addr_list).to_string())
                });
                // Extract full CIDR address from the "addresses" field.
                let lease_address = current.and_then(|s| {
                    let addr_str = s.fields.get("addresses")?.value.as_list()?.first()?.as_str()?;
                    Some(addr_str.to_string())
                });
                // Read lease timing and compute remaining seconds.
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
                    interface: factory.interface().to_string(),
                    has_lease,
                    lease_ip,
                    lease_address,
                    lease_time_secs,
                    lease_remaining_secs,
                }
            })
            .collect()
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
}
