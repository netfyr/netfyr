use indexmap::IndexMap;
use netfyr_reconcile::{DiffKind, FieldChangeKind, StateDiff as ReconcileStateDiff};
use netfyr_state::{FieldValue, Provenance, Selector, State, StateMetadata, StateSet, Value};
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
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub outcome: Option<String>,
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

fn value_to_json(v: &Value) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

impl SerializableStateSet {
    /// Convert back to a `StateSet` for diff computation during revert.
    ///
    /// Field values are deserialized from JSON using `Value`'s untagged serde
    /// implementation, which correctly round-trips IpAddr, IpNetwork, numbers,
    /// booleans, strings, lists, and maps. All fields are tagged with
    /// `Provenance::UserConfigured { policy_ref: "revert" }`.
    pub fn to_state_set(&self) -> Result<StateSet, String> {
        let mut state_set = StateSet::new();

        for entity in &self.entities {
            let selector = Selector::with_name(&entity.selector_name);

            let fields_obj = entity.fields.as_object().ok_or_else(|| {
                format!(
                    "entity '{}' fields must be a JSON object",
                    entity.selector_name
                )
            })?;

            let mut fields: IndexMap<String, FieldValue> = IndexMap::new();
            for (field_name, json_val) in fields_obj {
                let value: Value = serde_json::from_value(json_val.clone())
                    .map_err(|e| format!("entity '{}' field '{}': {}", entity.selector_name, field_name, e))?;
                fields.insert(
                    field_name.clone(),
                    FieldValue {
                        value,
                        provenance: Provenance::UserConfigured {
                            policy_ref: "revert".into(),
                        },
                    },
                );
            }

            let state = State {
                entity_type: entity.entity_type.clone(),
                selector,
                fields,
                metadata: StateMetadata::new(),
                policy_ref: Some("revert".into()),
                priority: 100,
            };

            state_set.insert(state);
        }

        Ok(state_set)
    }
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
                            outcome: None,
                        },
                        FieldChangeKind::Unset { current } => SerializableFieldChange {
                            field_name: fc.field_name.clone(),
                            change_kind: "unset".to_string(),
                            current: Some(value_to_json(&current.value)),
                            desired: None,
                            outcome: None,
                        },
                        FieldChangeKind::Unchanged { value } => SerializableFieldChange {
                            field_name: fc.field_name.clone(),
                            change_kind: "unchanged".to_string(),
                            current: Some(value_to_json(&value.value)),
                            desired: None,
                            outcome: None,
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

    /// AC: External change entries have no per-field outcomes — all field changes from
    /// SerializableDiff::from have outcome=None (outcomes are only set by apply_outcomes
    /// after a real apply; external change entries are never passed through apply_outcomes).
    #[test]
    fn test_statediff_conversion_always_sets_outcome_to_none_on_all_field_changes() {
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
                    field_changes: vec![
                        FieldChange {
                            field_name: "mtu".to_string(),
                            change: FieldChangeKind::Set {
                                current: Some(fv(Value::U64(1500))),
                                desired: fv(Value::U64(9000)),
                            },
                        },
                        FieldChange {
                            field_name: "addresses".to_string(),
                            change: FieldChangeKind::Unchanged {
                                value: fv(Value::String("10.0.0.1/24".to_string())),
                            },
                        },
                    ],
                },
                DiffOperation {
                    kind: DiffKind::Remove,
                    entity_type: "ethernet".to_string(),
                    selector: Selector::with_name("eth2"),
                    field_changes: vec![FieldChange {
                        field_name: "routes".to_string(),
                        change: FieldChangeKind::Unset { current: fv(Value::U64(0)) },
                    }],
                },
            ],
        };

        let serializable = SerializableDiff::from(&diff);
        for op in &serializable.operations {
            for fc in &op.field_changes {
                assert!(
                    fc.outcome.is_none(),
                    "field change '{}' on '{}' must have outcome=None after conversion from ReconcileStateDiff \
                     (outcomes are set only by apply_outcomes after a real apply, not during conversion)",
                    fc.field_name,
                    op.entity_name
                );
            }
        }
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

    // ── Feature: SerializableStateSet::to_state_set ───────────────────────────

    /// AC: Snapshot round-trips through serialization — StateSet with 3 entities
    /// → SerializableStateSet → to_state_set() → same entities and field values.
    #[test]
    fn test_to_state_set_round_trip_preserves_all_entities_and_fields() {
        let mut original = StateSet::new();
        original.insert(make_state(
            "ethernet",
            "eth0",
            vec![("mtu", Value::U64(1500)), ("speed", Value::U64(1000))],
        ));
        original.insert(make_state(
            "ethernet",
            "eth1",
            vec![("mtu", Value::U64(9000))],
        ));
        original.insert(make_state(
            "ethernet",
            "eth2",
            vec![("enabled", Value::Bool(true))],
        ));

        let serializable = SerializableStateSet::from(&original);
        let restored = serializable.to_state_set().expect("to_state_set must succeed");

        assert_eq!(restored.len(), 3, "all 3 entities must be present after round-trip");

        let eth0 = restored.get("ethernet", "eth0").expect("eth0 must be in restored set");
        assert_eq!(
            eth0.fields["mtu"].value,
            Value::U64(1500),
            "eth0 mtu must be 1500 after round-trip"
        );
        assert_eq!(
            eth0.fields["speed"].value,
            Value::U64(1000),
            "eth0 speed must be 1000 after round-trip"
        );

        let eth1 = restored.get("ethernet", "eth1").expect("eth1 must be in restored set");
        assert_eq!(
            eth1.fields["mtu"].value,
            Value::U64(9000),
            "eth1 mtu must be 9000 after round-trip"
        );

        let eth2 = restored.get("ethernet", "eth2").expect("eth2 must be in restored set");
        assert_eq!(
            eth2.fields["enabled"].value,
            Value::Bool(true),
            "eth2 enabled must be true after round-trip"
        );
    }

    /// AC: All fields in the restored StateSet have Provenance::UserConfigured { policy_ref: "revert" }.
    #[test]
    fn test_to_state_set_sets_provenance_to_user_configured_revert() {
        let serializable = SerializableStateSet {
            entities: vec![super::SerializableState {
                entity_type: "ethernet".to_string(),
                selector_name: "eth0".to_string(),
                fields: serde_json::json!({ "mtu": 1500u64, "speed": 1000u64 }),
            }],
        };

        let restored = serializable.to_state_set().expect("to_state_set must succeed");
        let eth0 = restored.get("ethernet", "eth0").expect("eth0 must be present");

        for (field_name, fv) in &eth0.fields {
            match &fv.provenance {
                Provenance::UserConfigured { policy_ref } => {
                    assert_eq!(
                        policy_ref, "revert",
                        "field '{}' must have policy_ref=\"revert\", got \"{}\"",
                        field_name, policy_ref
                    );
                }
                other => panic!(
                    "field '{}' must have UserConfigured provenance, got {:?}",
                    field_name, other
                ),
            }
        }
    }

    /// AC: entity_type and selector name are preserved through to_state_set().
    #[test]
    fn test_to_state_set_preserves_entity_type_and_selector_name() {
        let serializable = SerializableStateSet {
            entities: vec![super::SerializableState {
                entity_type: "bond".to_string(),
                selector_name: "bond0".to_string(),
                fields: serde_json::json!({ "mtu": 9000u64 }),
            }],
        };

        let restored = serializable.to_state_set().expect("to_state_set must succeed");
        let bond0 = restored.get("bond", "bond0").expect("bond0 must be in restored set");
        assert_eq!(bond0.entity_type, "bond", "entity_type must be 'bond'");
        assert_eq!(
            bond0.selector.key(),
            "bond0",
            "selector key must be 'bond0'"
        );
    }

    /// AC: IP network addresses (CIDR notation) round-trip through to_state_set.
    #[test]
    fn test_to_state_set_round_trip_with_ip_network_fields() {
        // Use canonical network addresses (no host bits set) to avoid canonicalization
        // changing e.g. "10.99.0.1/24" → "10.99.0.0/24".
        let serializable = SerializableStateSet {
            entities: vec![super::SerializableState {
                entity_type: "ethernet".to_string(),
                selector_name: "eth0".to_string(),
                fields: serde_json::json!({ "gateway": "10.0.0.0/24" }),
            }],
        };

        let restored = serializable.to_state_set().expect("to_state_set must succeed");
        let eth0 = restored.get("ethernet", "eth0").expect("eth0 must be present");

        let gateway = eth0.fields.get("gateway").expect("gateway field must be present");
        assert!(
            gateway.value.as_ip_network().is_some(),
            "IP CIDR field must deserialize to Value::IpNetwork, not String"
        );
        assert_eq!(
            gateway.value.as_ip_network().unwrap().to_string(),
            "10.0.0.0/24",
            "IP network must round-trip with correct CIDR notation"
        );
    }

    /// AC: A list of IP networks (addresses field) round-trips through to_state_set.
    #[test]
    fn test_to_state_set_round_trip_with_list_of_ip_networks() {
        let serializable = SerializableStateSet {
            entities: vec![super::SerializableState {
                entity_type: "ethernet".to_string(),
                selector_name: "eth0".to_string(),
                // Use canonical network addresses (no host bits).
                fields: serde_json::json!({
                    "addresses": ["192.168.0.0/24", "172.16.0.0/16"]
                }),
            }],
        };

        let restored = serializable.to_state_set().expect("to_state_set must succeed");
        let eth0 = restored.get("ethernet", "eth0").expect("eth0 must be present");

        let addresses = eth0.fields.get("addresses").expect("addresses field must be present");
        let list = addresses.value.as_list().expect("addresses must be a Value::List");
        assert_eq!(list.len(), 2, "addresses list must have 2 entries");
        assert!(
            list[0].as_ip_network().is_some(),
            "first address must be Value::IpNetwork"
        );
        assert!(
            list[1].as_ip_network().is_some(),
            "second address must be Value::IpNetwork"
        );
        assert_eq!(
            list[0].as_ip_network().unwrap().to_string(),
            "192.168.0.0/24"
        );
        assert_eq!(
            list[1].as_ip_network().unwrap().to_string(),
            "172.16.0.0/16"
        );
    }

    /// AC: to_state_set returns Err when fields is not a JSON object.
    #[test]
    fn test_to_state_set_returns_error_when_fields_is_not_an_object() {
        let bad_state_set = SerializableStateSet {
            entities: vec![super::SerializableState {
                entity_type: "ethernet".to_string(),
                selector_name: "eth0".to_string(),
                fields: serde_json::json!("this is a string, not an object"),
            }],
        };

        let result = bad_state_set.to_state_set();
        assert!(
            result.is_err(),
            "to_state_set must return Err when fields is not a JSON object"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("eth0") || err.contains("fields must be a JSON object"),
            "error message must identify the entity; got: {err}"
        );
    }

    /// AC: Empty SerializableStateSet round-trips to an empty StateSet.
    #[test]
    fn test_to_state_set_empty_round_trips_to_empty_state_set() {
        let empty = SerializableStateSet { entities: vec![] };
        let restored = empty.to_state_set().expect("empty to_state_set must succeed");
        assert!(restored.is_empty(), "empty SerializableStateSet must produce empty StateSet");
    }

    /// AC: String fields round-trip through to_state_set.
    #[test]
    fn test_to_state_set_round_trip_with_string_field() {
        let serializable = SerializableStateSet {
            entities: vec![super::SerializableState {
                entity_type: "ethernet".to_string(),
                selector_name: "eth0".to_string(),
                fields: serde_json::json!({ "label": "primary-uplink" }),
            }],
        };

        let restored = serializable.to_state_set().expect("to_state_set must succeed");
        let eth0 = restored.get("ethernet", "eth0").expect("eth0 must be present");
        assert_eq!(
            eth0.fields["label"].value,
            Value::String("primary-uplink".to_string()),
            "string field must round-trip as Value::String"
        );
    }
}
