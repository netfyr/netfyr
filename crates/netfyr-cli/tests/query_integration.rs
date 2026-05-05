//! Integration tests for the `netfyr query` CLI command (story 302-cli-query).
//!
//! Tests are split into two groups:
//!
//! 1. **Error-case tests** — spawn the binary, check exit codes and stderr/stdout.
//!    These do not require network access and run on any host.
//!
//! 2. **Network-namespace tests** — create an unprivileged user + network
//!    namespace, set up veth pairs, run the binary as a subprocess, and verify
//!    the JSON or YAML output.
//!    Tests are skipped automatically if unprivileged namespaces are unavailable.

use std::path::PathBuf;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Absolute path of the `netfyr-cli` binary produced by this workspace build.
fn netfyr_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_netfyr"))
}

/// Combine stdout and stderr into one string for assertion messages.
fn combined(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

// ── Feature: Error cases (no system access required) ─────────────────────────

/// AC: Invalid selector key shows an error about the invalid key and lists
///     valid selector keys: type, name, driver, mac, pci_path.
///     Exit code is 2.
#[test]
fn test_query_invalid_selector_key_shows_error_and_lists_valid_keys_exit_2() {
    let output = std::process::Command::new(netfyr_bin())
        .args(["query", "--selector", "invalid_key=value"])
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run netfyr");

    assert_eq!(
        output.status.code(),
        Some(2),
        "invalid selector key must produce exit code 2; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let text = combined(&output);
    // Error must list all valid selector keys.
    assert!(
        text.contains("type")
            && text.contains("name")
            && text.contains("driver")
            && text.contains("mac")
            && text.contains("pci_path"),
        "error must list valid selector keys: type, name, driver, mac, pci_path; got: {text}"
    );
}

/// AC: Same error when using the short -s flag with an invalid key.
#[test]
fn test_query_invalid_selector_key_via_short_flag_shows_error_exit_2() {
    let output = std::process::Command::new(netfyr_bin())
        .args(["query", "-s", "bogus_field=value"])
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run netfyr");

    assert_eq!(
        output.status.code(),
        Some(2),
        "invalid selector key via -s must produce exit code 2"
    );

    let text = combined(&output);
    assert!(
        text.contains("name") && text.contains("driver"),
        "error must list valid keys; got: {text}"
    );
}

/// Selector argument missing '=' returns an error (not key=value format).
#[test]
fn test_query_selector_without_equals_sign_shows_error_exit_2() {
    let output = std::process::Command::new(netfyr_bin())
        .args(["query", "--selector", "nameeth0"])
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run netfyr");

    assert_eq!(
        output.status.code(),
        Some(2),
        "selector without '=' must produce exit code 2"
    );
}

/// AC: Invalid type value shows an error ("Unknown entity type: ...") and lists
///     valid entity types.  Exit code is 2.
#[test]
fn test_query_invalid_type_value_shows_error_with_valid_types_exit_2() {
    let output = std::process::Command::new(netfyr_bin())
        .args(["query", "--selector", "type=foobar_unknown_type_xyz"])
        .env("NO_COLOR", "1")
        .env("NETFYR_SOCKET_PATH", "/nonexistent")
        .output()
        .expect("failed to run netfyr");

    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown entity type must produce exit code 2; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let text = combined(&output);
    // Error must mention the invalid type.
    assert!(
        text.contains("foobar_unknown_type_xyz")
            || text.to_lowercase().contains("unknown")
            || text.to_lowercase().contains("invalid"),
        "error must mention the unknown type; got: {text}"
    );
    // Error must list at least one valid entity type.
    assert!(
        text.contains("ethernet"),
        "error must list valid entity types (at minimum 'ethernet'); got: {text}"
    );
}

/// AC: --output yaml is an accepted argument value (does not cause clap error).
#[test]
fn test_query_explicit_yaml_output_flag_accepted_by_clap() {
    // We only care that clap accepts the argument, not that it returns results.
    // Exit code 2 means clap rejected the value; 0 means success.
    let output = std::process::Command::new(netfyr_bin())
        .args(["query", "--output", "yaml"])
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run netfyr");

    assert_ne!(
        output.status.code(),
        Some(2),
        "--output yaml must be accepted by clap; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// AC: Short flag -o is accepted (same as --output).
#[test]
fn test_query_short_output_flag_o_accepted_by_clap() {
    let output = std::process::Command::new(netfyr_bin())
        .args(["query", "-o", "json"])
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run netfyr");

    assert_ne!(
        output.status.code(),
        Some(2),
        "short -o flag must be accepted by clap; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// AC: Short flag -s is accepted (same as --selector).
#[test]
fn test_query_short_selector_flag_s_accepted_by_clap() {
    let output = std::process::Command::new(netfyr_bin())
        .args(["query", "-s", "type=ethernet"])
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run netfyr");

    assert_ne!(
        output.status.code(),
        Some(2),
        "short -s flag must be accepted by clap; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// AC: No matching entities returns an empty list and exit code 0 (default YAML).
#[test]
fn test_query_no_matching_entities_empty_yaml_exit_0() {
    let output = std::process::Command::new(netfyr_bin())
        .args(["query", "--selector", "name=eth99_definitely_does_not_exist_xyz"])
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run netfyr");

    assert_eq!(
        output.status.code(),
        Some(0),
        "no matching entities must exit 0; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Output must be valid YAML (even if empty).
    let parsed: Result<serde_yaml::Value, _> = serde_yaml::from_str(&stdout);
    assert!(
        parsed.is_ok(),
        "output must be valid YAML for empty result; got: {stdout}"
    );
}

/// AC: No matching entities in JSON format → empty JSON array, exit code 0.
#[test]
fn test_query_no_matching_entities_empty_json_array_exit_0() {
    let output = std::process::Command::new(netfyr_bin())
        .args([
            "query",
            "--selector",
            "name=eth99_definitely_does_not_exist_xyz",
            "--output",
            "json",
        ])
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run netfyr");

    assert_eq!(
        output.status.code(),
        Some(0),
        "no matching entities in JSON must exit 0"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|_| panic!("output must be valid JSON for empty result; got: {stdout}"));
    assert!(parsed.is_array(), "JSON output for empty result must be an array");
    assert!(
        parsed.as_array().unwrap().is_empty(),
        "JSON array must be empty when no entities match; got: {stdout}"
    );
}

// ── Feature: Integration tests for CLI query (unprivileged netns) ─────────────

#[cfg(test)]
mod netns_tests {
    use super::*;
    use netfyr_test_utils::netns::{add_address, create_veth_pair, set_link_up, set_mtu};
    use netfyr_test_utils::NetnsGuard;

    /// Enter a new unprivileged network namespace or skip the test if unavailable.
    fn enter_namespace() -> Option<NetnsGuard> {
        match NetnsGuard::new() {
            Ok(g) => Some(g),
            Err(e) => {
                eprintln!("Skipping netns test: {e}");
                None
            }
        }
    }

    /// AC: Query veth interface in namespace → exit 0, JSON contains name, mtu, and addresses.
    #[tokio::test(flavor = "current_thread")]
    async fn test_query_veth_json_output_has_name_mtu_and_addresses() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        create_veth_pair("veth-q0", "veth-q1")
            .await
            .expect("create_veth_pair failed");
        set_link_up("veth-q0").await.expect("set_link_up failed");
        set_mtu("veth-q0", 1400).await.expect("set_mtu failed");
        add_address("veth-q0", "10.99.0.1/24")
            .await
            .expect("add_address failed");

        let output = tokio::process::Command::new(netfyr_bin())
            .args(["query", "-s", "name=veth-q0", "-o", "json"])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(0),
            "query must exit 0; stdout={stdout} stderr={stderr}"
        );

        let parsed: serde_json::Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|_| panic!("output must be valid JSON; got: {stdout}"));
        assert!(parsed.is_array(), "output must be a JSON array");

        let arr = parsed.as_array().unwrap();
        assert_eq!(
            arr.len(),
            1,
            "must return exactly one entity for veth-q0; got {}: {stdout}",
            arr.len()
        );

        let entity = &arr[0];
        // AC: entity shows type, selector properties, and config fields at the top level
        assert_eq!(
            entity["type"].as_str(),
            Some("ethernet"),
            "entity type must be 'ethernet'"
        );
        assert_eq!(
            entity["name"].as_str(),
            Some("veth-q0"),
            "entity name must be 'veth-q0'"
        );
        // AC: mtu=1400 is present
        assert_eq!(
            entity["mtu"].as_u64(),
            Some(1400),
            "entity mtu must be 1400"
        );
        // AC: addresses contains 10.99.0.1/24
        let addresses = entity["addresses"]
            .as_array()
            .expect("addresses field must be a JSON array");
        let has_addr = addresses
            .iter()
            .any(|a| a.as_str().map(|s| s.contains("10.99.0.1")).unwrap_or(false));
        assert!(
            has_addr,
            "addresses must contain 10.99.0.1/24; got: {addresses:?}"
        );
    }

    /// AC: Query all interfaces in namespace returns at least 2 entities (both veth ends).
    #[tokio::test(flavor = "current_thread")]
    async fn test_query_all_interfaces_in_namespace_returns_both_veth_ends() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        create_veth_pair("veth-qa0", "veth-qa1")
            .await
            .expect("create_veth_pair failed");

        let output = tokio::process::Command::new(netfyr_bin())
            .args(["query", "-o", "json"])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(0),
            "query all must exit 0; stdout={stdout} stderr={stderr}"
        );

        let parsed: serde_json::Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|_| panic!("output must be valid JSON; got: {stdout}"));
        let arr = parsed.as_array().expect("output must be a JSON array");
        assert!(
            arr.len() >= 2,
            "query all must return at least 2 entities (both veth ends); got {}: {stdout}",
            arr.len()
        );
    }

    /// AC: YAML output format (default) — produces valid YAML containing mtu: 1400.
    #[tokio::test(flavor = "current_thread")]
    async fn test_query_default_yaml_output_is_valid_yaml_and_contains_mtu() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        create_veth_pair("veth-qy0", "veth-qy1")
            .await
            .expect("create_veth_pair failed");
        set_link_up("veth-qy0").await.expect("set_link_up failed");
        set_mtu("veth-qy0", 1400).await.expect("set_mtu failed");

        // Default output is YAML (no --output flag).
        let output = tokio::process::Command::new(netfyr_bin())
            .args(["query", "-s", "name=veth-qy0"])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(0),
            "query must exit 0; stdout={stdout} stderr={stderr}"
        );

        // AC: Output is valid YAML.
        let parsed: serde_yaml::Value = serde_yaml::from_str(&stdout)
            .unwrap_or_else(|_| panic!("output must be valid YAML; got: {stdout}"));
        assert!(
            parsed.is_sequence(),
            "YAML output must be a sequence; got: {stdout}"
        );

        // AC: Contains "mtu: 1400".
        assert!(
            stdout.contains("mtu: 1400"),
            "YAML output must contain 'mtu: 1400'; got: {stdout}"
        );
    }

    /// AC: Query by entity type via selector → only ethernet entities returned.
    #[tokio::test(flavor = "current_thread")]
    async fn test_query_by_type_ethernet_returns_only_ethernet_entities() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        create_veth_pair("veth-qt0", "veth-qt1")
            .await
            .expect("create_veth_pair failed");

        let output = tokio::process::Command::new(netfyr_bin())
            .args(["query", "-s", "type=ethernet", "-o", "json"])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(0),
            "query by type=ethernet must exit 0; stdout={stdout} stderr={stderr}"
        );

        let parsed: serde_json::Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|_| panic!("output must be valid JSON; got: {stdout}"));
        let arr = parsed.as_array().expect("output must be a JSON array");

        // All returned entities must have type = "ethernet".
        for entity in arr {
            assert_eq!(
                entity["type"].as_str(),
                Some("ethernet"),
                "all returned entities must have type=ethernet; got: {entity}"
            );
        }
        // Both veth ends are ethernet — at least 2 must be returned.
        assert!(
            arr.len() >= 2,
            "at least 2 ethernet entities (the veth pair) must be returned; got {}: {stdout}",
            arr.len()
        );
    }

    /// AC: Query with name selector → only the named interface is returned; the peer is absent.
    #[tokio::test(flavor = "current_thread")]
    async fn test_query_with_name_selector_returns_only_named_interface() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        create_veth_pair("veth-qn0", "veth-qn1")
            .await
            .expect("create_veth_pair failed");

        let output = tokio::process::Command::new(netfyr_bin())
            .args(["query", "-s", "name=veth-qn0", "-o", "json"])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(0),
            "query by name must exit 0; stdout={stdout} stderr={stderr}"
        );

        let parsed: serde_json::Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|_| panic!("output must be valid JSON; got: {stdout}"));
        let arr = parsed.as_array().expect("output must be a JSON array");

        // Only veth-qn0 must be present.
        assert_eq!(
            arr.len(),
            1,
            "name selector must return exactly one entity; got {}: {stdout}",
            arr.len()
        );
        assert_eq!(
            arr[0]["name"].as_str(),
            Some("veth-qn0"),
            "returned entity must be named veth-qn0"
        );

        // AC: eth1 (veth-qn1) is not shown.
        let has_qn1 = arr.iter().any(|e| e["name"].as_str() == Some("veth-qn1"));
        assert!(
            !has_qn1,
            "veth-qn1 must not appear in output when --selector name=veth-qn0; got: {stdout}"
        );
    }

    /// AC: Multiple selectors use AND logic — only entities matching ALL selectors are returned.
    #[tokio::test(flavor = "current_thread")]
    async fn test_query_multiple_selectors_and_logic_returns_matching_entity_only() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        create_veth_pair("veth-qm0", "veth-qm1")
            .await
            .expect("create_veth_pair failed");

        // type=ethernet AND name=veth-qm0 — only veth-qm0 should match.
        let output = tokio::process::Command::new(netfyr_bin())
            .args([
                "query",
                "-s", "type=ethernet",
                "-s", "name=veth-qm0",
                "-o", "json",
            ])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(0),
            "query with AND selectors must exit 0; stdout={stdout} stderr={stderr}"
        );

        let parsed: serde_json::Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|_| panic!("output must be valid JSON; got: {stdout}"));
        let arr = parsed.as_array().expect("output must be a JSON array");

        // AND logic: only veth-qm0 (ethernet + name=veth-qm0) must be returned.
        assert_eq!(
            arr.len(),
            1,
            "AND logic must return exactly one entity; got {}: {stdout}",
            arr.len()
        );
        assert_eq!(arr[0]["name"].as_str(), Some("veth-qm0"));
        assert_eq!(arr[0]["type"].as_str(), Some("ethernet"));
    }

    /// AC: Short -o json flag produces valid JSON (same as --output json).
    #[tokio::test(flavor = "current_thread")]
    async fn test_query_short_output_flag_produces_valid_json_same_as_long_flag() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        create_veth_pair("veth-qo0", "veth-qo1")
            .await
            .expect("create_veth_pair failed");

        let short_output = tokio::process::Command::new(netfyr_bin())
            .args(["query", "-s", "type=ethernet", "-o", "json"])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let long_output = tokio::process::Command::new(netfyr_bin())
            .args(["query", "--selector", "type=ethernet", "--output", "json"])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        // Both must succeed.
        assert_eq!(short_output.status.code(), Some(0), "-o json must exit 0");
        assert_eq!(long_output.status.code(), Some(0), "--output json must exit 0");

        // Both must produce valid JSON arrays.
        let short_stdout = String::from_utf8_lossy(&short_output.stdout);
        let long_stdout = String::from_utf8_lossy(&long_output.stdout);

        let short_json: serde_json::Value =
            serde_json::from_str(&short_stdout)
                .unwrap_or_else(|_| panic!("-o json must produce valid JSON; got: {short_stdout}"));
        let long_json: serde_json::Value =
            serde_json::from_str(&long_stdout)
                .unwrap_or_else(|_| panic!("--output json must produce valid JSON; got: {long_stdout}"));

        assert!(short_json.is_array(), "-o json must produce a JSON array");
        assert!(long_json.is_array(), "--output json must produce a JSON array");
    }

    /// AC: JSON output can be piped to jq — mtu value is directly accessible.
    #[tokio::test(flavor = "current_thread")]
    async fn test_query_json_output_mtu_accessible_for_jq_piping() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        create_veth_pair("veth-jq0", "veth-jq1")
            .await
            .expect("create_veth_pair failed");
        set_link_up("veth-jq0").await.expect("set_link_up failed");
        set_mtu("veth-jq0", 1400).await.expect("set_mtu failed");

        let output = tokio::process::Command::new(netfyr_bin())
            .args(["query", "-s", "name=veth-jq0", "-o", "json"])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            output.status.code(),
            Some(0),
            "query must exit 0; got: {stdout}"
        );

        // Simulate `jq '.[].mtu'` by parsing JSON and extracting mtu.
        let parsed: serde_json::Value =
            serde_json::from_str(&stdout)
                .unwrap_or_else(|_| panic!("output must be valid JSON; got: {stdout}"));
        let arr = parsed.as_array().expect("output must be a JSON array");
        assert_eq!(arr.len(), 1, "must return 1 entity for veth-jq0");
        let mtu = arr[0]["mtu"].as_u64();
        assert_eq!(
            mtu,
            Some(1400),
            "mtu must be 1400 (simulating jq '.[].mtu'); got: {mtu:?}"
        );
    }

    /// AC: Default output (no -o flag) is YAML; each entity has a 'type' field at the top level.
    #[tokio::test(flavor = "current_thread")]
    async fn test_query_default_output_is_yaml_sequence_with_type_field() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        create_veth_pair("veth-qd0", "veth-qd1")
            .await
            .expect("create_veth_pair failed");

        let output = tokio::process::Command::new(netfyr_bin())
            .args(["query", "-s", "type=ethernet"])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(0),
            "default YAML output must exit 0; stdout={stdout} stderr={stderr}"
        );

        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&stdout)
                .unwrap_or_else(|_| panic!("default output must be valid YAML; got: {stdout}"));

        assert!(
            parsed.is_sequence(),
            "default output must be a YAML sequence; got: {stdout}"
        );

        // AC: Each entry shows type, selector properties, and config fields at the top level.
        if let serde_yaml::Value::Sequence(seq) = &parsed {
            assert!(!seq.is_empty(), "must return at least one entity");
            for entity in seq {
                assert!(
                    entity.get("type").is_some(),
                    "each YAML entity must have a 'type' field at the top level; got: {entity:?}"
                );
            }
        }
    }
}
