//! netfyr-state crate — foundational data model for network entity configuration.
//!
//! This crate defines the types that every other netfyr crate depends on.
//!
//! # Design decisions
//!
//! - **`IndexMap` for fields and selector entries.** Fields are stored in
//!   [`IndexMap`] rather than `HashMap` so that
//!   serialisation order is deterministic and diffs are stable across runs.
//!   `Selector::key()` sorts its entries alphabetically to produce a canonical
//!   string key regardless of insertion order.
//!
//! - **IP-aware YAML parsing.** [`Value`] uses a custom deserializer that
//!   tries to parse unquoted strings as IP addresses or CIDR networks before
//!   falling back to plain `String`. This lets users write `192.168.1.0/24`
//!   in YAML without explicit type annotations, while the internal model
//!   carries a typed `Value::IpNetwork`.
//!
//! - **Provenance tracking.** Each [`FieldValue`] pairs its [`Value`] with a
//!   [`Provenance`] variant (`UserConfigured`, `KernelDefault`, `ExternalTool`,
//!   `Derived`). This tells the reconciliation engine and diff display where a
//!   value came from — essential for conflict reporting and history.
//!
//! - **Selector AND-logic.** A [`Selector`] matches only entities that satisfy
//!   *all* specified criteria (name, type, driver, PCI path, MAC, labels).
//!   AND-logic provides stable hardware identification across reboots: a
//!   selector that requires both `driver=ixgbe` and `pci_path=0000:03:00.0`
//!   won't accidentally match a different NIC if the kernel renumbers
//!   interfaces.

pub mod diff;
pub mod entity;
pub mod field;
pub mod loader;
pub mod metadata;
pub mod provenance;
pub mod schema;
pub mod selector;
pub mod set;
pub mod value;
pub mod yaml;

pub use diff::{diff, DiffOp, StateDiff};
pub use entity::State;
pub use field::FieldValue;
pub use loader::{load_dir, load_file};
pub use metadata::StateMetadata;
pub use provenance::Provenance;
pub use schema::{
    EntitySchema, FieldConstraints, FieldSchemaInfo, FieldType, SchemaRegistry, ValidationError,
    ValidationErrorKind, ValidationErrors,
};
pub use selector::{MacAddr, MacAddrParseError, Selector};
pub use set::{complement, intersection, union, Conflict, ConflictError, StateSet};
pub use value::Value;
pub use yaml::{
    deserialize_value, parse_state_value, parse_yaml, serialize_state_to_value, serialize_value,
    state_to_yaml, state_to_yaml_explicit, YamlError,
};

/// A string identifying a category of network entity (e.g., `"ethernet"`, `"bond"`, `"vlan"`).
///
/// Type alias for `String` — zero-cost and fully compatible with all existing code
/// that uses `String` for entity types.
pub type EntityType = String;

/// Known entity type constants. Use these instead of raw string literals
/// to avoid typos and centralize the set of supported types.
pub mod entity_types {
    pub const ETHERNET: &str = "ethernet";
}

/// Compares two `Value`s using schema-declared comparison keys.
///
/// When `comparison_keys` is empty, falls back to `PartialEq`.
/// When non-empty and both values are `List`, items are compared pairwise
/// using only the specified keys from map items.
pub fn values_eq_for_field(a: &Value, b: &Value, comparison_keys: &[String]) -> bool {
    if comparison_keys.is_empty() {
        return a == b;
    }
    match (a, b) {
        (Value::List(la), Value::List(lb)) => {
            if la.len() != lb.len() {
                return false;
            }
            la.iter().zip(lb.iter()).all(|(ia, ib)| item_eq(ia, ib, comparison_keys))
        }
        _ => a == b,
    }
}

fn item_eq(a: &Value, b: &Value, comparison_keys: &[String]) -> bool {
    match (a, b) {
        (Value::Map(ma), Value::Map(mb)) => {
            comparison_keys.iter().all(|k| ma.get(k) == mb.get(k))
        }
        (Value::Map(m), Value::String(s)) | (Value::String(s), Value::Map(m)) => {
            comparison_keys.first().and_then(|k| m.get(k).and_then(Value::as_str)) == Some(s.as_str())
        }
        _ => a == b,
    }
}

// ── Route normalization ──────────────────────────────────────────────────────

const DEFAULT_ROUTE_METRIC: u64 = 100;

/// Fills in `metric: 100` on any route map in `routes` fields that doesn't
/// already specify a metric. This makes desired state comparable to kernel
/// state via `PartialEq`, since the kernel always assigns the default metric.
pub fn normalize_route_defaults(state_set: &mut set::StateSet) {
    for state in state_set.iter_mut() {
        if let Some(fv) = state.fields.get_mut("routes") {
            if let Value::List(ref mut routes) = fv.value {
                for route in routes.iter_mut() {
                    if let Value::Map(ref mut map) = route {
                        if !map.contains_key("metric") {
                            map.insert(
                                "metric".to_string(),
                                Value::U64(DEFAULT_ROUTE_METRIC),
                            );
                        }
                    }
                }
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use indexmap::IndexMap;
    use ipnetwork::IpNetwork;
    use std::net::{IpAddr, Ipv4Addr};

    // ── MacAddr tests ─────────────────────────────────────────────────────────

    /// Scenario: MacAddr parsing and formatting
    #[test]
    fn test_mac_addr_parse_uppercase_succeeds() {
        let mac: MacAddr = "AA:BB:CC:DD:EE:FF".parse().expect("should parse uppercase MAC");
        assert_eq!(mac.0, [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    }

    /// Scenario: MacAddr parsing and formatting — Display produces lowercase with colons
    #[test]
    fn test_mac_addr_display_lowercase_with_colons() {
        let mac: MacAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        assert_eq!(mac.to_string(), "aa:bb:cc:dd:ee:ff");
    }

    /// MacAddr accepts lowercase input too (case-insensitive parsing)
    #[test]
    fn test_mac_addr_parse_lowercase_succeeds() {
        let mac: MacAddr = "aa:bb:cc:dd:ee:ff".parse().expect("should parse lowercase MAC");
        assert_eq!(mac.to_string(), "aa:bb:cc:dd:ee:ff");
    }

    /// Scenario: MacAddr rejects invalid format — non-MAC string returns error
    #[test]
    fn test_mac_addr_parse_invalid_format_fails() {
        let result: Result<MacAddr, _> = "not-a-mac".parse();
        assert!(result.is_err(), "parsing an invalid MAC should fail");
    }

    /// MacAddr rejects strings with too few octets
    #[test]
    fn test_mac_addr_parse_too_few_octets_fails() {
        let result: Result<MacAddr, _> = "AA:BB:CC:DD:EE".parse();
        assert!(result.is_err());
    }

    /// MacAddr rejects strings with too many octets
    #[test]
    fn test_mac_addr_parse_too_many_octets_fails() {
        let result: Result<MacAddr, _> = "AA:BB:CC:DD:EE:FF:00".parse();
        assert!(result.is_err());
    }

    /// MacAddr rejects hex values out of byte range
    #[test]
    fn test_mac_addr_parse_invalid_hex_fails() {
        let result: Result<MacAddr, _> = "ZZ:BB:CC:DD:EE:FF".parse();
        assert!(result.is_err());
    }

    /// MacAddrParseError implements Display
    #[test]
    fn test_mac_addr_parse_error_display() {
        let err = MacAddrParseError;
        let msg = err.to_string();
        assert!(!msg.is_empty(), "error message should not be empty");
    }

    /// MacAddr serializes to the lowercase colon-separated string
    #[test]
    fn test_mac_addr_serialize_as_string() {
        let mac: MacAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let json = serde_json::to_string(&mac).unwrap();
        assert_eq!(json, "\"aa:bb:cc:dd:ee:ff\"");
    }

    /// MacAddr deserializes from a lowercase or uppercase string
    #[test]
    fn test_mac_addr_deserialize_from_string() {
        let mac: MacAddr = serde_json::from_str("\"AA:BB:CC:DD:EE:FF\"").unwrap();
        assert_eq!(mac.to_string(), "aa:bb:cc:dd:ee:ff");
    }

    /// MacAddr parsed from lowercase and uppercase represent the same bytes (case-insensitive equality)
    #[test]
    fn test_mac_addr_equality_is_case_insensitive() {
        let lower: MacAddr = "aa:bb:cc:dd:ee:ff".parse().unwrap();
        let upper: MacAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        assert_eq!(lower, upper, "MAC addresses that differ only in case must be equal");
    }

    // ── Selector::matches() tests ─────────────────────────────────────────────

    /// Scenario: Exact name matching — same name on both selectors returns true
    #[test]
    fn test_matches_exact_name_returns_true() {
        let sel = Selector::with_name("eth0");
        let target = Selector {
            name: Some("eth0".to_string()),
            driver: Some("ixgbe".to_string()),
            ..Default::default()
        };
        assert!(sel.matches(&target));
    }

    /// Scenario: Name mismatch — different names returns false
    #[test]
    fn test_matches_name_mismatch_returns_false() {
        let sel = Selector::with_name("eth0");
        let target = Selector::with_name("eth1");
        assert!(!sel.matches(&target));
    }

    /// Scenario: Multi-field AND matching succeeds — driver + type_ both match
    #[test]
    fn test_matches_multi_field_and_all_match_returns_true() {
        let sel = Selector {
            driver: Some("ixgbe".to_string()),
            type_: Some("ethernet".to_string()),
            ..Default::default()
        };
        let target = Selector {
            name: Some("eth0".to_string()),
            driver: Some("ixgbe".to_string()),
            type_: Some("ethernet".to_string()),
            ..Default::default()
        };
        assert!(sel.matches(&target));
    }

    /// Scenario: Multi-field AND matching fails on one mismatch — type_ differs
    #[test]
    fn test_matches_multi_field_and_one_mismatch_returns_false() {
        let sel = Selector {
            driver: Some("ixgbe".to_string()),
            type_: Some("wifi".to_string()),
            ..Default::default()
        };
        let target = Selector {
            name: Some("eth0".to_string()),
            driver: Some("ixgbe".to_string()),
            type_: Some("ethernet".to_string()),
            ..Default::default()
        };
        assert!(!sel.matches(&target));
    }

    /// Scenario: Unspecified fields match anything — only driver set, target has many fields
    #[test]
    fn test_matches_unspecified_fields_match_anything() {
        let sel = Selector {
            driver: Some("ixgbe".to_string()),
            ..Default::default()
        };
        let target = Selector {
            name: Some("eth0".to_string()),
            driver: Some("ixgbe".to_string()),
            type_: Some("ethernet".to_string()),
            pci_path: Some("0000:03:00.0".to_string()),
            ..Default::default()
        };
        assert!(sel.matches(&target));
    }

    /// Scenario: Empty selector matches everything — all-None selector matches any target
    #[test]
    fn test_matches_empty_selector_matches_everything() {
        let sel = Selector::new();
        let target = Selector {
            name: Some("eth0".to_string()),
            driver: Some("ixgbe".to_string()),
            type_: Some("ethernet".to_string()),
            ..Default::default()
        };
        assert!(sel.matches(&target));
    }

    /// Empty selector also matches another empty selector
    #[test]
    fn test_matches_empty_selector_matches_empty_target() {
        let sel = Selector::new();
        let target = Selector::new();
        assert!(sel.matches(&target));
    }

    /// Scenario: Label subset matching succeeds — self has {"role":"uplink"}, target has that plus more
    #[test]
    fn test_matches_label_subset_matching_succeeds() {
        let mut sel = Selector::new();
        sel.labels.insert("role".to_string(), "uplink".to_string());

        let mut target = Selector::new();
        target.labels.insert("role".to_string(), "uplink".to_string());
        target.labels.insert("env".to_string(), "prod".to_string());

        assert!(sel.matches(&target));
    }

    /// Scenario: Label subset matching fails on missing label — target is missing a required label
    #[test]
    fn test_matches_label_subset_fails_on_missing_label() {
        let mut sel = Selector::new();
        sel.labels.insert("role".to_string(), "uplink".to_string());
        sel.labels.insert("env".to_string(), "staging".to_string());

        let mut target = Selector::new();
        target.labels.insert("role".to_string(), "uplink".to_string());
        // "env" is absent on target

        assert!(!sel.matches(&target));
    }

    /// Scenario: Label value mismatch — same key, different value returns false
    #[test]
    fn test_matches_label_value_mismatch_returns_false() {
        let mut sel = Selector::new();
        sel.labels.insert("role".to_string(), "downlink".to_string());

        let mut target = Selector::new();
        target.labels.insert("role".to_string(), "uplink".to_string());

        assert!(!sel.matches(&target));
    }

    /// Scenario: MAC address matching — bytes are equal so comparison is case-insensitive
    #[test]
    fn test_matches_mac_address_case_insensitive() {
        let mac_lower: MacAddr = "aa:bb:cc:dd:ee:ff".parse().unwrap();
        let mac_upper: MacAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();

        let sel = Selector {
            mac: Some(mac_lower),
            ..Default::default()
        };
        let target = Selector {
            name: Some("eth0".to_string()),
            mac: Some(mac_upper),
            ..Default::default()
        };
        assert!(
            sel.matches(&target),
            "MAC matching must be case-insensitive (bytes compared, not strings)"
        );
    }

    /// matches() returns false when self specifies a MAC but target has no MAC
    #[test]
    fn test_matches_mac_specified_but_target_missing_mac_returns_false() {
        let mac: MacAddr = "aa:bb:cc:dd:ee:ff".parse().unwrap();
        let sel = Selector {
            mac: Some(mac),
            ..Default::default()
        };
        let target = Selector::with_name("eth0");
        assert!(!sel.matches(&target));
    }

    /// matches() returns false when self specifies a pci_path but target differs
    #[test]
    fn test_matches_pci_path_mismatch_returns_false() {
        let sel = Selector {
            pci_path: Some("0000:03:00.0".to_string()),
            ..Default::default()
        };
        let target = Selector {
            pci_path: Some("0000:04:00.0".to_string()),
            ..Default::default()
        };
        assert!(!sel.matches(&target));
    }

    /// matches() returns true when pci_path matches exactly
    #[test]
    fn test_matches_pci_path_exact_match_returns_true() {
        let sel = Selector {
            pci_path: Some("0000:03:00.0".to_string()),
            ..Default::default()
        };
        let target = Selector {
            name: Some("eth0".to_string()),
            pci_path: Some("0000:03:00.0".to_string()),
            ..Default::default()
        };
        assert!(sel.matches(&target));
    }

    // ── Selector::is_specific() tests ─────────────────────────────────────────

    /// Scenario: is_specific returns true for named selectors
    #[test]
    fn test_is_specific_returns_true_when_name_set() {
        let sel = Selector::with_name("eth0");
        assert!(sel.is_specific());
    }

    /// Scenario: is_specific returns false for unnamed selectors
    #[test]
    fn test_is_specific_returns_false_when_name_unset() {
        let sel = Selector {
            driver: Some("ixgbe".to_string()),
            ..Default::default()
        };
        assert!(!sel.is_specific());
    }

    /// is_specific returns false for the empty selector
    #[test]
    fn test_is_specific_returns_false_for_empty_selector() {
        assert!(!Selector::new().is_specific());
    }

    // ── Selector::key() tests ─────────────────────────────────────────────────

    /// Scenario: key produces stable identifier from name — returns the name directly
    #[test]
    fn test_key_returns_name_when_name_is_set() {
        let sel = Selector::with_name("eth0");
        assert_eq!(sel.key(), "eth0");
    }

    /// Scenario: key produces deterministic identifier without name — called twice, same result
    #[test]
    fn test_key_is_deterministic_without_name() {
        let sel = Selector {
            driver: Some("ixgbe".to_string()),
            type_: Some("ethernet".to_string()),
            ..Default::default()
        };
        assert_eq!(sel.key(), sel.key(), "key() must return the same value on repeated calls");
    }

    /// Scenario: key produces deterministic identifier without name — contains driver and type
    #[test]
    fn test_key_contains_driver_and_entity_type_without_name() {
        let sel = Selector {
            driver: Some("ixgbe".to_string()),
            type_: Some("ethernet".to_string()),
            ..Default::default()
        };
        let key = sel.key();
        assert!(
            key.contains("driver=ixgbe"),
            "key should contain driver=ixgbe, got: {key}"
        );
        assert!(
            key.contains("type=ethernet"),
            "key should contain type=ethernet, got: {key}"
        );
    }

    /// key for selector with labels contains the label in the expected format
    #[test]
    fn test_key_contains_labels_without_name() {
        let mut sel = Selector::new();
        sel.labels.insert("role".to_string(), "uplink".to_string());
        let key = sel.key();
        assert!(
            key.contains("labels.role=uplink"),
            "key should contain labels.role=uplink, got: {key}"
        );
    }

    /// key with only driver and a label produces the same result regardless of HashMap order
    #[test]
    fn test_key_label_order_is_deterministic() {
        let mut sel1 = Selector {
            driver: Some("ixgbe".to_string()),
            ..Default::default()
        };
        sel1.labels.insert("role".to_string(), "uplink".to_string());
        sel1.labels.insert("env".to_string(), "prod".to_string());

        // Build a second selector with labels inserted in the opposite order.
        let mut sel2 = Selector {
            driver: Some("ixgbe".to_string()),
            ..Default::default()
        };
        sel2.labels.insert("env".to_string(), "prod".to_string());
        sel2.labels.insert("role".to_string(), "uplink".to_string());

        assert_eq!(sel1.key(), sel2.key(), "key() must be stable regardless of label insertion order");
    }

    /// Empty selector returns an empty string key
    #[test]
    fn test_key_empty_selector_returns_empty_string() {
        assert_eq!(Selector::new().key(), "");
    }

    // ── Serialization tests ───────────────────────────────────────────────────

    /// Scenario: Selector serializes with only set fields (skip_serializing_if)
    /// Uses JSON as a proxy for the serde annotations (they apply to all formats).
    #[test]
    fn test_selector_serializes_only_set_fields() {
        let sel = Selector::with_name("eth0");
        let json = serde_json::to_string(&sel).unwrap();

        assert!(
            json.contains("\"name\":\"eth0\""),
            "serialized output should contain name field, got: {json}"
        );
        assert!(
            !json.contains("driver"),
            "serialized output must not contain driver when unset, got: {json}"
        );
        assert!(
            !json.contains("pci_path"),
            "serialized output must not contain pci_path when unset, got: {json}"
        );
        assert!(
            !json.contains("mac"),
            "serialized output must not contain mac when unset, got: {json}"
        );
        assert!(
            !json.contains("labels"),
            "serialized output must not contain labels when empty, got: {json}"
        );
        assert!(
            !json.contains("entity_type"),
            "serialized output must not contain entity_type when unset, got: {json}"
        );
    }

    /// Selector with all fields set round-trips through JSON correctly
    #[test]
    fn test_selector_round_trips_through_json() {
        let mut sel = Selector {
            name: Some("eth0".to_string()),
            type_: Some("ethernet".to_string()),
            driver: Some("ixgbe".to_string()),
            pci_path: Some("0000:03:00.0".to_string()),
            mac: Some("aa:bb:cc:dd:ee:ff".parse().unwrap()),
            ..Default::default()
        };
        sel.labels.insert("role".to_string(), "uplink".to_string());

        let json = serde_json::to_string(&sel).unwrap();
        let restored: Selector = serde_json::from_str(&json).unwrap();
        assert_eq!(sel, restored);
    }

    /// Deserializing an empty JSON object yields a default Selector (all None, no labels)
    #[test]
    fn test_selector_deserializes_from_empty_object() {
        let sel: Selector = serde_json::from_str("{}").unwrap();
        assert_eq!(sel, Selector::new());
    }

    // ── Value tests ───────────────────────────────────────────────────────────

    #[test]
    fn test_value_all_variants_constructable() {
        let ip: IpAddr = Ipv4Addr::new(10, 0, 1, 1).into();
        let net: IpNetwork = "10.0.1.0/24".parse().unwrap();
        let mut map = IndexMap::new();
        map.insert("key".to_string(), Value::String("val".to_string()));

        let _s = Value::String("eth0".to_string());
        let _u = Value::U64(1500);
        let _i = Value::I64(-1);
        let _b = Value::Bool(true);
        let _ip = Value::IpAddr(ip);
        let _net = Value::IpNetwork(net);
        let _list = Value::List(vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
        ]);
        let _map = Value::Map(map);
    }

    #[test]
    fn test_value_all_variants_clone_debug_partialeq() {
        let ip: IpAddr = Ipv4Addr::new(10, 0, 1, 1).into();
        let net: IpNetwork = "10.0.1.0/24".parse().unwrap();
        let mut map = IndexMap::new();
        map.insert("key".to_string(), Value::String("val".to_string()));

        let variants = vec![
            Value::String("eth0".to_string()),
            Value::U64(1500),
            Value::I64(-1),
            Value::Bool(true),
            Value::IpAddr(ip),
            Value::IpNetwork(net),
            Value::List(vec![
                Value::String("a".to_string()),
                Value::String("b".to_string()),
            ]),
            Value::Map(map),
        ];

        for v in &variants {
            let cloned = v.clone();
            assert_eq!(v, &cloned, "Clone and PartialEq must agree for {:?}", v);
            assert!(!format!("{:?}", v).is_empty(), "Debug must produce non-empty output");
        }
    }

    #[test]
    fn test_value_from_str_slice() {
        assert_eq!(Value::from("hello"), Value::String("hello".to_string()));
    }

    #[test]
    fn test_value_from_string() {
        assert_eq!(
            Value::from("hello".to_string()),
            Value::String("hello".to_string())
        );
    }

    #[test]
    fn test_value_from_u64() {
        assert_eq!(Value::from(42u64), Value::U64(42));
    }

    #[test]
    fn test_value_from_i64() {
        assert_eq!(Value::from(-7i64), Value::I64(-7));
    }

    #[test]
    fn test_value_from_bool_true() {
        assert_eq!(Value::from(true), Value::Bool(true));
    }

    #[test]
    fn test_value_from_bool_false() {
        assert_eq!(Value::from(false), Value::Bool(false));
    }

    #[test]
    fn test_value_from_ip_addr() {
        let ip: IpAddr = Ipv4Addr::new(192, 168, 1, 1).into();
        assert_eq!(Value::from(ip), Value::IpAddr(ip));
    }

    #[test]
    fn test_value_from_ip_network() {
        let net: IpNetwork = "192.168.1.0/24".parse().unwrap();
        assert_eq!(Value::from(net), Value::IpNetwork(net));
    }

    #[test]
    fn test_value_u64_as_u64_returns_some() {
        assert_eq!(Value::U64(1500).as_u64(), Some(1500));
    }

    #[test]
    fn test_value_u64_as_str_returns_none() {
        assert_eq!(Value::U64(1500).as_str(), None);
    }

    #[test]
    fn test_value_u64_as_bool_returns_none() {
        assert_eq!(Value::U64(1500).as_bool(), None);
    }

    #[test]
    fn test_value_u64_as_i64_returns_none() {
        assert_eq!(Value::U64(1500).as_i64(), None);
    }

    #[test]
    fn test_value_u64_as_ip_addr_returns_none() {
        assert_eq!(Value::U64(1500).as_ip_addr(), None);
    }

    #[test]
    fn test_value_string_as_str_returns_some() {
        assert_eq!(Value::String("eth0".to_string()).as_str(), Some("eth0"));
    }

    #[test]
    fn test_value_string_as_u64_returns_none() {
        assert_eq!(Value::String("eth0".to_string()).as_u64(), None);
    }

    #[test]
    fn test_value_bool_as_bool_returns_some() {
        assert_eq!(Value::Bool(true).as_bool(), Some(true));
    }

    #[test]
    fn test_value_i64_as_i64_returns_some() {
        assert_eq!(Value::I64(-1).as_i64(), Some(-1));
    }

    #[test]
    fn test_value_ip_addr_accessor_returns_some() {
        let ip: IpAddr = Ipv4Addr::new(10, 0, 1, 1).into();
        assert_eq!(Value::IpAddr(ip).as_ip_addr(), Some(&ip));
    }

    #[test]
    fn test_value_ip_addr_accessor_returns_none_for_other() {
        assert_eq!(Value::U64(1).as_ip_addr(), None);
    }

    #[test]
    fn test_value_ip_network_accessor_returns_some() {
        let net: IpNetwork = "10.0.0.0/8".parse().unwrap();
        assert_eq!(Value::IpNetwork(net).as_ip_network(), Some(&net));
    }

    #[test]
    fn test_value_ip_network_accessor_returns_none_for_other() {
        assert_eq!(Value::Bool(true).as_ip_network(), None);
    }

    #[test]
    fn test_value_list_accessor_returns_some() {
        let list = vec![Value::String("a".to_string())];
        assert_eq!(Value::List(list.clone()).as_list(), Some(&list));
    }

    #[test]
    fn test_value_list_accessor_returns_none_for_other() {
        assert_eq!(Value::U64(1).as_list(), None);
    }

    #[test]
    fn test_value_map_accessor_returns_some() {
        let mut map = IndexMap::new();
        map.insert("k".to_string(), Value::Bool(false));
        assert_eq!(Value::Map(map.clone()).as_map(), Some(&map));
    }

    #[test]
    fn test_value_map_accessor_returns_none_for_other() {
        assert_eq!(Value::String("x".to_string()).as_map(), None);
    }

    #[test]
    fn test_value_display_string() {
        assert_eq!(format!("{}", Value::String("eth0".to_string())), "eth0");
    }

    #[test]
    fn test_value_display_u64() {
        assert_eq!(format!("{}", Value::U64(1500)), "1500");
    }

    #[test]
    fn test_value_display_i64() {
        assert_eq!(format!("{}", Value::I64(-1)), "-1");
    }

    #[test]
    fn test_value_display_bool() {
        assert_eq!(format!("{}", Value::Bool(true)), "true");
    }

    #[test]
    fn test_value_display_ip_addr() {
        let ip: IpAddr = Ipv4Addr::new(10, 0, 1, 1).into();
        assert_eq!(format!("{}", Value::IpAddr(ip)), "10.0.1.1");
    }

    #[test]
    fn test_value_display_ip_network() {
        let net: IpNetwork = "10.0.1.0/24".parse().unwrap();
        assert_eq!(format!("{}", Value::IpNetwork(net)), "10.0.1.0/24");
    }

    #[test]
    fn test_value_display_list() {
        let list = Value::List(vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
        ]);
        assert_eq!(format!("{}", list), "[a, b]");
    }

    #[test]
    fn test_value_display_map() {
        let mut map = IndexMap::new();
        map.insert("key".to_string(), Value::String("val".to_string()));
        assert_eq!(format!("{}", Value::Map(map)), "{key: val}");
    }

    // ── Provenance tests ──────────────────────────────────────────────────────

    #[test]
    fn test_provenance_user_configured_policy_ref() {
        let p = Provenance::UserConfigured {
            policy_ref: "my-policy".to_string(),
        };
        match p {
            Provenance::UserConfigured { policy_ref } => {
                assert_eq!(policy_ref, "my-policy");
            }
            _ => panic!("Expected UserConfigured"),
        }
    }

    #[test]
    fn test_provenance_kernel_default_has_no_additional_fields() {
        let p = Provenance::KernelDefault;
        assert!(matches!(p, Provenance::KernelDefault));
    }

    #[test]
    fn test_provenance_external_tool_fields() {
        let ts = Utc::now();
        let p = Provenance::ExternalTool {
            tool: "iproute2".to_string(),
            detected_at: ts,
        };
        match p {
            Provenance::ExternalTool { tool, detected_at } => {
                assert_eq!(tool, "iproute2");
                assert_eq!(detected_at, ts);
            }
            _ => panic!("Expected ExternalTool"),
        }
    }

    #[test]
    fn test_provenance_derived_reason() {
        let p = Provenance::Derived {
            reason: "auto-broadcast".to_string(),
        };
        match p {
            Provenance::Derived { reason } => {
                assert_eq!(reason, "auto-broadcast");
            }
            _ => panic!("Expected Derived"),
        }
    }

    #[test]
    fn test_provenance_clone_debug_partialeq() {
        let variants = vec![
            Provenance::UserConfigured {
                policy_ref: "p".to_string(),
            },
            Provenance::KernelDefault,
            Provenance::ExternalTool {
                tool: "t".to_string(),
                detected_at: Utc::now(),
            },
            Provenance::Derived {
                reason: "r".to_string(),
            },
        ];
        for v in &variants {
            let cloned = v.clone();
            assert_eq!(v, &cloned);
            assert!(!format!("{:?}", v).is_empty());
        }
    }

    // ── FieldValue tests ──────────────────────────────────────────────────────

    #[test]
    fn test_field_value_stores_value_and_provenance() {
        let fv = FieldValue {
            value: Value::U64(9000),
            provenance: Provenance::UserConfigured {
                policy_ref: "bond0".to_string(),
            },
        };

        assert_eq!(fv.value, Value::U64(9000));
        assert_eq!(
            fv.provenance,
            Provenance::UserConfigured {
                policy_ref: "bond0".to_string()
            }
        );
    }

    #[test]
    fn test_field_value_clone_debug_partialeq() {
        let fv = FieldValue {
            value: Value::U64(9000),
            provenance: Provenance::KernelDefault,
        };
        let cloned = fv.clone();
        assert_eq!(fv, cloned);
        assert!(!format!("{:?}", fv).is_empty());
    }

    // ── StateMetadata tests ───────────────────────────────────────────────────

    #[test]
    fn test_state_metadata_ids_are_unique() {
        let m1 = StateMetadata::new();
        let m2 = StateMetadata::new();
        assert_ne!(m1.id, m2.id, "Two StateMetadata instances must have different id values");
        assert_ne!(
            m1.timeline_id, m2.timeline_id,
            "Two StateMetadata instances must have different timeline_id values"
        );
    }

    #[test]
    fn test_state_metadata_created_at_is_recent() {
        let before = Utc::now();
        let m = StateMetadata::new();
        let after = Utc::now();
        assert!(
            m.created_at >= before && m.created_at <= after,
            "created_at must be within the current moment: {:?} not in [{:?}, {:?}]",
            m.created_at,
            before,
            after
        );
    }

    #[test]
    fn test_state_metadata_labels_is_empty() {
        let m = StateMetadata::new();
        assert!(m.labels.is_empty(), "labels must be empty by default");
    }

    #[test]
    fn test_state_metadata_description_is_none() {
        let m = StateMetadata::new();
        assert!(m.description.is_none(), "description must be None by default");
    }

    #[test]
    fn test_state_metadata_ids_are_uuidv7() {
        let m = StateMetadata::new();
        assert_eq!(m.id.get_version_num(), 7, "id must be a UUIDv7");
        assert_eq!(m.timeline_id.get_version_num(), 7, "timeline_id must be a UUIDv7");
    }

    #[test]
    fn test_state_metadata_clone_debug_partialeq() {
        let m = StateMetadata::new();
        let cloned = m.clone();
        assert_eq!(m, cloned);
        assert!(!format!("{:?}", m).is_empty());
    }

    #[test]
    fn test_state_metadata_default_equals_new() {
        let m = StateMetadata::default();
        assert!(m.labels.is_empty());
        assert!(m.description.is_none());
        assert_eq!(m.id.get_version_num(), 7);
    }

    // ── values_eq_for_field tests ────────────────────────────────────────────

    #[test]
    fn test_values_eq_empty_keys_delegates_to_partial_eq() {
        let a = Value::String("10.0.1.50/24".to_string());
        let b = Value::String("10.0.1.50/24".to_string());
        assert!(values_eq_for_field(&a, &b, &[]));

        let c = Value::String("10.0.1.51/24".to_string());
        assert!(!values_eq_for_field(&a, &c, &[]));
    }

    #[test]
    fn test_values_eq_map_vs_string_matches_on_comparison_key() {
        let keys = vec!["address".to_string()];
        let mut m = IndexMap::new();
        m.insert("address".to_string(), Value::String("10.0.1.50/24".to_string()));
        m.insert("valid_lft".to_string(), Value::U64(3600));
        let map_val = Value::List(vec![Value::Map(m)]);
        let str_val = Value::List(vec![Value::String("10.0.1.50/24".to_string())]);
        assert!(values_eq_for_field(&map_val, &str_val, &keys));
    }

    #[test]
    fn test_values_eq_map_vs_string_mismatch() {
        let keys = vec!["address".to_string()];
        let mut m = IndexMap::new();
        m.insert("address".to_string(), Value::String("10.0.1.51/24".to_string()));
        let map_val = Value::List(vec![Value::Map(m)]);
        let str_val = Value::List(vec![Value::String("10.0.1.50/24".to_string())]);
        assert!(!values_eq_for_field(&map_val, &str_val, &keys));
    }

    #[test]
    fn test_values_eq_map_vs_map_ignores_extra_keys() {
        let keys = vec!["address".to_string()];
        let mut m1 = IndexMap::new();
        m1.insert("address".to_string(), Value::String("10.0.1.50/24".to_string()));
        m1.insert("valid_lft".to_string(), Value::U64(3600));
        let mut m2 = IndexMap::new();
        m2.insert("address".to_string(), Value::String("10.0.1.50/24".to_string()));
        m2.insert("valid_lft".to_string(), Value::U64(7200));
        let a = Value::List(vec![Value::Map(m1)]);
        let b = Value::List(vec![Value::Map(m2)]);
        assert!(values_eq_for_field(&a, &b, &keys));
    }

    #[test]
    fn test_values_eq_list_length_mismatch() {
        let keys = vec!["address".to_string()];
        let a = Value::List(vec![Value::String("10.0.1.50/24".to_string())]);
        let b = Value::List(vec![
            Value::String("10.0.1.50/24".to_string()),
            Value::String("10.0.1.51/24".to_string()),
        ]);
        assert!(!values_eq_for_field(&a, &b, &keys));
    }

    #[test]
    fn test_values_eq_non_list_with_keys_falls_back_to_partial_eq() {
        let keys = vec!["address".to_string()];
        let a = Value::U64(1500);
        let b = Value::U64(1500);
        assert!(values_eq_for_field(&a, &b, &keys));
    }
}
