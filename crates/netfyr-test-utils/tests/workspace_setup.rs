//! Integration tests for SPEC-001: Workspace Setup
//!
//! Verifies that the Rust workspace is correctly configured with all eight
//! crates, proper feature flags, expected file structure, and working binaries.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Returns the absolute path to the workspace root by walking up from this
/// crate's manifest directory (crates/netfyr-test-utils → crates → workspace root).
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent() // crates/
        .expect("crate dir has no parent")
        .parent() // workspace root
        .expect("crates dir has no parent")
        .to_path_buf()
}

// ---------------------------------------------------------------------------
// Scenario: Root workspace compiles successfully
// ---------------------------------------------------------------------------

/// AC: `cargo build` from the project root succeeds with exit code 0.
#[test]
fn test_workspace_compiles_successfully() {
    let root = workspace_root();
    let status = Command::new("cargo")
        .args(["build"])
        .current_dir(&root)
        .status()
        .expect("failed to invoke cargo");
    assert!(
        status.success(),
        "cargo build failed with status: {}",
        status
    );
}

// ---------------------------------------------------------------------------
// Scenario: Individual crate compiles in isolation
// ---------------------------------------------------------------------------

/// AC: `cargo build -p netfyr-state` succeeds on its own.
#[test]
fn test_individual_crate_netfyr_state_compiles() {
    let root = workspace_root();
    let status = Command::new("cargo")
        .args(["build", "-p", "netfyr-state"])
        .current_dir(&root)
        .status()
        .expect("failed to invoke cargo");
    assert!(status.success(), "cargo build -p netfyr-state failed");
}

// ---------------------------------------------------------------------------
// Scenario: Workspace members are correctly listed
// ---------------------------------------------------------------------------

/// AC: The workspace members list contains exactly 8 entries with the correct names.
#[test]
fn test_workspace_members_listed_exactly() {
    let root = workspace_root();
    let content = fs::read_to_string(root.join("Cargo.toml"))
        .expect("failed to read root Cargo.toml");

    let expected: &[&str] = &[
        "crates/netfyr-state",
        "crates/netfyr-reconcile",
        "crates/netfyr-backend",
        "crates/netfyr-policy",
        "crates/netfyr-varlink",
        "crates/netfyr-cli",
        "crates/netfyr-daemon",
        "crates/netfyr-test-utils",
    ];

    for member in expected {
        assert!(
            content.contains(member),
            "root Cargo.toml is missing workspace member: {member}"
        );
    }

    // Count member-like lines to verify there are exactly 8 (no extras).
    let member_line_count = content
        .lines()
        .filter(|l| {
            let t = l.trim();
            t.starts_with('"') && t.contains("crates/") && t.ends_with("\",")
                || t.starts_with('"') && t.contains("crates/") && t.ends_with('"')
        })
        .count();

    assert_eq!(
        member_line_count, 9,
        "expected exactly 9 workspace member entries, found {member_line_count}"
    );
}

// ---------------------------------------------------------------------------
// Scenario: CLI crate produces a binary
// ---------------------------------------------------------------------------

/// AC: `cargo build -p netfyr-cli` produces a binary in target/debug/.
#[test]
fn test_cli_binary_is_produced() {
    let root = workspace_root();

    let build_status = Command::new("cargo")
        .args(["build", "-p", "netfyr-cli"])
        .current_dir(&root)
        .status()
        .expect("failed to invoke cargo");
    assert!(build_status.success(), "cargo build -p netfyr-cli failed");

    let binary = root.join("target/debug/netfyr-cli");
    assert!(
        binary.exists(),
        "binary netfyr-cli was not produced at {binary:?}"
    );
}

/// AC: Running the netfyr-cli binary prints "netfyr" to stdout.
#[test]
fn test_cli_binary_prints_netfyr() {
    let root = workspace_root();

    // Ensure the binary is built before running it.
    let build_status = Command::new("cargo")
        .args(["build", "-p", "netfyr-cli"])
        .current_dir(&root)
        .status()
        .expect("failed to invoke cargo");
    assert!(build_status.success(), "cargo build -p netfyr-cli failed");

    let output = Command::new(root.join("target/debug/netfyr-cli"))
        .output()
        .expect("failed to run netfyr-cli binary");

    assert!(
        output.status.success(),
        "netfyr-cli exited with non-zero status: {}",
        output.status
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        "netfyr",
        "netfyr-cli stdout was {:?}, expected \"netfyr\"",
        stdout
    );
}

// ---------------------------------------------------------------------------
// Scenario: Daemon crate produces a binary
// ---------------------------------------------------------------------------

/// AC: `cargo build -p netfyr-daemon` produces a binary in target/debug/.
#[test]
fn test_daemon_binary_is_produced() {
    let root = workspace_root();

    let build_status = Command::new("cargo")
        .args(["build", "-p", "netfyr-daemon"])
        .current_dir(&root)
        .status()
        .expect("failed to invoke cargo");
    assert!(build_status.success(), "cargo build -p netfyr-daemon failed");

    let binary = root.join("target/debug/netfyr-daemon");
    assert!(
        binary.exists(),
        "binary netfyr-daemon was not produced at {binary:?}"
    );
}

/// AC: Running the netfyr-daemon binary prints "netfyr" to stdout.
#[test]
fn test_daemon_binary_prints_netfyr() {
    let root = workspace_root();

    let build_status = Command::new("cargo")
        .args(["build", "-p", "netfyr-daemon"])
        .current_dir(&root)
        .status()
        .expect("failed to invoke cargo");
    assert!(build_status.success(), "cargo build -p netfyr-daemon failed");

    let output = Command::new(root.join("target/debug/netfyr-daemon"))
        .output()
        .expect("failed to run netfyr-daemon binary");

    assert!(
        output.status.success(),
        "netfyr-daemon exited with non-zero status: {}",
        output.status
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        "netfyr",
        "netfyr-daemon stdout was {:?}, expected \"netfyr\"",
        stdout
    );
}

// ---------------------------------------------------------------------------
// Scenario: Library crates have correct structure
// ---------------------------------------------------------------------------

/// AC: Each library crate contains Cargo.toml and src/lib.rs.
#[test]
fn test_library_crates_have_correct_structure() {
    let root = workspace_root();
    let library_crates = [
        "netfyr-state",
        "netfyr-reconcile",
        "netfyr-backend",
        "netfyr-policy",
        "netfyr-varlink",
        "netfyr-test-utils",
    ];

    for name in &library_crates {
        let dir = root.join("crates").join(name);
        assert!(
            dir.join("Cargo.toml").exists(),
            "{name}: Cargo.toml not found"
        );
        assert!(
            dir.join("src/lib.rs").exists(),
            "{name}: src/lib.rs not found"
        );
    }
}

/// AC: netfyr-cli and netfyr-daemon each have Cargo.toml and src/main.rs.
#[test]
fn test_binary_crates_have_correct_structure() {
    let root = workspace_root();
    let binary_crates = ["netfyr-cli", "netfyr-daemon"];

    for name in &binary_crates {
        let dir = root.join("crates").join(name);
        assert!(
            dir.join("Cargo.toml").exists(),
            "{name}: Cargo.toml not found"
        );
        assert!(
            dir.join("src/main.rs").exists(),
            "{name}: src/main.rs not found"
        );
    }

    // netfyr-daemon must NOT have src/lib.rs (it is a pure binary).
    // netfyr-cli intentionally has src/lib.rs: SPEC-501 requires the xtask crate
    // to depend on it as a library (for `Cli::command()` man page generation).
    let daemon_dir = root.join("crates").join("netfyr-daemon");
    assert!(
        !daemon_dir.join("src/lib.rs").exists(),
        "netfyr-daemon: unexpectedly contains src/lib.rs"
    );
}

/// AC: No crate has extraneous files (src/ contains only the expected source files).
#[test]
fn test_no_extraneous_source_files_in_library_crates() {
    let root = workspace_root();

    // Each entry is (crate_name, sorted list of expected source files).
    // netfyr-state has set.rs and diff.rs in addition to lib.rs per SPEC-004.
    // SPEC-005 adds loader.rs and yaml.rs for YAML serialization support.
    // SPEC-006 adds schema.rs and schemas/ for entity schema validation.
    let library_crates: &[(&str, &[&str])] = &[
        ("netfyr-state", &["diff.rs", "lib.rs", "loader.rs", "schema.rs", "schemas", "set.rs", "yaml.rs"]),
        // SPEC-203 adds diff.rs and report.rs for diff generation.
        ("netfyr-reconcile", &["diff.rs", "lib.rs", "report.rs"]),
        // SPEC-401 adds dhcp/ for the DHCPv4 factory implementation.
        ("netfyr-backend", &["dhcp", "lib.rs", "netlink", "registry.rs", "report.rs", "trait_.rs"]),
        ("netfyr-policy", &["lib.rs"]),
        // SPEC-404 adds client.rs, io.netfyr.varlink, and types.rs for the Varlink IPC API.
        ("netfyr-varlink", &["client.rs", "io.netfyr.varlink", "lib.rs", "types.rs"]),
        // SPEC-401 adds dnsmasq.rs for DHCPv4 integration test infrastructure.
        ("netfyr-test-utils", &["dnsmasq.rs", "lib.rs", "netns.rs"]),
    ];

    for (name, expected) in library_crates {
        let src_dir = root.join("crates").join(name).join("src");
        let mut entries: Vec<String> = fs::read_dir(&src_dir)
            .unwrap_or_else(|e| panic!("{name}: cannot read src/: {e}"))
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        let expected_strings: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            entries,
            expected_strings,
            "{name}: src/ should contain exactly {expected:?}, found: {entries:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario: Test utils crate compiles
// ---------------------------------------------------------------------------

/// AC: `cargo build -p netfyr-test-utils` succeeds.
#[test]
fn test_test_utils_crate_compiles() {
    let root = workspace_root();
    let status = Command::new("cargo")
        .args(["build", "-p", "netfyr-test-utils"])
        .current_dir(&root)
        .status()
        .expect("failed to invoke cargo");
    assert!(status.success(), "cargo build -p netfyr-test-utils failed");
}

// ---------------------------------------------------------------------------
// Supplementary: Crate Cargo.toml metadata
// ---------------------------------------------------------------------------

/// AC: Each crate's Cargo.toml specifies edition 2021 and version 0.1.0.
#[test]
fn test_each_crate_cargo_toml_has_correct_metadata() {
    let root = workspace_root();
    let all_crates = [
        "netfyr-state",
        "netfyr-reconcile",
        "netfyr-backend",
        "netfyr-policy",
        "netfyr-varlink",
        "netfyr-cli",
        "netfyr-daemon",
        "netfyr-test-utils",
    ];

    for name in &all_crates {
        let toml_path = root.join("crates").join(name).join("Cargo.toml");
        let content = fs::read_to_string(&toml_path)
            .unwrap_or_else(|e| panic!("{name}: cannot read Cargo.toml: {e}"));

        assert!(
            content.contains("edition = \"2021\""),
            "{name}: Cargo.toml does not specify edition = \"2021\""
        );
        assert!(
            content.contains("version = \"0.1.0\""),
            "{name}: Cargo.toml does not specify version = \"0.1.0\""
        );
        assert!(
            content.contains(&format!("name = \"{name}\"")),
            "{name}: Cargo.toml does not specify the correct package name"
        );
    }
}

/// AC: The workspace resolver is set to "2".
#[test]
fn test_workspace_uses_resolver_2() {
    let root = workspace_root();
    let content = fs::read_to_string(root.join("Cargo.toml"))
        .expect("failed to read root Cargo.toml");
    assert!(
        content.contains("resolver = \"2\""),
        "root Cargo.toml does not set resolver = \"2\""
    );
}
