//! Shared query utilities for netlink-based backends.

use std::io;
use std::path::Path;

use netfyr_state::{MacAddr, Selector};
use rtnetlink::Handle;

use crate::BackendError;

/// Open a new netlink connection, spawn the connection task on the current
/// tokio runtime, and return the `Handle`.
///
/// A new connection is created per query to avoid stale-socket issues and to
/// keep `NetlinkBackend::new()` synchronous (no async constructor needed).
/// Netlink socket creation costs a single `socket()` + `bind()` syscall pair
/// and is negligible relative to the enumeration work.
pub async fn establish_connection() -> Result<Handle, BackendError> {
    let (connection, handle, _) =
        rtnetlink::new_connection().map_err(|e| {
            if e.kind() == io::ErrorKind::PermissionDenied {
                BackendError::PermissionDenied(e.to_string())
            } else {
                BackendError::QueryFailed {
                    entity_type: "ethernet".to_string(),
                    source: Box::new(e),
                }
            }
        })?;
    tokio::spawn(connection);
    Ok(handle)
}

/// Build a `Selector` that represents the discovered attributes of a link.
///
/// Used to apply `user_selector.matches(&discovered)` for filtering.
pub fn build_discovered_selector(
    name: &str,
    mac: Option<[u8; 6]>,
    driver: Option<&str>,
    pci_path: Option<&str>,
) -> Selector {
    Selector {
        name: Some(name.to_owned()),
        mac: mac.map(MacAddr),
        driver: driver.map(str::to_owned),
        pci_path: pci_path.map(str::to_owned),
        ..Default::default()
    }
}

/// Read `/sys/class/net/<name>/speed` and return the value in Mbps.
///
/// Returns `None` if the file doesn't exist, can't be read, can't be parsed,
/// or the kernel returns `-1` (sentinel for "no speed available", e.g., link
/// is down or the driver doesn't report speed).
pub fn read_sysfs_speed(name: &str) -> Option<u64> {
    let path = format!("/sys/class/net/{name}/speed");
    let content = std::fs::read_to_string(&path).ok()?;
    let trimmed = content.trim();
    // Parse as i64 first: the kernel may write "-1" when speed is unknown.
    let value: i64 = trimmed.parse().ok()?;
    if value < 0 {
        None
    } else {
        Some(value as u64)
    }
}

/// Read the driver name from `/sys/class/net/<name>/device/driver` symlink.
///
/// Returns the basename of the symlink target (e.g., `"e1000"`, `"ixgbe"`).
/// Returns `None` for virtual interfaces that have no PCI device.
pub fn read_sysfs_driver(name: &str) -> Option<String> {
    let path = format!("/sys/class/net/{name}/device/driver");
    let target = std::fs::read_link(Path::new(&path)).ok()?;
    target.file_name().map(|s| s.to_string_lossy().into_owned())
}

/// Read the PCI path from `/sys/class/net/<name>/device` symlink.
///
/// Returns the basename of the symlink target (e.g., `"0000:03:00.0"`).
/// Returns `None` for interfaces with no associated PCI device.
pub fn read_sysfs_pci_path(name: &str) -> Option<String> {
    let path = format!("/sys/class/net/{name}/device");
    let target = std::fs::read_link(Path::new(&path)).ok()?;
    target.file_name().map(|s| s.to_string_lossy().into_owned())
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use netfyr_state::MacAddr;

    // ── build_discovered_selector ─────────────────────────────────────────────

    /// Scenario: name-only selector is built correctly.
    #[test]
    fn test_build_discovered_selector_sets_name() {
        let sel = build_discovered_selector("eth0", None, None, None);
        assert_eq!(sel.name.as_deref(), Some("eth0"));
        assert!(sel.mac.is_none(), "mac must be None when not provided");
        assert!(sel.driver.is_none(), "driver must be None when not provided");
        assert!(sel.pci_path.is_none(), "pci_path must be None when not provided");
    }

    /// Scenario: MAC is stored as MacAddr bytes when provided.
    #[test]
    fn test_build_discovered_selector_sets_mac() {
        let mac = [0xAAu8, 0xBBu8, 0xCCu8, 0xDDu8, 0xEEu8, 0x01u8];
        let sel = build_discovered_selector("eth0", Some(mac), None, None);
        assert_eq!(
            sel.mac.as_ref().map(|m| m.0),
            Some(mac),
            "MAC bytes must match what was provided"
        );
    }

    /// Scenario: driver is stored when provided.
    #[test]
    fn test_build_discovered_selector_sets_driver() {
        let sel = build_discovered_selector("eth0", None, Some("ixgbe"), None);
        assert_eq!(sel.driver.as_deref(), Some("ixgbe"));
    }

    /// Scenario: pci_path is stored when provided.
    #[test]
    fn test_build_discovered_selector_sets_pci_path() {
        let sel = build_discovered_selector("eth0", None, None, Some("0000:03:00.0"));
        assert_eq!(sel.pci_path.as_deref(), Some("0000:03:00.0"));
    }

    /// Scenario: All four fields are set correctly when all are provided.
    #[test]
    fn test_build_discovered_selector_with_all_fields() {
        let mac = [0xAAu8, 0xBBu8, 0xCCu8, 0xDDu8, 0xEEu8, 0xFFu8];
        let sel = build_discovered_selector(
            "eth0",
            Some(mac),
            Some("ixgbe"),
            Some("0000:03:00.0"),
        );
        assert_eq!(sel.name.as_deref(), Some("eth0"));
        assert_eq!(sel.mac.as_ref().map(|m| m.0), Some(mac));
        assert_eq!(sel.driver.as_deref(), Some("ixgbe"));
        assert_eq!(sel.pci_path.as_deref(), Some("0000:03:00.0"));
    }

    // ── Selector matching via build_discovered_selector ───────────────────────

    /// Scenario: A user selector with name="eth0" matches the discovered selector
    /// built for the "eth0" interface.
    #[test]
    fn test_build_discovered_selector_name_match() {
        let user_sel = Selector::with_name("eth0");
        let discovered = build_discovered_selector("eth0", None, None, None);
        assert!(
            user_sel.matches(&discovered),
            "name selector must match discovered selector with same name"
        );
    }

    /// Scenario: A name selector does not match a discovered selector with a
    /// different name (mismatched interface).
    #[test]
    fn test_build_discovered_selector_name_mismatch_does_not_match() {
        let user_sel = Selector::with_name("eth1");
        let discovered = build_discovered_selector("eth0", None, None, None);
        assert!(
            !user_sel.matches(&discovered),
            "name selector 'eth1' must not match discovered selector for 'eth0'"
        );
    }

    /// Scenario: A user selector with mac="aa:bb:cc:dd:ee:01" matches the
    /// discovered selector built for an interface with that MAC.
    #[test]
    fn test_build_discovered_selector_mac_match() {
        let mac = [0xAAu8, 0xBBu8, 0xCCu8, 0xDDu8, 0xEEu8, 0x01u8];
        let discovered = build_discovered_selector("eth0", Some(mac), None, None);
        let user_sel = Selector {
            mac: Some(MacAddr(mac)),
            ..Default::default()
        };
        assert!(
            user_sel.matches(&discovered),
            "MAC selector must match discovered selector with same MAC bytes"
        );
    }

    /// Scenario: A MAC selector does not match when MAC bytes differ.
    #[test]
    fn test_build_discovered_selector_mac_mismatch_does_not_match() {
        let mac0 = [0xAAu8, 0xBBu8, 0xCCu8, 0xDDu8, 0xEEu8, 0x01u8];
        let mac1 = [0xAAu8, 0xBBu8, 0xCCu8, 0xDDu8, 0xEEu8, 0x02u8];
        let discovered = build_discovered_selector("eth0", Some(mac0), None, None);
        let user_sel = Selector {
            mac: Some(MacAddr(mac1)),
            ..Default::default()
        };
        assert!(
            !user_sel.matches(&discovered),
            "MAC selector must not match discovered selector with different MAC"
        );
    }

    /// Scenario: Query by driver selector — driver="ixgbe" matches an interface
    /// with that driver.
    #[test]
    fn test_build_discovered_selector_driver_match() {
        let discovered = build_discovered_selector("eth0", None, Some("ixgbe"), None);
        let user_sel = Selector {
            driver: Some("ixgbe".to_string()),
            ..Default::default()
        };
        assert!(
            user_sel.matches(&discovered),
            "driver selector must match discovered selector with same driver"
        );
    }

    /// Scenario: A driver selector does not match an interface with a different
    /// driver (e.g., selecting "ixgbe" when the interface uses "e1000").
    #[test]
    fn test_build_discovered_selector_driver_mismatch_does_not_match() {
        let discovered = build_discovered_selector("eth0", None, Some("e1000"), None);
        let user_sel = Selector {
            driver: Some("ixgbe".to_string()),
            ..Default::default()
        };
        assert!(
            !user_sel.matches(&discovered),
            "driver selector 'ixgbe' must not match discovered selector with driver 'e1000'"
        );
    }

    /// Scenario: Query with multiple selector fields uses AND logic —
    /// name AND mac both match → result is a match.
    #[test]
    fn test_build_discovered_selector_and_logic_both_match() {
        let mac = [0xAAu8, 0xBBu8, 0xCCu8, 0xDDu8, 0xEEu8, 0x01u8];
        let discovered = build_discovered_selector("eth0", Some(mac), None, None);
        let user_sel = Selector {
            name: Some("eth0".to_string()),
            mac: Some(MacAddr(mac)),
            ..Default::default()
        };
        assert!(
            user_sel.matches(&discovered),
            "AND selector (name + mac both matching) must return true"
        );
    }

    /// Scenario: Query with multiple selector fields uses AND logic —
    /// name matches but mac does not → no match.
    #[test]
    fn test_build_discovered_selector_and_logic_mac_mismatch_fails() {
        let mac0 = [0xAAu8, 0xBBu8, 0xCCu8, 0xDDu8, 0xEEu8, 0x01u8];
        let mac1 = [0xAAu8, 0xBBu8, 0xCCu8, 0xDDu8, 0xEEu8, 0x02u8];
        let discovered = build_discovered_selector("eth0", Some(mac0), None, None);
        let user_sel = Selector {
            name: Some("eth0".to_string()),
            mac: Some(MacAddr(mac1)), // wrong MAC
            ..Default::default()
        };
        assert!(
            !user_sel.matches(&discovered),
            "AND selector must fail if any field mismatches (name matches but mac does not)"
        );
    }

    /// Scenario: Query with multiple selector fields uses AND logic —
    /// driver AND name both match.
    #[test]
    fn test_build_discovered_selector_and_logic_driver_and_name() {
        let discovered = build_discovered_selector("eth0", None, Some("ixgbe"), None);
        let user_sel = Selector {
            name: Some("eth0".to_string()),
            driver: Some("ixgbe".to_string()),
            ..Default::default()
        };
        assert!(
            user_sel.matches(&discovered),
            "AND selector with matching name and driver must return true"
        );
    }

    /// Scenario: PCI path selector matches the discovered pci_path.
    #[test]
    fn test_build_discovered_selector_pci_path_match() {
        let discovered = build_discovered_selector("eth0", None, None, Some("0000:03:00.0"));
        let user_sel = Selector {
            pci_path: Some("0000:03:00.0".to_string()),
            ..Default::default()
        };
        assert!(user_sel.matches(&discovered), "pci_path selector must match");
    }

    /// Scenario: PCI path selector does not match a different pci_path.
    #[test]
    fn test_build_discovered_selector_pci_path_mismatch_does_not_match() {
        let discovered = build_discovered_selector("eth0", None, None, Some("0000:04:00.0"));
        let user_sel = Selector {
            pci_path: Some("0000:03:00.0".to_string()),
            ..Default::default()
        };
        assert!(!user_sel.matches(&discovered), "pci_path selector must not match different pci_path");
    }

    // ── read_sysfs_speed / read_sysfs_driver ──────────────────────────────────

    /// read_sysfs_speed returns None for a non-existent interface name.
    #[test]
    fn test_read_sysfs_speed_nonexistent_interface_returns_none() {
        let result = read_sysfs_speed("interface_that_does_not_exist_xyzzy_99");
        assert!(result.is_none(), "non-existent interface should return None for speed");
    }

    /// read_sysfs_driver returns None for a non-existent interface name.
    #[test]
    fn test_read_sysfs_driver_nonexistent_interface_returns_none() {
        let result = read_sysfs_driver("interface_that_does_not_exist_xyzzy_99");
        assert!(result.is_none(), "non-existent interface should return None for driver");
    }

    /// read_sysfs_pci_path returns None for a non-existent interface name.
    #[test]
    fn test_read_sysfs_pci_path_nonexistent_interface_returns_none() {
        let result = read_sysfs_pci_path("interface_that_does_not_exist_xyzzy_99");
        assert!(result.is_none(), "non-existent interface should return None for pci_path");
    }
}
