//! netfyr-reconcile crate — per-field priority merge for network policy reconciliation.
//!
//! # Design decisions
//!
//! - **Per-field, not per-entity.** Reconciliation resolves each field
//!   independently so that two teams can manage different fields on the same
//!   interface (e.g. one team sets MTU while another manages addresses)
//!   without requiring a single combined policy.
//!
//! - **Conflicts omit, not tiebreak.** When two policies at the same priority
//!   set the same field to different values, the field is omitted from the
//!   effective state and reported as a [`Conflict`]. This avoids silent wrong
//!   behaviour — the user must resolve the ambiguity by adjusting priorities
//!   or removing the duplicate.
//!
//! - **Order-insensitive list comparison.** List fields (e.g. addresses) are
//!   compared as multisets for conflict detection. `["10.0.0.1/24",
//!   "10.0.0.2/24"]` and `["10.0.0.2/24", "10.0.0.1/24"]` are considered
//!   equal, preventing spurious conflicts when two policies configure the
//!   same set of addresses in different order.

pub mod conflict;
pub mod diff;
pub mod report;

pub use conflict::{Conflict, ConflictContribution, ConflictReport, values_equal_for_conflict};
pub use diff::{generate_diff, DiffKind, DiffOperation, FieldChange, FieldChangeKind, StateDiff};
pub use report::DiffReport;

use std::collections::HashMap;
use std::fmt;

use indexmap::IndexMap;
use netfyr_state::{FieldValue, Provenance, Selector, State, StateMetadata, StateSet, Value};

// ── PolicyId ──────────────────────────────────────────────────────────────────

/// Unique identifier for a policy.
///
/// A newtype over `String` that prevents accidentally mixing policy IDs with
/// arbitrary strings while deriving all traits needed for use as a map key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PolicyId(pub String);

impl PolicyId {
    /// Returns the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PolicyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for PolicyId {
    fn from(s: String) -> Self {
        PolicyId(s)
    }
}

impl From<&str> for PolicyId {
    fn from(s: &str) -> Self {
        PolicyId(s.to_owned())
    }
}

// ── EntityKey / FieldName ─────────────────────────────────────────────────────

/// Canonical key for an entity: `(entity_type, selector.key())`.
///
/// Aligns with the existing `StateSet` keying convention.
pub type EntityKey = (String, String);

/// A field name (e.g., `"mtu"`, `"addresses"`).
pub type FieldName = String;

// ── PolicyInput ───────────────────────────────────────────────────────────────

/// Input to the reconciliation engine from a single policy.
#[derive(Clone, Debug)]
pub struct PolicyInput {
    /// Unique identifier for this policy.
    pub policy_id: PolicyId,
    /// Priority of this policy. Higher numbers win in per-field priority resolution.
    /// The conventional default is 100.
    pub priority: u32,
    /// The state set produced by this policy.
    pub state_set: StateSet,
}

// ── ReconciliationResult ──────────────────────────────────────────────────────

/// The output of the reconciliation engine.
#[derive(Clone, Debug)]
pub struct ReconciliationResult {
    /// The merged desired state of the entire system.
    pub effective_state: StateSet,
    /// Maps `((entity_type, selector_key), field_name)` to the policy that
    /// provided the winning value for that field.
    ///
    /// Conflicted fields (omitted from `effective_state`) are absent from this map.
    pub field_sources: HashMap<(EntityKey, FieldName), PolicyId>,
    /// Field conflicts detected during reconciliation.
    pub conflicts: ConflictReport,
}

// ── Merge algorithm ───────────────────────────────────────────────────────────

/// Protocol sub-object field names that receive recursive per-sub-key priority merge.
///
/// Route entry maps (e.g. `{destination: "...", gateway: "..."}`) are opaque values and
/// must NOT be in this list — decomposing them would break their semantics.
const PROTOCOL_SUB_OBJECTS: &[&str] = &["ipv4", "ipv6"];

/// Return type from [`resolve_sub_fields`]: merged value, source entries, and sub-conflicts.
type SubFieldResult = (Option<FieldValue>, Vec<((EntityKey, FieldName), PolicyId)>, Vec<Conflict>);

/// Performs per-sub-key priority resolution for a protocol sub-object field.
///
/// Returns `None` when any contender's value is not a `Value::Map`, signalling the
/// caller to fall back to opaque (whole-field) resolution.
///
/// On success returns `(merged_fv, sources, sub_conflicts)`:
/// - `merged_fv`: the merged `FieldValue` (or `None` if all sub-keys conflicted).
/// - `sources`: `field_sources` entries using dotted paths (`"ipv4.addresses"`, …).
/// - `sub_conflicts`: any sub-key conflicts (field_name is the dotted path).
fn resolve_sub_fields(
    field_name: &str,
    entity_key: &EntityKey,
    contenders: &[(PolicyId, u32, FieldValue)],
) -> Option<SubFieldResult> {
    // Guard: every contender must be a Value::Map, otherwise fall back to opaque resolution.
    if !contenders.iter().all(|(_, _, fv)| matches!(fv.value, Value::Map(_))) {
        return None;
    }

    // Collect per-sub-key contenders across ALL policies (not just top-priority),
    // because a lower-priority policy may be the sole provider of a sub-key.
    let mut sub_key_contenders: HashMap<String, Vec<(PolicyId, u32, Value, Provenance)>> =
        HashMap::new();
    for (policy_id, priority, fv) in contenders {
        if let Value::Map(map) = &fv.value {
            for (sub_key, sub_val) in map {
                sub_key_contenders.entry(sub_key.clone()).or_default().push((
                    policy_id.clone(),
                    *priority,
                    sub_val.clone(),
                    fv.provenance.clone(),
                ));
            }
        }
    }

    let mut merged_map: IndexMap<String, Value> = IndexMap::new();
    let mut sources: Vec<((EntityKey, FieldName), PolicyId)> = Vec::new();
    let mut sub_conflicts: Vec<Conflict> = Vec::new();

    // Process sub-keys in sorted order for deterministic output.
    let mut sub_keys: Vec<&String> = sub_key_contenders.keys().collect();
    sub_keys.sort();

    for sub_key in sub_keys {
        let sub_contenders = &sub_key_contenders[sub_key];
        let dotted = format!("{}.{}", field_name, sub_key);

        let max_priority = sub_contenders.iter().map(|(_, p, _, _)| *p).max().unwrap_or(0);

        let top: Vec<&(PolicyId, u32, Value, Provenance)> =
            sub_contenders.iter().filter(|(_, p, _, _)| *p == max_priority).collect();

        let first_val = &top[0].2;
        let all_agree = top.iter().all(|(_, _, v, _)| values_equal_for_conflict(v, first_val));

        if all_agree {
            merged_map.insert(sub_key.clone(), first_val.clone());
            sources.push(((entity_key.clone(), dotted), top[0].0.clone()));
        } else {
            let contributions: Vec<ConflictContribution> = top
                .iter()
                .map(|(pid, _, v, prov)| ConflictContribution {
                    policy_id: pid.clone(),
                    value: FieldValue { value: v.clone(), provenance: prov.clone() },
                })
                .collect();
            sub_conflicts.push(Conflict {
                entity_key: entity_key.clone(),
                field_name: dotted,
                priority: max_priority,
                contributions,
            });
        }
    }

    // Provenance for the outer FieldValue: use the highest-priority contender's provenance.
    let best_provenance = contenders
        .iter()
        .max_by_key(|(_, p, _)| *p)
        .map(|(_, _, fv)| fv.provenance.clone())
        .unwrap_or(Provenance::KernelDefault);

    if merged_map.is_empty() {
        Some((None, sources, sub_conflicts))
    } else {
        Some((
            Some(FieldValue { value: Value::Map(merged_map), provenance: best_provenance }),
            sources,
            sub_conflicts,
        ))
    }
}

/// Merges N policy inputs into a single effective `StateSet` using per-field priority.
///
/// # Algorithm
///
/// 1. **Collect**: iterate every `PolicyInput`'s `StateSet`, grouping all field
///    contenders by entity key `(entity_type, selector.key())`.
/// 2. **Resolve**: for each entity, iterate each field name and pick the winner:
///    - Highest priority wins.
///    - Tie at the same priority with the **same value**: first policy (by input
///      order) is recorded in `field_sources`; no conflict is raised.
///    - Tie at the same priority with **different values**: a `Conflict` is
///      recorded and the field is **omitted** from the effective state.
/// 3. Build the effective `StateSet` from all winning fields and return a
///    `ReconciliationResult`.
pub fn merge(inputs: Vec<PolicyInput>) -> ReconciliationResult {
    if inputs.is_empty() {
        return ReconciliationResult {
            effective_state: StateSet::new(),
            field_sources: HashMap::new(),
            conflicts: ConflictReport::new(),
        };
    }

    // Phase 1 ── collect per-entity data.
    //
    // For each entity key we track:
    //   - The `Selector` (from the first state seen for that entity).
    //   - The maximum policy priority among all contributing policies.
    //   - Per-field: Vec<(PolicyId, policy_priority, FieldValue)>.
    type FieldContenders = Vec<(PolicyId, u32, FieldValue)>;

    struct EntityData {
        selector: Selector,
        max_policy_priority: u32,
        fields: HashMap<FieldName, FieldContenders>,
    }

    let mut entity_map: HashMap<EntityKey, EntityData> = HashMap::new();

    for input in &inputs {
        for state in input.state_set.iter() {
            let key: EntityKey = (state.entity_type.clone(), state.selector.key());

            let entry = entity_map.entry(key).or_insert_with(|| EntityData {
                selector: state.selector.clone(),
                max_policy_priority: 0,
                fields: HashMap::new(),
            });

            // Track the highest contributing policy priority for this entity.
            entry.max_policy_priority = entry.max_policy_priority.max(input.priority);

            // Accumulate per-field contenders.
            for (field_name, field_value) in &state.fields {
                entry
                    .fields
                    .entry(field_name.clone())
                    .or_default()
                    .push((input.policy_id.clone(), input.priority, field_value.clone()));
            }
        }
    }

    // Phase 2 ── resolve per-entity, per-field.
    let mut effective_state = StateSet::new();
    let mut field_sources: HashMap<(EntityKey, FieldName), PolicyId> = HashMap::new();
    let mut conflict_list: Vec<Conflict> = Vec::new();

    for (entity_key, entity_data) in entity_map {
        // Process field names in sorted order so the merged State's fields are
        // in a deterministic order (alphabetical by field name).
        let mut field_names: Vec<&FieldName> = entity_data.fields.keys().collect();
        field_names.sort();

        let mut merged_state = State {
            entity_type: entity_key.0.clone(),
            selector: entity_data.selector,
            fields: Default::default(),
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: entity_data.max_policy_priority,
        };

        for field_name in field_names {
            let contenders = &entity_data.fields[field_name];

            // Protocol sub-objects (ipv4, ipv6) get per-sub-key priority resolution
            // instead of opaque whole-field comparison.
            if PROTOCOL_SUB_OBJECTS.contains(&field_name.as_str()) {
                if let Some((merged_fv, sub_sources, sub_conflicts)) =
                    resolve_sub_fields(field_name, &entity_key, contenders)
                {
                    if let Some(fv) = merged_fv {
                        merged_state.fields.insert(field_name.clone(), fv);
                    }
                    field_sources.extend(sub_sources);
                    conflict_list.extend(sub_conflicts);
                    continue;
                }
                // resolve_sub_fields returned None (a contender was not a Value::Map);
                // fall through to opaque resolution below.
            }

            // Find the maximum priority among all contenders for this field.
            let max_priority = contenders
                .iter()
                .map(|(_, p, _)| *p)
                .max()
                .unwrap_or(0);

            // Keep only the contenders at the maximum priority.
            let top: Vec<&(PolicyId, u32, FieldValue)> = contenders
                .iter()
                .filter(|(_, p, _)| *p == max_priority)
                .collect();

            let first_value: &Value = &top[0].2.value;
            let all_agree =
                top.iter().all(|(_, _, fv)| values_equal_for_conflict(&fv.value, first_value));

            if all_agree {
                // Single winner or unanimous tie — first by input order wins.
                let (winner_id, _, winner_fv) = &top[0];
                merged_state.fields.insert(field_name.clone(), winner_fv.clone());
                field_sources
                    .insert((entity_key.clone(), field_name.clone()), winner_id.clone());
            } else {
                // Irreconcilable conflict — omit the field from effective state.
                let contributions: Vec<ConflictContribution> = top
                    .iter()
                    .map(|(pid, _, fv)| ConflictContribution {
                        policy_id: (*pid).clone(),
                        value: (*fv).clone(),
                    })
                    .collect();
                conflict_list.push(Conflict {
                    entity_key: entity_key.clone(),
                    field_name: field_name.clone(),
                    priority: max_priority,
                    contributions,
                });
            }
        }

        effective_state.insert(merged_state);
    }

    ReconciliationResult {
        effective_state,
        field_sources,
        conflicts: ConflictReport {
            conflicts: conflict_list,
        },
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{
        merge, values_equal_for_conflict, Conflict, ConflictContribution, ConflictReport, PolicyId,
        PolicyInput, ReconciliationResult,
    };
    use netfyr_state::{FieldValue, Provenance, Selector, State, StateMetadata, StateSet, Value};

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Builds a `FieldValue` with `KernelDefault` provenance (sufficient for merge tests).
    fn fv(v: Value) -> FieldValue {
        FieldValue {
            value: v,
            provenance: Provenance::KernelDefault,
        }
    }

    fn make_fv(v: Value) -> FieldValue {
        FieldValue { value: v, provenance: Provenance::KernelDefault }
    }

    /// Builds a `State` for a named entity without requiring a direct `indexmap` import.
    fn make_state(entity_type: &str, name: &str, fields: Vec<(&str, Value)>) -> State {
        let mut s = State {
            entity_type: entity_type.to_string(),
            selector: Selector::with_name(name),
            fields: Default::default(),
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 0,
        };
        for (k, v) in fields {
            s.fields.insert(k.to_string(), fv(v));
        }
        s
    }

    /// Wraps states into a `PolicyInput`.
    fn make_input(id: &str, priority: u32, states: Vec<State>) -> PolicyInput {
        let mut ss = StateSet::new();
        for s in states {
            ss.insert(s);
        }
        PolicyInput {
            policy_id: PolicyId::from(id),
            priority,
            state_set: ss,
        }
    }

    /// Looks up which policy won a given field on a given entity.
    fn get_source<'a>(
        result: &'a ReconciliationResult,
        entity_type: &str,
        selector_key: &str,
        field: &str,
    ) -> Option<&'a PolicyId> {
        result.field_sources.get(&(
            (entity_type.to_string(), selector_key.to_string()),
            field.to_string(),
        ))
    }

    fn make_contribution(policy_id: &str, v: Value) -> ConflictContribution {
        ConflictContribution { policy_id: PolicyId::from(policy_id), value: make_fv(v) }
    }

    fn make_conflict(
        entity_type: &str,
        selector_key: &str,
        field_name: &str,
        priority: u32,
        contributions: Vec<ConflictContribution>,
    ) -> Conflict {
        Conflict {
            entity_key: (entity_type.to_string(), selector_key.to_string()),
            field_name: field_name.to_string(),
            priority,
            contributions,
        }
    }

    // ── Scenario: Single policy produces effective state unchanged ────────────

    #[test]
    fn test_single_policy_produces_effective_state_unchanged() {
        let addresses = Value::List(vec![Value::String("10.0.1.50/24".to_string())]);
        let input = make_input(
            "eth0-config",
            100,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![("mtu", Value::U64(1500)), ("addresses", addresses.clone())],
            )],
        );

        let result = merge(vec![input]);

        let eth0 = result
            .effective_state
            .get("ethernet", "eth0")
            .expect("ethernet/eth0 should be in effective state");
        assert_eq!(eth0.fields["mtu"].value, Value::U64(1500));
        assert_eq!(eth0.fields["addresses"].value, addresses);

        assert_eq!(
            get_source(&result, "ethernet", "eth0", "mtu").map(|p| p.as_str()),
            Some("eth0-config"),
            "mtu should be attributed to eth0-config"
        );
        assert_eq!(
            get_source(&result, "ethernet", "eth0", "addresses").map(|p| p.as_str()),
            Some("eth0-config"),
            "addresses should be attributed to eth0-config"
        );
    }

    // ── Scenario: Two policies contribute different fields to the same entity ─

    #[test]
    fn test_two_policies_contribute_different_fields_to_same_entity() {
        let addresses = Value::List(vec![Value::String("10.0.1.50/24".to_string())]);
        let base = make_input(
            "eth0-base",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))])],
        );
        let dhcp = make_input(
            "eth0-dhcp",
            100,
            vec![make_state("ethernet", "eth0", vec![("addresses", addresses.clone())])],
        );

        let result = merge(vec![base, dhcp]);

        let eth0 = result
            .effective_state
            .get("ethernet", "eth0")
            .expect("ethernet/eth0 should be in effective state");
        assert_eq!(eth0.fields["mtu"].value, Value::U64(1500), "mtu from eth0-base");
        assert_eq!(eth0.fields["addresses"].value, addresses, "addresses from eth0-dhcp");

        assert_eq!(
            get_source(&result, "ethernet", "eth0", "mtu").map(|p| p.as_str()),
            Some("eth0-base"),
        );
        assert_eq!(
            get_source(&result, "ethernet", "eth0", "addresses").map(|p| p.as_str()),
            Some("eth0-dhcp"),
        );
    }

    // ── Scenario: Higher priority policy overrides a field from lower priority ─

    #[test]
    fn test_higher_priority_policy_overrides_field_from_lower_priority() {
        let base = make_input(
            "eth0-base",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))])],
        );
        let override_p = make_input(
            "eth0-override",
            200,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))])],
        );

        let result = merge(vec![base, override_p]);

        let eth0 = result
            .effective_state
            .get("ethernet", "eth0")
            .expect("ethernet/eth0 should be in effective state");
        assert_eq!(
            eth0.fields["mtu"].value,
            Value::U64(9000),
            "higher-priority policy (200) must override lower-priority (100)"
        );
        assert_eq!(
            get_source(&result, "ethernet", "eth0", "mtu").map(|p| p.as_str()),
            Some("eth0-override"),
            "mtu must be attributed to the overriding policy"
        );
    }

    // ── Scenario: Higher priority overrides only conflicting fields, not all ──

    #[test]
    fn test_higher_priority_overrides_only_conflicting_fields_not_all() {
        let addresses = Value::List(vec![Value::String("10.0.1.50/24".to_string())]);
        let base = make_input(
            "eth0-base",
            100,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![("mtu", Value::U64(1500)), ("addresses", addresses.clone())],
            )],
        );
        let override_p = make_input(
            "eth0-override",
            200,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))])],
        );

        let result = merge(vec![base, override_p]);

        let eth0 = result
            .effective_state
            .get("ethernet", "eth0")
            .expect("ethernet/eth0 should be in effective state");
        assert_eq!(eth0.fields["mtu"].value, Value::U64(9000), "mtu overridden by higher priority");
        assert_eq!(
            eth0.fields["addresses"].value, addresses,
            "addresses not overridden; should remain from base policy"
        );
    }

    // ── Scenario: Three policies with cascading priorities ────────────────────

    #[test]
    fn test_three_policies_with_cascading_priorities() {
        let default_addrs = Value::List(vec![Value::String("10.0.0.1/24".to_string())]);
        let emergency_addrs = Value::List(vec![Value::String("192.168.1.1/24".to_string())]);

        let default_p = make_input(
            "default",
            50,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![("mtu", Value::U64(1500)), ("addresses", default_addrs)],
            )],
        );
        let team_p = make_input(
            "team",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))])],
        );
        let emergency_p = make_input(
            "emergency",
            200,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![("addresses", emergency_addrs.clone())],
            )],
        );

        let result = merge(vec![default_p, team_p, emergency_p]);

        let eth0 = result
            .effective_state
            .get("ethernet", "eth0")
            .expect("ethernet/eth0 should be in effective state");

        assert_eq!(
            eth0.fields["mtu"].value,
            Value::U64(9000),
            "mtu: team (100) beats default (50)"
        );
        assert_eq!(
            eth0.fields["addresses"].value,
            emergency_addrs,
            "addresses: emergency (200) beats default (50)"
        );

        assert_eq!(
            get_source(&result, "ethernet", "eth0", "mtu").map(|p| p.as_str()),
            Some("team"),
        );
        assert_eq!(
            get_source(&result, "ethernet", "eth0", "addresses").map(|p| p.as_str()),
            Some("emergency"),
        );
    }

    // ── Scenario: Policies targeting different entities do not interact ────────

    #[test]
    fn test_policies_targeting_different_entities_do_not_interact() {
        let eth0_config = make_input(
            "eth0-config",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))])],
        );
        let eth1_config = make_input(
            "eth1-config",
            100,
            vec![make_state("ethernet", "eth1", vec![("mtu", Value::U64(9000))])],
        );

        let result = merge(vec![eth0_config, eth1_config]);

        assert_eq!(result.effective_state.len(), 2, "effective state should contain exactly 2 entities");

        let eth0 = result
            .effective_state
            .get("ethernet", "eth0")
            .expect("ethernet/eth0 should be present");
        assert_eq!(eth0.fields["mtu"].value, Value::U64(1500));

        let eth1 = result
            .effective_state
            .get("ethernet", "eth1")
            .expect("ethernet/eth1 should be present");
        assert_eq!(eth1.fields["mtu"].value, Value::U64(9000));
    }

    // ── Scenario: Same priority, same value is not a conflict ─────────────────

    #[test]
    fn test_same_priority_same_value_is_not_a_conflict() {
        let policy_a = make_input(
            "policy-a",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))])],
        );
        let policy_b = make_input(
            "policy-b",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))])],
        );

        let result = merge(vec![policy_a, policy_b]);

        let eth0 = result
            .effective_state
            .get("ethernet", "eth0")
            .expect("ethernet/eth0 should be in effective state");
        assert_eq!(eth0.fields["mtu"].value, Value::U64(1500));
        assert!(
            result.conflicts.is_empty(),
            "agreeing values at equal priority must not produce a conflict; got {:?}",
            result.conflicts.conflicts
        );
    }

    // ── Scenario: Empty policy input produces empty effective state ────────────

    #[test]
    fn test_empty_policy_input_produces_empty_effective_state() {
        let result = merge(vec![]);

        assert!(result.effective_state.is_empty(), "effective state should be empty");
        assert!(result.field_sources.is_empty(), "field_sources should be empty");
        assert!(result.conflicts.is_empty(), "conflicts should be empty");
    }

    // ── Scenario: Policy with multiple entities ───────────────────────────────

    #[test]
    fn test_policy_with_multiple_entities_all_appear_in_effective_state() {
        let addresses = Value::List(vec![Value::String("10.0.1.50/24".to_string())]);
        let servers = Value::List(vec![Value::String("10.0.1.2".to_string())]);
        let input = make_input(
            "network-config",
            100,
            vec![
                make_state(
                    "ethernet",
                    "eth0",
                    vec![("mtu", Value::U64(1500)), ("addresses", addresses.clone())],
                ),
                make_state("ethernet", "eth1", vec![("mtu", Value::U64(9000))]),
                make_state("dns", "global", vec![("servers", servers.clone())]),
            ],
        );

        let result = merge(vec![input]);

        assert_eq!(result.effective_state.len(), 3, "all 3 entities should appear");

        let eth0 = result.effective_state.get("ethernet", "eth0").expect("eth0");
        assert_eq!(eth0.fields["mtu"].value, Value::U64(1500));
        assert_eq!(eth0.fields["addresses"].value, addresses);

        let eth1 = result.effective_state.get("ethernet", "eth1").expect("eth1");
        assert_eq!(eth1.fields["mtu"].value, Value::U64(9000));

        let dns = result.effective_state.get("dns", "global").expect("dns/global");
        assert_eq!(dns.fields["servers"].value, servers);
    }

    // ── Scenario: Lower priority policy fields included when not overridden ───

    #[test]
    fn test_lower_priority_policy_fields_included_when_not_overridden() {
        let addresses = Value::List(vec![Value::String("10.0.1.50/24".to_string())]);
        let routes = Value::List(vec![Value::String("default via 10.0.1.1".to_string())]);
        let base = make_input(
            "base",
            50,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![
                    ("mtu", Value::U64(1500)),
                    ("addresses", addresses.clone()),
                    ("routes", routes.clone()),
                ],
            )],
        );
        let overlay = make_input(
            "overlay",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))])],
        );

        let result = merge(vec![base, overlay]);

        let eth0 = result
            .effective_state
            .get("ethernet", "eth0")
            .expect("ethernet/eth0 should be in effective state");

        assert_eq!(eth0.fields["mtu"].value, Value::U64(9000), "mtu overridden");
        assert_eq!(eth0.fields["addresses"].value, addresses, "addresses kept from base");
        assert_eq!(eth0.fields["routes"].value, routes, "routes kept from base");

        assert_eq!(
            get_source(&result, "ethernet", "eth0", "mtu").map(|p| p.as_str()),
            Some("overlay"),
        );
        assert_eq!(
            get_source(&result, "ethernet", "eth0", "addresses").map(|p| p.as_str()),
            Some("base"),
        );
        assert_eq!(
            get_source(&result, "ethernet", "eth0", "routes").map(|p| p.as_str()),
            Some("base"),
        );
    }

    // ── Extra: same priority, different values → conflict, field omitted ───────

    #[test]
    fn test_same_priority_different_values_reports_conflict_and_omits_field() {
        let policy_a = make_input(
            "policy-a",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))])],
        );
        let policy_b = make_input(
            "policy-b",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))])],
        );

        let result = merge(vec![policy_a, policy_b]);

        // Conflicted field must be absent from field_sources.
        assert!(
            get_source(&result, "ethernet", "eth0", "mtu").is_none(),
            "conflicted field must not appear in field_sources"
        );

        // If the entity appears in the effective state, mtu must be absent.
        if let Some(eth0) = result.effective_state.get("ethernet", "eth0") {
            assert!(
                !eth0.fields.contains_key("mtu"),
                "conflicted mtu field must be omitted from effective state"
            );
        }

        // A conflict must be recorded.
        assert_eq!(result.conflicts.len(), 1, "exactly one conflict should be reported");
        let conflict = &result.conflicts.conflicts[0];
        assert_eq!(conflict.entity_key, ("ethernet".to_string(), "eth0".to_string()));
        assert_eq!(conflict.field_name, "mtu");
        // Both contending values must be present.
        let values: Vec<&Value> = conflict.contributions.iter().map(|c| &c.value.value).collect();
        assert!(values.contains(&&Value::U64(1500)));
        assert!(values.contains(&&Value::U64(9000)));
    }

    // ── Extra: field_sources is absent for conflicted fields ──────────────────

    #[test]
    fn test_field_sources_does_not_include_conflicted_fields() {
        let policy_a = make_input(
            "policy-a",
            100,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![("mtu", Value::U64(1500)), ("speed", Value::U64(1000))],
            )],
        );
        let policy_b = make_input(
            "policy-b",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))])],
        );

        let result = merge(vec![policy_a, policy_b]);

        // mtu is in conflict — absent from field_sources.
        assert!(
            get_source(&result, "ethernet", "eth0", "mtu").is_none(),
            "conflicted mtu must not appear in field_sources"
        );
        // speed is uncontested — must appear in field_sources.
        assert_eq!(
            get_source(&result, "ethernet", "eth0", "speed").map(|p| p.as_str()),
            Some("policy-a"),
            "uncontested speed field must appear in field_sources"
        );
    }

    // ── Extra: single policy, no conflict report ──────────────────────────────

    #[test]
    fn test_single_policy_produces_no_conflicts() {
        let input = make_input(
            "only-policy",
            100,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![("mtu", Value::U64(1500)), ("speed", Value::U64(1000))],
            )],
        );

        let result = merge(vec![input]);

        assert!(result.conflicts.is_empty(), "a single policy must produce no conflicts");
    }

    // ── SPEC-202: Conflict Detection Tests ────────────────────────────────────

    // Scenario 1: Two policies conflict on same field at same priority —
    //   a Conflict is reported for the entity, field, and priority.
    #[test]
    fn test_conflict_reported_for_same_field_same_priority_different_values() {
        let team_a = make_input(
            "team-a",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))])],
        );
        let team_b = make_input(
            "team-b",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))])],
        );

        let result = merge(vec![team_a, team_b]);

        assert_eq!(result.conflicts.len(), 1, "exactly one conflict expected for mtu");
        let conflict = &result.conflicts.conflicts[0];
        assert_eq!(
            conflict.entity_key,
            ("ethernet".to_string(), "eth0".to_string()),
            "conflict entity_key must be ethernet/eth0"
        );
        assert_eq!(conflict.field_name, "mtu", "conflict field_name must be 'mtu'");
        assert_eq!(conflict.priority, 100, "conflict priority must be 100");
    }

    // Scenario 1 (detailed): the conflict lists both policy ids and their values.
    #[test]
    fn test_conflict_lists_both_policy_ids_and_values() {
        let team_a = make_input(
            "team-a",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))])],
        );
        let team_b = make_input(
            "team-b",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))])],
        );

        let result = merge(vec![team_a, team_b]);

        let conflict = &result.conflicts.conflicts[0];
        assert_eq!(conflict.contributions.len(), 2);

        let policy_ids: Vec<&str> =
            conflict.contributions.iter().map(|c| c.policy_id.as_str()).collect();
        assert!(policy_ids.contains(&"team-a"), "conflict must list policy 'team-a'");
        assert!(policy_ids.contains(&"team-b"), "conflict must list policy 'team-b'");

        let values: Vec<&Value> =
            conflict.contributions.iter().map(|c| &c.value.value).collect();
        assert!(values.contains(&&Value::U64(9000)), "conflict must list value 9000");
        assert!(values.contains(&&Value::U64(1500)), "conflict must list value 1500");
    }

    // Scenario 1 (exclusion): conflicted field must NOT appear in effective state.
    #[test]
    fn test_conflicted_field_excluded_from_effective_state() {
        let team_a = make_input(
            "team-a",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))])],
        );
        let team_b = make_input(
            "team-b",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))])],
        );

        let result = merge(vec![team_a, team_b]);

        if let Some(eth0) = result.effective_state.get("ethernet", "eth0") {
            assert!(
                !eth0.fields.contains_key("mtu"),
                "conflicted 'mtu' must be absent from the effective state"
            );
        }
        assert!(
            get_source(&result, "ethernet", "eth0", "mtu").is_none(),
            "conflicted field must not appear in field_sources"
        );
    }

    // Scenario 2: Same value at same priority is not a conflict; effective state gets the value.
    #[test]
    fn test_same_value_same_priority_not_a_conflict_effective_state_has_value() {
        let team_a = make_input(
            "team-a",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))])],
        );
        let team_b = make_input(
            "team-b",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))])],
        );

        let result = merge(vec![team_a, team_b]);

        assert!(
            result.conflicts.is_empty(),
            "identical values at same priority must not produce a conflict"
        );
        let eth0 = result
            .effective_state
            .get("ethernet", "eth0")
            .expect("eth0 must be in effective state");
        assert_eq!(
            eth0.fields["mtu"].value,
            Value::U64(1500),
            "effective state must have mtu=1500"
        );
    }

    // Scenario 3: Higher priority resolves what would otherwise be a conflict.
    #[test]
    fn test_higher_priority_resolves_conflict_no_conflict_reported() {
        let base = make_input(
            "base",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))])],
        );
        let override_p = make_input(
            "override",
            200,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))])],
        );

        let result = merge(vec![base, override_p]);

        assert!(
            result.conflicts.is_empty(),
            "higher-priority policy must resolve without producing a conflict"
        );
        let eth0 =
            result.effective_state.get("ethernet", "eth0").expect("eth0 must be in effective state");
        assert_eq!(
            eth0.fields["mtu"].value,
            Value::U64(9000),
            "effective state must have mtu=9000 (higher priority wins)"
        );
    }

    // Scenario 4: A conflict on one field does not affect non-conflicting fields.
    #[test]
    fn test_conflict_on_one_field_does_not_affect_other_fields() {
        let addresses = Value::List(vec![Value::String("10.0.1.50/24".to_string())]);
        let team_a = make_input(
            "team-a",
            100,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![("mtu", Value::U64(9000)), ("addresses", addresses.clone())],
            )],
        );
        let team_b = make_input(
            "team-b",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))])],
        );

        let result = merge(vec![team_a, team_b]);

        // mtu is in conflict
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts.conflicts[0].field_name, "mtu");

        let eth0 = result
            .effective_state
            .get("ethernet", "eth0")
            .expect("eth0 must be in effective state");
        assert!(
            !eth0.fields.contains_key("mtu"),
            "conflicted 'mtu' must be excluded from effective state"
        );
        assert_eq!(
            eth0.fields["addresses"].value,
            addresses,
            "non-conflicting 'addresses' must remain in effective state"
        );
    }

    // Scenario 5: Three-way conflict at same priority.
    #[test]
    fn test_three_way_conflict_at_same_priority() {
        let a = make_input(
            "a",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))])],
        );
        let b = make_input(
            "b",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))])],
        );
        let c = make_input(
            "c",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(4500))])],
        );

        let result = merge(vec![a, b, c]);

        assert_eq!(result.conflicts.len(), 1, "exactly one conflict for mtu");
        let conflict = &result.conflicts.conflicts[0];
        assert_eq!(conflict.contributions.len(), 3, "three contributions in the conflict");

        let policy_ids: Vec<&str> =
            conflict.contributions.iter().map(|c| c.policy_id.as_str()).collect();
        assert!(policy_ids.contains(&"a"), "policy 'a' must be in contributions");
        assert!(policy_ids.contains(&"b"), "policy 'b' must be in contributions");
        assert!(policy_ids.contains(&"c"), "policy 'c' must be in contributions");

        let values: Vec<u64> =
            conflict.contributions.iter().filter_map(|c| c.value.value.as_u64()).collect();
        assert!(values.contains(&1500), "value 1500 must be in contributions");
        assert!(values.contains(&9000), "value 9000 must be in contributions");
        assert!(values.contains(&4500), "value 4500 must be in contributions");

        if let Some(eth0) = result.effective_state.get("ethernet", "eth0") {
            assert!(
                !eth0.fields.contains_key("mtu"),
                "conflicted mtu must be absent from effective state"
            );
        }
    }

    // Scenario 6: Conflict at lower priority does not matter when a higher priority wins.
    #[test]
    fn test_lower_priority_conflict_irrelevant_when_higher_priority_wins() {
        let low_a = make_input(
            "low-a",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))])],
        );
        let low_b = make_input(
            "low-b",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))])],
        );
        let high = make_input(
            "high",
            200,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(4500))])],
        );

        let result = merge(vec![low_a, low_b, high]);

        assert!(
            result.conflicts.is_empty(),
            "priority 200 wins outright; no conflict should be reported"
        );
        let eth0 = result
            .effective_state
            .get("ethernet", "eth0")
            .expect("eth0 must be in effective state");
        assert_eq!(
            eth0.fields["mtu"].value,
            Value::U64(4500),
            "effective state must have mtu=4500 (highest priority wins)"
        );
    }

    // Scenario 7: List fields use set (order-insensitive) comparison — same elements, different
    //   order must not produce a conflict.
    #[test]
    fn test_list_fields_same_values_different_order_not_a_conflict() {
        let a = make_input(
            "a",
            100,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![("addresses", Value::List(vec![
                    Value::String("10.0.1.50/24".to_string()),
                    Value::String("10.0.1.51/24".to_string()),
                ]))],
            )],
        );
        let b = make_input(
            "b",
            100,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![("addresses", Value::List(vec![
                    Value::String("10.0.1.51/24".to_string()),
                    Value::String("10.0.1.50/24".to_string()),
                ]))],
            )],
        );

        let result = merge(vec![a, b]);

        assert!(
            result.conflicts.is_empty(),
            "same list values in different order must not produce a conflict"
        );
        let eth0 = result
            .effective_state
            .get("ethernet", "eth0")
            .expect("eth0 must be in effective state");
        let addresses =
            eth0.fields["addresses"].value.as_list().expect("addresses must be a list");
        let addr_strs: Vec<&str> = addresses.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            addr_strs.contains(&"10.0.1.50/24"),
            "effective state addresses must contain 10.0.1.50/24"
        );
        assert!(
            addr_strs.contains(&"10.0.1.51/24"),
            "effective state addresses must contain 10.0.1.51/24"
        );
    }

    // Scenario 8: List fields with genuinely different values produce a conflict.
    #[test]
    fn test_list_fields_different_values_produce_conflict() {
        let a = make_input(
            "a",
            100,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![("addresses", Value::List(vec![
                    Value::String("10.0.1.50/24".to_string()),
                ]))],
            )],
        );
        let b = make_input(
            "b",
            100,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![("addresses", Value::List(vec![
                    Value::String("10.0.2.50/24".to_string()),
                ]))],
            )],
        );

        let result = merge(vec![a, b]);

        assert_eq!(result.conflicts.len(), 1, "different list values must produce a conflict");
        assert_eq!(result.conflicts.conflicts[0].field_name, "addresses");
        if let Some(eth0) = result.effective_state.get("ethernet", "eth0") {
            assert!(
                !eth0.fields.contains_key("addresses"),
                "conflicted 'addresses' must be excluded from effective state"
            );
        }
    }

    // Scenario 9: Multiple conflicts on different entities; ConflictReport.len() returns 2.
    #[test]
    fn test_multiple_conflicts_on_different_entities_conflict_report_len_is_2() {
        let a = make_input(
            "a",
            100,
            vec![
                make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))]),
                make_state("ethernet", "eth1", vec![("mtu", Value::U64(9000))]),
            ],
        );
        let b = make_input(
            "b",
            100,
            vec![
                make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]),
                make_state("ethernet", "eth1", vec![("mtu", Value::U64(1500))]),
            ],
        );

        let result = merge(vec![a, b]);

        assert_eq!(result.conflicts.len(), 2, "ConflictReport.len() must return 2");

        let entity_keys: Vec<(&str, &str)> = result
            .conflicts
            .conflicts
            .iter()
            .map(|c| (c.entity_key.0.as_str(), c.entity_key.1.as_str()))
            .collect();
        assert!(
            entity_keys.contains(&("ethernet", "eth0")),
            "conflict on ethernet/eth0 expected"
        );
        assert!(
            entity_keys.contains(&("ethernet", "eth1")),
            "conflict on ethernet/eth1 expected"
        );
    }

    // Scenario 10: ConflictReport.summary() produces readable output with entity, field,
    //   conflicting policy names, and their values.
    #[test]
    fn test_conflict_report_summary_contains_entity_field_and_policy_details() {
        let team_a = make_input(
            "eth0-team-a",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))])],
        );
        let team_b = make_input(
            "eth0-team-b",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))])],
        );

        let result = merge(vec![team_a, team_b]);

        let summary = result.conflicts.summary();
        assert!(summary.contains("CONFLICTS:"), "summary must contain 'CONFLICTS:' header");
        assert!(summary.contains("ethernet"), "summary must contain entity type 'ethernet'");
        assert!(summary.contains("eth0"), "summary must contain selector key 'eth0'");
        assert!(summary.contains("mtu"), "summary must contain field name 'mtu'");
        assert!(
            summary.contains("eth0-team-a"),
            "summary must contain policy id 'eth0-team-a'"
        );
        assert!(
            summary.contains("eth0-team-b"),
            "summary must contain policy id 'eth0-team-b'"
        );
        assert!(summary.contains("9000"), "summary must contain value 9000");
        assert!(summary.contains("1500"), "summary must contain value 1500");
    }

    // Scenario 11: ConflictReport.by_entity() groups correctly —
    //   2 keys, eth0 has 2 conflicts, eth1 has 1.
    #[test]
    fn test_conflict_report_by_entity_groups_correctly() {
        let a = make_input(
            "a",
            100,
            vec![
                make_state(
                    "ethernet",
                    "eth0",
                    vec![
                        ("mtu", Value::U64(9000)),
                        (
                            "addresses",
                            Value::List(vec![Value::String("10.0.1.50/24".to_string())]),
                        ),
                    ],
                ),
                make_state("ethernet", "eth1", vec![("mtu", Value::U64(9000))]),
            ],
        );
        let b = make_input(
            "b",
            100,
            vec![
                make_state(
                    "ethernet",
                    "eth0",
                    vec![
                        ("mtu", Value::U64(1500)),
                        (
                            "addresses",
                            Value::List(vec![Value::String("10.0.2.50/24".to_string())]),
                        ),
                    ],
                ),
                make_state("ethernet", "eth1", vec![("mtu", Value::U64(1500))]),
            ],
        );

        let result = merge(vec![a, b]);

        assert_eq!(result.conflicts.len(), 3, "3 total conflicts: eth0/mtu, eth0/addresses, eth1/mtu");

        let by_entity = result.conflicts.by_entity();
        assert_eq!(by_entity.len(), 2, "by_entity must have 2 distinct entity keys");

        let eth0_key = ("ethernet".to_string(), "eth0".to_string());
        let eth1_key = ("ethernet".to_string(), "eth1".to_string());
        assert!(by_entity.contains_key(&eth0_key), "by_entity must have ethernet/eth0");
        assert!(by_entity.contains_key(&eth1_key), "by_entity must have ethernet/eth1");
        assert_eq!(by_entity[&eth0_key].len(), 2, "ethernet/eth0 must have 2 conflicts");
        assert_eq!(by_entity[&eth1_key].len(), 1, "ethernet/eth1 must have 1 conflict");
    }

    // Scenario 12: Non-conflicting policies produce an empty ConflictReport.
    #[test]
    fn test_no_conflicts_produces_empty_conflict_report() {
        let a = make_input(
            "policy-a",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))])],
        );
        let b = make_input(
            "policy-b",
            200,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))])],
        );

        let result = merge(vec![a, b]);

        assert!(
            result.conflicts.is_empty(),
            "ConflictReport.is_empty() must return true when there are no conflicts"
        );
        assert_eq!(
            result.conflicts.len(),
            0,
            "ConflictReport.len() must return 0 when there are no conflicts"
        );
    }

    // ── values_equal_for_conflict unit tests ──────────────────────────────────

    #[test]
    fn test_values_equal_for_conflict_identical_u64_scalars_returns_true() {
        assert!(values_equal_for_conflict(&Value::U64(1500), &Value::U64(1500)));
    }

    #[test]
    fn test_values_equal_for_conflict_different_u64_scalars_returns_false() {
        assert!(!values_equal_for_conflict(&Value::U64(9000), &Value::U64(1500)));
    }

    #[test]
    fn test_values_equal_for_conflict_identical_strings_returns_true() {
        assert!(values_equal_for_conflict(
            &Value::String("active-backup".to_string()),
            &Value::String("active-backup".to_string()),
        ));
    }

    #[test]
    fn test_values_equal_for_conflict_different_strings_returns_false() {
        assert!(!values_equal_for_conflict(
            &Value::String("active-backup".to_string()),
            &Value::String("802.3ad".to_string()),
        ));
    }

    #[test]
    fn test_values_equal_for_conflict_identical_bools_returns_true() {
        assert!(values_equal_for_conflict(&Value::Bool(true), &Value::Bool(true)));
    }

    #[test]
    fn test_values_equal_for_conflict_different_bools_returns_false() {
        assert!(!values_equal_for_conflict(&Value::Bool(true), &Value::Bool(false)));
    }

    #[test]
    fn test_values_equal_for_conflict_list_same_order_returns_true() {
        let a = Value::List(vec![
            Value::String("10.0.1.50/24".to_string()),
            Value::String("10.0.1.51/24".to_string()),
        ]);
        let b = Value::List(vec![
            Value::String("10.0.1.50/24".to_string()),
            Value::String("10.0.1.51/24".to_string()),
        ]);
        assert!(values_equal_for_conflict(&a, &b));
    }

    #[test]
    fn test_values_equal_for_conflict_list_different_order_returns_true() {
        let a = Value::List(vec![
            Value::String("10.0.1.50/24".to_string()),
            Value::String("10.0.1.51/24".to_string()),
        ]);
        let b = Value::List(vec![
            Value::String("10.0.1.51/24".to_string()),
            Value::String("10.0.1.50/24".to_string()),
        ]);
        assert!(
            values_equal_for_conflict(&a, &b),
            "list comparison must be order-insensitive to avoid false conflicts"
        );
    }

    #[test]
    fn test_values_equal_for_conflict_list_different_values_returns_false() {
        let a = Value::List(vec![Value::String("10.0.1.50/24".to_string())]);
        let b = Value::List(vec![Value::String("10.0.2.50/24".to_string())]);
        assert!(!values_equal_for_conflict(&a, &b));
    }

    #[test]
    fn test_values_equal_for_conflict_list_different_lengths_returns_false() {
        let a = Value::List(vec![
            Value::String("10.0.1.50/24".to_string()),
            Value::String("10.0.1.51/24".to_string()),
        ]);
        let b = Value::List(vec![Value::String("10.0.1.50/24".to_string())]);
        assert!(!values_equal_for_conflict(&a, &b));
    }

    #[test]
    fn test_values_equal_for_conflict_both_empty_lists_returns_true() {
        assert!(values_equal_for_conflict(&Value::List(vec![]), &Value::List(vec![])));
    }

    // ── ConflictReport unit tests ─────────────────────────────────────────────

    #[test]
    fn test_conflict_report_new_is_empty_and_len_zero() {
        let report = ConflictReport::new();
        assert!(report.is_empty(), "new ConflictReport must be empty");
        assert_eq!(report.len(), 0, "new ConflictReport must have len 0");
    }

    #[test]
    fn test_conflict_report_with_one_conflict_not_empty_and_len_one() {
        let report = ConflictReport {
            conflicts: vec![make_conflict(
                "ethernet",
                "eth0",
                "mtu",
                100,
                vec![
                    make_contribution("team-a", Value::U64(9000)),
                    make_contribution("team-b", Value::U64(1500)),
                ],
            )],
        };
        assert!(!report.is_empty());
        assert_eq!(report.len(), 1);
    }

    #[test]
    fn test_conflict_report_summary_empty_string_when_no_conflicts() {
        let report = ConflictReport::new();
        assert_eq!(
            report.summary(),
            "",
            "summary of empty ConflictReport must be an empty string"
        );
    }

    #[test]
    fn test_conflict_report_summary_contains_expected_content() {
        let report = ConflictReport {
            conflicts: vec![make_conflict(
                "ethernet",
                "eth0",
                "mtu",
                100,
                vec![
                    make_contribution("eth0-team-a", Value::U64(9000)),
                    make_contribution("eth0-team-b", Value::U64(1500)),
                ],
            )],
        };
        let summary = report.summary();
        assert!(summary.contains("CONFLICTS:"), "summary must contain 'CONFLICTS:' header");
        assert!(summary.contains("ethernet"), "summary must contain entity type");
        assert!(summary.contains("eth0"), "summary must contain selector key");
        assert!(summary.contains("mtu"), "summary must contain field name");
        assert!(summary.contains("eth0-team-a"), "summary must contain policy id 'eth0-team-a'");
        assert!(summary.contains("eth0-team-b"), "summary must contain policy id 'eth0-team-b'");
        assert!(summary.contains("9000"), "summary must contain value 9000");
        assert!(summary.contains("1500"), "summary must contain value 1500");
    }

    #[test]
    fn test_conflict_report_summary_two_policy_conflict_says_both_priority() {
        let report = ConflictReport {
            conflicts: vec![make_conflict(
                "ethernet",
                "eth0",
                "mtu",
                100,
                vec![
                    make_contribution("team-a", Value::U64(9000)),
                    make_contribution("team-b", Value::U64(1500)),
                ],
            )],
        };
        let summary = report.summary();
        assert!(
            summary.contains("both priority 100"),
            "two-policy conflict must say 'both priority 100', got: {summary}"
        );
    }

    #[test]
    fn test_conflict_report_summary_three_policy_conflict_says_all_priority() {
        let report = ConflictReport {
            conflicts: vec![make_conflict(
                "ethernet",
                "eth0",
                "mtu",
                100,
                vec![
                    make_contribution("a", Value::U64(1500)),
                    make_contribution("b", Value::U64(9000)),
                    make_contribution("c", Value::U64(4500)),
                ],
            )],
        };
        let summary = report.summary();
        assert!(
            summary.contains("all priority 100"),
            "three-policy conflict must say 'all priority 100', got: {summary}"
        );
    }

    #[test]
    fn test_conflict_report_by_entity_empty_when_no_conflicts() {
        let report = ConflictReport::new();
        assert!(report.by_entity().is_empty());
    }

    #[test]
    fn test_conflict_report_by_entity_groups_conflicts_by_entity_key() {
        let c_eth0_mtu = make_conflict(
            "ethernet",
            "eth0",
            "mtu",
            100,
            vec![
                make_contribution("a", Value::U64(9000)),
                make_contribution("b", Value::U64(1500)),
            ],
        );
        let c_eth0_addr = make_conflict(
            "ethernet",
            "eth0",
            "addresses",
            100,
            vec![
                make_contribution(
                    "a",
                    Value::List(vec![Value::String("10.0.1.50/24".to_string())]),
                ),
                make_contribution(
                    "b",
                    Value::List(vec![Value::String("10.0.2.50/24".to_string())]),
                ),
            ],
        );
        let c_eth1_mtu = make_conflict(
            "ethernet",
            "eth1",
            "mtu",
            100,
            vec![
                make_contribution("a", Value::U64(9000)),
                make_contribution("b", Value::U64(1500)),
            ],
        );

        let report = ConflictReport {
            conflicts: vec![c_eth0_mtu, c_eth0_addr, c_eth1_mtu],
        };

        let by_entity = report.by_entity();
        assert_eq!(by_entity.len(), 2, "must have 2 distinct entity keys");

        let eth0_key = ("ethernet".to_string(), "eth0".to_string());
        let eth1_key = ("ethernet".to_string(), "eth1".to_string());
        assert!(by_entity.contains_key(&eth0_key), "must have ethernet/eth0 key");
        assert!(by_entity.contains_key(&eth1_key), "must have ethernet/eth1 key");
        assert_eq!(by_entity[&eth0_key].len(), 2, "ethernet/eth0 must have 2 conflicts");
        assert_eq!(by_entity[&eth1_key].len(), 1, "ethernet/eth1 must have 1 conflict");
    }

    #[test]
    fn test_conflict_struct_fields_are_recorded_correctly() {
        let conflict = make_conflict(
            "ethernet",
            "eth0",
            "mtu",
            100,
            vec![
                make_contribution("team-a", Value::U64(9000)),
                make_contribution("team-b", Value::U64(1500)),
            ],
        );

        assert_eq!(conflict.entity_key, ("ethernet".to_string(), "eth0".to_string()));
        assert_eq!(conflict.field_name, "mtu");
        assert_eq!(conflict.priority, 100);
        assert_eq!(conflict.contributions.len(), 2);
        assert_eq!(conflict.contributions[0].policy_id.as_str(), "team-a");
        assert_eq!(conflict.contributions[0].value.value, Value::U64(9000));
        assert_eq!(conflict.contributions[1].policy_id.as_str(), "team-b");
        assert_eq!(conflict.contributions[1].value.value, Value::U64(1500));
    }

    // ── ipv4 sub-object tests (SPEC-201 per-sub-field merge) ──────────────────

    /// Builds a `Value::Map` for protocol sub-objects like `ipv4` or `ipv6`.
    fn make_map_value(fields: Vec<(&str, Value)>) -> Value {
        let mut map: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
        for (k, v) in fields {
            map.insert(k.to_string(), v);
        }
        Value::Map(map)
    }

    /// Scenario 1 (ipv4 variant): single policy with ipv4 as a sub-object.
    /// field_sources must use dotted paths like "ipv4.addresses", not just "ipv4".
    #[test]
    fn test_single_policy_ipv4_subobject_field_sources_use_dotted_paths() {
        let ipv4 = make_map_value(vec![(
            "addresses",
            Value::List(vec![Value::String("10.0.1.50/24".to_string())]),
        )]);
        let input = make_input(
            "eth0-config",
            100,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![("mtu", Value::U64(1500)), ("ipv4", ipv4)],
            )],
        );

        let result = merge(vec![input]);

        let eth0 = result
            .effective_state
            .get("ethernet", "eth0")
            .expect("ethernet/eth0 should be in effective state");
        assert_eq!(eth0.fields["mtu"].value, Value::U64(1500));
        assert!(
            matches!(&eth0.fields["ipv4"].value, Value::Map(_)),
            "ipv4 field must be a Map in the effective state"
        );

        // field_sources must record the dotted path "ipv4.addresses"
        assert_eq!(
            get_source(&result, "ethernet", "eth0", "mtu").map(|p| p.as_str()),
            Some("eth0-config"),
            "mtu must be attributed to eth0-config"
        );
        assert_eq!(
            get_source(&result, "ethernet", "eth0", "ipv4.addresses").map(|p| p.as_str()),
            Some("eth0-config"),
            "field_sources must map (ethernet/eth0, ipv4.addresses) to eth0-config"
        );
        // "ipv4" as a whole must NOT appear in field_sources for sub-objects
        // (sub-key resolution replaces the top-level entry with dotted entries)
        assert!(
            get_source(&result, "ethernet", "eth0", "ipv4").is_none(),
            "top-level 'ipv4' key must not appear in field_sources when sub-key resolution is used"
        );
    }

    /// Scenario 2 (ipv4 variant): two policies contribute different ipv4 sub-fields.
    /// field_sources must track each sub-field independently with dotted paths.
    #[test]
    fn test_two_policies_ipv4_different_subfields_merged_with_dotted_sources() {
        let addresses = Value::List(vec![Value::String("10.0.1.50/24".to_string())]);
        let routes = Value::List(vec![Value::String("0.0.0.0/0 via 10.0.1.1".to_string())]);

        // eth0-base provides ipv4.addresses only
        let base = make_input(
            "eth0-base",
            100,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![("ipv4", make_map_value(vec![("addresses", addresses.clone())]))],
            )],
        );
        // eth0-dhcp provides ipv4.routes only
        let dhcp = make_input(
            "eth0-dhcp",
            100,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![("ipv4", make_map_value(vec![("routes", routes.clone())]))],
            )],
        );

        let result = merge(vec![base, dhcp]);

        let eth0 = result
            .effective_state
            .get("ethernet", "eth0")
            .expect("ethernet/eth0 should be in effective state");

        let ipv4_map = eth0.fields["ipv4"].value.as_map().expect("ipv4 must be a map");
        assert!(ipv4_map.contains_key("addresses"), "merged ipv4 must contain 'addresses'");
        assert!(ipv4_map.contains_key("routes"), "merged ipv4 must contain 'routes'");

        assert_eq!(
            get_source(&result, "ethernet", "eth0", "ipv4.addresses").map(|p| p.as_str()),
            Some("eth0-base"),
            "ipv4.addresses must be attributed to eth0-base"
        );
        assert_eq!(
            get_source(&result, "ethernet", "eth0", "ipv4.routes").map(|p| p.as_str()),
            Some("eth0-dhcp"),
            "ipv4.routes must be attributed to eth0-dhcp"
        );
    }

    /// Scenario 11: per-sub-field priority merge within the ipv4 sub-object.
    /// Policy B (priority 200) wins ipv4.addresses; policy A (priority 100, sole
    /// provider) wins ipv4.routes.
    #[test]
    fn test_per_subfield_priority_merge_within_ipv4_subobject() {
        let routes = Value::List(vec![Value::String("0.0.0.0/0 via 10.0.1.1".to_string())]);

        // Policy A (priority 100): ipv4={addresses: ["10.0.1.50/24"], routes: [...]}
        let policy_a = make_input(
            "policy-a",
            100,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![(
                    "ipv4",
                    make_map_value(vec![
                        (
                            "addresses",
                            Value::List(vec![Value::String("10.0.1.50/24".to_string())]),
                        ),
                        ("routes", routes.clone()),
                    ]),
                )],
            )],
        );

        // Policy B (priority 200): ipv4={addresses: ["10.0.2.50/24"]}
        let policy_b = make_input(
            "policy-b",
            200,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![(
                    "ipv4",
                    make_map_value(vec![(
                        "addresses",
                        Value::List(vec![Value::String("10.0.2.50/24".to_string())]),
                    )]),
                )],
            )],
        );

        let result = merge(vec![policy_a, policy_b]);

        let eth0 = result
            .effective_state
            .get("ethernet", "eth0")
            .expect("ethernet/eth0 should be in effective state");
        let ipv4_map = eth0.fields["ipv4"].value.as_map().expect("ipv4 must be a map");

        // ipv4.addresses: policy B (priority 200) wins
        let effective_addresses =
            ipv4_map.get("addresses").expect("ipv4.addresses must be present");
        let addr_strs: Vec<String> = effective_addresses
            .as_list()
            .expect("addresses must be a list")
            .iter()
            .map(|v| v.to_string())
            .collect();
        assert!(
            addr_strs.iter().any(|s| s.contains("10.0.2.50")),
            "ipv4.addresses must contain 10.0.2.50/24 (from policy-b, priority 200): {:?}",
            addr_strs
        );
        assert!(
            !addr_strs.iter().any(|s| s.contains("10.0.1.50")),
            "policy-a's 10.0.1.50/24 must not appear in ipv4.addresses (lower priority): {:?}",
            addr_strs
        );

        // ipv4.routes: policy A is the sole provider → included
        let effective_routes = ipv4_map.get("routes").expect(
            "ipv4.routes must be present (policy-a is the sole provider)",
        );
        assert_eq!(effective_routes, &routes, "ipv4.routes must come from policy-a");

        // field_sources must use dotted paths
        assert_eq!(
            get_source(&result, "ethernet", "eth0", "ipv4.addresses").map(|p| p.as_str()),
            Some("policy-b"),
            "ipv4.addresses must be attributed to policy-b (priority 200)"
        );
        assert_eq!(
            get_source(&result, "ethernet", "eth0", "ipv4.routes").map(|p| p.as_str()),
            Some("policy-a"),
            "ipv4.routes must be attributed to policy-a (sole provider)"
        );

        assert!(result.conflicts.is_empty(), "no conflicts should be reported");
    }

    /// Scenario 10 (ipv4 variant): lower priority ipv4 sub-fields are included
    /// when not overridden by a higher-priority policy that only touches other fields.
    #[test]
    fn test_lower_priority_ipv4_subfields_included_when_not_overridden() {
        let routes = Value::List(vec![Value::String("0.0.0.0/0 via 10.0.1.1".to_string())]);

        // base (priority 50): mtu=1500, ipv4={addresses: [...], routes: [...]}
        let base = make_input(
            "base",
            50,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![
                    ("mtu", Value::U64(1500)),
                    (
                        "ipv4",
                        make_map_value(vec![
                            (
                                "addresses",
                                Value::List(vec![Value::String("10.0.1.50/24".to_string())]),
                            ),
                            ("routes", routes.clone()),
                        ]),
                    ),
                ],
            )],
        );

        // overlay (priority 100): mtu=9000 only — does not touch ipv4
        let overlay = make_input(
            "overlay",
            100,
            vec![make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))])],
        );

        let result = merge(vec![base, overlay]);

        let eth0 = result
            .effective_state
            .get("ethernet", "eth0")
            .expect("ethernet/eth0 should be in effective state");

        // mtu overridden by overlay
        assert_eq!(eth0.fields["mtu"].value, Value::U64(9000), "mtu overridden by overlay");

        // ipv4 sub-object comes from base (sole provider)
        let ipv4_map = eth0.fields["ipv4"].value.as_map().expect("ipv4 must be a map");
        let addr_strs: Vec<String> = ipv4_map["addresses"]
            .as_list()
            .expect("addresses must be a list")
            .iter()
            .map(|v| v.to_string())
            .collect();
        assert!(
            addr_strs.iter().any(|s| s.contains("10.0.1.50")),
            "ipv4.addresses must come from base (sole provider)"
        );
        assert_eq!(&ipv4_map["routes"], &routes, "ipv4.routes must come from base (sole provider)");

        // field_sources
        assert_eq!(
            get_source(&result, "ethernet", "eth0", "mtu").map(|p| p.as_str()),
            Some("overlay"),
            "mtu attributed to overlay"
        );
        assert_eq!(
            get_source(&result, "ethernet", "eth0", "ipv4.addresses").map(|p| p.as_str()),
            Some("base"),
            "ipv4.addresses attributed to base"
        );
        assert_eq!(
            get_source(&result, "ethernet", "eth0", "ipv4.routes").map(|p| p.as_str()),
            Some("base"),
            "ipv4.routes attributed to base"
        );
    }

    /// Verify that when two policies provide different values for the same ipv4 sub-field
    /// at the same priority, a conflict is reported for the dotted path (e.g. "ipv4.addresses").
    #[test]
    fn test_ipv4_subfield_conflict_reported_with_dotted_field_name() {
        let policy_a = make_input(
            "policy-a",
            100,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![(
                    "ipv4",
                    make_map_value(vec![(
                        "addresses",
                        Value::List(vec![Value::String("10.0.1.50/24".to_string())]),
                    )]),
                )],
            )],
        );
        let policy_b = make_input(
            "policy-b",
            100,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![(
                    "ipv4",
                    make_map_value(vec![(
                        "addresses",
                        Value::List(vec![Value::String("10.0.2.50/24".to_string())]),
                    )]),
                )],
            )],
        );

        let result = merge(vec![policy_a, policy_b]);

        assert_eq!(
            result.conflicts.len(),
            1,
            "one conflict expected for ipv4.addresses"
        );
        let conflict = &result.conflicts.conflicts[0];
        assert_eq!(
            conflict.field_name, "ipv4.addresses",
            "conflict field_name must use the dotted path 'ipv4.addresses', got '{}'",
            conflict.field_name
        );
        assert_eq!(
            conflict.entity_key,
            ("ethernet".to_string(), "eth0".to_string())
        );
    }

    /// Verify that ipv6 sub-object fields also receive per-sub-field merge,
    /// consistent with the PROTOCOL_SUB_OBJECTS list.
    #[test]
    fn test_ipv6_subobject_per_subfield_merge() {
        let policy_a = make_input(
            "policy-a",
            100,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![(
                    "ipv6",
                    make_map_value(vec![(
                        "addresses",
                        Value::List(vec![Value::String("2001:db8::1/64".to_string())]),
                    )]),
                )],
            )],
        );
        let policy_b = make_input(
            "policy-b",
            100,
            vec![make_state(
                "ethernet",
                "eth0",
                vec![(
                    "ipv6",
                    make_map_value(vec![(
                        "autoconf",
                        Value::Bool(true),
                    )]),
                )],
            )],
        );

        let result = merge(vec![policy_a, policy_b]);

        // Both sub-fields should appear — different keys, no conflict
        assert!(result.conflicts.is_empty(), "no conflicts expected");
        let eth0 =
            result.effective_state.get("ethernet", "eth0").expect("eth0 must be in effective state");
        let ipv6_map = eth0.fields["ipv6"].value.as_map().expect("ipv6 must be a map");
        assert!(ipv6_map.contains_key("addresses"), "ipv6.addresses must be present");
        assert!(ipv6_map.contains_key("autoconf"), "ipv6.autoconf must be present");

        assert_eq!(
            get_source(&result, "ethernet", "eth0", "ipv6.addresses").map(|p| p.as_str()),
            Some("policy-a"),
        );
        assert_eq!(
            get_source(&result, "ethernet", "eth0", "ipv6.autoconf").map(|p| p.as_str()),
            Some("policy-b"),
        );
    }
}
