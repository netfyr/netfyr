//! StateSet collection type and per-field set operations (union, intersection, complement).

use indexmap::IndexMap;

use crate::{FieldValue, State, StateMetadata, Value};

// ── Conflict / ConflictError ──────────────────────────────────────────────────

/// A single field-level conflict detected during a `union` operation.
#[derive(Clone, Debug, PartialEq)]
pub struct Conflict {
    /// Entity type of the conflicting entity (e.g., `"ethernet"`).
    pub entity_type: String,
    /// Selector key of the conflicting entity (e.g., `"eth0"`).
    pub selector_key: String,
    /// Field name where the conflict occurred.
    pub field: String,
    /// Value from the first operand.
    pub value_a: Value,
    /// Value from the second operand.
    pub value_b: Value,
    /// The equal priority at which both values were asserted.
    pub priority: u32,
}

/// Error returned when `union` encounters fields with equal priority and different values.
///
/// Contains every conflict found across all entities and fields — the full scan is
/// always completed so callers receive a complete conflict report, not just the first.
#[derive(Debug, Clone, thiserror::Error)]
#[error("union produced field conflicts")]
pub struct ConflictError {
    pub conflicts: Vec<Conflict>,
}

// ── StateSet ──────────────────────────────────────────────────────────────────

/// A collection of `State` values keyed by `(entity_type, selector.key())`.
///
/// `IndexMap` preserves insertion order, which gives deterministic iteration and
/// serialization output regardless of the order states were inserted.
#[derive(Clone, Debug, Default)]
pub struct StateSet {
    inner: IndexMap<(String, String), State>,
}

impl StateSet {
    /// Returns a new, empty `StateSet`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a state, replacing any existing entry with the same composite key.
    ///
    /// The composite key is `(state.entity_type, state.selector.key())`.
    pub fn insert(&mut self, state: State) {
        let key = (state.entity_type.clone(), state.selector.key());
        self.inner.insert(key, state);
    }

    /// Returns a reference to the state for the given `(entity_type, selector_key)` pair,
    /// or `None` if no such state exists.
    pub fn get(&self, entity_type: &str, selector_key: &str) -> Option<&State> {
        self.inner
            .get(&(entity_type.to_owned(), selector_key.to_owned()))
    }

    /// Removes and returns the state for the given `(entity_type, selector_key)` pair,
    /// or `None` if no such state exists.
    ///
    /// Uses `shift_remove` to preserve the insertion order of the remaining entries.
    pub fn remove(&mut self, entity_type: &str, selector_key: &str) -> Option<State> {
        self.inner
            .shift_remove(&(entity_type.to_owned(), selector_key.to_owned()))
    }

    /// Iterates over all states in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &State> {
        self.inner.values()
    }

    /// Iterates mutably over all states in insertion order.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut State> {
        self.inner.values_mut()
    }

    /// Returns the number of states in this set.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if the set contains no states.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns all `(entity_type, selector_key)` pairs in insertion order.
    pub fn entities(&self) -> Vec<(String, String)> {
        self.inner.keys().cloned().collect()
    }
}

// ── union ─────────────────────────────────────────────────────────────────────

/// Merges two state sets per-field.
///
/// - Entities present in only one set are included as-is.
/// - Entities present in both are merged field by field:
///   - Field present in only one: included.
///   - Field in both with same value (any priority): `a`'s `FieldValue` is used.
///   - Field in both with different values and different priorities: higher priority wins.
///   - Field in both with different values and equal priorities: a `Conflict` is recorded.
///
/// The entire scan always completes before returning so all conflicts are reported at once.
/// The merged `State` for overlapping entities receives fresh `StateMetadata`, `policy_ref:
/// None`, and `priority = max(a.priority, b.priority)`.
pub fn union(a: &StateSet, b: &StateSet) -> Result<StateSet, ConflictError> {
    let mut result = StateSet::new();
    let mut conflicts: Vec<Conflict> = Vec::new();

    // ── Entities in A (may overlap with B) ───────────────────────────────────
    for (key, state_a) in &a.inner {
        if let Some(state_b) = b.inner.get(key) {
            // Entity is in both sets — merge per-field.
            let mut merged_fields: IndexMap<String, FieldValue> = IndexMap::new();

            // Fields from A (possibly also in B).
            for (field_name, fv_a) in &state_a.fields {
                if let Some(fv_b) = state_b.fields.get(field_name) {
                    if fv_a.value == fv_b.value {
                        // Same value: no conflict; take A's FieldValue (deterministic).
                        merged_fields.insert(field_name.clone(), fv_a.clone());
                    } else if state_a.priority > state_b.priority {
                        merged_fields.insert(field_name.clone(), fv_a.clone());
                    } else if state_b.priority > state_a.priority {
                        merged_fields.insert(field_name.clone(), fv_b.clone());
                    } else {
                        // Equal priority, different values — conflict.
                        conflicts.push(Conflict {
                            entity_type: key.0.clone(),
                            selector_key: key.1.clone(),
                            field: field_name.clone(),
                            value_a: fv_a.value.clone(),
                            value_b: fv_b.value.clone(),
                            priority: state_a.priority,
                        });
                    }
                } else {
                    // Field only in A.
                    merged_fields.insert(field_name.clone(), fv_a.clone());
                }
            }

            // Fields only in B.
            for (field_name, fv_b) in &state_b.fields {
                if !state_a.fields.contains_key(field_name) {
                    merged_fields.insert(field_name.clone(), fv_b.clone());
                }
            }

            // Insert the merged state regardless of conflicts — we finish the full scan
            // so that ConflictError contains the complete conflict list.
            result.inner.insert(
                key.clone(),
                State {
                    entity_type: state_a.entity_type.clone(),
                    selector: state_a.selector.clone(),
                    fields: merged_fields,
                    // Fresh metadata: a merged state is a new logical entity.
                    metadata: StateMetadata::new(),
                    policy_ref: None,
                    priority: state_a.priority.max(state_b.priority),
                },
            );
        } else {
            // Entity only in A — include as-is.
            result.inner.insert(key.clone(), state_a.clone());
        }
    }

    // ── Entities only in B ────────────────────────────────────────────────────
    for (key, state_b) in &b.inner {
        if !a.inner.contains_key(key) {
            result.inner.insert(key.clone(), state_b.clone());
        }
    }

    if !conflicts.is_empty() {
        Err(ConflictError { conflicts })
    } else {
        Ok(result)
    }
}

// ── intersection ──────────────────────────────────────────────────────────────

/// Returns the fields present in both sets with the same value.
///
/// Only entities present in **both** sets are considered. Within those entities,
/// only fields whose name appears in both and whose `value` is equal (by `PartialEq`)
/// are included. Priority and provenance are ignored for the equality check.
///
/// The returned `State` for each entity uses `a`'s `FieldValue`, metadata, and priority.
///
/// **Note**: `Value::Map` and `Value::List` comparisons are order-sensitive because
/// `IndexMap` equality is order-sensitive. This is intentional — the spec does not
/// require order-insensitive comparison.
///
/// Entities with zero matching fields are excluded from the result.
pub fn intersection(a: &StateSet, b: &StateSet) -> StateSet {
    let mut result = StateSet::new();

    for (key, state_a) in &a.inner {
        if let Some(state_b) = b.inner.get(key) {
            let mut common_fields: IndexMap<String, FieldValue> = IndexMap::new();

            for (field_name, fv_a) in &state_a.fields {
                if let Some(fv_b) = state_b.fields.get(field_name) {
                    if fv_a.value == fv_b.value {
                        // Take A's FieldValue (deterministic; provenance/priority preserved from A).
                        common_fields.insert(field_name.clone(), fv_a.clone());
                    }
                }
            }

            if !common_fields.is_empty() {
                result.inner.insert(
                    key.clone(),
                    State {
                        entity_type: state_a.entity_type.clone(),
                        selector: state_a.selector.clone(),
                        fields: common_fields,
                        metadata: state_a.metadata.clone(),
                        policy_ref: state_a.policy_ref.clone(),
                        priority: state_a.priority,
                    },
                );
            }
        }
        // Entities only in A are excluded (intersection requires presence in both).
    }

    result
}

// ── complement ─────────────────────────────────────────────────────────────────

/// Returns the fields in `a` that are **not** present in `b`.
///
/// For each entity in `a`:
/// - If the entity is absent from `b`, the entire entity (all fields) is included.
/// - If the entity is present in `b`, only the fields whose **name** does not appear
///   in `b`'s version of that entity are included. Field values are not compared —
///   a field in `b` with a different value still "covers" the field in `a`.
///
/// This is the deletion-detection primitive:
/// `complement(system_state, desired_state)` yields what the backend should remove.
///
/// Entities with zero remaining fields are excluded from the result.
pub fn complement(a: &StateSet, b: &StateSet) -> StateSet {
    let mut result = StateSet::new();

    for (key, state_a) in &a.inner {
        if let Some(state_b) = b.inner.get(key) {
            // Entity in both — keep only fields not present in B (by name).
            let mut remaining_fields: IndexMap<String, FieldValue> = IndexMap::new();

            for (field_name, fv_a) in &state_a.fields {
                if !state_b.fields.contains_key(field_name) {
                    remaining_fields.insert(field_name.clone(), fv_a.clone());
                }
            }

            if !remaining_fields.is_empty() {
                result.inner.insert(
                    key.clone(),
                    State {
                        entity_type: state_a.entity_type.clone(),
                        selector: state_a.selector.clone(),
                        fields: remaining_fields,
                        metadata: state_a.metadata.clone(),
                        policy_ref: state_a.policy_ref.clone(),
                        priority: state_a.priority,
                    },
                );
            }
        } else {
            // Entity only in A — include the whole thing.
            result.inner.insert(key.clone(), state_a.clone());
        }
    }

    result
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
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

    // ── StateSet collection ───────────────────────────────────────────────────

    /// Scenario: Insert and retrieve an entity state
    #[test]
    fn test_stateset_insert_and_get_returns_state() {
        let mut ss = StateSet::new();
        ss.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let got = ss.get("ethernet", "eth0");
        assert!(got.is_some(), "get should return the inserted state");
        assert_eq!(got.unwrap().fields["mtu"].value, Value::U64(1500));
    }

    /// Scenario: Insert and retrieve — len() returns 1 after one insert
    #[test]
    fn test_stateset_len_returns_one_after_insert() {
        let mut ss = StateSet::new();
        ss.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));
        assert_eq!(ss.len(), 1);
    }

    /// New StateSet starts empty
    #[test]
    fn test_stateset_new_is_empty() {
        let ss = StateSet::new();
        assert!(ss.is_empty());
        assert_eq!(ss.len(), 0);
    }

    /// Scenario: Insert replaces existing entity with same key
    #[test]
    fn test_stateset_insert_replaces_existing_with_same_key() {
        let mut ss = StateSet::new();
        ss.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));
        ss.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))], 100));

        assert_eq!(ss.len(), 1, "Replacing an entry must not increase len");
        let got = ss.get("ethernet", "eth0").unwrap();
        assert_eq!(got.fields["mtu"].value, Value::U64(9000));
    }

    /// Scenario: Remove an entity state — returns Some and decrements len
    #[test]
    fn test_stateset_remove_returns_state_and_decrements_len() {
        let mut ss = StateSet::new();
        ss.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let removed = ss.remove("ethernet", "eth0");
        assert!(removed.is_some(), "remove should return Some for an existing key");
        assert_eq!(ss.len(), 0, "len should be 0 after removing the only entry");
    }

    /// Remove a non-existent key returns None
    #[test]
    fn test_stateset_remove_nonexistent_returns_none() {
        let mut ss = StateSet::new();
        assert!(ss.remove("ethernet", "eth0").is_none());
    }

    /// get for a non-existent key returns None
    #[test]
    fn test_stateset_get_nonexistent_returns_none() {
        let ss = StateSet::new();
        assert!(ss.get("ethernet", "eth0").is_none());
    }

    /// iter yields all inserted states
    #[test]
    fn test_stateset_iter_yields_all_states() {
        let mut ss = StateSet::new();
        ss.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));
        ss.insert(make_state("bond", "bond0", vec![("mode", Value::from("802.3ad"))], 100));

        let items: Vec<&State> = ss.iter().collect();
        assert_eq!(items.len(), 2);
    }

    /// entities() returns all (entity_type, selector_key) pairs
    #[test]
    fn test_stateset_entities_returns_all_pairs() {
        let mut ss = StateSet::new();
        ss.insert(make_state("ethernet", "eth0", vec![], 100));
        ss.insert(make_state("bond", "bond0", vec![], 100));

        let entities = ss.entities();
        assert_eq!(entities.len(), 2);
        assert!(entities.contains(&("ethernet".to_string(), "eth0".to_string())));
        assert!(entities.contains(&("bond".to_string(), "bond0".to_string())));
    }

    // ── union ─────────────────────────────────────────────────────────────────

    /// Scenario: Union of disjoint sets — result contains both entities
    #[test]
    fn test_union_of_disjoint_sets_contains_both_entities() {
        let mut a = StateSet::new();
        a.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let mut b = StateSet::new();
        b.insert(make_state("bond", "bond0", vec![("mode", Value::from("802.3ad"))], 100));

        let result = union(&a, &b).expect("union of disjoint sets should not conflict");
        assert_eq!(result.len(), 2);
        assert!(result.get("ethernet", "eth0").is_some());
        assert!(result.get("bond", "bond0").is_some());
    }

    /// Scenario: Union of disjoint sets — each entity retains its original fields
    #[test]
    fn test_union_disjoint_sets_retain_original_fields() {
        let mut a = StateSet::new();
        a.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let mut b = StateSet::new();
        b.insert(make_state("bond", "bond0", vec![("mode", Value::from("802.3ad"))], 100));

        let result = union(&a, &b).unwrap();
        assert_eq!(result.get("ethernet", "eth0").unwrap().fields["mtu"].value, Value::U64(1500));
        assert_eq!(result.get("bond", "bond0").unwrap().fields["mode"].value, Value::from("802.3ad"));
    }

    /// Scenario: Union merges fields from same entity — both fields appear in result
    #[test]
    fn test_union_merges_fields_from_same_entity() {
        let mut a = StateSet::new();
        a.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let mut b = StateSet::new();
        b.insert(make_state("ethernet", "eth0", vec![("speed", Value::U64(1000))], 100));

        let result = union(&a, &b).expect("union with non-conflicting fields should succeed");
        let merged = result.get("ethernet", "eth0").unwrap();
        assert!(merged.fields.contains_key("mtu"), "merged state should contain mtu from A");
        assert!(merged.fields.contains_key("speed"), "merged state should contain speed from B");
    }

    /// Scenario: Union resolves conflicts by higher priority — higher priority field wins
    #[test]
    fn test_union_higher_priority_wins_conflict() {
        let mut a = StateSet::new();
        a.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let mut b = StateSet::new();
        b.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))], 200));

        let result = union(&a, &b).expect("higher priority should resolve conflict");
        let merged = result.get("ethernet", "eth0").unwrap();
        // B has higher priority (200 > 100), so mtu=9000 wins
        assert_eq!(merged.fields["mtu"].value, Value::U64(9000));
    }

    /// Scenario: Union resolves conflicts by higher priority — provenance is preserved from winner
    #[test]
    fn test_union_higher_priority_provenance_is_preserved_from_winner() {
        let mut a = StateSet::new();
        a.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let mut b_fields: IndexMap<String, FieldValue> = IndexMap::new();
        b_fields.insert(
            "mtu".to_string(),
            FieldValue {
                value: Value::U64(9000),
                provenance: Provenance::UserConfigured {
                    policy_ref: "high-prio-policy".to_string(),
                },
            },
        );
        let mut b = StateSet::new();
        b.insert(State {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            fields: b_fields,
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 200,
        });

        let result = union(&a, &b).unwrap();
        let merged = result.get("ethernet", "eth0").unwrap();
        assert_eq!(merged.fields["mtu"].value, Value::U64(9000));
        match &merged.fields["mtu"].provenance {
            Provenance::UserConfigured { policy_ref } => {
                assert_eq!(policy_ref, "high-prio-policy");
            }
            other => panic!("Expected UserConfigured provenance from B, got {:?}", other),
        }
    }

    /// Scenario: Union reports error on equal-priority field conflict
    #[test]
    fn test_union_equal_priority_different_values_returns_conflict_error() {
        let mut a = StateSet::new();
        a.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let mut b = StateSet::new();
        b.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))], 100));

        let result = union(&a, &b);
        assert!(result.is_err(), "Equal-priority conflict should return ConflictError");

        let err = result.unwrap_err();
        assert_eq!(err.conflicts.len(), 1);
        let conflict = &err.conflicts[0];
        assert_eq!(conflict.entity_type, "ethernet");
        assert_eq!(conflict.selector_key, "eth0");
        assert_eq!(conflict.field, "mtu");
        assert_eq!(conflict.value_a, Value::U64(1500));
        assert_eq!(conflict.value_b, Value::U64(9000));
        assert_eq!(conflict.priority, 100);
    }

    /// Scenario: Union allows equal-priority fields with same value — succeeds with mtu=1500
    #[test]
    fn test_union_equal_priority_same_value_succeeds() {
        let mut a = StateSet::new();
        a.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let mut b = StateSet::new();
        b.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let result = union(&a, &b);
        assert!(result.is_ok(), "Same value at equal priority should not conflict");
        let merged = result.unwrap();
        assert_eq!(merged.get("ethernet", "eth0").unwrap().fields["mtu"].value, Value::U64(1500));
    }

    /// Union reports ALL conflicts across all fields, not just the first one
    #[test]
    fn test_union_reports_all_conflicts_not_just_first() {
        let mut a = StateSet::new();
        a.insert(make_state(
            "ethernet",
            "eth0",
            vec![("mtu", Value::U64(1500)), ("speed", Value::U64(100))],
            100,
        ));

        let mut b = StateSet::new();
        b.insert(make_state(
            "ethernet",
            "eth0",
            vec![("mtu", Value::U64(9000)), ("speed", Value::U64(1000))],
            100,
        ));

        let err = union(&a, &b).unwrap_err();
        assert_eq!(err.conflicts.len(), 2, "All conflicts should be reported");
    }

    // ── intersection ──────────────────────────────────────────────────────────

    /// Scenario: Intersection of overlapping sets — only shared fields with same value
    #[test]
    fn test_intersection_returns_common_fields_with_same_value() {
        let mut a = StateSet::new();
        a.insert(make_state(
            "ethernet",
            "eth0",
            vec![("mtu", Value::U64(1500)), ("speed", Value::U64(1000))],
            100,
        ));

        let mut b = StateSet::new();
        b.insert(make_state(
            "ethernet",
            "eth0",
            vec![("mtu", Value::U64(1500)), ("duplex", Value::from("full"))],
            100,
        ));

        let result = intersection(&a, &b);
        let eth0 = result.get("ethernet", "eth0").expect("eth0 should be in intersection");
        assert!(eth0.fields.contains_key("mtu"), "mtu (same value) should be in intersection");
        assert!(!eth0.fields.contains_key("speed"), "speed (only in A) should be excluded");
        assert!(!eth0.fields.contains_key("duplex"), "duplex (only in B) should be excluded");
    }

    /// Scenario: Intersection of disjoint entities — result is empty
    #[test]
    fn test_intersection_of_disjoint_entities_is_empty() {
        let mut a = StateSet::new();
        a.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let mut b = StateSet::new();
        b.insert(make_state("bond", "bond0", vec![("mode", Value::from("802.3ad"))], 100));

        let result = intersection(&a, &b);
        assert!(result.is_empty());
    }

    /// Scenario: Intersection excludes fields with different values — entity excluded with no remaining fields
    #[test]
    fn test_intersection_excludes_fields_with_different_values_entity_excluded() {
        let mut a = StateSet::new();
        a.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let mut b = StateSet::new();
        b.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))], 100));

        let result = intersection(&a, &b);
        assert!(
            result.get("ethernet", "eth0").is_none(),
            "Entity with no common field values should be excluded from intersection"
        );
        assert!(result.is_empty());
    }

    // ── complement ────────────────────────────────────────────────────────────

    /// Scenario: Complement yields fields only in A — mtu excluded, speed included
    #[test]
    fn test_complement_yields_fields_only_in_a() {
        let mut a = StateSet::new();
        a.insert(make_state(
            "ethernet",
            "eth0",
            vec![("mtu", Value::U64(1500)), ("speed", Value::U64(1000))],
            100,
        ));

        let mut b = StateSet::new();
        b.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let result = complement(&a, &b);
        let eth0 = result.get("ethernet", "eth0").expect("eth0 should remain in complement");
        assert!(!eth0.fields.contains_key("mtu"), "mtu exists in B, should be excluded");
        assert!(eth0.fields.contains_key("speed"), "speed is only in A, should be included");
    }

    /// Scenario: Complement of identical sets is empty
    #[test]
    fn test_complement_of_identical_sets_is_empty() {
        let mut a = StateSet::new();
        a.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let mut b = StateSet::new();
        b.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let result = complement(&a, &b);
        assert!(result.is_empty(), "Complement of identical sets should be empty");
    }

    /// Scenario: Complement yields entire entities not in B
    #[test]
    fn test_complement_yields_entire_entity_not_in_b() {
        let mut a = StateSet::new();
        a.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));
        a.insert(make_state("bond", "bond0", vec![("mode", Value::from("802.3ad"))], 100));

        let mut b = StateSet::new();
        b.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let result = complement(&a, &b);
        // bond0 is absent from B — should appear with all its fields
        let bond0 = result.get("bond", "bond0").expect("bond0 should be in complement");
        assert!(bond0.fields.contains_key("mode"));
        // eth0 has all fields covered by B — should not appear
        assert!(
            result.get("ethernet", "eth0").is_none(),
            "eth0 has all its fields in B so should not appear in complement"
        );
    }

    /// Complement excludes a field by name even when the value in B differs from A
    #[test]
    fn test_complement_excludes_field_by_name_regardless_of_value_difference() {
        // Per spec: a field in B with a different value still "covers" the field in A
        let mut a = StateSet::new();
        a.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))], 100));

        let mut b = StateSet::new();
        // B has mtu with a different value; complement still excludes it by name
        b.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))], 100));

        let result = complement(&a, &b);
        assert!(
            result.get("ethernet", "eth0").is_none(),
            "mtu in B (different value) still covers mtu in A — entity should not appear"
        );
    }
}
