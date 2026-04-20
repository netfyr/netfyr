//! Tests for SPEC-001 acceptance criteria: CLI crate produces a binary.
//!
//! Verifies that:
//! - The `netfyr-cli` binary exists in the target directory after building.
//! - Running the binary with no arguments prints usage help (including "netfyr") to stderr and exits 2.
//!   (SPEC-301: clap uses SubcommandRequiredElseHelp, which exits 2 and writes help to stderr.)

use std::path::PathBuf;
use std::process::Command;

/// Returns the path to the compiled `netfyr-cli` binary.
fn netfyr_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_netfyr"))
}

// ---------------------------------------------------------------------------
// Scenario: CLI crate produces a binary
// ---------------------------------------------------------------------------

/// AC: A binary named "netfyr-cli" is produced in the target directory.
#[test]
fn test_cli_binary_exists_in_target_directory() {
    let bin = netfyr_bin();
    assert!(
        bin.exists(),
        "Expected netfyr-cli binary to exist at {:?}",
        bin
    );
}

/// AC: Running the binary with no arguments prints usage help containing "netfyr" to stderr.
///
/// SPEC-301: clap SubcommandRequiredElseHelp writes help to stderr when no subcommand is given.
#[test]
fn test_cli_binary_prints_netfyr_to_stdout_when_no_args_given() {
    let output = Command::new(netfyr_bin())
        .output()
        .expect("Failed to spawn netfyr-cli binary");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("netfyr"),
        "Expected 'netfyr' in stderr when running netfyr-cli with no arguments, got: {:?}",
        stderr
    );
}

/// AC: The binary exits with code 2 when invoked with no arguments (no subcommand given).
///
/// SPEC-301: SubcommandRequiredElseHelp exits 2 when no subcommand is provided.
#[test]
fn test_cli_binary_exits_zero_with_no_args() {
    let status = Command::new(netfyr_bin())
        .status()
        .expect("Failed to spawn netfyr-cli binary");

    assert!(
        status.code() == Some(2),
        "Expected netfyr-cli to exit with code 2 when called with no arguments, got: {:?}",
        status.code()
    );
}
