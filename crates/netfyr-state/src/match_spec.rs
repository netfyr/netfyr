//! The `Match` type: a selector identifying which device a state targets.

use std::fmt;

/// Identifies which system device a [`crate::State`] targets.
///
/// All fields are optional. Matching is AND over the fields set in `self`
/// and asymmetric: [`Match::matches`] answers "does `self` match `other`".
/// A field set in `self` that is unset in `other` is a mismatch. That lets
/// a partial match from a policy match a fully populated match from the
/// kernel without the reverse also holding.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Match {
    /// Interface name, e.g. `"eth0"`.
    pub name: Option<String>,
    /// Device type, e.g. `"ethernet"`.
    pub r#type: Option<String>,
    /// Driver name, e.g. `"ixgbe"`.
    pub driver: Option<String>,
    /// PCI path of the device.
    pub pci_path: Option<String>,
    /// MAC address. Compared case-insensitively by [`Match::matches`], but
    /// stored verbatim.
    pub mac: Option<String>,
}

impl Match {
    /// Whether `self` matches `other`: every field
    /// set in `self` must be set in `other` and have equal content; unset
    /// fields in `self` match anything. `mac` compares
    /// case-insensitively.
    pub fn matches(&self, other: &Match) -> bool {
        field_matches(&self.name, &other.name)
            && field_matches(&self.r#type, &other.r#type)
            && field_matches(&self.driver, &other.driver)
            && field_matches(&self.pci_path, &other.pci_path)
            && match (&self.mac, &other.mac) {
                (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                (Some(_), None) => false,
                (None, _) => true,
            }
    }

    /// Whether no field is set. A match-less document parses to this, and
    /// the apply path auto-generates a match for states in this state.
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.r#type.is_none()
            && self.driver.is_none()
            && self.pci_path.is_none()
            && self.mac.is_none()
    }
}

impl fmt::Display for Match {
    /// The set fields as `name=value` pairs in a fixed order, or `*` when no
    /// field is set (the empty selector matches every device).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if let Some(name) = &self.name {
            parts.push(format!("name={name}"));
        }
        if let Some(value) = &self.r#type {
            parts.push(format!("type={value}"));
        }
        if let Some(driver) = &self.driver {
            parts.push(format!("driver={driver}"));
        }
        if let Some(pci_path) = &self.pci_path {
            parts.push(format!("pci_path={pci_path}"));
        }
        if let Some(mac) = &self.mac {
            parts.push(format!("mac={mac}"));
        }
        if parts.is_empty() {
            f.write_str("*")
        } else {
            f.write_str(&parts.join(", "))
        }
    }
}

fn field_matches(self_field: &Option<String>, other_field: &Option<String>) -> bool {
    match (self_field, other_field) {
        (Some(a), Some(b)) => a == b,
        (Some(_), None) => false,
        (None, _) => true,
    }
}

#[cfg(test)]
mod tests {
    use crate::Match;

    fn mk(
        name: Option<&str>,
        r#type: Option<&str>,
        driver: Option<&str>,
        pci_path: Option<&str>,
        mac: Option<&str>,
    ) -> Match {
        Match {
            name: name.map(str::to_string),
            r#type: r#type.map(str::to_string),
            driver: driver.map(str::to_string),
            pci_path: pci_path.map(str::to_string),
            mac: mac.map(str::to_string),
        }
    }

    /// A fully populated match, modeling the live device the scenarios
    /// compare against (devices are not a type in this crate).
    fn full() -> Match {
        mk(
            Some("eth0"),
            Some("ethernet"),
            Some("ixgbe"),
            Some("0000:00:02.0"),
            Some("aa:bb:cc:dd:ee:ff"),
        )
    }

    #[test]
    fn name_only_matches() {
        assert!(mk(Some("eth0"), None, None, None, None).matches(&full()));
    }

    #[test]
    fn name_only_mismatch() {
        let other = mk(Some("eth1"), Some("ethernet"), Some("ixgbe"), None, None);
        assert!(!mk(Some("eth0"), None, None, None, None).matches(&other));
    }

    #[test]
    fn name_type_matches() {
        assert!(mk(Some("eth0"), Some("ethernet"), None, None, None).matches(&full()));
    }

    #[test]
    fn name_type_mismatch() {
        let other = mk(Some("eth1"), Some("ethernet"), None, None, None);
        assert!(!mk(Some("eth0"), Some("ethernet"), None, None, None).matches(&other));
    }

    #[test]
    fn and_logic_all_set_fields_must_match() {
        let a = mk(None, None, Some("ixgbe"), Some("0000:00:02.0"), None);
        let b = mk(None, None, Some("ixgbe"), Some("0000:00:02.0"), None);
        assert!(a.matches(&b));
        // One differing set field is enough to break the AND.
        let c = mk(None, None, Some("ixgbe"), Some("0000:00:03.0"), None);
        assert!(!a.matches(&c));
    }

    #[test]
    fn asymmetry_partial_matches_full_but_not_reverse() {
        let partial = mk(Some("eth0"), None, None, None, None);
        let full = full();
        assert!(partial.matches(&full));
        // A field set in `self` and unset in `other` is a mismatch, so the
        // reverse does not hold.
        assert!(!full.matches(&partial));
    }

    #[test]
    fn mac_case_insensitive_both_orders() {
        let upper = mk(None, None, None, None, Some("AA:BB:CC:DD:EE:FF"));
        let lower = mk(None, None, None, None, Some("aa:bb:cc:dd:ee:ff"));
        assert!(upper.matches(&lower));
        assert!(lower.matches(&upper));
    }

    #[test]
    fn mac_matching_does_not_normalize_storage() {
        let upper = mk(None, None, None, None, Some("AA:BB:CC:DD:EE:FF"));
        let lower = mk(None, None, None, None, Some("aa:bb:cc:dd:ee:ff"));
        let _ = upper.matches(&lower);
        // Matching compares case-insensitively but stores verbatim.
        assert_eq!(upper.mac.as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        assert_eq!(lower.mac.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
    }

    #[test]
    fn set_in_self_unset_in_other_is_mismatch() {
        let empty = Match::default();
        assert!(!mk(Some("eth0"), None, None, None, None).matches(&empty));
        assert!(!mk(None, Some("ethernet"), None, None, None).matches(&empty));
        assert!(!mk(None, None, Some("ixgbe"), None, None).matches(&empty));
        assert!(!mk(None, None, None, Some("0000:00:02.0"), None).matches(&empty));
        assert!(!mk(None, None, None, None, Some("aa:bb:cc:dd:ee:ff")).matches(&empty));
    }

    #[test]
    fn empty_match_matches_anything() {
        assert!(Match::default().matches(&full()));
        assert!(Match::default().is_empty());
        assert!(!mk(Some("eth0"), None, None, None, None).is_empty());
    }
}
