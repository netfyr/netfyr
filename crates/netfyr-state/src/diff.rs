//! Diff operation: compute the changes needed to transform one StateSet into another.

use indexmap::IndexMap;

use crate::{FieldValue, Selector, values_eq_for_field};
use crate::schema::SchemaRegistry;
use crate::set::StateSet;

// ── DiffOp ────────────────────────────────────────────────────────────────────

/// A single operation required to transform a `from` state set into a `to` state set.
#[derive(Clone, Debug, PartialEq)]
pub enum DiffOp {
    /// Entity exists in `to` but not in `from` — must be created.
    Add {
        entity_type: String,
        selector: Selector,
        /// All fields of the new entity.
        fields: IndexMap<String, FieldValue>,
    },
    /// Entity exists in both `from` and `to` but differs in at least one field.
    Modify {
        entity_type: String,
        selector: Selector,
        /// Fields that are new in `to` or whose `value` differs from `from`.
        ///
        /// A field that appears in `to` but not in `from` is treated as "changed"
        /// (added to an existing entity), consistent with the spec's acceptance criteria.
        changed_fields: IndexMap<String, FieldValue>,
        /// Names of fields present in `from` that are absent in `to`.
        removed_fields: Vec<String>,
    },
    /// Entity exists in `from` but not in `to` — must be deleted.
    Remove {
        entity_type: String,
        selector: Selector,
    },
}

// ── StateDiff ─────────────────────────────────────────────────────────────────

/// The result of a `diff` operation: an ordered list of `DiffOp` values.
#[derive(Clone, Debug, Default)]
pub struct StateDiff {
    ops: Vec<DiffOp>,
}

impl StateDiff {
    /// Constructs a `StateDiff` from a pre-built ops vector.
    ///
    /// Used by dispatch layers (e.g., `BackendRegistry`) that partition a diff by entity
    /// type and need to construct per-backend sub-diffs.
    pub fn new(ops: Vec<DiffOp>) -> Self {
        Self { ops }
    }

    /// Returns the list of operations as a slice.
    pub fn ops(&self) -> &[DiffOp] {
        &self.ops
    }

    /// Consumes the diff and returns the operations vector.
    pub fn into_ops(self) -> Vec<DiffOp> {
        self.ops
    }

    /// Returns `true` if there are no operations (the two state sets are identical).
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Returns a human-readable summary of the operations.
    ///
    /// Format: `"{n} added, {n} modified, {n} removed"`.
    pub fn summary(&self) -> String {
        let mut added = 0usize;
        let mut modified = 0usize;
        let mut removed = 0usize;

        for op in &self.ops {
            match op {
                DiffOp::Add { .. } => added += 1,
                DiffOp::Modify { .. } => modified += 1,
                DiffOp::Remove { .. } => removed += 1,
            }
        }

        format!("{added} added, {modified} modified, {removed} removed")
    }
}

impl DiffOp {
    /// Returns a reference to the entity type for any variant.
    pub fn entity_type(&self) -> &str {
        match self {
            DiffOp::Add { entity_type, .. }
            | DiffOp::Modify { entity_type, .. }
            | DiffOp::Remove { entity_type, .. } => entity_type,
        }
    }

    /// Returns a reference to the selector for any variant.
    pub fn selector(&self) -> &Selector {
        match self {
            DiffOp::Add { selector, .. }
            | DiffOp::Modify { selector, .. }
            | DiffOp::Remove { selector, .. } => selector,
        }
    }
}

// ── diff ──────────────────────────────────────────────────────────────────────

/// Computes the operations needed to transform `from` into `to`.
///
/// The comparison is **value-only**: two fields are considered equal when their
/// `FieldValue.value` is equal, regardless of provenance. This matches the
/// reconciliation use case — the backend only needs to act on value changes.
///
/// Operations are emitted in two passes:
/// 1. Entities in `to` → `Add` (if absent in `from`) or `Modify` (if different).
/// 2. Entities in `from` absent in `to` → `Remove`.
pub fn diff(from: &StateSet, to: &StateSet, schema: &SchemaRegistry) -> StateDiff {
    let mut ops: Vec<DiffOp> = Vec::new();

    // ── Pass 1: entities in `to` ─────────────────────────────────────────────
    for (entity_type, selector_key) in to.entities() {
        let state_to = to
            .get(&entity_type, &selector_key)
            .expect("entity returned by entities() must exist in the set");

        if let Some(state_from) = from.get(&entity_type, &selector_key) {
            // Entity in both — check field-level differences.
            let mut changed_fields: IndexMap<String, FieldValue> = IndexMap::new();
            let mut removed_fields: Vec<String> = Vec::new();

            // Fields in `to`: new or changed relative to `from`.
            for (field_name, fv_to) in &state_to.fields {
                let cmp_keys = schema
                    .field_info(&entity_type, field_name)
                    .map(|info| info.comparison_keys)
                    .unwrap_or_default();
                match state_from.fields.get(field_name) {
                    Some(fv_from) if !values_eq_for_field(&fv_from.value, &fv_to.value, &cmp_keys) => {
                        changed_fields.insert(field_name.clone(), fv_to.clone());
                    }
                    None => {
                        // Field added in `to`.
                        changed_fields.insert(field_name.clone(), fv_to.clone());
                    }
                    // Same value — no change.
                    _ => {}
                }
            }

            // Fields in `from` absent in `to` — removed.
            for field_name in state_from.fields.keys() {
                if !state_to.fields.contains_key(field_name) {
                    removed_fields.push(field_name.clone());
                }
            }

            if !changed_fields.is_empty() || !removed_fields.is_empty() {
                ops.push(DiffOp::Modify {
                    entity_type: state_to.entity_type.clone(),
                    selector: state_to.selector.clone(),
                    changed_fields,
                    removed_fields,
                });
            }
        } else {
            // Entity only in `to` — must be added.
            ops.push(DiffOp::Add {
                entity_type: state_to.entity_type.clone(),
                selector: state_to.selector.clone(),
                fields: state_to.fields.clone(),
            });
        }
    }

    // ── Pass 2: entities in `from` absent in `to` ────────────────────────────
    for (entity_type, selector_key) in from.entities() {
        if to.get(&entity_type, &selector_key).is_none() {
            let state_from = from
                .get(&entity_type, &selector_key)
                .expect("entity returned by entities() must exist in the set");

            ops.push(DiffOp::Remove {
                entity_type: state_from.entity_type.clone(),
                selector: state_from.selector.clone(),
            });
        }
    }

    StateDiff { ops }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::set::StateSet;
    use crate::schema::SchemaRegistry;
    use crate::{FieldValue, Provenance, Selector, State, StateMetadata, Value};
    use indexmap::IndexMap;

    // ── Test helper ───────────────────────────────────────────────────────────

    fn make_state(entity_type: &str, name: &str, fields: Vec<(&str, Value)>, priority: u32) -> State {
        let mut field_map: IndexMap<String, FieldValue> = IndexMap::new();
        for (k, v) in fields {
            field_map.insert(
                k.to_string(),
                FieldValue {
                    value: v,
                    provenance: Provenance::KernelDefault,
                },
            );
        }
        State {
            entity_type: entity_type.to_string(),
            selector: Selector::with_name(name),
            fields: field_map,
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority,
        }
    }

    // ── diff tests ────────────────────────────────────────────────────────────

    /// Scenario: Diff detects added entities — Add op for entity in `to` but not in `from`
    #[test]
    fn test_diff_detects_added_entities() {
        let from = StateSet::new();
        let mut to = StateSet::new();
        to.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let result = diff(&from, &to, &SchemaRegistry::new());
        assert_eq!(result.ops().len(), 1);
        match &result.ops()[0] {
            DiffOp::Add { entity_type, selector, fields } => {
                assert_eq!(entity_type, "ethernet");
                assert_eq!(selector.name.as_deref(), Some("eth0"));
                assert_eq!(fields["mtu"].value, Value::U64(1500));
            }
            other => panic!("Expected Add op, got {:?}", other),
        }
    }

    /// Scenario: Diff detects removed entities — Remove op for entity in `from` but not in `to`
    #[test]
    fn test_diff_detects_removed_entities() {
        let mut from = StateSet::new();
        from.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));
        let to = StateSet::new();

        let result = diff(&from, &to, &SchemaRegistry::new());
        assert_eq!(result.ops().len(), 1);
        match &result.ops()[0] {
            DiffOp::Remove { entity_type, selector } => {
                assert_eq!(entity_type, "ethernet");
                assert_eq!(selector.name.as_deref(), Some("eth0"));
            }
            other => panic!("Expected Remove op, got {:?}", other),
        }
    }

    /// Scenario: Diff detects modified fields — Modify op with changed_fields
    #[test]
    fn test_diff_detects_modified_fields() {
        let mut from = StateSet::new();
        from.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let mut to = StateSet::new();
        to.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))], 100));

        let result = diff(&from, &to, &SchemaRegistry::new());
        assert_eq!(result.ops().len(), 1);
        match &result.ops()[0] {
            DiffOp::Modify { entity_type, selector, changed_fields, removed_fields } => {
                assert_eq!(entity_type, "ethernet");
                assert_eq!(selector.name.as_deref(), Some("eth0"));
                assert!(changed_fields.contains_key("mtu"), "mtu should be in changed_fields");
                assert_eq!(changed_fields["mtu"].value, Value::U64(9000));
                assert!(removed_fields.is_empty(), "No fields should be removed");
            }
            other => panic!("Expected Modify op, got {:?}", other),
        }
    }

    /// Scenario: Diff detects added and removed fields on same entity
    #[test]
    fn test_diff_detects_added_and_removed_fields_on_same_entity() {
        let mut from = StateSet::new();
        from.insert(make_state(
            "ethernet",
            "eth0",
            vec![("mtu", Value::U64(1500)), ("speed", Value::U64(1000))],
            100,
        ));

        let mut to = StateSet::new();
        to.insert(make_state(
            "ethernet",
            "eth0",
            vec![("mtu", Value::U64(1500)), ("duplex", Value::from("full"))],
            100,
        ));

        let result = diff(&from, &to, &SchemaRegistry::new());
        assert_eq!(result.ops().len(), 1);
        match &result.ops()[0] {
            DiffOp::Modify { changed_fields, removed_fields, .. } => {
                // duplex is new in `to` — should appear as a changed field
                assert!(
                    changed_fields.contains_key("duplex"),
                    "duplex added in `to` should be in changed_fields"
                );
                // speed was in `from` but not in `to` — should be in removed_fields
                assert!(
                    removed_fields.contains(&"speed".to_string()),
                    "speed absent in `to` should be in removed_fields"
                );
                // mtu is unchanged — should not appear
                assert!(
                    !changed_fields.contains_key("mtu"),
                    "unchanged mtu should not be in changed_fields"
                );
            }
            other => panic!("Expected Modify op, got {:?}", other),
        }
    }

    /// Scenario: Diff of identical sets is empty — no ops generated
    #[test]
    fn test_diff_of_identical_sets_is_empty() {
        let mut from = StateSet::new();
        from.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let mut to = StateSet::new();
        to.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let result = diff(&from, &to, &SchemaRegistry::new());
        assert!(result.is_empty(), "Identical sets should produce empty diff");
        assert_eq!(result.ops().len(), 0);
    }

    /// Diff of two empty sets is empty
    #[test]
    fn test_diff_of_two_empty_sets_is_empty() {
        let from = StateSet::new();
        let to = StateSet::new();
        let result = diff(&from, &to, &SchemaRegistry::new());
        assert!(result.is_empty());
    }

    /// Diff does not emit a Modify when only provenance differs (value-only comparison)
    #[test]
    fn test_diff_no_modify_when_only_provenance_differs() {
        let mut from_state = make_state("ethernet", "eth0", vec![], 100);
        from_state.fields.insert(
            "mtu".to_string(),
            FieldValue {
                value: Value::U64(1500),
                provenance: Provenance::KernelDefault,
            },
        );
        let mut from = StateSet::new();
        from.insert(from_state);

        let mut to_state = make_state("ethernet", "eth0", vec![], 100);
        to_state.fields.insert(
            "mtu".to_string(),
            FieldValue {
                value: Value::U64(1500),
                provenance: Provenance::UserConfigured {
                    policy_ref: "my-policy".to_string(),
                },
            },
        );
        let mut to = StateSet::new();
        to.insert(to_state);

        let result = diff(&from, &to, &SchemaRegistry::new());
        assert!(
            result.is_empty(),
            "Diff is value-only; same value with different provenance must not generate Modify"
        );
    }

    // ── StateDiff summary ─────────────────────────────────────────────────────

    /// Scenario: StateDiff summary formatting — "2 added, 1 modified, 1 removed"
    #[test]
    fn test_statediff_summary_formatting() {
        // Build sets such that we get 2 adds, 1 modify, 1 remove
        let mut from = StateSet::new();
        from.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));
        from.insert(make_state("ethernet", "to_remove", vec![("mtu", Value::U64(1500))], 100));

        let mut to = StateSet::new();
        // eth0 with changed mtu → Modify
        to.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))], 100));
        // Two new entities → Add x2
        to.insert(make_state("bond", "bond0", vec![("mode", Value::from("802.3ad"))], 100));
        to.insert(make_state("vlan", "vlan10", vec![("id", Value::U64(10))], 100));
        // to_remove absent from `to` → Remove

        let result = diff(&from, &to, &SchemaRegistry::new());
        let summary = result.summary();
        assert_eq!(
            summary, "2 added, 1 modified, 1 removed",
            "Summary should match expected format, got: {summary}"
        );
    }

    /// Summary for an empty diff is "0 added, 0 modified, 0 removed"
    #[test]
    fn test_statediff_summary_all_zeros_for_empty_diff() {
        let from = StateSet::new();
        let to = StateSet::new();
        let result = diff(&from, &to, &SchemaRegistry::new());
        assert_eq!(result.summary(), "0 added, 0 modified, 0 removed");
    }

    /// is_empty() returns true when there are no operations
    #[test]
    fn test_statediff_is_empty_returns_true_for_no_ops() {
        let from = StateSet::new();
        let to = StateSet::new();
        let result = diff(&from, &to, &SchemaRegistry::new());
        assert!(result.is_empty());
    }

    /// is_empty() returns false when there are operations
    #[test]
    fn test_statediff_is_empty_returns_false_when_ops_exist() {
        let from = StateSet::new();
        let mut to = StateSet::new();
        to.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let result = diff(&from, &to, &SchemaRegistry::new());
        assert!(!result.is_empty());
    }

    /// DHCP map address vs kernel string address produces no diff when CIDRs match.
    #[test]
    fn test_diff_dhcp_map_address_vs_kernel_string_no_diff() {
        let mut from = StateSet::new();
        // Kernel query produces string addresses
        from.insert(make_state(
            "ethernet",
            "eth0",
            vec![("addresses", Value::List(vec![Value::String("10.0.1.50/24".to_string())]))],
            100,
        ));

        let mut to = StateSet::new();
        // DHCP factory produces map addresses with lifetime
        let mut addr_map = IndexMap::new();
        addr_map.insert("address".to_string(), Value::String("10.0.1.50/24".to_string()));
        addr_map.insert("valid_lft".to_string(), Value::U64(3600));
        addr_map.insert("preferred_lft".to_string(), Value::U64(3600));
        to.insert(make_state(
            "ethernet",
            "eth0",
            vec![("addresses", Value::List(vec![Value::Map(addr_map)]))],
            100,
        ));

        let result = diff(&from, &to, &SchemaRegistry::new());
        assert!(result.is_empty(), "DHCP map address with same CIDR as kernel string should produce no diff");
    }

    /// DHCP map address vs kernel string address produces Modify when CIDRs differ.
    #[test]
    fn test_diff_dhcp_map_address_vs_kernel_string_different_cidr() {
        let mut from = StateSet::new();
        from.insert(make_state(
            "ethernet",
            "eth0",
            vec![("addresses", Value::List(vec![Value::String("10.0.1.50/24".to_string())]))],
            100,
        ));

        let mut to = StateSet::new();
        let mut addr_map = IndexMap::new();
        addr_map.insert("address".to_string(), Value::String("10.0.1.51/24".to_string()));
        addr_map.insert("valid_lft".to_string(), Value::U64(3600));
        to.insert(make_state(
            "ethernet",
            "eth0",
            vec![("addresses", Value::List(vec![Value::Map(addr_map)]))],
            100,
        ));

        let result = diff(&from, &to, &SchemaRegistry::new());
        assert_eq!(result.ops().len(), 1, "different CIDRs should produce a Modify");
        assert!(matches!(result.ops()[0], DiffOp::Modify { .. }));
    }
}
