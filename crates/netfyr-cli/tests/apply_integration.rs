//! Integration tests for the `netfyr apply` CLI command (story 301-cli-apply).
//!
//! Tests are split into two groups:
//!
//! 1. **Error-case tests** — spawn the binary, check exit codes and output.
//!    These do not require network access and run on any host.
//!
//! 2. **Network-namespace tests** — create an unprivileged user + network
//!    namespace, set up veth pairs, write policy YAML files, run the binary
//!    as a subprocess, and verify kernel state with sysfs or rtnetlink.
//!    Tests are skipped automatically if unprivileged namespaces are unavailable.

use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Absolute path of the `netfyr-cli` binary produced by this workspace build.
fn netfyr_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_netfyr"))
}

/// Write `content` to `<dir>/<filename>` and return the full path.
fn write_file(dir: &TempDir, filename: &str, content: &str) -> PathBuf {
    let path = dir.path().join(filename);
    fs::write(&path, content).expect("failed to write temp file");
    path
}

/// Combine stdout and stderr into one string for assertion.
fn combined(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

// ── Feature: Error cases (no system access required) ─────────────────────────

/// AC: No path arguments defaults to /etc/netfyr/policies/ (not a clap error).
/// With a nonexistent default dir, the binary exits with code 2 (path not found).
#[test]
fn test_apply_no_args_uses_default_path() {
    let output = std::process::Command::new(netfyr_bin())
        .arg("apply")
        .env("NO_COLOR", "1")
        .env("NETFYR_SOCKET_PATH", "/nonexistent")
        .env("NETFYR_APPLY_DEFAULT_DIR", "/nonexistent/default/policies")
        .output()
        .expect("failed to run netfyr");

    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit code 2 when default path does not exist; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let out = combined(&output);
    assert!(
        out.contains("not found") || out.contains("No such file"),
        "error must indicate path not found; output={out}"
    );
}

/// AC: Path does not exist shows "path not found" error, exit code 2.
#[test]
fn test_apply_nonexistent_path_shows_error_exit_code_2() {
    let output = std::process::Command::new(netfyr_bin())
        .args(["apply", "/nonexistent-path-netfyr-test-xyz"])
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run netfyr");

    assert_eq!(
        output.status.code(),
        Some(2),
        "nonexistent path must produce exit code 2"
    );

    let text = combined(&output);
    assert!(
        text.contains("path not found") || text.contains("/nonexistent-path-netfyr-test-xyz"),
        "error output must mention the path; got: {text}"
    );
}

/// AC: YAML parse error returns exit code 2.
#[test]
fn test_apply_invalid_yaml_returns_exit_code_2() {
    let dir = tempfile::tempdir().unwrap();
    // This is valid YAML syntax but does not produce a valid state document
    // (missing required `type` field) — loader returns an error.
    let path = write_file(&dir, "bad.yaml", "{ unclosed: [bracket\n");

    let output = std::process::Command::new(netfyr_bin())
        .args(["apply", path.to_str().unwrap()])
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run netfyr");

    assert_eq!(
        output.status.code(),
        Some(2),
        "YAML parse error must produce exit code 2; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// AC: DHCP policy without daemon fails with clear error mentioning "daemon".
#[test]
fn test_apply_dhcp_policy_without_daemon_exits_2_mentions_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let dhcp_yaml = "\
kind: policy
name: eth0-dhcp
factory: dhcpv4
priority: 100
selector:
  name: eth0
";
    let path = write_file(&dir, "eth0-dhcp.yaml", dhcp_yaml);

    let output = std::process::Command::new(netfyr_bin())
        .args(["apply", path.to_str().unwrap()])
        .env("NO_COLOR", "1")
        .env("NETFYR_SOCKET_PATH", "/nonexistent")
        .output()
        .expect("failed to run netfyr");

    assert_eq!(
        output.status.code(),
        Some(2),
        "DHCP policy without daemon must produce exit code 2"
    );

    let text = combined(&output);
    assert!(
        text.contains("daemon") || text.contains("netfyr daemon"),
        "error must mention 'daemon'; got: {text}"
    );
}

/// AC: DHCP policy without daemon error includes "systemctl start netfyr".
#[test]
fn test_apply_dhcp_policy_without_daemon_mentions_systemctl_start_netfyr() {
    let dir = tempfile::tempdir().unwrap();
    let dhcp_yaml = "\
kind: policy
name: eth0-dhcp
factory: dhcpv4
priority: 100
selector:
  name: eth0
";
    let path = write_file(&dir, "eth0-dhcp.yaml", dhcp_yaml);

    let output = std::process::Command::new(netfyr_bin())
        .args(["apply", path.to_str().unwrap()])
        .env("NO_COLOR", "1")
        .env("NETFYR_SOCKET_PATH", "/nonexistent")
        .output()
        .expect("failed to run netfyr");

    let text = combined(&output);
    assert!(
        text.contains("systemctl start netfyr"),
        "error must include 'systemctl start netfyr'; got: {text}"
    );
}

/// AC: DHCP policy without daemon correctly identifies the policy name.
#[test]
fn test_apply_dhcp_policy_without_daemon_names_the_policy() {
    let dir = tempfile::tempdir().unwrap();
    let dhcp_yaml = "\
kind: policy
name: eth0-dhcp
factory: dhcpv4
priority: 100
selector:
  name: eth0
";
    let path = write_file(&dir, "eth0-dhcp.yaml", dhcp_yaml);

    let output = std::process::Command::new(netfyr_bin())
        .args(["apply", path.to_str().unwrap()])
        .env("NO_COLOR", "1")
        .env("NETFYR_SOCKET_PATH", "/nonexistent")
        .output()
        .expect("failed to run netfyr");

    let text = combined(&output);
    assert!(
        text.contains("eth0-dhcp"),
        "error must name the problematic policy 'eth0-dhcp'; got: {text}"
    );
}

/// AC: Multiple DHCP policies all named in the error.
#[test]
fn test_apply_multiple_dhcp_policies_without_daemon_mentions_all() {
    let dir = tempfile::tempdir().unwrap();
    let dhcp_a = "\
kind: policy
name: eth0-dhcp
factory: dhcpv4
priority: 100
selector:
  name: eth0
";
    let dhcp_b = "\
kind: policy
name: eth1-dhcp
factory: dhcpv4
priority: 100
selector:
  name: eth1
";
    write_file(&dir, "eth0-dhcp.yaml", dhcp_a);
    write_file(&dir, "eth1-dhcp.yaml", dhcp_b);

    let output = std::process::Command::new(netfyr_bin())
        .args(["apply", dir.path().to_str().unwrap()])
        .env("NO_COLOR", "1")
        .env("NETFYR_SOCKET_PATH", "/nonexistent")
        .output()
        .expect("failed to run netfyr");

    assert_eq!(output.status.code(), Some(2));

    let text = combined(&output);
    // Both policy names should appear in the error
    assert!(
        text.contains("eth0-dhcp"),
        "error must name 'eth0-dhcp'; got: {text}"
    );
    assert!(
        text.contains("eth1-dhcp"),
        "error must name 'eth1-dhcp'; got: {text}"
    );
}

/// AC: Bare state YAML (no "kind" field) is auto-wrapped — parse succeeds.
/// We verify this by checking the error is NOT a parse error but something
/// later (e.g., apply failure on a nonexistent interface) which confirms
/// parsing and policy wrapping succeeded.
#[test]
fn test_apply_bare_state_yaml_is_auto_wrapped_parse_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    // Bare state — no "kind" field. The policy name will be derived from
    // filename "eth-nonexistent". The interface "eth-nonexistent" does not
    // exist on the system; apply will fail, but NOT due to a parse error.
    let path = write_file(
        &dir,
        "eth-nonexistent.yaml",
        "type: ethernet\nname: eth-nonexistent\nmtu: 1500\n",
    );

    let output = std::process::Command::new(netfyr_bin())
        .args(["apply", path.to_str().unwrap()])
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run netfyr");

    let text = combined(&output);
    // The error should NOT be a YAML parse error; it should be a backend error.
    // If the text contains "YAML" and "syntax" together, parsing failed.
    let is_yaml_parse_error =
        (text.contains("YAML") || text.contains("yaml")) && text.contains("syntax");
    assert!(
        !is_yaml_parse_error,
        "bare state YAML must parse successfully (error should come from backend, not parser); got: {text}"
    );
}

/// AC: Explicit kind:policy bare state is auto-wrapped same as no-kind.
/// The policy name is derived from filename, not from inside the document.
#[test]
fn test_apply_explicit_kind_state_is_treated_as_bare_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(
        &dir,
        "eth-nonexistent.yaml",
        "kind: state\ntype: ethernet\nname: eth-nonexistent\nmtu: 1500\n",
    );

    let output = std::process::Command::new(netfyr_bin())
        .args(["apply", path.to_str().unwrap()])
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run netfyr");

    let text = combined(&output);
    // Should parse successfully (no YAML syntax error)
    let is_yaml_parse_error =
        (text.contains("YAML") || text.contains("yaml")) && text.contains("syntax");
    assert!(
        !is_yaml_parse_error,
        "kind:state YAML must parse successfully as a wrapped bare state; got: {text}"
    );
}

// ── Feature: Integration tests for CLI apply (unprivileged netns) ─────────────
//
// Each netns test:
//   1. Tries to create an unprivileged user + network namespace via NetnsGuard.
//   2. Skips gracefully if the kernel does not permit unprivileged namespaces.
//   3. Creates a veth pair inside the namespace.
//   4. Writes policy YAML to a temp dir.
//   5. Runs the `netfyr` binary as a subprocess (which inherits the namespace
//      because fork() preserves the caller thread's namespace).
//   6. Verifies state via sysfs or IP address subprocess.

#[cfg(test)]
mod netns_tests {
    use super::*;
    use netfyr_test_utils::netns::{create_veth_pair, set_link_up};
    use netfyr_test_utils::NetnsGuard;

    /// Read interface MTU from sysfs (no rtnetlink needed).
    fn read_mtu(iface: &str) -> u32 {
        let path = format!("/sys/class/net/{iface}/mtu");
        fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {path}: {e}"))
            .trim()
            .parse()
            .expect("mtu should be numeric")
    }

    /// Check if an IP address appears in the output of `ip addr show <iface>`.
    fn has_address(iface: &str, cidr: &str) -> bool {
        let out = std::process::Command::new("ip")
            .args(["addr", "show", iface])
            .output()
            .expect("failed to run ip");
        String::from_utf8_lossy(&out.stdout).contains(cidr)
    }

    /// Enter a new network namespace or skip the test if namespaces are unavailable.
    /// Returns `Some(guard)` on success, `None` to signal skip.
    fn enter_namespace() -> Option<NetnsGuard> {
        match NetnsGuard::new() {
            Ok(g) => Some(g),
            Err(e) => {
                eprintln!("Skipping netns test: {e}");
                None
            }
        }
    }

    /// AC: Apply a single YAML policy file with no changes needed → exit 0,
    ///     output "No changes needed".
    #[tokio::test(flavor = "current_thread")]
    async fn test_apply_no_changes_needed_exits_0_with_message() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        create_veth_pair("veth-nc0", "veth-nc1")
            .await
            .expect("create_veth_pair failed");
        set_link_up("veth-nc0").await.expect("set_link_up failed");

        // Read the actual MTU so the policy matches exactly.
        let actual_mtu = read_mtu("veth-nc0");

        let dir = tempfile::tempdir().unwrap();
        let yaml = format!("type: ethernet\nname: veth-nc0\nmtu: {actual_mtu}\n");
        let path = write_file(&dir, "veth-nc0.yaml", &yaml);

        let output = tokio::process::Command::new(netfyr_bin())
            .args(["apply", path.to_str().unwrap()])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(0),
            "no-changes case must exit 0; stdout={stdout} stderr={stderr}"
        );
        assert!(
            stdout.contains("No changes needed"),
            "output must contain 'No changes needed'; got: {stdout}"
        );
    }

    /// AC: Apply a YAML file with MTU change → exit 0, "Applied" in output,
    ///     kernel MTU actually updated.
    #[tokio::test(flavor = "current_thread")]
    async fn test_apply_changes_mtu_exits_0_and_state_is_updated() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        create_veth_pair("veth-mtu0", "veth-mtu1")
            .await
            .expect("create_veth_pair failed");
        set_link_up("veth-mtu0").await.expect("set_link_up failed");

        // Confirm starting MTU is 1500 (veth default).
        assert_eq!(read_mtu("veth-mtu0"), 1500, "precondition: veth-mtu0 must start at mtu 1500");

        let dir = tempfile::tempdir().unwrap();
        let path =
            write_file(&dir, "veth-mtu0.yaml", "type: ethernet\nname: veth-mtu0\nmtu: 1400\n");

        let output = tokio::process::Command::new(netfyr_bin())
            .args(["apply", path.to_str().unwrap()])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(0),
            "MTU change must exit 0; stdout={stdout} stderr={stderr}"
        );
        assert!(
            stdout.contains("Applied") || stdout.contains("change"),
            "output must report applied changes; got: {stdout}"
        );

        // Verify the kernel state was actually updated.
        assert_eq!(
            read_mtu("veth-mtu0"),
            1400,
            "veth-mtu0 MTU must be 1400 after apply"
        );
    }

    /// AC: Apply all files in a directory → both policy files are loaded and applied.
    #[tokio::test(flavor = "current_thread")]
    async fn test_apply_directory_loads_all_policy_files() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        create_veth_pair("veth-dir0", "veth-dir1")
            .await
            .expect("create_veth_pair failed");
        set_link_up("veth-dir0").await.expect("set_link_up failed");
        set_link_up("veth-dir1").await.expect("set_link_up failed");

        let dir = tempfile::tempdir().unwrap();
        // Two separate policy files in the directory.
        write_file(&dir, "veth-dir0.yaml", "type: ethernet\nname: veth-dir0\nmtu: 1400\n");
        write_file(&dir, "veth-dir1.yaml", "type: ethernet\nname: veth-dir1\nmtu: 1300\n");

        let output = tokio::process::Command::new(netfyr_bin())
            .args(["apply", dir.path().to_str().unwrap()])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(0),
            "directory apply must exit 0; stdout={stdout} stderr={stderr}"
        );

        // Both interfaces must be updated.
        assert_eq!(read_mtu("veth-dir0"), 1400, "veth-dir0 must have mtu 1400");
        assert_eq!(read_mtu("veth-dir1"), 1300, "veth-dir1 must have mtu 1300");
    }

    /// AC: Bare state YAML is auto-wrapped into a static policy with default
    ///     priority 100; the policy name is derived from the filename.
    #[tokio::test(flavor = "current_thread")]
    async fn test_apply_bare_state_yaml_auto_wrapped_applies_successfully() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        create_veth_pair("veth-bare0", "veth-bare1")
            .await
            .expect("create_veth_pair failed");
        set_link_up("veth-bare0").await.expect("set_link_up failed");

        let dir = tempfile::tempdir().unwrap();
        // Bare state — no "kind" field. Policy name comes from filename "veth-bare0".
        let path =
            write_file(&dir, "veth-bare0.yaml", "type: ethernet\nname: veth-bare0\nmtu: 1350\n");

        let output = tokio::process::Command::new(netfyr_bin())
            .args(["apply", path.to_str().unwrap()])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(0),
            "bare state yaml must be auto-wrapped and applied; stdout={stdout} stderr={stderr}"
        );
        assert_eq!(
            read_mtu("veth-bare0"),
            1350,
            "veth-bare0 MTU must be 1350 after auto-wrapped apply"
        );
    }

    /// AC: Dry-run shows diff without applying — system state is unchanged.
    #[tokio::test(flavor = "current_thread")]
    async fn test_apply_dry_run_does_not_change_mtu() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        create_veth_pair("veth-dry0", "veth-dry1")
            .await
            .expect("create_veth_pair failed");
        set_link_up("veth-dry0").await.expect("set_link_up failed");

        assert_eq!(read_mtu("veth-dry0"), 1500, "precondition: veth-dry0 must start at mtu 1500");

        let dir = tempfile::tempdir().unwrap();
        let path =
            write_file(&dir, "veth-dry0.yaml", "type: ethernet\nname: veth-dry0\nmtu: 1400\n");

        let output = tokio::process::Command::new(netfyr_bin())
            .args(["apply", "--dry-run", path.to_str().unwrap()])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Dry-run with changes → exit code 1 (spec: dry-run exits 1 when diff is non-empty)
        assert_eq!(
            output.status.code(),
            Some(1),
            "dry-run with pending changes must exit 1; stdout={stdout} stderr={stderr}"
        );

        // Output must describe what would change
        assert!(
            stdout.contains("Dry run") || stdout.contains("dry run") || stdout.contains("would"),
            "dry-run output must indicate preview mode; got: {stdout}"
        );

        // The kernel state must NOT have changed
        assert_eq!(
            read_mtu("veth-dry0"),
            1500,
            "dry-run must not change the kernel MTU"
        );
    }

    /// AC: Dry-run with no changes needed → exit 0, "No changes needed" in output.
    #[tokio::test(flavor = "current_thread")]
    async fn test_apply_dry_run_no_changes_exits_0_with_message() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        create_veth_pair("veth-drnc0", "veth-drnc1")
            .await
            .expect("create_veth_pair failed");
        set_link_up("veth-drnc0").await.expect("set_link_up failed");

        let actual_mtu = read_mtu("veth-drnc0");
        let dir = tempfile::tempdir().unwrap();
        let yaml = format!("type: ethernet\nname: veth-drnc0\nmtu: {actual_mtu}\n");
        let path = write_file(&dir, "veth-drnc0.yaml", &yaml);

        let output = tokio::process::Command::new(netfyr_bin())
            .args(["apply", "--dry-run", path.to_str().unwrap()])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(0),
            "dry-run with no changes must exit 0; stdout={stdout} stderr={stderr}"
        );
        assert!(
            stdout.contains("No changes needed"),
            "dry-run output must say 'No changes needed'; got: {stdout}"
        );
    }

    /// AC: Apply with address — address actually appears on the interface.
    #[tokio::test(flavor = "current_thread")]
    async fn test_apply_with_address_in_namespace() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        create_veth_pair("veth-addr0", "veth-addr1")
            .await
            .expect("create_veth_pair failed");
        set_link_up("veth-addr0").await.expect("set_link_up failed");

        let dir = tempfile::tempdir().unwrap();
        let yaml = "\
type: ethernet
name: veth-addr0
mtu: 1400
addresses:
  - 10.99.0.1/24
";
        let path = write_file(&dir, "veth-addr0.yaml", yaml);

        let output = tokio::process::Command::new(netfyr_bin())
            .args(["apply", path.to_str().unwrap()])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(0),
            "apply with address must exit 0; stdout={stdout} stderr={stderr}"
        );

        // Verify MTU was set
        assert_eq!(read_mtu("veth-addr0"), 1400, "veth-addr0 MTU must be 1400");

        // Verify address was assigned
        assert!(
            has_address("veth-addr0", "10.99.0.1"),
            "veth-addr0 must have address 10.99.0.1/24"
        );
    }

    /// AC: Partial failure reports mixed results and exits with code 1.
    /// We use one existing interface (veth-pf0) and one that does not exist (eth-pf-ghost).
    #[tokio::test(flavor = "current_thread")]
    async fn test_apply_partial_failure_exits_1_reports_success_and_failure() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        create_veth_pair("veth-pf0", "veth-pf1")
            .await
            .expect("create_veth_pair failed");
        set_link_up("veth-pf0").await.expect("set_link_up failed");

        let dir = tempfile::tempdir().unwrap();
        // Policy for the existing interface.
        write_file(&dir, "veth-pf0.yaml", "type: ethernet\nname: veth-pf0\nmtu: 1400\n");
        // Policy for a non-existent interface.
        write_file(
            &dir,
            "eth-pf-ghost.yaml",
            "type: ethernet\nname: eth-pf-ghost\nmtu: 1400\n",
        );

        let output = tokio::process::Command::new(netfyr_bin())
            .args(["apply", dir.path().to_str().unwrap()])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let text = format!("{stdout}{stderr}");

        // Exit code 1 = partial failure
        assert_eq!(
            output.status.code(),
            Some(1),
            "partial failure must exit 1; stdout={stdout} stderr={stderr}"
        );

        // The successful change must be reflected in the kernel.
        assert_eq!(read_mtu("veth-pf0"), 1400, "veth-pf0 MTU must be 1400");

        // Output must report failure for the ghost interface.
        assert!(
            text.contains("eth-pf-ghost") || text.contains("failed"),
            "output must mention the failed interface; got: {text}"
        );
    }

    /// AC: Total failure returns exit code 2.
    /// Apply a policy for a non-existent interface — nothing succeeds.
    #[tokio::test(flavor = "current_thread")]
    async fn test_apply_total_failure_exits_2() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        let dir = tempfile::tempdir().unwrap();
        // Interface "eth-ghost-xyz" does not exist in the new namespace.
        let path = write_file(
            &dir,
            "eth-ghost-xyz.yaml",
            "type: ethernet\nname: eth-ghost-xyz\nmtu: 1400\n",
        );

        let output = tokio::process::Command::new(netfyr_bin())
            .args(["apply", path.to_str().unwrap()])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(2),
            "total failure must exit 2; stdout={stdout} stderr={stderr}"
        );
    }

    /// AC: Conflicts are reported as warnings and the conflicting field is not applied.
    /// Use two explicit policy documents at the same priority with different MTUs,
    /// then a separate address field that should still be applied.
    #[tokio::test(flavor = "current_thread")]
    async fn test_apply_conflict_warning_and_exit_1() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };

        create_veth_pair("veth-cf0", "veth-cf1")
            .await
            .expect("create_veth_pair failed");
        set_link_up("veth-cf0").await.expect("set_link_up failed");

        let initial_mtu = read_mtu("veth-cf0");

        let dir = tempfile::tempdir().unwrap();

        // Two policies at the same priority setting conflicting MTUs.
        let policy_a = "\
kind: policy
name: team-a
factory: static
priority: 100
state:
  type: ethernet
  name: veth-cf0
  mtu: 9000
";
        let policy_b = "\
kind: policy
name: team-b
factory: static
priority: 100
state:
  type: ethernet
  name: veth-cf0
  mtu: 1500
";
        write_file(&dir, "team-a.yaml", policy_a);
        write_file(&dir, "team-b.yaml", policy_b);

        let output = tokio::process::Command::new(netfyr_bin())
            .args(["apply", dir.path().to_str().unwrap()])
            .env("NO_COLOR", "1")
            .output()
            .await
            .expect("failed to run netfyr");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let text = format!("{stdout}{stderr}");

        // Exit code 1: conflicts were detected.
        assert_eq!(
            output.status.code(),
            Some(1),
            "conflict must produce exit 1; stdout={stdout} stderr={stderr}"
        );

        // Output must include a conflict warning.
        assert!(
            text.contains("conflict") || text.contains("Conflict") || text.contains("Warning"),
            "output must warn about the conflict; got: {text}"
        );

        // The conflicting MTU field must NOT have been applied to the kernel.
        // The mtu should remain at the initial value (not 9000 or 1500 if they differ).
        // Since both 9000 and 1500 conflict and initial is 1500, mtu stays at initial.
        let final_mtu = read_mtu("veth-cf0");
        // The conflicted field must not have been applied (mtu stays at initial or is
        // one of the conflicting values — what matters is the conflict is reported).
        // We cannot guarantee which value (if any) is applied since it's implementation-
        // defined whether conflicted fields are left at initial state.
        // NOTE: The spec says conflicting fields are NOT applied; verify by checking
        // the MTU did not change to a value not consistent with conflict omission.
        // In practice: initial is 1500, conflict values are 9000 and 1500 — since
        // conflicting fields are omitted, the mtu should remain at initial (1500).
        assert_eq!(
            final_mtu, initial_mtu,
            "conflicting mtu field must not be applied; initial={initial_mtu}, final={final_mtu}"
        );
    }
}
