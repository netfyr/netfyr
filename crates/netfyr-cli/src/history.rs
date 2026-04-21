//! Implementation of the `netfyr history` subcommand.
//!
//! Two runtime modes are supported, detected automatically:
//!
//! 1. **Daemon-free**: reads journal files directly via `Journal::open_default()`.
//! 2. **Daemon**: retrieves history via Varlink `GetHistory` / `GetJournalEntry`.

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::{Args, ValueEnum};
use colored::Colorize;

use netfyr_journal::{ApplyOutcome, Journal, JournalEntry, SerializableDiffOp, Trigger};
use netfyr_varlink::{VarlinkClient, VarlinkError};

use crate::daemon_socket_path;

// ── Output format ─────────────────────────────────────────────────────────────

#[derive(Clone, ValueEnum, PartialEq)]
pub enum HistoryOutputFormat {
    Text,
    Json,
}

// ── CLI args ──────────────────────────────────────────────────────────────────

#[derive(Args)]
pub struct HistoryArgs {
    /// Number of entries to show (most recent first)
    #[arg(long, short = 'n', default_value = "20")]
    pub count: usize,

    /// Show entries since this time (e.g. 1h, 30m, 7d or ISO 8601)
    #[arg(long)]
    pub since: Option<String>,

    /// Filter by trigger type (apply, dhcp, external, startup, revert)
    #[arg(long)]
    pub trigger: Option<String>,

    /// Filter by entity name (name=X)
    #[arg(long, short = 's', value_parser = parse_history_selector)]
    pub selector: Vec<(String, String)>,

    /// Show full detail for a single entry by sequence ID
    #[arg(long)]
    pub show: Option<u64>,

    /// Output format: text (default), json
    #[arg(long, short = 'o', default_value = "text")]
    pub output: HistoryOutputFormat,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run_history(args: HistoryArgs) -> Result<ExitCode> {
    let socket_path = daemon_socket_path();
    match VarlinkClient::connect(&socket_path).await {
        Ok(mut client) => {
            return run_history_daemon(&mut client, &args).await;
        }
        Err(VarlinkError::ConnectionFailed(_)) => {
            // Daemon not running — fall through to local mode.
        }
        Err(e) => {
            return Err(
                anyhow::Error::from(e).context("unexpected error connecting to daemon socket")
            );
        }
    }
    run_history_local(&args).await
}

// ── Local mode ────────────────────────────────────────────────────────────────

async fn run_history_local(args: &HistoryArgs) -> Result<ExitCode> {
    let journal_dir = journal_dir_path();
    let dir = Path::new(&journal_dir);

    if !dir.exists() {
        eprintln!("No journal found at {}/", journal_dir);
        return Ok(ExitCode::from(1u8));
    }

    let journal = Journal::open(dir)
        .with_context(|| format!("failed to open journal at {}", journal_dir))?;

    if let Some(seq) = args.show {
        let entry = journal
            .read_entry(seq)
            .with_context(|| format!("failed to read journal entry #{}", seq))?;

        match entry {
            Some(e) => {
                print_detail(&e, &args.output)?;
                return Ok(ExitCode::from(0u8));
            }
            None => {
                eprintln!("Entry #{} not found", seq);
                return Ok(ExitCode::from(1u8));
            }
        }
    }

    let has_filters =
        args.since.is_some() || args.trigger.is_some() || !args.selector.is_empty();
    let read_count = if has_filters { 10_000 } else { args.count };

    let raw_entries = journal
        .read_recent(read_count)
        .context("failed to read journal entries")?;

    if raw_entries.is_empty() {
        println!("No journal entries found.");
        return Ok(ExitCode::from(0u8));
    }

    let entries = filter_entries(raw_entries, args)?;

    if entries.is_empty() {
        println!("No journal entries found.");
        return Ok(ExitCode::from(0u8));
    }

    print_list(&entries, &args.output)?;
    Ok(ExitCode::from(0u8))
}

// ── Daemon mode ───────────────────────────────────────────────────────────────

async fn run_history_daemon(
    client: &mut VarlinkClient,
    args: &HistoryArgs,
) -> Result<ExitCode> {
    if let Some(seq) = args.show {
        let raw = client
            .get_journal_entry(seq)
            .await
            .context("failed to get journal entry from daemon")?;

        match raw {
            Some(value) => {
                let entry: JournalEntry = serde_json::from_value(value)
                    .context("failed to deserialize journal entry from daemon")?;
                print_detail(&entry, &args.output)?;
                return Ok(ExitCode::from(0u8));
            }
            None => {
                eprintln!("Entry #{} not found", seq);
                return Ok(ExitCode::from(1u8));
            }
        }
    }

    let selector_name = args.selector.first().map(|(_, v)| v.clone());
    let raw_entries = client
        .get_history(
            Some(args.count),
            args.since.clone(),
            args.trigger.clone(),
            selector_name,
        )
        .await
        .context("failed to get history from daemon")?;

    if raw_entries.is_empty() {
        println!("No journal entries found.");
        return Ok(ExitCode::from(0u8));
    }

    let entries: Vec<JournalEntry> = raw_entries
        .into_iter()
        .map(|v| serde_json::from_value(v).context("failed to deserialize journal entry"))
        .collect::<Result<Vec<_>>>()?;

    print_list(&entries, &args.output)?;
    Ok(ExitCode::from(0u8))
}

// ── Parsing helpers ───────────────────────────────────────────────────────────

fn parse_history_selector(s: &str) -> Result<(String, String), String> {
    let eq = s.find('=').ok_or_else(|| {
        format!(
            "selector must be in key=value format, got: {:?}. Only 'name' key is supported",
            s
        )
    })?;
    let key = &s[..eq];
    let value = &s[eq + 1..];

    if key != "name" {
        return Err(format!(
            "invalid selector key {:?}; history only supports 'name' (e.g. name=eth0)",
            key
        ));
    }

    Ok((key.to_string(), value.to_string()))
}

pub fn parse_since(s: &str) -> Result<DateTime<Utc>> {
    let now = Utc::now();

    // Try relative duration: 30s, 5m, 1h, 7d
    if let Some((num_str, unit)) = parse_relative_duration(s) {
        let num: u64 = num_str
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid number in duration: {:?}", s))?;
        let seconds: u64 = match unit {
            "s" => num,
            "m" => num * 60,
            "h" => num * 3600,
            "d" => num * 86400,
            _ => return Err(anyhow::anyhow!("invalid duration unit {:?}; use s, m, h, or d", unit)),
        };
        let duration = chrono::Duration::try_seconds(seconds as i64)
            .ok_or_else(|| anyhow::anyhow!("duration overflow: {:?}", s))?;
        return Ok(now - duration);
    }

    // Try ISO 8601 / RFC 3339
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| {
            anyhow::anyhow!(
                "invalid duration or timestamp {:?}; use e.g. 1h, 30m, 7d or ISO 8601",
                s
            )
        })
}

fn parse_relative_duration(s: &str) -> Option<(&str, &str)> {
    for unit in &["d", "h", "m", "s"] {
        if let Some(num_str) = s.strip_suffix(unit) {
            if !num_str.is_empty() {
                return Some((num_str, unit));
            }
        }
    }
    None
}

// ── Filtering ─────────────────────────────────────────────────────────────────

pub fn filter_entries(
    entries: Vec<JournalEntry>,
    args: &HistoryArgs,
) -> Result<Vec<JournalEntry>> {
    let since_cutoff = match &args.since {
        Some(s) => Some(parse_since(s)?),
        None => None,
    };

    let filtered = entries
        .into_iter()
        .filter(|e| {
            if let Some(cutoff) = since_cutoff {
                if e.timestamp < cutoff {
                    return false;
                }
            }
            if let Some(t) = &args.trigger {
                if !matches_trigger(e, t) {
                    return false;
                }
            }
            if !args.selector.is_empty() && !matches_selector(e, &args.selector) {
                return false;
            }
            true
        })
        .take(args.count)
        .collect();

    Ok(filtered)
}

pub fn matches_trigger(entry: &JournalEntry, trigger_filter: &str) -> bool {
    let trigger_type = trigger_type_str(&entry.trigger);
    trigger_type
        .to_lowercase()
        .contains(&trigger_filter.to_lowercase())
}

fn trigger_type_str(trigger: &Trigger) -> &'static str {
    match trigger {
        Trigger::PolicyApply { .. } => "policy_apply",
        Trigger::DhcpEvent { .. } => "dhcp_event",
        Trigger::ExternalChange { .. } => "external_change",
        Trigger::DaemonStartup => "daemon_startup",
        Trigger::Revert { .. } => "revert",
    }
}

pub fn matches_selector(entry: &JournalEntry, selectors: &[(String, String)]) -> bool {
    for (key, value) in selectors {
        if key == "name" {
            let found = entry
                .diff
                .operations
                .iter()
                .any(|op| op.entity_name == *value);
            if !found {
                return false;
            }
        }
    }
    true
}

// ── Output dispatch ───────────────────────────────────────────────────────────

fn print_list(entries: &[JournalEntry], format: &HistoryOutputFormat) -> Result<()> {
    match format {
        HistoryOutputFormat::Text => print!("{}", format_text_list(entries)),
        HistoryOutputFormat::Json => println!("{}", format_json_list(entries)?),
    }
    Ok(())
}

fn print_detail(entry: &JournalEntry, format: &HistoryOutputFormat) -> Result<()> {
    match format {
        HistoryOutputFormat::Text => print!("{}", format_text_detail(entry)),
        HistoryOutputFormat::Json => println!("{}", format_json_detail(entry)?),
    }
    Ok(())
}

// ── Text formatting ───────────────────────────────────────────────────────────

pub fn format_text_list(entries: &[JournalEntry]) -> String {
    // Fixed-width overhead: SEQ(5)+sp+TIMESTAMP(21)+sp+TRIGGER(15)+sp+ENTITIES(14)+sp+OUTCOME(16)+sp = 76
    const FIXED_OVERHEAD: usize = 76;
    let terminal_width = get_terminal_width();
    let max_changes_width = terminal_width.saturating_sub(FIXED_OVERHEAD).max(10);

    let mut out = String::new();
    out.push_str(&format!(
        "{:<5} {:<21} {:<15} {:<14} {:<16} {}\n",
        "SEQ", "TIMESTAMP", "TRIGGER", "ENTITIES", "OUTCOME", "CHANGES"
    ));

    for entry in entries {
        let seq = entry.seq.to_string();
        let ts = entry.timestamp.format("%Y-%m-%d %H:%M:%S").to_string();
        let trigger = trigger_display_name(&entry.trigger);
        let entities = entities_summary(&entry.diff.operations);
        let outcome = outcome_summary(&entry.outcome);
        let changes_plain = changes_summary(&entry.diff.operations);
        let changes_truncated = if changes_plain.len() > max_changes_width {
            let mut trim_to = max_changes_width.saturating_sub(3);
            while trim_to > 0 && !changes_plain.is_char_boundary(trim_to) {
                trim_to -= 1;
            }
            format!("{}...", &changes_plain[..trim_to])
        } else {
            changes_plain
        };
        let changes = colorize_changes(&changes_truncated);

        out.push_str(&format!(
            "{:<5} {:<21} {:<15} {:<14} {:<16} {}\n",
            seq, ts, trigger, entities, outcome, changes
        ));
    }

    out
}

pub fn format_text_detail(entry: &JournalEntry) -> String {
    let mut out = String::new();

    let ts = entry.timestamp.format("%Y-%m-%d %H:%M:%S").to_string();
    out.push_str(&format!("Entry #{} at {} UTC\n", entry.seq, ts));

    let trigger_type = trigger_display_name(&entry.trigger);
    let trigger_detail = trigger_detail_str(&entry.trigger);
    out.push_str(&format!("Trigger: {}{}\n", trigger_type, trigger_detail));

    if !entry.active_policies.is_empty() {
        out.push_str("Active policies:\n");
        for p in &entry.active_policies {
            out.push_str(&format!(
                "  - {} ({}, priority {})\n",
                p.name, p.factory_type, p.priority
            ));
        }
    }

    out.push_str("Diff:\n");
    if entry.diff.operations.is_empty() {
        out.push_str("  (no changes)\n");
    } else {
        for op in &entry.diff.operations {
            let colored_prefix = match op.kind.as_str() {
                "add" => "+".green(),
                "remove" => "-".red(),
                _ => "~".yellow(),
            };
            out.push_str(&format!("  {} {} {}\n", colored_prefix, op.entity_type, op.entity_name));
            for fc in &op.field_changes {
                if fc.change_kind == "unchanged" {
                    continue;
                }
                let line = match fc.change_kind.as_str() {
                    "set" if fc.current.is_none() => {
                        let desired = opt_json_compact(&fc.desired);
                        format!("      {}{}: {}", "+".green(), fc.field_name, desired)
                    }
                    "set" => {
                        let current = opt_json_compact(&fc.current);
                        let desired = opt_json_compact(&fc.desired);
                        format!("      {}{}: {} -> {}", "~".yellow(), fc.field_name, current, desired)
                    }
                    "unset" => {
                        let current = opt_json_compact(&fc.current);
                        format!("      {}{}: {}", "-".red(), fc.field_name, current)
                    }
                    _ => continue,
                };
                out.push_str(&format!("{}\n", line));
            }
        }
    }

    out.push_str(&format!("Outcome: {}\n", outcome_summary(&entry.outcome)));

    if !entry.state_after.entities.is_empty() {
        out.push_str("State after:\n");
        for state in &entry.state_after.entities {
            out.push_str(&format!(
                "  {} {}:\n",
                state.entity_type, state.selector_name
            ));
            if let Some(obj) = state.fields.as_object() {
                for (k, v) in obj {
                    out.push_str(&format!("    {}: {}\n", k, json_compact(v)));
                }
            }
        }
    }

    out
}

pub fn format_json_list(entries: &[JournalEntry]) -> Result<String> {
    serde_json::to_string_pretty(entries).context("failed to serialize entries to JSON")
}

pub fn format_json_detail(entry: &JournalEntry) -> Result<String> {
    serde_json::to_string_pretty(entry).context("failed to serialize entry to JSON")
}

// ── Display helpers ───────────────────────────────────────────────────────────

pub fn trigger_display_name(trigger: &Trigger) -> &'static str {
    match trigger {
        Trigger::PolicyApply { .. } => "policy-apply",
        Trigger::DhcpEvent { .. } => "dhcp-lease",
        Trigger::ExternalChange { .. } => "external",
        Trigger::DaemonStartup => "daemon-startup",
        Trigger::Revert { .. } => "revert",
    }
}

fn trigger_detail_str(trigger: &Trigger) -> String {
    match trigger {
        Trigger::PolicyApply { source } => format!(" (source: {})", source),
        Trigger::DhcpEvent { policy_name, event_kind } => {
            format!(" (policy: {}, event: {})", policy_name, event_kind)
        }
        Trigger::ExternalChange { changed_entities } => {
            if changed_entities.is_empty() {
                String::new()
            } else {
                format!(" ({})", changed_entities.join(", "))
            }
        }
        Trigger::DaemonStartup => String::new(),
        Trigger::Revert { target_seq } => format!(" (target seq: {})", target_seq),
    }
}

pub fn outcome_summary(outcome: &ApplyOutcome) -> String {
    match outcome {
        ApplyOutcome::Applied { succeeded, failed, skipped } => {
            let mut parts: Vec<String> = Vec::new();
            if *succeeded > 0 {
                parts.push(format!("{} ok", succeeded));
            }
            if *failed > 0 {
                parts.push(format!("{} failed", failed));
            }
            if *skipped > 0 {
                parts.push(format!("{} skipped", skipped));
            }
            if parts.is_empty() {
                "applied".to_string()
            } else {
                format!("applied ({})", parts.join(", "))
            }
        }
        ApplyOutcome::Observed => "observed".to_string(),
    }
}

pub fn entities_summary(ops: &[SerializableDiffOp]) -> String {
    if ops.is_empty() {
        return "(none)".to_string();
    }
    let names: Vec<&str> = ops.iter().map(|op| op.entity_name.as_str()).collect();
    if names.len() <= 3 {
        names.join(", ")
    } else {
        let shown = &names[..2];
        let remaining = names.len() - 2;
        format!("{}, +{} more", shown.join(", "), remaining)
    }
}

pub fn changes_summary(ops: &[SerializableDiffOp]) -> String {
    if ops.is_empty() {
        return "(none)".to_string();
    }

    let add_count = ops.iter().filter(|op| op.kind == "add").count();
    let remove_count = ops.iter().filter(|op| op.kind == "remove").count();
    let modify_count = ops.iter().filter(|op| op.kind == "modify").count();

    // Only entity additions
    if add_count > 0 && remove_count == 0 && modify_count == 0 {
        return if add_count == 1 {
            "+entity".to_string()
        } else {
            format!("+{} entities", add_count)
        };
    }

    // Only entity removals
    if remove_count > 0 && add_count == 0 && modify_count == 0 {
        return if remove_count == 1 {
            "-entity".to_string()
        } else {
            format!("-{} entities", remove_count)
        };
    }

    // Mixed or modify-only: collect field-level changes
    let mut changes: Vec<String> = Vec::new();
    for op in ops {
        match op.kind.as_str() {
            "add" => changes.push(format!("+{}", op.entity_name)),
            "remove" => changes.push(format!("-{}", op.entity_name)),
            _ => {
                for fc in &op.field_changes {
                    if fc.change_kind == "unchanged" {
                        continue;
                    }
                    let is_list = fc.current.as_ref().map(|v| v.is_array()).unwrap_or(false)
                        || fc.desired.as_ref().map(|v| v.is_array()).unwrap_or(false);
                    if is_list {
                        let current_items: Vec<&serde_json::Value> = fc
                            .current
                            .as_ref()
                            .and_then(|v| v.as_array())
                            .map(|a| a.iter().collect())
                            .unwrap_or_default();
                        let desired_items: Vec<&serde_json::Value> = fc
                            .desired
                            .as_ref()
                            .and_then(|v| v.as_array())
                            .map(|a| a.iter().collect())
                            .unwrap_or_default();
                        let additions = desired_items
                            .iter()
                            .filter(|d| !current_items.contains(d))
                            .count();
                        let removals = current_items
                            .iter()
                            .filter(|c| !desired_items.contains(c))
                            .count();
                        if additions == 0 && removals == 0 {
                            continue;
                        }
                        let notation = match (additions, removals) {
                            (a, 0) => format!("{}(+{})", fc.field_name, a),
                            (0, r) => format!("{}(-{})", fc.field_name, r),
                            (a, r) => format!("{}(+{},-{})", fc.field_name, a, r),
                        };
                        if !changes.contains(&notation) {
                            changes.push(notation);
                        }
                    } else {
                        let notation = match fc.change_kind.as_str() {
                            "set" if fc.current.is_none() => format!("+{}", fc.field_name),
                            "set" => format!("~{}", fc.field_name),
                            "unset" => format!("-{}", fc.field_name),
                            _ => continue,
                        };
                        if !changes.contains(&notation) {
                            changes.push(notation);
                        }
                    }
                }
            }
        }
    }

    if changes.is_empty() {
        return "(no changes)".to_string();
    }

    if changes.len() <= 3 {
        changes.join(", ")
    } else {
        let shown = &changes[..3];
        let remaining = changes.len() - 3;
        format!("{}, +{} more", shown.join(", "), remaining)
    }
}

fn get_terminal_width() -> usize {
    use terminal_size::{terminal_size, Width};
    terminal_size().map(|(Width(w), _)| w as usize).unwrap_or(120)
}

fn colorize_changes(plain: &str) -> String {
    if plain == "(none)" || plain == "(no changes)" {
        return plain.to_string();
    }
    let (main, has_ellipsis) = if plain.ends_with("...") && plain.len() > 3 {
        (&plain[..plain.len() - 3], true)
    } else {
        (plain, false)
    };
    if main.is_empty() {
        return if has_ellipsis { "...".to_string() } else { plain.to_string() };
    }
    let colored: String = main
        .split(", ")
        .map(colorize_change_token)
        .collect::<Vec<_>>()
        .join(", ");
    if has_ellipsis { format!("{}...", colored) } else { colored }
}

fn colorize_change_token(token: &str) -> String {
    if token.is_empty() || token.ends_with(" more") {
        return token.to_string();
    }
    match token.chars().next() {
        Some('+') => token.green().to_string(),
        Some('-') => token.red().to_string(),
        Some('~') => token.yellow().to_string(),
        _ => {
            // List notation: "field(+N)", "field(-N)", "field(+N,-M)"
            if let Some(paren_start) = token.find('(') {
                if token.ends_with(')') {
                    let field_name = &token[..paren_start];
                    let inner = &token[paren_start + 1..token.len() - 1];
                    let parts: String = inner
                        .split(',')
                        .map(|p| {
                            if p.starts_with('+') {
                                p.green().to_string()
                            } else if p.starts_with('-') {
                                p.red().to_string()
                            } else {
                                p.to_string()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    return format!("{}({})", field_name, parts);
                }
            }
            token.to_string()
        }
    }
}

fn json_compact(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "?".to_string())
}

fn opt_json_compact(v: &Option<serde_json::Value>) -> String {
    match v {
        Some(val) => json_compact(val),
        None => "null".to_string(),
    }
}

pub fn journal_dir_path() -> String {
    std::env::var("NETFYR_JOURNAL_DIR")
        .unwrap_or_else(|_| "/var/lib/netfyr/journal".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::ExitCode;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    };

    use chrono::{Duration, Utc};
    use netfyr_journal::{
        ApplyOutcome, Journal, JournalEntry, PolicySummary, SerializableDiff,
        SerializableDiffOp, SerializableFieldChange, SerializableState, SerializableStateSet,
        Trigger,
    };

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    fn temp_dir() -> std::path::PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join(format!("netfyr-cli-history-test-{}-{}", std::process::id(), id));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_entry() -> JournalEntry {
        JournalEntry {
            seq: 0,
            timestamp: Utc::now(),
            trigger: Trigger::PolicyApply { source: "test.yaml".to_string() },
            active_policies: vec![],
            diff: SerializableDiff { operations: vec![] },
            state_after: SerializableStateSet { entities: vec![] },
            outcome: ApplyOutcome::Applied { succeeded: 1, failed: 0, skipped: 0 },
        }
    }

    fn make_entry_with_entity(entity_name: &str) -> JournalEntry {
        let mut entry = make_entry();
        entry.diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: entity_name.to_string(),
                field_changes: vec![],
            }],
        };
        entry
    }

    fn default_args() -> HistoryArgs {
        HistoryArgs {
            count: 20,
            since: None,
            trigger: None,
            selector: vec![],
            show: None,
            output: HistoryOutputFormat::Text,
        }
    }

    // ── parse_since ───────────────────────────────────────────────────────────

    /// AC: --since 30s parses to approximately 30 seconds ago.
    #[test]
    fn test_parse_since_30s_returns_time_30_seconds_ago() {
        let before = Utc::now();
        let result = parse_since("30s").unwrap();
        let after = Utc::now();

        assert!(
            result >= before - Duration::seconds(31) && result <= after - Duration::seconds(29),
            "parse_since(\"30s\") should return approx 30 seconds ago"
        );
    }

    /// AC: --since 5m parses to approximately 5 minutes ago.
    #[test]
    fn test_parse_since_5m_returns_time_5_minutes_ago() {
        let before = Utc::now();
        let result = parse_since("5m").unwrap();
        let after = Utc::now();

        assert!(
            result >= before - Duration::seconds(301) && result <= after - Duration::seconds(299),
            "parse_since(\"5m\") should return approx 5 minutes ago"
        );
    }

    /// AC: --since 1h parses to approximately 1 hour ago.
    #[test]
    fn test_parse_since_1h_returns_time_1_hour_ago() {
        let before = Utc::now();
        let result = parse_since("1h").unwrap();
        let after = Utc::now();

        assert!(
            result >= before - Duration::seconds(3601) && result <= after - Duration::seconds(3599),
            "parse_since(\"1h\") should return approx 1 hour ago"
        );
    }

    /// AC: --since 7d parses to approximately 7 days ago.
    #[test]
    fn test_parse_since_7d_returns_time_7_days_ago() {
        let before = Utc::now();
        let result = parse_since("7d").unwrap();
        let after = Utc::now();

        assert!(
            result >= before - Duration::seconds(7 * 86401)
                && result <= after - Duration::seconds(7 * 86399),
            "parse_since(\"7d\") should return approx 7 days ago"
        );
    }

    /// AC: --since with ISO 8601 timestamp returns the exact time.
    #[test]
    fn test_parse_since_iso8601_timestamp_returns_exact_time() {
        let result = parse_since("2026-04-20T14:00:00Z").unwrap();
        let expected = chrono::DateTime::parse_from_rfc3339("2026-04-20T14:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(result, expected, "parse_since should accept ISO 8601 timestamps");
    }

    /// AC: --since with invalid input returns an error.
    #[test]
    fn test_parse_since_invalid_input_returns_error() {
        assert!(parse_since("foobar").is_err(), "invalid duration string should return error");
    }

    /// AC: --since with unknown unit returns an error.
    #[test]
    fn test_parse_since_unknown_unit_returns_error() {
        assert!(parse_since("5x").is_err(), "unknown duration unit should return error");
    }

    // ── matches_trigger ───────────────────────────────────────────────────────

    /// AC: "apply" filter matches PolicyApply trigger entries.
    #[test]
    fn test_matches_trigger_apply_matches_policy_apply() {
        let entry = make_entry(); // PolicyApply
        assert!(matches_trigger(&entry, "apply"), "\"apply\" should match PolicyApply trigger");
    }

    /// AC: "dhcp" filter matches DhcpEvent trigger entries.
    #[test]
    fn test_matches_trigger_dhcp_matches_dhcp_event() {
        let mut entry = make_entry();
        entry.trigger =
            Trigger::DhcpEvent { policy_name: "eth0-dhcp".to_string(), event_kind: "acquired".to_string() };
        assert!(matches_trigger(&entry, "dhcp"), "\"dhcp\" should match DhcpEvent trigger");
    }

    /// AC: "external" filter matches ExternalChange trigger entries.
    #[test]
    fn test_matches_trigger_external_matches_external_change() {
        let mut entry = make_entry();
        entry.trigger = Trigger::ExternalChange { changed_entities: vec![] };
        assert!(
            matches_trigger(&entry, "external"),
            "\"external\" should match ExternalChange trigger"
        );
    }

    /// AC: "startup" filter matches DaemonStartup trigger entries.
    #[test]
    fn test_matches_trigger_startup_matches_daemon_startup() {
        let mut entry = make_entry();
        entry.trigger = Trigger::DaemonStartup;
        assert!(
            matches_trigger(&entry, "startup"),
            "\"startup\" should match DaemonStartup trigger"
        );
    }

    /// AC: "revert" filter matches Revert trigger entries.
    #[test]
    fn test_matches_trigger_revert_matches_revert() {
        let mut entry = make_entry();
        entry.trigger = Trigger::Revert { target_seq: 5 };
        assert!(matches_trigger(&entry, "revert"), "\"revert\" should match Revert trigger");
    }

    /// AC: trigger filter is case-insensitive.
    #[test]
    fn test_matches_trigger_is_case_insensitive() {
        let entry = make_entry(); // PolicyApply
        assert!(
            matches_trigger(&entry, "APPLY"),
            "trigger filter matching should be case insensitive"
        );
    }

    /// AC: non-matching trigger filter returns false.
    #[test]
    fn test_matches_trigger_non_matching_returns_false() {
        let entry = make_entry(); // PolicyApply
        assert!(
            !matches_trigger(&entry, "dhcp"),
            "\"dhcp\" should not match PolicyApply trigger"
        );
    }

    // ── matches_selector ─────────────────────────────────────────────────────

    /// AC: name=eth0 selector matches entry with eth0 in diff operations.
    #[test]
    fn test_matches_selector_name_eth0_matches_entry_with_eth0_in_diff() {
        let entry = make_entry_with_entity("eth0");
        let selectors = vec![("name".to_string(), "eth0".to_string())];
        assert!(
            matches_selector(&entry, &selectors),
            "name=eth0 should match entry that has eth0 in diff operations"
        );
    }

    /// AC: name=eth0 selector does not match entry with only eth1 in diff.
    #[test]
    fn test_matches_selector_name_eth0_does_not_match_entry_with_only_eth1() {
        let entry = make_entry_with_entity("eth1");
        let selectors = vec![("name".to_string(), "eth0".to_string())];
        assert!(
            !matches_selector(&entry, &selectors),
            "name=eth0 should not match entry with only eth1 in diff"
        );
    }

    /// AC: empty selectors list matches any entry.
    #[test]
    fn test_matches_selector_empty_selectors_matches_any_entry() {
        let entry = make_entry();
        assert!(matches_selector(&entry, &[]), "empty selectors should match any entry");
    }

    /// AC: selector against entry with no diff operations returns false.
    #[test]
    fn test_matches_selector_empty_diff_does_not_match_name_selector() {
        let entry = make_entry(); // empty diff
        let selectors = vec![("name".to_string(), "eth0".to_string())];
        assert!(
            !matches_selector(&entry, &selectors),
            "entry with no diff ops should not match a name selector"
        );
    }

    // ── filter_entries ────────────────────────────────────────────────────────

    /// AC: Filter by time -- only entries newer than cutoff are shown.
    #[test]
    fn test_filter_entries_since_1h_filters_out_older_entries() {
        let old_entry = {
            let mut e = make_entry();
            e.timestamp = Utc::now() - Duration::hours(2);
            e
        };
        let recent_entry = {
            let mut e = make_entry();
            e.timestamp = Utc::now() - Duration::minutes(30);
            e
        };

        let mut args = default_args();
        args.since = Some("1h".to_string());

        let result = filter_entries(vec![old_entry, recent_entry], &args).unwrap();
        assert_eq!(result.len(), 1, "only 1 entry should pass the since=1h filter");
    }

    /// AC: Filter by trigger type -- only matching trigger entries are returned.
    #[test]
    fn test_filter_entries_trigger_filter_shows_only_matching_entries() {
        let apply_entry = make_entry(); // PolicyApply
        let mut external_entry = make_entry();
        external_entry.trigger = Trigger::ExternalChange { changed_entities: vec![] };

        let mut args = default_args();
        args.trigger = Some("external".to_string());

        let result = filter_entries(vec![apply_entry, external_entry], &args).unwrap();
        assert_eq!(result.len(), 1, "only 1 entry should pass the --trigger external filter");
        assert!(
            matches!(result[0].trigger, Trigger::ExternalChange { .. }),
            "remaining entry should have ExternalChange trigger"
        );
    }

    /// AC: Filter by entity name -- only entries touching named entity are shown.
    #[test]
    fn test_filter_entries_selector_filter_shows_only_entities_matching_name() {
        let eth0_entry = make_entry_with_entity("eth0");
        let eth1_entry = make_entry_with_entity("eth1");

        let mut args = default_args();
        args.selector = vec![("name".to_string(), "eth0".to_string())];

        let result = filter_entries(vec![eth0_entry, eth1_entry], &args).unwrap();
        assert_eq!(result.len(), 1, "only 1 entry should pass the name=eth0 filter");
    }

    /// AC: Limit entry count -- -n 5 shows exactly 5 entries from a larger set.
    #[test]
    fn test_filter_entries_count_limits_number_of_results() {
        let entries: Vec<JournalEntry> = (0..10).map(|_| make_entry()).collect();

        let mut args = default_args();
        args.count = 5;

        let result = filter_entries(entries, &args).unwrap();
        assert_eq!(result.len(), 5, "count=5 should limit results to exactly 5 entries");
    }

    /// AC: Combine filters -- all three filters apply with AND logic.
    #[test]
    fn test_filter_entries_combined_filters_use_and_logic() {
        let all_match = {
            let mut e = make_entry_with_entity("eth0");
            e.timestamp = Utc::now() - Duration::minutes(30);
            e.trigger = Trigger::PolicyApply { source: "test.yaml".to_string() };
            e
        };
        let only_entity_matches = {
            let mut e = make_entry_with_entity("eth0");
            e.timestamp = Utc::now() - Duration::hours(2); // fails since filter
            e
        };
        let only_time_matches = {
            let mut e = make_entry(); // no eth0 in diff
            e.timestamp = Utc::now() - Duration::minutes(30);
            e
        };

        let mut args = default_args();
        args.since = Some("1h".to_string());
        args.selector = vec![("name".to_string(), "eth0".to_string())];

        let result =
            filter_entries(vec![all_match, only_entity_matches, only_time_matches], &args).unwrap();
        assert_eq!(
            result.len(),
            1,
            "combined filters (AND) should return only the entry matching all conditions"
        );
    }

    // ── format_text_list ──────────────────────────────────────────────────────

    /// AC: Text list output contains the header with all required column names.
    #[test]
    fn test_format_text_list_contains_header_with_all_column_names() {
        let output = format_text_list(&[make_entry()]);
        assert!(
            output.contains("SEQ")
                && output.contains("TIMESTAMP")
                && output.contains("TRIGGER")
                && output.contains("ENTITIES")
                && output.contains("CHANGES")
                && output.contains("OUTCOME"),
            "text list header should contain SEQ, TIMESTAMP, TRIGGER, ENTITIES, CHANGES, OUTCOME"
        );
    }

    /// AC: Text list shows exactly N data rows plus 1 header row.
    #[test]
    fn test_format_text_list_has_one_header_plus_one_row_per_entry() {
        let entries: Vec<JournalEntry> = (0..5).map(|_| make_entry()).collect();
        let output = format_text_list(&entries);
        let line_count = output.lines().count();
        assert_eq!(line_count, 6, "text list should have 1 header + 5 data rows = 6 lines total");
    }

    /// AC: Text list shows seq number for each entry.
    #[test]
    fn test_format_text_list_shows_seq_number_for_each_entry() {
        let mut entry = make_entry();
        entry.seq = 142;
        let output = format_text_list(&[entry]);
        assert!(output.contains("142"), "text list should show the entry's seq number");
    }

    /// AC: Empty entries list produces only the header row.
    #[test]
    fn test_format_text_list_empty_entries_produces_only_header() {
        let output = format_text_list(&[]);
        assert_eq!(
            output.lines().count(),
            1,
            "empty entries list should produce exactly 1 header row"
        );
    }

    // ── format_text_detail ────────────────────────────────────────────────────

    /// AC: Detail output shows "Entry #<seq>" header.
    #[test]
    fn test_format_text_detail_shows_entry_number_and_timestamp() {
        let mut entry = make_entry();
        entry.seq = 42;
        let output = format_text_detail(&entry);
        assert!(
            output.contains("Entry #42"),
            "detail output should contain 'Entry #42'"
        );
    }

    /// AC: Detail output shows trigger type and source details.
    #[test]
    fn test_format_text_detail_shows_trigger_with_source() {
        let mut entry = make_entry();
        entry.trigger = Trigger::PolicyApply { source: "/etc/netfyr/policies/".to_string() };
        let output = format_text_detail(&entry);
        assert!(
            output.contains("Trigger:") && output.contains("policy-apply"),
            "detail output should show 'Trigger: policy-apply'"
        );
        assert!(
            output.contains("/etc/netfyr/policies/"),
            "detail output should show the trigger source path"
        );
    }

    /// AC: Detail output shows active policies section.
    #[test]
    fn test_format_text_detail_shows_active_policies_section() {
        let mut entry = make_entry();
        entry.active_policies = vec![PolicySummary {
            name: "eth0-config".to_string(),
            factory_type: "static".to_string(),
            priority: 100,
        }];
        let output = format_text_detail(&entry);
        assert!(
            output.contains("Active policies:") && output.contains("eth0-config"),
            "detail output should show 'Active policies:' with policy names"
        );
        assert!(output.contains("static"), "detail output should show factory type");
        assert!(output.contains("100"), "detail output should show priority");
    }

    /// AC: Detail output shows diff section with field changes.
    #[test]
    fn test_format_text_detail_shows_diff_with_field_changes() {
        let mut entry = make_entry();
        entry.diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth0".to_string(),
                field_changes: vec![SerializableFieldChange {
                    field_name: "mtu".to_string(),
                    change_kind: "set".to_string(),
                    current: Some(serde_json::json!(1500u64)),
                    desired: Some(serde_json::json!(9000u64)),
                }],
            }],
        };
        let output = format_text_detail(&entry);
        assert!(
            output.contains("Diff:") && output.contains("ethernet") && output.contains("eth0"),
            "detail output should show diff section with entity type and name"
        );
        assert!(output.contains("mtu"), "detail output should show the changed field name");
    }

    /// AC: Detail output shows outcome section.
    #[test]
    fn test_format_text_detail_shows_outcome_section() {
        let mut entry = make_entry();
        entry.outcome = ApplyOutcome::Applied { succeeded: 1, failed: 0, skipped: 0 };
        let output = format_text_detail(&entry);
        assert!(
            output.contains("Outcome:") && output.contains("applied"),
            "detail output should contain 'Outcome:' with outcome description"
        );
    }

    /// AC: Detail output shows state snapshot after the change.
    #[test]
    fn test_format_text_detail_shows_state_after_section() {
        let mut entry = make_entry();
        entry.state_after = SerializableStateSet {
            entities: vec![SerializableState {
                entity_type: "ethernet".to_string(),
                selector_name: "eth0".to_string(),
                fields: serde_json::json!({ "mtu": 9000u64 }),
            }],
        };
        let output = format_text_detail(&entry);
        assert!(
            output.contains("State after:"),
            "detail output should contain 'State after:' section"
        );
    }

    // ── format_json_list ──────────────────────────────────────────────────────

    /// AC: JSON list output is a valid JSON array with N elements.
    #[test]
    fn test_format_json_list_produces_valid_json_array_with_correct_count() {
        let mut entries: Vec<JournalEntry> = (0..5).map(|_| make_entry()).collect();
        for (i, e) in entries.iter_mut().enumerate() {
            e.seq = (i + 1) as u64;
        }

        let json_str = format_json_list(&entries).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("JSON list should be valid JSON");
        assert!(parsed.is_array(), "JSON list output should be a JSON array");
        assert_eq!(
            parsed.as_array().unwrap().len(),
            5,
            "JSON array should contain exactly 5 elements"
        );
    }

    /// AC: Each JSON list element has the JournalEntry structure (seq, timestamp, trigger, diff, outcome).
    #[test]
    fn test_format_json_list_each_element_has_journal_entry_fields() {
        let mut entry = make_entry();
        entry.seq = 42;

        let json_str = format_json_list(&[entry]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let elem = &parsed.as_array().unwrap()[0];

        assert!(elem.get("seq").is_some(), "element should have 'seq' field");
        assert!(elem.get("timestamp").is_some(), "element should have 'timestamp' field");
        assert!(elem.get("trigger").is_some(), "element should have 'trigger' field");
        assert!(elem.get("diff").is_some(), "element should have 'diff' field");
        assert!(elem.get("outcome").is_some(), "element should have 'outcome' field");
        assert_eq!(elem["seq"].as_u64(), Some(42), "seq field should match the entry's seq");
    }

    // ── format_json_detail ────────────────────────────────────────────────────

    /// AC: JSON detail output is a valid JSON object representing the entry.
    #[test]
    fn test_format_json_detail_produces_valid_json_object_with_correct_seq() {
        let mut entry = make_entry();
        entry.seq = 42;

        let json_str = format_json_detail(&entry).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("JSON detail should be valid JSON");
        assert!(parsed.is_object(), "JSON detail output should be a JSON object");
        assert_eq!(
            parsed["seq"].as_u64(),
            Some(42),
            "JSON detail should contain the correct seq number"
        );
    }

    // ── entities_summary ─────────────────────────────────────────────────────

    /// AC: Empty diff operations produces "(none)".
    #[test]
    fn test_entities_summary_empty_ops_returns_none_string() {
        assert_eq!(entities_summary(&[]), "(none)", "empty ops should produce '(none)'");
    }

    /// AC: Single entity produces just the entity name.
    #[test]
    fn test_entities_summary_single_entity_returns_name() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![],
        }];
        assert_eq!(entities_summary(&ops), "eth0");
    }

    /// AC: Two entities produce comma-separated names.
    #[test]
    fn test_entities_summary_two_entities_returns_comma_separated() {
        let ops = vec![
            SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth0".to_string(),
                field_changes: vec![],
            },
            SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth1".to_string(),
                field_changes: vec![],
            },
        ];
        assert_eq!(entities_summary(&ops), "eth0, eth1");
    }

    /// AC: More than 3 entities shows "+N more" truncation.
    #[test]
    fn test_entities_summary_more_than_3_entities_truncated_with_plus_n_more() {
        let ops: Vec<SerializableDiffOp> = (0..5)
            .map(|i| SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: format!("eth{}", i),
                field_changes: vec![],
            })
            .collect();
        let result = entities_summary(&ops);
        assert!(
            result.contains("+3 more"),
            "5 entities should truncate with '+3 more', got: {}",
            result
        );
    }

    // ── changes_summary ───────────────────────────────────────────────────────

    /// AC: Empty ops produces "(none)".
    #[test]
    fn test_changes_summary_empty_ops_returns_none_string() {
        assert_eq!(changes_summary(&[]), "(none)");
    }

    /// AC: Single add op shows "+entity".
    #[test]
    fn test_changes_summary_single_add_op_returns_plus_entity() {
        let ops = vec![SerializableDiffOp {
            kind: "add".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![],
        }];
        assert_eq!(changes_summary(&ops), "+entity");
    }

    /// AC: Multiple add ops show "+N entities".
    #[test]
    fn test_changes_summary_multiple_add_ops_returns_plus_n_entities() {
        let ops: Vec<SerializableDiffOp> = (0..3)
            .map(|i| SerializableDiffOp {
                kind: "add".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: format!("eth{}", i),
                field_changes: vec![],
            })
            .collect();
        assert_eq!(changes_summary(&ops), "+3 entities");
    }

    /// AC: Single remove op shows "-entity".
    #[test]
    fn test_changes_summary_single_remove_op_returns_minus_entity() {
        let ops = vec![SerializableDiffOp {
            kind: "remove".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![],
        }];
        assert_eq!(changes_summary(&ops), "-entity");
    }

    /// AC: Field modification (set with current) shows "~field".
    #[test]
    fn test_changes_summary_modify_with_existing_field_shows_tilde_field() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "mtu".to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::json!(1500u64)),
                desired: Some(serde_json::json!(9000u64)),
            }],
        }];
        let result = changes_summary(&ops);
        assert!(result.contains("~mtu"), "field modification should show '~mtu', got: {}", result);
    }

    /// AC: New field added (set without current) shows "+field".
    #[test]
    fn test_changes_summary_new_field_set_shows_plus_field() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "addr".to_string(),
                change_kind: "set".to_string(),
                current: None,
                desired: Some(serde_json::json!("10.0.0.1/24")),
            }],
        }];
        let result = changes_summary(&ops);
        assert!(result.contains("+addr"), "new field should show '+addr', got: {}", result);
    }

    /// AC: Field removed (unset) shows "-field".
    #[test]
    fn test_changes_summary_unset_field_shows_minus_field() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "addr".to_string(),
                change_kind: "unset".to_string(),
                current: Some(serde_json::json!("10.0.0.1/24")),
                desired: None,
            }],
        }];
        let result = changes_summary(&ops);
        assert!(result.contains("-addr"), "unset field should show '-addr', got: {}", result);
    }

    // ── outcome_summary ───────────────────────────────────────────────────────

    /// AC: Applied outcome with multiple counts shows "applied (N ok, N failed)".
    #[test]
    fn test_outcome_summary_applied_with_mixed_counts_includes_all_nonzero() {
        let outcome = ApplyOutcome::Applied { succeeded: 2, failed: 1, skipped: 0 };
        let result = outcome_summary(&outcome);
        assert!(result.contains("applied"), "should say 'applied'");
        assert!(result.contains("2 ok"), "should show '2 ok'");
        assert!(result.contains("1 failed"), "should show '1 failed'");
        assert!(!result.contains("skipped"), "should not show 'skipped' when skipped=0");
    }

    /// AC: Applied with only successes shows "applied (N ok)" without failed/skipped.
    #[test]
    fn test_outcome_summary_applied_with_only_successes_omits_zero_counts() {
        let outcome = ApplyOutcome::Applied { succeeded: 3, failed: 0, skipped: 0 };
        let result = outcome_summary(&outcome);
        assert!(result.contains("3 ok"), "should show '3 ok'");
        assert!(!result.contains("failed"), "should not show 'failed' when count is 0");
        assert!(!result.contains("skipped"), "should not show 'skipped' when count is 0");
    }

    /// AC: Observed outcome produces "observed".
    #[test]
    fn test_outcome_summary_observed_returns_observed() {
        assert_eq!(outcome_summary(&ApplyOutcome::Observed), "observed");
    }

    // ── trigger_display_name ──────────────────────────────────────────────────

    /// AC: All trigger variants produce the correct display name.
    #[test]
    fn test_trigger_display_name_all_variants_return_correct_names() {
        assert_eq!(
            trigger_display_name(&Trigger::PolicyApply { source: "x".to_string() }),
            "policy-apply"
        );
        assert_eq!(
            trigger_display_name(&Trigger::DhcpEvent {
                policy_name: "x".to_string(),
                event_kind: "y".to_string()
            }),
            "dhcp-lease"
        );
        assert_eq!(
            trigger_display_name(&Trigger::ExternalChange { changed_entities: vec![] }),
            "external"
        );
        assert_eq!(trigger_display_name(&Trigger::DaemonStartup), "daemon-startup");
        assert_eq!(trigger_display_name(&Trigger::Revert { target_seq: 1 }), "revert");
    }

    // ── Integration tests via run_history_local ────────────────────────────────

    /// AC: Journal directory does not exist → exit code 1.
    #[tokio::test]
    async fn test_run_history_local_no_journal_dir_returns_exit_code_1() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let nonexistent = format!(
            "/tmp/netfyr-definitely-does-not-exist-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        // Safety: protected by ENV_MUTEX
        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", &nonexistent) };
        let result = run_history_local(&default_args()).await.unwrap();
        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };

        assert_eq!(
            result,
            ExitCode::from(1u8),
            "should return exit code 1 when journal directory does not exist"
        );
    }

    /// AC: Empty journal → exit code 0 (prints "No journal entries found.").
    #[tokio::test]
    async fn test_run_history_local_empty_journal_returns_exit_code_0() {
        let dir = temp_dir();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let _journal = Journal::open(&dir).unwrap();

        // Safety: protected by ENV_MUTEX
        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", dir.to_str().unwrap()) };
        let result = run_history_local(&default_args()).await.unwrap();
        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            result,
            ExitCode::from(0u8),
            "empty journal should return exit code 0"
        );
    }

    /// AC: Show nonexistent entry → exit code 1.
    #[tokio::test]
    async fn test_run_history_local_show_nonexistent_entry_returns_exit_code_1() {
        let dir = temp_dir();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let _journal = Journal::open(&dir).unwrap();

        // Safety: protected by ENV_MUTEX
        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", dir.to_str().unwrap()) };
        let mut args = default_args();
        args.show = Some(9999);
        let result = run_history_local(&args).await.unwrap();
        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            result,
            ExitCode::from(1u8),
            "--show 9999 should return exit code 1 when that entry does not exist"
        );
    }

    /// AC: Show existing entry → exit code 0.
    #[tokio::test]
    async fn test_run_history_local_show_existing_entry_returns_exit_code_0() {
        let dir = temp_dir();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let mut journal = Journal::open(&dir).unwrap();
        journal.append(make_entry()).unwrap(); // gets seq=1

        // Safety: protected by ENV_MUTEX
        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", dir.to_str().unwrap()) };
        let mut args = default_args();
        args.show = Some(1);
        let result = run_history_local(&args).await.unwrap();
        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            result,
            ExitCode::from(0u8),
            "--show 1 should return exit code 0 when entry exists"
        );
    }

    /// AC: List recent entries -- 30 entries, default count=20 → exit code 0.
    #[tokio::test]
    async fn test_run_history_local_with_30_entries_and_default_count_returns_exit_code_0() {
        let dir = temp_dir();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let mut journal = Journal::open(&dir).unwrap();
        for _ in 0..30 {
            journal.append(make_entry()).unwrap();
        }

        // Safety: protected by ENV_MUTEX
        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", dir.to_str().unwrap()) };
        let args = default_args(); // count=20
        let result = run_history_local(&args).await.unwrap();
        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            result,
            ExitCode::from(0u8),
            "listing entries with count=20 from 30-entry journal should succeed"
        );
    }

    /// AC: Read recent returns entries in reverse chronological order (most recent first).
    #[test]
    fn test_read_recent_returns_most_recent_entries_first_via_journal() {
        let dir = temp_dir();
        let mut journal = Journal::open(&dir).unwrap();

        for _ in 0..5 {
            journal.append(make_entry()).unwrap();
        }

        let entries = journal.read_recent(5).unwrap();
        assert_eq!(entries.len(), 5);
        for i in 0..entries.len() - 1 {
            assert!(
                entries[i].seq > entries[i + 1].seq,
                "entries should be in reverse order: [{}].seq={} > [{}].seq={}",
                i, entries[i].seq, i+1, entries[i+1].seq
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// AC: read_recent with -n 5 returns exactly 5 from a 30-entry journal.
    #[test]
    fn test_read_recent_with_n_5_returns_exactly_5_from_30_entry_journal() {
        let dir = temp_dir();
        let mut journal = Journal::open(&dir).unwrap();

        for _ in 0..30 {
            journal.append(make_entry()).unwrap();
        }

        let entries = journal.read_recent(5).unwrap();
        assert_eq!(entries.len(), 5, "read_recent(5) on 30-entry journal should return exactly 5");

        std::fs::remove_dir_all(&dir).ok();
    }

    // ── filter_entries count without other filters ─────────────────────────────

    /// AC: filter_entries with count=20 and no other filters returns exactly 20 from 30 entries.
    #[test]
    fn test_filter_entries_default_count_20_returns_20_from_30_entries_without_filters() {
        let entries: Vec<JournalEntry> = (0..30).map(|_| make_entry()).collect();

        let args = default_args(); // count=20, no other filters
        let result = filter_entries(entries, &args).unwrap();
        assert_eq!(
            result.len(),
            20,
            "filter_entries with count=20 and no filters should return exactly 20 entries"
        );
    }

    // ── changes_summary: list field notation ──────────────────────────────────

    /// AC: List field with only additions shows "field(+N)" notation.
    #[test]
    fn test_changes_summary_list_field_additions_shows_plus_n_notation() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "addresses".to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::json!([])),
                desired: Some(serde_json::json!(["10.0.0.1/24", "10.0.0.2/24"])),
            }],
        }];
        let result = changes_summary(&ops);
        assert!(
            result.contains("addresses(+2)"),
            "list field with 2 added items should show 'addresses(+2)', got: {}",
            result
        );
    }

    /// AC: List field with only removals shows "field(-N)" notation.
    #[test]
    fn test_changes_summary_list_field_removals_shows_minus_n_notation() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "addresses".to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::json!(["10.0.0.1/24"])),
                desired: Some(serde_json::json!([])),
            }],
        }];
        let result = changes_summary(&ops);
        assert!(
            result.contains("addresses(-1)"),
            "list field with 1 removed item should show 'addresses(-1)', got: {}",
            result
        );
    }

    /// AC: List field with both additions and removals shows "field(+N,-M)" notation.
    #[test]
    fn test_changes_summary_list_field_additions_and_removals_shows_combined_notation() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "addresses".to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::json!(["192.168.1.1/24"])),
                desired: Some(serde_json::json!(["10.0.0.1/24", "10.0.0.2/24"])),
            }],
        }];
        let result = changes_summary(&ops);
        assert!(
            result.contains("addresses(+2,-1)"),
            "list field with 2 added and 1 removed should show 'addresses(+2,-1)', got: {}",
            result
        );
    }

    /// AC: List field with no changes (same content) produces "(no changes)".
    #[test]
    fn test_changes_summary_list_field_unchanged_content_produces_no_changes() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "addresses".to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::json!(["10.0.0.1/24"])),
                desired: Some(serde_json::json!(["10.0.0.1/24"])),
            }],
        }];
        let result = changes_summary(&ops);
        assert_eq!(
            result, "(no changes)",
            "list field with identical current and desired should produce '(no changes)'"
        );
    }

    // ── format_text_detail: empty diff ────────────────────────────────────────

    /// AC: Detail output with empty diff shows "(no changes)" in the Diff section.
    #[test]
    fn test_format_text_detail_empty_diff_shows_no_changes() {
        let entry = make_entry(); // diff has empty operations
        let output = format_text_detail(&entry);
        assert!(
            output.contains("Diff:"),
            "detail output should always contain 'Diff:' section"
        );
        assert!(
            output.contains("(no changes)"),
            "detail output with empty diff should show '(no changes)'"
        );
    }

    // ── format_text_detail: no active policies ────────────────────────────────

    /// AC: Detail output with no active policies does not show "Active policies:" section.
    #[test]
    fn test_format_text_detail_no_active_policies_omits_policies_section() {
        let entry = make_entry(); // active_policies is empty
        let output = format_text_detail(&entry);
        assert!(
            !output.contains("Active policies:"),
            "detail output with no active policies should not show 'Active policies:' section"
        );
    }

    // ── format_text_list: row data correctness ────────────────────────────────

    /// AC: Each row in the text list shows the trigger display name.
    #[test]
    fn test_format_text_list_row_shows_trigger_display_name() {
        let mut entry = make_entry();
        entry.seq = 1;
        entry.trigger = Trigger::ExternalChange { changed_entities: vec![] };
        let output = format_text_list(&[entry]);
        let data_row = output.lines().nth(1).unwrap();
        assert!(
            data_row.contains("external"),
            "data row should show trigger display name 'external', got: {}",
            data_row
        );
    }

    /// AC: Each row in the text list shows the entity name from the diff.
    #[test]
    fn test_format_text_list_row_shows_entity_name_from_diff() {
        let entry = make_entry_with_entity("eth0");
        let output = format_text_list(&[entry]);
        let data_row = output.lines().nth(1).unwrap();
        assert!(
            data_row.contains("eth0"),
            "data row should show entity name 'eth0', got: {}",
            data_row
        );
    }

    /// AC: Each row in the text list shows the outcome description.
    #[test]
    fn test_format_text_list_row_shows_outcome_description() {
        let mut entry = make_entry();
        entry.outcome = ApplyOutcome::Applied { succeeded: 2, failed: 0, skipped: 0 };
        let output = format_text_list(&[entry]);
        let data_row = output.lines().nth(1).unwrap();
        assert!(
            data_row.contains("applied"),
            "data row should show outcome 'applied', got: {}",
            data_row
        );
    }

    // ── journal_dir_path ──────────────────────────────────────────────────────

    /// AC: journal_dir_path returns the NETFYR_JOURNAL_DIR env var value when set.
    #[test]
    fn test_journal_dir_path_returns_netfyr_journal_dir_env_var_when_set() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        // Safety: protected by ENV_MUTEX
        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", "/custom/journal/path") };
        let path = journal_dir_path();
        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };
        assert_eq!(
            path, "/custom/journal/path",
            "journal_dir_path should return the NETFYR_JOURNAL_DIR env var value"
        );
    }

    /// AC: journal_dir_path returns default "/var/lib/netfyr/journal" when env var is not set.
    #[test]
    fn test_journal_dir_path_returns_default_when_env_var_not_set() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };
        let path = journal_dir_path();
        assert_eq!(
            path, "/var/lib/netfyr/journal",
            "journal_dir_path should default to '/var/lib/netfyr/journal'"
        );
    }

    // ── changes_summary: mixed entity add/remove ──────────────────────────────

    /// AC: Mix of add and remove entity ops shows individual "+name" and "-name" tokens.
    #[test]
    fn test_changes_summary_mixed_add_and_remove_entity_ops_shows_individual_tokens() {
        let ops = vec![
            SerializableDiffOp {
                kind: "add".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth2".to_string(),
                field_changes: vec![],
            },
            SerializableDiffOp {
                kind: "remove".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth3".to_string(),
                field_changes: vec![],
            },
        ];
        let result = changes_summary(&ops);
        assert!(
            result.contains("+eth2"),
            "mixed add/remove should show '+eth2', got: {}",
            result
        );
        assert!(
            result.contains("-eth3"),
            "mixed add/remove should show '-eth3', got: {}",
            result
        );
    }

    // ── parse_since: edge cases ───────────────────────────────────────────────

    /// AC: --since 0s is valid and returns approximately now.
    #[test]
    fn test_parse_since_0s_is_valid_and_returns_approximately_now() {
        let before = Utc::now();
        let result = parse_since("0s").unwrap();
        let after = Utc::now();
        assert!(
            result >= before - Duration::seconds(1) && result <= after + Duration::seconds(1),
            "parse_since(\"0s\") should return approximately now"
        );
    }

    /// AC: --since with empty string returns an error.
    #[test]
    fn test_parse_since_empty_string_returns_error() {
        assert!(parse_since("").is_err(), "empty string should return error");
    }
}
