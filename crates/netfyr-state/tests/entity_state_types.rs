//! Integration tests for SPEC-002 acceptance criteria covering cross-module
//! behaviour: State construction, serde round-trip, and field insertion order.

use chrono::Utc;
use indexmap::IndexMap;
use netfyr_state::{FieldValue, Provenance, Selector, State, StateMetadata, Value};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_full_state() -> State {
    let mut labels = HashMap::new();
    labels.insert("role".to_string(), "uplink".to_string());

    let mut metadata = StateMetadata::new();
    metadata.labels = labels;

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

// ---------------------------------------------------------------------------
// Scenario 1: Create a State with all fields populated
// ---------------------------------------------------------------------------

#[test]
fn test_state_all_fields_populated_clone_partialeq() {
    let state = make_full_state();
    let cloned = state.clone();

    // Clone is equal to the original
    assert_eq!(state, cloned);
}

#[test]
fn test_state_all_fields_populated_debug_non_empty() {
    let state = make_full_state();
    let debug_str = format!("{:?}", state);
    assert!(!debug_str.is_empty(), "Debug output must be non-empty");
}

#[test]
fn test_state_entity_type_field() {
    let state = make_full_state();
    assert_eq!(state.entity_type, "ethernet");
}

#[test]
fn test_state_selector_name() {
    let state = make_full_state();
    assert_eq!(state.selector.name.as_deref(), Some("eth0"));
}

#[test]
fn test_state_mtu_field_value_and_provenance() {
    let state = make_full_state();
    let mtu = state.fields.get("mtu").expect("mtu field must exist");
    assert_eq!(mtu.value, Value::U64(1500));
    assert_eq!(
        mtu.provenance,
        Provenance::UserConfigured {
            policy_ref: "eth0-policy".to_string()
        }
    );
}

#[test]
fn test_state_metadata_labels() {
    let state = make_full_state();
    assert_eq!(
        state.metadata.labels.get("role").map(String::as_str),
        Some("uplink")
    );
}

#[test]
fn test_state_policy_ref() {
    let state = make_full_state();
    assert_eq!(state.policy_ref.as_deref(), Some("eth0-policy"));
}

#[test]
fn test_state_priority() {
    let state = make_full_state();
    assert_eq!(state.priority, 100);
}

// ---------------------------------------------------------------------------
// Scenario 8: All types serialize and deserialize with serde
// ---------------------------------------------------------------------------

#[test]
fn test_state_serde_round_trip() {
    // Build a richly-populated State with various Value types
    let net: ipnetwork::IpNetwork = "10.0.1.0/24".parse().unwrap();

    let mut addresses_map = IndexMap::new();
    addresses_map.insert("prefix".to_string(), Value::IpNetwork(net));

    let mut fields = IndexMap::new();
    fields.insert(
        "mtu".to_string(),
        FieldValue {
            value: Value::U64(1500),
            provenance: Provenance::UserConfigured {
                policy_ref: "pol".to_string(),
            },
        },
    );
    fields.insert(
        "enabled".to_string(),
        FieldValue {
            value: Value::Bool(true),
            provenance: Provenance::KernelDefault,
        },
    );
    fields.insert(
        "gateway_network".to_string(),
        FieldValue {
            value: Value::IpNetwork(net),
            provenance: Provenance::ExternalTool {
                tool: "iproute2".to_string(),
                detected_at: Utc::now(),
            },
        },
    );
    fields.insert(
        "tags".to_string(),
        FieldValue {
            value: Value::List(vec![
                Value::String("a".to_string()),
                Value::String("b".to_string()),
            ]),
            provenance: Provenance::Derived {
                reason: "computed".to_string(),
            },
        },
    );
    fields.insert(
        "address".to_string(),
        FieldValue {
            value: Value::Map(addresses_map),
            provenance: Provenance::KernelDefault,
        },
    );
    fields.insert(
        "offset".to_string(),
        FieldValue {
            value: Value::I64(-1),
            provenance: Provenance::KernelDefault,
        },
    );

    let mut metadata = StateMetadata::new();
    metadata.description = Some("test interface".to_string());
    metadata
        .labels
        .insert("env".to_string(), "test".to_string());

    let original = State {
        entity_type: "ethernet".to_string(),
        selector: Selector::with_name("eth0"),
        fields,
        metadata,
        policy_ref: Some("eth0-policy".to_string()),
        priority: 200,
    };

    // Serialize
    let json = serde_json::to_string(&original).expect("serialization must succeed");
    assert!(!json.is_empty(), "JSON output must be non-empty");

    // Deserialize
    let deserialized: State =
        serde_json::from_str(&json).expect("deserialization must succeed");

    assert_eq!(original, deserialized, "Round-tripped value must equal the original");
}

#[test]
fn test_field_value_serde_round_trip() {
    let fv = FieldValue {
        value: Value::U64(9000),
        provenance: Provenance::UserConfigured {
            policy_ref: "bond0".to_string(),
        },
    };
    let json = serde_json::to_string(&fv).expect("serialize");
    let back: FieldValue = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(fv, back);
}

#[test]
fn test_provenance_serde_all_variants() {
    let variants = vec![
        Provenance::UserConfigured {
            policy_ref: "p".to_string(),
        },
        Provenance::KernelDefault,
        Provenance::ExternalTool {
            tool: "iproute2".to_string(),
            detected_at: Utc::now(),
        },
        Provenance::Derived {
            reason: "auto".to_string(),
        },
    ];
    for v in variants {
        let json = serde_json::to_string(&v).expect("serialize");
        let back: Provenance = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(v, back);
    }
}

// ---------------------------------------------------------------------------
// Scenario 9: State fields preserve insertion order
// ---------------------------------------------------------------------------

#[test]
fn test_state_fields_preserve_insertion_order() {
    let mut fields = IndexMap::new();
    fields.insert(
        "mtu".to_string(),
        FieldValue {
            value: Value::U64(1500),
            provenance: Provenance::KernelDefault,
        },
    );
    fields.insert(
        "addresses".to_string(),
        FieldValue {
            value: Value::List(vec![]),
            provenance: Provenance::KernelDefault,
        },
    );
    fields.insert(
        "routes".to_string(),
        FieldValue {
            value: Value::List(vec![]),
            provenance: Provenance::KernelDefault,
        },
    );

    let state = State {
        entity_type: "ethernet".to_string(),
        selector: Selector::new(),
        fields,
        metadata: StateMetadata::new(),
        policy_ref: None,
        priority: 100,
    };

    let keys: Vec<&str> = state.fields.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec!["mtu", "addresses", "routes"],
        "Fields must be iterated in insertion order"
    );
}

#[test]
fn test_state_fields_insertion_order_preserved_after_serde() {
    let mut fields = IndexMap::new();
    for name in &["mtu", "addresses", "routes"] {
        fields.insert(
            name.to_string(),
            FieldValue {
                value: Value::String(name.to_string()),
                provenance: Provenance::KernelDefault,
            },
        );
    }

    let state = State {
        entity_type: "vlan".to_string(),
        selector: Selector::new(),
        fields,
        metadata: StateMetadata::new(),
        policy_ref: None,
        priority: 50,
    };

    let json = serde_json::to_string(&state).expect("serialize");
    let back: State = serde_json::from_str(&json).expect("deserialize");

    let keys: Vec<&str> = back.fields.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["mtu", "addresses", "routes"]);
}

// ---------------------------------------------------------------------------
// Selector placeholder tests
// ---------------------------------------------------------------------------

#[test]
fn test_selector_new_has_no_name() {
    let s = Selector::new();
    assert!(s.name.is_none());
}

#[test]
fn test_selector_with_name() {
    let s = Selector::with_name("eth0");
    assert_eq!(s.name.as_deref(), Some("eth0"));
}

#[test]
fn test_selector_default_has_no_name() {
    let s = Selector::default();
    assert!(s.name.is_none());
}

// ---------------------------------------------------------------------------
// Value::IpAddr JSON round-trip
// ---------------------------------------------------------------------------

/// Value::IpAddr(x.x.x.x) must survive a JSON round-trip as IpAddr, not
/// IpNetwork(x.x.x.x/32). The custom Deserialize impl checks for `/`
/// before trying IpNetwork.
#[test]
fn test_value_ip_addr_json_round_trip() {
    let ip: std::net::IpAddr = std::net::Ipv4Addr::new(10, 0, 1, 1).into();
    let original = Value::IpAddr(ip);

    let json = serde_json::to_string(&original).expect("serialize");
    let deserialized: Value = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(original, deserialized, "IpAddr must survive a JSON round-trip");
}
