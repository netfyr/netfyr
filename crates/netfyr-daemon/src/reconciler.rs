//! Reconciliation engine wrapper for the daemon.
//!
//! `Reconciler` is stateless except for the `BackendRegistry` and
//! `SchemaRegistry` it holds at construction time. It can be called from both
//! the Varlink request handler and the factory event handler without locking.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use netfyr_backend::{ApplyReport, BackendRegistry, NetlinkBackend};
use netfyr_journal::{
    summarize_policies, ApplyOutcome, Journal, JournalEntry, SequenceId, SerializableDiff,
    SerializableDiffOp, SerializableFieldChange, SerializableState, SerializableStateSet, Trigger,
};
use netfyr_policy::{FactoryType, Policy, StaticFactory, StateFactory};
use netfyr_reconcile::{
    generate_diff, merge, ConflictReport, EntityKey, PolicyId, PolicyInput,
    StateDiff as ReconcileStateDiff,
};
use netfyr_state::{SchemaRegistry, Selector, StateSet};

use crate::factory_manager::FactoryManager;
use crate::policy_store::PolicyStore;

// ── ApplyResult ───────────────────────────────────────────────────────────────

/// The result of a full reconciliation and apply cycle.
pub struct ApplyResult {
    pub report: ApplyReport,
    pub conflicts: ConflictReport,
}

// ── RevertResult ──────────────────────────────────────────────────────────────

/// The result of a revert operation.
pub struct RevertResult {
    /// Rich diff for display and journal recording.
    pub reconcile_diff: ReconcileStateDiff,
    /// Apply report; `None` if this was a dry-run.
    pub report: Option<ApplyReport>,
}

// ── Reconciler ────────────────────────────────────────────────────────────────

/// Coordinates reconciliation: merges policy inputs, diffs against actual
/// system state, and applies changes via the backend registry.
pub struct Reconciler {
    backend_registry: BackendRegistry,
    schema_registry: SchemaRegistry,
    journal: Mutex<Option<Journal>>,
    /// Set to `true` while `reconcile_and_apply` is running so the netlink
    /// monitor can suppress self-generated change notifications.
    is_applying: Arc<AtomicBool>,
}

impl Reconciler {
    /// Create a `Reconciler` with the standard backend and schema registries.
    pub fn new() -> Self {
        let mut registry = BackendRegistry::new();
        let netlink = Arc::new(NetlinkBackend::new());
        if let Err(e) = registry.register(netlink) {
            tracing::error!("Failed to register NetlinkBackend: {}", e);
        }

        let journal = match Journal::open_default() {
            Ok(j) => {
                tracing::debug!("Journal opened successfully");
                Some(j)
            }
            Err(e) => {
                tracing::warn!("Failed to open journal (journal writes disabled): {}", e);
                None
            }
        };

        Self {
            backend_registry: registry,
            schema_registry: SchemaRegistry::default(),
            journal: Mutex::new(journal),
            is_applying: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Signal that a reconcile-and-apply cycle is in progress.
    ///
    /// The netlink monitor checks this flag to discard self-generated events.
    pub fn set_applying(&self, applying: bool) {
        self.is_applying.store(applying, Ordering::SeqCst);
    }

    /// Returns `true` while a reconcile-and-apply cycle is in progress.
    pub fn is_applying(&self) -> bool {
        self.is_applying.load(Ordering::SeqCst)
    }

    /// Return the set of entity selector keys (interface names) covered by the
    /// current effective policy set. Used by the netlink monitor to filter
    /// events for unmanaged interfaces.
    pub fn managed_entity_names(
        &self,
        policy_store: &PolicyStore,
        factory_manager: &FactoryManager,
    ) -> HashSet<String> {
        self.build_policy_inputs(policy_store, factory_manager)
            .into_iter()
            .flat_map(|input| {
                input
                    .state_set
                    .entities()
                    .into_iter()
                    .map(|(_entity_type, selector_key)| selector_key)
            })
            .collect()
    }

    /// Query the backend for each named entity, compare against the last known
    /// journal snapshot, and append an `ExternalChange` journal entry if any
    /// fields actually differ.
    ///
    /// Returns immediately (no-op) when the journal is unavailable.
    pub async fn record_external_change(
        &self,
        changed_entity_names: Vec<String>,
        policy_store: &PolicyStore,
    ) -> anyhow::Result<()> {
        // Query current state for each entity before locking the journal.
        // The backend queries are async and must not hold the journal mutex.
        let mut current_states: std::collections::HashMap<String, netfyr_state::State> =
            std::collections::HashMap::new();

        for entity_name in &changed_entity_names {
            let selector = Selector::with_name(entity_name);
            match self
                .backend_registry
                .query(&"ethernet".to_string(), Some(&selector))
                .await
            {
                Ok(state_set) => {
                    if let Some(state) = state_set.get("ethernet", entity_name) {
                        current_states.insert(entity_name.clone(), state.clone());
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        entity = %entity_name,
                        error = %e,
                        "Failed to query current state for external change detection"
                    );
                }
            }
        }

        if current_states.is_empty() {
            return Ok(());
        }

        // Lock journal and compute per-entity diffs.
        let mut guard = match self.journal.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("Journal mutex poisoned: {}", e);
                return Ok(());
            }
        };

        let journal = match guard.as_mut() {
            Some(j) => j,
            None => return Ok(()),
        };

        let mut diff_ops: Vec<SerializableDiffOp> = Vec::new();
        let mut state_after_entities: Vec<SerializableState> = Vec::new();
        let mut changed: Vec<String> = Vec::new();

        for (entity_name, current_state) in &current_states {
            let last_state = match journal.latest_state_for(entity_name) {
                Ok(Some(s)) => s,
                Ok(None) => continue, // No prior snapshot; entity may be unmanaged
                Err(e) => {
                    tracing::warn!(
                        entity = %entity_name,
                        error = %e,
                        "Failed to read journal snapshot for external change"
                    );
                    continue;
                }
            };

            let field_changes = compute_external_field_changes(&last_state, current_state);
            if field_changes.is_empty() {
                continue; // State matches journal snapshot; spurious event
            }

            // Build state_after entry for this entity.
            let mut obj = serde_json::Map::new();
            for (k, fv) in &current_state.fields {
                let json_val =
                    serde_json::to_value(&fv.value).unwrap_or(serde_json::Value::Null);
                obj.insert(k.clone(), json_val);
            }
            state_after_entities.push(SerializableState {
                entity_type: "ethernet".to_string(),
                selector_name: entity_name.clone(),
                fields: serde_json::Value::Object(obj),
            });

            diff_ops.push(SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: entity_name.clone(),
                field_changes,
            });

            changed.push(entity_name.clone());
        }

        if changed.is_empty() {
            return Ok(());
        }

        let entry = JournalEntry {
            seq: 0,
            timestamp: chrono::Utc::now(),
            trigger: Trigger::ExternalChange {
                changed_entities: changed.clone(),
            },
            active_policies: summarize_policies(policy_store.policies()),
            diff: SerializableDiff { operations: diff_ops },
            state_after: SerializableStateSet {
                entities: state_after_entities,
            },
            outcome: ApplyOutcome::Observed,
        };

        if let Err(e) = journal.append(entry) {
            tracing::warn!(
                error = %e,
                "Failed to write external change journal entry"
            );
        } else {
            tracing::info!(
                entities = %changed.join(", "),
                "External change detected"
            );
        }

        Ok(())
    }

    /// Run full reconciliation and apply the resulting diff to the system.
    ///
    /// Steps:
    /// 1. Build `PolicyInput` list from static policies and factory states.
    /// 2. Run `merge()` to produce the effective desired state.
    /// 3. Query actual system state via the backend registry.
    /// 4. Compute rich diff for journal and lean `netfyr_state::StateDiff` for apply.
    /// 5. If the diff is empty, write a journal entry and return an empty report.
    /// 6. Apply the diff; write journal entry; return the report and any conflicts.
    pub async fn reconcile_and_apply(
        &self,
        policy_store: &PolicyStore,
        factory_manager: &FactoryManager,
        trigger: Trigger,
    ) -> Result<ApplyResult> {
        let inputs = self.build_policy_inputs(policy_store, factory_manager);

        // Compute managed_entities before merge() consumes the inputs.
        let managed_entities: HashSet<EntityKey> = inputs
            .iter()
            .flat_map(|input| input.state_set.entities())
            .collect();

        let merged = merge(inputs);
        let effective_state = merged.effective_state;
        let conflicts = merged.conflicts;

        let actual_state = self.backend_registry.query_all().await?;

        // Compute the rich diff for journal recording.
        let reconcile_diff = generate_diff(
            &effective_state,
            &actual_state,
            &managed_entities,
            &self.schema_registry,
        );

        // Restrict the actual state to only the entities present in the effective
        // desired state before computing the diff. This prevents the daemon from
        // generating Remove operations for interfaces not covered by any policy.
        let mut managed_actual = StateSet::new();
        for (entity_type, selector_key) in effective_state.entities() {
            if let Some(state) = actual_state.get(&entity_type, &selector_key) {
                managed_actual.insert(state.clone());
            }
        }

        let state_diff = netfyr_state::diff::diff(&managed_actual, &effective_state);

        if state_diff.is_empty() {
            tracing::debug!("Reconciliation: no changes needed");
            self.append_journal_entry(
                policy_store,
                &trigger,
                &reconcile_diff,
                &effective_state,
                ApplyOutcome::Applied {
                    succeeded: 0,
                    failed: 0,
                    skipped: 0,
                },
            );
            return Ok(ApplyResult {
                report: ApplyReport::new(),
                conflicts,
            });
        }

        let report = self.backend_registry.apply(&state_diff).await?;

        self.append_journal_entry(
            policy_store,
            &trigger,
            &reconcile_diff,
            &effective_state,
            ApplyOutcome::Applied {
                succeeded: report.succeeded.len() as u32,
                failed: report.failed.len() as u32,
                skipped: report.skipped.len() as u32,
            },
        );

        Ok(ApplyResult { report, conflicts })
    }

    /// Compute what changes *would* be made without applying them.
    ///
    /// Returns the rich `netfyr_reconcile::StateDiff` (with per-field old→new
    /// values) suitable for Varlink serialization, along with any conflicts.
    pub async fn dry_run(
        &self,
        policy_store: &PolicyStore,
        factory_manager: &FactoryManager,
    ) -> Result<(ReconcileStateDiff, ConflictReport)> {
        let inputs = self.build_policy_inputs(policy_store, factory_manager);
        // Compute managed_entities before merge() consumes the inputs.
        let managed_entities: HashSet<EntityKey> = inputs
            .iter()
            .flat_map(|input| input.state_set.entities())
            .collect();
        let merged = merge(inputs);
        let effective_state = merged.effective_state;
        let conflicts = merged.conflicts;

        let actual_state = self.backend_registry.query_all().await?;

        let reconcile_diff =
            generate_diff(&effective_state, &actual_state, &managed_entities, &self.schema_registry);

        Ok((reconcile_diff, conflicts))
    }

    /// Revert the system to match a historical journal snapshot.
    ///
    /// Computes the diff from the current system state to `target_state`, then
    /// applies it (unless `dry_run` is true). Records a journal entry on apply.
    pub async fn revert(
        &self,
        target_state: &StateSet,
        target_seq: SequenceId,
        policies: &[Policy],
        dry_run: bool,
    ) -> Result<RevertResult> {
        let actual_state = self.backend_registry.query_all().await?;

        let managed_entities: HashSet<EntityKey> = target_state.entities().into_iter().collect();

        let reconcile_diff = generate_diff(
            target_state,
            &actual_state,
            &managed_entities,
            &self.schema_registry,
        );

        // Restrict actual state to only entities present in the target snapshot.
        let mut managed_actual = StateSet::new();
        for (entity_type, selector_key) in target_state.entities() {
            if let Some(state) = actual_state.get(&entity_type, &selector_key) {
                managed_actual.insert(state.clone());
            }
        }

        let state_diff = netfyr_state::diff::diff(&managed_actual, target_state);

        if dry_run {
            return Ok(RevertResult {
                reconcile_diff,
                report: None,
            });
        }

        if state_diff.is_empty() {
            self.append_revert_journal_entry(
                policies,
                target_seq,
                &reconcile_diff,
                target_state,
                ApplyOutcome::Applied { succeeded: 0, failed: 0, skipped: 0 },
            );
            return Ok(RevertResult {
                reconcile_diff,
                report: Some(ApplyReport::new()),
            });
        }

        self.set_applying(true);
        let apply_report = match self.backend_registry.apply(&state_diff).await {
            Ok(r) => r,
            Err(e) => {
                self.set_applying(false);
                return Err(e.into());
            }
        };
        self.set_applying(false);

        self.append_revert_journal_entry(
            policies,
            target_seq,
            &reconcile_diff,
            target_state,
            ApplyOutcome::Applied {
                succeeded: apply_report.succeeded.len() as u32,
                failed: apply_report.failed.len() as u32,
                skipped: apply_report.skipped.len() as u32,
            },
        );

        Ok(RevertResult {
            reconcile_diff,
            report: Some(apply_report),
        })
    }

    /// Query current system state via the backend registry.
    pub async fn query(
        &self,
        entity_type: Option<&str>,
        selector: Option<&Selector>,
    ) -> Result<StateSet> {
        if let Some(et) = entity_type {
            let state_set = self
                .backend_registry
                .query(&et.to_string(), selector)
                .await?;
            Ok(state_set)
        } else {
            let state_set = self.backend_registry.query_all().await?;
            Ok(state_set)
        }
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn append_journal_entry(
        &self,
        policy_store: &PolicyStore,
        trigger: &Trigger,
        reconcile_diff: &ReconcileStateDiff,
        effective_state: &StateSet,
        outcome: ApplyOutcome,
    ) {
        let mut guard = match self.journal.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("Journal mutex poisoned: {}", e);
                return;
            }
        };
        if let Some(ref mut journal) = *guard {
            let entry = JournalEntry {
                seq: 0,
                timestamp: chrono::Utc::now(),
                trigger: trigger.clone(),
                active_policies: summarize_policies(policy_store.policies()),
                diff: SerializableDiff::from(reconcile_diff),
                state_after: SerializableStateSet::from(effective_state),
                outcome,
            };
            if let Err(e) = journal.append(entry) {
                tracing::warn!("Failed to write journal entry: {}", e);
            }
        }
    }

    fn append_revert_journal_entry(
        &self,
        policies: &[Policy],
        target_seq: SequenceId,
        reconcile_diff: &ReconcileStateDiff,
        target_state: &StateSet,
        outcome: ApplyOutcome,
    ) {
        let mut guard = match self.journal.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("Journal mutex poisoned: {}", e);
                return;
            }
        };
        if let Some(ref mut journal) = *guard {
            let entry = JournalEntry {
                seq: 0,
                timestamp: chrono::Utc::now(),
                trigger: Trigger::Revert { target_seq },
                active_policies: summarize_policies(policies),
                diff: SerializableDiff::from(reconcile_diff),
                state_after: SerializableStateSet::from(target_state),
                outcome,
            };
            if let Err(e) = journal.append(entry) {
                tracing::warn!("Failed to write revert journal entry: {}", e);
            }
        }
    }

    /// Build the `Vec<PolicyInput>` fed into `merge()`.
    fn build_policy_inputs(
        &self,
        policy_store: &PolicyStore,
        factory_manager: &FactoryManager,
    ) -> Vec<PolicyInput> {
        let static_factory = StaticFactory;
        let mut inputs = Vec::new();

        // Static policies
        for policy in policy_store.policies() {
            if policy.factory_type != FactoryType::Static {
                continue;
            }
            match static_factory.produce(policy) {
                Ok(state_set) => {
                    inputs.push(PolicyInput {
                        policy_id: PolicyId(policy.name.clone()),
                        priority: policy.priority,
                        state_set,
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        policy = %policy.name,
                        error = %e,
                        "Failed to produce state from static policy; skipping"
                    );
                }
            }
        }

        // Factory-produced states (DHCPv4 leases)
        for (policy_name, state) in factory_manager.produced_states() {
            let priority = policy_store
                .policies()
                .iter()
                .find(|p| p.name == policy_name)
                .map(|p| p.priority)
                .unwrap_or(100);

            let mut state_set = StateSet::new();
            state_set.insert(state);

            inputs.push(PolicyInput {
                policy_id: PolicyId(policy_name),
                priority,
                state_set,
            });
        }

        inputs
    }
}

// ── External diff helpers ─────────────────────────────────────────────────────

/// Compare a journal snapshot against the current queried state and produce a
/// list of field-level changes. Fields that match are omitted; fields that
/// differ, were added, or were removed each produce one entry.
fn compute_external_field_changes(
    last: &SerializableState,
    current: &netfyr_state::State,
) -> Vec<SerializableFieldChange> {
    let mut changes = Vec::new();

    // Fields present in the current (live) state.
    for (field_name, fv) in &current.fields {
        let current_json = serde_json::to_value(&fv.value).unwrap_or(serde_json::Value::Null);
        match last.fields.get(field_name) {
            Some(last_val) if last_val == &current_json => {
                // Unchanged — skip.
            }
            Some(last_val) => {
                changes.push(SerializableFieldChange {
                    field_name: field_name.clone(),
                    change_kind: "set".to_string(),
                    current: Some(last_val.clone()), // old value (from journal)
                    desired: Some(current_json),     // new value (from system)
                });
            }
            None => {
                // Field appeared since the last snapshot.
                changes.push(SerializableFieldChange {
                    field_name: field_name.clone(),
                    change_kind: "set".to_string(),
                    current: None,
                    desired: Some(current_json),
                });
            }
        }
    }

    // Fields present in the last snapshot but absent from the current state.
    if let Some(last_obj) = last.fields.as_object() {
        for (field_name, last_val) in last_obj {
            if !current.fields.contains_key(field_name) {
                changes.push(SerializableFieldChange {
                    field_name: field_name.clone(),
                    change_kind: "unset".to_string(),
                    current: Some(last_val.clone()),
                    desired: None,
                });
            }
        }
    }

    changes
}

// ── Reconciler tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory_manager::FactoryManager;
    use crate::policy_store::PolicyStore;

    // ── Feature: Reconciler initialization ────────────────────────────────────

    /// Smoke test: Reconciler::new() must not panic.
    #[test]
    fn test_reconciler_new_does_not_panic() {
        let _reconciler = Reconciler::new();
    }

    // ── Feature: Dry-run with empty policy store ───────────────────────────────

    /// Scenario: Dry-run computes diff without applying — empty store returns Ok.
    #[tokio::test]
    async fn test_reconciler_dry_run_with_empty_ephemeral_store_returns_ok() {
        let reconciler = Reconciler::new();
        let store = PolicyStore::ephemeral(vec![]);
        let factory_manager = FactoryManager::new();
        let result = reconciler.dry_run(&store, &factory_manager).await;
        assert!(
            result.is_ok(),
            "dry_run with empty store must succeed: {:?}",
            result.err()
        );
    }

    /// Scenario: Dry-run with empty store produces no conflicts.
    #[tokio::test]
    async fn test_reconciler_dry_run_with_empty_store_produces_no_conflicts() {
        let reconciler = Reconciler::new();
        let store = PolicyStore::ephemeral(vec![]);
        let factory_manager = FactoryManager::new();
        let (_, conflicts) = reconciler
            .dry_run(&store, &factory_manager)
            .await
            .unwrap();
        assert!(
            conflicts.is_empty(),
            "empty policy store must produce no conflicts"
        );
    }

    /// Scenario: Dry-run does not modify system state (result is not applied).
    /// We verify this by running dry_run twice and getting identical results.
    #[tokio::test]
    async fn test_reconciler_dry_run_is_repeatable() {
        let reconciler = Reconciler::new();
        let store = PolicyStore::ephemeral(vec![]);
        let factory_manager = FactoryManager::new();

        let (diff1, _) = reconciler.dry_run(&store, &factory_manager).await.unwrap();
        let (diff2, _) = reconciler.dry_run(&store, &factory_manager).await.unwrap();
        assert_eq!(
            diff1.len(),
            diff2.len(),
            "dry_run must not alter system state: both runs must produce the same diff length"
        );
    }

    // ── Feature: Query via daemon ──────────────────────────────────────────────

    /// Scenario: Query returns current system state — query with no filter succeeds.
    #[tokio::test]
    async fn test_reconciler_query_all_returns_ok() {
        let reconciler = Reconciler::new();
        let result = reconciler.query(None, None).await;
        assert!(
            result.is_ok(),
            "query with no entity type filter must succeed: {:?}",
            result.err()
        );
    }

    /// Scenario: Query returns a StateSet (possibly empty, possibly with interfaces).
    #[tokio::test]
    async fn test_reconciler_query_returns_state_set() {
        let reconciler = Reconciler::new();
        let state_set = reconciler.query(None, None).await.unwrap();
        let _len = state_set.len();
    }

    // ── Feature: Full reconcile_and_apply ─────────────────────────────────────

    /// Scenario: reconcile_and_apply with empty store is a no-op and returns Ok.
    #[tokio::test]
    async fn test_reconciler_reconcile_and_apply_empty_store_returns_ok() {
        let reconciler = Reconciler::new();
        let store = PolicyStore::ephemeral(vec![]);
        let factory_manager = FactoryManager::new();
        let result = reconciler
            .reconcile_and_apply(&store, &factory_manager, Trigger::DaemonStartup)
            .await;
        assert!(
            result.is_ok(),
            "reconcile_and_apply with empty store must succeed: {:?}",
            result.err()
        );
    }

    /// Scenario: reconcile_and_apply with empty store produces no conflicts.
    #[tokio::test]
    async fn test_reconciler_reconcile_and_apply_empty_store_no_conflicts() {
        let reconciler = Reconciler::new();
        let store = PolicyStore::ephemeral(vec![]);
        let factory_manager = FactoryManager::new();
        let apply_result = reconciler
            .reconcile_and_apply(&store, &factory_manager, Trigger::DaemonStartup)
            .await
            .unwrap();
        assert!(
            apply_result.conflicts.is_empty(),
            "empty policy store must produce no conflicts during reconcile_and_apply"
        );
    }

    /// Scenario: reconcile_and_apply with empty store produces a successful report.
    #[tokio::test]
    async fn test_reconciler_reconcile_and_apply_empty_store_report_has_no_failures() {
        let reconciler = Reconciler::new();
        let store = PolicyStore::ephemeral(vec![]);
        let factory_manager = FactoryManager::new();
        let apply_result = reconciler
            .reconcile_and_apply(&store, &factory_manager, Trigger::DaemonStartup)
            .await
            .unwrap();
        assert!(
            apply_result.report.is_success(),
            "empty policy store must produce a successful (no-failure) apply report"
        );
    }

    // ── Feature: Dry-run with policy ──────────────────────────────────────────

    /// Scenario: Dry-run with a static policy for a nonexistent interface returns Ok.
    #[tokio::test]
    async fn test_reconciler_dry_run_with_static_policy_returns_ok() {
        use netfyr_policy::parse_policy_yaml;
        let reconciler = Reconciler::new();
        let yaml = "kind: policy\nname: test\nfactory: static\npriority: 100\n\
                    state:\n  type: ethernet\n  name: nonexistent-eth99\n  mtu: 1400\n";
        let policies = parse_policy_yaml(yaml).unwrap();
        let store = PolicyStore::ephemeral(policies);
        let factory_manager = FactoryManager::new();
        let result = reconciler.dry_run(&store, &factory_manager).await;
        assert!(
            result.is_ok(),
            "dry_run with a static policy must succeed: {:?}",
            result.err()
        );
    }

    /// Scenario: Dry-run does not alter state.
    #[tokio::test]
    async fn test_reconciler_dry_run_does_not_alter_system_state() {
        let reconciler = Reconciler::new();
        let store = PolicyStore::ephemeral(vec![]);
        let factory_manager = FactoryManager::new();

        let before = reconciler.query(None, None).await.unwrap();
        let _ = reconciler.dry_run(&store, &factory_manager).await.unwrap();
        let after = reconciler.query(None, None).await.unwrap();

        assert_eq!(
            before.len(),
            after.len(),
            "dry_run must not change the number of system entities"
        );
    }

    // ── Feature: is_applying flag (AC: Self-changes are excluded) ─────────────

    /// AC: is_applying defaults to false — the flag must start in a clean state.
    #[test]
    fn test_is_applying_defaults_to_false() {
        let reconciler = Reconciler::new();
        assert!(!reconciler.is_applying(), "is_applying must be false on initialization");
    }

    /// AC: set_applying(true) makes is_applying return true.
    #[test]
    fn test_set_applying_true_makes_is_applying_return_true() {
        let reconciler = Reconciler::new();
        reconciler.set_applying(true);
        assert!(
            reconciler.is_applying(),
            "is_applying must be true after set_applying(true)"
        );
    }

    /// AC: set_applying(false) after set_applying(true) resets the flag.
    #[test]
    fn test_set_applying_false_resets_is_applying_to_false() {
        let reconciler = Reconciler::new();
        reconciler.set_applying(true);
        reconciler.set_applying(false);
        assert!(
            !reconciler.is_applying(),
            "is_applying must be false after set_applying(false)"
        );
    }

    /// AC: The applying flag can be toggled repeatedly without corruption.
    #[test]
    fn test_set_applying_flag_can_be_toggled_repeatedly() {
        let reconciler = Reconciler::new();
        for _ in 0..5 {
            reconciler.set_applying(true);
            assert!(reconciler.is_applying());
            reconciler.set_applying(false);
            assert!(!reconciler.is_applying());
        }
    }

    // ── Feature: managed_entity_names (AC: Monitor ignores unmanaged) ─────────

    /// AC: No policies → managed_entity_names returns empty set.
    #[test]
    fn test_managed_entity_names_returns_empty_for_no_policies() {
        let reconciler = Reconciler::new();
        let store = PolicyStore::ephemeral(vec![]);
        let fm = FactoryManager::new();
        let names = reconciler.managed_entity_names(&store, &fm);
        assert!(
            names.is_empty(),
            "no policies → managed entity names must be empty"
        );
    }

    /// AC: A static policy for an interface → that interface appears in managed names.
    #[test]
    fn test_managed_entity_names_includes_policy_target_interface() {
        use netfyr_policy::parse_policy_yaml;
        let reconciler = Reconciler::new();
        let yaml = "kind: policy\nname: p1\nfactory: static\npriority: 100\n\
                    state:\n  type: ethernet\n  name: managed-iface0\n  mtu: 1400\n";
        let policies = parse_policy_yaml(yaml).unwrap();
        let store = PolicyStore::ephemeral(policies);
        let fm = FactoryManager::new();
        let names = reconciler.managed_entity_names(&store, &fm);
        assert!(
            names.contains("managed-iface0"),
            "managed entity names must include the policy target interface"
        );
    }

    /// AC: Interfaces not covered by any policy are absent from managed names.
    #[test]
    fn test_managed_entity_names_excludes_non_policy_interfaces() {
        use netfyr_policy::parse_policy_yaml;
        let reconciler = Reconciler::new();
        let yaml = "kind: policy\nname: p1\nfactory: static\npriority: 100\n\
                    state:\n  type: ethernet\n  name: managed-iface0\n  mtu: 1400\n";
        let policies = parse_policy_yaml(yaml).unwrap();
        let store = PolicyStore::ephemeral(policies);
        let fm = FactoryManager::new();
        let names = reconciler.managed_entity_names(&store, &fm);
        assert!(
            !names.contains("unmanaged-iface1"),
            "unmanaged-iface1 must not appear in managed entity names"
        );
    }

    /// AC: Multiple policies → all their targets appear in managed names.
    #[test]
    fn test_managed_entity_names_includes_all_policy_targets() {
        use netfyr_policy::parse_policy_yaml;
        let reconciler = Reconciler::new();
        let yaml = "kind: policy\nname: p1\nfactory: static\npriority: 100\n\
                    state:\n  type: ethernet\n  name: iface-a\n  mtu: 1400\n\
                    ---\nkind: policy\nname: p2\nfactory: static\npriority: 100\n\
                    state:\n  type: ethernet\n  name: iface-b\n  mtu: 1500\n";
        let policies = parse_policy_yaml(yaml).unwrap();
        let store = PolicyStore::ephemeral(policies);
        let fm = FactoryManager::new();
        let names = reconciler.managed_entity_names(&store, &fm);
        assert!(names.contains("iface-a"), "iface-a must be in managed names");
        assert!(names.contains("iface-b"), "iface-b must be in managed names");
    }

    // ── Feature: record_external_change ───────────────────────────────────────

    /// AC: record_external_change with an empty entity list returns Ok immediately.
    #[tokio::test]
    async fn test_record_external_change_with_empty_entity_list_returns_ok() {
        let reconciler = Reconciler::new();
        let store = PolicyStore::ephemeral(vec![]);
        let result = reconciler.record_external_change(vec![], &store).await;
        assert!(
            result.is_ok(),
            "record_external_change with empty list must return Ok: {:?}",
            result.err()
        );
    }

    /// AC: record_external_change for a nonexistent interface returns Ok without panicking.
    ///
    /// When the backend cannot find the interface, no journal entry is written and
    /// the function returns Ok (graceful degradation).
    #[tokio::test]
    async fn test_record_external_change_for_nonexistent_interface_returns_ok() {
        let reconciler = Reconciler::new();
        let store = PolicyStore::ephemeral(vec![]);
        // The backend query for "nonexistent-eth99999" returns no state.
        let result = reconciler
            .record_external_change(vec!["nonexistent-eth99999".to_string()], &store)
            .await;
        assert!(
            result.is_ok(),
            "record_external_change for a nonexistent interface must return Ok"
        );
    }

    /// AC: record_external_change for multiple nonexistent interfaces returns Ok.
    #[tokio::test]
    async fn test_record_external_change_for_multiple_nonexistent_interfaces_returns_ok() {
        let reconciler = Reconciler::new();
        let store = PolicyStore::ephemeral(vec![]);
        let result = reconciler
            .record_external_change(
                vec![
                    "nonexistent-0".to_string(),
                    "nonexistent-1".to_string(),
                    "nonexistent-2".to_string(),
                ],
                &store,
            )
            .await;
        assert!(
            result.is_ok(),
            "record_external_change for multiple nonexistent interfaces must return Ok"
        );
    }
}
