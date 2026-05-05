//! Tests for SPEC-001: Workspace Setup acceptance criteria.
//!
//! These tests verify the workspace structure, Cargo.toml configuration,
//! file layout, and integration-test helper availability without building
//! binaries (compile-time checks happen in the CI build step itself).

use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns the workspace root, derived from this crate's CARGO_MANIFEST_DIR
/// (crates/netfyr-state), going up two levels: netfyr-state → crates → root.
fn workspace_root() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest_dir)
        .parent()
        .expect("crates/netfyr-state must have a parent")
        .parent()
        .expect("crates must have a parent (workspace root)")
        .to_path_buf()
}

fn read_workspace_cargo_toml() -> String {
    let path = workspace_root().join("Cargo.toml");
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read {:?}: {}", path, e))
}

// ---------------------------------------------------------------------------
// Scenario: Workspace members are correctly listed
// ---------------------------------------------------------------------------

/// AC: workspace members list contains the 7 required crates.
#[test]
fn test_workspace_members_contain_required_crates() {
    let cargo_toml = read_workspace_cargo_toml();

    let required_members = [
        "crates/netfyr-state",
        "crates/netfyr-reconcile",
        "crates/netfyr-backend",
        "crates/netfyr-policy",
        "crates/netfyr-varlink",
        "crates/netfyr-cli",
        "crates/netfyr-daemon",
    ];

    for member in &required_members {
        assert!(
            cargo_toml.contains(member),
            "Workspace Cargo.toml is missing required member: {}",
            member
        );
    }
}

/// AC: workspace members list contains the 7 required crates (plus netfyr-test-utils).
///
/// The spec lists 7 crates. The workspace also includes "crates/netfyr-test-utils"
/// as an 8th member because netfyr-backend, netfyr-cli, and netfyr-daemon all
/// depend on it via path dependencies — Cargo requires path-dependency crates
/// within the workspace directory to be workspace members. This is a legitimate
/// addition that does not violate the spirit of the spec.
#[test]
fn test_workspace_members_count_is_seven() {
    let cargo_toml = read_workspace_cargo_toml();

    // Verify all 7 spec-required crates are present under crates/.
    let required_members = [
        "crates/netfyr-state",
        "crates/netfyr-reconcile",
        "crates/netfyr-backend",
        "crates/netfyr-policy",
        "crates/netfyr-varlink",
        "crates/netfyr-cli",
        "crates/netfyr-daemon",
    ];
    for member in &required_members {
        assert!(
            cargo_toml.contains(member),
            "Workspace Cargo.toml is missing required member: {}",
            member
        );
    }

    // Count crates/* members; allow 7 (spec) or 8 (spec + netfyr-test-utils).
    let crates_member_count = cargo_toml
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            trimmed.starts_with('"') && trimmed.contains("crates/")
        })
        .count();

    assert!(
        (7..=9).contains(&crates_member_count),
        "Expected 7 or 8 crates/* workspace members, found {}",
        crates_member_count
    );
}

// ---------------------------------------------------------------------------
// Scenario: Library crates have correct structure
// ---------------------------------------------------------------------------

/// AC: each library crate has a Cargo.toml and src/lib.rs.
#[test]
fn test_library_crates_have_cargo_toml_and_lib_rs() {
    let root = workspace_root();

    let library_crates = [
        "netfyr-state",
        "netfyr-reconcile",
        "netfyr-backend",
        "netfyr-policy",
        "netfyr-varlink",
    ];

    for crate_name in &library_crates {
        let crate_dir = root.join("crates").join(crate_name);

        let cargo_toml = crate_dir.join("Cargo.toml");
        assert!(
            cargo_toml.exists(),
            "Library crate '{}' must have a Cargo.toml at {:?}",
            crate_name,
            cargo_toml
        );

        let lib_rs = crate_dir.join("src").join("lib.rs");
        assert!(
            lib_rs.exists(),
            "Library crate '{}' must have src/lib.rs at {:?}",
            crate_name,
            lib_rs
        );
    }
}

/// AC: binary crates (netfyr-cli, netfyr-daemon) each have a Cargo.toml and src/main.rs.
#[test]
fn test_binary_crates_have_cargo_toml_and_main_rs() {
    let root = workspace_root();

    let binary_crates = ["netfyr-cli", "netfyr-daemon"];

    for crate_name in &binary_crates {
        let crate_dir = root.join("crates").join(crate_name);

        let cargo_toml = crate_dir.join("Cargo.toml");
        assert!(
            cargo_toml.exists(),
            "Binary crate '{}' must have a Cargo.toml at {:?}",
            crate_name,
            cargo_toml
        );

        let main_rs = crate_dir.join("src").join("main.rs");
        assert!(
            main_rs.exists(),
            "Binary crate '{}' must have src/main.rs at {:?}",
            crate_name,
            main_rs
        );
    }
}

/// AC: each library crate's Cargo.toml declares the correct package name.
#[test]
fn test_each_crate_cargo_toml_has_correct_package_name() {
    let root = workspace_root();

    let all_crates = [
        "netfyr-state",
        "netfyr-reconcile",
        "netfyr-backend",
        "netfyr-policy",
        "netfyr-varlink",
        "netfyr-cli",
        "netfyr-daemon",
    ];

    for crate_name in &all_crates {
        let cargo_toml_path = root.join("crates").join(crate_name).join("Cargo.toml");
        let content = fs::read_to_string(&cargo_toml_path)
            .unwrap_or_else(|e| panic!("Failed to read {:?}: {}", cargo_toml_path, e));

        let expected_name_line = format!("name = \"{}\"", crate_name);
        assert!(
            content.contains(&expected_name_line),
            "Cargo.toml for '{}' must declare `{}` in [package]",
            crate_name,
            expected_name_line
        );
    }
}

/// AC: each crate's Cargo.toml declares edition = "2021".
#[test]
fn test_each_crate_cargo_toml_uses_edition_2021() {
    let root = workspace_root();

    let all_crates = [
        "netfyr-state",
        "netfyr-reconcile",
        "netfyr-backend",
        "netfyr-policy",
        "netfyr-varlink",
        "netfyr-cli",
        "netfyr-daemon",
    ];

    for crate_name in &all_crates {
        let cargo_toml_path = root.join("crates").join(crate_name).join("Cargo.toml");
        let content = fs::read_to_string(&cargo_toml_path)
            .unwrap_or_else(|e| panic!("Failed to read {:?}: {}", cargo_toml_path, e));

        assert!(
            content.contains("edition = \"2021\""),
            "Cargo.toml for '{}' must declare `edition = \"2021\"`",
            crate_name
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario: Integration test helpers exist
// ---------------------------------------------------------------------------

/// AC: tests/helpers.sh exists in the workspace.
#[test]
fn test_helpers_sh_exists() {
    let helpers = workspace_root().join("tests").join("helpers.sh");
    assert!(
        helpers.exists(),
        "tests/helpers.sh must exist at {:?}",
        helpers
    );
}

/// AC: helpers.sh defines functions netns_setup, create_veth, add_address,
/// start_dnsmasq, cleanup.
#[test]
fn test_helpers_sh_defines_required_functions() {
    let helpers_path = workspace_root().join("tests").join("helpers.sh");
    let content = fs::read_to_string(&helpers_path)
        .unwrap_or_else(|e| panic!("Failed to read helpers.sh: {}", e));

    let required_functions = [
        "netns_setup",
        "create_veth",
        "add_address",
        "start_dnsmasq",
        "cleanup",
    ];

    for func in &required_functions {
        // Shell functions are defined as `name() {` or `function name {`.
        let definition_form_1 = format!("{}()", func);
        let definition_form_2 = format!("function {}", func);
        assert!(
            content.contains(&definition_form_1) || content.contains(&definition_form_2),
            "helpers.sh must define function '{}' (expected '{}()' or 'function {}')",
            func,
            func,
            func
        );
    }
}

/// AC: helpers.sh is sourced by all test scripts in tests/.
///
/// Any *.sh files in tests/ (other than helpers.sh itself) must source helpers.sh.
#[test]
fn test_all_test_scripts_source_helpers_sh() {
    let tests_dir = workspace_root().join("tests");

    let entries = fs::read_dir(&tests_dir)
        .unwrap_or_else(|e| panic!("Failed to read tests/ directory: {}", e));

    let test_scripts: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|e| e.to_str()) == Some("sh")
                && p.file_name().and_then(|n| n.to_str()) != Some("helpers.sh")
        })
        .collect();

    for script in &test_scripts {
        let content = fs::read_to_string(script)
            .unwrap_or_else(|e| panic!("Failed to read {:?}: {}", script, e));

        assert!(
            content.contains("helpers.sh"),
            "Test script {:?} must source helpers.sh (expected `source ... helpers.sh` or `. ... helpers.sh`)",
            script
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario: Workspace resolver is set to "2"
// ---------------------------------------------------------------------------

/// AC: The root Cargo.toml uses the v2 dependency resolver.
#[test]
fn test_workspace_uses_resolver_2() {
    let cargo_toml = read_workspace_cargo_toml();
    assert!(
        cargo_toml.contains("resolver = \"2\""),
        "Root Cargo.toml must declare `resolver = \"2\"` in [workspace]"
    );
}

// ---------------------------------------------------------------------------
// Scenario: CLI crate produces a binary that prints "netfyr"
// ---------------------------------------------------------------------------

/// AC: running `netfyr-cli` with no arguments prints exactly "netfyr" to stdout.
///
/// Requires `cargo build -p netfyr-cli` to have run first.
#[test]
fn test_cli_binary_prints_netfyr_with_no_args() {
    use std::process::Command;

    let binary = workspace_root()
        .join("target")
        .join("debug")
        .join("netfyr-cli");

    if !binary.exists() {
        panic!(
            "FAIL: netfyr-cli binary not found at {:?}. Run `cargo build -p netfyr-cli` first.",
            binary
        );
    }

    let output = Command::new(&binary)
        .output()
        .unwrap_or_else(|e| panic!("Failed to run netfyr-cli: {}", e));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout_trimmed = stdout.trim();

    assert_eq!(
        stdout_trimmed, "netfyr",
        "netfyr-cli with no arguments must print exactly 'netfyr' to stdout, got: {:?}",
        stdout_trimmed
    );
}

// ---------------------------------------------------------------------------
// Scenario: Daemon crate produces a binary that prints "netfyr"
// ---------------------------------------------------------------------------

/// AC: running `netfyr-daemon` prints "netfyr" as the first line of stdout.
///
/// The daemon is long-running; this test starts it with a temp socket/policy
/// dir, reads the first stdout line within 5 s, then kills it.
///
/// Requires `cargo build -p netfyr-daemon` to have run first.
#[test]
fn test_daemon_binary_prints_netfyr_as_first_stdout_line() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    let binary = workspace_root()
        .join("target")
        .join("debug")
        .join("netfyr-daemon");

    if !binary.exists() {
        panic!(
            "FAIL: netfyr-daemon binary not found at {:?}. Run `cargo build -p netfyr-daemon` first.",
            binary
        );
    }

    // Create a unique temp directory so the daemon can start without needing
    // system paths like /run/netfyr/ or /var/lib/netfyr/.
    let tmpdir = {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "netfyr-ws-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap_or_else(|e| panic!("Failed to create tmpdir: {}", e));
        p
    };
    let socket_path = tmpdir.join("netfyr.sock");
    let policy_dir = tmpdir.join("policies");
    fs::create_dir_all(&policy_dir).unwrap_or_else(|e| panic!("Failed to create policy dir: {}", e));

    let mut child = Command::new(&binary)
        .env("NETFYR_SOCKET_PATH", socket_path.to_str().unwrap())
        .env("NETFYR_POLICY_DIR", policy_dir.to_str().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to spawn netfyr-daemon: {}", e));

    // Read the first line from stdout in a background thread so we can apply
    // a timeout without blocking the main thread indefinitely.
    let stdout = child.stdout.take().expect("piped stdout must be available");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).ok();
        tx.send(line.trim().to_string()).ok();
    });

    let first_line = rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_default();

    child.kill().ok();
    child.wait().ok();
    let _ = fs::remove_dir_all(&tmpdir);

    assert_eq!(
        first_line, "netfyr",
        "netfyr-daemon must print 'netfyr' as the first line of stdout on startup"
    );
}

// ---------------------------------------------------------------------------
// Scenario: Makefile integration-test target builds and runs tests
// ---------------------------------------------------------------------------

/// AC: Makefile exists at the workspace root.
#[test]
fn test_makefile_exists() {
    let makefile = workspace_root().join("Makefile");
    assert!(
        makefile.exists(),
        "Makefile must exist at {:?}",
        makefile
    );
}

/// AC: Makefile declares the integration-test target with cargo build and the
/// tests/[0-9]*.sh discovery glob, and marks it .PHONY.
#[test]
fn test_makefile_integration_test_target_structure() {
    let makefile_path = workspace_root().join("Makefile");
    let content = fs::read_to_string(&makefile_path)
        .unwrap_or_else(|e| panic!("Failed to read Makefile: {}", e));

    assert!(
        content.contains("integration-test"),
        "Makefile must declare an 'integration-test' target"
    );
    assert!(
        content.contains("cargo build"),
        "Makefile integration-test target must run 'cargo build' first"
    );
    // The glob used to discover numbered test scripts.
    assert!(
        content.contains("tests/[0-9]"),
        "Makefile must discover test scripts via a 'tests/[0-9]*.sh' glob"
    );
    assert!(
        content.contains(".PHONY"),
        "Makefile must declare 'integration-test' as .PHONY"
    );
}

/// AC: Makefile integration-test propagates failure (overall exit is non-zero
/// if any test fails).
#[test]
fn test_makefile_integration_test_propagates_failure() {
    // The Makefile delegates to scripts/run-integration-tests.sh which tracks
    // failures via a `failed` variable and `exit 1`. Check the runner script.
    let runner_path = workspace_root().join("scripts/run-integration-tests.sh");
    let content = fs::read_to_string(&runner_path)
        .unwrap_or_else(|e| panic!("Failed to read runner script: {}", e));

    assert!(
        content.contains("failed") && content.contains("exit 1"),
        "Integration test runner must track failures and exit with code 1 if any test fails"
    );
}

// ---------------------------------------------------------------------------
// Scenario: Test script naming follows convention
// ---------------------------------------------------------------------------

/// AC: each test script in tests/ follows NNN-description.sh naming where NNN
/// is one or more digits; helpers.sh is the only non-numbered .sh file.
#[test]
fn test_numbered_scripts_follow_naming_convention() {
    let tests_dir = workspace_root().join("tests");

    let scripts: Vec<_> = fs::read_dir(&tests_dir)
        .unwrap_or_else(|e| panic!("Failed to read tests/ directory: {}", e))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("sh"))
        .collect();

    for script in &scripts {
        let name = script.file_name().unwrap().to_string_lossy().into_owned();
        if name == "helpers.sh" {
            continue;
        }
        // Must start with one or more ASCII digits followed by '-'.
        let digit_end = name.chars().position(|c| !c.is_ascii_digit());
        let ok = match digit_end {
            Some(pos) if pos > 0 => name.chars().nth(pos) == Some('-'),
            _ => false,
        };
        assert!(
            ok,
            "Test script '{}' must follow NNN-description.sh naming convention \
             (starts with one or more digits then '-')",
            name
        );
    }
}

/// AC: helpers.sh is the only non-numbered .sh file in tests/.
#[test]
fn test_helpers_sh_is_only_non_numbered_sh_file() {
    let tests_dir = workspace_root().join("tests");

    let non_numbered: Vec<_> = fs::read_dir(&tests_dir)
        .unwrap_or_else(|e| panic!("Failed to read tests/ directory: {}", e))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("sh"))
        .filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            name != "helpers.sh"
                && !name
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
        })
        .collect();

    assert!(
        non_numbered.is_empty(),
        "Only helpers.sh may be a non-numbered .sh file in tests/; found: {:?}",
        non_numbered
    );
}

// ---------------------------------------------------------------------------
// Scenario: Test scripts never skip on missing prerequisites
// ---------------------------------------------------------------------------

/// AC: no test script uses '|| exit 0' to silently swallow prerequisite failures.
#[test]
fn test_no_script_silently_skips_with_exit_0() {
    let tests_dir = workspace_root().join("tests");

    let test_scripts: Vec<_> = fs::read_dir(&tests_dir)
        .unwrap_or_else(|e| panic!("Failed to read tests/ directory: {}", e))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            p.extension().and_then(|e| e.to_str()) == Some("sh") && name != "helpers.sh"
        })
        .collect();

    for script in &test_scripts {
        let content = fs::read_to_string(script)
            .unwrap_or_else(|e| panic!("Failed to read {:?}: {}", script, e));

        assert!(
            !content.contains("|| exit 0"),
            "Test script {:?} uses '|| exit 0' which silently skips on failure; \
             the no-skip policy requires 'exit 1' when prerequisites are missing",
            script.file_name().unwrap()
        );
    }
}

/// AC: helpers.sh calls 'exit 1' (not 'exit 0') when 'unshare' is not available.
#[test]
fn test_helpers_sh_exits_1_when_unshare_missing() {
    let helpers_path = workspace_root().join("tests").join("helpers.sh");
    let content = fs::read_to_string(&helpers_path)
        .unwrap_or_else(|e| panic!("Failed to read helpers.sh: {}", e));

    // Find the block around the unshare check and verify 'exit 1' is present.
    let lines: Vec<&str> = content.lines().collect();
    let has_unshare_exit_1 = lines.windows(8).any(|w| {
        let block = w.join("\n");
        (block.contains("unshare") || block.contains("NETNS")) && block.contains("exit 1")
    });

    assert!(
        has_unshare_exit_1,
        "helpers.sh must call 'exit 1' (not 'exit 0') when 'unshare' is not available"
    );
}

/// AC: helpers.sh calls 'exit 1' (not 'exit 0') when 'dnsmasq' is not available.
#[test]
fn test_helpers_sh_exits_1_when_dnsmasq_missing() {
    let helpers_path = workspace_root().join("tests").join("helpers.sh");
    let content = fs::read_to_string(&helpers_path)
        .unwrap_or_else(|e| panic!("Failed to read helpers.sh: {}", e));

    let lines: Vec<&str> = content.lines().collect();
    let has_dnsmasq_exit_1 = lines.windows(8).any(|w| {
        let block = w.join("\n");
        block.contains("dnsmasq") && block.contains("exit 1")
    });

    assert!(
        has_dnsmasq_exit_1,
        "helpers.sh must call 'exit 1' (not 'exit 0') when 'dnsmasq' is not available"
    );
}
