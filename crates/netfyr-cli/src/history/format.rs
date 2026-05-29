//! Text and JSON formatting for history output.
//!
//! High-level functions that produce the final text or JSON output for
//! `netfyr history` list and detail views.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use colored::Colorize;
use indexmap::IndexMap;

use netfyr_journal::{
    ApplyOutcome, JournalEntry, SerializableFieldChange, SerializableState, Trigger,
};

use super::display::*;

// ── Timestamp formatting ─────────────────────────────────────────────────────

pub(crate) fn format_timestamp(ts: DateTime<Utc>, now: DateTime<Utc>, absolute: bool) -> String {
    if absolute {
        return ts.format("%Y-%m-%d %H:%M:%S").to_string();
    }
    let secs = (now - ts).num_seconds().max(0);
    if secs < 60 {
        format!("{} sec ago", secs)
    } else if secs < 3600 {
        format!("{} min ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        let ts_date = ts.date_naive();
        let now_date = now.date_naive();
        if ts_date == now_date - chrono::Duration::days(1) {
            format!("yesterday {}", ts.format("%H:%M"))
        } else {
            ts.format("%Y-%m-%d %H:%M").to_string()
        }
    }
}

// ── Trigger column formatting ────────────────────────────────────────────────

pub(crate) fn format_trigger_column(entry: &JournalEntry) -> String {
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

pub(crate) fn format_trigger_column_fitted(entry: &JournalEntry, max_width: usize) -> String {
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

// ── Row cells ────────────────────────────────────────────────────────────────

struct RowCells {
    seq: String,
    ts: String,
    entities: String,
    changes: String,
    is_daemon_startup: bool,
}

// ── Text list formatting ─────────────────────────────────────────────────────

pub(crate) fn format_text_list(entries: &[JournalEntry], absolute_timestamps: bool) -> String {
    format_text_list_with_width(entries, absolute_timestamps, get_terminal_width())
}

fn get_terminal_width() -> usize {
    use terminal_size::{terminal_size, Width};
    terminal_size()
        .map(|(Width(w), _)| w as usize)
        .unwrap_or(120)
}

pub(crate) fn format_text_list_with_width(
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
    let ideal_changes =
        cw("CHANGES").max(rows.iter().map(|r| cw(&r.changes)).max().unwrap_or(0));

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
        if let Some(t_extra) = (remaining * excess_trig).checked_div(excess_total) {
            let e_extra = remaining * excess_ent / excess_total;
            let t = MIN_TRIG + t_extra;
            let e = MIN_ENT + e_extra;
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
        let entities_fitted =
            entities_summary_fitted(&entry.diff.operations, &entry.state_after.entities, w_ent);
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

// ── Text detail formatting ───────────────────────────────────────────────────

pub(crate) fn format_text_detail(entry: &JournalEntry) -> String {
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
                    && matches!(fc.outcome.as_deref(), Some("failed") | Some("skipped"))
            });
    let show_annotations = has_mixed_outcomes;

    out.push_str("Diff:\n");
    if entry.diff.operations.is_empty() {
        out.push_str("  (no changes)\n");
    } else {
        for op in &entry.diff.operations {
            let header = match op.kind.as_str() {
                "add" => format!("  + {} {}\n", op.entity_type, op.entity_name)
                    .green()
                    .to_string(),
                "remove" => format!("  - {} {}\n", op.entity_type, op.entity_name)
                    .red()
                    .to_string(),
                _ => format!("  ~ {} {}\n", op.entity_type, op.entity_name)
                    .yellow()
                    .to_string(),
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

// ── JSON formatting ──────────────────────────────────────────────────────────

pub(crate) fn format_json_list(entries: &[JournalEntry]) -> Result<String> {
    serde_json::to_string_pretty(entries).context("failed to serialize entries to JSON")
}

pub(crate) fn format_json_detail(entry: &JournalEntry) -> Result<String> {
    serde_json::to_string_pretty(entry).context("failed to serialize entry to JSON")
}

// ── Diff formatting helpers ──────────────────────────────────────────────────

fn format_annotation_colored(annotation: Option<&str>) -> String {
    match annotation {
        Some("failed") => format!("  [{}]", "failed".red()),
        Some("skipped") => format!("  [{}]", "skipped".yellow()),
        Some("applied") => format!("  [{}]", "applied".green()),
        Some(other) => format!("  [{}]", other),
        None => String::new(),
    }
}

fn format_scalar_field_diff(
    out: &mut String,
    fc: &SerializableFieldChange,
    annotation: Option<&str>,
) {
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

fn format_list_field_diff(
    out: &mut String,
    fc: &SerializableFieldChange,
    annotation: Option<&str>,
) {
    let empty = Vec::new();
    let current_items = fc
        .current
        .as_ref()
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    let desired_items = fc
        .desired
        .as_ref()
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
                let mut s = dest.clone();
                if let Some(serde_json::Value::String(gw)) = map.get("gateway") {
                    s.push_str(" via ");
                    s.push_str(gw);
                }
                let metric = map.get("metric").and_then(|m| m.as_u64()).unwrap_or(0);
                if metric != 0 {
                    s.push_str(&format!(" metric {}", metric));
                }
                s
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

// ── State conversion ─────────────────────────────────────────────────────────

pub(crate) fn serializable_state_to_flat_map(
    state: &SerializableState,
) -> IndexMap<String, serde_json::Value> {
    let mut map = IndexMap::new();
    map.insert(
        "type".to_string(),
        serde_json::Value::String(state.entity_type.clone()),
    );
    map.insert(
        "name".to_string(),
        serde_json::Value::String(state.selector_name.clone()),
    );
    if let Some(obj) = state.fields.as_object() {
        for (k, v) in obj {
            map.insert(k.clone(), v.clone());
        }
    }
    map
}
