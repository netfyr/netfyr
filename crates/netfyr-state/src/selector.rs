//! Selector types for targeting system entities.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

// ── MacAddrParseError ─────────────────────────────────────────────────────────

/// Error returned when parsing a MAC address string fails.
#[derive(Clone, Debug, PartialEq)]
pub struct MacAddrParseError;

impl fmt::Display for MacAddrParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid MAC address; expected format AA:BB:CC:DD:EE:FF")
    }
}

impl std::error::Error for MacAddrParseError {}

// ── MacAddr ───────────────────────────────────────────────────────────────────

/// A 6-byte hardware (MAC) address.
///
/// Stored as raw bytes so equality comparison is always case-insensitive.
/// Serialized/deserialized as a lowercase colon-separated hex string.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MacAddr(pub [u8; 6]);

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        )
    }
}

impl FromStr for MacAddr {
    type Err = MacAddrParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 6 {
            return Err(MacAddrParseError);
        }
        let mut bytes = [0u8; 6];
        for (i, part) in parts.iter().enumerate() {
            bytes[i] = u8::from_str_radix(part, 16).map_err(|_| MacAddrParseError)?;
        }
        Ok(MacAddr(bytes))
    }
}

impl Serialize for MacAddr {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for MacAddr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

// ── Selector ──────────────────────────────────────────────────────────────────

/// Identifies which system entity a state targets.
///
/// All specified (non-None, non-empty) fields must match for `matches()` to
/// return true (AND logic). Unspecified fields match anything.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Selector {
    /// Exact interface or entity name (e.g., `"eth0"`, `"bond0.100"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Technology type filter (e.g., `"ethernet"`, `"wifi"`).
    /// Serialized as `"type"` because `type` is a Rust keyword.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    /// Kernel driver name (e.g., `"ixgbe"`, `"mlx5_core"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// PCI device path for stable hardware identification (e.g., `"0000:03:00.0"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_path: Option<String>,
    /// MAC address (6-byte hardware address).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<MacAddr>,
    /// User-defined key-value labels; all specified labels must be present with
    /// matching values on the target (subset matching).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,
}

impl Selector {
    /// Returns a selector with all fields unset (matches everything).
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a selector targeting a specific named entity.
    pub fn with_name(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            ..Default::default()
        }
    }

    /// Returns `true` if all fields set in `self` match the corresponding
    /// fields in `other`. Unspecified fields (None / empty labels) in `self`
    /// match anything in `other`.
    pub fn matches(&self, other: &Selector) -> bool {
        if let Some(ref v) = self.name {
            if other.name.as_deref() != Some(v.as_str()) {
                return false;
            }
        }
        if let Some(ref v) = self.type_ {
            if other.type_.as_deref() != Some(v.as_str()) {
                return false;
            }
        }
        if let Some(ref v) = self.driver {
            if other.driver.as_deref() != Some(v.as_str()) {
                return false;
            }
        }
        if let Some(ref v) = self.pci_path {
            if other.pci_path.as_deref() != Some(v.as_str()) {
                return false;
            }
        }
        if let Some(ref v) = self.mac {
            if other.mac.as_ref() != Some(v) {
                return false;
            }
        }
        for (k, v) in &self.labels {
            if other.labels.get(k) != Some(v) {
                return false;
            }
        }
        true
    }

    /// Returns `true` if the selector has a `name` set, meaning it targets a
    /// single known entity rather than a class of entities.
    pub fn is_specific(&self) -> bool {
        self.name.is_some()
    }

    /// Returns a stable string key used for indexing in a StateSet.
    ///
    /// When `name` is set, returns the name directly. Otherwise, produces a
    /// deterministic semicolon-delimited encoding of all set fields in
    /// alphabetical order (e.g., `"driver=ixgbe;type=ethernet"`).
    ///
    /// A fully-empty selector returns `""`.
    pub fn key(&self) -> String {
        if let Some(ref n) = self.name {
            return n.clone();
        }

        let mut parts: Vec<String> = Vec::new();

        if let Some(ref v) = self.driver {
            parts.push(format!("driver={v}"));
        }
        if let Some(ref v) = self.type_ {
            parts.push(format!("type={v}"));
        }
        if let Some(ref v) = self.mac {
            parts.push(format!("mac={v}"));
        }
        if let Some(ref v) = self.pci_path {
            parts.push(format!("pci_path={v}"));
        }

        // Sort labels by key for determinism (HashMap iteration order is unspecified).
        let mut label_pairs: Vec<(&String, &String)> = self.labels.iter().collect();
        label_pairs.sort_by_key(|(k, _)| k.as_str());
        for (k, v) in label_pairs {
            parts.push(format!("labels.{k}={v}"));
        }

        // Sort all parts together so labels interleave correctly with field names.
        parts.sort();
        parts.join(";")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── MacAddr parsing and formatting ────────────────────────────────────────

    #[test]
    fn test_mac_addr_parse_uppercase_and_display_lowercase() {
        let mac: MacAddr = "AA:BB:CC:DD:EE:FF".parse().expect("should parse");
        assert_eq!(mac.to_string(), "aa:bb:cc:dd:ee:ff");
    }

    #[test]
    fn test_mac_addr_parse_lowercase() {
        let mac: MacAddr = "aa:bb:cc:dd:ee:ff".parse().expect("should parse");
        assert_eq!(mac.0, [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
    }

    #[test]
    fn test_mac_addr_rejects_invalid_format() {
        assert!("not-a-mac".parse::<MacAddr>().is_err());
    }

    #[test]
    fn test_mac_addr_rejects_too_few_octets() {
        assert!("aa:bb:cc:dd:ee".parse::<MacAddr>().is_err());
    }

    #[test]
    fn test_mac_addr_rejects_too_many_octets() {
        assert!("aa:bb:cc:dd:ee:ff:00".parse::<MacAddr>().is_err());
    }

    #[test]
    fn test_mac_addr_rejects_non_hex_chars() {
        assert!("gg:bb:cc:dd:ee:ff".parse::<MacAddr>().is_err());
    }

    #[test]
    fn test_mac_addr_equality_is_case_insensitive() {
        let upper: MacAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let lower: MacAddr = "aa:bb:cc:dd:ee:ff".parse().unwrap();
        assert_eq!(upper, lower);
    }

    #[test]
    fn test_mac_addr_serialize_deserialize_roundtrip() {
        let mac: MacAddr = "aa:bb:cc:dd:ee:ff".parse().unwrap();
        let serialized = serde_yaml::to_string(&mac).unwrap();
        let deserialized: MacAddr = serde_yaml::from_str(&serialized).unwrap();
        assert_eq!(mac, deserialized);
    }

    // ── matches(): name field ─────────────────────────────────────────────────

    #[test]
    fn test_matches_exact_name_succeeds() {
        let selector = Selector::with_name("eth0");
        let target = Selector {
            name: Some("eth0".into()),
            driver: Some("ixgbe".into()),
            ..Default::default()
        };
        assert!(selector.matches(&target));
    }

    #[test]
    fn test_matches_name_mismatch_fails() {
        let selector = Selector::with_name("eth0");
        let target = Selector::with_name("eth1");
        assert!(!selector.matches(&target));
    }

    // ── matches(): multi-field AND logic ──────────────────────────────────────

    #[test]
    fn test_matches_multi_field_and_all_match() {
        let selector = Selector {
            driver: Some("ixgbe".into()),
            type_: Some("ethernet".into()),
            ..Default::default()
        };
        let target = Selector {
            name: Some("eth0".into()),
            driver: Some("ixgbe".into()),
            type_: Some("ethernet".into()),
            ..Default::default()
        };
        assert!(selector.matches(&target));
    }

    #[test]
    fn test_matches_multi_field_and_one_mismatch_fails() {
        let selector = Selector {
            driver: Some("ixgbe".into()),
            type_: Some("wifi".into()),
            ..Default::default()
        };
        let target = Selector {
            name: Some("eth0".into()),
            driver: Some("ixgbe".into()),
            type_: Some("ethernet".into()),
            ..Default::default()
        };
        assert!(!selector.matches(&target));
    }

    // ── matches(): unspecified fields match anything ───────────────────────────

    #[test]
    fn test_matches_unspecified_fields_match_anything() {
        let selector = Selector {
            driver: Some("ixgbe".into()),
            ..Default::default()
        };
        let target = Selector {
            name: Some("eth0".into()),
            driver: Some("ixgbe".into()),
            type_: Some("ethernet".into()),
            pci_path: Some("0000:03:00.0".into()),
            ..Default::default()
        };
        assert!(selector.matches(&target));
    }

    #[test]
    fn test_matches_empty_selector_matches_everything() {
        let selector = Selector::new();
        let target = Selector {
            name: Some("eth0".into()),
            driver: Some("ixgbe".into()),
            type_: Some("ethernet".into()),
            ..Default::default()
        };
        assert!(selector.matches(&target));
    }

    #[test]
    fn test_matches_empty_selector_matches_empty_target() {
        assert!(Selector::new().matches(&Selector::new()));
    }

    // ── matches(): label subset logic ─────────────────────────────────────────

    #[test]
    fn test_matches_label_subset_succeeds() {
        let mut selector = Selector::new();
        selector.labels.insert("role".into(), "uplink".into());

        let mut target = Selector::new();
        target.labels.insert("role".into(), "uplink".into());
        target.labels.insert("env".into(), "prod".into());

        assert!(selector.matches(&target));
    }

    #[test]
    fn test_matches_label_missing_in_target_fails() {
        let mut selector = Selector::new();
        selector.labels.insert("role".into(), "uplink".into());
        selector.labels.insert("env".into(), "staging".into());

        let mut target = Selector::new();
        target.labels.insert("role".into(), "uplink".into());

        assert!(!selector.matches(&target));
    }

    #[test]
    fn test_matches_label_value_mismatch_fails() {
        let mut selector = Selector::new();
        selector.labels.insert("role".into(), "downlink".into());

        let mut target = Selector::new();
        target.labels.insert("role".into(), "uplink".into());

        assert!(!selector.matches(&target));
    }

    // ── matches(): MAC address ────────────────────────────────────────────────

    #[test]
    fn test_matches_mac_case_insensitive() {
        let lower_mac: MacAddr = "aa:bb:cc:dd:ee:ff".parse().unwrap();
        let upper_mac: MacAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();

        let selector = Selector {
            mac: Some(lower_mac),
            ..Default::default()
        };
        let target = Selector {
            name: Some("eth0".into()),
            mac: Some(upper_mac),
            ..Default::default()
        };
        assert!(selector.matches(&target));
    }

    #[test]
    fn test_matches_mac_mismatch_fails() {
        let selector = Selector {
            mac: Some("aa:bb:cc:dd:ee:ff".parse().unwrap()),
            ..Default::default()
        };
        let target = Selector {
            mac: Some("11:22:33:44:55:66".parse().unwrap()),
            ..Default::default()
        };
        assert!(!selector.matches(&target));
    }

    // ── matches(): pci_path field ─────────────────────────────────────────────

    #[test]
    fn test_matches_pci_path_succeeds() {
        let selector = Selector {
            pci_path: Some("0000:03:00.0".into()),
            ..Default::default()
        };
        let target = Selector {
            name: Some("eth0".into()),
            pci_path: Some("0000:03:00.0".into()),
            ..Default::default()
        };
        assert!(selector.matches(&target));
    }

    #[test]
    fn test_matches_pci_path_mismatch_fails() {
        let selector = Selector {
            pci_path: Some("0000:03:00.0".into()),
            ..Default::default()
        };
        let target = Selector {
            pci_path: Some("0000:04:00.0".into()),
            ..Default::default()
        };
        assert!(!selector.matches(&target));
    }

    // ── is_specific() ─────────────────────────────────────────────────────────

    #[test]
    fn test_is_specific_true_for_named_selector() {
        let selector = Selector::with_name("eth0");
        assert!(selector.is_specific());
    }

    #[test]
    fn test_is_specific_false_for_unnamed_selector() {
        let selector = Selector {
            driver: Some("ixgbe".into()),
            ..Default::default()
        };
        assert!(!selector.is_specific());
    }

    #[test]
    fn test_is_specific_false_for_empty_selector() {
        assert!(!Selector::new().is_specific());
    }

    // ── key() ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_key_returns_name_when_set() {
        let selector = Selector::with_name("eth0");
        assert_eq!(selector.key(), "eth0");
    }

    #[test]
    fn test_key_deterministic_without_name() {
        let selector = Selector {
            driver: Some("ixgbe".into()),
            type_: Some("ethernet".into()),
            ..Default::default()
        };
        let key1 = selector.key();
        let key2 = selector.key();
        assert_eq!(key1, key2);
        assert!(key1.contains("driver=ixgbe"), "key={key1}");
        assert!(key1.contains("type=ethernet"), "key={key1}");
    }

    #[test]
    fn test_key_with_labels_is_deterministic() {
        let mut selector = Selector::new();
        selector.labels.insert("role".into(), "uplink".into());
        selector.labels.insert("env".into(), "prod".into());

        let key1 = selector.key();
        let key2 = selector.key();
        assert_eq!(key1, key2);
        assert!(key1.contains("labels.role=uplink"), "key={key1}");
        assert!(key1.contains("labels.env=prod"), "key={key1}");
    }

    #[test]
    fn test_key_driver_and_type_ordering() {
        // driver sorts before type alphabetically, so driver=ixgbe;type=ethernet
        let selector = Selector {
            driver: Some("ixgbe".into()),
            type_: Some("ethernet".into()),
            ..Default::default()
        };
        let key = selector.key();
        assert_eq!(key, "driver=ixgbe;type=ethernet");
    }

    // ── YAML serialization ────────────────────────────────────────────────────

    #[test]
    fn test_selector_serializes_only_set_fields() {
        let selector = Selector::with_name("eth0");
        let yaml = serde_yaml::to_string(&selector).unwrap();
        assert!(yaml.contains("name: eth0"), "yaml={yaml}");
        assert!(!yaml.contains("type"), "yaml should not contain 'type': {yaml}");
        assert!(!yaml.contains("driver"), "yaml should not contain 'driver': {yaml}");
        assert!(!yaml.contains("pci_path"), "yaml should not contain 'pci_path': {yaml}");
        assert!(!yaml.contains("mac"), "yaml should not contain 'mac': {yaml}");
        assert!(!yaml.contains("labels"), "yaml should not contain 'labels': {yaml}");
    }

    #[test]
    fn test_selector_yaml_roundtrip_name_only() {
        let original = Selector::with_name("bond0.100");
        let yaml = serde_yaml::to_string(&original).unwrap();
        let parsed: Selector = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_selector_yaml_type_field_renamed() {
        let selector = Selector {
            type_: Some("ethernet".into()),
            ..Default::default()
        };
        let yaml = serde_yaml::to_string(&selector).unwrap();
        // Serialized as "type:", not "type_:"
        assert!(yaml.contains("type: ethernet"), "yaml={yaml}");
        assert!(!yaml.contains("type_"), "yaml should not contain 'type_': {yaml}");
    }

    #[test]
    fn test_selector_yaml_roundtrip_multifield() {
        let mut selector = Selector {
            driver: Some("ixgbe".into()),
            type_: Some("ethernet".into()),
            pci_path: Some("0000:03:00.0".into()),
            mac: Some("aa:bb:cc:dd:ee:ff".parse().unwrap()),
            ..Default::default()
        };
        selector.labels.insert("role".into(), "uplink".into());

        let yaml = serde_yaml::to_string(&selector).unwrap();
        let parsed: Selector = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(selector, parsed);
    }

    #[test]
    fn test_selector_deserializes_from_spec_example() {
        let yaml = "type: ethernet\ndriver: ixgbe\nlabels:\n  role: uplink\n";
        let selector: Selector = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(selector.type_.as_deref(), Some("ethernet"));
        assert_eq!(selector.driver.as_deref(), Some("ixgbe"));
        assert_eq!(selector.labels.get("role").map(|s| s.as_str()), Some("uplink"));
        assert!(selector.name.is_none());
    }
}
