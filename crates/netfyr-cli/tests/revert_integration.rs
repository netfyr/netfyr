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

// ── Feature: Revert integration tests (unprivileged netns) ───────────────────
//
// Each test:
//   1. Enters a new user+network namespace (skipped if unavailable).
//   2. Creates a veth pair inside the namespace.
//   3. Runs `netfyr apply` as a subprocess (inherits namespace, uses temp journal).
//   4. Runs `netfyr revert` as a subprocess.
//   5. Verifies system state via sysfs / `ip addr show`.
//
// All subprocesses point to a non-existent NETFYR_SOCKET_PATH to force
// standalone (daemon-free) mode, and to a temp NETFYR_JOURNAL_DIR so
// journal entries written by apply are visible to the subsequent revert.

#[cfg(test)]
mod netns_tests {
    use super::*;
    use netfyr_test_utils::netns::{create_veth_pair, set_link_up};
    use netfyr_test_utils::NetnsGuard;
    use std::fs;

    // Nonexistent socket → forces standalone (daemon-free) mode in subprocesses.
    const NO_DAEMON_SOCK: &str = "/tmp/netfyr-revert-inttest-no-daemon.sock";

    fn read_mtu(iface: &str) -> u32 {
        let path = format!("/sys/class/net/{iface}/mtu");
        fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {path}: {e}"))
            .trim()
            .parse()
            .expect("mtu should be numeric")
    }

    fn has_address(iface: &str, addr_fragment: &str) -> bool {
        let out = std::process::Command::new("ip")
            .args(["addr", "show", iface])
            .output()
            .expect("failed to run ip");
        String::from_utf8_lossy(&out.stdout).contains(addr_fragment)
    }

    fn enter_namespace() -> Option<NetnsGuard> {
        match NetnsGuard::new() {
            Ok(g) => Some(g),
            Err(e) => {
                eprintln!("Skipping netns test: {e}");
                None
            }
        }
    }

    /// Run `netfyr apply` with a bare-state YAML string, storing the journal in `journal_dir`.
    async fn apply_yaml(yaml: &str, journal_dir: &std::path::Path) -> std::process::Output {
        let policy_dir = tempfile::tempdir().unwrap();
        let policy_file = policy_dir.path().join("policy.yaml");
        fs::write(&policy_file, yaml).unwrap();
        tokio::process::Command::new(netfyr_bin())
            .args(["apply", policy_file.to_str().unwrap()])
            .env("NO_COLOR", "1")
            .env("NETFYR_JOURNAL_DIR", journal_dir)
            .env("NETFYR_SOCKET_PATH", NO_DAEMON_SOCK)
            .output()
            .await
            .expect("failed to run netfyr apply")
    }

    /// Run `netfyr revert <seq>` (with optional `--dry-run`), using `journal_dir`.
    async fn revert_seq(
        seq: u64,
        dry_run: bool,
        journal_dir: &std::path::Path,
    ) -> std::process::Output {
        let mut cmd = tokio::process::Command::new(netfyr_bin());
        cmd.arg("revert")
            .arg(seq.to_string())
            .env("NO_COLOR", "1")
            .env("NETFYR_JOURNAL_DIR", journal_dir)
            .env("NETFYR_SOCKET_PATH", NO_DAEMON_SOCK);
        if dry_run {
            cmd.arg("--dry-run");
        }
        cmd.output().await.expect("failed to run netfyr revert")
    }

    /// AC: Revert to a previous state — veth MTU is restored from 1300 back to 1400.
    ///
    /// Scenario:
    ///   apply mtu=1400 → seq=1
    ///   apply mtu=1300 → seq=2
    ///   revert 1       → mtu must return to 1400
    #[tokio::test(flavor = "current_thread")]
    async fn test_revert_changes_mtu_to_target_state() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };
        create_veth_pair("veth-rv0", "veth-rv1").await.expect("create_veth_pair failed");
        set_link_up("veth-rv0").await.expect("set_link_up failed");

        let journal_dir = tempfile::tempdir().unwrap();

        // Apply mtu=1400 → journal entry seq=1.
        let out1 = apply_yaml(
            "type: ethernet\nname: veth-rv0\nmtu: 1400\n",
            journal_dir.path(),
        )
        .await;
        assert_eq!(
            out1.status.code(),
            Some(0),
            "first apply must exit 0; got: {}",
            combined(&out1)
        );

        // Apply mtu=1300 → journal entry seq=2.
        let out2 = apply_yaml(
            "type: ethernet\nname: veth-rv0\nmtu: 1300\n",
            journal_dir.path(),
        )
        .await;
        assert_eq!(
            out2.status.code(),
            Some(0),
            "second apply must exit 0; got: {}",
            combined(&out2)
        );

        assert_eq!(read_mtu("veth-rv0"), 1300, "precondition: mtu must be 1300 after second apply");

        // Revert to seq=1 (target mtu=1400).
        let out = revert_seq(1, false, journal_dir.path()).await;
        let text = combined(&out);

        // AC: exit code 0, output mentions "Applied", MTU is restored.
        assert_eq!(out.status.code(), Some(0), "revert must exit 0; got: {text}");
        assert!(
            text.contains("Applied"),
            "revert output must contain 'Applied'; got: {text}"
        );
        assert_eq!(read_mtu("veth-rv0"), 1400, "mtu must be 1400 after revert to seq=1");
    }

    /// AC: Revert dry-run previews changes — output shows current → target MTU, MTU unchanged.
    ///
    /// Scenario:
    ///   apply mtu=1400 → seq=1
    ///   apply mtu=1300 → seq=2
    ///   revert 1 --dry-run → output shows "1300" and "1400", mtu stays 1300
    #[tokio::test(flavor = "current_thread")]
    async fn test_revert_dry_run_shows_mtu_change_without_applying() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };
        create_veth_pair("veth-rvd0", "veth-rvd1").await.expect("create_veth_pair failed");
        set_link_up("veth-rvd0").await.expect("set_link_up failed");

        let journal_dir = tempfile::tempdir().unwrap();

        // Apply mtu=1400 → seq=1.
        let out1 = apply_yaml(
            "type: ethernet\nname: veth-rvd0\nmtu: 1400\n",
            journal_dir.path(),
        )
        .await;
        assert_eq!(
            out1.status.code(),
            Some(0),
            "first apply must exit 0; got: {}",
            combined(&out1)
        );

        // Apply mtu=1300 → seq=2.
        let out2 = apply_yaml(
            "type: ethernet\nname: veth-rvd0\nmtu: 1300\n",
            journal_dir.path(),
        )
        .await;
        assert_eq!(
            out2.status.code(),
            Some(0),
            "second apply must exit 0; got: {}",
            combined(&out2)
        );

        assert_eq!(read_mtu("veth-rvd0"), 1300, "precondition: mtu must be 1300");

        // Dry-run revert to seq=1 (target mtu=1400, current mtu=1300).
        let out = revert_seq(1, true, journal_dir.path()).await;
        let text = combined(&out);

        // AC: output shows both the current value (1300) and the target value (1400).
        assert!(
            text.contains("1300") && text.contains("1400"),
            "dry-run output must show current (1300) and target (1400) values; got: {text}"
        );
        assert!(
            text.contains("mtu"),
            "dry-run output must mention 'mtu'; got: {text}"
        );

        // AC: MTU must not have changed (dry-run only previews).
        assert_eq!(read_mtu("veth-rvd0"), 1300, "dry-run must not change the mtu");
    }

    /// AC: Revert when already at target state — "No changes needed" message, exit 0.
    ///
    /// Scenario:
    ///   apply mtu=1400 → seq=1 (system is now at mtu=1400)
    ///   revert 1       → no-op (already at target)
    #[tokio::test(flavor = "current_thread")]
    async fn test_revert_no_changes_needed_when_already_at_target_state() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };
        create_veth_pair("veth-rvnc0", "veth-rvnc1").await.expect("create_veth_pair failed");
        set_link_up("veth-rvnc0").await.expect("set_link_up failed");

        let journal_dir = tempfile::tempdir().unwrap();

        // Apply mtu=1400 → seq=1. The system is now at mtu=1400.
        let out1 = apply_yaml(
            "type: ethernet\nname: veth-rvnc0\nmtu: 1400\n",
            journal_dir.path(),
        )
        .await;
        assert_eq!(
            out1.status.code(),
            Some(0),
            "apply must exit 0; got: {}",
            combined(&out1)
        );
        assert_eq!(read_mtu("veth-rvnc0"), 1400, "precondition: mtu must be 1400");

        // Revert to seq=1 — system already matches the target.
        let out = revert_seq(1, false, journal_dir.path()).await;
        let text = combined(&out);

        // AC: exit code 0 and "No changes needed" in output.
        assert_eq!(
            out.status.code(),
            Some(0),
            "revert with no changes must exit 0; got: {text}"
        );
        assert!(
            text.contains("No changes needed"),
            "output must say 'No changes needed'; got: {text}"
        );
    }

    /// AC: Revert records journal entry with trigger "revert" and target_seq.
    ///
    /// After a successful revert, the most recent journal entry must have
    /// Trigger::Revert { target_seq } pointing at the entry that was reverted to.
    #[tokio::test(flavor = "current_thread")]
    async fn test_revert_records_journal_entry_with_revert_trigger_and_correct_state_after() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };
        create_veth_pair("veth-rvj0", "veth-rvj1").await.expect("create_veth_pair failed");
        set_link_up("veth-rvj0").await.expect("set_link_up failed");

        let journal_dir = tempfile::tempdir().unwrap();

        // Apply mtu=1400 → seq=1.
        let out1 = apply_yaml(
            "type: ethernet\nname: veth-rvj0\nmtu: 1400\n",
            journal_dir.path(),
        )
        .await;
        assert_eq!(out1.status.code(), Some(0), "first apply must exit 0");

        // Apply mtu=1300 → seq=2.
        let out2 = apply_yaml(
            "type: ethernet\nname: veth-rvj0\nmtu: 1300\n",
            journal_dir.path(),
        )
        .await;
        assert_eq!(out2.status.code(), Some(0), "second apply must exit 0");

        // Revert to seq=1 → writes revert journal entry (seq=3).
        let out = revert_seq(1, false, journal_dir.path()).await;
        let text = combined(&out);
        assert_eq!(out.status.code(), Some(0), "revert must exit 0; got: {text}");

        // AC: most recent journal entry has Trigger::Revert { target_seq: 1 }.
        let journal = Journal::open(journal_dir.path()).expect("journal must open");
        let entries = journal.read_recent(1).expect("read_recent must succeed");
        assert!(!entries.is_empty(), "journal must have at least 1 entry after revert");
        let latest = &entries[0];

        match &latest.trigger {
            Trigger::Revert { target_seq } => {
                assert_eq!(*target_seq, 1, "revert entry must reference target_seq=1");
            }
            other => panic!(
                "most recent journal entry must be Trigger::Revert, got {:?}",
                other
            ),
        }

        // AC: outcome reflects the apply result (at least 1 succeeded).
        assert!(
            matches!(latest.outcome, ApplyOutcome::Applied { succeeded, .. } if succeeded >= 1),
            "revert journal entry outcome must record at least 1 succeeded; got {:?}",
            latest.outcome
        );

        // AC: state_after matches the target entry's state_after (mtu=1400).
        assert!(
            !latest.state_after.entities.is_empty(),
            "revert journal entry state_after must not be empty"
        );
        let entity = latest
            .state_after
            .entities
            .iter()
            .find(|e| e.selector_name == "veth-rvj0")
            .expect("state_after must contain veth-rvj0");
        assert_eq!(
            entity.fields["mtu"],
            serde_json::json!(1400u64),
            "state_after mtu must be 1400 (the target state from seq=1)"
        );
    }

    /// AC: Revert with address changes — original addresses are restored, new address removed.
    ///
    /// Scenario:
    ///   apply addresses=[10.99.0.1/24, 10.99.0.2/24] → seq=1
    ///   apply addresses=[10.99.0.3/24]               → seq=2
    ///   revert 1 → 10.99.0.1 and 10.99.0.2 restored, 10.99.0.3 removed
    ///
    /// NOTE: Interface addresses use host-bit notation (e.g. "10.99.0.1/24"),
    /// which are stored as Value::String in the journal (Ipv4Network rejects host
    /// bits). The round-trip through the journal serializer preserves the string
    /// representation, so the revert should correctly restore the original addresses.
    #[tokio::test(flavor = "current_thread")]
    async fn test_revert_restores_addresses_from_journal_snapshot() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };
        create_veth_pair("veth-rva0", "veth-rva1").await.expect("create_veth_pair failed");
        set_link_up("veth-rva0").await.expect("set_link_up failed");

        let journal_dir = tempfile::tempdir().unwrap();

        // Apply with two addresses → seq=1.
        let yaml_a = "type: ethernet\nname: veth-rva0\nmtu: 1400\naddresses:\n  - 10.99.0.1/24\n  - 10.99.0.2/24\n";
        let out1 = apply_yaml(yaml_a, journal_dir.path()).await;
        assert_eq!(
            out1.status.code(),
            Some(0),
            "first apply must exit 0; got: {}",
            combined(&out1)
        );
        assert!(
            has_address("veth-rva0", "10.99.0.1"),
            "precondition: 10.99.0.1 must be set after first apply"
        );
        assert!(
            has_address("veth-rva0", "10.99.0.2"),
            "precondition: 10.99.0.2 must be set after first apply"
        );

        // Apply with one different address → seq=2.
        let yaml_b =
            "type: ethernet\nname: veth-rva0\nmtu: 1400\naddresses:\n  - 10.99.0.3/24\n";
        let out2 = apply_yaml(yaml_b, journal_dir.path()).await;
        assert_eq!(
            out2.status.code(),
            Some(0),
            "second apply must exit 0; got: {}",
            combined(&out2)
        );
        assert!(
            has_address("veth-rva0", "10.99.0.3"),
            "precondition: 10.99.0.3 must be set after second apply"
        );

        // Revert to seq=1 — should restore original addresses.
        let out = revert_seq(1, false, journal_dir.path()).await;
        let text = combined(&out);
        assert_eq!(out.status.code(), Some(0), "revert must exit 0; got: {text}");

        // AC: 10.99.0.1 and 10.99.0.2 are restored.
        assert!(
            has_address("veth-rva0", "10.99.0.1"),
            "10.99.0.1 must be restored after revert"
        );
        assert!(
            has_address("veth-rva0", "10.99.0.2"),
            "10.99.0.2 must be restored after revert"
        );
        // AC: 10.99.0.3 is removed.
        assert!(
            !has_address("veth-rva0", "10.99.0.3"),
            "10.99.0.3 must be removed after revert"
        );
    }

    /// AC: Dry-run does not record a journal entry.
    ///
    /// After a dry-run revert, the journal must not contain a new revert entry.
    #[tokio::test(flavor = "current_thread")]
    async fn test_revert_dry_run_does_not_record_journal_entry() {
        let _guard = match enter_namespace() {
            Some(g) => g,
            None => return,
        };
        create_veth_pair("veth-rvdj0", "veth-rvdj1").await.expect("create_veth_pair failed");
        set_link_up("veth-rvdj0").await.expect("set_link_up failed");

        let journal_dir = tempfile::tempdir().unwrap();

        // Apply mtu=1400 → seq=1.
        let out1 = apply_yaml(
            "type: ethernet\nname: veth-rvdj0\nmtu: 1400\n",
            journal_dir.path(),
        )
        .await;
        assert_eq!(out1.status.code(), Some(0), "apply must exit 0");

        // Apply mtu=1300 → seq=2.
        let out2 = apply_yaml(
            "type: ethernet\nname: veth-rvdj0\nmtu: 1300\n",
            journal_dir.path(),
        )
        .await;
        assert_eq!(out2.status.code(), Some(0), "second apply must exit 0");

        // Count entries before dry-run revert.
        let journal = Journal::open(journal_dir.path()).expect("journal must open");
        let entries_before = journal.read_recent(100).expect("read_recent must succeed");
        let count_before = entries_before.len();

        // Dry-run revert to seq=1 — must not write a new journal entry.
        let out = revert_seq(1, true, journal_dir.path()).await;
        let text = combined(&out);
        // Dry-run with pending changes → non-zero exit code (1).
        assert!(
            out.status.code() != Some(2),
            "dry-run revert must not exit 2; got: {text}"
        );

        // Re-open the journal and count entries again.
        let journal2 = Journal::open(journal_dir.path()).expect("journal must open after dry-run");
        let entries_after = journal2.read_recent(100).expect("read_recent must succeed");
        let count_after = entries_after.len();

        assert_eq!(
            count_after, count_before,
            "dry-run revert must not add a new journal entry (before={count_before}, after={count_after})"
        );

        // Verify the MTU was not changed by the dry-run.
        assert_eq!(read_mtu("veth-rvdj0"), 1300, "dry-run must not change mtu");
    }
}

// ── Feature: State revert (daemon mode) ──────────────────────────────────────
//
// These tests spawn a mock Varlink server over a Unix socket so that the CLI
// binary enters its daemon-mode path (VarlinkClient::connect succeeds).
// No real daemon or network access is required.

#[cfg(test)]
mod daemon_mode_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    // ── Mock server helpers ───────────────────────────────────────────────────

    /// Read one NUL-terminated message from a synchronous stream and parse it as JSON.
    fn read_varlink_request<R: Read>(stream: &mut R) -> serde_json::Value {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).expect("read byte from mock stream");
            if byte[0] == 0 {
                break;
            }
            buf.push(byte[0]);
        }
        serde_json::from_slice(&buf).expect("request must be valid JSON")
    }

    /// Write a NUL-terminated Varlink success response to a synchronous stream.
    ///
    /// `params_json` is a pre-serialized JSON object string placed under `"parameters"`.
    fn write_varlink_success<W: Write>(stream: &mut W, params_json: &str) {
        let response = format!(r#"{{"parameters":{}}}"#, params_json);
        let mut msg = response.into_bytes();
        msg.push(0);
        stream.write_all(&msg).expect("write mock response");
    }

    /// Write a NUL-terminated Varlink error response to a synchronous stream.
    fn write_varlink_error<W: Write>(stream: &mut W, error_name: &str, reason: &str) {
        let response = serde_json::json!({
            "error": error_name,
            "parameters": { "reason": reason }
        });
        let mut msg = serde_json::to_vec(&response).expect("serialize error response");
        msg.push(0);
        stream.write_all(&msg).expect("write mock error response");
    }

    /// Spawn a background thread that accepts exactly one Varlink connection,
    /// reads one request, sends `params_json` as a success response, and returns
    /// the parsed request value.
    ///
    /// The `UnixListener` is bound **before** spawning so the socket file exists
    /// before the client's `connect()` call.
    fn spawn_success_server(
        socket_path: &str,
        params_json: &'static str,
    ) -> thread::JoinHandle<serde_json::Value> {
        let listener = UnixListener::bind(socket_path).expect("bind mock socket");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection");
            let req = read_varlink_request(&mut stream);
            write_varlink_success(&mut stream, params_json);
            req
        })
    }

    /// Spawn a background thread that accepts exactly one Varlink connection,
    /// reads one request, and sends a Varlink error response.
    fn spawn_error_server(
        socket_path: &str,
        error_name: &'static str,
        reason: &'static str,
    ) -> thread::JoinHandle<serde_json::Value> {
        let listener = UnixListener::bind(socket_path).expect("bind mock error socket");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection");
            let req = read_varlink_request(&mut stream);
            write_varlink_error(&mut stream, error_name, reason);
            req
        })
    }

    // ── AC: Revert via daemon routes through io.netfyr.Revert ────────────────

    /// AC: When the daemon socket is reachable, `netfyr revert` sends
    /// `io.netfyr.Revert` with the correct `target_seq` and `dry_run=false`.
    ///
    /// This exercises the daemon-mode branch in `run_revert`.
    #[test]
    fn test_revert_daemon_mode_sends_varlink_revert_method_with_correct_target_seq() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("revert-method.sock")
            .to_string_lossy()
            .into_owned();

        // Mock server: accept one connection, return a successful revert report.
        let server = spawn_success_server(
            &socket_path,
            r#"{"report":{"succeeded":1,"failed":0,"skipped":0,"changes":[],"conflicts":[]},"entry_timestamp":"2026-04-20T15:00:00Z"}"#,
        );

        let output = std::process::Command::new(netfyr_bin())
            .args(["revert", "42"])
            .env("NO_COLOR", "1")
            .env("NETFYR_SOCKET_PATH", &socket_path)
            .output()
            .expect("failed to run netfyr revert in daemon mode");

        let req = server.join().expect("mock server thread must finish");

        // AC: The CLI must have sent io.netfyr.Revert.
        assert_eq!(
            req["method"].as_str(),
            Some("io.netfyr.Revert"),
            "daemon mode must route through io.netfyr.Revert; got method={:?}",
            req["method"]
        );
        assert_eq!(
            req["parameters"]["target_seq"].as_u64(),
            Some(42),
            "target_seq must be 42"
        );
        assert_eq!(
            req["parameters"]["dry_run"].as_bool(),
            Some(false),
            "dry_run must be false for a non-dry-run call"
        );
        let _ = output;
    }

    // ── AC: Policy drift warning is printed to stderr after daemon revert ─────

    /// AC: After a successful daemon-mode revert, the CLI prints a warning to
    /// stderr explaining that the active policy set was not changed.
    #[test]
    fn test_revert_daemon_mode_prints_policy_drift_warning_to_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("revert-warning.sock")
            .to_string_lossy()
            .into_owned();

        let server = spawn_success_server(
            &socket_path,
            r#"{"report":{"succeeded":1,"failed":0,"skipped":0,"changes":[],"conflicts":[]},"entry_timestamp":"2026-04-20T15:00:00Z"}"#,
        );

        let output = std::process::Command::new(netfyr_bin())
            .args(["revert", "1"])
            .env("NO_COLOR", "1")
            .env("NETFYR_SOCKET_PATH", &socket_path)
            .output()
            .expect("failed to run netfyr revert in daemon mode");

        let _req = server.join().expect("mock server thread must finish");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Warning") || stderr.contains("warning"),
            "stderr must contain a policy drift warning; got: {stderr}"
        );
        assert!(
            stderr.contains("policy") || stderr.contains("reconciliation"),
            "warning must mention policy drift or reconciliation; got: {stderr}"
        );
    }

    // ── AC: No policy drift warning in dry-run daemon mode ────────────────────

    /// AC: `netfyr revert --dry-run` in daemon mode does NOT print the policy
    /// drift warning because no changes are applied.
    #[test]
    fn test_revert_dry_run_daemon_mode_does_not_print_policy_drift_warning() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("revert-dryrun-warn.sock")
            .to_string_lossy()
            .into_owned();

        let server = spawn_success_server(
            &socket_path,
            r#"{"report":{"succeeded":0,"failed":0,"skipped":0,"changes":[{"kind":"modify","entity_type":"ethernet","entity_name":"veth0","description":"mtu: 1300 -> 1400","status":"planned"}],"conflicts":[]},"entry_timestamp":"2026-04-20T15:00:00Z"}"#,
        );

        let output = std::process::Command::new(netfyr_bin())
            .args(["revert", "1", "--dry-run"])
            .env("NO_COLOR", "1")
            .env("NETFYR_SOCKET_PATH", &socket_path)
            .output()
            .expect("failed to run netfyr revert --dry-run in daemon mode");

        let _req = server.join().expect("mock server thread must finish");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("Warning: the active policy set was not changed"),
            "dry-run must NOT print the policy drift warning; got stderr: {stderr}"
        );
    }

    // ── AC: EntryNotFound in daemon mode exits with code 1 ───────────────────

    /// AC: When the daemon returns `io.netfyr.EntryNotFound`, `netfyr revert`
    /// exits with code 1 and prints an error message identifying the missing entry.
    #[test]
    fn test_revert_daemon_mode_entry_not_found_exits_with_code_1() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("revert-notfound.sock")
            .to_string_lossy()
            .into_owned();

        let server = spawn_error_server(
            &socket_path,
            "io.netfyr.EntryNotFound",
            "Entry #9999 not found",
        );

        let output = std::process::Command::new(netfyr_bin())
            .args(["revert", "9999"])
            .env("NO_COLOR", "1")
            .env("NETFYR_SOCKET_PATH", &socket_path)
            .output()
            .expect("failed to run netfyr revert in daemon mode with missing entry");

        let _req = server.join().expect("mock server thread must finish");

        assert_eq!(
            output.status.code(),
            Some(1),
            "EntryNotFound in daemon mode must exit with code 1; got: {:?}",
            output.status.code()
        );

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("9999") || stderr.contains("not found") || stderr.contains("Entry"),
            "error output must identify the missing entry; got stderr: {stderr}"
        );
    }

    // ── AC: Dry-run via daemon sends dry_run=true ─────────────────────────────

    /// AC: `netfyr revert <seq> --dry-run` in daemon mode sends `dry_run=true`
    /// in the Varlink Revert request.
    #[test]
    fn test_revert_dry_run_daemon_mode_sends_dry_run_true_in_request() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("revert-dry-flag.sock")
            .to_string_lossy()
            .into_owned();

        let server = spawn_success_server(
            &socket_path,
            r#"{"report":{"succeeded":0,"failed":0,"skipped":0,"changes":[],"conflicts":[]},"entry_timestamp":"2026-04-20T14:30:00Z"}"#,
        );

        let output = std::process::Command::new(netfyr_bin())
            .args(["revert", "5", "--dry-run"])
            .env("NO_COLOR", "1")
            .env("NETFYR_SOCKET_PATH", &socket_path)
            .output()
            .expect("failed to run netfyr revert --dry-run in daemon mode");

        let req = server.join().expect("mock server thread must finish");

        // AC: dry_run must be true in the forwarded Varlink request.
        assert_eq!(
            req["parameters"]["dry_run"].as_bool(),
            Some(true),
            "dry-run flag must be forwarded as dry_run=true; got: {:?}",
            req["parameters"]["dry_run"]
        );
        assert_eq!(
            req["parameters"]["target_seq"].as_u64(),
            Some(5),
            "target_seq must be 5"
        );

        let _ = output;
    }

    // ── AC: Daemon dry-run does not record a new journal entry ────────────────

    /// AC: In daemon mode, `--dry-run` is forwarded to the daemon which handles
    /// journal recording. The Varlink request must include `dry_run=true` so the
    /// daemon knows not to record a journal entry.
    ///
    /// This complements `test_revert_dry_run_does_not_record_journal_entry`
    /// (which tests the standalone path) by verifying the daemon-mode wire
    /// protocol correctly carries the dry_run flag.
    #[test]
    fn test_revert_dry_run_daemon_mode_forwards_dry_run_flag_to_prevent_journal_write() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("revert-dry-journal.sock")
            .to_string_lossy()
            .into_owned();

        let server = spawn_success_server(
            &socket_path,
            r#"{"report":{"succeeded":0,"failed":0,"skipped":0,"changes":[],"conflicts":[]},"entry_timestamp":"2026-04-20T14:30:00Z"}"#,
        );

        let _ = std::process::Command::new(netfyr_bin())
            .args(["revert", "3", "--dry-run"])
            .env("NO_COLOR", "1")
            .env("NETFYR_SOCKET_PATH", &socket_path)
            .output()
            .expect("failed to run netfyr revert --dry-run");

        let req = server.join().expect("mock server thread must finish");

        assert_eq!(
            req["parameters"]["dry_run"].as_bool(),
            Some(true),
            "dry_run=true must be sent to the daemon so it skips journal recording"
        );
    }
}
