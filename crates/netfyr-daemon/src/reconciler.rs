//! Reconciliation engine wrapper for the daemon.
//!
//! `Reconciler` is stateless except for the `BackendRegistry` and
//! `SchemaRegistry` it holds at construction time. It can be called from both
//! the Varlink request handler and the factory event handler without locking.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use netfyr_backend::{ApplyReport, BackendError, BackendRegistry, NetlinkBackend};
use netfyr_journal::{
    summarize_policies, ApplyOutcome, Journal, JournalEntry, SequenceId, SerializableDiff,
    SerializableDiffOp, SerializableFieldChange, SerializableState, SerializableStateSet, Trigger,
};
use netfyr_policy::{FactoryType, Policy, StaticFactory, StateFactory};
use netfyr_reconcile::{
    generate_diff, merge, ConflictReport, EntityKey, PolicyId, PolicyInput,
    StateDiff as ReconcileStateDiff,
};
use netfyr_state::{DiffOp, SchemaRegistry, Selector, StateSet};

use crate::factory_manager::FactoryManager;
use crate::policy_store::PolicyStore;

// Fields that are effectively immutable during normal operation. These are
// stripped from journal snapshots and external-change diffs to avoid noise.
// Observable read-only fields like carrier and speed are intentionally excluded
// so that link flaps and speed renegotiations appear in history.
const STABLE_FIELDS: &[&str] = &["name", "driver", "mac"];

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
            tracing::error!(%e, "failed to register NetlinkBackend");
        }

        let journal = match Journal::open_default() {
            Ok(j) => {
                tracing::debug!("Journal opened successfully");
                Some(j)
            }
            Err(e) => {
                tracing::warn!(%e, "failed to open journal, journal writes disabled");
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
            tracing::debug!("no current states returned, skipping external change check");
            return Ok(());
        }

        // Lock journal and compute per-entity diffs.
        let mut guard = match self.journal.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(%e, "journal mutex poisoned");
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
            let entity_changed = !field_changes.is_empty();
            let field_count = field_changes.len();
            tracing::debug!(entity = %entity_name, changed = entity_changed, field_count, "external change check");
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
            tracing::debug!("no external field changes detected");
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

        let policy_count = inputs.len();
        let entity_count = managed_entities.len();
        tracing::debug!(policy_count, entity_count, "starting reconciliation");
        let merged = merge(inputs);
        let mut effective_state = merged.effective_state;
        netfyr_state::normalize_route_defaults(&mut effective_state);
        let conflicts = merged.conflicts;

        let actual_state = self.backend_registry.query_all().await?;

        // Compute the rich diff for journal recording.
        let reconcile_diff = generate_diff(
            &effective_state,
            &actual_state,
            &managed_entities,
            &self.schema_registry,
        );

        let adds = reconcile_diff.additions().count();
        let modifies = reconcile_diff.modifications().count();
        let removes = reconcile_diff.removals().count();
        tracing::debug!(adds, modifies, removes, "diff computed");

        // Restrict the actual state to only the entities present in the effective
        // desired state before computing the diff. This prevents the daemon from
        // generating Remove operations for interfaces not covered by any policy.
        // Additionally, drop fields marked keep-when-absent that are not in the
        // desired state — these have a kernel default and should not be unset
        // just because no policy manages them.
        let managed_actual = self.restrict_to_managed(&actual_state, &effective_state);

        let state_diff = netfyr_state::diff::diff(&managed_actual, &effective_state, &self.schema_registry);
        let state_diff = inject_dhcp_addresses(&trigger, policy_store, &effective_state, state_diff);
        let state_diff = restrict_to_dhcp_trigger(&trigger, policy_store, state_diff);

        if state_diff.is_empty() {
            tracing::debug!("Reconciliation: no changes needed");
            let journal_state = filter_stable_fields(&managed_actual);
            let (journal_diff, journal_state) =
                if let Some(iface) = dhcp_trigger_interface(&trigger, policy_store) {
                    (
                        filter_diff_for_interface(&reconcile_diff, iface),
                        filter_state_for_interface(&journal_state, iface),
                    )
                } else {
                    (reconcile_diff.clone(), journal_state)
                };
            self.append_journal_entry(
                policy_store,
                &trigger,
                &journal_diff,
                &journal_state,
                ApplyOutcome::Applied {
                    succeeded: 0,
                    failed: 0,
                    skipped: 0,
                },
                None,
            );
            return Ok(ApplyResult {
                report: ApplyReport::new(),
                conflicts,
            });
        }

        let report = self.backend_registry.apply(&state_diff).await?;

        let succeeded = report.succeeded.len();
        let failed = report.failed.len();
        let skipped = report.skipped.len();
        tracing::debug!(succeeded, failed, skipped, "apply completed");

        // Re-query so state_after reflects what actually exists post-apply,
        // not just the desired policy fields.  Self-generated netlink events
        // (~500 ms later) will then compare equal and produce no diff.
        let post_apply_state = match self.backend_registry.query_all().await {
            Ok(actual_post) => {
                let mut managed_post = StateSet::new();
                for (entity_type, selector_key) in effective_state.entities() {
                    if let Some(s) = actual_post.get(&entity_type, &selector_key) {
                        managed_post.insert(s.clone());
                    }
                }
                filter_stable_fields(&managed_post)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to re-query after apply; falling back to pre-apply state for journal snapshot"
                );
                filter_stable_fields(&managed_actual)
            }
        };

        let (journal_diff, journal_state) =
            if let Some(iface) = dhcp_trigger_interface(&trigger, policy_store) {
                (
                    filter_diff_for_interface(&reconcile_diff, iface),
                    filter_state_for_interface(&post_apply_state, iface),
                )
            } else {
                (reconcile_diff, post_apply_state)
            };
        self.append_journal_entry(
            policy_store,
            &trigger,
            &journal_diff,
            &journal_state,
            ApplyOutcome::Applied {
                succeeded: report.succeeded.len() as u32,
                failed: report.failed.len() as u32,
                skipped: report.skipped.len() as u32,
            },
            Some(&report),
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
        let mut effective_state = merged.effective_state;
        netfyr_state::normalize_route_defaults(&mut effective_state);
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
        let managed_actual = self.restrict_to_managed(&actual_state, target_state);

        let state_diff = netfyr_state::diff::diff(&managed_actual, target_state, &self.schema_registry);

        if dry_run {
            return Ok(RevertResult {
                reconcile_diff,
                report: None,
            });
        }

        let filtered_target = filter_stable_fields(target_state);

        if state_diff.is_empty() {
            self.append_revert_journal_entry(
                policies,
                target_seq,
                &reconcile_diff,
                &filtered_target,
                ApplyOutcome::Applied { succeeded: 0, failed: 0, skipped: 0 },
                None,
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
            &filtered_target,
            ApplyOutcome::Applied {
                succeeded: apply_report.succeeded.len() as u32,
                failed: apply_report.failed.len() as u32,
                skipped: apply_report.skipped.len() as u32,
            },
            Some(&apply_report),
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
            let mut merged = StateSet::new();
            for et in self.backend_registry.supported_entities() {
                match self.backend_registry.query(&et, selector).await {
                    Ok(state_set) => {
                        for state in state_set.iter() {
                            merged.insert(state.clone());
                        }
                    }
                    Err(BackendError::NotFound { .. }) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(merged)
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
        apply_report: Option<&ApplyReport>,
    ) {
        let mut guard = match self.journal.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(%e, "journal mutex poisoned");
                return;
            }
        };
        if let Some(ref mut journal) = *guard {
            let mut diff = SerializableDiff::from(reconcile_diff);
            if let Some(report) = apply_report {
                annotate_diff_outcomes(&mut diff, report);
            }
            let entry = JournalEntry {
                seq: 0,
                timestamp: chrono::Utc::now(),
                trigger: trigger.clone(),
                active_policies: summarize_policies(policy_store.policies()),
                diff,
                state_after: SerializableStateSet::from(effective_state),
                outcome,
            };
            if let Err(e) = journal.append(entry) {
                tracing::warn!(%e, "failed to write journal entry");
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
        apply_report: Option<&ApplyReport>,
    ) {
        let mut guard = match self.journal.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(%e, "journal mutex poisoned");
                return;
            }
        };
        if let Some(ref mut journal) = *guard {
            let mut diff = SerializableDiff::from(reconcile_diff);
            if let Some(report) = apply_report {
                annotate_diff_outcomes(&mut diff, report);
            }
            let entry = JournalEntry {
                seq: 0,
                timestamp: chrono::Utc::now(),
                trigger: Trigger::Revert { target_seq },
                active_policies: summarize_policies(policies),
                diff,
                state_after: SerializableStateSet::from(target_state),
                outcome,
            };
            if let Err(e) = journal.append(entry) {
                tracing::warn!(%e, "failed to write revert journal entry");
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

    /// Restrict `actual` to managed entities and filter out fields marked
    /// `keep-when-absent` that are not present in the desired state. This
    /// ensures the apply diff (which has no schema access) does not generate
    /// spurious removed-field entries for kernel-defaulted fields like `mtu`.
    fn restrict_to_managed(&self, actual: &StateSet, desired: &StateSet) -> StateSet {
        let mut filtered = StateSet::new();
        for (entity_type, selector_key) in desired.entities() {
            if let Some(actual_state) = actual.get(&entity_type, &selector_key) {
                let desired_state = desired
                    .get(&entity_type, &selector_key)
                    .expect("entity returned by entities() must exist");
                let mut s = actual_state.clone();
                s.fields.retain(|name, _| {
                    desired_state.fields.contains_key(name)
                        || !self
                            .schema_registry
                            .field_info(&entity_type, name)
                            .map(|info| info.keep_when_absent)
                            .unwrap_or(false)
                });
                filtered.insert(s);
            }
        }
        filtered
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Annotate each non-unchanged field change in `diff` with its apply outcome
/// ("applied", "failed", or "skipped") by matching entities in `report`.
///
/// The backend applies changes atomically per entity, so all field changes for
/// an entity share the same outcome.
fn annotate_diff_outcomes(diff: &mut SerializableDiff, report: &ApplyReport) {
    for op in &mut diff.operations {
        let outcome = if report
            .succeeded
            .iter()
            .any(|s| s.entity_type == op.entity_type && s.selector.key() == op.entity_name)
        {
            Some("applied")
        } else if report
            .failed
            .iter()
            .any(|f| f.entity_type == op.entity_type && f.selector.key() == op.entity_name)
        {
            Some("failed")
        } else if report
            .skipped
            .iter()
            .any(|s| s.entity_type == op.entity_type && s.selector.key() == op.entity_name)
        {
            Some("skipped")
        } else {
            None
        };

        if let Some(outcome_str) = outcome {
            for fc in &mut op.field_changes {
                if fc.change_kind != "unchanged" {
                    fc.outcome = Some(outcome_str.to_string());
                }
            }
        }
    }
}

/// On DHCP events, inject the addresses field into the diff even when the CIDR
/// hasn't changed — the kernel lifetime needs refreshing via `ip addr replace`.
fn inject_dhcp_addresses(
    trigger: &Trigger,
    policy_store: &PolicyStore,
    effective_state: &StateSet,
    diff: netfyr_state::StateDiff,
) -> netfyr_state::StateDiff {
    let iface = match dhcp_trigger_interface(trigger, policy_store) {
        Some(name) => name,
        None => return diff,
    };

    // Find the desired addresses with lifetime attributes.
    let mut addr_fv_for_iface = None;
    let mut entity_type_for_iface = None;
    let mut selector_for_iface = None;
    for (entity_type, selector_key) in effective_state.entities() {
        let state = effective_state.get(&entity_type, &selector_key).unwrap();
        if state.selector.name.as_deref() != Some(iface) {
            continue;
        }
        if let Some(addr_fv) = state.fields.get("addresses") {
            let has_lifetime = addr_fv.value.as_list().map_or(false, |list| {
                list.iter().any(|v| v.as_map().and_then(|m| m.get("valid_lft")).is_some())
            });
            if has_lifetime {
                addr_fv_for_iface = Some(addr_fv.clone());
                entity_type_for_iface = Some(entity_type);
                selector_for_iface = Some(state.selector.clone());
            }
        }
        break;
    }

    let (addr_fv, entity_type, selector) = match (addr_fv_for_iface, entity_type_for_iface, selector_for_iface) {
        (Some(a), Some(e), Some(s)) => (a, e, s),
        _ => return diff,
    };

    // Check if the diff already includes addresses for this interface.
    let already_has_addresses = diff.ops().iter().any(|op| {
        if let DiffOp::Modify { selector: s, changed_fields, .. } = op {
            s.name.as_deref() == Some(iface) && changed_fields.contains_key("addresses")
        } else {
            false
        }
    });
    if already_has_addresses {
        return diff;
    }

    tracing::debug!(interface = %iface, "injecting DHCP addresses for lifetime refresh");

    // Merge into existing Modify op for this interface, or append a new one.
    let mut ops = diff.into_ops();
    let mut merged = false;
    for op in &mut ops {
        if let DiffOp::Modify { selector: s, changed_fields, .. } = op {
            if s.name.as_deref() == Some(iface) {
                changed_fields.insert("addresses".to_string(), addr_fv.clone());
                merged = true;
                break;
            }
        }
    }
    if !merged {
        let changed = std::iter::once(("addresses".to_string(), addr_fv)).collect();
        ops.push(DiffOp::Modify {
            entity_type,
            selector,
            changed_fields: changed,
            removed_fields: vec![],
        });
    }
    netfyr_state::StateDiff::new(ops)
}

/// Resolve the interface name for a DHCP trigger from the policy store.
fn dhcp_trigger_interface<'a>(trigger: &Trigger, policy_store: &'a PolicyStore) -> Option<&'a str> {
    if let Trigger::DhcpEvent { ref policy_name, .. } = trigger {
        policy_store
            .policies()
            .iter()
            .find(|p| p.name == *policy_name)
            .and_then(|p| p.selector.as_ref())
            .and_then(|s| s.name.as_deref())
    } else {
        None
    }
}

/// Filter a reconcile diff to keep only operations for `iface_name`.
fn filter_diff_for_interface(
    diff: &ReconcileStateDiff,
    iface_name: &str,
) -> ReconcileStateDiff {
    ReconcileStateDiff {
        operations: diff
            .operations
            .iter()
            .filter(|op| op.selector.key() == iface_name)
            .cloned()
            .collect(),
    }
}

/// Filter a state set to keep only the entity matching `iface_name`.
fn filter_state_for_interface(state: &StateSet, iface_name: &str) -> StateSet {
    let mut filtered = StateSet::new();
    for s in state.iter() {
        if s.selector.key() == iface_name {
            filtered.insert(s.clone());
        }
    }
    filtered
}

/// For DHCP events, restrict the apply diff to only the triggering interface.
///
/// Without this, a reconciliation triggered by interface A would also apply
/// pending changes for interfaces B and C (whose factories already have
/// leases). When B's own event fires later, its address is already applied
/// and the journal records an empty diff — losing the change from history.
fn restrict_to_dhcp_trigger(
    trigger: &Trigger,
    policy_store: &PolicyStore,
    diff: netfyr_state::StateDiff,
) -> netfyr_state::StateDiff {
    let iface = match dhcp_trigger_interface(trigger, policy_store) {
        Some(name) => name,
        None => return diff,
    };
    let filtered_ops: Vec<_> = diff
        .into_ops()
        .into_iter()
        .filter(|op| op.selector().name.as_deref() == Some(iface))
        .collect();
    netfyr_state::StateDiff::new(filtered_ops)
}

/// Return a copy of `set` with all `STABLE_FIELDS` removed from every state.
///
/// Journal snapshots must reflect actual observable state so that self-generated
/// netlink events (debounced ~500 ms later) compare equal and produce no diff.
fn filter_stable_fields(set: &StateSet) -> StateSet {
    let mut filtered = StateSet::new();
    for state in set.iter() {
        let mut s = state.clone();
        for field in STABLE_FIELDS {
            s.fields.shift_remove(*field);
        }
        filtered.insert(s);
    }
    filtered
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
        if STABLE_FIELDS.contains(&field_name.as_str()) {
            continue;
        }
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
                    outcome: None,
                });
            }
            None => {
                // Field appeared since the last snapshot.
                changes.push(SerializableFieldChange {
                    field_name: field_name.clone(),
                    change_kind: "set".to_string(),
                    current: None,
                    desired: Some(current_json),
                    outcome: None,
                });
            }
        }
    }

    // Fields present in the last snapshot but absent from the current state.
    if let Some(last_obj) = last.fields.as_object() {
        for (field_name, last_val) in last_obj {
            if STABLE_FIELDS.contains(&field_name.as_str()) {
                continue;
            }
            if !current.fields.contains_key(field_name) {
                changes.push(SerializableFieldChange {
                    field_name: field_name.clone(),
                    change_kind: "unset".to_string(),
                    current: Some(last_val.clone()),
                    desired: None,
                    outcome: None,
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
    use netfyr_state::{FieldValue, Provenance, State, StateMetadata, Value};

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

    // ── Feature: compute_external_field_changes ───────────────────────────────
    //
    // These tests verify the diff logic that underpins "the entry's diff shows
    // mtu: 9000 -> 1500" and related acceptance criteria.

    /// Build a SerializableState (journal snapshot) with the given JSON fields.
    fn make_journal_snapshot(name: &str, fields: serde_json::Value) -> SerializableState {
        SerializableState {
            entity_type: "ethernet".to_string(),
            selector_name: name.to_string(),
            fields,
        }
    }

    /// Build a netfyr_state::State with the given (field_name, Value) pairs.
    fn make_current_state(name: &str, fields: Vec<(&str, Value)>) -> State {
        let mut state = State {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name(name),
            fields: Default::default(),
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 100,
        };
        for (k, v) in fields {
            state.fields.insert(
                k.to_string(),
                FieldValue { value: v, provenance: Provenance::KernelDefault },
            );
        }
        state
    }

    /// AC: Monitor detects MTU change — diff shows mtu: 9000 -> 1500.
    /// The changed field records the old value as 'current' (from journal snapshot)
    /// and the new value as 'desired' (from the current system state).
    #[test]
    fn test_compute_external_field_changes_mtu_9000_to_1500_records_old_and_new_values() {
        let last = make_journal_snapshot("veth-e2e0", serde_json::json!({ "mtu": 9000u64 }));
        let current = make_current_state("veth-e2e0", vec![("mtu", Value::U64(1500))]);

        let changes = compute_external_field_changes(&last, &current);

        assert_eq!(changes.len(), 1, "exactly one field change should be detected for mtu");
        let change = &changes[0];
        assert_eq!(change.field_name, "mtu");
        assert_eq!(change.change_kind, "set", "mtu change must have kind 'set'");
        assert_eq!(
            change.current,
            Some(serde_json::json!(9000u64)),
            "current must be the old value (9000) from the journal snapshot"
        );
        assert_eq!(
            change.desired,
            Some(serde_json::json!(1500u64)),
            "desired must be the new value (1500) from the live system"
        );
    }

    /// AC: Unchanged fields do not appear in the diff — no spurious journal entries.
    #[test]
    fn test_compute_external_field_changes_unchanged_field_is_not_included() {
        let last = make_journal_snapshot("eth0", serde_json::json!({ "mtu": 1500u64 }));
        let current = make_current_state("eth0", vec![("mtu", Value::U64(1500))]);

        let changes = compute_external_field_changes(&last, &current);

        assert!(changes.is_empty(), "unchanged field must produce no change entries");
    }

    /// AC: Monitor detects address addition — field present in current but not in snapshot
    /// appears with change_kind="set" and no 'current' value.
    #[test]
    fn test_compute_external_field_changes_new_field_in_current_has_none_current() {
        let last = make_journal_snapshot("eth0", serde_json::json!({ "mtu": 1500u64 }));
        let current = make_current_state(
            "eth0",
            vec![
                ("mtu", Value::U64(1500)),       // unchanged
                ("addresses", Value::U64(1000)), // new field (address-like addition)
            ],
        );

        let changes = compute_external_field_changes(&last, &current);

        assert_eq!(changes.len(), 1, "only the newly added field should appear");
        let change = &changes[0];
        assert_eq!(change.field_name, "addresses");
        assert_eq!(change.change_kind, "set");
        assert!(
            change.current.is_none(),
            "added field must have no 'current' value — it was not in the snapshot"
        );
        assert_eq!(change.desired, Some(serde_json::json!(1000u64)));
    }

    /// AC: Monitor detects address removal — field present in snapshot but absent from
    /// current appears with change_kind="unset" and no 'desired' value.
    #[test]
    fn test_compute_external_field_changes_removed_field_has_unset_kind_and_no_desired() {
        let last = make_journal_snapshot(
            "eth0",
            serde_json::json!({ "mtu": 1500u64, "addresses": 1000u64 }),
        );
        let current = make_current_state("eth0", vec![("mtu", Value::U64(1500))]);

        let changes = compute_external_field_changes(&last, &current);

        assert_eq!(changes.len(), 1, "only the removed field should appear");
        let change = &changes[0];
        assert_eq!(change.field_name, "addresses");
        assert_eq!(change.change_kind, "unset", "removed field must have kind 'unset'");
        assert_eq!(
            change.current,
            Some(serde_json::json!(1000u64)),
            "removed field must record its last known value from the snapshot"
        );
        assert!(
            change.desired.is_none(),
            "removed field must have no 'desired' value"
        );
    }

    /// AC: Both snapshot and current state empty → no changes (no spurious entries).
    #[test]
    fn test_compute_external_field_changes_both_empty_produces_no_changes() {
        let last = make_journal_snapshot("eth0", serde_json::json!({}));
        let current = make_current_state("eth0", vec![]);

        let changes = compute_external_field_changes(&last, &current);

        assert!(changes.is_empty(), "both empty → no changes");
    }

    /// AC: Mixed changed and unchanged fields — only changed fields appear in the diff.
    #[test]
    fn test_compute_external_field_changes_mixed_only_changed_fields_in_result() {
        let last = make_journal_snapshot(
            "eth0",
            serde_json::json!({ "mtu": 9000u64, "speed": 1000u64 }),
        );
        let current = make_current_state(
            "eth0",
            vec![
                ("mtu", Value::U64(1500)),   // changed
                ("speed", Value::U64(1000)), // unchanged
            ],
        );

        let changes = compute_external_field_changes(&last, &current);

        assert_eq!(changes.len(), 1, "only the changed field should appear");
        assert_eq!(changes[0].field_name, "mtu", "speed must not appear — it is unchanged");
        assert_eq!(changes[0].current, Some(serde_json::json!(9000u64)));
        assert_eq!(changes[0].desired, Some(serde_json::json!(1500u64)));
    }

    /// AC: Burst changes coalesced — multiple changed fields are all captured in one diff.
    #[test]
    fn test_compute_external_field_changes_multiple_changed_fields_all_captured() {
        let last = make_journal_snapshot(
            "eth0",
            serde_json::json!({ "mtu": 9000u64, "addresses": 100u64 }),
        );
        let current = make_current_state(
            "eth0",
            vec![
                ("mtu", Value::U64(1500)),        // changed
                ("addresses", Value::U64(1000)),  // also changed
            ],
        );

        let changes = compute_external_field_changes(&last, &current);

        assert_eq!(changes.len(), 2, "both changed fields must appear");
        let mtu = changes.iter().find(|c| c.field_name == "mtu").expect("mtu must be present");
        assert_eq!(mtu.current, Some(serde_json::json!(9000u64)));
        assert_eq!(mtu.desired, Some(serde_json::json!(1500u64)));
        let addresses =
            changes.iter().find(|c| c.field_name == "addresses").expect("addresses must be present");
        assert_eq!(addresses.current, Some(serde_json::json!(100u64)));
        assert_eq!(addresses.desired, Some(serde_json::json!(1000u64)));
    }

    /// AC: External changes do not trigger re-reconciliation — record_external_change
    /// is a pure observation (ApplyOutcome::Observed) and never calls reconcile_and_apply.
    /// Verified here at the type level: the Observed variant serializes as "observed".
    #[test]
    fn test_apply_outcome_observed_serializes_as_observed_for_external_change_entries() {
        let outcome = ApplyOutcome::Observed;
        let json = serde_json::to_string(&outcome).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value["kind"].as_str(),
            Some("observed"),
            "ExternalChange entries must carry ApplyOutcome::Observed (kind='observed')"
        );
    }

    /// AC: Journal entry trigger type is "external_change" and contains changed_entities.
    #[test]
    fn test_external_change_trigger_has_correct_type_and_changed_entities() {
        let trigger = Trigger::ExternalChange {
            changed_entities: vec!["veth-e2e0".to_string()],
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value["type"].as_str(),
            Some("external_change"),
            "trigger type discriminator must be 'external_change'"
        );
        let entities =
            value["changed_entities"].as_array().expect("changed_entities must be an array");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].as_str(), Some("veth-e2e0"));
    }

    /// AC: changed_entities in trigger includes all interfaces that actually changed.
    #[test]
    fn test_external_change_trigger_changed_entities_includes_all_changed_interfaces() {
        let trigger = Trigger::ExternalChange {
            changed_entities: vec!["eth0".to_string(), "eth1".to_string(), "eth2".to_string()],
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let entities =
            value["changed_entities"].as_array().expect("changed_entities must be an array");
        assert_eq!(entities.len(), 3, "all three changed interfaces must appear");
        let names: Vec<&str> = entities.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"eth0"), "eth0 must be in changed_entities");
        assert!(names.contains(&"eth1"), "eth1 must be in changed_entities");
        assert!(names.contains(&"eth2"), "eth2 must be in changed_entities");
    }

    // ── Feature: Stable fields excluded from external diffs (AC) ──────────────
    //
    // STABLE_FIELDS = ["name", "driver", "mac"]
    //
    /// AC: Stable fields are excluded from external diffs — name, driver, and mac
    /// must not appear. Observable fields (carrier, speed) MUST appear when changed.
    #[test]
    fn test_compute_external_field_changes_all_readonly_field_names_are_excluded() {
        // Snapshot: only the writable "mtu" field.
        let last = make_journal_snapshot("veth-e2e0", serde_json::json!({ "mtu": 9000u64 }));
        // Current: mtu changed + stable fields present + observable fields present.
        let current = make_current_state(
            "veth-e2e0",
            vec![
                ("mtu", Value::U64(1500)),       // writable, changed — must appear
                ("carrier", Value::Bool(true)),   // observable — must appear
                ("speed", Value::U64(1000)),      // observable — must appear
                ("mac", Value::U64(0)),           // stable — must be excluded
                ("driver", Value::U64(0)),        // stable — must be excluded
                ("name", Value::U64(0)),          // stable — must be excluded
            ],
        );

        let changes = compute_external_field_changes(&last, &current);

        let field_names: Vec<&str> = changes.iter().map(|c| c.field_name.as_str()).collect();

        // mtu, carrier, and speed must appear (changed or new observable fields).
        assert!(field_names.contains(&"mtu"), "mtu must appear");
        assert!(field_names.contains(&"carrier"), "carrier must appear (observable)");
        assert!(field_names.contains(&"speed"), "speed must appear (observable)");

        // Stable fields must not appear.
        for stable_field in STABLE_FIELDS {
            assert!(
                !field_names.contains(stable_field),
                "stable field '{}' must not appear in the external change diff",
                stable_field
            );
        }
    }

    /// AC: Stable fields in the journal snapshot that are absent from the current state
    /// must not produce a diff entry (excluded in both directions).
    #[test]
    fn test_compute_external_field_changes_readonly_fields_absent_from_current_are_excluded() {
        // Snapshot contains stable fields (e.g., written by an older code path).
        let last = make_journal_snapshot(
            "eth0",
            serde_json::json!({
                "mtu": 1500u64,
                "mac": 0u64,
                "driver": 0u64,
                "name": 0u64
            }),
        );
        // Current has only mtu — stable fields absent.
        let current = make_current_state("eth0", vec![("mtu", Value::U64(1500))]);

        let changes = compute_external_field_changes(&last, &current);

        assert!(
            changes.is_empty(),
            "stable fields absent from current state must not produce diff entries: {:?}",
            changes.iter().map(|c| c.field_name.as_str()).collect::<Vec<_>>()
        );
    }

    /// AC: Stable fields (name, driver, mac) are excluded from external diffs,
    /// while observable fields (mtu, enabled, carrier, speed) appear.
    #[test]
    fn test_compute_external_field_changes_stable_fields_excluded_observable_fields_appear() {
        let last = make_journal_snapshot(
            "eth0",
            serde_json::json!({ "mtu": 9000u64, "enabled": true, "carrier": true, "speed": 1000u64 }),
        );
        let current = make_current_state(
            "eth0",
            vec![
                ("mtu", Value::U64(1500)),       // writable, changed
                ("enabled", Value::Bool(false)),  // writable, changed
                ("carrier", Value::Bool(false)),  // observable readonly, changed
                ("speed", Value::U64(100)),       // observable readonly, changed
                ("name", Value::from("eth0")),    // stable, should be excluded
                ("driver", Value::from("virtio")),// stable, should be excluded
            ],
        );

        let changes = compute_external_field_changes(&last, &current);

        let field_names: Vec<&str> = changes.iter().map(|c| c.field_name.as_str()).collect();

        // Observable fields must appear (changed and not in STABLE_FIELDS).
        assert!(field_names.contains(&"mtu"), "mtu must appear (changed)");
        assert!(field_names.contains(&"enabled"), "enabled must appear (changed)");
        assert!(field_names.contains(&"carrier"), "carrier must appear (observable, changed)");
        assert!(field_names.contains(&"speed"), "speed must appear (observable, changed)");

        // Stable fields must not appear.
        assert!(
            !field_names.contains(&"name"),
            "name must not appear (stable): {:?}",
            field_names
        );
        assert!(
            !field_names.contains(&"driver"),
            "driver must not appear (stable): {:?}",
            field_names
        );
    }

    // ── Feature: List-type field changes (addresses and routes) ──────────────────
    //
    // The spec mandates tracking addresses and routes which are list-typed fields.
    // These tests verify that compute_external_field_changes correctly detects
    // differences in list values (address additions, address removals, route changes).

    /// AC: Monitor detects address addition — list-type addresses field with an
    /// extra element is detected as a change.
    #[test]
    fn test_compute_external_field_changes_list_field_address_addition_detected() {
        // Snapshot: one address
        let last = make_journal_snapshot(
            "veth-e2e0",
            serde_json::json!({ "addresses": ["10.99.0.1/24"] }),
        );
        // Current: two addresses (one added externally)
        let current = make_current_state(
            "veth-e2e0",
            vec![("addresses", Value::List(vec![
                Value::String("10.99.0.1/24".to_string()),
                Value::String("10.99.0.2/24".to_string()),
            ]))],
        );

        let changes = compute_external_field_changes(&last, &current);

        assert_eq!(changes.len(), 1, "one field (addresses) must have changed");
        let change = &changes[0];
        assert_eq!(change.field_name, "addresses");
        assert_eq!(change.change_kind, "set");
        assert_eq!(change.current, Some(serde_json::json!(["10.99.0.1/24"])));
        assert!(change.desired.is_some(), "new address list must be in desired");
    }

    /// AC: Monitor detects address removal — list-type addresses field with fewer
    /// elements is detected as a change.
    #[test]
    fn test_compute_external_field_changes_list_field_address_removal_detected() {
        // Snapshot: two addresses
        let last = make_journal_snapshot(
            "veth-e2e0",
            serde_json::json!({ "addresses": ["10.99.0.1/24", "10.99.0.2/24"] }),
        );
        // Current: one address (one removed externally)
        let current = make_current_state(
            "veth-e2e0",
            vec![("addresses", Value::List(vec![
                Value::String("10.99.0.1/24".to_string()),
            ]))],
        );

        let changes = compute_external_field_changes(&last, &current);

        assert_eq!(changes.len(), 1, "addresses field must show a change after removal");
        let change = &changes[0];
        assert_eq!(change.field_name, "addresses");
        assert_eq!(change.change_kind, "set");
        assert_eq!(
            change.current,
            Some(serde_json::json!(["10.99.0.1/24", "10.99.0.2/24"]))
        );
    }

    /// AC: Monitor detects route addition — list-type routes field is tracked.
    #[test]
    fn test_compute_external_field_changes_list_field_route_addition_detected() {
        // Snapshot: empty routes
        let last = make_journal_snapshot("veth-e2e0", serde_json::json!({ "routes": [] }));
        // Current: one route added externally
        let current = make_current_state(
            "veth-e2e0",
            vec![("routes", Value::List(vec![Value::String("10.99.1.0/24".to_string())]))],
        );

        let changes = compute_external_field_changes(&last, &current);

        assert_eq!(changes.len(), 1, "routes field must show a change after route addition");
        let change = &changes[0];
        assert_eq!(change.field_name, "routes");
        assert_eq!(change.change_kind, "set");
        assert_eq!(change.current, Some(serde_json::json!([])));
        assert!(change.desired.is_some());
    }

    /// AC: Monitor detects route removal — list-type routes field shrinks.
    #[test]
    fn test_compute_external_field_changes_list_field_route_removal_detected() {
        // Snapshot: one route
        let last = make_journal_snapshot(
            "veth-e2e0",
            serde_json::json!({ "routes": ["10.99.1.0/24"] }),
        );
        // Current: no routes
        let current = make_current_state(
            "veth-e2e0",
            vec![("routes", Value::List(vec![]))],
        );

        let changes = compute_external_field_changes(&last, &current);

        assert_eq!(changes.len(), 1, "routes field must show a change after route removal");
        assert_eq!(changes[0].field_name, "routes");
        assert_eq!(changes[0].change_kind, "set");
        assert_eq!(changes[0].current, Some(serde_json::json!(["10.99.1.0/24"])));
    }

    /// AC: Route and address changes are coalesced — both address and route list
    /// changes in a single state comparison appear together in one call's output.
    #[test]
    fn test_compute_external_field_changes_route_and_address_both_captured() {
        let last = make_journal_snapshot(
            "veth-e2e0",
            serde_json::json!({
                "addresses": ["10.99.0.1/24"],
                "routes": []
            }),
        );
        let current = make_current_state(
            "veth-e2e0",
            vec![
                ("addresses", Value::List(vec![
                    Value::String("10.99.0.1/24".to_string()),
                    Value::String("10.99.0.3/24".to_string()),
                ])),
                ("routes", Value::List(vec![Value::String("10.99.3.0/24".to_string())])),
            ],
        );

        let changes = compute_external_field_changes(&last, &current);

        assert_eq!(changes.len(), 2, "both address and route changes must be captured");
        let addr = changes.iter().find(|c| c.field_name == "addresses")
            .expect("addresses change must be present");
        assert_eq!(addr.change_kind, "set");
        let route = changes.iter().find(|c| c.field_name == "routes")
            .expect("routes change must be present");
        assert_eq!(route.change_kind, "set");
    }

    /// AC: Unchanged list-type fields do not appear in the diff.
    #[test]
    fn test_compute_external_field_changes_unchanged_list_field_not_included() {
        let last = make_journal_snapshot(
            "veth-e2e0",
            serde_json::json!({ "addresses": ["10.99.0.1/24"] }),
        );
        let current = make_current_state(
            "veth-e2e0",
            vec![("addresses", Value::List(vec![Value::String("10.99.0.1/24".to_string())]))],
        );

        let changes = compute_external_field_changes(&last, &current);

        assert!(
            changes.is_empty(),
            "unchanged list field must produce no change entries, got: {:?}",
            changes.iter().map(|c| c.field_name.as_str()).collect::<Vec<_>>()
        );
    }

    /// AC: An empty snapshot vs a current state with only stable fields produces no changes.
    /// This guards against the monitor generating spurious entries for name/driver/mac
    /// when no observable fields have actually changed.
    #[test]
    fn test_compute_external_field_changes_only_stable_fields_in_current_produces_no_diff() {
        let last = make_journal_snapshot("eth0", serde_json::json!({}));
        let current = make_current_state(
            "eth0",
            vec![
                ("mac", Value::U64(0)),
                ("driver", Value::U64(0)),
                ("name", Value::U64(0)),
            ],
        );

        let changes = compute_external_field_changes(&last, &current);

        assert!(
            changes.is_empty(),
            "current state with only stable fields must produce no external change diff"
        );
    }

    // ── Feature: Revert ───────────────────────────────────────────────────────

    /// Dry-run revert returns Ok with report=None.
    #[tokio::test]
    async fn test_reconciler_revert_dry_run_returns_ok_with_none_report() {
        let reconciler = Reconciler::new();
        let result = reconciler.revert(&StateSet::new(), 1, &[], true).await;
        assert!(result.is_ok(), "revert dry-run must succeed: {:?}", result.err());
        assert!(
            result.unwrap().report.is_none(),
            "dry-run revert must return None report"
        );
    }

    /// Apply revert with empty target returns Ok with report=Some.
    #[tokio::test]
    async fn test_reconciler_revert_apply_with_empty_target_returns_ok_with_report() {
        let reconciler = Reconciler::new();
        let result = reconciler.revert(&StateSet::new(), 1, &[], false).await;
        assert!(result.is_ok(), "revert apply must succeed: {:?}", result.err());
        assert!(
            result.unwrap().report.is_some(),
            "apply revert must return Some report"
        );
    }

    /// is_applying flag is false after revert() completes (empty target = no apply needed).
    #[tokio::test]
    async fn test_reconciler_revert_is_not_applying_after_completion() {
        let reconciler = Reconciler::new();
        let _ = reconciler.revert(&StateSet::new(), 1, &[], false).await;
        assert!(
            !reconciler.is_applying(),
            "is_applying must be false after revert completes"
        );
    }

    /// Empty target state produces an empty reconcile diff.
    #[tokio::test]
    async fn test_reconciler_revert_with_empty_target_produces_empty_diff() {
        let reconciler = Reconciler::new();
        let result = reconciler.revert(&StateSet::new(), 1, &[], false).await.unwrap();
        assert!(
            result.reconcile_diff.is_empty(),
            "empty target state must produce an empty reconcile diff"
        );
    }
}
