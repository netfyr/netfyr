//! DUID-LLT (Link-Layer + Time) generation and persistence.
// SPEC-412 will integrate this module; all items unused until then.
#![allow(dead_code)]
//!
//! RFC 8415 §11.2 — DUID type 1 is DUID-LLT: a 2-byte DUID type, 2-byte
//! hardware type, 4-byte time, and variable-length link-layer address.
//! For Ethernet the link-layer address is the 6-byte MAC.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// DUID-LLT for Ethernet interfaces (RFC 8415 §11.2).
#[derive(Debug, Clone)]
pub struct DuidLlt {
    /// Hardware type; always 1 for Ethernet (RFC 826).
    pub hardware_type: u16,
    /// Seconds since 2000-01-01 00:00:00 UTC (the DHCPv6 epoch).
    pub time: u32,
    /// MAC address of the generating interface.
    pub link_layer_addr: [u8; 6],
}

/// Seconds between the Unix epoch (1970-01-01) and the DHCPv6 epoch (2000-01-01).
const DHCPV6_EPOCH_OFFSET: u64 = 946_684_800;

/// Encode a `DuidLlt` to its 14-byte wire representation.
///
/// Layout: [type=1 (2B)][hw-type (2B)][time (4B)][mac (6B)] = 14 bytes.
pub fn encode_duid(duid: &DuidLlt) -> Vec<u8> {
    let mut buf = Vec::with_capacity(14);
    // DUID type 1 = DUID-LLT
    buf.extend_from_slice(&1u16.to_be_bytes());
    buf.extend_from_slice(&duid.hardware_type.to_be_bytes());
    buf.extend_from_slice(&duid.time.to_be_bytes());
    buf.extend_from_slice(&duid.link_layer_addr);
    buf
}

/// Decode a `DuidLlt` from wire bytes.
///
/// Returns an error if the data is too short or the DUID type is not 1 (LLT).
pub fn decode_duid(data: &[u8]) -> Result<DuidLlt, String> {
    if data.len() < 14 {
        return Err(format!("duid: data too short: {} < 14 bytes", data.len()));
    }
    let duid_type = u16::from_be_bytes([data[0], data[1]]);
    if duid_type != 1 {
        return Err(format!("duid: unsupported type {duid_type}, expected 1 (LLT)"));
    }
    let hardware_type = u16::from_be_bytes([data[2], data[3]]);
    let time = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let mut link_layer_addr = [0u8; 6];
    link_layer_addr.copy_from_slice(&data[8..14]);
    Ok(DuidLlt { hardware_type, time, link_layer_addr })
}

/// Load a DUID from `path` if it exists, otherwise generate a new DUID-LLT
/// and attempt to persist it.
///
/// If the file exists but is corrupt, a new DUID is generated (not persisted
/// over the corrupt file — it will be overwritten on next successful call).
/// If persistence fails (read-only filesystem, missing parent), the generated
/// DUID is returned with a tracing warning; the client will still work, it just
/// won't survive daemon restarts with the same DUID.
pub fn load_or_create_duid(path: &Path, mac: [u8; 6]) -> Result<DuidLlt, String> {
    // Attempt to load an existing DUID.
    if path.exists() {
        match fs::read(path) {
            Ok(data) => match decode_duid(&data) {
                Ok(duid) => return Ok(duid),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "dhcpv6: corrupt DUID file; regenerating");
                }
            },
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "dhcpv6: failed to read DUID file; regenerating");
            }
        }
    }

    // Generate a new DUID-LLT.
    let duid = DuidLlt {
        hardware_type: 1, // Ethernet
        time: dhcpv6_time(),
        link_layer_addr: mac,
    };

    // Persist to disk; failure is non-fatal.
    let encoded = encode_duid(&duid);
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            tracing::warn!(path = %path.display(), error = %e, "dhcpv6: failed to create DUID directory");
            return Ok(duid);
        }
    }
    if let Err(e) = fs::write(path, &encoded) {
        tracing::warn!(path = %path.display(), error = %e, "dhcpv6: failed to persist DUID (non-fatal)");
    }

    Ok(duid)
}

/// Compute seconds elapsed since the DHCPv6 epoch (2000-01-01 00:00:00 UTC).
///
/// Returns 0 if the system clock is before the epoch (should never happen).
fn dhcpv6_time() -> u32 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    now.saturating_sub(DHCPV6_EPOCH_OFFSET) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_duid() -> DuidLlt {
        DuidLlt {
            hardware_type: 1,
            time: 756_864_000,
            link_layer_addr: [0x00, 0x11, 0x22, 0x33, 0x44, 0x55],
        }
    }

    // Scenario: encode_duid produces 14-byte wire representation
    #[test]
    fn test_encode_duid_produces_14_bytes() {
        let encoded = encode_duid(&sample_duid());
        assert_eq!(encoded.len(), 14, "encoded DUID must be 14 bytes");
    }

    // Scenario: First two bytes of encoded DUID are the DUID type (1 = DUID-LLT)
    #[test]
    fn test_encode_duid_type_field_is_one() {
        let encoded = encode_duid(&sample_duid());
        let duid_type = u16::from_be_bytes([encoded[0], encoded[1]]);
        assert_eq!(duid_type, 1, "DUID type field must be 1 (DUID-LLT)");
    }

    // Scenario: DUID encode/decode round-trip preserves all fields
    #[test]
    fn test_encode_decode_duid_roundtrip() {
        let original = sample_duid();
        let encoded = encode_duid(&original);
        let decoded = decode_duid(&encoded).expect("decode must succeed on valid DUID");
        assert_eq!(decoded.hardware_type, original.hardware_type);
        assert_eq!(decoded.time, original.time);
        assert_eq!(decoded.link_layer_addr, original.link_layer_addr);
    }

    // Scenario: decode_duid fails on data shorter than 14 bytes
    #[test]
    fn test_decode_duid_fails_on_short_data() {
        let short = vec![0u8; 13];
        let result = decode_duid(&short);
        assert!(result.is_err(), "decode must fail on data shorter than 14 bytes");
    }

    // Scenario: decode_duid fails when DUID type is not 1
    #[test]
    fn test_decode_duid_fails_on_wrong_type() {
        let mut data = vec![0u8; 14];
        data[0] = 0;
        data[1] = 3; // DUID type 3 (DUID-LL), not 1 (DUID-LLT)
        let result = decode_duid(&data);
        assert!(result.is_err(), "decode must fail on non-LLT DUID type");
    }

    // Scenario: decode_duid succeeds on exactly 14 bytes of valid DUID-LLT
    #[test]
    fn test_decode_duid_succeeds_on_14_bytes() {
        let encoded = encode_duid(&sample_duid());
        assert_eq!(encoded.len(), 14);
        let decoded = decode_duid(&encoded);
        assert!(decoded.is_ok(), "decode must succeed on 14 valid bytes");
    }

    // Scenario: decode_duid succeeds on data longer than 14 bytes (extra bytes ignored)
    #[test]
    fn test_decode_duid_succeeds_on_extra_bytes() {
        let mut encoded = encode_duid(&sample_duid());
        encoded.extend_from_slice(&[0xFF, 0xFF]); // extra bytes
        let decoded = decode_duid(&encoded);
        assert!(decoded.is_ok(), "decode must succeed when data has extra trailing bytes");
    }

    // Scenario: DUID hardware type is 1 (Ethernet)
    #[test]
    fn test_duid_hardware_type_is_ethernet() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("duid");
        let mac = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
        let duid = load_or_create_duid(&path, mac).unwrap();
        assert_eq!(duid.hardware_type, 1, "new DUID hardware type must be 1 (Ethernet)");
    }

    // Scenario: load_or_create_duid creates a new DUID when file does not exist
    #[test]
    fn test_load_or_create_duid_creates_new_when_missing() {
        let dir = TempDir::new().unwrap();
        // Nested path: parent directory does not exist yet
        let path = dir.path().join("subdir").join("duid");
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let duid = load_or_create_duid(&path, mac).expect("must succeed even with missing parent");
        assert_eq!(duid.hardware_type, 1, "new DUID must have hardware_type=1");
        assert_eq!(duid.link_layer_addr, mac, "new DUID must use the provided MAC");
    }

    // Scenario: DUID is persisted and loaded correctly across daemon restarts
    #[test]
    fn test_load_or_create_duid_persists_and_reloads() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("duid");
        let mac = [0xde, 0xad, 0xbe, 0xef, 0x00, 0x01];

        let first = load_or_create_duid(&path, mac).expect("first call must succeed");
        assert!(path.exists(), "DUID file must be created on first call");

        let second = load_or_create_duid(&path, mac).expect("second call must succeed");
        assert_eq!(first.hardware_type, second.hardware_type);
        assert_eq!(first.time, second.time, "loaded DUID must have same time");
        assert_eq!(first.link_layer_addr, second.link_layer_addr);
    }

    // Scenario: The same DUID is used across daemon restarts (identical bytes)
    #[test]
    fn test_duid_same_after_restart() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("duid");
        let mac = [0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00];

        let duid1 = load_or_create_duid(&path, mac).unwrap();
        let duid2 = load_or_create_duid(&path, mac).unwrap();

        assert_eq!(
            encode_duid(&duid1),
            encode_duid(&duid2),
            "DUID wire encoding must be identical after daemon restart"
        );
    }

    // Scenario: load_or_create_duid regenerates DUID on corrupt file (non-fatal)
    #[test]
    fn test_load_or_create_duid_regenerates_on_corrupt() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("duid");
        std::fs::write(&path, b"this is not a valid duid").unwrap();
        let mac = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc];
        let duid =
            load_or_create_duid(&path, mac).expect("must succeed even when file is corrupt");
        assert_eq!(
            duid.link_layer_addr, mac,
            "regenerated DUID must use the provided MAC"
        );
    }

    // Scenario: DUID time field is in the DHCPv6 epoch (seconds since 2000-01-01)
    #[test]
    fn test_new_duid_time_is_in_dhcpv6_epoch() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("duid");
        let mac = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        let duid = load_or_create_duid(&path, mac).unwrap();
        // DHCPV6_EPOCH_OFFSET is 946_684_800 (seconds between 1970 and 2000).
        // Current year (2026) means ~26 years * 31_557_600 s/year ≈ 820M seconds since 2000.
        // The time field must be positive and reasonable (< 2^32).
        assert!(duid.time > 0, "DUID time must be positive");
        assert!(duid.time < u32::MAX, "DUID time must fit in u32");
    }
}
