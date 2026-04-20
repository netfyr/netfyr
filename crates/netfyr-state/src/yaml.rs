//! YAML serialization and deserialization for the flat user-facing format.
//!
//! The flat format places `type`, selector properties, and configuration
//! properties all at the same top level — unlike the internal nested `State`
//! struct. This module converts between the two representations using raw
//! `serde_yaml::Value` manipulation to avoid conflicting with the existing
//! JSON-oriented `Serialize`/`Deserialize` derives on `State`.

use crate::{FieldValue, MacAddr, MacAddrParseError, Provenance, Selector, State, StateMetadata, Value};
use indexmap::IndexMap;
use ipnetwork::Ipv4Network;
use serde::de::Deserialize;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::str::FromStr;

// ── Property classification constants ─────────────────────────────────────────

/// Properties that map to the Selector (matching properties).
const SELECTOR_KEYS: &[&str] = &["name", "driver", "mac", "pci_path"];

/// Properties with special meaning (not stored in State).
const META_KEYS: &[&str] = &["kind", "type"];

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum YamlError {
    /// YAML syntax error from serde_yaml.
    #[error("YAML parse error: {0}")]
    Parse(#[from] serde_yaml::Error),

    /// IO error reading a file.
    #[error("IO error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Document has no `type` key.
    #[error("missing required 'type' field in YAML document")]
    MissingType,

    /// `kind` is present but not `"state"`.
    #[error("invalid 'kind' value: '{0}'; expected 'state' or absent")]
    InvalidKind(String),

    /// The `mac` key could not be parsed as a MAC address.
    #[error("invalid MAC address '{value}': {source}")]
    InvalidMac {
        value: String,
        #[source]
        source: MacAddrParseError,
    },

    /// Two states with the same (entity_type, selector_key) found across files.
    #[error("duplicate entity '{entity_type}/{selector_key}' found in {path}")]
    DuplicateKey {
        entity_type: String,
        selector_key: String,
        path: PathBuf,
    },

    /// Unsupported YAML value (null, float, tagged).
    #[error("unsupported YAML value: {0}")]
    InvalidValue(String),

    /// Document is not a YAML mapping.
    #[error("expected a YAML mapping at the document root")]
    ExpectedMapping,

    /// A selector key or `type` value is not a string.
    #[error("expected a string value for key '{key}'")]
    ExpectedString { key: String },
}

// ── Value conversion ──────────────────────────────────────────────────────────

/// Converts a raw `serde_yaml::Value` to the crate's `Value` enum.
///
/// Heuristic applied in order:
/// 1. YAML bool → `Value::Bool`
/// 2. YAML integer (≥ 0) → `Value::U64`; integer (< 0) → `Value::I64`; float → error
/// 3. YAML string → try `IpNetwork`, then `IpAddr`, fall back to `Value::String`
/// 4. YAML sequence → `Value::List`
/// 5. YAML mapping → `Value::Map`
/// 6. YAML null or tagged → error
pub fn deserialize_value(v: &serde_yaml::Value) -> Result<Value, YamlError> {
    match v {
        serde_yaml::Value::Bool(b) => Ok(Value::Bool(*b)),

        serde_yaml::Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Ok(Value::U64(u))
            } else if let Some(i) = n.as_i64() {
                Ok(Value::I64(i))
            } else {
                Err(YamlError::InvalidValue(format!(
                    "floating-point numbers are not supported: {n}"
                )))
            }
        }

        serde_yaml::Value::String(s) => {
            // Only attempt IpNetwork when a '/' is present; the ipnetwork crate
            // accepts bare IP addresses (e.g. "10.0.1.1") and turns them into /32
            // host-route networks, which would prevent bare IPs from being parsed
            // as Value::IpAddr as the spec requires.
            if s.contains('/') {
                if let Ok(net) = Ipv4Network::from_str(s) {
                    return Ok(Value::IpNetwork(net));
                }
            }
            if let Ok(ip) = Ipv4Addr::from_str(s) {
                Ok(Value::IpAddr(ip))
            } else {
                Ok(Value::String(s.clone()))
            }
        }

        serde_yaml::Value::Sequence(seq) => {
            let items = seq
                .iter()
                .map(deserialize_value)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::List(items))
        }

        serde_yaml::Value::Mapping(map) => {
            let mut result = IndexMap::new();
            for (k, v) in map {
                let key = match k {
                    serde_yaml::Value::String(s) => s.clone(),
                    _ => {
                        return Err(YamlError::InvalidValue(
                            "mapping keys must be strings".to_string(),
                        ))
                    }
                };
                result.insert(key, deserialize_value(v)?);
            }
            Ok(Value::Map(result))
        }

        serde_yaml::Value::Null => Err(YamlError::InvalidValue(
            "null values are not supported".to_string(),
        )),

        serde_yaml::Value::Tagged(_) => Err(YamlError::InvalidValue(
            "tagged YAML values are not supported".to_string(),
        )),
    }
}

/// Converts the crate's `Value` to a `serde_yaml::Value` for emission.
///
/// `IpAddr` and `IpNetwork` are serialized as plain strings (YAML has no
/// dedicated IP type). This round-trips correctly because `deserialize_value`
/// applies the IP-detection heuristic on the way back in.
pub fn serialize_value(v: &Value) -> serde_yaml::Value {
    match v {
        Value::Bool(b) => serde_yaml::Value::Bool(*b),
        Value::U64(n) => serde_yaml::Value::Number(serde_yaml::Number::from(*n)),
        Value::I64(n) => serde_yaml::Value::Number(serde_yaml::Number::from(*n)),
        Value::String(s) => serde_yaml::Value::String(s.clone()),
        Value::IpAddr(ip) => serde_yaml::Value::String(ip.to_string()),
        Value::IpNetwork(net) => serde_yaml::Value::String(net.to_string()),
        Value::List(items) => {
            serde_yaml::Value::Sequence(items.iter().map(serialize_value).collect())
        }
        Value::Map(map) => {
            let mut mapping = serde_yaml::Mapping::new();
            for (k, v) in map {
                mapping.insert(serde_yaml::Value::String(k.clone()), serialize_value(v));
            }
            serde_yaml::Value::Mapping(mapping)
        }
    }
}

// ── State parsing ─────────────────────────────────────────────────────────────

/// Parses one YAML document (already deserialized to `serde_yaml::Value`) into a `State`.
fn parse_raw_to_state(raw: serde_yaml::Value) -> Result<State, YamlError> {
    let map = match raw {
        serde_yaml::Value::Mapping(m) => m,
        _ => return Err(YamlError::ExpectedMapping),
    };

    // Check optional `kind` field.
    let kind_key = serde_yaml::Value::String("kind".to_string());
    if let Some(kind_val) = map.get(&kind_key) {
        match kind_val {
            serde_yaml::Value::String(k) if k == "state" => {}
            serde_yaml::Value::String(k) => return Err(YamlError::InvalidKind(k.clone())),
            _ => return Err(YamlError::InvalidKind("<non-string>".to_string())),
        }
    }

    // Extract required `type` → entity_type.
    let type_key = serde_yaml::Value::String("type".to_string());
    let entity_type = match map.get(&type_key) {
        Some(serde_yaml::Value::String(t)) => t.clone(),
        Some(_) => {
            return Err(YamlError::ExpectedString {
                key: "type".to_string(),
            })
        }
        None => return Err(YamlError::MissingType),
    };

    // Build selector from SELECTOR_KEYS.
    let mut selector = Selector::default();

    let extract_str = |map: &serde_yaml::Mapping, key: &str| -> Result<Option<String>, YamlError> {
        let k = serde_yaml::Value::String(key.to_string());
        match map.get(&k) {
            None => Ok(None),
            Some(serde_yaml::Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(YamlError::ExpectedString {
                key: key.to_string(),
            }),
        }
    };

    if let Some(name) = extract_str(&map, "name")? {
        selector.name = Some(name);
    }
    if let Some(driver) = extract_str(&map, "driver")? {
        selector.driver = Some(driver);
    }
    if let Some(pci_path) = extract_str(&map, "pci_path")? {
        selector.pci_path = Some(pci_path);
    }
    if let Some(mac_str) = extract_str(&map, "mac")? {
        let mac = MacAddr::from_str(&mac_str).map_err(|source| YamlError::InvalidMac {
            value: mac_str,
            source,
        })?;
        selector.mac = Some(mac);
    }

    // All remaining keys (not in META_KEYS or SELECTOR_KEYS) become fields.
    let mut fields = IndexMap::new();
    for (k, v) in &map {
        let key_str = match k {
            serde_yaml::Value::String(s) => s.as_str(),
            _ => continue, // skip non-string keys silently
        };
        if META_KEYS.contains(&key_str) || SELECTOR_KEYS.contains(&key_str) {
            continue;
        }
        let value = deserialize_value(v)?;
        fields.insert(
            key_str.to_string(),
            FieldValue {
                value,
                provenance: Provenance::UserConfigured {
                    policy_ref: String::new(),
                },
            },
        );
    }

    Ok(State {
        entity_type,
        selector,
        fields,
        metadata: StateMetadata::new(),
        policy_ref: None,
        priority: 100,
    })
}

/// Parses a raw `serde_yaml::Value` (flat-format mapping) into a `State`.
///
/// This is the public interface to the flat-format parser. It accepts the same
/// input as `parse_yaml` document-by-document. Used by `netfyr-policy` to
/// convert embedded `state:` / `states:` sub-documents inside policy YAML into
/// `State` values without re-serializing them to strings first.
pub fn parse_state_value(raw: serde_yaml::Value) -> Result<State, YamlError> {
    parse_raw_to_state(raw)
}

/// Parses a YAML string that may contain one or more `---`-separated documents.
///
/// Each document must be a flat mapping with a required `type` key. Empty
/// documents (null values between separators) are silently skipped. Returns
/// an error if any document has an unrecognised `kind` value.
pub fn parse_yaml(input: &str) -> Result<Vec<State>, YamlError> {
    let mut results = Vec::new();
    for document in serde_yaml::Deserializer::from_str(input) {
        let raw: serde_yaml::Value =
            Deserialize::deserialize(document).map_err(YamlError::Parse)?;
        // Skip empty documents (e.g. a trailing `---`).
        if matches!(raw, serde_yaml::Value::Null) {
            continue;
        }
        results.push(parse_raw_to_state(raw)?);
    }
    Ok(results)
}

// ── State serialization ───────────────────────────────────────────────────────

/// Builds a flat `serde_yaml::Mapping` from a `State` (bare format, no `kind`).
///
/// Public so that other crates (e.g., `netfyr-daemon`'s policy store) can embed
/// flat-format state sub-documents inside policy YAML files without duplicating
/// this serialization logic.
pub fn serialize_state_to_value(state: &State) -> serde_yaml::Value {
    let mut map = serde_yaml::Mapping::new();

    map.insert(
        serde_yaml::Value::String("type".to_string()),
        serde_yaml::Value::String(state.entity_type.clone()),
    );

    if let Some(name) = &state.selector.name {
        map.insert(
            serde_yaml::Value::String("name".to_string()),
            serde_yaml::Value::String(name.clone()),
        );
    }
    if let Some(driver) = &state.selector.driver {
        map.insert(
            serde_yaml::Value::String("driver".to_string()),
            serde_yaml::Value::String(driver.clone()),
        );
    }
    if let Some(mac) = &state.selector.mac {
        map.insert(
            serde_yaml::Value::String("mac".to_string()),
            serde_yaml::Value::String(mac.to_string()),
        );
    }
    if let Some(pci_path) = &state.selector.pci_path {
        map.insert(
            serde_yaml::Value::String("pci_path".to_string()),
            serde_yaml::Value::String(pci_path.clone()),
        );
    }

    for (key, field_value) in &state.fields {
        map.insert(
            serde_yaml::Value::String(key.clone()),
            serialize_value(&field_value.value),
        );
    }

    serde_yaml::Value::Mapping(map)
}

/// Builds a flat `serde_yaml::Mapping` with `kind: state` prepended.
fn serialize_state_to_value_explicit(state: &State) -> serde_yaml::Value {
    let base = serialize_state_to_value(state);
    match base {
        serde_yaml::Value::Mapping(base_map) => {
            let mut map = serde_yaml::Mapping::new();
            map.insert(
                serde_yaml::Value::String("kind".to_string()),
                serde_yaml::Value::String("state".to_string()),
            );
            for (k, v) in base_map {
                map.insert(k, v);
            }
            serde_yaml::Value::Mapping(map)
        }
        v => v,
    }
}

/// Serializes a `State` to a flat bare YAML string (no `kind:` field).
pub fn state_to_yaml(state: &State) -> Result<String, YamlError> {
    let value = serialize_state_to_value(state);
    serde_yaml::to_string(&value).map_err(YamlError::Parse)
}

/// Serializes a `State` to a flat explicit YAML string with `kind: state`.
pub fn state_to_yaml_explicit(state: &State) -> Result<String, YamlError> {
    let value = serialize_state_to_value_explicit(state);
    serde_yaml::to_string(&value).map_err(YamlError::Parse)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FieldValue, Provenance, Selector, State, StateMetadata, Value};
    use indexmap::IndexMap;
    use ipnetwork::Ipv4Network;
    use std::net::Ipv4Addr;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn make_fv(v: Value) -> FieldValue {
        FieldValue {
            value: v,
            provenance: Provenance::UserConfigured {
                policy_ref: String::new(),
            },
        }
    }

    /// Build a minimal State with entity_type, name selector, and an mtu field.
    fn make_state(entity_type: &str, name: &str, mtu: u64) -> State {
        let mut fields = IndexMap::new();
        fields.insert("mtu".to_string(), make_fv(Value::U64(mtu)));
        State {
            entity_type: entity_type.to_string(),
            selector: Selector::with_name(name),
            fields,
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 100,
        }
    }

    // ── deserialize_value ─────────────────────────────────────────────────────

    /// Scenario: Boolean values are parsed correctly — true becomes Value::Bool(true)
    #[test]
    fn test_deserialize_value_bool_true() {
        let result = deserialize_value(&serde_yaml::Value::Bool(true)).unwrap();
        assert_eq!(result, Value::Bool(true));
    }

    /// Scenario: Boolean values are parsed correctly — false becomes Value::Bool(false)
    #[test]
    fn test_deserialize_value_bool_false() {
        let result = deserialize_value(&serde_yaml::Value::Bool(false)).unwrap();
        assert_eq!(result, Value::Bool(false));
    }

    /// Scenario: Positive integers are parsed as U64
    #[test]
    fn test_deserialize_value_positive_integer_as_u64() {
        let result = deserialize_value(&serde_yaml::Value::Number(
            serde_yaml::Number::from(1500u64),
        ))
        .unwrap();
        assert_eq!(result, Value::U64(1500));
    }

    /// Scenario: Negative integers are parsed as I64
    #[test]
    fn test_deserialize_value_negative_integer_as_i64() {
        let result = deserialize_value(&serde_yaml::Value::Number(
            serde_yaml::Number::from(-1i64),
        ))
        .unwrap();
        assert_eq!(result, Value::I64(-1));
    }

    /// Zero is non-negative so it maps to U64
    #[test]
    fn test_deserialize_value_zero_as_u64() {
        let result = deserialize_value(&serde_yaml::Value::Number(
            serde_yaml::Number::from(0u64),
        ))
        .unwrap();
        assert_eq!(result, Value::U64(0));
    }

    /// Scenario: String values that look like IPs are parsed as IpAddr
    #[test]
    fn test_deserialize_value_ip_addr_string_becomes_ip_addr() {
        let result =
            deserialize_value(&serde_yaml::Value::String("10.0.1.1".to_string())).unwrap();
        let expected_ip: Ipv4Addr = "10.0.1.1".parse().unwrap();
        assert_eq!(result, Value::IpAddr(expected_ip));
    }

    /// Scenario: String values that look like CIDR are parsed as IpNetwork
    #[test]
    fn test_deserialize_value_cidr_string_becomes_ip_network() {
        let result =
            deserialize_value(&serde_yaml::Value::String("10.0.1.0/24".to_string())).unwrap();
        let expected_net: Ipv4Network = "10.0.1.0/24".parse().unwrap();
        assert_eq!(result, Value::IpNetwork(expected_net));
    }

    /// Scenario: Plain strings remain as strings — "802.3ad" is not an IP
    #[test]
    fn test_deserialize_value_plain_string_stays_as_string() {
        let result =
            deserialize_value(&serde_yaml::Value::String("802.3ad".to_string())).unwrap();
        assert_eq!(result, Value::String("802.3ad".to_string()));
    }

    /// Null values return an error (not supported)
    #[test]
    fn test_deserialize_value_null_returns_error() {
        let result = deserialize_value(&serde_yaml::Value::Null);
        assert!(result.is_err(), "null YAML value should return an error");
    }

    /// Sequence is mapped to Value::List; its elements are deserialized recursively
    #[test]
    fn test_deserialize_value_sequence_as_list_with_ip_network_element() {
        let seq = serde_yaml::Value::Sequence(vec![serde_yaml::Value::String(
            "10.0.1.50/24".to_string(),
        )]);
        let result = deserialize_value(&seq).unwrap();
        let list = result.as_list().expect("should be a list");
        assert_eq!(list.len(), 1);
        let expected_net: Ipv4Network = "10.0.1.50/24".parse().unwrap();
        assert_eq!(list[0], Value::IpNetwork(expected_net));
    }

    /// Mapping is mapped to Value::Map; its values are deserialized recursively
    #[test]
    fn test_deserialize_value_mapping_as_map() {
        let mut mapping = serde_yaml::Mapping::new();
        mapping.insert(
            serde_yaml::Value::String("metric".to_string()),
            serde_yaml::Value::Number(serde_yaml::Number::from(100u64)),
        );
        let result = deserialize_value(&serde_yaml::Value::Mapping(mapping)).unwrap();
        let map = result.as_map().expect("should be a map");
        assert_eq!(map.get("metric"), Some(&Value::U64(100)));
    }

    // ── parse_yaml ────────────────────────────────────────────────────────────

    /// Scenario: Parse flat bare state — returns one State
    #[test]
    fn test_parse_yaml_flat_bare_state_returns_one_state() {
        let yaml = "type: ethernet\nname: eth0\nmtu: 1500\naddresses:\n  - 10.0.1.50/24\n";
        let states = parse_yaml(yaml).unwrap();
        assert_eq!(states.len(), 1);
    }

    /// Scenario: Parse flat bare state — entity_type is "ethernet"
    #[test]
    fn test_parse_yaml_flat_bare_state_entity_type_is_ethernet() {
        let yaml = "type: ethernet\nname: eth0\nmtu: 1500\n";
        let states = parse_yaml(yaml).unwrap();
        assert_eq!(states[0].entity_type, "ethernet");
    }

    /// Scenario: Parse flat bare state — selector.name is Some("eth0")
    #[test]
    fn test_parse_yaml_flat_bare_state_selector_name_is_eth0() {
        let yaml = "type: ethernet\nname: eth0\nmtu: 1500\n";
        let states = parse_yaml(yaml).unwrap();
        assert_eq!(states[0].selector.name, Some("eth0".to_string()));
    }

    /// Scenario: Parse flat bare state — fields contains "mtu" with Value::U64(1500)
    #[test]
    fn test_parse_yaml_flat_bare_state_mtu_field_u64_1500() {
        let yaml = "type: ethernet\nname: eth0\nmtu: 1500\n";
        let states = parse_yaml(yaml).unwrap();
        assert_eq!(states[0].fields["mtu"].value, Value::U64(1500));
    }

    /// Scenario: Parse flat bare state — addresses list contains one IpNetwork value
    #[test]
    fn test_parse_yaml_flat_bare_state_addresses_list_contains_ip_network() {
        let yaml = "type: ethernet\nname: eth0\nmtu: 1500\naddresses:\n  - 10.0.1.50/24\n";
        let states = parse_yaml(yaml).unwrap();
        let addrs = &states[0].fields["addresses"].value;
        let list = addrs.as_list().expect("addresses should be a list");
        assert_eq!(list.len(), 1);
        let expected_net: Ipv4Network = "10.0.1.50/24".parse().unwrap();
        assert_eq!(list[0], Value::IpNetwork(expected_net));
    }

    /// Scenario: Parse bare state with driver selector — selector.driver is Some("ixgbe")
    #[test]
    fn test_parse_yaml_driver_selector_driver_is_ixgbe() {
        let yaml = "type: ethernet\ndriver: ixgbe\nmtu: 9000\n";
        let states = parse_yaml(yaml).unwrap();
        assert_eq!(states[0].selector.driver, Some("ixgbe".to_string()));
    }

    /// Scenario: Parse bare state with driver selector — selector.name is None
    #[test]
    fn test_parse_yaml_driver_selector_name_is_none() {
        let yaml = "type: ethernet\ndriver: ixgbe\nmtu: 9000\n";
        let states = parse_yaml(yaml).unwrap();
        assert_eq!(states[0].selector.name, None);
    }

    /// Scenario: Parse bare state with driver selector — mtu field is 9000
    #[test]
    fn test_parse_yaml_driver_selector_mtu_9000() {
        let yaml = "type: ethernet\ndriver: ixgbe\nmtu: 9000\n";
        let states = parse_yaml(yaml).unwrap();
        assert_eq!(states[0].fields["mtu"].value, Value::U64(9000));
    }

    /// Scenario: Parse explicit format with kind: state — returns one State with mtu=9000
    #[test]
    fn test_parse_yaml_explicit_kind_state_mtu_9000() {
        let yaml = "kind: state\ntype: ethernet\nname: eth0\nmtu: 9000\n";
        let states = parse_yaml(yaml).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].fields["mtu"].value, Value::U64(9000));
    }

    /// Scenario: kind field is not stored on the State
    #[test]
    fn test_parse_yaml_explicit_kind_not_stored_in_fields() {
        let yaml = "kind: state\ntype: ethernet\nname: eth0\nmtu: 9000\n";
        let states = parse_yaml(yaml).unwrap();
        assert!(
            !states[0].fields.contains_key("kind"),
            "kind should not be stored in fields"
        );
    }

    /// Scenario: Bare and explicit formats produce identical entity_type, selector, and fields
    #[test]
    fn test_parse_yaml_bare_and_explicit_produce_same_structure() {
        let bare = "type: ethernet\nname: eth0\nmtu: 9000\n";
        let explicit = "kind: state\ntype: ethernet\nname: eth0\nmtu: 9000\n";
        let bare_states = parse_yaml(bare).unwrap();
        let explicit_states = parse_yaml(explicit).unwrap();
        assert_eq!(bare_states[0].entity_type, explicit_states[0].entity_type);
        assert_eq!(bare_states[0].selector.name, explicit_states[0].selector.name);
        assert_eq!(
            bare_states[0].fields["mtu"].value,
            explicit_states[0].fields["mtu"].value
        );
    }

    /// Scenario: Parse multi-document YAML — returns two State values
    #[test]
    fn test_parse_yaml_multi_document_returns_two_states() {
        let yaml = "type: ethernet\nname: eth0\nmtu: 1500\n---\ntype: ethernet\nname: eth1\nmtu: 9000\n";
        let states = parse_yaml(yaml).unwrap();
        assert_eq!(states.len(), 2);
    }

    /// Scenario: Multi-document — first state has selector.name "eth0" and mtu 1500
    #[test]
    fn test_parse_yaml_multi_document_first_state_eth0_mtu_1500() {
        let yaml = "type: ethernet\nname: eth0\nmtu: 1500\n---\ntype: ethernet\nname: eth1\nmtu: 9000\n";
        let states = parse_yaml(yaml).unwrap();
        assert_eq!(states[0].selector.name, Some("eth0".to_string()));
        assert_eq!(states[0].fields["mtu"].value, Value::U64(1500));
    }

    /// Scenario: Multi-document — second state has selector.name "eth1" and mtu 9000
    #[test]
    fn test_parse_yaml_multi_document_second_state_eth1_mtu_9000() {
        let yaml = "type: ethernet\nname: eth0\nmtu: 1500\n---\ntype: ethernet\nname: eth1\nmtu: 9000\n";
        let states = parse_yaml(yaml).unwrap();
        assert_eq!(states[1].selector.name, Some("eth1".to_string()));
        assert_eq!(states[1].fields["mtu"].value, Value::U64(9000));
    }

    /// Scenario: Parse route objects in fields — "routes" is a Value::List
    #[test]
    fn test_parse_yaml_route_objects_routes_is_a_list() {
        let yaml = "type: ethernet\nname: eth0\nroutes:\n  - destination: 0.0.0.0/0\n    gateway: 10.0.1.1\n    metric: 100\n";
        let states = parse_yaml(yaml).unwrap();
        assert!(
            states[0].fields["routes"].value.as_list().is_some(),
            "routes should be a Value::List"
        );
    }

    /// Scenario: Parse route objects — first element is a Value::Map with keys destination, gateway, metric
    #[test]
    fn test_parse_yaml_route_objects_first_element_is_map_with_expected_keys() {
        let yaml = "type: ethernet\nname: eth0\nroutes:\n  - destination: 0.0.0.0/0\n    gateway: 10.0.1.1\n    metric: 100\n";
        let states = parse_yaml(yaml).unwrap();
        let routes = states[0].fields["routes"].value.as_list().unwrap();
        let route_map = routes[0].as_map().expect("route element should be a map");
        assert!(route_map.contains_key("destination"), "map should have 'destination'");
        assert!(route_map.contains_key("gateway"), "map should have 'gateway'");
        assert!(route_map.contains_key("metric"), "map should have 'metric'");
    }

    /// Scenario: Selector properties are not in fields — name and driver go to selector only
    #[test]
    fn test_parse_yaml_selector_properties_name_and_driver_not_in_fields() {
        let yaml = "type: ethernet\nname: eth0\ndriver: e1000\nmtu: 1500\n";
        let states = parse_yaml(yaml).unwrap();
        let state = &states[0];
        assert_eq!(state.selector.name, Some("eth0".to_string()));
        assert_eq!(state.selector.driver, Some("e1000".to_string()));
        assert!(
            !state.fields.contains_key("name"),
            "name should not appear in fields"
        );
        assert!(
            !state.fields.contains_key("driver"),
            "driver should not appear in fields"
        );
    }

    /// Scenario: Selector properties are not in fields — only mtu is in fields
    #[test]
    fn test_parse_yaml_only_config_properties_in_fields() {
        let yaml = "type: ethernet\nname: eth0\ndriver: e1000\nmtu: 1500\n";
        let states = parse_yaml(yaml).unwrap();
        assert!(states[0].fields.contains_key("mtu"), "mtu should be in fields");
        assert_eq!(states[0].fields.len(), 1, "only mtu should be in fields");
    }

    /// type field is classified as a meta property and is not stored in fields
    #[test]
    fn test_parse_yaml_type_not_stored_in_fields() {
        let yaml = "type: ethernet\nname: eth0\n";
        let states = parse_yaml(yaml).unwrap();
        assert!(
            !states[0].fields.contains_key("type"),
            "type should not appear in fields"
        );
    }

    /// Missing type field returns MissingType error
    #[test]
    fn test_parse_yaml_missing_type_returns_missing_type_error() {
        let yaml = "name: eth0\nmtu: 1500\n";
        let result = parse_yaml(yaml);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), YamlError::MissingType));
    }

    /// Invalid kind value returns InvalidKind error
    #[test]
    fn test_parse_yaml_invalid_kind_value_returns_invalid_kind_error() {
        let yaml = "kind: policy\ntype: ethernet\nname: eth0\n";
        let result = parse_yaml(yaml);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), YamlError::InvalidKind(_)));
    }

    /// A trailing `---` produces a null document that is silently skipped
    #[test]
    fn test_parse_yaml_trailing_separator_is_skipped() {
        let yaml = "type: ethernet\nname: eth0\nmtu: 1500\n---\n";
        let states = parse_yaml(yaml).unwrap();
        assert_eq!(states.len(), 1);
    }

    /// FieldValue from YAML parse has UserConfigured provenance with empty policy_ref
    #[test]
    fn test_parse_yaml_field_provenance_is_user_configured_with_empty_policy_ref() {
        let yaml = "type: ethernet\nname: eth0\nmtu: 1500\n";
        let states = parse_yaml(yaml).unwrap();
        match &states[0].fields["mtu"].provenance {
            Provenance::UserConfigured { policy_ref } => {
                assert!(
                    policy_ref.is_empty(),
                    "policy_ref should be an empty string for standalone YAML"
                );
            }
            other => panic!("expected UserConfigured provenance, got {:?}", other),
        }
    }

    // ── state_to_yaml (bare format) ───────────────────────────────────────────

    /// Scenario: Serialize State to flat format — output contains "type: ethernet"
    #[test]
    fn test_state_to_yaml_contains_type_ethernet() {
        let state = make_state("ethernet", "eth0", 1500);
        let yaml = state_to_yaml(&state).unwrap();
        assert!(
            yaml.contains("type: ethernet"),
            "yaml should contain 'type: ethernet', got:\n{yaml}"
        );
    }

    /// Scenario: Serialize State to flat format — "name: eth0" at top level (not nested)
    #[test]
    fn test_state_to_yaml_contains_name_at_top_level() {
        let state = make_state("ethernet", "eth0", 1500);
        let yaml = state_to_yaml(&state).unwrap();
        assert!(
            yaml.contains("name: eth0"),
            "yaml should contain 'name: eth0', got:\n{yaml}"
        );
    }

    /// Scenario: Serialize State to flat format — "mtu: 1500" at top level (not nested)
    #[test]
    fn test_state_to_yaml_contains_mtu_at_top_level() {
        let state = make_state("ethernet", "eth0", 1500);
        let yaml = state_to_yaml(&state).unwrap();
        assert!(
            yaml.contains("mtu: 1500"),
            "yaml should contain 'mtu: 1500', got:\n{yaml}"
        );
    }

    /// Scenario: Serialize State to flat format — does not contain "kind:"
    #[test]
    fn test_state_to_yaml_does_not_contain_kind_field() {
        let state = make_state("ethernet", "eth0", 1500);
        let yaml = state_to_yaml(&state).unwrap();
        assert!(
            !yaml.contains("kind:"),
            "bare yaml should not contain 'kind:', got:\n{yaml}"
        );
    }

    /// Scenario: Serialize State to flat format — no "selector:" nesting
    #[test]
    fn test_state_to_yaml_does_not_contain_selector_key() {
        let state = make_state("ethernet", "eth0", 1500);
        let yaml = state_to_yaml(&state).unwrap();
        assert!(
            !yaml.contains("selector:"),
            "flat yaml should not have 'selector:' nesting, got:\n{yaml}"
        );
    }

    /// Scenario: Serialize State to flat format — no "fields:" nesting
    #[test]
    fn test_state_to_yaml_does_not_contain_fields_key() {
        let state = make_state("ethernet", "eth0", 1500);
        let yaml = state_to_yaml(&state).unwrap();
        assert!(
            !yaml.contains("fields:"),
            "flat yaml should not have 'fields:' nesting, got:\n{yaml}"
        );
    }

    // ── state_to_yaml_explicit ────────────────────────────────────────────────

    /// Scenario: Serialize State with explicit kind — output contains "kind: state"
    #[test]
    fn test_state_to_yaml_explicit_contains_kind_state() {
        let state = make_state("ethernet", "eth0", 1500);
        let yaml = state_to_yaml_explicit(&state).unwrap();
        assert!(
            yaml.contains("kind: state"),
            "explicit yaml should contain 'kind: state', got:\n{yaml}"
        );
    }

    /// Scenario: kind: state is the first field in explicit format
    #[test]
    fn test_state_to_yaml_explicit_kind_appears_before_type() {
        let state = make_state("ethernet", "eth0", 1500);
        let yaml = state_to_yaml_explicit(&state).unwrap();
        let kind_pos = yaml.find("kind:").expect("should contain 'kind:'");
        let type_pos = yaml.find("type:").expect("should contain 'type:'");
        assert!(
            kind_pos < type_pos,
            "kind: should appear before type: in explicit format"
        );
    }

    // ── Round-trip ────────────────────────────────────────────────────────────

    /// Scenario: Round-trip preserves entity_type
    #[test]
    fn test_round_trip_yaml_preserves_entity_type() {
        let state = make_state("ethernet", "eth0", 1500);
        let yaml = state_to_yaml(&state).unwrap();
        let restored = &parse_yaml(&yaml).unwrap()[0];
        assert_eq!(restored.entity_type, "ethernet");
    }

    /// Scenario: Round-trip preserves selector.name
    #[test]
    fn test_round_trip_yaml_preserves_selector_name() {
        let state = make_state("ethernet", "eth0", 1500);
        let yaml = state_to_yaml(&state).unwrap();
        let restored = &parse_yaml(&yaml).unwrap()[0];
        assert_eq!(restored.selector.name, Some("eth0".to_string()));
    }

    /// Scenario: Round-trip preserves field values
    #[test]
    fn test_round_trip_yaml_preserves_mtu_field_value() {
        let state = make_state("ethernet", "eth0", 1500);
        let yaml = state_to_yaml(&state).unwrap();
        let restored = &parse_yaml(&yaml).unwrap()[0];
        assert_eq!(restored.fields["mtu"].value, Value::U64(1500));
    }

    /// Scenario: Round-trip — metadata is regenerated (not preserved through YAML)
    #[test]
    fn test_round_trip_yaml_metadata_id_is_regenerated() {
        let state = make_state("ethernet", "eth0", 1500);
        let original_id = state.metadata.id;
        let yaml = state_to_yaml(&state).unwrap();
        let restored = &parse_yaml(&yaml).unwrap()[0];
        assert_ne!(
            restored.metadata.id,
            original_id,
            "metadata.id should be regenerated after YAML round-trip"
        );
    }

    /// Scenario: Round-trip with various field types preserves all values.
    ///
    /// String, U64, Bool, IpAddr, IpNetwork (with prefix), List, and Map all survive
    /// a YAML round-trip.
    #[test]
    fn test_round_trip_yaml_various_field_types() {
        let net: Ipv4Network = "10.0.1.0/24".parse().unwrap();
        let ip: Ipv4Addr = "10.0.1.1".parse().unwrap();

        let mut inner_map = IndexMap::new();
        inner_map.insert("proto".to_string(), Value::String("tcp".to_string()));

        let mut fields = IndexMap::new();
        fields.insert("mtu".to_string(), make_fv(Value::U64(9000)));
        fields.insert("enabled".to_string(), make_fv(Value::Bool(true)));
        fields.insert("label".to_string(), make_fv(Value::String("uplink".to_string())));
        fields.insert("gateway".to_string(), make_fv(Value::IpAddr(ip)));
        fields.insert("network".to_string(), make_fv(Value::IpNetwork(net)));
        fields.insert(
            "tags".to_string(),
            make_fv(Value::List(vec![
                Value::String("prod".to_string()),
                Value::String("core".to_string()),
            ])),
        );
        fields.insert("opts".to_string(), make_fv(Value::Map(inner_map)));

        let state = State {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            fields,
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 100,
        };

        let yaml = state_to_yaml(&state).unwrap();
        let restored = &parse_yaml(&yaml).unwrap()[0];

        assert_eq!(restored.entity_type, "ethernet");
        assert_eq!(restored.selector.name, Some("eth0".to_string()));
        assert_eq!(restored.fields["mtu"].value, Value::U64(9000));
        assert_eq!(restored.fields["enabled"].value, Value::Bool(true));
        assert_eq!(restored.fields["label"].value, Value::String("uplink".to_string()));
        assert_eq!(restored.fields["gateway"].value, Value::IpAddr(ip));
        assert_eq!(restored.fields["network"].value, Value::IpNetwork(net));

        let tags = restored.fields["tags"].value.as_list().unwrap();
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0], Value::String("prod".to_string()));
        assert_eq!(tags[1], Value::String("core".to_string()));

        let opts = restored.fields["opts"].value.as_map().unwrap();
        assert_eq!(opts.get("proto"), Some(&Value::String("tcp".to_string())));
    }

    /// Scenario: Parse route objects — destination is IpNetwork, gateway is IpAddr, metric is U64
    #[test]
    fn test_parse_yaml_route_values_are_correctly_typed() {
        let yaml = "type: ethernet\nname: eth0\nroutes:\n  - destination: 0.0.0.0/0\n    gateway: 10.0.1.1\n    metric: 100\n";
        let states = parse_yaml(yaml).unwrap();
        let routes = states[0].fields["routes"].value.as_list().unwrap();
        let route_map = routes[0].as_map().unwrap();

        let expected_net: Ipv4Network = "0.0.0.0/0".parse().unwrap();
        assert_eq!(
            route_map.get("destination"),
            Some(&Value::IpNetwork(expected_net)),
            "destination should be Value::IpNetwork"
        );

        let expected_gw: Ipv4Addr = "10.0.1.1".parse().unwrap();
        assert_eq!(
            route_map.get("gateway"),
            Some(&Value::IpAddr(expected_gw)),
            "gateway should be Value::IpAddr"
        );

        assert_eq!(
            route_map.get("metric"),
            Some(&Value::U64(100)),
            "metric should be Value::U64"
        );
    }

    /// Scenario: Address list ordering — element order from YAML is preserved through parsing
    #[test]
    fn test_parse_yaml_list_element_order_is_preserved() {
        // Three addresses in a specific order; the first should stay first after parsing.
        let yaml = "type: ethernet\nname: eth0\naddresses:\n  - 10.0.1.50/24\n  - 10.0.2.50/24\n  - 10.0.3.50/24\n";
        let states = parse_yaml(yaml).unwrap();
        let list = states[0].fields["addresses"].value.as_list().unwrap();

        assert_eq!(list.len(), 3, "all three addresses should be present");

        let n1: Ipv4Network = "10.0.1.50/24".parse().unwrap();
        let n2: Ipv4Network = "10.0.2.50/24".parse().unwrap();
        let n3: Ipv4Network = "10.0.3.50/24".parse().unwrap();

        assert_eq!(list[0], Value::IpNetwork(n1), "first address should be 10.0.1.50/24");
        assert_eq!(list[1], Value::IpNetwork(n2), "second address should be 10.0.2.50/24");
        assert_eq!(list[2], Value::IpNetwork(n3), "third address should be 10.0.3.50/24");
    }

    /// Scenario: mac selector key in YAML is parsed into selector.mac (not fields)
    #[test]
    fn test_parse_yaml_mac_selector_parsed_to_selector_mac() {
        let yaml = "type: ethernet\nmac: aa:bb:cc:dd:ee:ff\nmtu: 1500\n";
        let states = parse_yaml(yaml).unwrap();
        let state = &states[0];

        // mac must land on the selector, not in fields
        assert!(
            state.selector.mac.is_some(),
            "mac should be parsed into selector.mac"
        );
        let mac = state.selector.mac.as_ref().unwrap();
        assert_eq!(mac.to_string(), "aa:bb:cc:dd:ee:ff");

        assert!(
            !state.fields.contains_key("mac"),
            "mac should not appear in fields"
        );
    }

    /// Scenario: A `Value::IpAddr` field survives a YAML round-trip correctly.
    #[test]
    fn test_round_trip_yaml_ip_addr_round_trips_correctly() {
        let ip: Ipv4Addr = "10.0.1.1".parse().unwrap();

        let mut fields = IndexMap::new();
        fields.insert("gateway".to_string(), make_fv(Value::IpAddr(ip)));

        let state = State {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            fields,
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 100,
        };

        let yaml = state_to_yaml(&state).unwrap();
        let restored = &parse_yaml(&yaml).unwrap()[0];

        assert_eq!(restored.fields["gateway"].value, Value::IpAddr(ip));
    }
}
