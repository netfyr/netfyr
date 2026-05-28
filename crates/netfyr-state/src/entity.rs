use crate::{FieldValue, Selector, StateMetadata};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// The top-level type representing one network entity's configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// The kind of entity (e.g., `"ethernet"`, `"bond"`, `"vlan"`).
    pub entity_type: String,
    /// Identifies which system entity this targets.
    pub selector: Selector,
    /// Ordered key-value configuration fields.
    ///
    /// `IndexMap` is used to preserve insertion order, which matters for
    /// deterministic YAML serialization and user-facing output.
    pub fields: IndexMap<String, FieldValue>,
    /// Identity and tracking metadata.
    pub metadata: StateMetadata,
    /// Name of the policy that produced this state.
    pub policy_ref: Option<String>,
    /// Numeric priority for field-level conflict resolution (higher wins).
    pub priority: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Provenance, Value};
    use ipnetwork::IpNetwork;
    use std::net::{IpAddr, Ipv4Addr};

    fn make_full_state() -> State {
        let mut metadata = StateMetadata::new();
        metadata.labels.insert("role".to_string(), "uplink".to_string());

        let mut fields = IndexMap::new();
        fields.insert(
            "mtu".to_string(),
            FieldValue {
                value: Value::U64(1500),
                provenance: Provenance::UserConfigured {
                    policy_ref: "eth0-policy".to_string(),
                },
            },
        );

        State {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            fields,
            metadata,
            policy_ref: Some("eth0-policy".to_string()),
            priority: 100,
        }
    }

    /// Scenario: Create a State with all fields populated — entity_type, selector, fields,
    /// metadata with labels, policy_ref, and priority are all stored correctly.
    #[test]
    fn test_state_create_with_all_fields_populated() {
        let state = make_full_state();

        assert_eq!(state.entity_type, "ethernet");
        assert_eq!(state.selector.name, Some("eth0".to_string()));
        assert!(state.fields.contains_key("mtu"));
        assert_eq!(
            state.fields["mtu"].value,
            Value::U64(1500),
            "mtu field value must be U64(1500)"
        );
        assert_eq!(
            state.fields["mtu"].provenance,
            Provenance::UserConfigured {
                policy_ref: "eth0-policy".to_string()
            }
        );
        assert_eq!(state.policy_ref, Some("eth0-policy".to_string()));
        assert_eq!(state.priority, 100);
        assert_eq!(
            state.metadata.labels.get("role"),
            Some(&"uplink".to_string())
        );
    }

    /// Scenario: Create a State with all fields populated — the State can be cloned.
    #[test]
    fn test_state_can_be_cloned() {
        let state = make_full_state();
        let _cloned = state.clone();
    }

    /// Scenario: Create a State with all fields populated — clone is equal to original via PartialEq.
    #[test]
    fn test_state_clone_equals_original_via_partial_eq() {
        let state = make_full_state();
        let cloned = state.clone();
        assert_eq!(state, cloned, "Clone must be equal to the original via PartialEq");
    }

    /// Scenario: Create a State with all fields populated — Debug formatting produces a non-empty string.
    #[test]
    fn test_state_debug_produces_non_empty_string() {
        let state = make_full_state();
        let debug_str = format!("{:?}", state);
        assert!(!debug_str.is_empty(), "Debug formatting must produce a non-empty string");
    }

    /// Scenario: All types serialize and deserialize with serde — State round-trips through JSON.
    #[test]
    fn test_state_json_round_trip() {
        let state = make_full_state();

        let json = serde_json::to_string(&state).expect("serialization must succeed");
        assert!(!json.is_empty(), "JSON output must not be empty");

        let deserialized: State =
            serde_json::from_str(&json).expect("deserialization must succeed");
        assert_eq!(state, deserialized, "Deserialized State must equal the original");
    }

    /// Scenario: All types serialize and deserialize with serde — State with all Value variants
    /// round-trips through JSON correctly.
    #[test]
    fn test_state_json_round_trip_with_all_value_variants() {
        let ip: IpAddr = Ipv4Addr::new(10, 0, 1, 1).into();
        // Spec: IpAddr and IpNetwork are IPv4 only
        let net: IpNetwork = "10.0.1.0/24".parse().unwrap();

        let mut map_val = indexmap::IndexMap::new();
        map_val.insert("key".to_string(), Value::String("val".to_string()));

        let mut fields = IndexMap::new();
        fields.insert("name".to_string(), FieldValue { value: Value::String("eth0".to_string()), provenance: Provenance::KernelDefault });
        fields.insert("mtu".to_string(), FieldValue { value: Value::U64(1500), provenance: Provenance::KernelDefault });
        fields.insert("offset".to_string(), FieldValue { value: Value::I64(-1), provenance: Provenance::KernelDefault });
        fields.insert("enabled".to_string(), FieldValue { value: Value::Bool(true), provenance: Provenance::KernelDefault });
        fields.insert("address".to_string(), FieldValue { value: Value::IpAddr(ip), provenance: Provenance::KernelDefault });
        fields.insert("network".to_string(), FieldValue { value: Value::IpNetwork(net), provenance: Provenance::KernelDefault });
        fields.insert("tags".to_string(), FieldValue { value: Value::List(vec![Value::String("a".to_string()), Value::String("b".to_string())]), provenance: Provenance::KernelDefault });
        fields.insert("meta".to_string(), FieldValue { value: Value::Map(map_val), provenance: Provenance::KernelDefault });

        let state = State {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            fields,
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 100,
        };

        let json = serde_json::to_string(&state).expect("serialization must succeed");
        let deserialized: State =
            serde_json::from_str(&json).expect("deserialization must succeed");
        assert_eq!(state, deserialized, "State with all Value variants must round-trip through JSON");
    }

    /// Scenario: State fields preserve insertion order — fields appear in the order they were inserted.
    #[test]
    fn test_state_fields_preserve_insertion_order() {
        let mut fields = IndexMap::new();
        fields.insert(
            "mtu".to_string(),
            FieldValue { value: Value::U64(1500), provenance: Provenance::KernelDefault },
        );
        fields.insert(
            "addresses".to_string(),
            FieldValue { value: Value::List(vec![]), provenance: Provenance::KernelDefault },
        );
        fields.insert(
            "routes".to_string(),
            FieldValue { value: Value::List(vec![]), provenance: Provenance::KernelDefault },
        );

        let state = State {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            fields,
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 100,
        };

        let keys: Vec<&str> = state.fields.keys().map(|k| k.as_str()).collect();
        assert_eq!(
            keys,
            vec!["mtu", "addresses", "routes"],
            "Fields must be iterated in insertion order: mtu, addresses, routes"
        );
    }

    /// Scenario: State with policy_ref None is valid and round-trips through JSON.
    #[test]
    fn test_state_without_policy_ref_round_trips() {
        let state = State {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            fields: IndexMap::new(),
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 100,
        };

        let json = serde_json::to_string(&state).expect("serialization must succeed");
        let deserialized: State =
            serde_json::from_str(&json).expect("deserialization must succeed");
        assert_eq!(state, deserialized);
        assert!(deserialized.policy_ref.is_none());
    }

    /// Scenario: State priority field defaults to 100 when explicitly set.
    #[test]
    fn test_state_priority_field_is_stored() {
        let state = State {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            fields: IndexMap::new(),
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 200,
        };
        assert_eq!(state.priority, 200);
    }
}
