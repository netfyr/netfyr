//! IPv6 address generation and netlink address management for the ipv6auto factory.

use std::net::{IpAddr, Ipv6Addr};
use std::time::Duration;

use futures::TryStreamExt;
use netlink_packet_route::address::{AddressAttribute, AddressFlags, AddressHeaderFlags, CacheInfo};
use netlink_packet_route::AddressFamily;
use rtnetlink::new_connection;

// ── EUI-64 address generation ─────────────────────────────────────────────────

/// Expand a 48-bit MAC address into a 64-bit Modified EUI-64 interface identifier.
///
/// Per RFC 4291 Appendix A: insert `0xff:0xfe` between bytes 3 and 4, then
/// flip the Universal/Local bit (bit 6 of the first octet, i.e. XOR with 0x02).
pub fn mac_to_eui64(mac: [u8; 6]) -> [u8; 8] {
    [
        mac[0] ^ 0x02, // flip U/L bit
        mac[1],
        mac[2],
        0xff,
        0xfe,
        mac[3],
        mac[4],
        mac[5],
    ]
}

/// Combine a /64 prefix with a MAC-derived EUI-64 interface identifier to form
/// a SLAAC address (RFC 4862).
///
/// Only valid for /64 prefixes. For any other prefix length, returns `prefix`
/// unmodified (SLAAC is not defined for non-/64 prefixes).
pub fn generate_slaac_address(prefix: Ipv6Addr, prefix_len: u8, mac: [u8; 6]) -> Ipv6Addr {
    if prefix_len != 64 {
        return prefix;
    }
    let eui64 = mac_to_eui64(mac);
    let prefix_bytes = prefix.octets();
    let mut addr_bytes = [0u8; 16];
    addr_bytes[..8].copy_from_slice(&prefix_bytes[..8]);
    addr_bytes[8..].copy_from_slice(&eui64);
    Ipv6Addr::from(addr_bytes)
}

// ── Netlink helpers ───────────────────────────────────────────────────────────

/// Read the interface's MAC address via rtnetlink (not sysfs — sysfs is not
/// network-namespace-aware in all environments).
pub async fn get_interface_mac(interface: &str) -> Result<[u8; 6], String> {
    use netlink_packet_route::link::LinkAttribute;

    let (conn, handle, _) = new_connection()
        .map_err(|e| format!("netlink connection failed: {e}"))?;
    tokio::spawn(conn);

    let mut links = handle
        .link()
        .get()
        .match_name(interface.to_string())
        .execute();

    let msg = links
        .try_next()
        .await
        .map_err(|e| format!("netlink query failed for {interface}: {e}"))?
        .ok_or_else(|| format!("interface not found: {interface}"))?;

    for attr in &msg.attributes {
        if let LinkAttribute::Address(bytes) = attr {
            if bytes.len() == 6 {
                let mut mac = [0u8; 6];
                mac.copy_from_slice(bytes);
                return Ok(mac);
            }
        }
    }

    Err(format!("no MAC address found for interface {interface}"))
}

/// Get the interface index for `interface` via rtnetlink.
pub async fn get_ifindex(interface: &str) -> Result<u32, String> {
    let (conn, handle, _) = new_connection()
        .map_err(|e| format!("netlink connection failed: {e}"))?;
    tokio::spawn(conn);

    let mut links = handle
        .link()
        .get()
        .match_name(interface.to_string())
        .execute();

    let msg = links
        .try_next()
        .await
        .map_err(|e| format!("netlink query failed for {interface}: {e}"))?
        .ok_or_else(|| format!("interface not found: {interface}"))?;

    Ok(msg.header.index)
}

// ── DAD monitoring ────────────────────────────────────────────────────────────

/// Wait for a link-local IPv6 address on `interface` to complete DAD (Duplicate
/// Address Detection), transitioning from tentative to permanent.
///
/// Polls via rtnetlink address dump every 100ms. DAD typically completes in ~1s.
/// Returns the link-local address on success.
/// Returns an error on DAD failure (`IFA_F_DADFAILED`) or timeout.
pub async fn wait_for_link_local_dad(
    interface: &str,
    timeout: Duration,
) -> Result<Ipv6Addr, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let poll_interval = Duration::from_millis(100);

    loop {
        match check_link_local_status(interface).await {
            Ok(Some(addr)) => return Ok(addr),
            Ok(None) => {} // Not yet available or still tentative; keep polling.
            Err(e) if e.contains("dadfailed") => {
                return Err(format!("link-local DAD failed on {interface}: {e}"))
            }
            Err(_) => {} // Transient netlink error; keep polling.
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(format!(
                "timeout waiting for link-local DAD to complete on {interface}"
            ));
        }
        let remaining = deadline - now;
        tokio::time::sleep(remaining.min(poll_interval)).await;
    }
}

/// Check whether a link-local IPv6 address on `interface` has completed DAD.
///
/// Uses the same `classify_dad_state` logic as `interface.rs`: checks the 32-bit
/// `IFA_FLAGS` attribute first, falls back to the 8-bit header flags field.
///
/// Returns:
/// - `Ok(Some(addr))` if a non-tentative link-local address exists.
/// - `Ok(None)` if no link-local address exists or it is still tentative.
/// - `Err(...)` if `IFA_F_DADFAILED` is set, or on a netlink error.
async fn check_link_local_status(interface: &str) -> Result<Option<Ipv6Addr>, String> {
    let (conn, handle, _) = new_connection()
        .map_err(|e| format!("netlink connection failed: {e}"))?;
    tokio::spawn(conn);

    let ifindex = get_ifindex(interface).await?;

    let mut stream = handle.address().get().execute();

    while let Some(msg) = stream
        .try_next()
        .await
        .map_err(|e| format!("address dump failed: {e}"))?
    {
        if msg.header.family != AddressFamily::Inet6 {
            continue;
        }
        if msg.header.index != ifindex {
            continue;
        }

        let mut addr_v6: Option<Ipv6Addr> = None;
        let mut has_ifa_flags_attr = false;
        let mut dad_state = "preferred";

        for attr in &msg.attributes {
            match attr {
                AddressAttribute::Address(IpAddr::V6(v6)) => {
                    addr_v6 = Some(*v6);
                }
                AddressAttribute::Flags(flags) => {
                    has_ifa_flags_attr = true;
                    if flags.contains(AddressFlags::Dadfailed) {
                        dad_state = "dadfailed";
                    } else if flags.contains(AddressFlags::Tentative) {
                        dad_state = "tentative";
                    } else if flags.contains(AddressFlags::Deprecated) {
                        dad_state = "deprecated";
                    }
                }
                _ => {}
            }
        }

        // Fall back to header flags when no IFA_FLAGS attribute is present (older kernels).
        if !has_ifa_flags_attr {
            let hf = &msg.header.flags;
            if hf.contains(AddressHeaderFlags::Dadfailed) {
                dad_state = "dadfailed";
            } else if hf.contains(AddressHeaderFlags::Tentative) {
                dad_state = "tentative";
            }
        }

        let Some(v6) = addr_v6 else { continue };

        // Only interested in link-local addresses (fe80::/10).
        if (v6.segments()[0] & 0xffc0) != 0xfe80 {
            continue;
        }

        match dad_state {
            "dadfailed" => return Err(format!("dadfailed for {v6} on {interface}")),
            "tentative" => {
                // Address is still tentative; continue to next message.
            }
            _ => return Ok(Some(v6)),
        }
    }

    Ok(None)
}

/// Result of a DAD (Duplicate Address Detection) status check for a SLAAC address.
pub enum DadStatus {
    /// DAD completed successfully; address is permanent and usable.
    Complete,
    /// DAD is still in progress; address is tentative.
    Tentative,
    /// DAD failed; address has a conflict and was removed by the kernel.
    Failed,
}

/// Check the DAD status of a specific IPv6 SLAAC address on `interface`.
///
/// Performs an rtnetlink address dump and inspects `IFA_FLAGS`/header flags for
/// the matching address. Returns `Tentative` if the address is not found yet
/// (caller should retry), `Complete` if DAD passed, `Failed` if DAD failed.
pub async fn check_slaac_address_dad(
    interface: &str,
    addr: Ipv6Addr,
    prefix_len: u8,
) -> DadStatus {
    let (conn, handle, _) = match new_connection() {
        Ok(c) => c,
        Err(_) => return DadStatus::Tentative,
    };
    tokio::spawn(conn);

    let ifindex = match get_ifindex(interface).await {
        Ok(i) => i,
        Err(_) => return DadStatus::Tentative,
    };

    let mut stream = handle.address().get().execute();

    while let Ok(Some(msg)) = stream.try_next().await {
        if msg.header.family != AddressFamily::Inet6 {
            continue;
        }
        if msg.header.index != ifindex {
            continue;
        }
        if msg.header.prefix_len != prefix_len {
            continue;
        }
        let matches = msg.attributes.iter().any(|a| {
            matches!(a, AddressAttribute::Address(IpAddr::V6(v)) if *v == addr)
        });
        if !matches {
            continue;
        }

        let mut dad_failed = false;
        let mut tentative = false;
        let mut has_ifa_flags = false;

        for attr in &msg.attributes {
            if let AddressAttribute::Flags(flags) = attr {
                has_ifa_flags = true;
                if flags.contains(AddressFlags::Dadfailed) {
                    dad_failed = true;
                } else if flags.contains(AddressFlags::Tentative) {
                    tentative = true;
                }
            }
        }

        if !has_ifa_flags {
            if msg.header.flags.contains(AddressHeaderFlags::Dadfailed) {
                dad_failed = true;
            } else if msg.header.flags.contains(AddressHeaderFlags::Tentative) {
                tentative = true;
            }
        }

        return if dad_failed {
            DadStatus::Failed
        } else if tentative {
            DadStatus::Tentative
        } else {
            DadStatus::Complete
        };
    }

    // Address not found yet; return Tentative so the caller retries.
    DadStatus::Tentative
}

// ── Address management ────────────────────────────────────────────────────────

/// Add an IPv6 address to `interface` via rtnetlink with specified lifetimes.
///
/// Sets `IFA_CACHEINFO` to configure `valid_lft` and `preferred_lft`. The kernel
/// automatically runs DAD on the newly added address.
pub async fn add_ipv6_address(
    interface: &str,
    addr: Ipv6Addr,
    prefix_len: u8,
    valid_lft: u32,
    preferred_lft: u32,
) -> Result<(), String> {
    let (conn, handle, _) = new_connection()
        .map_err(|e| format!("netlink connection failed: {e}"))?;
    tokio::spawn(conn);

    let ifindex = get_ifindex(interface).await?;

    let mut req = handle
        .address()
        .add(ifindex, IpAddr::V6(addr), prefix_len);

    let mut ci = CacheInfo::default();
    ci.ifa_valid = valid_lft;
    ci.ifa_preferred = preferred_lft;
    req.message_mut().attributes.push(AddressAttribute::CacheInfo(ci));

    req.execute()
        .await
        .map_err(|e| format!("failed to add {addr}/{prefix_len} to {interface}: {e}"))
}

/// Remove an IPv6 address from `interface` via rtnetlink.
///
/// Looks up the full `AddressMessage` for the address before issuing the delete
/// request (the rtnetlink delete API requires the full message, not just ip+prefix).
pub async fn remove_ipv6_address(
    interface: &str,
    addr: Ipv6Addr,
    prefix_len: u8,
) -> Result<(), String> {
    let (conn, handle, _) = new_connection()
        .map_err(|e| format!("netlink connection failed: {e}"))?;
    tokio::spawn(conn);

    let ifindex = get_ifindex(interface).await?;

    // Dump all addresses and find the matching one.
    let mut stream = handle.address().get().execute();
    let mut found_msg = None;

    while let Some(msg) = stream
        .try_next()
        .await
        .map_err(|e| format!("address dump failed: {e}"))?
    {
        if msg.header.family != AddressFamily::Inet6 {
            continue;
        }
        if msg.header.index != ifindex {
            continue;
        }
        if msg.header.prefix_len != prefix_len {
            continue;
        }
        let matches = msg.attributes.iter().any(|a| {
            matches!(a, AddressAttribute::Address(IpAddr::V6(v)) if *v == addr)
        });
        if matches {
            found_msg = Some(msg);
            break;
        }
    }

    let msg = found_msg.ok_or_else(|| {
        format!("address {addr}/{prefix_len} not found on {interface}")
    })?;

    handle
        .address()
        .del(msg)
        .execute()
        .await
        .map_err(|e| format!("failed to remove {addr}/{prefix_len} from {interface}: {e}"))
}

/// Update the lifetimes of an existing IPv6 address by replacing it in-place.
#[cfg(test)]
mod tests {
    use super::*;

    // ── mac_to_eui64 ──────────────────────────────────────────────────────────

    /// Scenario: EUI-64 address generation from MAC (spec example)
    /// Given MAC aa:bb:cc:dd:ee:ff
    /// Then EUI-64 interface id is a8:bb:cc:ff:fe:dd:ee:ff
    /// (bit 7 of first byte flipped: aa -> a8, ff:fe inserted in middle)
    #[test]
    fn test_mac_to_eui64_spec_example() {
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let eui64 = mac_to_eui64(mac);
        assert_eq!(
            eui64,
            [0xa8, 0xbb, 0xcc, 0xff, 0xfe, 0xdd, 0xee, 0xff],
            "EUI-64 must flip U/L bit and insert ff:fe"
        );
    }

    /// Scenario: U/L bit flip — MAC with bit already clear stays clear (already locally administered)
    /// MAC with bit 7 already set (globally unique): aa -> a8 (flip sets bit)
    /// MAC with bit 7 already clear (locally administered): 02 -> 00 (flip clears bit)
    #[test]
    fn test_mac_to_eui64_ul_bit_flip_locally_administered_mac() {
        let mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01]; // locally administered
        let eui64 = mac_to_eui64(mac);
        // 0x02 ^ 0x02 = 0x00
        assert_eq!(eui64[0], 0x00, "U/L bit must be flipped: 0x02 -> 0x00");
    }

    /// Scenario: ff:fe is inserted between bytes 3 and 4
    #[test]
    fn test_mac_to_eui64_inserts_fffe() {
        let mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let eui64 = mac_to_eui64(mac);
        assert_eq!(eui64[3], 0xff, "byte[3] must be 0xff");
        assert_eq!(eui64[4], 0xfe, "byte[4] must be 0xfe");
        assert_eq!(eui64[5], 0x44, "byte[5] must be mac[3]");
        assert_eq!(eui64[6], 0x55, "byte[6] must be mac[4]");
        assert_eq!(eui64[7], 0x66, "byte[7] must be mac[5]");
    }

    /// Scenario: Original MAC bytes (except first) are preserved exactly
    #[test]
    fn test_mac_to_eui64_preserves_original_bytes() {
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let eui64 = mac_to_eui64(mac);
        // mac[1..3] = bytes [1..3] of eui64
        assert_eq!(eui64[1], 0xbb);
        assert_eq!(eui64[2], 0xcc);
        // mac[3..6] = bytes [5..8] of eui64 (after ff:fe insertion)
        assert_eq!(eui64[5], 0xdd);
        assert_eq!(eui64[6], 0xee);
        assert_eq!(eui64[7], 0xff);
    }

    // ── generate_slaac_address ────────────────────────────────────────────────

    /// Scenario: EUI-64 SLAAC address from spec example
    /// Given MAC aa:bb:cc:dd:ee:ff and prefix 2001:db8::/64
    /// Then address is 2001:db8::a8bb:ccff:fedd:eeff/64
    #[test]
    fn test_generate_slaac_address_spec_example() {
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let prefix: Ipv6Addr = "2001:db8::".parse().unwrap();
        let addr = generate_slaac_address(prefix, 64, mac);
        let expected: Ipv6Addr = "2001:db8::a8bb:ccff:fedd:eeff".parse().unwrap();
        assert_eq!(addr, expected, "SLAAC address must combine prefix + EUI-64");
    }

    /// Scenario: Non-/64 prefix returns prefix unmodified (SLAAC undefined for non-/64)
    #[test]
    fn test_generate_slaac_address_non64_prefix_returns_prefix_unchanged() {
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let prefix: Ipv6Addr = "2001:db8::".parse().unwrap();
        let addr = generate_slaac_address(prefix, 48, mac);
        assert_eq!(addr, prefix, "non-/64 prefix must be returned unmodified");
    }

    /// Scenario: /128 prefix returns prefix unmodified
    #[test]
    fn test_generate_slaac_address_128_prefix_returns_prefix_unchanged() {
        let mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let prefix: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let addr = generate_slaac_address(prefix, 128, mac);
        assert_eq!(addr, prefix, "/128 prefix must be returned unmodified");
    }

    /// Scenario: Upper 64 bits come from prefix, lower 64 bits come from EUI-64
    #[test]
    fn test_generate_slaac_address_upper_bits_from_prefix_lower_from_eui64() {
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let prefix: Ipv6Addr = "fd00:1234:5678:9abc::".parse().unwrap();
        let addr = generate_slaac_address(prefix, 64, mac);
        let octets = addr.octets();
        // Upper 8 bytes must match prefix
        let prefix_octets = prefix.octets();
        assert_eq!(&octets[..8], &prefix_octets[..8], "upper 64 bits must come from prefix");
        // Lower 8 bytes must be EUI-64 of mac
        let eui64 = mac_to_eui64(mac);
        assert_eq!(&octets[8..], &eui64, "lower 64 bits must be EUI-64 of MAC");
    }
}

pub async fn update_address_lifetime(
    interface: &str,
    addr: Ipv6Addr,
    prefix_len: u8,
    valid_lft: u32,
    preferred_lft: u32,
) -> Result<(), String> {
    let (conn, handle, _) = new_connection()
        .map_err(|e| format!("netlink connection failed: {e}"))?;
    tokio::spawn(conn);

    let ifindex = get_ifindex(interface).await?;

    let mut req = handle
        .address()
        .add(ifindex, IpAddr::V6(addr), prefix_len)
        .replace();

    let mut ci = CacheInfo::default();
    ci.ifa_valid = valid_lft;
    ci.ifa_preferred = preferred_lft;
    req.message_mut().attributes.push(AddressAttribute::CacheInfo(ci));

    req.execute()
        .await
        .map_err(|e| format!("failed to update lifetime of {addr}/{prefix_len} on {interface}: {e}"))
}
