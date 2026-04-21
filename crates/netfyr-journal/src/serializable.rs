use netfyr_reconcile::{DiffKind, FieldChangeKind, StateDiff as ReconcileStateDiff};
use netfyr_state::StateSet;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableDiff {
    pub operations: Vec<SerializableDiffOp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableDiffOp {
    pub kind: String,
    pub entity_type: String,
    pub entity_name: String,
    pub field_changes: Vec<SerializableFieldChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableFieldChange {
    pub field_name: String,
    pub change_kind: String,
    pub current: Option<serde_json::Value>,
    pub desired: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableStateSet {
    pub entities: Vec<SerializableState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableState {
    pub entity_type: String,
    pub selector_name: String,
    pub fields: serde_json::Value,
}

fn value_to_json(v: &netfyr_state::Value) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

impl From<&ReconcileStateDiff> for SerializableDiff {
    fn from(diff: &ReconcileStateDiff) -> Self {
        let operations = diff
            .operations
            .iter()
            .map(|op| {
                let kind = match op.kind {
                    DiffKind::Add => "add",
                    DiffKind::Modify => "modify",
                    DiffKind::Remove => "remove",
                }
                .to_string();

                let field_changes = op
                    .field_changes
                    .iter()
                    .map(|fc| match &fc.change {
                        FieldChangeKind::Set { current, desired } => SerializableFieldChange {
                            field_name: fc.field_name.clone(),
                            change_kind: "set".to_string(),
                            current: current.as_ref().map(|fv| value_to_json(&fv.value)),
                            desired: Some(value_to_json(&desired.value)),
                        },
                        FieldChangeKind::Unset { current } => SerializableFieldChange {
                            field_name: fc.field_name.clone(),
                            change_kind: "unset".to_string(),
                            current: Some(value_to_json(&current.value)),
                            desired: None,
                        },
                        FieldChangeKind::Unchanged { value } => SerializableFieldChange {
                            field_name: fc.field_name.clone(),
                            change_kind: "unchanged".to_string(),
                            current: Some(value_to_json(&value.value)),
                            desired: None,
                        },
                    })
                    .collect();

                SerializableDiffOp {
                    kind,
                    entity_type: op.entity_type.clone(),
                    entity_name: op.selector.key(),
                    field_changes,
                }
            })
            .collect();

        SerializableDiff { operations }
    }
}

impl From<&StateSet> for SerializableStateSet {
    fn from(state_set: &StateSet) -> Self {
        let entities = state_set
            .iter()
            .map(|state| {
                let mut obj = serde_json::Map::new();
                for (k, fv) in &state.fields {
                    obj.insert(k.clone(), value_to_json(&fv.value));
                }
                SerializableState {
                    entity_type: state.entity_type.clone(),
                    selector_name: state.selector.key(),
                    fields: serde_json::Value::Object(obj),
                }
            })
            .collect();

        SerializableStateSet { entities }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use netfyr_reconcile::{DiffKind, DiffOperation, FieldChange, FieldChangeKind};
    use netfyr_state::{FieldValue, Provenance, Selector, State, StateMetadata, StateSet, Value};

    fn fv(v: Value) -> FieldValue {
        FieldValue { value: v, provenance: Provenance::KernelDefault }
    }

    fn make_state(entity_type: &str, name: &str, fields: Vec<(&str, Value)>) -> State {
        let mut s = State {
            entity_type: entity_type.to_string(),
            selector: Selector::with_name(name),
            fields: Default::default(),
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 100,
        };
        for (k, v) in fields {
            s.fields.insert(k.to_string(), fv(v));
        }
        s
    }

    /// AC: StateDiff with Add, Modify, Remove operations converts to SerializableDiff with correct kinds.
    #[test]
    fn test_statediff_with_add_modify_remove_converts_to_serializable_diff() {
        let diff = ReconcileStateDiff {
            operations: vec![
                DiffOperation {
                    kind: DiffKind::Add,
                    entity_type: "ethernet".to_string(),
                    selector: Selector::with_name("eth0"),
                    field_changes: vec![FieldChange {
                        field_name: "mtu".to_string(),
                        change: FieldChangeKind::Set { current: None, desired: fv(Value::U64(1500)) },
                    }],
                },
                DiffOperation {
                    kind: DiffKind::Modify,
                    entity_type: "ethernet".to_string(),
                    selector: Selector::with_name("eth1"),
                    field_changes: vec![FieldChange {
                        field_name: "mtu".to_string(),
                        change: FieldChangeKind::Set {
                            current: Some(fv(Value::U64(1500))),
                            desired: fv(Value::U64(9000)),
                        },
                    }],
                },
                DiffOperation {
                    kind: DiffKind::Remove,
                    entity_type: "ethernet".to_string(),
                    selector: Selector::with_name("eth2"),
                    field_changes: vec![FieldChange {
                        field_name: "mtu".to_string(),
                        change: FieldChangeKind::Unset { current: fv(Value::U64(1500)) },
                    }],
                },
            ],
        };

        let serializable = SerializableDiff::from(&diff);
        assert_eq!(serializable.operations.len(), 3, "should have 3 operations");

        let add_op = serializable
            .operations
            .iter()
            .find(|op| op.kind == "add")
            .expect("should have an add op");
        assert_eq!(add_op.entity_type, "ethernet");
        assert_eq!(add_op.entity_name, "eth0");
        assert_eq!(add_op.field_changes.len(), 1);
        assert_eq!(add_op.field_changes[0].field_name, "mtu");
        assert_eq!(add_op.field_changes[0].change_kind, "set");
        assert!(
            add_op.field_changes[0].current.is_none(),
            "add op field should have no current value"
        );
        assert_eq!(add_op.field_changes[0].desired, Some(serde_json::json!(1500u64)));

        let modify_op = serializable
            .operations
            .iter()
            .find(|op| op.kind == "modify")
            .expect("should have a modify op");
        assert_eq!(modify_op.entity_name, "eth1");
        assert_eq!(modify_op.field_changes[0].change_kind, "set");
        assert_eq!(modify_op.field_changes[0].current, Some(serde_json::json!(1500u64)));
        assert_eq!(modify_op.field_changes[0].desired, Some(serde_json::json!(9000u64)));

        let remove_op = serializable
            .operations
            .iter()
            .find(|op| op.kind == "remove")
            .expect("should have a remove op");
        assert_eq!(remove_op.entity_name, "eth2");
        assert_eq!(remove_op.field_changes[0].change_kind, "unset");
        assert_eq!(remove_op.field_changes[0].current, Some(serde_json::json!(1500u64)));
        assert!(
            remove_op.field_changes[0].desired.is_none(),
            "unset field should have no desired value"
        );
    }

    /// AC: StateSet with 3 ethernet entities converts with correct type, name, and fields; no provenance.
    #[test]
    fn test_stateset_converts_to_serializable_stateset_with_all_entities() {
        let mut state_set = StateSet::new();
        state_set.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));
        state_set.insert(make_state("ethernet", "eth1", vec![("mtu", Value::U64(9000))]));
        state_set.insert(make_state("ethernet", "eth2", vec![("mtu", Value::U64(1400))]));

        let serializable = SerializableStateSet::from(&state_set);
        assert_eq!(serializable.entities.len(), 3, "all 3 entities should be present");

        let names: Vec<&str> =
            serializable.entities.iter().map(|e| e.selector_name.as_str()).collect();
        assert!(names.contains(&"eth0"), "eth0 should be present");
        assert!(names.contains(&"eth1"), "eth1 should be present");
        assert!(names.contains(&"eth2"), "eth2 should be present");

        for entity in &serializable.entities {
            assert_eq!(entity.entity_type, "ethernet");
            assert!(entity.fields.is_object(), "fields should be a JSON object");
            assert!(entity.fields.get("mtu").is_some(), "mtu field should be present");
        }
    }

    /// AC: provenance information is not included — only field values appear.
    #[test]
    fn test_stateset_serialization_does_not_include_provenance() {
        let mut state_set = StateSet::new();
        state_set.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));

        let serializable = SerializableStateSet::from(&state_set);
        let entity = &serializable.entities[0];

        let json = serde_json::to_string(&entity.fields).unwrap();
        assert!(
            !json.contains("provenance"),
            "serialized fields should not contain 'provenance'"
        );
        assert!(
            !json.contains("KernelDefault"),
            "serialized fields should not contain provenance variant names"
        );
        assert!(
            !json.contains("source"),
            "serialized fields should not contain provenance source tag"
        );
        assert_eq!(
            entity.fields["mtu"].as_u64(),
            Some(1500),
            "mtu should be present as a plain number value"
        );
    }

    /// AC: empty StateDiff converts to empty SerializableDiff.
    #[test]
    fn test_empty_statediff_converts_to_empty_serializable_diff() {
        let diff = ReconcileStateDiff { operations: vec![] };
        let serializable = SerializableDiff::from(&diff);
        assert!(serializable.operations.is_empty(), "empty diff → empty serializable diff");
    }

    /// AC: empty StateSet converts to empty SerializableStateSet.
    #[test]
    fn test_empty_stateset_converts_to_empty_serializable_stateset() {
        let state_set = StateSet::new();
        let serializable = SerializableStateSet::from(&state_set);
        assert!(serializable.entities.is_empty(), "empty state set → empty serializable state set");
    }

    /// AC: Unchanged field change kind converts correctly (current set, desired None).
    #[test]
    fn test_unchanged_field_change_converts_with_current_and_no_desired() {
        let diff = ReconcileStateDiff {
            operations: vec![DiffOperation {
                kind: DiffKind::Modify,
                entity_type: "ethernet".to_string(),
                selector: Selector::with_name("eth0"),
                field_changes: vec![FieldChange {
                    field_name: "addresses".to_string(),
                    change: FieldChangeKind::Unchanged {
                        value: fv(Value::String("192.168.1.1/24".to_string())),
                    },
                }],
            }],
        };

        let serializable = SerializableDiff::from(&diff);
        let op = &serializable.operations[0];
        assert_eq!(op.field_changes[0].change_kind, "unchanged");
        assert_eq!(
            op.field_changes[0].current,
            Some(serde_json::json!("192.168.1.1/24"))
        );
        assert!(
            op.field_changes[0].desired.is_none(),
            "unchanged field should have no desired value"
        );
    }

    /// AC: entity_name in SerializableDiffOp is the selector's key() value.
    #[test]
    fn test_serializable_diff_op_entity_name_matches_selector_key() {
        let diff = ReconcileStateDiff {
            operations: vec![DiffOperation {
                kind: DiffKind::Add,
                entity_type: "ethernet".to_string(),
                selector: Selector::with_name("bond0"),
                field_changes: vec![],
            }],
        };

        let serializable = SerializableDiff::from(&diff);
        assert_eq!(serializable.operations[0].entity_name, "bond0");
    }
}
