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
use indexmap::IndexMap;

use netfyr_journal::{ApplyOutcome, Journal, JournalEntry, SerializableDiffOp, SerializableFieldChange, SerializableState, Trigger};
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

    /// Show full detail for a single entry by sequence ID.
    /// Positive values are absolute sequence numbers.
    /// Negative values count from the end: -1 is the most recent entry.
    #[arg(long, allow_hyphen_values = true)]
    pub show: Option<i64>,

    /// Output format: text (default), json
    #[arg(long, short = 'o', default_value = "text")]
    pub output: HistoryOutputFormat,

    /// Show full timestamps (YYYY-MM-DD HH:MM:SS) instead of relative/abbreviated
    #[arg(long)]
    pub absolute_timestamps: bool,
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
        if seq > 0 {
            let entry = journal
                .read_entry(seq as u64)
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
        } else if seq < 0 {
            // Negative offset: -1 is most recent, -2 is second-to-last, etc.
            // read_recent(k) returns entries newest-first; the last element is the k-th-to-last.
            let k = seq.unsigned_abs() as usize;
            let entries = journal
                .read_recent(k)
                .context("failed to read journal entries for negative offset")?;
            if entries.len() < k {
                eprintln!("Entry not found");
                return Ok(ExitCode::from(1u8));
            }
            let e = entries.into_iter().last().expect("entries.len() == k >= 1");
            print_detail(&e, &args.output)?;
            return Ok(ExitCode::from(0u8));
        } else {
            eprintln!("Entry #0 not found");
            return Ok(ExitCode::from(1u8));
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

    print_list(&entries, &args.output, args.absolute_timestamps)?;
    Ok(ExitCode::from(0u8))
}

// ── Daemon mode ───────────────────────────────────────────────────────────────

async fn run_history_daemon(
    client: &mut VarlinkClient,
    args: &HistoryArgs,
) -> Result<ExitCode> {
    if let Some(seq) = args.show {
        if seq > 0 {
            let raw = client
                .get_journal_entry(seq as u64)
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
        } else if seq < 0 {
            let k = seq.unsigned_abs() as usize;
            let raw_entries = client
                .get_history(Some(k), None, None, None)
                .await
                .context("failed to get history from daemon for negative offset")?;
            if raw_entries.len() < k {
                eprintln!("Entry not found");
                return Ok(ExitCode::from(1u8));
            }
            let last_value = raw_entries.into_iter().last().expect("len == k >= 1");
            let entry: JournalEntry = serde_json::from_value(last_value)
                .context("failed to deserialize journal entry from daemon")?;
            print_detail(&entry, &args.output)?;
            return Ok(ExitCode::from(0u8));
        } else {
            eprintln!("Entry #0 not found");
            return Ok(ExitCode::from(1u8));
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

    let mut entries: Vec<JournalEntry> = raw_entries
        .into_iter()
        .map(|v| serde_json::from_value(v).context("failed to deserialize journal entry"))
        .collect::<Result<Vec<_>>>()?;

    // The Varlink API only accepts a single selector_name filter. When multiple
    // selectors were provided, apply the remaining ones client-side so daemon
    // mode is behaviourally identical to local mode.
    if args.selector.len() > 1 {
        entries.retain(|e| matches_selector(e, &args.selector));
    }

    print_list(&entries, &args.output, args.absolute_timestamps)?;
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

fn print_list(
    entries: &[JournalEntry],
    format: &HistoryOutputFormat,
    absolute_timestamps: bool,
) -> Result<()> {
    match format {
        HistoryOutputFormat::Text => print!("{}", format_text_list(entries, absolute_timestamps)),
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

const SYSTEM_ENTITY_TYPES: &[&str] = &["dns", "hostname", "ntp"];

fn entity_display_name(op: &SerializableDiffOp) -> String {
    if SYSTEM_ENTITY_TYPES.contains(&op.entity_type.as_str()) {
        format!("sys:{}", op.entity_type)
    } else {
        op.entity_name.clone()
    }
}

fn state_entity_display_name(state: &SerializableState) -> String {
    if SYSTEM_ENTITY_TYPES.contains(&state.entity_type.as_str()) {
        format!("sys:{}", state.entity_type)
    } else {
        state.selector_name.clone()
    }
}

fn format_timestamp(ts: DateTime<Utc>, now: DateTime<Utc>, absolute: bool) -> String {
    if absolute {
        return ts.format("%Y-%m-%d %H:%M:%S").to_string();
    }
    let ts_date = ts.date_naive();
    let now_date = now.date_naive();
    if ts_date == now_date {
        let secs = (now - ts).num_seconds().max(0);
        if secs < 60 {
            format!("{} sec ago", secs)
        } else if secs < 3600 {
            format!("{} min ago", secs / 60)
        } else {
            format!("{}h ago", secs / 3600)
        }
    } else if ts_date == now_date - chrono::Duration::days(1) {
        format!("yesterday {}", ts.format("%H:%M"))
    } else {
        ts.format("%Y-%m-%d %H:%M").to_string()
    }
}

fn format_trigger_column(entry: &JournalEntry) -> String {
    match &entry.trigger {
        Trigger::PolicyApply { .. } => match entry.active_policies.as_slice() {
            [] => "apply".to_string(),
            [p] => format!("apply ({})", p.name),
            [p, rest @ ..] => format!("apply ({}, +{})", p.name, rest.len()),
        },
        Trigger::DhcpEvent { event_kind, .. } => match event_kind.as_str() {
            "lease_acquired" => "dhcp-acquire".to_string(),
            "lease_renewed" => "dhcp-renew".to_string(),
            "lease_expired" => "dhcp-expire".to_string(),
            k => format!("dhcp-{}", k),
        },
        Trigger::ExternalChange { .. } => "external".to_string(),
        Trigger::DaemonStartup => "daemon-startup".to_string(),
        Trigger::Revert { target_seq } => format!("revert ({})", target_seq),
    }
}

fn format_trigger_column_fitted(entry: &JournalEntry, max_width: usize) -> String {
    if let Trigger::PolicyApply { .. } = &entry.trigger {
        let policies = &entry.active_policies;
        let total = policies.len();
        if total > 1 {
            let prefix = "apply (";
            for show in (1..=total).rev() {
                let names: Vec<&str> = policies[..show].iter().map(|p| p.name.as_str()).collect();
                let rest = total - show;
                let candidate = if rest == 0 {
                    format!("{}{})", prefix, names.join(", "))
                } else {
                    format!("{}{}, +{})", prefix, names.join(", "), rest)
                };
                if candidate.chars().count() <= max_width {
                    return candidate;
                }
            }
            let first_with_count = format!("apply ({}, +{})", policies[0].name, total - 1);
            if first_with_count.chars().count() > max_width {
                let suffix = format!(", +{})", total - 1);
                let name_budget = max_width.saturating_sub(prefix.len() + suffix.len());
                if name_budget >= 2 {
                    let name_chars: Vec<char> = policies[0].name.chars().collect();
                    let fitted_name = format!(
                        "{}…",
                        name_chars[..name_budget - 1].iter().collect::<String>()
                    );
                    let candidate = format!("{}{}{}", prefix, fitted_name, suffix);
                    if candidate.chars().count() <= max_width {
                        return candidate;
                    }
                }
            }
            let count_only = format!("apply (+{})", total);
            if count_only.chars().count() <= max_width {
                return count_only;
            }
        } else if total == 1 {
            let full = format!("apply ({})", policies[0].name);
            if full.chars().count() <= max_width {
                return full;
            }
            let prefix = "apply (";
            let suffix = ")";
            let name_budget = max_width.saturating_sub(prefix.len() + suffix.len());
            if name_budget >= 2 {
                let name_chars: Vec<char> = policies[0].name.chars().collect();
                let fitted_name = format!(
                    "{}…",
                    name_chars[..name_budget - 1].iter().collect::<String>()
                );
                let candidate = format!("{}{}{}", prefix, fitted_name, suffix);
                if candidate.chars().count() <= max_width {
                    return candidate;
                }
            }
            let count_only = format!("apply (+{})", total);
            if count_only.chars().count() <= max_width {
                return count_only;
            }
        } else {
            return "apply".to_string();
        }
    }
    let full = format_trigger_column(entry);
    if full.chars().count() <= max_width {
        return full;
    }
    pad_or_truncate(&full, max_width)
}

fn pad_or_truncate(s: &str, width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() > width {
        if width == 0 {
            return String::new();
        }
        chars[..width - 1].iter().collect::<String>() + "…"
    } else {
        let padding = " ".repeat(width - chars.len());
        format!("{}{}", s, padding)
    }
}

fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars || max_chars == 0 {
        s.to_string()
    } else {
        chars[..max_chars - 1].iter().collect::<String>() + "…"
    }
}

fn is_link_local(addr: &str) -> bool {
    let bare = addr.split('/').next().unwrap_or(addr);
    bare.starts_with("fe80:") || bare.starts_with("169.254.")
}

fn is_default_route(dest: &str) -> bool {
    dest == "0.0.0.0/0" || dest == "::/0"
}

fn json_display_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
        _ => serde_json::to_string(v).unwrap_or_else(|_| "?".to_string()),
    }
}

fn format_address_changes(added: Vec<&str>, removed: Vec<&str>) -> Vec<String> {
    let mut parts = Vec::new();
    if added.is_empty() && removed.is_empty() {
        return parts;
    }
    let total = added.len() + removed.len();

    let mut added_sorted: Vec<&str> = added;
    added_sorted.sort_by_key(|a| if is_link_local(a) { 1 } else { 0 });
    let mut removed_sorted: Vec<&str> = removed;
    removed_sorted.sort_by_key(|a| if is_link_local(a) { 1 } else { 0 });

    if total >= 9 {
        if !added_sorted.is_empty() {
            parts.push(format!("+{} addrs", added_sorted.len()));
        }
        if !removed_sorted.is_empty() {
            parts.push(format!("-{} addrs", removed_sorted.len()));
        }
        return parts;
    }

    if total <= 2 {
        for addr in &added_sorted {
            parts.push(format!("+{}", addr));
        }
        for addr in &removed_sorted {
            parts.push(format!("-{}", addr));
        }
        return parts;
    }

    // 3-8: show first 2 additions by value, count rest; first 1 removal, count rest
    if !added_sorted.is_empty() {
        let show = 2.min(added_sorted.len());
        for addr in &added_sorted[..show] {
            parts.push(format!("+{}", addr));
        }
        let rem = added_sorted.len() - show;
        if rem > 0 {
            parts.push(format!("(+{} addrs)", rem));
        }
    }
    if !removed_sorted.is_empty() {
        parts.push(format!("-{}", removed_sorted[0]));
        let rem = removed_sorted.len() - 1;
        if rem > 0 {
            parts.push(format!("(-{} addrs)", rem));
        }
    }
    parts
}

fn format_route_changes(
    added_routes: Vec<&serde_json::Value>,
    removed_routes: Vec<&serde_json::Value>,
) -> Vec<String> {
    let mut parts = Vec::new();
    let mut added_dflt: Vec<&serde_json::Value> = Vec::new();
    let mut added_nondflt: Vec<&serde_json::Value> = Vec::new();
    for r in &added_routes {
        let dest = r.as_object()
            .and_then(|o| o.get("destination"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if is_default_route(dest) {
            added_dflt.push(r);
        } else {
            added_nondflt.push(r);
        }
    }
    let mut removed_dflt: Vec<&serde_json::Value> = Vec::new();
    let mut removed_nondflt: Vec<&serde_json::Value> = Vec::new();
    for r in &removed_routes {
        let dest = r.as_object()
            .and_then(|o| o.get("destination"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if is_default_route(dest) {
            removed_dflt.push(r);
        } else {
            removed_nondflt.push(r);
        }
    }
    for r in &added_dflt {
        let gw = r.as_object()
            .and_then(|o| o.get("gateway"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        parts.push(format!("+dflt via {}", gw));
    }
    for r in &removed_dflt {
        let gw = r.as_object()
            .and_then(|o| o.get("gateway"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        parts.push(format!("-dflt via {}", gw));
    }
    let n_add = added_nondflt.len();
    let n_rem = removed_nondflt.len();
    if n_add > 0 {
        parts.push(format!("+{} {}", n_add, if n_add == 1 { "route" } else { "routes" }));
    }
    if n_rem > 0 {
        parts.push(format!("-{} {}", n_rem, if n_rem == 1 { "route" } else { "routes" }));
    }
    parts
}

fn format_dns_changes(added: Vec<&str>, removed: Vec<&str>) -> Vec<String> {
    let mut parts = Vec::new();
    for addr in &added {
        parts.push(format!("+ns {}", addr));
    }
    for addr in &removed {
        parts.push(format!("-ns {}", addr));
    }
    parts
}

struct RowCells {
    seq: String,
    ts: String,
    entities: String,
    changes: String,
    is_daemon_startup: bool,
}

pub fn format_text_list(entries: &[JournalEntry], absolute_timestamps: bool) -> String {
    format_text_list_with_width(entries, absolute_timestamps, get_terminal_width())
}

fn format_text_list_with_width(
    entries: &[JournalEntry],
    absolute_timestamps: bool,
    term_width: usize,
) -> String {
    const MAX_TERM_WIDTH: usize = 200;
    const MIN_TRIG: usize = 12;
    const MIN_ENT: usize = 10;
    const MIN_CHANGES: usize = 15;

    let effective_width = term_width.min(MAX_TERM_WIDTH);
    let now = Utc::now();

    let rows: Vec<RowCells> = entries
        .iter()
        .map(|e| RowCells {
            seq: e.seq.to_string(),
            ts: format_timestamp(e.timestamp, now, absolute_timestamps),
            entities: entities_summary_with_state(&e.diff.operations, &e.state_after.entities),
            changes: changes_column(e),
            is_daemon_startup: matches!(e.trigger, Trigger::DaemonStartup),
        })
        .collect();

    let cw = |s: &str| s.chars().count();

    let w_seq = cw("SEQ").max(rows.iter().map(|r| cw(&r.seq)).max().unwrap_or(0));
    let w_ts = cw("TIMESTAMP").max(rows.iter().map(|r| cw(&r.ts)).max().unwrap_or(0));

    let ideal_trig = cw("TRIGGER").max(
        entries
            .iter()
            .map(|e| cw(&format_trigger_column_fitted(e, usize::MAX)))
            .max()
            .unwrap_or(0),
    );
    let ideal_ent = cw("ENTITIES").max(rows.iter().map(|r| cw(&r.entities)).max().unwrap_or(0));
    let ideal_changes = cw("CHANGES").max(rows.iter().map(|r| cw(&r.changes)).max().unwrap_or(0));

    let separator_overhead = 4 * 2; // 4 separators of 2 spaces each
    let available = effective_width
        .saturating_sub(w_seq + w_ts + separator_overhead)
        .max(MIN_TRIG + MIN_ENT + MIN_CHANGES);

    let ideal_total = ideal_trig + ideal_ent + ideal_changes;
    let (w_trig, w_ent, w_changes) = if ideal_total <= available {
        (ideal_trig, ideal_ent, ideal_changes)
    } else {
        let remaining = available.saturating_sub(MIN_TRIG + MIN_ENT + MIN_CHANGES);
        let excess_trig = ideal_trig.saturating_sub(MIN_TRIG);
        let excess_ent = ideal_ent.saturating_sub(MIN_ENT);
        let excess_chg = ideal_changes.saturating_sub(MIN_CHANGES);
        let excess_total = excess_trig + excess_ent + excess_chg;
        if excess_total > 0 {
            let t = MIN_TRIG + (remaining * excess_trig / excess_total);
            let e = MIN_ENT + (remaining * excess_ent / excess_total);
            let c = available - t - e;
            (t, e, c)
        } else {
            let third = available / 3;
            (third, third, available - 2 * third)
        }
    };

    let mut out = String::new();
    out.push_str(&format!(
        "{}  {}  {}  {}  {}\n",
        pad_or_truncate("SEQ", w_seq),
        pad_or_truncate("TIMESTAMP", w_ts),
        pad_or_truncate("TRIGGER", w_trig),
        pad_or_truncate("ENTITIES", w_ent),
        "CHANGES",
    ));

    for (i, (row, entry)) in rows.iter().zip(entries.iter()).enumerate() {
        let trigger_fitted = format_trigger_column_fitted(entry, w_trig);
        let entities_fitted = entities_summary_fitted(&entry.diff.operations, &entry.state_after.entities, w_ent);
        let changes_plain = truncate_with_ellipsis(&row.changes, w_changes);
        let changes = colorize_changes(&changes_plain);
        out.push_str(&format!(
            "{}  {}  {}  {}  {}\n",
            pad_or_truncate(&row.seq, w_seq),
            pad_or_truncate(&row.ts, w_ts),
            pad_or_truncate(&trigger_fitted, w_trig),
            pad_or_truncate(&entities_fitted, w_ent),
            changes,
        ));
        if row.is_daemon_startup && i + 1 < rows.len() {
            out.push_str("──── daemon restart ────\n");
        }
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

    // Determine whether to show per-field outcome annotations.
    // Annotations are shown only when outcomes are mixed (some failed or skipped)
    // and the trigger is not an external change observation.
    let is_observed = matches!(entry.outcome, ApplyOutcome::Observed);
    let has_mixed_outcomes = !is_observed
        && entry
            .diff
            .operations
            .iter()
            .flat_map(|op| op.field_changes.iter())
            .any(|fc| {
                fc.change_kind != "unchanged"
                    && matches!(
                        fc.outcome.as_deref(),
                        Some("failed") | Some("skipped")
                    )
            });
    let show_annotations = has_mixed_outcomes;

    out.push_str("Diff:\n");
    if entry.diff.operations.is_empty() {
        out.push_str("  (no changes)\n");
    } else {
        for op in &entry.diff.operations {
            let header = match op.kind.as_str() {
                "add" => format!("  + {} {}\n", op.entity_type, op.entity_name).green().to_string(),
                "remove" => format!("  - {} {}\n", op.entity_type, op.entity_name).red().to_string(),
                _ => format!("  ~ {} {}\n", op.entity_type, op.entity_name).yellow().to_string(),
            };
            out.push_str(&header);
            for fc in &op.field_changes {
                if fc.change_kind == "unchanged" {
                    continue;
                }
                let annotation = if show_annotations {
                    fc.outcome.as_deref()
                } else {
                    None
                };
                let is_list = fc.current.as_ref().is_some_and(|v| v.is_array())
                    || fc.desired.as_ref().is_some_and(|v| v.is_array());
                if is_list {
                    format_list_field_diff(&mut out, fc, annotation);
                } else {
                    format_scalar_field_diff(&mut out, fc, annotation);
                }
            }
        }
    }

    out.push_str(&format!("Outcome: {}\n", outcome_detail(&entry.outcome)));

    if !entry.state_after.entities.is_empty() {
        let maps: Vec<IndexMap<String, serde_json::Value>> = entry
            .state_after
            .entities
            .iter()
            .map(serializable_state_to_flat_map)
            .collect();
        let yaml = serde_yaml::to_string(&maps).unwrap_or_default();
        let yaml = yaml.strip_prefix("---\n").unwrap_or(&yaml);
        out.push_str("State after:\n");
        out.push_str(yaml);
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
        ApplyOutcome::Applied { failed, .. } => {
            if *failed > 0 {
                format!("applied ({} fail)", failed)
            } else {
                "applied".to_string()
            }
        }
        ApplyOutcome::Observed => "observed".to_string(),
    }
}

fn count_failed_fields(entry: &JournalEntry) -> u32 {
    let count = entry
        .diff
        .operations
        .iter()
        .flat_map(|op| op.field_changes.iter())
        .filter(|fc| fc.change_kind != "unchanged" && fc.outcome.as_deref() == Some("failed"))
        .count() as u32;
    if count == 0 {
        // Legacy fallback for entries written before per-field outcome annotation.
        if let ApplyOutcome::Applied { failed, .. } = &entry.outcome {
            return *failed;
        }
    }
    count
}

fn changes_column(entry: &JournalEntry) -> String {
    let changes = changes_summary(&entry.diff.operations);
    match &entry.outcome {
        ApplyOutcome::Applied { failed, .. } if *failed > 0 => {
            let n = count_failed_fields(entry);
            format!("FAIL({}) {}", n, changes)
        }
        _ => changes,
    }
}

pub fn outcome_detail(outcome: &ApplyOutcome) -> String {
    match outcome {
        ApplyOutcome::Applied { succeeded, failed, skipped } => {
            format!("applied ({} succeeded, {} failed, {} skipped)", succeeded, failed, skipped)
        }
        ApplyOutcome::Observed => "observed".to_string(),
    }
}

pub fn entities_summary(ops: &[SerializableDiffOp]) -> String {
    entities_summary_with_state(ops, &[])
}

fn entities_summary_with_state(ops: &[SerializableDiffOp], state_entities: &[SerializableState]) -> String {
    if ops.is_empty() {
        if state_entities.is_empty() {
            return "(none)".to_string();
        }
        let names: Vec<String> = state_entities
            .iter()
            .filter(|s| !SYSTEM_ENTITY_TYPES.contains(&s.entity_type.as_str()))
            .map(|s| state_entity_display_name(s))
            .collect();
        if names.is_empty() {
            return "(none)".to_string();
        }
        return names.join(", ");
    }

    let items: Vec<(String, bool)> = ops
        .iter()
        .map(|op| {
            let prefix = match op.kind.as_str() {
                "add" => "+",
                "remove" => "-",
                _ => "",
            };
            let is_lifecycle = !prefix.is_empty();
            (format!("{}{}", prefix, entity_display_name(op)), is_lifecycle)
        })
        .collect();

    let count = items.len();

    if count <= 3 {
        return items.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>().join(", ");
    }

    if count <= 6 {
        // Prioritize lifecycle (add/remove) entities, show first 2
        let mut sorted = items.clone();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
        let shown: Vec<&str> = sorted[..2].iter().map(|(s, _)| s.as_str()).collect();
        return format!("{} (+{} more)", shown.join(", "), count - 2);
    }

    // 7+ entities: aggregate counts
    let add_count = ops.iter().filter(|op| op.kind == "add").count();
    let remove_count = ops.iter().filter(|op| op.kind == "remove").count();
    let modify_count = count - add_count - remove_count;
    let mut parts = Vec::new();
    if add_count > 0 {
        parts.push(format!("+{}", add_count));
    }
    if modify_count > 0 {
        parts.push(format!("~{}", modify_count));
    }
    if remove_count > 0 {
        parts.push(format!("-{}", remove_count));
    }
    format!("{} entities", parts.join(", "))
}

fn entities_summary_fitted(ops: &[SerializableDiffOp], state_entities: &[SerializableState], max_width: usize) -> String {
    let full = entities_summary_with_state(ops, state_entities);
    if full.chars().count() <= max_width {
        return full;
    }

    let items: Vec<String> = ops
        .iter()
        .map(|op| {
            let prefix = match op.kind.as_str() {
                "add" => "+",
                "remove" => "-",
                _ => "",
            };
            format!("{}{}", prefix, entity_display_name(op))
        })
        .collect();

    let count = items.len();
    if count == 0 {
        return full;
    }

    for show in (1..count).rev() {
        let shown: Vec<&str> = items[..show].iter().map(|s| s.as_str()).collect();
        let rest = count - show;
        let candidate = format!("{}, +{}…", shown.join(", "), rest);
        if candidate.chars().count() <= max_width {
            return candidate;
        }
    }

    let aggregate = format!("({} entities)", count);
    if aggregate.chars().count() <= max_width {
        return aggregate;
    }

    pad_or_truncate(&full, max_width)
}

pub fn changes_summary(ops: &[SerializableDiffOp]) -> String {
    if ops.is_empty() {
        return "(none)".to_string();
    }

    let mut parts: Vec<String> = Vec::new();

    for op in ops {
        match op.kind.as_str() {
            "add" => {
                parts.push(format!("+{}", entity_display_name(op)));
            }
            "remove" => {
                parts.push(format!("-{}", entity_display_name(op)));
            }
            _ => {
                for fc in &op.field_changes {
                    if fc.change_kind == "unchanged" {
                        continue;
                    }
                    let is_list = fc.current.as_ref().is_some_and(|v| v.is_array())
                        || fc.desired.as_ref().is_some_and(|v| v.is_array());

                    if is_list {
                        let empty_arr = Vec::new();
                        let current_items = fc
                            .current
                            .as_ref()
                            .and_then(|v| v.as_array())
                            .unwrap_or(&empty_arr);
                        let desired_items = fc
                            .desired
                            .as_ref()
                            .and_then(|v| v.as_array())
                            .unwrap_or(&empty_arr);

                        let added: Vec<&serde_json::Value> = desired_items
                            .iter()
                            .filter(|d| !current_items.contains(d))
                            .collect();
                        let removed: Vec<&serde_json::Value> = current_items
                            .iter()
                            .filter(|c| !desired_items.contains(c))
                            .collect();

                        if added.is_empty() && removed.is_empty() {
                            continue;
                        }

                        match fc.field_name.as_str() {
                            "addresses" => {
                                let a: Vec<&str> =
                                    added.iter().filter_map(|v| v.as_str().or_else(|| v.get("address")?.as_str())).collect();
                                let r: Vec<&str> =
                                    removed.iter().filter_map(|v| v.as_str().or_else(|| v.get("address")?.as_str())).collect();
                                parts.extend(format_address_changes(a, r));
                            }
                            "routes" => {
                                parts.extend(format_route_changes(added, removed));
                            }
                            "nameservers" => {
                                let a: Vec<&str> =
                                    added.iter().filter_map(|v| v.as_str()).collect();
                                let r: Vec<&str> =
                                    removed.iter().filter_map(|v| v.as_str()).collect();
                                parts.extend(format_dns_changes(a, r));
                            }
                            "search" | "search_domains" => {
                                let cur = fc
                                    .current
                                    .as_ref()
                                    .map(json_display_value)
                                    .unwrap_or_default();
                                let des = fc
                                    .desired
                                    .as_ref()
                                    .map(json_display_value)
                                    .unwrap_or_default();
                                if cur.is_empty() {
                                    parts.push(format!("+search: {}", des));
                                } else if des.is_empty() {
                                    parts.push("-search".to_string());
                                } else {
                                    parts.push(format!("search {}→{}", cur, des));
                                }
                            }
                            other => {
                                if !added.is_empty() {
                                    parts.push(format!("+{} {}", added.len(), other));
                                }
                                if !removed.is_empty() {
                                    parts.push(format!("-{} {}", removed.len(), other));
                                }
                            }
                        }
                    } else {
                        match fc.change_kind.as_str() {
                            "set" if fc.current.is_some() => {
                                let old = json_display_value(fc.current.as_ref().unwrap());
                                let new =
                                    json_display_value(fc.desired.as_ref().unwrap_or(&serde_json::Value::Null));
                                parts.push(format!("{} {}→{}", fc.field_name, old, new));
                            }
                            "set" => {
                                let val = json_display_value(
                                    fc.desired.as_ref().unwrap_or(&serde_json::Value::Null),
                                );
                                parts.push(format!("+{}: {}", fc.field_name, val));
                            }
                            "unset" => {
                                parts.push(format!("-{}", fc.field_name));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    if parts.is_empty() {
        return "(no changes)".to_string();
    }

    parts.join(", ")
}

fn get_terminal_width() -> usize {
    use terminal_size::{terminal_size, Width};
    terminal_size().map(|(Width(w), _)| w as usize).unwrap_or(120)
}

fn colorize_changes(plain: &str) -> String {
    if plain == "(none)" || plain == "(no changes)" {
        return plain.to_string();
    }
    // Handle Unicode ellipsis appended by truncate_with_ellipsis
    let ellipsis_char = '…';
    let (main, suffix) = if plain.ends_with(ellipsis_char) {
        let idx = plain.len() - ellipsis_char.len_utf8();
        (&plain[..idx], "…")
    } else {
        (plain, "")
    };
    if main.is_empty() {
        return plain.to_string();
    }
    let colored: String = main
        .split(", ")
        .map(colorize_change_token)
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}{}", colored, suffix)
}

fn colorize_change_token(token: &str) -> String {
    if token.is_empty() {
        return token.to_string();
    }
    match token.chars().next() {
        Some('+') => token.green().to_string(),
        Some('-') => token.red().to_string(),
        Some('(') => {
            if token.starts_with("(+") {
                token.green().to_string()
            } else if token.starts_with("(-") {
                token.red().to_string()
            } else {
                token.to_string()
            }
        }
        _ => {
            if token.contains('→') {
                token.yellow().to_string()
            } else {
                token.to_string()
            }
        }
    }
}

fn format_annotation_colored(annotation: Option<&str>) -> String {
    match annotation {
        Some("failed") => format!("  [{}]", "failed".red()),
        Some("skipped") => format!("  [{}]", "skipped".yellow()),
        Some("applied") => format!("  [{}]", "applied".green()),
        Some(other) => format!("  [{}]", other),
        None => String::new(),
    }
}

fn format_scalar_field_diff(out: &mut String, fc: &SerializableFieldChange, annotation: Option<&str>) {
    let ann = format_annotation_colored(annotation);
    match fc.change_kind.as_str() {
        "set" if fc.current.is_none() => {
            let desired = opt_json_compact(&fc.desired);
            let line = format!("+{}: {}", fc.field_name, desired);
            out.push_str(&format!("      {}{}\n", line.green(), ann));
        }
        "set" => {
            let current = opt_json_compact(&fc.current);
            let desired = opt_json_compact(&fc.desired);
            let old_line = format!("-{}: {}", fc.field_name, current);
            let new_line = format!("+{}: {}", fc.field_name, desired);
            out.push_str(&format!("      {}\n", old_line.red()));
            out.push_str(&format!("      {}{}\n", new_line.green(), ann));
        }
        "unset" => {
            let current = opt_json_compact(&fc.current);
            let line = format!("-{}: {}", fc.field_name, current);
            out.push_str(&format!("      {}{}\n", line.red(), ann));
        }
        _ => {}
    }
}

fn format_list_field_diff(out: &mut String, fc: &SerializableFieldChange, annotation: Option<&str>) {
    let empty = Vec::new();
    let current_items = fc.current.as_ref()
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    let desired_items = fc.desired.as_ref()
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);

    let ann = format_annotation_colored(annotation);
    out.push_str(&format!("      {}:\n", fc.field_name));
    for item in desired_items {
        if !current_items.contains(item) {
            let line = format!("        +{}", format_list_element(item));
            out.push_str(&format!("{}{}\n", line.green(), ann));
        }
    }
    for item in current_items {
        if !desired_items.contains(item) {
            let line = format!("        -{}", format_list_element(item));
            out.push_str(&format!("{}{}\n", line.red(), ann));
        }
    }
}

fn format_list_element(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(dest)) = map.get("destination") {
                let metric = map.get("metric")
                    .and_then(|m| m.as_u64())
                    .unwrap_or(0);
                if metric == 0 {
                    dest.clone()
                } else {
                    format!("{} metric {}", dest, metric)
                }
            } else {
                json_compact(v)
            }
        }
        _ => json_compact(v),
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

fn serializable_state_to_flat_map(state: &SerializableState) -> IndexMap<String, serde_json::Value> {
    let mut map = IndexMap::new();
    map.insert("type".to_string(), serde_json::Value::String(state.entity_type.clone()));
    map.insert("name".to_string(), serde_json::Value::String(state.selector_name.clone()));
    if let Some(obj) = state.fields.as_object() {
        for (k, v) in obj {
            map.insert(k.clone(), v.clone());
        }
    }
    map
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

    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut in_escape = false;
        for c in s.chars() {
            if in_escape {
                if c == 'm' {
                    in_escape = false;
                }
            } else if c == '\x1b' {
                in_escape = true;
            } else {
                out.push(c);
            }
        }
        out
    }

    fn default_args() -> HistoryArgs {
        HistoryArgs {
            count: 20,
            since: None,
            trigger: None,
            selector: vec![],
            show: None,
            output: HistoryOutputFormat::Text,
            absolute_timestamps: false,
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
        let output = format_text_list(&[make_entry()], false);
        assert!(
            output.contains("SEQ")
                && output.contains("TIMESTAMP")
                && output.contains("TRIGGER")
                && output.contains("ENTITIES")
                && output.contains("CHANGES"),
            "text list header should contain SEQ, TIMESTAMP, TRIGGER, ENTITIES, CHANGES"
        );
    }

    /// AC: Text list shows exactly N data rows plus 1 header row.
    #[test]
    fn test_format_text_list_has_one_header_plus_one_row_per_entry() {
        let entries: Vec<JournalEntry> = (0..5).map(|_| make_entry()).collect();
        let output = format_text_list(&entries, false);
        let line_count = output.lines().count();
        assert_eq!(line_count, 6, "text list should have 1 header + 5 data rows = 6 lines total");
    }

    /// AC: Text list shows seq number for each entry.
    #[test]
    fn test_format_text_list_shows_seq_number_for_each_entry() {
        let mut entry = make_entry();
        entry.seq = 142;
        let output = format_text_list(&[entry], false);
        assert!(output.contains("142"), "text list should show the entry's seq number");
    }

    /// AC: Empty entries list produces only the header row.
    #[test]
    fn test_format_text_list_empty_entries_produces_only_header() {
        let output = format_text_list(&[], false);
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

    /// AC: Detail diff shows scalar change as unified-diff lines.
    #[test]
    fn test_format_text_detail_shows_scalar_change_as_unified_diff() {
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
                    outcome: None,
                }],
            }],
        };
        let output = format_text_detail(&entry);
        assert!(
            output.contains("Diff:") && output.contains("ethernet") && output.contains("eth0"),
            "detail output should show diff section with entity type and name"
        );
        let plain = strip_ansi(&output);
        assert!(
            plain.contains("-mtu: 1500"),
            "scalar change must show '-mtu: 1500' line, got:\n{plain}"
        );
        assert!(
            plain.contains("+mtu: 9000"),
            "scalar change must show '+mtu: 9000' line, got:\n{plain}"
        );
    }

    /// AC: Detail diff shows list field additions as per-element lines.
    #[test]
    fn test_format_text_detail_shows_list_field_as_per_element_diff() {
        let mut entry = make_entry();
        entry.diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "enp7s0".to_string(),
                field_changes: vec![SerializableFieldChange {
                    field_name: "addresses".to_string(),
                    change_kind: "set".to_string(),
                    current: Some(serde_json::json!(["172.25.12.1/24"])),
                    desired: Some(serde_json::json!(["172.25.12.1/24", "172.25.14.22/32"])),
                    outcome: None,
                }],
            }],
        };
        let output = format_text_detail(&entry);
        let plain = strip_ansi(&output);
        assert!(
            plain.contains("addresses:"),
            "list field must show header 'addresses:', got:\n{plain}"
        );
        assert!(
            plain.contains("+172.25.14.22/32"),
            "added element must show '+172.25.14.22/32', got:\n{plain}"
        );
        assert!(
            !plain.contains("172.25.12.1/24"),
            "unchanged element must not appear, got:\n{plain}"
        );
    }

    /// AC: Detail diff shows route changes with readable format.
    #[test]
    fn test_format_text_detail_shows_route_element_readable_format() {
        let mut entry = make_entry();
        entry.diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth0".to_string(),
                field_changes: vec![SerializableFieldChange {
                    field_name: "routes".to_string(),
                    change_kind: "set".to_string(),
                    current: Some(serde_json::json!([])),
                    desired: Some(serde_json::json!([
                        {"destination": "10.0.0.0/8", "metric": 100}
                    ])),
                    outcome: None,
                }],
            }],
        };
        let output = format_text_detail(&entry);
        let plain = strip_ansi(&output);
        assert!(
            plain.contains("routes:"),
            "route field must show header 'routes:', got:\n{plain}"
        );
        assert!(
            plain.contains("+10.0.0.0/8 metric 100"),
            "route must show '+10.0.0.0/8 metric 100', got:\n{plain}"
        );
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

    /// AC: Detail output shows state snapshot after the change in YAML block format.
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
        assert!(
            output.contains("- type: ethernet"),
            "state-after should contain '- type: ethernet' in YAML block format, got:\n{output}"
        );
        assert!(
            output.contains("  name: eth0"),
            "state-after should contain '  name: eth0' in YAML block format, got:\n{output}"
        );
        assert!(
            output.contains("  mtu: 9000"),
            "state-after should contain '  mtu: 9000' in YAML block format, got:\n{output}"
        );
    }

    /// AC: State-after with list fields renders addresses as YAML block sequences, not JSON arrays.
    #[test]
    fn test_format_text_detail_state_after_addresses_yaml_block_sequence() {
        let mut entry = make_entry();
        entry.state_after = SerializableStateSet {
            entities: vec![SerializableState {
                entity_type: "ethernet".to_string(),
                selector_name: "eth0".to_string(),
                fields: serde_json::json!({
                    "mtu": 9000u64,
                    "addresses": ["10.0.1.50/24", "172.16.0.1/24"]
                }),
            }],
        };
        let output = format_text_detail(&entry);
        assert!(
            output.contains("  - 10.0.1.50/24"),
            "addresses must be rendered as YAML block sequence items, got:\n{output}"
        );
        assert!(
            !output.contains("[\"10.0.1.50/24\""),
            "addresses must not be rendered as JSON inline array, got:\n{output}"
        );
    }

    /// AC: serializable_state_to_flat_map places "type" first, "name" second.
    #[test]
    fn test_serializable_state_to_flat_map_puts_type_first_name_second() {
        let state = SerializableState {
            entity_type: "ethernet".to_string(),
            selector_name: "eth0".to_string(),
            fields: serde_json::json!({ "mtu": 1500u64 }),
        };
        let map = serializable_state_to_flat_map(&state);
        let keys: Vec<&str> = map.keys().map(|k| k.as_str()).collect();
        assert_eq!(keys[0], "type", "first key must be 'type'");
        assert_eq!(keys[1], "name", "second key must be 'name'");
        assert_eq!(map["type"], serde_json::json!("ethernet"));
        assert_eq!(map["name"], serde_json::json!("eth0"));
        assert_eq!(map["mtu"], serde_json::json!(1500u64));
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

    /// AC: 4-6 entities show first 2 plus "(+N more)".
    #[test]
    fn test_entities_summary_many_entities_truncated_with_plus_n_more() {
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
            result.contains("(+3 more)"),
            "5 entities should show first 2 + '(+3 more)', got: {}",
            result
        );
        assert!(
            result.contains("eth0") && result.contains("eth1"),
            "first 2 entities should be shown, got: {}",
            result
        );
    }

    /// AC: Three short entities that fit within 25 chars are shown in full.
    #[test]
    fn test_entities_summary_three_short_entities_shown_in_full() {
        let ops: Vec<SerializableDiffOp> = (0..3)
            .map(|i| SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: format!("eth{}", i),
                field_changes: vec![],
            })
            .collect();
        assert_eq!(entities_summary(&ops), "eth0, eth1, eth2");
    }

    // ── changes_summary ───────────────────────────────────────────────────────

    /// AC: Empty ops produces "(none)".
    #[test]
    fn test_changes_summary_empty_ops_returns_none_string() {
        assert_eq!(changes_summary(&[]), "(none)");
    }

    /// AC: Single add op shows "+{entity_name}".
    #[test]
    fn test_changes_summary_single_add_op_returns_plus_entity() {
        let ops = vec![SerializableDiffOp {
            kind: "add".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![],
        }];
        assert_eq!(changes_summary(&ops), "+eth0");
    }

    /// AC: Multiple add ops show "+name1, +name2, ...".
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
        assert_eq!(changes_summary(&ops), "+eth0, +eth1, +eth2");
    }

    /// AC: Single remove op shows "-{entity_name}".
    #[test]
    fn test_changes_summary_single_remove_op_returns_minus_entity() {
        let ops = vec![SerializableDiffOp {
            kind: "remove".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![],
        }];
        assert_eq!(changes_summary(&ops), "-eth0");
    }

    /// AC: Field modification (set with current) shows "field old→new".
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
                outcome: None,
            }],
        }];
        let result = changes_summary(&ops);
        assert!(
            result.contains("mtu 1500→9000"),
            "field modification should show 'mtu 1500→9000', got: {}",
            result
        );
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
                outcome: None,
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
                outcome: None,
            }],
        }];
        let result = changes_summary(&ops);
        assert!(result.contains("-addr"), "unset field should show '-addr', got: {}", result);
    }

    // ── outcome_summary ───────────────────────────────────────────────────────

    /// AC: Applied outcome with failures shows "applied (N fail)".
    #[test]
    fn test_outcome_summary_applied_with_failures_shows_fail_count() {
        let outcome = ApplyOutcome::Applied { succeeded: 2, failed: 1, skipped: 0 };
        let result = outcome_summary(&outcome);
        assert_eq!(result, "applied (1 fail)");
    }

    /// AC: Applied with only successes shows "applied" without counts.
    #[test]
    fn test_outcome_summary_applied_with_only_successes_shows_applied() {
        let outcome = ApplyOutcome::Applied { succeeded: 3, failed: 0, skipped: 0 };
        let result = outcome_summary(&outcome);
        assert_eq!(result, "applied");
    }

    /// AC: Applied with skips but no failures shows "applied" without counts.
    #[test]
    fn test_outcome_summary_applied_with_skips_no_failures_shows_applied() {
        let outcome = ApplyOutcome::Applied { succeeded: 1, failed: 0, skipped: 5 };
        let result = outcome_summary(&outcome);
        assert_eq!(result, "applied");
    }

    /// AC: Observed outcome produces "observed".
    #[test]
    fn test_outcome_summary_observed_returns_observed() {
        assert_eq!(outcome_summary(&ApplyOutcome::Observed), "observed");
    }

    /// AC: Detail view shows full breakdown with all counts.
    #[test]
    fn test_outcome_detail_shows_full_breakdown() {
        let outcome = ApplyOutcome::Applied { succeeded: 2, failed: 1, skipped: 3 };
        let result = outcome_detail(&outcome);
        assert_eq!(result, "applied (2 succeeded, 1 failed, 3 skipped)");
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
    //
    // BUG: The three tests below were written with incorrect expected values.
    // The SPEC-352 spec says addresses should show actual values inline (e.g.
    // "+192.168.1.100/24"), not "addresses(+N)" notation. The implementation
    // correctly follows the spec and uses format_address_changes which shows
    // actual values for 1-2 total changes. These tests will fail — the verify
    // phase should update the assertions to match the correct spec behavior.

    /// AC: 2 address additions (total 2, ≤2 threshold) show actual values "+addr1, +addr2".
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
                outcome: None,
            }],
        }];
        let result = changes_summary(&ops);
        // Spec: 1-2 total changes show all by value
        assert!(
            result.contains("+10.0.0.1/24"),
            "2 address additions should show actual values '+addr', got: {}",
            result
        );
        assert!(
            result.contains("+10.0.0.2/24"),
            "both added addresses should appear by value, got: {}",
            result
        );
    }

    /// AC: 1 address removal (total 1, ≤2 threshold) shows actual value "-addr".
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
                outcome: None,
            }],
        }];
        let result = changes_summary(&ops);
        // Spec: 1-2 total changes show all by value
        assert!(
            result.contains("-10.0.0.1/24"),
            "1 address removal should show actual value '-addr', got: {}",
            result
        );
    }

    /// AC: 2 additions + 1 removal (total 3, 3-8 range) shows first 2 added by value + first removed by value.
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
                outcome: None,
            }],
        }];
        let result = changes_summary(&ops);
        // Spec: 3-8 total → first 2 added by value, count rest; first 1 removed by value, count rest
        // Total = 3: 2 added (show both) + 1 removed (show it)
        assert!(
            result.contains("+10.0.0.1/24"),
            "first added address should appear by value, got: {}",
            result
        );
        assert!(
            result.contains("+10.0.0.2/24"),
            "second added address should appear by value, got: {}",
            result
        );
        assert!(
            result.contains("-192.168.1.1/24"),
            "removed address should appear by value, got: {}",
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
                outcome: None,
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
        let output = format_text_list(&[entry], false);
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
        let output = format_text_list(&[entry], false);
        let data_row = output.lines().nth(1).unwrap();
        assert!(
            data_row.contains("eth0"),
            "data row should show entity name 'eth0', got: {}",
            data_row
        );
    }

    /// AC: Rows with failed operations show FAIL prefix in CHANGES column.
    #[test]
    fn test_format_text_list_row_shows_fail_prefix_when_failures() {
        let mut entry = make_entry();
        entry.outcome = ApplyOutcome::Applied { succeeded: 1, failed: 2, skipped: 0 };
        let output = format_text_list(&[entry], false);
        let data_row = output.lines().nth(1).unwrap();
        assert!(
            data_row.contains("FAIL"),
            "data row should show FAIL prefix when there are failures, got: {}",
            data_row
        );
    }

    /// AC: Rows without failures do not show FAIL prefix.
    #[test]
    fn test_format_text_list_row_no_fail_prefix_when_no_failures() {
        let mut entry = make_entry();
        entry.outcome = ApplyOutcome::Applied { succeeded: 2, failed: 0, skipped: 0 };
        let output = format_text_list(&[entry], false);
        let data_row = output.lines().nth(1).unwrap();
        assert!(
            !data_row.contains("FAIL"),
            "data row should not show FAIL when no failures, got: {}",
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

    // ── format_timestamp ──────────────────────────────────────────────────────

    /// AC: Entries from today under 60 seconds show "N sec ago".
    #[test]
    fn test_format_timestamp_today_under_60s_shows_sec_ago() {
        let now = Utc::now();
        let ts = now - Duration::seconds(45);
        let result = format_timestamp(ts, now, false);
        assert!(
            result.ends_with("sec ago"),
            "entry from 45s ago should show 'N sec ago', got: {}",
            result
        );
        assert!(
            result.contains("45"),
            "should contain seconds count 45, got: {}",
            result
        );
    }

    /// AC: Entries from today show relative durations "5 min ago".
    #[test]
    fn test_format_timestamp_today_5min_shows_min_ago() {
        let now = Utc::now();
        let ts = now - Duration::minutes(5);
        let result = format_timestamp(ts, now, false);
        assert_eq!(result, "5 min ago", "entry from 5 min ago should show '5 min ago', got: {}", result);
    }

    /// AC: Entries from today 30 minutes ago show "30 min ago".
    #[test]
    fn test_format_timestamp_today_30min_shows_30_min_ago() {
        let now = Utc::now();
        let ts = now - Duration::minutes(30);
        let result = format_timestamp(ts, now, false);
        assert_eq!(result, "30 min ago", "entry from 30 min ago should show '30 min ago', got: {}", result);
    }

    /// AC: Entries from today over 1 hour show "Nh ago" format.
    #[test]
    fn test_format_timestamp_today_2h_shows_h_ago() {
        let now = Utc::now();
        let ts = now - Duration::hours(2);
        let result = format_timestamp(ts, now, false);
        assert_eq!(result, "2h ago", "entry from 2h ago should show '2h ago', got: {}", result);
    }

    /// AC: Entries from today 5 hours ago show "5h ago".
    #[test]
    fn test_format_timestamp_today_5h_shows_5h_ago() {
        let now = Utc::now();
        let ts = now - Duration::hours(5);
        let result = format_timestamp(ts, now, false);
        assert_eq!(result, "5h ago", "entry from 5h ago should show '5h ago', got: {}", result);
    }

    /// AC: Entries from yesterday show "yesterday HH:MM" format.
    #[test]
    fn test_format_timestamp_yesterday_shows_yesterday_hhmm() {
        let now = Utc::now();
        // Move back exactly 1 day (same time yesterday)
        let yesterday = now - Duration::days(1);
        let result = format_timestamp(yesterday, now, false);
        assert!(
            result.starts_with("yesterday "),
            "entry from yesterday should start with 'yesterday ', got: {}",
            result
        );
        // Should also contain the time in HH:MM format
        let time_part = &result["yesterday ".len()..];
        assert!(
            time_part.len() == 5 && time_part.contains(':'),
            "time part should be in HH:MM format, got: {}",
            time_part
        );
    }

    /// AC: Older entries show full date in "YYYY-MM-DD HH:MM" format.
    #[test]
    fn test_format_timestamp_3_days_ago_shows_full_date() {
        let now = Utc::now();
        let ts = now - Duration::days(3);
        let result = format_timestamp(ts, now, false);
        // Should be "YYYY-MM-DD HH:MM" format
        assert!(
            result.len() >= 16 && result.contains('-') && result.contains(':'),
            "entry from 3 days ago should show YYYY-MM-DD HH:MM format, got: {}",
            result
        );
        // Should NOT start with "yesterday"
        assert!(
            !result.starts_with("yesterday"),
            "entry from 3 days ago should not show 'yesterday', got: {}",
            result
        );
        // Should NOT end with "ago"
        assert!(
            !result.ends_with("ago"),
            "entry from 3 days ago should not end with 'ago', got: {}",
            result
        );
        // Should look like a date: e.g., "2026-04-21 14:30"
        assert_eq!(&result[4..5], "-", "should have '-' at position 4 (year-month separator), got: {}", result);
    }

    /// AC: Absolute timestamps flag overrides relative format to "YYYY-MM-DD HH:MM:SS".
    #[test]
    fn test_format_timestamp_absolute_mode_shows_full_datetime() {
        let now = Utc::now();
        let ts = now - Duration::minutes(5);
        let result = format_timestamp(ts, now, true);
        // Should be exactly "YYYY-MM-DD HH:MM:SS"
        assert!(
            result.len() == 19,
            "absolute timestamp should be 19 chars (YYYY-MM-DD HH:MM:SS), got len={}, val={}",
            result.len(), result
        );
        assert_eq!(&result[4..5], "-");
        assert_eq!(&result[7..8], "-");
        assert_eq!(&result[10..11], " ");
        assert_eq!(&result[13..14], ":");
        assert_eq!(&result[16..17], ":");
    }

    /// AC: Detail view always shows full ISO 8601 timestamp (no relative format).
    #[test]
    fn test_format_text_detail_timestamp_always_full_iso8601_format() {
        let mut entry = make_entry();
        entry.timestamp = chrono::DateTime::parse_from_rfc3339("2026-04-20T14:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let output = format_text_detail(&entry);
        assert!(
            output.contains("2026-04-20 14:30:00"),
            "detail view timestamp must be in 'YYYY-MM-DD HH:MM:SS' format, got:\n{}",
            output
        );
        assert!(
            output.contains("UTC"),
            "detail view timestamp must include 'UTC' suffix, got:\n{}",
            output
        );
    }

    /// AC: --absolute-timestamps flag makes text list show YYYY-MM-DD HH:MM:SS.
    #[test]
    fn test_format_text_list_absolute_timestamps_shows_full_format() {
        let mut entry = make_entry();
        entry.timestamp = chrono::DateTime::parse_from_rfc3339("2026-04-20T14:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let output = format_text_list(&[entry], true);
        // With absolute timestamps, should show full format, not relative
        assert!(
            output.contains("2026-04-20 14:30:00"),
            "absolute timestamps mode should show YYYY-MM-DD HH:MM:SS, got:\n{}",
            output
        );
        assert!(
            !output.contains("ago"),
            "absolute timestamps mode should not show 'ago', got:\n{}",
            output
        );
    }

    // ── format_trigger_column ─────────────────────────────────────────────────

    /// AC: PolicyApply with single policy shows "apply (eth0-static)".
    #[test]
    fn test_format_trigger_column_policy_apply_single_policy() {
        let mut entry = make_entry();
        entry.trigger = Trigger::PolicyApply { source: "test.yaml".to_string() };
        entry.active_policies = vec![PolicySummary {
            name: "eth0-static".to_string(),
            factory_type: "static".to_string(),
            priority: 100,
        }];
        let result = format_trigger_column(&entry);
        assert_eq!(result, "apply (eth0-static)");
    }

    /// AC: PolicyApply with multiple policies shows "apply (first-name, +N)".
    #[test]
    fn test_format_trigger_column_policy_apply_multiple_policies_shows_plus_n() {
        let mut entry = make_entry();
        entry.trigger = Trigger::PolicyApply { source: "test.yaml".to_string() };
        entry.active_policies = vec![
            PolicySummary { name: "eth0-static".to_string(), factory_type: "static".to_string(), priority: 100 },
            PolicySummary { name: "eth0-dhcp".to_string(), factory_type: "dhcpv4".to_string(), priority: 100 },
            PolicySummary { name: "eth1-static".to_string(), factory_type: "static".to_string(), priority: 100 },
        ];
        let result = format_trigger_column(&entry);
        assert_eq!(result, "apply (eth0-static, +2)");
    }

    /// AC: PolicyApply with no policies shows "apply".
    #[test]
    fn test_format_trigger_column_policy_apply_no_policies() {
        let entry = make_entry(); // active_policies is empty
        let result = format_trigger_column(&entry);
        assert_eq!(result, "apply");
    }

    /// AC: DhcpEvent with lease_acquired shows "dhcp-acquire".
    #[test]
    fn test_format_trigger_column_dhcp_acquire() {
        let mut entry = make_entry();
        entry.trigger = Trigger::DhcpEvent {
            policy_name: "eth0-dhcp".to_string(),
            event_kind: "lease_acquired".to_string(),
        };
        let result = format_trigger_column(&entry);
        assert_eq!(result, "dhcp-acquire");
    }

    /// AC: DhcpEvent with lease_renewed shows "dhcp-renew".
    #[test]
    fn test_format_trigger_column_dhcp_renew() {
        let mut entry = make_entry();
        entry.trigger = Trigger::DhcpEvent {
            policy_name: "eth0-dhcp".to_string(),
            event_kind: "lease_renewed".to_string(),
        };
        let result = format_trigger_column(&entry);
        assert_eq!(result, "dhcp-renew");
    }

    /// AC: DhcpEvent with lease_expired shows "dhcp-expire".
    #[test]
    fn test_format_trigger_column_dhcp_expire() {
        let mut entry = make_entry();
        entry.trigger = Trigger::DhcpEvent {
            policy_name: "eth0-dhcp".to_string(),
            event_kind: "lease_expired".to_string(),
        };
        let result = format_trigger_column(&entry);
        assert_eq!(result, "dhcp-expire");
    }

    /// AC: ExternalChange trigger shows "external".
    #[test]
    fn test_format_trigger_column_external_change() {
        let mut entry = make_entry();
        entry.trigger = Trigger::ExternalChange { changed_entities: vec!["eth0".to_string()] };
        let result = format_trigger_column(&entry);
        assert_eq!(result, "external");
    }

    /// AC: DaemonStartup trigger shows "daemon-startup".
    #[test]
    fn test_format_trigger_column_daemon_startup() {
        let mut entry = make_entry();
        entry.trigger = Trigger::DaemonStartup;
        let result = format_trigger_column(&entry);
        assert_eq!(result, "daemon-startup");
    }

    /// AC: Revert trigger shows "revert (N)" with the target sequence number.
    #[test]
    fn test_format_trigger_column_revert_shows_target_seq() {
        let mut entry = make_entry();
        entry.trigger = Trigger::Revert { target_seq: 42 };
        let result = format_trigger_column(&entry);
        assert_eq!(result, "revert (42)");
    }

    // ── entities_summary: lifecycle prefixes ──────────────────────────────────

    /// AC: Add operation shows "+" prefix on entity name.
    #[test]
    fn test_entities_summary_add_op_shows_plus_prefix() {
        let ops = vec![SerializableDiffOp {
            kind: "add".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![],
        }];
        assert_eq!(entities_summary(&ops), "+eth0");
    }

    /// AC: Remove operation shows "-" prefix on entity name.
    #[test]
    fn test_entities_summary_remove_op_shows_minus_prefix() {
        let ops = vec![SerializableDiffOp {
            kind: "remove".to_string(),
            entity_type: "vlan".to_string(),
            entity_name: "bond0.200".to_string(),
            field_changes: vec![],
        }];
        assert_eq!(entities_summary(&ops), "-bond0.200");
    }

    /// AC: Modify operation shows no prefix on entity name.
    #[test]
    fn test_entities_summary_modify_op_shows_no_prefix() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![],
        }];
        assert_eq!(entities_summary(&ops), "eth0");
    }

    /// AC: System entity types use "sys:" prefix (dns → sys:dns).
    #[test]
    fn test_entities_summary_dns_entity_shows_sys_prefix() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "dns".to_string(),
            entity_name: "global".to_string(),
            field_changes: vec![],
        }];
        let result = entities_summary(&ops);
        assert_eq!(result, "sys:dns", "dns entity type should appear as 'sys:dns', got: {}", result);
    }

    /// AC: hostname entity uses "sys:hostname" display prefix.
    #[test]
    fn test_entities_summary_hostname_entity_shows_sys_prefix() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "hostname".to_string(),
            entity_name: "global".to_string(),
            field_changes: vec![],
        }];
        let result = entities_summary(&ops);
        assert_eq!(result, "sys:hostname");
    }

    /// AC: ntp entity uses "sys:ntp" display prefix.
    #[test]
    fn test_entities_summary_ntp_entity_shows_sys_prefix() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ntp".to_string(),
            entity_name: "global".to_string(),
            field_changes: vec![],
        }];
        let result = entities_summary(&ops);
        assert_eq!(result, "sys:ntp");
    }

    /// AC: Mixed interface and system entities show both with correct prefixes.
    #[test]
    fn test_entities_summary_mixed_interface_and_system_entities() {
        let ops = vec![
            SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth0".to_string(),
                field_changes: vec![],
            },
            SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "dns".to_string(),
                entity_name: "global".to_string(),
                field_changes: vec![],
            },
        ];
        let result = entities_summary(&ops);
        assert_eq!(result, "eth0, sys:dns");
    }

    /// AC: 7+ entities show aggregate counts "+N, ~M, -K entities".
    #[test]
    fn test_entities_summary_seven_plus_entities_shows_aggregate_counts() {
        let ops: Vec<SerializableDiffOp> = vec![
            ("add", "eth0"), ("add", "eth1"), ("add", "eth2"), ("add", "eth3"),
            ("modify", "eth4"), ("modify", "eth5"),
            ("remove", "eth6"),
        ]
        .into_iter()
        .map(|(kind, name)| SerializableDiffOp {
            kind: kind.to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: name.to_string(),
            field_changes: vec![],
        })
        .collect();

        let result = entities_summary(&ops);
        // 7 entities: 4 add, 2 modify, 1 remove → aggregate counts
        assert!(
            result.contains("entities"),
            "7+ entities should produce aggregate count summary with 'entities', got: {}",
            result
        );
        assert!(
            result.contains("+4"),
            "should show +4 additions, got: {}",
            result
        );
        assert!(
            result.contains("~2"),
            "should show ~2 modifications, got: {}",
            result
        );
        assert!(
            result.contains("-1"),
            "should show -1 removal, got: {}",
            result
        );
    }

    // ── changes_summary: address inline values ────────────────────────────────

    /// AC: Single address addition shows actual value "+192.168.1.100/24".
    #[test]
    fn test_changes_summary_single_address_addition_shows_actual_value() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "addresses".to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::json!([])),
                desired: Some(serde_json::json!(["192.168.1.100/24"])),
                outcome: None,
            }],
        }];
        let result = changes_summary(&ops);
        assert_eq!(result, "+192.168.1.100/24", "single address addition should show '+192.168.1.100/24', got: {}", result);
    }

    /// AC: Two address changes show both values inline.
    #[test]
    fn test_changes_summary_two_address_changes_show_both_values() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "addresses".to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::json!(["10.0.0.42/24"])),
                desired: Some(serde_json::json!(["10.0.0.50/24"])),
                outcome: None,
            }],
        }];
        let result = changes_summary(&ops);
        assert!(
            result.contains("+10.0.0.50/24"),
            "should show added address by value, got: {}",
            result
        );
        assert!(
            result.contains("-10.0.0.42/24"),
            "should show removed address by value, got: {}",
            result
        );
    }

    /// AC: 5 address additions and 3 removals caps at first 2 shown by value.
    #[test]
    fn test_changes_summary_5_addr_additions_3_removals_caps_at_2_shown() {
        let added: Vec<serde_json::Value> = (1..=5)
            .map(|i| serde_json::json!(format!("10.0.0.{}/24", i + 10)))
            .collect();
        let removed: Vec<serde_json::Value> = (1..=3)
            .map(|i| serde_json::json!(format!("192.168.{}.1/24", i)))
            .collect();
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "addresses".to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::Value::Array(removed)),
                desired: Some(serde_json::Value::Array(added)),
                outcome: None,
            }],
        }];
        let result = changes_summary(&ops);
        // 5 added + 3 removed = 8 total: 3–8 range → show first 2 added by value, count rest
        assert!(
            result.contains("(+3 addrs)"),
            "should show '(+3 addrs)' for remaining 3 additions after showing 2, got: {}",
            result
        );
        assert!(
            result.contains("-192.168.1.1/24"),
            "should show first removed address by value, got: {}",
            result
        );
        assert!(
            result.contains("(-2 addrs)"),
            "should show '(-2 addrs)' for remaining 2 removals, got: {}",
            result
        );
    }

    /// AC: 10 address additions and 10 removals shows only counts "+10 addrs, -10 addrs".
    #[test]
    fn test_changes_summary_10_plus_10_addresses_shows_only_counts() {
        let added: Vec<serde_json::Value> = (1..=10)
            .map(|i| serde_json::json!(format!("10.0.0.{}/24", i + 10)))
            .collect();
        let removed: Vec<serde_json::Value> = (1..=10)
            .map(|i| serde_json::json!(format!("192.168.0.{}/24", i)))
            .collect();
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "addresses".to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::Value::Array(removed)),
                desired: Some(serde_json::Value::Array(added)),
                outcome: None,
            }],
        }];
        let result = changes_summary(&ops);
        // 20 total → 9+ → count only
        assert!(
            result.contains("+10 addrs"),
            "9+ address changes should show '+10 addrs' count, got: {}",
            result
        );
        assert!(
            result.contains("-10 addrs"),
            "9+ address changes should show '-10 addrs' count, got: {}",
            result
        );
    }

    /// AC: Non-link-local addresses are shown before link-local addresses.
    #[test]
    fn test_changes_summary_address_priority_prefers_non_link_local() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "addresses".to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::json!([])),
                desired: Some(serde_json::json!(["fe80::1/64", "192.168.1.100/24"])),
                outcome: None,
            }],
        }];
        let result = changes_summary(&ops);
        // Both addresses added (total 2) → show all by value, non-link-local first
        let fe80_pos = result.find("fe80::1");
        let non_ll_pos = result.find("192.168.1.100");
        assert!(
            fe80_pos.is_some() && non_ll_pos.is_some(),
            "both addresses should appear in result, got: {}",
            result
        );
        assert!(
            non_ll_pos.unwrap() < fe80_pos.unwrap(),
            "non-link-local address 192.168.1.100/24 should appear before fe80::1/64, got: {}",
            result
        );
    }

    // ── changes_summary: route changes ────────────────────────────────────────

    /// AC: Default route addition is shown by value "+dflt via 10.0.0.1".
    #[test]
    fn test_changes_summary_default_route_addition_shown_by_value() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "routes".to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::json!([])),
                desired: Some(serde_json::json!([
                    {"destination": "0.0.0.0/0", "gateway": "10.0.0.1"}
                ])),
                outcome: None,
            }],
        }];
        let result = changes_summary(&ops);
        assert!(
            result.contains("+dflt via 10.0.0.1"),
            "default route addition should show '+dflt via 10.0.0.1', got: {}",
            result
        );
    }

    /// AC: Non-default routes show counts only, not individual destinations.
    #[test]
    fn test_changes_summary_non_default_routes_show_count_only() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "routes".to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::json!([])),
                desired: Some(serde_json::json!([
                    {"destination": "10.0.0.0/8", "gateway": "192.168.1.1"},
                    {"destination": "172.16.0.0/12", "gateway": "192.168.1.1"},
                    {"destination": "192.168.2.0/24", "gateway": "192.168.1.1"},
                ])),
                outcome: None,
            }],
        }];
        let result = changes_summary(&ops);
        assert_eq!(result, "+3 routes", "3 non-default routes should show '+3 routes', got: {}", result);
    }

    /// AC: Default route and non-default routes: default shown by value, others counted.
    #[test]
    fn test_changes_summary_default_route_and_non_default_routes_mixed() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "routes".to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::json!([])),
                desired: Some(serde_json::json!([
                    {"destination": "0.0.0.0/0", "gateway": "10.0.0.1"},
                    {"destination": "10.0.0.0/8", "gateway": "192.168.1.1"},
                    {"destination": "172.16.0.0/12", "gateway": "192.168.1.1"},
                    {"destination": "192.168.2.0/24", "gateway": "192.168.1.1"},
                    {"destination": "203.0.113.0/24", "gateway": "192.168.1.1"},
                ])),
                outcome: None,
            }],
        }];
        let result = changes_summary(&ops);
        assert!(
            result.contains("+dflt via 10.0.0.1"),
            "default route should be shown by value, got: {}",
            result
        );
        // 1 default + 4 non-default = 5 total; default shown separately, 4 counted as "+4 routes"
        assert!(
            result.contains("+4 routes"),
            "4 non-default routes should show '+4 routes', got: {}",
            result
        );
    }

    /// AC: Default route removal is shown by value "-dflt via ...".
    #[test]
    fn test_changes_summary_default_route_removal_shown_by_value() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "routes".to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::json!([
                    {"destination": "0.0.0.0/0", "gateway": "192.168.1.1"}
                ])),
                desired: Some(serde_json::json!([])),
                outcome: None,
            }],
        }];
        let result = changes_summary(&ops);
        assert!(
            result.contains("-dflt via 192.168.1.1"),
            "default route removal should show '-dflt via 192.168.1.1', got: {}",
            result
        );
    }

    // ── changes_summary: DNS changes ──────────────────────────────────────────

    /// AC: DNS nameserver addition shows "+ns 8.8.8.8".
    #[test]
    fn test_changes_summary_dns_nameserver_addition_shows_ns_shorthand() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "dns".to_string(),
            entity_name: "global".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "nameservers".to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::json!([])),
                desired: Some(serde_json::json!(["8.8.8.8"])),
                outcome: None,
            }],
        }];
        let result = changes_summary(&ops);
        assert!(
            result.contains("+ns 8.8.8.8"),
            "DNS nameserver addition should show '+ns 8.8.8.8', got: {}",
            result
        );
    }

    /// AC: DNS nameserver removal shows "-ns 10.0.0.1".
    #[test]
    fn test_changes_summary_dns_nameserver_removal_shows_ns_shorthand() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "dns".to_string(),
            entity_name: "global".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "nameservers".to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::json!(["10.0.0.1"])),
                desired: Some(serde_json::json!([])),
                outcome: None,
            }],
        }];
        let result = changes_summary(&ops);
        assert!(
            result.contains("-ns 10.0.0.1"),
            "DNS nameserver removal should show '-ns 10.0.0.1', got: {}",
            result
        );
    }

    /// AC: DNS search domain change shows "search old→new" scalar notation.
    #[test]
    fn test_changes_summary_dns_search_domain_change_shows_scalar_notation() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "dns".to_string(),
            entity_name: "global".to_string(),
            field_changes: vec![SerializableFieldChange {
                // "search" matches the "search" | "search_domains" branch
                field_name: "search".to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::json!("example.com")),
                desired: Some(serde_json::json!("corp.local")),
                outcome: None,
            }],
        }];
        let result = changes_summary(&ops);
        assert!(
            result.contains("search example.com→corp.local"),
            "DNS search domain change should show 'search example.com→corp.local', got: {}",
            result
        );
    }

    // ── format_text_list: daemon-startup separators ───────────────────────────

    /// AC: Separator "──── daemon restart ────" appears after a daemon-startup row.
    #[test]
    fn test_format_text_list_daemon_startup_separator_appears_after_startup_entry() {
        let mut apply_entry = make_entry();
        apply_entry.seq = 5;
        apply_entry.trigger = Trigger::PolicyApply { source: "test.yaml".to_string() };

        let mut startup_entry = make_entry();
        startup_entry.seq = 4;
        startup_entry.trigger = Trigger::DaemonStartup;

        let mut external_entry = make_entry();
        external_entry.seq = 3;
        external_entry.trigger = Trigger::ExternalChange { changed_entities: vec![] };

        let entries = vec![apply_entry, startup_entry, external_entry];
        let output = format_text_list(&entries, false);

        assert!(
            output.contains("──── daemon restart ────"),
            "separator must appear when daemon-startup entry is followed by another entry, got:\n{}",
            output
        );
    }

    /// AC: Separator is between daemon-startup row and the row below it (previous session).
    #[test]
    fn test_format_text_list_daemon_startup_separator_placed_between_sessions() {
        let mut startup_entry = make_entry();
        startup_entry.seq = 4;
        startup_entry.trigger = Trigger::DaemonStartup;

        let mut external_entry = make_entry();
        external_entry.seq = 3;
        external_entry.trigger = Trigger::ExternalChange { changed_entities: vec![] };

        let entries = vec![startup_entry, external_entry];
        let output = format_text_list(&entries, false);

        // The separator must appear between the daemon-startup row and the external row
        let lines: Vec<&str> = output.lines().collect();
        let startup_pos = lines.iter().position(|l| l.contains("daemon-startup"));
        let separator_pos = lines.iter().position(|l| l.contains("daemon restart"));

        assert!(startup_pos.is_some(), "daemon-startup row must be present, got:\n{}", output);
        assert!(separator_pos.is_some(), "separator must be present, got:\n{}", output);
        assert!(
            separator_pos.unwrap() == startup_pos.unwrap() + 1,
            "separator must appear immediately after daemon-startup row, got:\n{}",
            output
        );
    }

    /// AC: No separator appears when daemon-startup is the last (oldest) visible entry.
    #[test]
    fn test_format_text_list_no_separator_below_oldest_daemon_startup_entry() {
        let mut startup_entry = make_entry();
        startup_entry.seq = 1;
        startup_entry.trigger = Trigger::DaemonStartup;

        let entries = vec![startup_entry];
        let output = format_text_list(&entries, false);

        assert!(
            !output.contains("daemon restart"),
            "no separator should appear when daemon-startup is the only/oldest entry, got:\n{}",
            output
        );
    }

    /// AC: No separator appears when daemon-startup is the last entry in the list.
    #[test]
    fn test_format_text_list_no_separator_when_daemon_startup_is_last_entry() {
        let mut apply_entry = make_entry();
        apply_entry.seq = 5;
        apply_entry.trigger = Trigger::PolicyApply { source: "test.yaml".to_string() };

        let mut startup_entry = make_entry();
        startup_entry.seq = 4;
        startup_entry.trigger = Trigger::DaemonStartup;

        // startup_entry is last in list → no separator after it
        let entries = vec![apply_entry, startup_entry];
        let output = format_text_list(&entries, false);

        // Separator should appear because daemon-startup has an entry after it
        // Wait - startup is at index 1 and there are only 2 rows (indices 0,1)
        // i=1 is last (i+1 == rows.len()) → no separator
        assert!(
            !output.contains("daemon restart"),
            "no separator should appear when daemon-startup is the last row, got:\n{}",
            output
        );
    }

    // ── changes_summary: mtu scalar change format ────────────────────────────

    /// AC: Scalar mtu change from 1500 to 9000 shows "mtu 1500→9000".
    #[test]
    fn test_changes_summary_mtu_scalar_change_from_1500_to_9000() {
        let ops = vec![SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: "ethernet".to_string(),
            entity_name: "eth0".to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: "mtu".to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::json!(1500u64)),
                desired: Some(serde_json::json!(9000u64)),
                outcome: None,
            }],
        }];
        let result = changes_summary(&ops);
        assert_eq!(result, "mtu 1500→9000");
    }

    // ── format_text_list: adaptive column widths ────────────────────────────────

    /// AC: Long trigger is smart-truncated when terminal is narrow.
    #[test]
    fn test_format_text_list_trigger_column_truncated_when_exceeds_allocated_width() {
        let mut entry = make_entry();
        entry.trigger = Trigger::PolicyApply { source: "test.yaml".to_string() };
        entry.active_policies = vec![PolicySummary {
            name: "very-long-policy-name-exceeding-24".to_string(),
            factory_type: "static".to_string(),
            priority: 100,
        }];
        let output = format_text_list_with_width(&[entry], false, 60);
        let data_row = output.lines().nth(1).unwrap();
        let plain_row = strip_ansi(data_row);
        assert!(
            plain_row.contains("apply (") && plain_row.contains('…'),
            "trigger should be smart-truncated on narrow terminal, got: {}",
            plain_row
        );
    }

    /// AC: run_history_local with 30 entries and count=20 returns exactly 20 (verified via filter_entries).
    #[test]
    fn test_filter_entries_30_entries_count_20_returns_exactly_20() {
        let entries: Vec<JournalEntry> = (0..30).map(|_| make_entry()).collect();
        let args = default_args(); // count=20, no other filters
        let result = filter_entries(entries, &args).unwrap();
        assert_eq!(
            result.len(),
            20,
            "with 30 entries and default count=20, filter_entries should return exactly 20"
        );
    }

    // ── entities_summary: 4-6 entities prioritize lifecycle ──────────────────

    /// AC: With 4-6 entities, lifecycle (add/remove) entities are shown first.
    #[test]
    fn test_entities_summary_4_to_6_entities_prioritizes_lifecycle_over_modify() {
        // 4 entities: 2 modify then 1 add and 1 remove
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
        let result = entities_summary(&ops);
        // Should show 2 lifecycle entities first, then "(+2 more)"
        assert!(
            result.contains("(+2 more)"),
            "4 entities should show 2 entities and '(+2 more)', got: {}",
            result
        );
        // Lifecycle entities (+eth2 or -eth3) should appear before modify entities
        assert!(
            result.contains("+eth2") || result.contains("-eth3"),
            "lifecycle entities should be shown, got: {}",
            result
        );
    }

    // ── format_text_detail: Route diff with metric 0 omits metric ─────────────

    /// AC: Route with metric=0 in diff shows only destination (no "metric 0").
    #[test]
    fn test_format_text_detail_route_with_zero_metric_omits_metric_suffix() {
        let mut entry = make_entry();
        entry.diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth0".to_string(),
                field_changes: vec![SerializableFieldChange {
                    field_name: "routes".to_string(),
                    change_kind: "set".to_string(),
                    current: Some(serde_json::json!([])),
                    desired: Some(serde_json::json!([
                        {"destination": "172.25.14.22/32", "metric": 0}
                    ])),
                    outcome: None,
                }],
            }],
        };
        let output = format_text_detail(&entry);
        let plain = strip_ansi(&output);
        assert!(
            plain.contains("+172.25.14.22/32"),
            "route with metric=0 should show only destination, got:\n{}",
            plain
        );
        assert!(
            !plain.contains("metric 0"),
            "route with metric=0 should not show 'metric 0', got:\n{}",
            plain
        );
    }

    /// AC: Route with non-zero metric in diff shows "destination metric N".
    #[test]
    fn test_format_text_detail_route_with_nonzero_metric_shows_metric() {
        let mut entry = make_entry();
        entry.diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth0".to_string(),
                field_changes: vec![SerializableFieldChange {
                    field_name: "routes".to_string(),
                    change_kind: "set".to_string(),
                    current: Some(serde_json::json!([])),
                    desired: Some(serde_json::json!([
                        {"destination": "10.0.0.0/8", "metric": 100}
                    ])),
                    outcome: None,
                }],
            }],
        };
        let output = format_text_detail(&entry);
        let plain = strip_ansi(&output);
        assert!(
            plain.contains("+10.0.0.0/8 metric 100"),
            "route with metric=100 should show 'destination metric N', got:\n{}",
            plain
        );
    }

    // ── format_text_detail: state after with routes as YAML block sequence ────

    /// AC: State-after with routes renders as YAML block sequence, not JSON objects.
    #[test]
    fn test_format_text_detail_state_after_routes_yaml_block_sequence() {
        let mut entry = make_entry();
        entry.state_after = SerializableStateSet {
            entities: vec![SerializableState {
                entity_type: "ethernet".to_string(),
                selector_name: "eth0".to_string(),
                fields: serde_json::json!({
                    "routes": [
                        {"destination": "0.0.0.0/0", "gateway": "10.0.0.1"}
                    ]
                }),
            }],
        };
        let output = format_text_detail(&entry);
        // Should be YAML block sequence, not JSON array
        assert!(
            !output.contains("[{\"destination\""),
            "routes must not be rendered as JSON inline array/object, got:\n{}",
            output
        );
    }

    // ── outcome_summary: applied with failures ────────────────────────────────

    /// AC: Applied with 0 failures and 0 skips shows just "applied" (no counts).
    #[test]
    fn test_outcome_summary_applied_zero_failures_shows_applied_no_counts() {
        let outcome = ApplyOutcome::Applied { succeeded: 5, failed: 0, skipped: 0 };
        assert_eq!(outcome_summary(&outcome), "applied");
    }

    /// AC: Applied with 2 failures shows "applied (2 fail)".
    #[test]
    fn test_outcome_summary_applied_with_2_failures_shows_applied_2_fail() {
        let outcome = ApplyOutcome::Applied { succeeded: 3, failed: 2, skipped: 1 };
        assert_eq!(outcome_summary(&outcome), "applied (2 fail)");
    }

    // ── format_text_list: seq column content ─────────────────────────────────

    /// AC: Each entry in the text list shows its sequence number in the SEQ column.
    #[test]
    fn test_format_text_list_shows_correct_seq_numbers_for_multiple_entries() {
        let mut e1 = make_entry(); e1.seq = 10;
        let mut e2 = make_entry(); e2.seq = 11;
        let mut e3 = make_entry(); e3.seq = 12;
        let output = format_text_list(&[e1, e2, e3], false);
        assert!(output.contains("10"), "should show seq 10");
        assert!(output.contains("11"), "should show seq 11");
        assert!(output.contains("12"), "should show seq 12");
    }

    // ── format_text_detail: line coloring ─────────────────────────────────────

    static COLOR_MUTEX: Mutex<()> = Mutex::new(());

    /// AC: Detail diff colors entire lines, not just the prefix character.
    ///
    /// The spec requires ANSI codes to wrap the full "-mtu: 1500" / "+mtu: 9000"
    /// strings (including field name and value), not only the leading "-" or "+"
    /// character.  For example the red line must be `\x1b[31m-mtu: 1500\x1b[0m`,
    /// not `\x1b[31m-\x1b[0mmtu: 1500`.
    ///
    /// Verifies that ANSI color codes wrap the full line content, not just the +/- prefix.
    #[test]
    fn test_format_text_detail_scalar_change_colors_entire_line_not_just_prefix() {
        let _lock = COLOR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        colored::control::set_override(true);
        let output = {
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
                        outcome: None,
                    }],
                }],
            };
            format_text_detail(&entry)
        };
        // Restore before asserting so a panic does not leave color override enabled.
        colored::control::unset_override();

        // ANSI red = \x1b[31m ... \x1b[0m  (ESC [ 3 1 m)
        // ANSI green = \x1b[32m ... \x1b[0m  (ESC [ 3 2 m)
        //
        // The full "-mtu: 1500" and "+mtu: 9000" strings (including the field
        // name and the value) must be enclosed inside the ANSI color codes.
        assert!(
            output.contains("\x1b[31m-mtu: 1500\x1b[0m"),
            "the entire '-mtu: 1500' line must be red — ANSI codes must wrap the \
             full content, not only the '-' prefix character; got:\n{}",
            output.escape_debug()
        );
        assert!(
            output.contains("\x1b[32m+mtu: 9000\x1b[0m"),
            "the entire '+mtu: 9000' line must be green — ANSI codes must wrap the \
             full content, not only the '+' prefix character; got:\n{}",
            output.escape_debug()
        );
    }

    // ── trigger_detail_str: ExternalChange format ─────────────────────────────

    /// AC: trigger_detail_str for ExternalChange formats entity names as " (eth0, eth1)".
    ///
    /// The spec detail view shows "Trigger: external (entity1, entity2, ...)" —
    /// the trigger_detail_str must produce the parenthesized list.
    #[test]
    fn test_trigger_detail_str_external_change_formats_entity_names_in_parentheses() {
        let trigger = Trigger::ExternalChange {
            changed_entities: vec!["eth0".to_string(), "eth1".to_string()],
        };
        let detail = trigger_detail_str(&trigger);
        assert_eq!(
            detail, " (eth0, eth1)",
            "ExternalChange trigger_detail_str must produce ' (eth0, eth1)', got: {:?}",
            detail
        );
    }

    /// AC: trigger_detail_str for ExternalChange with no entities returns an empty string.
    #[test]
    fn test_trigger_detail_str_external_change_with_no_entities_returns_empty_string() {
        let trigger = Trigger::ExternalChange { changed_entities: vec![] };
        let detail = trigger_detail_str(&trigger);
        assert_eq!(
            detail, "",
            "ExternalChange trigger_detail_str with empty changed_entities must return '', got: {:?}",
            detail
        );
    }

    /// AC: format_text_detail for an ExternalChange entry includes the changed entity names
    /// in the Trigger line, showing "external (veth-e2e0)".
    ///
    /// The spec detail view shows the changed entities as part of the Trigger line.
    #[test]
    fn test_format_text_detail_external_change_trigger_line_includes_changed_entity_names() {
        let mut entry = make_entry();
        entry.trigger = Trigger::ExternalChange {
            changed_entities: vec!["veth-e2e0".to_string()],
        };
        entry.outcome = ApplyOutcome::Observed;
        let output = format_text_detail(&entry);

        assert!(
            output.contains("external"),
            "detail Trigger line must show 'external' for ExternalChange trigger, got:\n{}",
            output
        );
        assert!(
            output.contains("veth-e2e0"),
            "detail Trigger line must include the changed entity 'veth-e2e0', got:\n{}",
            output
        );
        // The trigger line must combine the display name and the entity list.
        assert!(
            output.contains("external (veth-e2e0)"),
            "detail Trigger line must be 'external (veth-e2e0)', got:\n{}",
            output
        );
    }

    /// AC: format_text_detail for an ExternalChange entry shows "observed" outcome.
    #[test]
    fn test_format_text_detail_external_change_outcome_is_observed() {
        let mut entry = make_entry();
        entry.trigger = Trigger::ExternalChange { changed_entities: vec!["eth0".to_string()] };
        entry.outcome = ApplyOutcome::Observed;
        let output = format_text_detail(&entry);
        assert!(
            output.contains("observed"),
            "detail Outcome line must show 'observed' for ExternalChange entry, got:\n{}",
            output
        );
    }

    // ── Negative offset: --show -1 / --show -3 / beyond size ─────────────────

    /// AC: --show -1 returns the most recent journal entry (seq=30 in a 30-entry journal).
    ///
    /// The implementation does: read_recent(1) → entries[0] = newest → print detail.
    #[test]
    fn test_negative_offset_1_read_recent_returns_most_recent_seq() {
        let dir = temp_dir();
        let mut journal = Journal::open(&dir).unwrap();
        for _ in 0..30 {
            journal.append(make_entry()).unwrap();
        }

        // --show -1 → k=1 → read_recent(1) → [seq=30]
        let entries = journal.read_recent(1).unwrap();
        assert_eq!(entries.len(), 1, "read_recent(1) should return exactly 1 entry");
        assert_eq!(
            entries[0].seq, 30,
            "--show -1 should resolve to seq=30 (the most recent entry)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// AC: --show -3 returns the 3rd-to-last entry (seq=28 in a 30-entry journal).
    ///
    /// The implementation does: read_recent(3) → entries = [30, 29, 28] → last() = 28.
    #[test]
    fn test_negative_offset_3_read_recent_returns_third_to_last_seq() {
        let dir = temp_dir();
        let mut journal = Journal::open(&dir).unwrap();
        for _ in 0..30 {
            journal.append(make_entry()).unwrap();
        }

        // --show -3 → k=3 → read_recent(3) → [seq=30, seq=29, seq=28] → last() = seq=28
        let entries = journal.read_recent(3).unwrap();
        assert_eq!(entries.len(), 3, "read_recent(3) should return exactly 3 entries");
        let last_seq = entries.into_iter().last().unwrap().seq;
        assert_eq!(
            last_seq, 28,
            "--show -3 should resolve to seq=28 (the 3rd-to-last entry)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// AC: --show -1 on a populated journal returns exit code 0.
    #[tokio::test]
    async fn test_run_history_local_show_negative_1_returns_exit_code_0() {
        let dir = temp_dir();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let mut journal = Journal::open(&dir).unwrap();
        for _ in 0..30 {
            journal.append(make_entry()).unwrap();
        }

        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", dir.to_str().unwrap()) };
        let mut args = default_args();
        args.show = Some(-1);
        let result = run_history_local(&args).await.unwrap();
        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            result,
            ExitCode::from(0u8),
            "--show -1 should return exit code 0 for a 30-entry journal"
        );
    }

    /// AC: --show -3 on a 30-entry journal returns exit code 0.
    #[tokio::test]
    async fn test_run_history_local_show_negative_3_returns_exit_code_0() {
        let dir = temp_dir();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let mut journal = Journal::open(&dir).unwrap();
        for _ in 0..30 {
            journal.append(make_entry()).unwrap();
        }

        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", dir.to_str().unwrap()) };
        let mut args = default_args();
        args.show = Some(-3);
        let result = run_history_local(&args).await.unwrap();
        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            result,
            ExitCode::from(0u8),
            "--show -3 should return exit code 0 for a 30-entry journal"
        );
    }

    /// AC: --show -10 on a 5-entry journal (offset exceeds size) returns exit code 1.
    ///
    /// When read_recent(10) returns only 5 entries (< k=10), "Entry not found" is printed
    /// and exit code 1 is returned.
    #[tokio::test]
    async fn test_run_history_local_show_negative_offset_beyond_journal_size_returns_exit_code_1()
    {
        let dir = temp_dir();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let mut journal = Journal::open(&dir).unwrap();
        for _ in 0..5 {
            journal.append(make_entry()).unwrap();
        }

        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", dir.to_str().unwrap()) };
        let mut args = default_args();
        args.show = Some(-10); // offset larger than journal size
        let result = run_history_local(&args).await.unwrap();
        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            result,
            ExitCode::from(1u8),
            "--show -10 with only 5 entries should return exit code 1 (Entry not found)"
        );
    }

    /// AC: read_recent(k) where k > journal size returns fewer than k entries.
    ///
    /// This validates the boundary check: entries.len() < k → "Entry not found".
    #[test]
    fn test_read_recent_beyond_journal_size_returns_fewer_entries_than_requested() {
        let dir = temp_dir();
        let mut journal = Journal::open(&dir).unwrap();
        for _ in 0..5 {
            journal.append(make_entry()).unwrap();
        }

        let entries = journal.read_recent(10).unwrap();
        assert!(
            entries.len() < 10,
            "read_recent(10) on a 5-entry journal should return fewer than 10 entries, got {}",
            entries.len()
        );
        assert_eq!(
            entries.len(),
            5,
            "read_recent(10) on a 5-entry journal should return exactly 5 entries"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// AC: --show 0 returns exit code 1 (seq=0 is never a valid entry).
    #[tokio::test]
    async fn test_run_history_local_show_seq_0_returns_exit_code_1() {
        let dir = temp_dir();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let mut journal = Journal::open(&dir).unwrap();
        journal.append(make_entry()).unwrap();

        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", dir.to_str().unwrap()) };
        let mut args = default_args();
        args.show = Some(0);
        let result = run_history_local(&args).await.unwrap();
        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            result,
            ExitCode::from(1u8),
            "--show 0 should return exit code 1 (seq 0 is not a valid entry)"
        );
    }

    // ── Feature: FAIL prefix ──────────────────────────────────────────────────

    /// AC: External change has no FAIL prefix in the CHANGES column.
    ///
    /// External changes produce ApplyOutcome::Observed, which does not match the
    /// Applied { failed, .. } if *failed > 0 pattern — so FAIL should never appear.
    #[test]
    fn test_format_text_list_external_change_has_no_fail_prefix() {
        let mut entry = make_entry();
        entry.trigger = Trigger::ExternalChange { changed_entities: vec!["eth0".to_string()] };
        entry.outcome = ApplyOutcome::Observed;
        entry.diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth0".to_string(),
                field_changes: vec![SerializableFieldChange {
                    field_name: "mtu".to_string(),
                    change_kind: "set".to_string(),
                    current: Some(serde_json::json!(1400u64)),
                    desired: Some(serde_json::json!(1500u64)),
                    outcome: None,
                }],
            }],
        };
        let output = format_text_list(&[entry], false);
        let data_row = output.lines().nth(1).unwrap();
        let plain = strip_ansi(data_row);
        assert!(
            !plain.contains("FAIL"),
            "external change should not show FAIL prefix, got: {}",
            plain
        );
    }

    /// AC: FAIL(N) count is the per-field failure count, not just the legacy outcome count.
    ///
    /// When field changes have explicit outcome="failed", count_failed_fields reads
    /// those per-field annotations and uses that count in FAIL(N).
    #[test]
    fn test_format_text_list_fail_count_reflects_per_field_failures() {
        let mut entry = make_entry();
        entry.outcome = ApplyOutcome::Applied { succeeded: 2, failed: 3, skipped: 0 };
        entry.diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth0".to_string(),
                field_changes: vec![
                    SerializableFieldChange {
                        field_name: "mtu".to_string(),
                        change_kind: "set".to_string(),
                        current: Some(serde_json::json!(1500u64)),
                        desired: Some(serde_json::json!(9000u64)),
                        outcome: Some("failed".to_string()),
                    },
                    SerializableFieldChange {
                        field_name: "speed".to_string(),
                        change_kind: "set".to_string(),
                        current: Some(serde_json::json!(100u64)),
                        desired: Some(serde_json::json!(1000u64)),
                        outcome: Some("failed".to_string()),
                    },
                    SerializableFieldChange {
                        field_name: "state".to_string(),
                        change_kind: "set".to_string(),
                        current: Some(serde_json::json!("down")),
                        desired: Some(serde_json::json!("up")),
                        outcome: Some("failed".to_string()),
                    },
                ],
            }],
        };
        let output = format_text_list(&[entry], false);
        let data_row = output.lines().nth(1).unwrap();
        let plain = strip_ansi(data_row);
        assert!(
            plain.contains("FAIL(3)"),
            "FAIL prefix should show per-field failure count FAIL(3), got: {}",
            plain
        );
    }

    /// AC: Multiple failures show correct count — FAIL(1) when only 1 field failed.
    #[test]
    fn test_format_text_list_fail_count_1_when_only_1_field_failed_and_2_skipped() {
        let mut entry = make_entry();
        entry.outcome = ApplyOutcome::Applied { succeeded: 0, failed: 1, skipped: 2 };
        entry.diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth0".to_string(),
                field_changes: vec![
                    SerializableFieldChange {
                        field_name: "mtu".to_string(),
                        change_kind: "set".to_string(),
                        current: Some(serde_json::json!(1400u64)),
                        desired: Some(serde_json::json!(9000u64)),
                        outcome: Some("skipped".to_string()),
                    },
                    SerializableFieldChange {
                        field_name: "addresses".to_string(),
                        change_kind: "set".to_string(),
                        current: Some(serde_json::json!([])),
                        desired: Some(serde_json::json!(["0.0.0.0/0"])),
                        outcome: Some("failed".to_string()),
                    },
                    SerializableFieldChange {
                        field_name: "state".to_string(),
                        change_kind: "set".to_string(),
                        current: Some(serde_json::json!("down")),
                        desired: Some(serde_json::json!("up")),
                        outcome: Some("skipped".to_string()),
                    },
                ],
            }],
        };
        let output = format_text_list(&[entry], false);
        let data_row = output.lines().nth(1).unwrap();
        let plain = strip_ansi(data_row);
        assert!(
            plain.contains("FAIL(1)"),
            "should show FAIL(1) when only 1 of 3 field changes failed (2 skipped), got: {}",
            plain
        );
    }

    // ── Feature: per-field outcome annotations ────────────────────────────────

    /// AC: Per-field outcome annotations appear for failed/skipped fields in mixed results.
    ///
    /// When any field change has outcome "failed" or "skipped", annotations appear.
    #[test]
    fn test_format_text_detail_per_field_annotations_on_mixed_outcomes() {
        let mut entry = make_entry();
        entry.outcome = ApplyOutcome::Applied { succeeded: 0, failed: 1, skipped: 2 };
        entry.diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "enp7s0".to_string(),
                field_changes: vec![
                    SerializableFieldChange {
                        field_name: "mtu".to_string(),
                        change_kind: "set".to_string(),
                        current: Some(serde_json::json!(1492u64)),
                        desired: Some(serde_json::json!(9000u64)),
                        outcome: Some("skipped".to_string()),
                    },
                    SerializableFieldChange {
                        field_name: "speed".to_string(),
                        change_kind: "set".to_string(),
                        current: Some(serde_json::json!(100u64)),
                        desired: Some(serde_json::json!(1000u64)),
                        outcome: Some("failed".to_string()),
                    },
                    SerializableFieldChange {
                        field_name: "state".to_string(),
                        change_kind: "set".to_string(),
                        current: Some(serde_json::json!("down")),
                        desired: Some(serde_json::json!("up")),
                        outcome: Some("skipped".to_string()),
                    },
                ],
            }],
        };
        let output = format_text_detail(&entry);
        let plain = strip_ansi(&output);
        assert!(
            plain.contains("[skipped]"),
            "mixed outcomes must show '[skipped]' annotation, got:\n{plain}"
        );
        assert!(
            plain.contains("[failed]"),
            "mixed outcomes must show '[failed]' annotation, got:\n{plain}"
        );
    }

    /// AC: [applied] annotation appears only when outcomes are mixed (some failed or skipped).
    ///
    /// The spec says [applied] is shown only when the reader needs to distinguish
    /// applied fields from failed/skipped ones.
    #[test]
    fn test_format_text_detail_applied_annotation_shown_only_with_mixed_outcomes() {
        let mut entry = make_entry();
        entry.outcome = ApplyOutcome::Applied { succeeded: 1, failed: 1, skipped: 0 };
        entry.diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth0".to_string(),
                field_changes: vec![
                    SerializableFieldChange {
                        field_name: "mtu".to_string(),
                        change_kind: "set".to_string(),
                        current: Some(serde_json::json!(1500u64)),
                        desired: Some(serde_json::json!(9000u64)),
                        outcome: Some("applied".to_string()),
                    },
                    SerializableFieldChange {
                        field_name: "speed".to_string(),
                        change_kind: "set".to_string(),
                        current: Some(serde_json::json!(100u64)),
                        desired: Some(serde_json::json!(1000u64)),
                        outcome: Some("failed".to_string()),
                    },
                ],
            }],
        };
        let output = format_text_detail(&entry);
        let plain = strip_ansi(&output);
        assert!(
            plain.contains("[applied]"),
            "mixed outcomes must show '[applied]' annotation for applied field, got:\n{plain}"
        );
        assert!(
            plain.contains("[failed]"),
            "mixed outcomes must show '[failed]' annotation, got:\n{plain}"
        );
    }

    /// AC: No annotations when all fields succeeded.
    ///
    /// When every field change has outcome "applied" (no failed or skipped),
    /// the Outcome line is sufficient and no per-field annotations appear.
    #[test]
    fn test_format_text_detail_no_annotations_when_all_fields_succeeded() {
        let mut entry = make_entry();
        entry.outcome = ApplyOutcome::Applied { succeeded: 2, failed: 0, skipped: 0 };
        entry.diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth0".to_string(),
                field_changes: vec![
                    SerializableFieldChange {
                        field_name: "mtu".to_string(),
                        change_kind: "set".to_string(),
                        current: Some(serde_json::json!(1500u64)),
                        desired: Some(serde_json::json!(9000u64)),
                        outcome: Some("applied".to_string()),
                    },
                    SerializableFieldChange {
                        field_name: "speed".to_string(),
                        change_kind: "set".to_string(),
                        current: Some(serde_json::json!(100u64)),
                        desired: Some(serde_json::json!(1000u64)),
                        outcome: Some("applied".to_string()),
                    },
                ],
            }],
        };
        let output = format_text_detail(&entry);
        let plain = strip_ansi(&output);
        assert!(
            !plain.contains("[applied]"),
            "all-success should NOT show '[applied]' annotation, got:\n{plain}"
        );
        assert!(
            !plain.contains("[failed]"),
            "all-success should NOT show '[failed]' annotation, got:\n{plain}"
        );
        assert!(
            !plain.contains("[skipped]"),
            "all-success should NOT show '[skipped]' annotation, got:\n{plain}"
        );
    }

    /// AC: External change entries (Observed outcome) have no per-field outcome annotations.
    #[test]
    fn test_format_text_detail_external_change_has_no_field_outcome_annotations() {
        let mut entry = make_entry();
        entry.trigger = Trigger::ExternalChange { changed_entities: vec!["eth0".to_string()] };
        entry.outcome = ApplyOutcome::Observed;
        entry.diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth0".to_string(),
                field_changes: vec![SerializableFieldChange {
                    field_name: "mtu".to_string(),
                    change_kind: "set".to_string(),
                    current: Some(serde_json::json!(1400u64)),
                    desired: Some(serde_json::json!(1500u64)),
                    // Even with a "skipped" outcome, external changes must show no annotations
                    outcome: Some("skipped".to_string()),
                }],
            }],
        };
        let output = format_text_detail(&entry);
        let plain = strip_ansi(&output);
        assert!(
            !plain.contains("[failed]"),
            "external change should not show '[failed]' annotation, got:\n{plain}"
        );
        assert!(
            !plain.contains("[skipped]"),
            "external change should not show '[skipped]' annotation, got:\n{plain}"
        );
        assert!(
            !plain.contains("[applied]"),
            "external change should not show '[applied]' annotation, got:\n{plain}"
        );
    }

    /// AC: Per-field annotation colors: [failed]=red, [skipped]=yellow, [applied]=green.
    ///
    /// The spec requires colored annotations to help operators distinguish outcomes at a glance.
    #[test]
    fn test_format_text_detail_annotation_colors_are_correct() {
        let _lock = COLOR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        colored::control::set_override(true);
        let output = {
            let mut entry = make_entry();
            entry.outcome = ApplyOutcome::Applied { succeeded: 1, failed: 1, skipped: 1 };
            entry.diff = SerializableDiff {
                operations: vec![SerializableDiffOp {
                    kind: "modify".to_string(),
                    entity_type: "ethernet".to_string(),
                    entity_name: "eth0".to_string(),
                    field_changes: vec![
                        SerializableFieldChange {
                            field_name: "mtu".to_string(),
                            change_kind: "set".to_string(),
                            current: Some(serde_json::json!(1500u64)),
                            desired: Some(serde_json::json!(9000u64)),
                            outcome: Some("applied".to_string()),
                        },
                        SerializableFieldChange {
                            field_name: "speed".to_string(),
                            change_kind: "set".to_string(),
                            current: Some(serde_json::json!(100u64)),
                            desired: Some(serde_json::json!(1000u64)),
                            outcome: Some("failed".to_string()),
                        },
                        SerializableFieldChange {
                            field_name: "state".to_string(),
                            change_kind: "set".to_string(),
                            current: Some(serde_json::json!("down")),
                            desired: Some(serde_json::json!("up")),
                            outcome: Some("skipped".to_string()),
                        },
                    ],
                }],
            };
            format_text_detail(&entry)
        };
        colored::control::unset_override();

        // ANSI red = \x1b[31m...m, yellow = \x1b[33m, green = \x1b[32m
        // The annotation format is: "  [<colored_text>]"
        // where <colored_text> = "\x1b[3Xm<word>\x1b[0m"
        assert!(
            output.contains("\x1b[31mfailed\x1b[0m"),
            "[failed] annotation must use red ANSI code (\\x1b[31m), got:\n{}",
            output.escape_debug()
        );
        assert!(
            output.contains("\x1b[33mskipped\x1b[0m"),
            "[skipped] annotation must use yellow ANSI code (\\x1b[33m), got:\n{}",
            output.escape_debug()
        );
        assert!(
            output.contains("\x1b[32mapplied\x1b[0m"),
            "[applied] annotation must use green ANSI code (\\x1b[32m), got:\n{}",
            output.escape_debug()
        );
    }

    // ── Feature: Dynamic column widths ────────────────────────────────────────

    /// AC: Column widths adapt to content — not padded to their maximum caps.
    ///
    /// With a single-digit seq number (1 char), the SEQ column width is
    /// max("SEQ".len(), 1) = 3, not the cap of 7. The header "SEQ" is
    /// formatted at exactly 3 chars with no extra padding.
    #[test]
    fn test_format_text_list_seq_column_width_adapts_to_content_not_capped() {
        let mut entry = make_entry();
        entry.seq = 7; // single-digit
        let output = format_text_list(&[entry], false);

        let header = output.lines().next().unwrap();
        // With w_seq=3, pad_or_truncate("SEQ", 3)="SEQ", then "  " separator before TIMESTAMP.
        // The header should start with "SEQ  " (3 + 2 sep), not "SEQ    " (7 + 2 sep).
        assert!(
            header.starts_with("SEQ  "),
            "SEQ column width should be 3 (header width) with single-digit seq, got header: {}",
            header
        );
        assert!(
            !header.starts_with("SEQ   "),
            "SEQ column should NOT be padded to the cap of 7 with single-digit seq, got header: {}",
            header
        );
    }

    /// AC: ENTITIES column width adapts to content (header width = 8 is the minimum).
    ///
    /// With entity names "eth0" (4 chars), the ENTITIES column width is
    /// max("ENTITIES".len(), 4) = 8. The header is padded to exactly 8 chars.
    #[test]
    fn test_format_text_list_entities_column_width_adapts_to_content() {
        let mut entry = make_entry();
        entry.seq = 1;
        // Single entity "eth0" (4 chars), shorter than the "ENTITIES" header (8 chars)
        entry.diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth0".to_string(),
                field_changes: vec![],
            }],
        };
        let output = format_text_list(&[entry], false);
        let data_row = output.lines().nth(1).unwrap();
        let plain = strip_ansi(data_row);

        // The data row should contain "eth0" padded to 8 chars (matching "ENTITIES" header),
        // not to the cap of 24 chars. Count spaces after "eth0" until the next non-space field.
        // With w_ent=8, "eth0" is padded to 8 chars = "eth0    " (4 + 4 spaces).
        // But we just verify the data contains "eth0" and the column is not over-padded to 24.
        assert!(
            plain.contains("eth0"),
            "data row must contain entity name 'eth0', got: {}",
            plain
        );

        // Find "eth0" in the plain row and check that there are <=20 spaces after it
        // (i.e., it's not padded to the max cap of 24).
        if let Some(pos) = plain.find("eth0") {
            let after_entity = &plain[pos + 4..];
            let spaces_after = after_entity.chars().take_while(|c| *c == ' ').count();
            // With w_ent=8, entity is padded to 8 chars. eth0=4 chars, so 4 spaces of padding,
            // then 2 separator spaces = 6 total spaces before CHANGES. This is < 20.
            assert!(
                spaces_after < 20,
                "entity column should not be padded to the cap of 24; \
                 found {} spaces after 'eth0' — expected ~6 (4 padding + 2 sep), got row: {}",
                spaces_after, plain
            );
        }
    }

    /// AC: On a narrow terminal, a long trigger is smart-truncated with the policy name
    /// shortened or reduced to a count, rather than cut mid-word.
    #[test]
    fn test_format_text_list_trigger_column_smart_truncated_on_narrow_terminal() {
        let mut entry = make_entry();
        entry.trigger = Trigger::PolicyApply { source: "test.yaml".to_string() };
        entry.active_policies = vec![PolicySummary {
            name: "a-very-long-policy-name-that-exceeds".to_string(),
            factory_type: "static".to_string(),
            priority: 100,
        }];
        let output = format_text_list_with_width(&[entry], false, 60);
        let data_row = output.lines().nth(1).unwrap();
        let plain = strip_ansi(data_row);
        assert!(
            plain.contains("apply (") && plain.contains("…"),
            "long trigger on narrow terminal should be smart-truncated, got: {}",
            plain
        );
    }

    // ── Feature: format_text_detail: list field annotations ──────────────────

    /// AC: List field changes (e.g. addresses) show the same annotation for each element.
    ///
    /// When a list field change has outcome "skipped", each changed element line gets [skipped].
    #[test]
    fn test_format_text_detail_list_field_annotation_per_element() {
        let mut entry = make_entry();
        entry.outcome = ApplyOutcome::Applied { succeeded: 0, failed: 0, skipped: 1 };
        entry.diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "enp7s0".to_string(),
                field_changes: vec![
                    SerializableFieldChange {
                        field_name: "dns_servers".to_string(),
                        change_kind: "set".to_string(),
                        current: Some(serde_json::json!([])),
                        desired: Some(serde_json::json!(["192.168.122.1"])),
                        outcome: Some("skipped".to_string()),
                    },
                    // A separate field that failed to trigger mixed-outcome annotations
                    SerializableFieldChange {
                        field_name: "mtu".to_string(),
                        change_kind: "set".to_string(),
                        current: Some(serde_json::json!(1492u64)),
                        desired: Some(serde_json::json!(9000u64)),
                        outcome: Some("failed".to_string()),
                    },
                ],
            }],
        };
        let output = format_text_detail(&entry);
        let plain = strip_ansi(&output);
        assert!(
            plain.contains("dns_servers:"),
            "list field must show 'dns_servers:' header, got:\n{plain}"
        );
        assert!(
            plain.contains("+192.168.122.1"),
            "added element must show '+192.168.122.1', got:\n{plain}"
        );
        assert!(
            plain.contains("[skipped]"),
            "list field element with 'skipped' outcome must show '[skipped]', got:\n{plain}"
        );
    }

    // ── Feature: format_text_detail: outcome detail breakdown ────────────────

    /// AC: outcome_detail for Applied shows "applied (N succeeded, M failed, K skipped)".
    #[test]
    fn test_outcome_detail_applied_with_all_counts() {
        let outcome = ApplyOutcome::Applied { succeeded: 1, failed: 1, skipped: 2 };
        let result = outcome_detail(&outcome);
        assert_eq!(result, "applied (1 succeeded, 1 failed, 2 skipped)");
    }

    /// AC: outcome_detail for Observed shows "observed".
    #[test]
    fn test_outcome_detail_observed_shows_observed() {
        let result = outcome_detail(&ApplyOutcome::Observed);
        assert_eq!(result, "observed");
    }

    /// AC: format_text_detail shows the outcome with full breakdown including skipped count.
    #[test]
    fn test_format_text_detail_outcome_shows_full_breakdown_with_skipped() {
        let mut entry = make_entry();
        entry.outcome = ApplyOutcome::Applied { succeeded: 0, failed: 1, skipped: 2 };
        let output = format_text_detail(&entry);
        assert!(
            output.contains("0 succeeded"),
            "detail outcome must show '0 succeeded', got:\n{}",
            output
        );
        assert!(
            output.contains("1 failed"),
            "detail outcome must show '1 failed', got:\n{}",
            output
        );
        assert!(
            output.contains("2 skipped"),
            "detail outcome must show '2 skipped', got:\n{}",
            output
        );
    }

    // ── Feature: format_text_list reverse chronological ordering ─────────────

    /// AC: The 20 most recent entries are shown in reverse chronological order (newest first).
    ///
    /// Entries read from the journal via read_recent are already newest-first;
    /// format_text_list must preserve that order (seq decreasing down the rows).
    #[test]
    fn test_format_text_list_entries_shown_in_reverse_chronological_order() {
        let mut entries: Vec<JournalEntry> = Vec::new();
        for seq in [10u64, 9, 8, 7, 6] {
            let mut e = make_entry();
            e.seq = seq;
            entries.push(e);
        }
        let output = format_text_list(&entries, false);
        let lines: Vec<&str> = output.lines().collect();
        // Skip header (line 0), data rows start at line 1
        let seqs: Vec<u64> = lines[1..]
            .iter()
            .filter(|l| !l.contains("daemon restart"))
            .map(|l| {
                l.split_whitespace()
                    .next()
                    .unwrap_or("0")
                    .parse::<u64>()
                    .unwrap_or(0)
            })
            .collect();
        assert_eq!(seqs.len(), 5, "should have 5 data rows");
        for i in 0..seqs.len() - 1 {
            assert!(
                seqs[i] > seqs[i + 1],
                "entries must be in reverse chronological order (seq {} > {}), got: {:?}",
                seqs[i], seqs[i + 1], seqs
            );
        }
    }

    // ── Feature: filter_entries -- all three filters together ─────────────────

    /// AC: Combine all three filters (--since, --trigger, -s name=X) with AND logic.
    ///
    /// Only entries that pass all three filters are returned.
    #[test]
    fn test_filter_entries_all_three_filters_combined_and_logic() {
        let matching = {
            let mut e = make_entry_with_entity("eth0");
            e.timestamp = Utc::now() - Duration::minutes(30);
            e.trigger = Trigger::PolicyApply { source: "test.yaml".to_string() };
            e
        };
        let wrong_time = {
            let mut e = make_entry_with_entity("eth0");
            e.timestamp = Utc::now() - Duration::hours(2); // fails --since 1h
            e.trigger = Trigger::PolicyApply { source: "test.yaml".to_string() };
            e
        };
        let wrong_trigger = {
            let mut e = make_entry_with_entity("eth0");
            e.timestamp = Utc::now() - Duration::minutes(30);
            e.trigger = Trigger::ExternalChange { changed_entities: vec![] }; // fails --trigger apply
            e
        };
        let wrong_entity = {
            let mut e = make_entry_with_entity("eth1"); // fails -s name=eth0
            e.timestamp = Utc::now() - Duration::minutes(30);
            e.trigger = Trigger::PolicyApply { source: "test.yaml".to_string() };
            e
        };

        let mut args = default_args();
        args.since = Some("1h".to_string());
        args.trigger = Some("apply".to_string());
        args.selector = vec![("name".to_string(), "eth0".to_string())];

        let result = filter_entries(
            vec![matching, wrong_time, wrong_trigger, wrong_entity],
            &args,
        )
        .unwrap();
        assert_eq!(
            result.len(),
            1,
            "combined --since 1h --trigger apply -s name=eth0 must use AND logic, returning only 1 entry"
        );
    }

    // ── format_trigger_column_fitted ──────────────────────────────────────────

    #[test]
    fn test_format_trigger_column_fitted_returns_full_when_fits() {
        let mut entry = make_entry();
        entry.active_policies = vec![PolicySummary {
            name: "short".to_string(),
            factory_type: "static".to_string(),
            priority: 100,
        }];
        let result = format_trigger_column_fitted(&entry, 40);
        assert_eq!(result, "apply (short)");
    }

    #[test]
    fn test_format_trigger_column_fitted_single_policy_truncates_name() {
        let mut entry = make_entry();
        entry.active_policies = vec![PolicySummary {
            name: "very-long-policy-name-that-wont-fit".to_string(),
            factory_type: "static".to_string(),
            priority: 100,
        }];
        let result = format_trigger_column_fitted(&entry, 20);
        assert!(
            result.contains("apply (") && result.contains("…") && result.ends_with(')'),
            "should truncate policy name with ellipsis inside parens, got: {}",
            result
        );
        assert!(result.chars().count() <= 20, "must fit in 20 chars, got: {}", result);
    }

    #[test]
    fn test_format_trigger_column_fitted_multiple_policies_shows_count() {
        let mut entry = make_entry();
        entry.active_policies = vec![
            PolicySummary { name: "server-network".to_string(), factory_type: "static".to_string(), priority: 100 },
            PolicySummary { name: "server-network2".to_string(), factory_type: "static".to_string(), priority: 90 },
            PolicySummary { name: "server-network3".to_string(), factory_type: "static".to_string(), priority: 80 },
        ];
        let result = format_trigger_column_fitted(&entry, 24);
        assert!(
            result.contains("+2)"),
            "should show +2 for hidden policies, got: {}",
            result
        );
        assert!(result.chars().count() <= 24, "must fit in 24 chars, got: {}", result);
    }

    #[test]
    fn test_format_trigger_column_fitted_multiple_policies_expands_when_room() {
        let mut entry = make_entry();
        entry.active_policies = vec![
            PolicySummary { name: "server-network".to_string(), factory_type: "static".to_string(), priority: 100 },
            PolicySummary { name: "server-network2".to_string(), factory_type: "static".to_string(), priority: 90 },
        ];
        let result = format_trigger_column_fitted(&entry, 80);
        assert_eq!(
            result, "apply (server-network, server-network2)",
            "with enough room, all policy names should be shown"
        );
    }

    #[test]
    fn test_format_trigger_column_fitted_very_narrow_falls_back() {
        let mut entry = make_entry();
        entry.active_policies = vec![PolicySummary {
            name: "x".to_string(),
            factory_type: "static".to_string(),
            priority: 100,
        }];
        let result = format_trigger_column_fitted(&entry, 8);
        assert!(result.chars().count() <= 8, "must fit in 8 chars, got: {}", result);
    }

    #[test]
    fn test_format_trigger_column_fitted_non_apply_unchanged() {
        let mut entry = make_entry();
        entry.trigger = Trigger::DaemonStartup;
        let result = format_trigger_column_fitted(&entry, 40);
        assert_eq!(result, "daemon-startup");
    }

    // ── entities_summary_fitted ──────────────────────────────────────────────

    #[test]
    fn test_entities_summary_fitted_returns_full_when_fits() {
        let ops = vec![
            SerializableDiffOp { kind: "modify".to_string(), entity_type: "ethernet".to_string(), entity_name: "eth0".to_string(), field_changes: vec![] },
            SerializableDiffOp { kind: "modify".to_string(), entity_type: "ethernet".to_string(), entity_name: "eth1".to_string(), field_changes: vec![] },
        ];
        let result = entities_summary_fitted(&ops, &[], 40);
        assert_eq!(result, "eth0, eth1");
    }

    #[test]
    fn test_entities_summary_fitted_three_entities_degrades_to_two_plus_count() {
        let ops = vec![
            SerializableDiffOp { kind: "modify".to_string(), entity_type: "ethernet".to_string(), entity_name: "eth0".to_string(), field_changes: vec![] },
            SerializableDiffOp { kind: "modify".to_string(), entity_type: "ethernet".to_string(), entity_name: "eth1".to_string(), field_changes: vec![] },
            SerializableDiffOp { kind: "modify".to_string(), entity_type: "ethernet".to_string(), entity_name: "wlan0".to_string(), field_changes: vec![] },
        ];
        let result = entities_summary_fitted(&ops, &[], 14);
        assert!(
            result.contains("+1…") || result.contains("+2…"),
            "should degrade to fewer items with +N, got: {}",
            result
        );
        assert!(result.chars().count() <= 14, "must fit in 14 chars, got: {}", result);
    }

    #[test]
    fn test_entities_summary_fitted_narrow_degrades_to_pure_count() {
        let ops: Vec<SerializableDiffOp> = (0..5)
            .map(|i| SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: format!("eth{}", i),
                field_changes: vec![],
            })
            .collect();
        let result = entities_summary_fitted(&ops, &[], 14);
        assert!(
            result.contains("entities") || result.contains("+"),
            "narrow budget should show aggregate or count, got: {}",
            result
        );
        assert!(result.chars().count() <= 14, "must fit in 14 chars, got: {}", result);
    }

    #[test]
    fn test_entities_summary_fitted_already_compact_passes_through() {
        let ops: Vec<SerializableDiffOp> = (0..8)
            .map(|i| SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: format!("eth{}", i),
                field_changes: vec![],
            })
            .collect();
        let full = entities_summary(&ops);
        let result = entities_summary_fitted(&ops, &[], 40);
        assert_eq!(result, full, "compact output should pass through unchanged");
    }

    // ── format_text_list_with_width ──────────────────────────────────────────

    #[test]
    fn test_format_text_list_with_width_narrow_terminal_all_columns_present() {
        let entry = make_entry_with_entity("eth0");
        let output = format_text_list_with_width(&[entry], false, 60);
        let header = output.lines().next().unwrap();
        assert!(header.contains("SEQ"), "header missing SEQ");
        assert!(header.contains("TIMESTAMP"), "header missing TIMESTAMP");
        assert!(header.contains("TRIGGER"), "header missing TRIGGER");
        assert!(header.contains("ENTITIES"), "header missing ENTITIES");
        assert!(header.contains("CHANGES"), "header missing CHANGES");
    }

    #[test]
    fn test_format_text_list_with_width_wide_terminal_no_truncation() {
        let mut entry = make_entry();
        entry.active_policies = vec![PolicySummary {
            name: "server-network".to_string(),
            factory_type: "static".to_string(),
            priority: 100,
        }];
        entry.diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth0".to_string(),
                field_changes: vec![],
            }],
        };
        let output = format_text_list_with_width(&[entry], false, 200);
        let data_row = output.lines().nth(1).unwrap();
        let plain = strip_ansi(data_row);
        assert!(
            plain.contains("apply (server-network)"),
            "wide terminal should show full trigger, got: {}",
            plain
        );
        assert!(!plain.contains('…'), "wide terminal should not truncate, got: {}", plain);
    }

    #[test]
    fn test_format_text_list_with_width_trigger_gets_more_space_on_wide_terminal() {
        let mut entry = make_entry();
        entry.active_policies = vec![PolicySummary {
            name: "a-moderately-long-policy-name".to_string(),
            factory_type: "static".to_string(),
            priority: 100,
        }];
        entry.diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth0".to_string(),
                field_changes: vec![],
            }],
        };
        // "apply (a-moderately-long-policy-name)" = 38 chars, exceeds old 24-char cap
        let output = format_text_list_with_width(&[entry], false, 160);
        let data_row = output.lines().nth(1).unwrap();
        let plain = strip_ansi(data_row);
        assert!(
            plain.contains("a-moderately-long-policy-name"),
            "160-col terminal should show full policy name that exceeds old 24-char cap, got: {}",
            plain
        );
    }

    #[test]
    fn test_format_text_list_with_width_200_cap_respected() {
        let mut entry = make_entry();
        entry.diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "ethernet".to_string(),
                entity_name: "eth0".to_string(),
                field_changes: vec![],
            }],
        };
        let output_200 = format_text_list_with_width(&[entry.clone()], false, 200);
        let output_300 = format_text_list_with_width(&[entry], false, 300);
        let row_200 = strip_ansi(output_200.lines().nth(1).unwrap());
        let row_300 = strip_ansi(output_300.lines().nth(1).unwrap());
        assert_eq!(
            row_200.trim_end().len(),
            row_300.trim_end().len(),
            "terminal widths beyond 200 should produce the same output"
        );
    }
}
