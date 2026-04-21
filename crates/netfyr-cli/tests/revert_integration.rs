//! Integration tests for the `netfyr revert` CLI command (story 354-state-revert).
//!
//! Two groups:
//!
//! 1. **Error-case tests** — spawn the binary, check exit codes and output.
//!    These do not require network access and run on any host.
//!
//! 2. **Journal-API tests** — use netfyr-journal directly to verify the
//!    Trigger::Revert metadata structure without requiring network operations.

use std::path::PathBuf;

use netfyr_journal::{ApplyOutcome, Journal, JournalEntry, Trigger};
use netfyr_journal::serializable::{
    SerializableDiff, SerializableState, SerializableStateSet,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn netfyr_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_netfyr"))
}

fn combined(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn make_entry_with_state(
    mtu: u64,
    entity_name: &str,
) -> JournalEntry {
    JournalEntry {
        seq: 0,
        timestamp: chrono::Utc::now(),
        trigger: Trigger::PolicyApply { source: "test".to_string() },
        active_policies: vec![],
        diff: SerializableDiff { operations: vec![] },
        state_after: SerializableStateSet {
            entities: vec![SerializableState {
                entity_type: "ethernet".to_string(),
                selector_name: entity_name.to_string(),
                fields: serde_json::json!({ "mtu": mtu }),
            }],
        },
        outcome: ApplyOutcome::Applied { succeeded: 1, failed: 0, skipped: 0 },
    }
}

// ── Feature: Error cases (no system access required) ─────────────────────────

/// AC: Missing target argument shows a clap usage error, exit code 2.
#[test]
fn test_revert_no_args_shows_clap_error_exit_code_2() {
    let output = std::process::Command::new(netfyr_bin())
        .arg("revert")
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run netfyr");

    assert_eq!(
        output.status.code(),
        Some(2),
        "missing target must produce clap exit code 2; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("required")
            || stderr.contains("Usage")
            || stderr.contains("USAGE")
            || stderr.contains("target"),
        "clap must mention missing argument; stderr={stderr}"
    );
}

/// AC: Non-numeric target argument produces a clap error, exit code 2.
#[test]
fn test_revert_non_numeric_target_shows_clap_error_exit_code_2() {
    let output = std::process::Command::new(netfyr_bin())
        .args(["revert", "not-a-number"])
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run netfyr");

    assert_eq!(
        output.status.code(),
        Some(2),
        "non-numeric target must produce clap exit code 2; got: {}",
        combined(&output),
    );
}

/// AC: "Entry #9999 not found" appears in the output when the entry does not exist.
///
/// We use NETFYR_JOURNAL_DIR (empty temp dir) and NETFYR_SOCKET_PATH (nonexistent)
/// to force standalone mode with no prior journal entries.
#[test]
fn test_revert_nonexistent_entry_shows_entry_not_found_in_output() {
    let dir = tempfile::tempdir().unwrap();
    // Pre-create the archive subdir so Journal::open doesn't fail.
    std::fs::create_dir_all(dir.path().join("archive")).unwrap();

    let output = std::process::Command::new(netfyr_bin())
        .args(["revert", "9999"])
        .env("NO_COLOR", "1")
        .env("NETFYR_JOURNAL_DIR", dir.path())
        .env("NETFYR_SOCKET_PATH", "/tmp/netfyr-nonexistent-test.sock")
        .output()
        .expect("failed to run netfyr");

    let text = combined(&output);
    assert!(
        text.contains("9999") && (text.contains("not found") || text.contains("Entry")),
        "output must mention entry #9999 not found; got: {text}"
    );
}

/// AC (spec): `netfyr revert 9999` for a nonexistent entry exits with code 1.
///
/// NOTE: The current implementation exits with code 2 in standalone mode (the
/// anyhow error bubbles to main() which always uses ExitCode::from(2u8)).
/// Daemon mode correctly returns exit code 1 via VarlinkError::EntryNotFound.
/// This test captures the spec requirement; a failing result indicates a bug.
#[test]
fn test_revert_nonexistent_entry_exit_code_is_1() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("archive")).unwrap();

    let output = std::process::Command::new(netfyr_bin())
        .args(["revert", "9999"])
        .env("NO_COLOR", "1")
        .env("NETFYR_JOURNAL_DIR", dir.path())
        .env("NETFYR_SOCKET_PATH", "/tmp/netfyr-nonexistent-test.sock")
        .output()
        .expect("failed to run netfyr");

    // BUG: standalone mode exits with 2 instead of 1 for "entry not found".
    // The spec says exit code 1. Daemon mode correctly returns 1 via
    // VarlinkError::EntryNotFound. This test documents the spec expectation.
    assert_eq!(
        output.status.code(),
        Some(1),
        "exit code must be 1 for missing entry (spec requirement); got: {}",
        output.status.code().unwrap_or(-1),
    );
}

// ── Feature: Revert journal entry metadata (no network required) ──────────────

/// AC: Revert entry has trigger "revert" with correct target_seq and state_after.
///
/// This test verifies the structure of a manually-constructed revert journal entry,
/// covering the "Revert entry contains correct metadata" acceptance criterion
/// without requiring network operations.
#[test]
fn test_revert_journal_entry_has_correct_trigger_and_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = Journal::open(dir.path()).expect("journal must open");

    // Write a baseline entry (seq=1 after append).
    let baseline = make_entry_with_state(1500, "eth0");
    journal.append(baseline).unwrap();
    let baseline_entry = journal.read_entry(1).unwrap().expect("entry 1 must exist");

    // Construct the revert journal entry as run_revert_standalone would.
    let revert_entry = JournalEntry {
        seq: 0,
        timestamp: chrono::Utc::now(),
        trigger: Trigger::Revert { target_seq: baseline_entry.seq },
        active_policies: vec![],
        diff: SerializableDiff { operations: vec![] },
        state_after: baseline_entry.state_after.clone(),
        outcome: ApplyOutcome::Applied { succeeded: 1, failed: 0, skipped: 0 },
    };
    journal.append(revert_entry).unwrap();

    // Read back the most recent entry (seq=2).
    let entries = journal.read_recent(1).unwrap();
    let latest = &entries[0];

    // AC: trigger is "revert" with target_seq pointing to the baseline entry.
    match &latest.trigger {
        Trigger::Revert { target_seq } => {
            assert_eq!(
                *target_seq,
                baseline_entry.seq,
                "revert entry must reference the baseline entry seq"
            );
        }
        other => panic!("expected Trigger::Revert, got {:?}", other),
    }

    // AC: state_after matches the target entry's state_after.
    assert_eq!(
        latest.state_after.entities.len(),
        baseline_entry.state_after.entities.len(),
        "revert entry state_after must have same number of entities as baseline"
    );
    let baseline_entity = &baseline_entry.state_after.entities[0];
    let revert_entity = &latest.state_after.entities[0];
    assert_eq!(
        revert_entity.selector_name, baseline_entity.selector_name,
        "revert entry state_after entity name must match baseline"
    );
    assert_eq!(
        revert_entity.fields["mtu"],
        baseline_entity.fields["mtu"],
        "revert entry state_after mtu must match baseline"
    );

    // AC: outcome reflects the apply result.
    assert!(
        matches!(
            latest.outcome,
            ApplyOutcome::Applied { succeeded: 1, .. }
        ),
        "revert entry outcome must record succeeded=1"
    );
}

/// AC: Revert trigger serializes to JSON with type="revert" and target_seq.
#[test]
fn test_revert_trigger_serializes_with_type_revert_and_target_seq() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = Journal::open(dir.path()).expect("journal must open");

    let revert_entry = JournalEntry {
        seq: 0,
        timestamp: chrono::Utc::now(),
        trigger: Trigger::Revert { target_seq: 5 },
        active_policies: vec![],
        diff: SerializableDiff { operations: vec![] },
        state_after: SerializableStateSet { entities: vec![] },
        outcome: ApplyOutcome::Applied { succeeded: 0, failed: 0, skipped: 0 },
    };
    journal.append(revert_entry).unwrap();

    let entries = journal.read_recent(1).unwrap();
    let latest = &entries[0];

    match &latest.trigger {
        Trigger::Revert { target_seq } => {
            assert_eq!(*target_seq, 5, "target_seq must be 5");
        }
        other => panic!("expected Trigger::Revert, got {:?}", other),
    }
}

/// AC: Multiple journal entries — read_recent(1) shows the revert entry, not the apply entry.
#[test]
fn test_revert_entry_appears_as_most_recent_after_apply_entry() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = Journal::open(dir.path()).expect("journal must open");

    // Write apply entry (seq=1).
    journal.append(make_entry_with_state(1400, "eth0")).unwrap();

    // Write revert entry (seq=2).
    let revert_entry = JournalEntry {
        seq: 0,
        timestamp: chrono::Utc::now(),
        trigger: Trigger::Revert { target_seq: 1 },
        active_policies: vec![],
        diff: SerializableDiff { operations: vec![] },
        state_after: SerializableStateSet {
            entities: vec![SerializableState {
                entity_type: "ethernet".to_string(),
                selector_name: "eth0".to_string(),
                fields: serde_json::json!({ "mtu": 1400u64 }),
            }],
        },
        outcome: ApplyOutcome::Applied { succeeded: 1, failed: 0, skipped: 0 },
    };
    journal.append(revert_entry).unwrap();

    // read_recent(1) should return the revert entry.
    let recent = journal.read_recent(1).unwrap();
    assert_eq!(recent.len(), 1, "read_recent(1) must return exactly 1 entry");
    assert!(
        matches!(recent[0].trigger, Trigger::Revert { target_seq: 1 }),
        "most recent entry must be the revert entry; got {:?}",
        recent[0].trigger
    );
}
