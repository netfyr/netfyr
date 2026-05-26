//! Display helper functions for history output.
//!
//! Low-level helpers that build summary strings for entities, changes,
//! triggers, outcomes, and colorization.  These are used by the
//! higher-level formatting functions in `format.rs`.

use colored::Colorize;
use netfyr_journal::{
    ApplyOutcome, JournalEntry, SerializableDiffOp, SerializableState, Trigger,
};

// ── Entity display helpers ───────────────────────────────────────────────────

pub(crate) const SYSTEM_ENTITY_TYPES: &[&str] = &["dns", "hostname", "ntp"];

pub(crate) fn entity_display_name(op: &SerializableDiffOp) -> String {
    if SYSTEM_ENTITY_TYPES.contains(&op.entity_type.as_str()) {
        format!("sys:{}", op.entity_type)
    } else {
        op.entity_name.clone()
    }
}

pub(crate) fn state_entity_display_name(state: &SerializableState) -> String {
    if SYSTEM_ENTITY_TYPES.contains(&state.entity_type.as_str()) {
        format!("sys:{}", state.entity_type)
    } else {
        state.selector_name.clone()
    }
}

// ── String helpers ───────────────────────────────────────────────────────────

pub(crate) fn pad_or_truncate(s: &str, width: usize) -> String {
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

pub(crate) fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
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

pub(crate) fn json_display_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
        _ => serde_json::to_string(v).unwrap_or_else(|_| "?".to_string()),
    }
}

// ── List change formatting ───────────────────────────────────────────────────

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

    for addr in &added_sorted {
        parts.push(format!("+{}", addr));
    }
    for addr in &removed_sorted {
        parts.push(format!("-{}", addr));
    }
    parts
}

fn format_route_dest_via(r: &serde_json::Value) -> String {
    let dest = r
        .as_object()
        .and_then(|o| o.get("destination"))
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    match r
        .as_object()
        .and_then(|o| o.get("gateway"))
        .and_then(|v| v.as_str())
    {
        Some(gw) => format!("{} via {}", dest, gw),
        None => dest.to_string(),
    }
}

fn format_route_changes(
    added_routes: Vec<&serde_json::Value>,
    removed_routes: Vec<&serde_json::Value>,
) -> Vec<String> {
    let mut parts = Vec::new();
    let mut added_dflt: Vec<&serde_json::Value> = Vec::new();
    let mut added_nondflt: Vec<&serde_json::Value> = Vec::new();
    for r in &added_routes {
        let dest = r
            .as_object()
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
        let dest = r
            .as_object()
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
        let gw = r
            .as_object()
            .and_then(|o| o.get("gateway"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        parts.push(format!("+dflt via {}", gw));
    }
    for r in &removed_dflt {
        let gw = r
            .as_object()
            .and_then(|o| o.get("gateway"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        parts.push(format!("-dflt via {}", gw));
    }
    let n_add = added_nondflt.len();
    let n_rem = removed_nondflt.len();
    if n_add + n_rem >= 9 {
        if n_add > 0 {
            parts.push(format!("+{} routes", n_add));
        }
        if n_rem > 0 {
            parts.push(format!("-{} routes", n_rem));
        }
    } else {
        for r in &added_nondflt {
            parts.push(format!("+rt {}", format_route_dest_via(r)));
        }
        for r in &removed_nondflt {
            parts.push(format!("-rt {}", format_route_dest_via(r)));
        }
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

// ── Trigger display ──────────────────────────────────────────────────────────

pub(crate) fn trigger_display_name(trigger: &Trigger) -> &'static str {
    match trigger {
        Trigger::PolicyApply { .. } => "policy-apply",
        Trigger::DhcpEvent { .. } => "dhcp-lease",
        Trigger::ExternalChange { .. } => "external",
        Trigger::DaemonStartup => "daemon-startup",
        Trigger::Revert { .. } => "revert",
    }
}

pub(crate) fn trigger_detail_str(trigger: &Trigger) -> String {
    match trigger {
        Trigger::PolicyApply { source } => format!(" (source: {})", source),
        Trigger::DhcpEvent {
            policy_name,
            event_kind,
        } => {
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

// ── Outcome display ──────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) fn outcome_summary(outcome: &ApplyOutcome) -> String {
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

pub(crate) fn count_failed_fields(entry: &JournalEntry) -> u32 {
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

pub(crate) fn changes_column(entry: &JournalEntry) -> String {
    let changes = changes_summary(&entry.diff.operations);
    match &entry.outcome {
        ApplyOutcome::Applied { failed, .. } if *failed > 0 => {
            let n = count_failed_fields(entry);
            format!("FAIL({}) {}", n, changes)
        }
        _ => changes,
    }
}

pub(crate) fn outcome_detail(outcome: &ApplyOutcome) -> String {
    match outcome {
        ApplyOutcome::Applied {
            succeeded,
            failed,
            skipped,
        } => {
            format!(
                "applied ({} succeeded, {} failed, {} skipped)",
                succeeded, failed, skipped
            )
        }
        ApplyOutcome::Observed => "observed".to_string(),
    }
}

// ── Entities summary ─────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) fn entities_summary(ops: &[SerializableDiffOp]) -> String {
    entities_summary_with_state(ops, &[])
}

pub(crate) fn entities_summary_with_state(
    ops: &[SerializableDiffOp],
    state_entities: &[SerializableState],
) -> String {
    if ops.is_empty() {
        if state_entities.is_empty() {
            return "(none)".to_string();
        }
        let names: Vec<String> = state_entities
            .iter()
            .filter(|s| !SYSTEM_ENTITY_TYPES.contains(&s.entity_type.as_str()))
            .map(state_entity_display_name)
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
            (
                format!("{}{}", prefix, entity_display_name(op)),
                is_lifecycle,
            )
        })
        .collect();

    let count = items.len();

    if count <= 3 {
        return items
            .iter()
            .map(|(s, _)| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
    }

    if count <= 6 {
        // Prioritize lifecycle (add/remove) entities, show first 2
        let mut sorted = items.clone();
        sorted.sort_by_key(|k| std::cmp::Reverse(k.1));
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

pub(crate) fn entities_summary_fitted(
    ops: &[SerializableDiffOp],
    state_entities: &[SerializableState],
    max_width: usize,
) -> String {
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

// ── Field display priority ───────────────────────────────────────────────────

fn field_display_priority(name: &str) -> u8 {
    match name {
        "enabled" => 1,
        "carrier" => 2,
        "addresses" => 3,
        "routes" => 4,
        "nameservers" => 5,
        "search" | "search_domains" => 6,
        "mtu" => 7,
        _ => 8,
    }
}

// ── Changes summary ──────────────────────────────────────────────────────────

pub(crate) fn changes_summary(ops: &[SerializableDiffOp]) -> String {
    if ops.is_empty() {
        return "(none)".to_string();
    }

    let mut parts: Vec<(u8, String)> = Vec::new();

    for op in ops {
        match op.kind.as_str() {
            "add" => {
                parts.push((0, format!("+{}", entity_display_name(op))));
            }
            "remove" => {
                parts.push((0, format!("-{}", entity_display_name(op))));
            }
            _ => {
                for fc in &op.field_changes {
                    if fc.change_kind == "unchanged" {
                        continue;
                    }
                    let prio = field_display_priority(&fc.field_name);
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

                        if fc.field_name == "addresses" {
                            fn extract_addr(v: &serde_json::Value) -> Option<&str> {
                                v.as_str().or_else(|| v.get("address")?.as_str())
                            }
                            let current_addrs: Vec<&str> =
                                current_items.iter().filter_map(|v| extract_addr(v)).collect();
                            let desired_addrs: Vec<&str> =
                                desired_items.iter().filter_map(|v| extract_addr(v)).collect();
                            let a: Vec<&str> = desired_addrs
                                .iter()
                                .filter(|d| !current_addrs.contains(d))
                                .copied()
                                .collect();
                            let r: Vec<&str> = current_addrs
                                .iter()
                                .filter(|c| !desired_addrs.contains(c))
                                .copied()
                                .collect();
                            if !a.is_empty() || !r.is_empty() {
                                for s in format_address_changes(a, r) {
                                    parts.push((prio, s));
                                }
                            }
                            continue;
                        }

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
                            "routes" => {
                                for s in format_route_changes(added, removed) {
                                    parts.push((prio, s));
                                }
                            }
                            "nameservers" => {
                                let a: Vec<&str> =
                                    added.iter().filter_map(|v| v.as_str()).collect();
                                let r: Vec<&str> =
                                    removed.iter().filter_map(|v| v.as_str()).collect();
                                for s in format_dns_changes(a, r) {
                                    parts.push((prio, s));
                                }
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
                                    parts.push((prio, format!("+search: {}", des)));
                                } else if des.is_empty() {
                                    parts.push((prio, "-search".to_string()));
                                } else {
                                    parts.push((prio, format!("search {}→{}", cur, des)));
                                }
                            }
                            other => {
                                if !added.is_empty() {
                                    parts.push((prio, format!("+{} {}", added.len(), other)));
                                }
                                if !removed.is_empty() {
                                    parts.push((prio, format!("-{} {}", removed.len(), other)));
                                }
                            }
                        }
                    } else {
                        match fc.change_kind.as_str() {
                            "set" if fc.current.is_some() => {
                                let old = json_display_value(fc.current.as_ref().unwrap());
                                let new = json_display_value(
                                    fc.desired.as_ref().unwrap_or(&serde_json::Value::Null),
                                );
                                parts.push((prio, format!("{} {}→{}", fc.field_name, old, new)));
                            }
                            "set" => {
                                let val = json_display_value(
                                    fc.desired.as_ref().unwrap_or(&serde_json::Value::Null),
                                );
                                parts.push((prio, format!("+{}: {}", fc.field_name, val)));
                            }
                            "unset" => {
                                parts.push((prio, format!("-{}", fc.field_name)));
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

    parts.sort_by_key(|(p, _)| *p);
    parts
        .into_iter()
        .map(|(_, s)| s)
        .collect::<Vec<_>>()
        .join(", ")
}

// ── Colorize helpers ─────────────────────────────────────────────────────────

pub(crate) fn colorize_changes(plain: &str) -> String {
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

// ── Journal path ─────────────────────────────────────────────────────────────

pub(crate) fn journal_dir_path() -> String {
    std::env::var("NETFYR_JOURNAL_DIR")
        .unwrap_or_else(|_| "/var/lib/netfyr/journal".to_string())
}
