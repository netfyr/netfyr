//! Integration tests for the `netfyr history` CLI command (story 352-history-cli).
//!
//! Spawns the binary with NETFYR_JOURNAL_DIR pointed at a temp directory and
//! NETFYR_SOCKET_PATH pointed at a nonexistent socket so the CLI uses direct
//! file access (no daemon required).

use std::path::{Path, PathBuf};
use std::process::Output;

use chrono::{Duration, Utc};
use netfyr_journal::{
    ApplyOutcome, Journal, JournalEntry, PolicySummary, Trigger,
};
use netfyr_journal::serializable::{
    SerializableDiff, SerializableDiffOp, SerializableFieldChange,
    SerializableState, SerializableStateSet,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn netfyr_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_netfyr"))
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn make_entry() -> JournalEntry {
    JournalEntry {
        seq: 0,
        timestamp: Utc::now(),
        trigger: Trigger::PolicyApply { source: "/etc/netfyr/policies/".to_string() },
        active_policies: vec![],
        diff: SerializableDiff { operations: vec![] },
        state_after: SerializableStateSet { entities: vec![] },
        outcome: ApplyOutcome::Applied { succeeded: 1, failed: 0, skipped: 0 },
    }
}

fn make_entry_at(timestamp: chrono::DateTime<Utc>) -> JournalEntry {
    let mut e = make_entry();
    e.timestamp = timestamp;
    e
}

fn make_entry_with_diff(entity_name: &str, field_changes: Vec<SerializableFieldChange>) -> JournalEntry {
    let mut e = make_entry();
    e.diff = SerializableDiff {
        operations: vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: entity_name.to_string(),
            field_changes,
        }],
    };
    e
}

fn make_entry_with_state_after() -> JournalEntry {
    let mut e = make_entry();
    e.state_after = SerializableStateSet {
        entities: vec![SerializableState {
            entity_type: "ethernet".to_string(),
            selector_name: "eth0".to_string(),
            fields: serde_json::json!({
                "mtu": 9000u64,
                "addresses": ["10.0.1.50/24"],
                "carrier": true
            }),
        }],
    };
    e
}

fn setup_journal(entries: Vec<JournalEntry>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = Journal::open(dir.path()).unwrap();
    for entry in entries {
        journal.append(entry).unwrap();
    }
    dir
}

fn run_history(dir: &Path, args: &[&str]) -> Output {
    std::process::Command::new(netfyr_bin())
        .arg("history")
        .args(args)
        .env("NO_COLOR", "1")
        .env("NETFYR_SOCKET_PATH", "/tmp/netfyr-nonexistent-integration-test.sock")
        .env("NETFYR_JOURNAL_DIR", dir)
        .output()
        .expect("failed to run netfyr history")
}

// ── Feature: History list command ─────────────────────────────────────────────

/// AC: List recent journal entries — 30 entries, default count=20 → 21 lines (header + 20 data).
#[test]
fn test_history_default_shows_20_entries_from_30() {
    let dir = setup_journal((0..30).map(|_| make_entry()).collect());
    let output = run_history(dir.path(), &[]);
    assert!(output.status.success(), "history should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    let lines: Vec<&str> = text.lines().collect();
    // header + 20 data rows
    assert_eq!(
        lines.len(),
        21,
        "default count=20 should produce header + 20 data rows, got {} lines:\n{text}",
        lines.len()
    );
}

/// AC: -n 5 returns exactly 5 entries (6 lines: header + 5 data).
#[test]
fn test_history_count_5_shows_5_entries() {
    let dir = setup_journal((0..30).map(|_| make_entry()).collect());
    let output = run_history(dir.path(), &["-n", "5"]);
    assert!(output.status.success(), "history -n 5 should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines.len(),
        6,
        "-n 5 should produce header + 5 data rows, got {} lines:\n{text}",
        lines.len()
    );
}

/// AC: --since 1h filters out entries older than 1 hour.
#[test]
fn test_history_since_filter() {
    let old = make_entry_at(Utc::now() - Duration::hours(2));
    let recent = make_entry_at(Utc::now() - Duration::minutes(5));
    let dir = setup_journal(vec![old, recent]);
    let output = run_history(dir.path(), &["--since", "1h"]);
    assert!(output.status.success(), "since filter should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    let data_lines: Vec<&str> = text.lines().skip(1).collect(); // skip header
    assert_eq!(
        data_lines.len(),
        1,
        "--since 1h should show only 1 recent entry, got {} lines:\n{text}",
        data_lines.len()
    );
}

/// AC: --trigger external filters to only ExternalChange entries.
#[test]
fn test_history_trigger_filter() {
    let mut apply_entry = make_entry();
    apply_entry.trigger = Trigger::PolicyApply { source: "test.yaml".to_string() };

    let mut external_entry = make_entry();
    external_entry.trigger = Trigger::ExternalChange { changed_entities: vec!["eth0".to_string()] };

    let dir = setup_journal(vec![apply_entry, external_entry]);
    let output = run_history(dir.path(), &["--trigger", "external"]);
    assert!(output.status.success(), "trigger filter should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    let data_lines: Vec<&str> = text.lines().skip(1).collect();
    assert_eq!(
        data_lines.len(),
        1,
        "--trigger external should show only 1 entry, got {} lines:\n{text}",
        data_lines.len()
    );
    assert!(
        text.contains("external"),
        "--trigger external result must show 'external' trigger name; got:\n{text}"
    );
}

/// AC: -s name=eth0 shows only entries that touched eth0.
#[test]
fn test_history_selector_filter() {
    let eth0_entry = make_entry_with_diff("eth0", vec![]);
    let eth1_entry = make_entry_with_diff("eth1", vec![]);
    let dir = setup_journal(vec![eth0_entry, eth1_entry]);
    let output = run_history(dir.path(), &["-s", "name=eth0"]);
    assert!(output.status.success(), "selector filter should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    let data_lines: Vec<&str> = text.lines().skip(1).collect();
    assert_eq!(
        data_lines.len(),
        1,
        "-s name=eth0 should show only 1 entry, got {} lines:\n{text}",
        data_lines.len()
    );
    assert!(
        text.contains("eth0"),
        "result must show 'eth0'; got:\n{text}"
    );
}

/// AC: Combined filters use AND logic.
#[test]
fn test_history_combined_filters() {
    // Entry matching all: eth0, apply trigger, recent timestamp
    let mut matching = make_entry_with_diff("eth0", vec![]);
    matching.timestamp = Utc::now() - Duration::minutes(5);
    matching.trigger = Trigger::PolicyApply { source: "test.yaml".to_string() };

    // Entry with eth1 (fails selector filter)
    let mut eth1_entry = make_entry_with_diff("eth1", vec![]);
    eth1_entry.timestamp = Utc::now() - Duration::minutes(5);

    // Entry with eth0 but old (fails since filter)
    let mut old_entry = make_entry_with_diff("eth0", vec![]);
    old_entry.timestamp = Utc::now() - Duration::hours(3);

    let dir = setup_journal(vec![old_entry, eth1_entry, matching]);
    let output = run_history(dir.path(), &["--since", "1h", "-s", "name=eth0", "--trigger", "apply"]);
    assert!(output.status.success(), "combined filters should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    let data_lines: Vec<&str> = text.lines().skip(1).collect();
    assert_eq!(
        data_lines.len(),
        1,
        "combined filters should show only 1 entry, got {} lines:\n{text}",
        data_lines.len()
    );
}

/// AC: List header contains all expected column names.
#[test]
fn test_history_list_header_contains_all_columns() {
    let dir = setup_journal(vec![make_entry()]);
    let output = run_history(dir.path(), &[]);
    let text = combined(&output);
    let header = text.lines().next().unwrap_or("");
    for col in &["SEQ", "TIMESTAMP", "TRIGGER", "ENTITIES", "CHANGES"] {
        assert!(
            header.contains(col),
            "header must contain '{col}'; got: {header}"
        );
    }
}

// ── Feature: History detail command ──────────────────────────────────────────

/// AC: --show 1 shows full detail for entry seq=1.
#[test]
fn test_history_show_positive_seq() {
    let mut entry = make_entry();
    entry.outcome = ApplyOutcome::Applied { succeeded: 1, failed: 0, skipped: 0 };
    let dir = setup_journal(vec![entry]);
    let output = run_history(dir.path(), &["--show", "1"]);
    assert!(output.status.success(), "--show 1 should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    assert!(
        text.contains("Entry #1"),
        "output must contain 'Entry #1'; got:\n{text}"
    );
    assert!(
        text.contains("Trigger:"),
        "output must contain 'Trigger:'; got:\n{text}"
    );
    assert!(
        text.contains("Outcome:"),
        "output must contain 'Outcome:'; got:\n{text}"
    );
}

/// AC: --show -1 resolves to the most recent entry.
#[test]
fn test_history_show_negative_1() {
    let dir = setup_journal((0..30).map(|_| make_entry()).collect());
    let output = run_history(dir.path(), &["--show", "-1"]);
    assert!(output.status.success(), "--show -1 should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    assert!(
        text.contains("Entry #30"),
        "--show -1 should show 'Entry #30' (most recent); got:\n{text}"
    );
}

/// AC: --show -3 resolves to the 3rd-to-last entry.
#[test]
fn test_history_show_negative_3() {
    let dir = setup_journal((0..30).map(|_| make_entry()).collect());
    let output = run_history(dir.path(), &["--show", "-3"]);
    assert!(output.status.success(), "--show -3 should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    assert!(
        text.contains("Entry #28"),
        "--show -3 should show 'Entry #28'; got:\n{text}"
    );
}

/// AC: --show -10 with only 5 entries shows "Entry not found" and exits with code 1.
#[test]
fn test_history_show_negative_beyond_size() {
    let dir = setup_journal((0..5).map(|_| make_entry()).collect());
    let output = run_history(dir.path(), &["--show", "-10"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "--show -10 with 5 entries should exit 1; got: {}",
        combined(&output)
    );
    let text = combined(&output);
    assert!(
        text.contains("not found") || text.contains("Entry not found"),
        "--show -10 should show 'Entry not found'; got:\n{text}"
    );
}

/// AC: --show 9999 for a nonexistent entry shows "Entry #9999 not found" and exits 1.
#[test]
fn test_history_show_nonexistent() {
    let dir = setup_journal(vec![make_entry()]);
    let output = run_history(dir.path(), &["--show", "9999"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "--show 9999 should exit 1; got: {}",
        combined(&output)
    );
    let text = combined(&output);
    assert!(
        text.contains("9999"),
        "output must mention '9999'; got:\n{text}"
    );
    assert!(
        text.contains("not found"),
        "output must mention 'not found'; got:\n{text}"
    );
}

// ── Feature: State-after YAML format ─────────────────────────────────────────

/// AC: State-after section uses YAML block format matching netfyr query output.
#[test]
fn test_history_show_state_after_yaml_block_format() {
    let dir = setup_journal(vec![make_entry_with_state_after()]);
    let output = run_history(dir.path(), &["--show", "1"]);
    assert!(output.status.success(), "--show 1 should exit 0; got: {}", combined(&output));
    let text = combined(&output);

    assert!(
        text.contains("State after:"),
        "output must contain 'State after:'; got:\n{text}"
    );
    assert!(
        text.contains("- type: ethernet"),
        "state-after must have '- type: ethernet' (YAML block); got:\n{text}"
    );
    assert!(
        text.contains("  name: eth0"),
        "state-after must have '  name: eth0'; got:\n{text}"
    );
    assert!(
        text.contains("  mtu: 9000"),
        "state-after must have '  mtu: 9000'; got:\n{text}"
    );
    // Addresses must be YAML block sequence, not JSON inline array
    assert!(
        text.contains("  - 10.0.1.50/24"),
        "addresses must appear as '  - 10.0.1.50/24' (YAML block sequence); got:\n{text}"
    );
    assert!(
        !text.contains("[\"10.0.1.50/24\"]"),
        "addresses must not appear as JSON inline array; got:\n{text}"
    );
}

// ── Feature: Diff rendering ───────────────────────────────────────────────────

/// AC: Scalar field change shows -old / +new lines in detail view.
#[test]
fn test_history_show_scalar_diff_lines() {
    let entry = make_entry_with_diff("eth0", vec![SerializableFieldChange {
        field_name: "mtu".to_string(),
        change_kind: "set".to_string(),
        current: Some(serde_json::json!(1500u64)),
        desired: Some(serde_json::json!(9000u64)),
        outcome: None,
    }]);
    let dir = setup_journal(vec![entry]);
    let output = run_history(dir.path(), &["--show", "1"]);
    assert!(output.status.success(), "--show 1 should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    assert!(
        text.contains("-mtu: 1500"),
        "diff must show '-mtu: 1500'; got:\n{text}"
    );
    assert!(
        text.contains("+mtu: 9000"),
        "diff must show '+mtu: 9000'; got:\n{text}"
    );
}

/// AC: List field addition shows field header and per-element +lines.
#[test]
fn test_history_show_list_diff_additions() {
    let entry = make_entry_with_diff("enp7s0", vec![SerializableFieldChange {
        field_name: "addresses".to_string(),
        change_kind: "set".to_string(),
        current: Some(serde_json::json!([])),
        desired: Some(serde_json::json!(["172.25.14.22/32"])),
        outcome: None,
    }]);
    let dir = setup_journal(vec![entry]);
    let output = run_history(dir.path(), &["--show", "1"]);
    assert!(output.status.success(), "--show 1 should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    assert!(
        text.contains("addresses:"),
        "diff must show 'addresses:' header; got:\n{text}"
    );
    assert!(
        text.contains("+172.25.14.22/32"),
        "diff must show '+172.25.14.22/32'; got:\n{text}"
    );
}

// ── Feature: JSON output ──────────────────────────────────────────────────────

/// AC: -o json list produces a valid JSON array with N elements.
#[test]
fn test_history_json_list() {
    let dir = setup_journal((0..5).map(|_| make_entry()).collect());
    let output = run_history(dir.path(), &["-n", "5", "-o", "json"]);
    assert!(output.status.success(), "json list should exit 0; got: {}", combined(&output));
    let text = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("output must be valid JSON; err={e}; got:\n{text}"));
    let arr = parsed.as_array().expect("output must be a JSON array");
    assert_eq!(arr.len(), 5, "JSON array must have 5 elements; got: {}", arr.len());
}

/// AC: --show 1 -o json produces a valid JSON object with "seq" field.
#[test]
fn test_history_show_json_detail() {
    let mut entry = make_entry();
    entry.seq = 0; // will be assigned as 1
    let dir = setup_journal(vec![entry]);
    let output = run_history(dir.path(), &["--show", "1", "-o", "json"]);
    assert!(output.status.success(), "json detail should exit 0; got: {}", combined(&output));
    let text = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("output must be valid JSON; err={e}; got:\n{text}"));
    assert!(parsed.is_object(), "JSON detail output must be an object");
    assert!(
        parsed.get("seq").is_some(),
        "JSON detail must have 'seq' field; got:\n{text}"
    );
}

// ── Feature: Empty/missing journal ────────────────────────────────────────────

/// AC: Empty journal shows "No journal entries found." and exits 0.
#[test]
fn test_history_empty_journal() {
    let dir = setup_journal(vec![]);
    let output = run_history(dir.path(), &[]);
    assert!(output.status.success(), "empty journal should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    assert!(
        text.contains("No journal entries found"),
        "empty journal must show 'No journal entries found.'; got:\n{text}"
    );
}

/// AC: Nonexistent journal directory shows "No journal found at" and exits 1.
#[test]
fn test_history_no_journal_directory() {
    let nonexistent = "/tmp/netfyr-history-integration-test-nonexistent-dir-12345";
    let output = std::process::Command::new(netfyr_bin())
        .arg("history")
        .env("NO_COLOR", "1")
        .env("NETFYR_SOCKET_PATH", "/tmp/netfyr-nonexistent-integration-test.sock")
        .env("NETFYR_JOURNAL_DIR", nonexistent)
        .output()
        .expect("failed to run netfyr history");
    assert_eq!(
        output.status.code(),
        Some(1),
        "missing journal dir should exit 1; got: {}",
        combined(&output)
    );
    let text = combined(&output);
    assert!(
        text.contains("No journal found at") || text.contains("journal"),
        "output must mention journal not found; got:\n{text}"
    );
}

// ── Feature: Color output ──────────────────────────────────────────────────────

/// AC: Detail diff colors entire lines — with --color always the "-mtu: 1500" line
/// is entirely red (ANSI red wraps the full string, not just the "-" prefix), and
/// "+mtu: 9000" is entirely green.
#[test]
fn test_history_show_color_always_wraps_entire_diff_lines() {
    let entry = make_entry_with_diff("eth0", vec![SerializableFieldChange {
        field_name: "mtu".to_string(),
        change_kind: "set".to_string(),
        current: Some(serde_json::json!(1500u64)),
        desired: Some(serde_json::json!(9000u64)),
        outcome: None,
    }]);
    let dir = setup_journal(vec![entry]);

    // Use --color always; do NOT set NO_COLOR so colors are actually emitted.
    let output = std::process::Command::new(netfyr_bin())
        .arg("--color").arg("always")
        .arg("history")
        .arg("--show").arg("1")
        .env("NETFYR_SOCKET_PATH", "/tmp/netfyr-nonexistent-integration-test.sock")
        .env("NETFYR_JOURNAL_DIR", dir.path())
        .output()
        .expect("failed to run netfyr history --color always --show 1");

    assert!(
        output.status.success(),
        "--show 1 --color always should exit 0; got: {}",
        combined(&output)
    );

    let text = String::from_utf8_lossy(&output.stdout);

    // ANSI red = \x1b[31m, green = \x1b[32m, reset = \x1b[0m.
    // The color must start BEFORE the field name, wrapping the full "-mtu: 1500" string.
    assert!(
        text.contains("\x1b[31m-mtu"),
        "red ANSI code must precede '-mtu' (entire line colored, not just prefix); got:\n{}",
        text.escape_debug()
    );
    assert!(
        text.contains("\x1b[32m+mtu"),
        "green ANSI code must precede '+mtu' (entire line colored, not just prefix); got:\n{}",
        text.escape_debug()
    );

    // The ANSI reset must NOT appear between the "-" prefix and the field name,
    // which would mean only the prefix character is colored.
    assert!(
        !text.contains("\x1b[0mmtu: 1500"),
        "ANSI reset must not appear before 'mtu: 1500' — color must wrap the full line; got:\n{}",
        text.escape_debug()
    );
    assert!(
        !text.contains("\x1b[0mmtu: 9000"),
        "ANSI reset must not appear before 'mtu: 9000' — color must wrap the full line; got:\n{}",
        text.escape_debug()
    );
}

// ── Feature: Route diff format ────────────────────────────────────────────────

/// AC: Detail diff shows route changes with readable format:
/// "routes:" header followed by "+destination metric N" per-element lines.
#[test]
fn test_history_show_route_diff_with_metric_readable_format() {
    let entry = make_entry_with_diff("eth0", vec![SerializableFieldChange {
        field_name: "routes".to_string(),
        change_kind: "set".to_string(),
        current: Some(serde_json::json!([])),
        desired: Some(serde_json::json!([
            {"destination": "10.0.0.0/8", "metric": 100}
        ])),
        outcome: None,
    }]);
    let dir = setup_journal(vec![entry]);
    let output = run_history(dir.path(), &["--show", "1"]);
    assert!(output.status.success(), "--show 1 should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    assert!(
        text.contains("routes:"),
        "diff must show 'routes:' header; got:\n{text}"
    );
    assert!(
        text.contains("+10.0.0.0/8 metric 100"),
        "route with metric must show '+10.0.0.0/8 metric 100'; got:\n{text}"
    );
}

/// AC: Route with metric=0 is shown without the "metric 0" suffix.
#[test]
fn test_history_show_route_diff_zero_metric_omitted() {
    let entry = make_entry_with_diff("eth0", vec![SerializableFieldChange {
        field_name: "routes".to_string(),
        change_kind: "set".to_string(),
        current: Some(serde_json::json!([])),
        desired: Some(serde_json::json!([
            {"destination": "172.25.14.22/32", "metric": 0}
        ])),
        outcome: None,
    }]);
    let dir = setup_journal(vec![entry]);
    let output = run_history(dir.path(), &["--show", "1"]);
    assert!(output.status.success(), "--show 1 should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    assert!(
        text.contains("+172.25.14.22/32"),
        "route with metric=0 must show '+172.25.14.22/32' without 'metric 0'; got:\n{text}"
    );
    assert!(
        !text.contains("metric 0"),
        "route with metric=0 must NOT include 'metric 0' suffix; got:\n{text}"
    );
}

// ── Feature: failure indicator in CHANGES column ─────────────────────────────

/// AC: CHANGES column shows "FAIL" prefix when failures occurred in list view.
#[test]
fn test_history_list_changes_column_shows_fail_prefix_when_failures_occurred() {
    let mut entry = make_entry();
    entry.outcome = ApplyOutcome::Applied { succeeded: 1, failed: 2, skipped: 0 };
    let dir = setup_journal(vec![entry]);
    let output = run_history(dir.path(), &[]);
    assert!(output.status.success(), "history should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    assert!(
        text.contains("FAIL"),
        "CHANGES column must show 'FAIL' prefix when failures occurred; got:\n{text}"
    );
}

/// AC: CHANGES column has no FAIL prefix for successful entries.
#[test]
fn test_history_list_changes_column_no_fail_prefix_for_success() {
    let mut entry = make_entry();
    entry.outcome = ApplyOutcome::Applied { succeeded: 2, failed: 0, skipped: 0 };
    let dir = setup_journal(vec![entry]);
    let output = run_history(dir.path(), &[]);
    assert!(output.status.success(), "history should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    let data_row = text.lines().nth(1).unwrap_or("");
    assert!(
        !data_row.contains("FAIL"),
        "CHANGES column must not show 'FAIL' when no failures; got:\n{text}"
    );
}

// ── Feature: Reverse chronological order ──────────────────────────────────────

/// AC: List recent journal entries — entries are shown in reverse chronological order
/// (most recent first). The first data row after the header must have the highest seq.
#[test]
fn test_history_list_entries_in_reverse_chronological_order() {
    // Insert 3 entries; they get seq=1, seq=2, seq=3 in insertion order.
    // read_recent returns newest-first, so seq=3 should appear in the first data row.
    let dir = setup_journal((0..3).map(|_| make_entry()).collect());
    let output = run_history(dir.path(), &["-n", "3"]);
    assert!(output.status.success(), "history should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines.len() >= 4,
        "expected header + 3 data rows, got {} lines:\n{text}",
        lines.len()
    );
    // First data row (lines[1]) must start with seq=3 (most recent).
    assert!(
        lines[1].starts_with("3"),
        "first data row must be the most recent entry (seq=3); got:\n{}",
        lines[1]
    );
    // Last data row (lines[3]) must start with seq=1 (oldest).
    assert!(
        lines[3].starts_with("1"),
        "last data row must be the oldest entry (seq=1); got:\n{}",
        lines[3]
    );
}

// ── Feature: CHANGES column notation ──────────────────────────────────────────

/// AC: CHANGES column shows actual address value "+addr" for single address addition (spec: 1-2 changes show values inline).
#[test]
fn test_history_list_changes_column_shows_addr_plus_n_for_list_additions() {
    let entry = make_entry_with_diff("eth0", vec![SerializableFieldChange {
        field_name: "addresses".to_string(),
        change_kind: "set".to_string(),
        current: Some(serde_json::json!([])),
        desired: Some(serde_json::json!(["10.0.0.1/24"])),
        outcome: None,
    }]);
    let dir = setup_journal(vec![entry]);
    let output = run_history(dir.path(), &[]);
    assert!(output.status.success(), "history should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    // Spec: 1-2 address changes show actual values "+addr"
    assert!(
        text.contains("+10.0.0.1/24"),
        "CHANGES column must show '+10.0.0.1/24' (actual value) for one address added; got:\n{text}"
    );
}

/// AC: CHANGES column shows "field old→new" notation for scalar field modifications.
#[test]
fn test_history_list_changes_column_shows_tilde_field_for_scalar_changes() {
    let entry = make_entry_with_diff("eth0", vec![SerializableFieldChange {
        field_name: "mtu".to_string(),
        change_kind: "set".to_string(),
        current: Some(serde_json::json!(1500u64)),
        desired: Some(serde_json::json!(9000u64)),
        outcome: None,
    }]);
    let dir = setup_journal(vec![entry]);
    let output = run_history(dir.path(), &[]);
    assert!(output.status.success(), "history should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    // Spec: scalar field changes use "field old→new" notation
    assert!(
        text.contains("mtu 1500→9000") || text.contains("mtu 1500\u{2192}9000"),
        "CHANGES column must show 'mtu 1500→9000' for scalar field modification; got:\n{text}"
    );
}

// ── Feature: Detail view active policies ──────────────────────────────────────

/// AC: Detail view shows active policies with factory type and priority.
#[test]
fn test_history_show_detail_includes_active_policies() {
    let mut entry = make_entry();
    entry.active_policies = vec![
        PolicySummary { name: "eth0-config".to_string(), factory_type: "static".to_string(), priority: 100 },
        PolicySummary { name: "eth0-dhcp".to_string(), factory_type: "dhcpv4".to_string(), priority: 100 },
    ];
    let dir = setup_journal(vec![entry]);
    let output = run_history(dir.path(), &["--show", "1"]);
    assert!(output.status.success(), "--show 1 should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    assert!(
        text.contains("Active policies:"),
        "detail must show 'Active policies:' section; got:\n{text}"
    );
    assert!(
        text.contains("eth0-config"),
        "detail must list policy 'eth0-config'; got:\n{text}"
    );
    assert!(
        text.contains("eth0-dhcp"),
        "detail must list policy 'eth0-dhcp'; got:\n{text}"
    );
    assert!(
        text.contains("static"),
        "detail must show factory type 'static'; got:\n{text}"
    );
}

// ── Feature: Detail view trigger display ──────────────────────────────────────

/// AC: Detail view shows trigger source for policy-apply trigger.
#[test]
fn test_history_show_detail_shows_trigger_source_for_policy_apply() {
    let mut entry = make_entry();
    entry.trigger = Trigger::PolicyApply { source: "/etc/netfyr/policies/".to_string() };
    let dir = setup_journal(vec![entry]);
    let output = run_history(dir.path(), &["--show", "1"]);
    assert!(output.status.success(), "--show 1 should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    assert!(
        text.contains("policy-apply"),
        "detail must show 'policy-apply' trigger name; got:\n{text}"
    );
    assert!(
        text.contains("/etc/netfyr/policies/"),
        "detail must show the trigger source path; got:\n{text}"
    );
}

/// AC: Detail view shows outcome with full breakdown (succeeded/failed/skipped counts).
#[test]
fn test_history_show_detail_outcome_shows_full_breakdown() {
    let mut entry = make_entry();
    entry.outcome = ApplyOutcome::Applied { succeeded: 3, failed: 1, skipped: 2 };
    let dir = setup_journal(vec![entry]);
    let output = run_history(dir.path(), &["--show", "1"]);
    assert!(output.status.success(), "--show 1 should exit 0; got: {}", combined(&output));
    let text = combined(&output);
    assert!(
        text.contains("3 succeeded"),
        "detail outcome must show '3 succeeded'; got:\n{text}"
    );
    assert!(
        text.contains("1 failed"),
        "detail outcome must show '1 failed'; got:\n{text}"
    );
    assert!(
        text.contains("2 skipped"),
        "detail outcome must show '2 skipped'; got:\n{text}"
    );
}

// ── Feature: show --show 0 edge case ──────────────────────────────────────────

/// AC: --show 0 is an invalid sequence number and returns "not found" with exit 1.
#[test]
fn test_history_show_zero_returns_not_found() {
    let dir = setup_journal(vec![make_entry()]);
    let output = run_history(dir.path(), &["--show", "0"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "--show 0 should exit 1; got: {}",
        combined(&output)
    );
    let text = combined(&output);
    assert!(
        text.contains("not found"),
        "--show 0 must report 'not found'; got:\n{text}"
    );
}
