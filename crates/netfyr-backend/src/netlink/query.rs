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
                    entity_type: "interface".to_string(),
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
    entity_type: &str,
    name: &str,
    mac: Option<[u8; 6]>,
    driver: Option<&str>,
    pci_path: Option<&str>,
) -> Selector {
    Selector {
        name: Some(name.to_owned()),
        type_: Some(entity_type.to_owned()),
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

/// Check if an interface is WiFi by detecting the `/sys/class/net/<name>/phy80211` symlink.
///
/// This symlink is created by the kernel's cfg80211 subsystem for all 802.11 interfaces.
/// Not namespace-aware (sysfs is global), same caveat as speed/driver lookups.
pub fn read_sysfs_is_wifi(name: &str) -> bool {
    std::fs::symlink_metadata(format!("/sys/class/net/{name}/phy80211")).is_ok()
}

/// Read the WiFi operating mode. Returns `None` — full implementation requires nl80211.
pub fn read_sysfs_wifi_mode(_name: &str) -> Option<String> {
    None
}

/// Read the WiFi SSID. Returns `None` — SSID requires nl80211.
pub fn read_sysfs_wifi_ssid(_name: &str) -> Option<String> {
    None
}

/// Read the WiFi frequency in MHz. Returns `None` — frequency requires nl80211.
pub fn read_sysfs_wifi_frequency(_name: &str) -> Option<u64> {
    None
}

/// Read the IPv6 address generation mode from procfs.
///
/// `/proc/sys/net/ipv6/conf/<name>/addr_gen_mode`: 0 → "eui64", 1 → "none".
/// Returns `None` on read/parse failure or unknown value.
pub fn read_procfs_addr_gen_mode(name: &str) -> Option<String> {
    let path = format!("/proc/sys/net/ipv6/conf/{name}/addr_gen_mode");
    let content = std::fs::read_to_string(&path).ok()?;
    match content.trim() {
        "0" => Some("eui64".to_owned()),
        "1" => Some("none".to_owned()),
        _ => None,
    }
}

/// Read the number of DAD NS probes from procfs.
///
/// `/proc/sys/net/ipv6/conf/<name>/dad_transmits`. Returns `None` on failure.
pub fn read_procfs_dad_transmits(name: &str) -> Option<u64> {
    let path = format!("/proc/sys/net/ipv6/conf/{name}/dad_transmits");
    let content = std::fs::read_to_string(&path).ok()?;
    content.trim().parse().ok()
}

/// Read DNS servers from `/etc/resolv.conf`, split by address family.
///
/// Returns `(ipv4_servers, ipv6_servers)`. Returns empty vectors if the file
/// doesn't exist, can't be read, or contains no valid nameserver lines.
pub fn read_dns_servers() -> (Vec<std::net::IpAddr>, Vec<std::net::IpAddr>) {
    parse_resolv_conf(&std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default())
}

fn parse_resolv_conf(content: &str) -> (Vec<std::net::IpAddr>, Vec<std::net::IpAddr>) {
    let mut ipv4 = Vec::new();
    let mut ipv6 = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("nameserver") {
            if let Ok(ip) = rest.trim().parse::<std::net::IpAddr>() {
                match ip {
                    std::net::IpAddr::V4(_) => ipv4.push(ip),
                    std::net::IpAddr::V6(_) => ipv6.push(ip),
                }
            }
        }
    }
    (ipv4, ipv6)
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
        let sel = build_discovered_selector("ethernet", "eth0", None, None, None);
        assert_eq!(sel.name.as_deref(), Some("eth0"));
        assert_eq!(sel.type_.as_deref(), Some("ethernet"));
        assert!(sel.mac.is_none(), "mac must be None when not provided");
        assert!(sel.driver.is_none(), "driver must be None when not provided");
        assert!(sel.pci_path.is_none(), "pci_path must be None when not provided");
    }

    /// Scenario: MAC is stored as MacAddr bytes when provided.
    #[test]
    fn test_build_discovered_selector_sets_mac() {
        let mac = [0xAAu8, 0xBBu8, 0xCCu8, 0xDDu8, 0xEEu8, 0x01u8];
        let sel = build_discovered_selector("ethernet", "eth0", Some(mac), None, None);
        assert_eq!(
            sel.mac.as_ref().map(|m| m.0),
            Some(mac),
            "MAC bytes must match what was provided"
        );
    }

    /// Scenario: driver is stored when provided.
    #[test]
    fn test_build_discovered_selector_sets_driver() {
        let sel = build_discovered_selector("ethernet", "eth0", None, Some("ixgbe"), None);
        assert_eq!(sel.driver.as_deref(), Some("ixgbe"));
    }

    /// Scenario: pci_path is stored when provided.
    #[test]
    fn test_build_discovered_selector_sets_pci_path() {
        let sel = build_discovered_selector("ethernet", "eth0", None, None, Some("0000:03:00.0"));
        assert_eq!(sel.pci_path.as_deref(), Some("0000:03:00.0"));
    }

    /// Scenario: All four fields are set correctly when all are provided.
    #[test]
    fn test_build_discovered_selector_with_all_fields() {
        let mac = [0xAAu8, 0xBBu8, 0xCCu8, 0xDDu8, 0xEEu8, 0xFFu8];
        let sel = build_discovered_selector(
            "ethernet",
            "eth0",
            Some(mac),
            Some("ixgbe"),
            Some("0000:03:00.0"),
        );
        assert_eq!(sel.name.as_deref(), Some("eth0"));
        assert_eq!(sel.type_.as_deref(), Some("ethernet"));
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
        let discovered = build_discovered_selector("ethernet", "eth0", None, None, None);
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
        let discovered = build_discovered_selector("ethernet", "eth0", None, None, None);
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
        let discovered = build_discovered_selector("ethernet", "eth0", Some(mac), None, None);
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
        let discovered = build_discovered_selector("ethernet", "eth0", Some(mac0), None, None);
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
        let discovered = build_discovered_selector("ethernet", "eth0", None, Some("ixgbe"), None);
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
        let discovered = build_discovered_selector("ethernet", "eth0", None, Some("e1000"), None);
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
        let discovered = build_discovered_selector("ethernet", "eth0", Some(mac), None, None);
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
        let discovered = build_discovered_selector("ethernet", "eth0", Some(mac0), None, None);
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
        let discovered = build_discovered_selector("ethernet", "eth0", None, Some("ixgbe"), None);
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
        let discovered = build_discovered_selector("ethernet", "eth0", None, None, Some("0000:03:00.0"));
        let user_sel = Selector {
            pci_path: Some("0000:03:00.0".to_string()),
            ..Default::default()
        };
        assert!(user_sel.matches(&discovered), "pci_path selector must match");
    }

    /// Scenario: A user selector with type_="ethernet" matches a discovered
    /// selector built for the "ethernet" entity type.
    #[test]
    fn test_build_discovered_selector_entity_type_match() {
        let discovered = build_discovered_selector("ethernet", "eth0", None, None, None);
        let user_sel = Selector {
            type_: Some("ethernet".to_string()),
            ..Default::default()
        };
        assert!(
            user_sel.matches(&discovered),
            "type_ selector must match discovered selector with same entity type"
        );
    }

    /// Scenario: A user selector with type_="wifi" does not match a
    /// discovered selector built for "ethernet".
    #[test]
    fn test_build_discovered_selector_entity_type_mismatch_does_not_match() {
        let discovered = build_discovered_selector("ethernet", "eth0", None, None, None);
        let user_sel = Selector {
            type_: Some("wifi".to_string()),
            ..Default::default()
        };
        assert!(
            !user_sel.matches(&discovered),
            "type_ selector 'wifi' must not match discovered selector for 'ethernet'"
        );
    }

    /// Scenario: PCI path selector does not match a different pci_path.
    #[test]
    fn test_build_discovered_selector_pci_path_mismatch_does_not_match() {
        let discovered = build_discovered_selector("ethernet", "eth0", None, None, Some("0000:04:00.0"));
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

    // ── read_sysfs_is_wifi ────────────────────────────────────────────────────────

    /// Scenario: Technology detection — a nonexistent interface has no
    /// /sys/class/net/<name>/phy80211 symlink so read_sysfs_is_wifi must return false.
    ///
    /// This is the foundational predicate for WiFi detection: when the phy80211
    /// symlink is absent, the interface is not WiFi (and will be classified as
    /// ethernet by detect_technology).
    #[test]
    fn test_read_sysfs_is_wifi_returns_false_for_nonexistent_interface() {
        let result = read_sysfs_is_wifi("interface_that_does_not_exist_xyzzy_99");
        assert!(
            !result,
            "read_sysfs_is_wifi must return false when phy80211 symlink is absent"
        );
    }

    /// read_sysfs_is_wifi returns false for multiple different nonexistent names.
    #[test]
    fn test_read_sysfs_is_wifi_returns_false_for_various_fake_names() {
        for name in &["eth0_fake_99", "enp0s3_fake", "wlan0_fake_no_phy"] {
            assert!(
                !read_sysfs_is_wifi(name),
                "read_sysfs_is_wifi({name}) must return false when phy80211 path absent"
            );
        }
    }

    // ── WiFi sysfs stubs ──────────────────────────────────────────────────────────

    /// Scenario: WiFi interface sub-object with mode/ssid/frequency.
    /// read_sysfs_wifi_mode is a stub that always returns None (full nl80211
    /// implementation is future work). Returning None means the "mode" field is
    /// omitted from the wifi sub-object and the sub-object itself may be absent.
    #[test]
    fn test_read_sysfs_wifi_mode_stub_always_returns_none() {
        // Stub ignores name; verify with several names including a real-looking one.
        for name in &["wlan0", "wlp3s0", "interface_that_does_not_exist_xyzzy_99"] {
            let result = read_sysfs_wifi_mode(name);
            assert!(
                result.is_none(),
                "read_sysfs_wifi_mode({name}) stub must return None until nl80211 is implemented"
            );
        }
    }

    /// read_sysfs_wifi_ssid is a stub that always returns None.
    /// A None SSID means the WiFi interface has no associated network or the
    /// nl80211 query has not been implemented yet.
    #[test]
    fn test_read_sysfs_wifi_ssid_stub_always_returns_none() {
        for name in &["wlan0", "wlp3s0", "interface_that_does_not_exist_xyzzy_99"] {
            let result = read_sysfs_wifi_ssid(name);
            assert!(
                result.is_none(),
                "read_sysfs_wifi_ssid({name}) stub must return None until nl80211 is implemented"
            );
        }
    }

    /// read_sysfs_wifi_frequency is a stub that always returns None.
    /// A None frequency means the WiFi interface is not associated or the
    /// nl80211 query has not been implemented yet.
    #[test]
    fn test_read_sysfs_wifi_frequency_stub_always_returns_none() {
        for name in &["wlan0", "wlp3s0", "interface_that_does_not_exist_xyzzy_99"] {
            let result = read_sysfs_wifi_frequency(name);
            assert!(
                result.is_none(),
                "read_sysfs_wifi_frequency({name}) stub must return None until nl80211 is implemented"
            );
        }
    }

    // ── parse_resolv_conf ─────────────────────────────────────────────────────

    #[test]
    fn test_parse_resolv_conf_empty_returns_empty() {
        let (v4, v6) = parse_resolv_conf("");
        assert!(v4.is_empty());
        assert!(v6.is_empty());
    }

    #[test]
    fn test_parse_resolv_conf_ipv4_nameservers() {
        let content = "nameserver 8.8.8.8\nnameserver 1.1.1.1\n";
        let (v4, v6) = parse_resolv_conf(content);
        assert_eq!(v4.len(), 2);
        assert!(v6.is_empty());
        assert_eq!(v4[0].to_string(), "8.8.8.8");
        assert_eq!(v4[1].to_string(), "1.1.1.1");
    }

    #[test]
    fn test_parse_resolv_conf_ipv6_nameservers() {
        let content = "nameserver 2001:4860:4860::8888\n";
        let (v4, v6) = parse_resolv_conf(content);
        assert!(v4.is_empty());
        assert_eq!(v6.len(), 1);
        assert_eq!(v6[0].to_string(), "2001:4860:4860::8888");
    }

    #[test]
    fn test_parse_resolv_conf_mixed_families() {
        let content = "nameserver 8.8.8.8\nnameserver 2001:db8::1\nnameserver 1.1.1.1\n";
        let (v4, v6) = parse_resolv_conf(content);
        assert_eq!(v4.len(), 2);
        assert_eq!(v6.len(), 1);
    }

    #[test]
    fn test_parse_resolv_conf_ignores_non_nameserver_lines() {
        let content = "# comment\ndomain example.com\nnameserver 8.8.8.8\nsearch example.com\n";
        let (v4, v6) = parse_resolv_conf(content);
        assert_eq!(v4.len(), 1);
        assert!(v6.is_empty());
    }

    #[test]
    fn test_parse_resolv_conf_ignores_invalid_ip() {
        let content = "nameserver not-an-ip\nnameserver 8.8.8.8\n";
        let (v4, v6) = parse_resolv_conf(content);
        assert_eq!(v4.len(), 1, "invalid IP must be ignored");
        assert!(v6.is_empty());
    }

    // ── read_procfs_addr_gen_mode ─────────────────────────────────────────────────

    /// Scenario: Query reports IPv6 link_local method.
    /// read_procfs_addr_gen_mode returns None for a nonexistent interface because
    /// /proc/sys/net/ipv6/conf/<name>/addr_gen_mode does not exist.
    #[test]
    fn test_read_procfs_addr_gen_mode_nonexistent_interface_returns_none() {
        let result = read_procfs_addr_gen_mode("interface_that_does_not_exist_xyzzy_99");
        assert!(
            result.is_none(),
            "non-existent interface must return None for addr_gen_mode"
        );
    }

    /// read_procfs_addr_gen_mode returns None for multiple nonexistent interface names.
    #[test]
    fn test_read_procfs_addr_gen_mode_various_fake_names_return_none() {
        for name in &["eth0_fake_procfs_99", "enp0s3_fake_procfs", "wlan0_fake_procfs"] {
            let result = read_procfs_addr_gen_mode(name);
            assert!(
                result.is_none(),
                "read_procfs_addr_gen_mode({name}) must return None when procfs file absent"
            );
        }
    }

    // ── read_procfs_dad_transmits ─────────────────────────────────────────────────

    /// Scenario: Query reports IPv6 dad_transmits.
    /// read_procfs_dad_transmits returns None for a nonexistent interface because
    /// /proc/sys/net/ipv6/conf/<name>/dad_transmits does not exist.
    #[test]
    fn test_read_procfs_dad_transmits_nonexistent_interface_returns_none() {
        let result = read_procfs_dad_transmits("interface_that_does_not_exist_xyzzy_99");
        assert!(
            result.is_none(),
            "non-existent interface must return None for dad_transmits"
        );
    }

    /// read_procfs_dad_transmits returns None for multiple nonexistent interface names.
    #[test]
    fn test_read_procfs_dad_transmits_various_fake_names_return_none() {
        for name in &["eth0_fake_procfs_99", "enp0s3_fake_procfs", "wlan0_fake_procfs"] {
            let result = read_procfs_dad_transmits(name);
            assert!(
                result.is_none(),
                "read_procfs_dad_transmits({name}) must return None when procfs file absent"
            );
        }
    }
}
