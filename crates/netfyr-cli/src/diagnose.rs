//! Implementation of the `netfyr diagnose` subcommand.
//!
//! Analyzes journal history and live system state to surface actionable
//! findings: configuration drift, carrier loss, DHCP lease failures,
//! recurring flaps, failed applies, and policy conflicts.
//!
//! Two runtime modes are supported, detected automatically:
//!
//! 1. **Daemon-free**: reads journal files directly, queries kernel via netlink.
//! 2. **Daemon**: retrieves history and state via Varlink.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::{Args, ValueEnum};
use colored::Colorize;
use serde::Serialize;

use netfyr_journal::{
    ApplyOutcome, Journal, JournalEntry, SequenceId, SerializableState, SerializableStateSet,
    Trigger,
};
use netfyr_state::SchemaRegistry;
use netfyr_varlink::{VarlinkClient, VarlinkError, VarlinkState};

use crate::daemon_socket_path;
use crate::history::{journal_dir_path, parse_since};

// ── Output format ─────────────────────────────────────────────────────────────

#[derive(Clone, ValueEnum)]
pub enum DiagnoseOutputFormat {
    Text,
    Json,
}

// ── CLI args ──────────────────────────────────────────────────────────────────

#[derive(Args)]
pub struct DiagnoseArgs {
    /// Filter by entity name (e.g. name=eth0)
    #[arg(long, short = 's', value_parser = parse_diagnose_selector)]
    pub selector: Vec<(String, String)>,

    /// How far back to scan journal entries (e.g. 1h, 30m, 7d or ISO 8601)
    #[arg(long, default_value = "1h")]
    pub since: String,

    /// Output format: text (default), json
    #[arg(long, short = 'o', default_value = "text")]
    pub output: DiagnoseOutputFormat,
}

// ── Severity ──────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Healthy,
    Info,
    Warning,
    Critical,
}

// ── PatternKind ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternKind {
    ConfigurationDrift,
    CarrierLoss,
    DhcpLeaseLost,
    RecurringFlaps,
    FailedApply,
    PolicyConflict,
    NoRecentActivity,
}

// ── Finding ───────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize)]
pub struct Finding {
    #[serde(rename = "entity")]
    pub entity_name: String,
    pub entity_type: String,
    pub severity: Severity,
    pub pattern: PatternKind,
    pub summary: String,
    pub details: Vec<String>,
    pub suggested_actions: Vec<String>,
    pub related_entries: Vec<SequenceId>,
}

// ── Internal data structures ──────────────────────────────────────────────────

struct CollectedData {
    entries: Vec<JournalEntry>,
    current_state: Vec<SerializableState>,
    managed_entities: Vec<(String, String)>,
    last_applied_entry: Option<JournalEntry>,
}

struct HealthyInfo {
    last_apply_seq: Option<SequenceId>,
    last_apply_ago: Option<String>,
    carrier_up: Option<bool>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run_diagnose(args: DiagnoseArgs) -> Result<ExitCode> {
    let socket_path = daemon_socket_path();
    match VarlinkClient::connect(&socket_path).await {
        Ok(mut client) => {
            return run_diagnose_daemon(&mut client, &args).await;
        }
        Err(VarlinkError::ConnectionFailed(_)) => {}
        Err(e) => {
            return Err(
                anyhow::Error::from(e).context("unexpected error connecting to daemon socket"),
            );
        }
    }
    run_diagnose_local(&args).await
}

// ── Daemon mode ───────────────────────────────────────────────────────────────

async fn run_diagnose_daemon(client: &mut VarlinkClient, args: &DiagnoseArgs) -> Result<ExitCode> {
    let selector_name = args.selector.first().map(|(_, v)| v.clone());
    let raw_entries = client
        .get_history(Some(10_000), Some(args.since.clone()), None, selector_name)
        .await
        .context("failed to get history from daemon")?;

    let all_entries: Vec<JournalEntry> = raw_entries
        .into_iter()
        .map(|v| serde_json::from_value(v).context("failed to deserialize journal entry"))
        .collect::<Result<Vec<_>>>()?;

    let last_applied = all_entries
        .iter()
        .filter(|e| matches!(&e.outcome, ApplyOutcome::Applied { .. }))
        .max_by_key(|e| e.seq)
        .cloned();

    let varlink_states = client
        .query(None)
        .await
        .context("failed to query current state from daemon")?;
    let current_state = varlink_states_to_serializable(varlink_states);

    let managed_entities = discover_managed_entities(&all_entries, last_applied.as_ref());

    run_analysis(
        CollectedData { entries: all_entries, current_state, managed_entities, last_applied_entry: last_applied },
        args,
    )
}

// ── Local mode ────────────────────────────────────────────────────────────────

async fn run_diagnose_local(args: &DiagnoseArgs) -> Result<ExitCode> {
    let journal_dir = journal_dir_path();
    let dir = Path::new(&journal_dir);

    if !dir.exists() {
        return run_analysis(
            CollectedData {
                entries: vec![],
                current_state: vec![],
                managed_entities: vec![],
                last_applied_entry: None,
            },
            args,
        );
    }

    let journal = Journal::open(dir)
        .with_context(|| format!("failed to open journal at {}", journal_dir))?;

    let (entries, last_applied) = collect_entries_local(&journal, &args.since)?;

    let registry = crate::apply::create_backend_registry();
    let state_set = match registry.query_all().await {
        Ok(ss) => ss,
        Err(e) => {
            tracing::warn!(%e, "failed to query current system state");
            netfyr_state::StateSet::new()
        }
    };
    let current_state = SerializableStateSet::from(&state_set).entities;

    let managed_entities = discover_managed_entities(&entries, last_applied.as_ref());

    run_analysis(
        CollectedData { entries, current_state, managed_entities, last_applied_entry: last_applied },
        args,
    )
}

// ── Data collection helpers ───────────────────────────────────────────────────

fn collect_entries_local(
    journal: &Journal,
    since_str: &str,
) -> Result<(Vec<JournalEntry>, Option<JournalEntry>)> {
    let raw = journal
        .read_recent(10_000)
        .context("failed to read journal entries")?;

    let last_applied = raw
        .iter()
        .filter(|e| matches!(&e.outcome, ApplyOutcome::Applied { .. }))
        .max_by_key(|e| e.seq)
        .cloned();

    let since = parse_since(since_str)?;
    let entries: Vec<JournalEntry> = raw.into_iter().filter(|e| e.timestamp >= since).collect();

    Ok((entries, last_applied))
}

fn discover_managed_entities(
    entries: &[JournalEntry],
    last_applied: Option<&JournalEntry>,
) -> Vec<(String, String)> {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut result: Vec<(String, String)> = Vec::new();

    // Primary: entities from the last applied entry
    if let Some(entry) = last_applied {
        for s in &entry.state_after.entities {
            let key = (s.entity_type.clone(), s.selector_name.clone());
            if seen.insert(key.clone()) {
                result.push(key);
            }
        }
    }

    // Augment: entities mentioned anywhere in entries
    for entry in entries {
        for s in &entry.state_after.entities {
            let key = (s.entity_type.clone(), s.selector_name.clone());
            if seen.insert(key.clone()) {
                result.push(key);
            }
        }
        for op in &entry.diff.operations {
            let key = (op.entity_type.clone(), op.entity_name.clone());
            if seen.insert(key.clone()) {
                result.push(key);
            }
        }
    }

    result
}

fn filter_entities_by_selector(
    entities: &[(String, String)],
    selectors: &[(String, String)],
) -> Vec<(String, String)> {
    if selectors.is_empty() {
        return entities.to_vec();
    }
    let names: HashSet<&str> = selectors
        .iter()
        .filter(|(k, _)| k == "name")
        .map(|(_, v)| v.as_str())
        .collect();
    entities
        .iter()
        .filter(|(_, name)| names.contains(name.as_str()))
        .cloned()
        .collect()
}

fn get_entity_state<'a>(
    current_state: &'a [SerializableState],
    entity_type: &str,
    entity_name: &str,
) -> Option<&'a SerializableState> {
    current_state
        .iter()
        .find(|s| s.entity_type == entity_type && s.selector_name == entity_name)
}

fn get_last_applied_state<'a>(
    last_applied: Option<&'a JournalEntry>,
    entity_type: &str,
    entity_name: &str,
) -> Option<&'a SerializableState> {
    last_applied?.state_after.entities.iter().find(|s| {
        s.entity_type == entity_type && s.selector_name == entity_name
    })
}

fn varlink_states_to_serializable(states: Vec<VarlinkState>) -> Vec<SerializableState> {
    states
        .into_iter()
        .map(|vs| SerializableState {
            entity_type: vs.entity_type,
            selector_name: vs.selector.name.unwrap_or_default(),
            fields: serde_json::Value::Object(vs.fields),
        })
        .collect()
}

// ── Core analysis pipeline ────────────────────────────────────────────────────

fn run_analysis(data: CollectedData, args: &DiagnoseArgs) -> Result<ExitCode> {
    let CollectedData { entries, current_state, managed_entities, last_applied_entry } = data;

    if entries.is_empty() && managed_entities.is_empty() {
        println!("No journal entries found. Run `netfyr apply` first.");
        return Ok(ExitCode::SUCCESS);
    }

    let entities_to_analyze = filter_entities_by_selector(&managed_entities, &args.selector);

    if entities_to_analyze.is_empty() {
        if args.selector.is_empty() {
            println!("No managed entities found.");
        } else {
            println!("No managed entities found matching selector.");
        }
        return Ok(ExitCode::SUCCESS);
    }

    let mut entity_findings: Vec<(String, String, Vec<Finding>)> = Vec::new();
    let mut healthy_entities: Vec<(String, String, HealthyInfo)> = Vec::new();

    for (entity_type, entity_name) in &entities_to_analyze {
        let findings = run_all_detectors(
            &entries,
            &current_state,
            entity_type,
            entity_name,
            last_applied_entry.as_ref(),
        );

        if findings.is_empty() {
            let et = entity_type.as_str();
            let en = entity_name.as_str();
            let last_apply = entries
                .iter()
                .filter(|e| matches!(&e.outcome, ApplyOutcome::Applied { .. }))
                .filter(|e| {
                    e.state_after
                        .entities
                        .iter()
                        .any(|s| s.entity_type == et && s.selector_name == en)
                })
                .max_by_key(|e| e.seq);

            let carrier_up =
                get_entity_state(&current_state, entity_type, entity_name)
                    .and_then(|s| s.fields.get("carrier"))
                    .and_then(|v| v.as_bool());

            healthy_entities.push((
                entity_type.clone(),
                entity_name.clone(),
                HealthyInfo {
                    last_apply_seq: last_apply.map(|e| e.seq),
                    last_apply_ago: last_apply.map(|e| relative_time(e.timestamp)),
                    carrier_up,
                },
            ));
        } else {
            entity_findings.push((entity_type.clone(), entity_name.clone(), findings));
        }
    }

    // Sort entity_findings: most severe entity first
    entity_findings.sort_by(|a, b| {
        let worst_a = a.2.iter().map(|f| &f.severity).max().cloned().unwrap_or(Severity::Healthy);
        let worst_b = b.2.iter().map(|f| &f.severity).max().cloned().unwrap_or(Severity::Healthy);
        worst_b.cmp(&worst_a).then(a.1.cmp(&b.1))
    });

    // Collect all findings for exit code
    let all_findings: Vec<&Finding> = entity_findings.iter().flat_map(|(_, _, fs)| fs).collect();

    match args.output {
        DiagnoseOutputFormat::Text => {
            let text = format_text(&entity_findings, &healthy_entities);
            print!("{}", text);
        }
        DiagnoseOutputFormat::Json => {
            let json = format_json(&entity_findings, &healthy_entities)?;
            println!("{}", json);
        }
    }

    Ok(determine_exit_code(&all_findings))
}

fn run_all_detectors(
    entries: &[JournalEntry],
    current_state: &[SerializableState],
    entity_type: &str,
    entity_name: &str,
    last_applied: Option<&JournalEntry>,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    findings.extend(detect_configuration_drift(
        entries, current_state, entity_type, entity_name, last_applied,
    ));
    findings.extend(detect_carrier_loss(
        entries, current_state, entity_type, entity_name, last_applied,
    ));
    findings.extend(detect_dhcp_lease_lost(
        entries, current_state, entity_type, entity_name, last_applied,
    ));
    findings.extend(detect_recurring_flaps(
        entries, current_state, entity_type, entity_name, last_applied,
    ));
    findings.extend(detect_failed_apply(
        entries, current_state, entity_type, entity_name, last_applied,
    ));
    findings.extend(detect_policy_conflict(
        entries, current_state, entity_type, entity_name, last_applied,
    ));
    findings.extend(detect_no_recent_activity(
        entries, current_state, entity_type, entity_name, last_applied,
    ));
    findings
}

// ── Detectors ─────────────────────────────────────────────────────────────────

fn detect_configuration_drift(
    entries: &[JournalEntry],
    current_state: &[SerializableState],
    entity_type: &str,
    entity_name: &str,
    last_applied: Option<&JournalEntry>,
) -> Vec<Finding> {
    let desired = match get_last_applied_state(last_applied, entity_type, entity_name) {
        Some(s) => s,
        None => return vec![],
    };
    let current = match get_entity_state(current_state, entity_type, entity_name) {
        Some(s) => s,
        None => return vec![],
    };

    let schema = SchemaRegistry::default();

    let desired_fields = match desired.fields.as_object() {
        Some(o) => o,
        None => return vec![],
    };
    let current_fields = match current.fields.as_object() {
        Some(o) => o,
        None => return vec![],
    };

    let mut details: Vec<String> = Vec::new();

    for (field, desired_val) in desired_fields {
        // Skip read-only fields
        if let Some(info) = schema.field_info(entity_type, field) {
            if !info.writable {
                continue;
            }
        }

        match current_fields.get(field.as_str()) {
            None => {
                details.push(format!(
                    "Policy wants {}={}, system has no value",
                    field, desired_val
                ));
            }
            Some(cv) if cv != desired_val => {
                details.push(format!(
                    "Policy wants {}={}, system has {}={}",
                    field, desired_val, field, cv
                ));
            }
            _ => {}
        }
    }

    if details.is_empty() {
        return vec![];
    }

    let summary = details[0].clone();

    // Find the most recent ExternalChange affecting this entity
    let recent_change = entries
        .iter()
        .filter(|e| {
            if let Trigger::ExternalChange { changed_entities } = &e.trigger {
                changed_entities.iter().any(|n| n == entity_name)
            } else {
                false
            }
        })
        .max_by_key(|e| e.seq);

    if let Some(change) = recent_change {
        details.push(format!(
            "Changed externally {} (seq {})",
            relative_time(change.timestamp),
            change.seq
        ));
    }

    let last_seq = last_applied.map(|e| e.seq).unwrap_or(0);
    let mut related = vec![last_seq];
    if let Some(c) = recent_change {
        related.push(c.seq);
    }
    related.sort_unstable();
    related.dedup();

    vec![Finding {
        entity_name: entity_name.to_string(),
        entity_type: entity_type.to_string(),
        severity: Severity::Warning,
        pattern: PatternKind::ConfigurationDrift,
        summary,
        details,
        suggested_actions: vec![
            "Run `netfyr apply` to re-converge".to_string(),
            format!(
                "Or `netfyr revert {}` to restore last applied state",
                last_seq
            ),
        ],
        related_entries: related,
    }]
}

fn detect_carrier_loss(
    entries: &[JournalEntry],
    current_state: &[SerializableState],
    entity_type: &str,
    entity_name: &str,
    _last_applied: Option<&JournalEntry>,
) -> Vec<Finding> {
    // Only report if current state has carrier=false
    let carrier_false = get_entity_state(current_state, entity_type, entity_name)
        .and_then(|s| s.fields.get("carrier"))
        .and_then(|v| v.as_bool())
        .map(|b| !b)
        .unwrap_or(false);

    if !carrier_false {
        return vec![];
    }

    // Collect entries that record this entity's carrier state, sorted by seq
    let mut carrier_history: Vec<(SequenceId, DateTime<Utc>, bool)> = entries
        .iter()
        .filter_map(|e| {
            let state = e
                .state_after
                .entities
                .iter()
                .find(|s| s.entity_type == entity_type && s.selector_name == entity_name)?;
            let carrier = state.fields.get("carrier")?.as_bool()?;
            Some((e.seq, e.timestamp, carrier))
        })
        .collect();
    carrier_history.sort_by_key(|(seq, _, _)| *seq);

    // Find the most recent entry where carrier was recorded as false
    if let Some((seq, ts, false)) = carrier_history.last() {
        let ago = relative_time(*ts);
        return vec![Finding {
            entity_name: entity_name.to_string(),
            entity_type: entity_type.to_string(),
            severity: Severity::Critical,
            pattern: PatternKind::CarrierLoss,
            summary: format!("Interface {} has no carrier", entity_name),
            details: vec![
                format!("Carrier lost {} (seq {})", ago, seq),
                "Interface has no active link".to_string(),
            ],
            suggested_actions: vec![
                "Check physical cable or switch port".to_string(),
                format!("Verify link with `ip link show {}`", entity_name),
            ],
            related_entries: vec![*seq],
        }];
    }

    // Carrier is down but no entry in window records it
    vec![Finding {
        entity_name: entity_name.to_string(),
        entity_type: entity_type.to_string(),
        severity: Severity::Critical,
        pattern: PatternKind::CarrierLoss,
        summary: format!("Interface {} has no carrier", entity_name),
        details: vec![
            "No carrier detected; loss predates the current scan window".to_string(),
            "Use --since with a longer duration to see when carrier was lost".to_string(),
        ],
        suggested_actions: vec![
            "Check physical cable or switch port".to_string(),
            format!("Verify link with `ip link show {}`", entity_name),
        ],
        related_entries: vec![],
    }]
}

fn detect_dhcp_lease_lost(
    entries: &[JournalEntry],
    _current_state: &[SerializableState],
    entity_type: &str,
    entity_name: &str,
    _last_applied: Option<&JournalEntry>,
) -> Vec<Finding> {
    // Filter DhcpEvent entries associated with this entity via state_after
    let mut dhcp_entries: Vec<&JournalEntry> = entries
        .iter()
        .filter(|e| matches!(&e.trigger, Trigger::DhcpEvent { .. }))
        .filter(|e| {
            e.state_after
                .entities
                .iter()
                .any(|s| s.entity_type == entity_type && s.selector_name == entity_name)
        })
        .collect();
    dhcp_entries.sort_by_key(|e| e.seq);

    // Track the most recent event per policy (true = acquired, false = expired)
    let mut policy_states: HashMap<String, (&JournalEntry, bool)> = HashMap::new();
    for entry in &dhcp_entries {
        if let Trigger::DhcpEvent { policy_name, event_kind } = &entry.trigger {
            // Known event_kind values: "lease_acquired", "lease_renewed",
            // "lease_expired", "lease_lost"
            let acquired = event_kind.contains("acquire") || event_kind.contains("renew");
            policy_states.insert(policy_name.clone(), (entry, acquired));
        }
    }

    policy_states
        .iter()
        .filter(|(_, (_, acquired))| !acquired)
        .map(|(policy_name, (entry, _))| {
            let ago = relative_time(entry.timestamp);
            Finding {
                entity_name: entity_name.to_string(),
                entity_type: entity_type.to_string(),
                severity: Severity::Critical,
                pattern: PatternKind::DhcpLeaseLost,
                summary: format!(
                    "DHCP lease expired {} ago (policy: {})",
                    ago, policy_name
                ),
                details: vec![
                    format!("Lease expired {} (seq {})", ago, entry.seq),
                    format!("Policy: {}", policy_name),
                    "Interface may have no addresses".to_string(),
                ],
                suggested_actions: vec![
                    "Check DHCP server reachability".to_string(),
                    format!(
                        "Verify link connectivity with `ip link show {}`",
                        entity_name
                    ),
                ],
                related_entries: vec![entry.seq],
            }
        })
        .collect()
}

fn detect_recurring_flaps(
    entries: &[JournalEntry],
    _current_state: &[SerializableState],
    entity_type: &str,
    entity_name: &str,
    _last_applied: Option<&JournalEntry>,
) -> Vec<Finding> {
    // Collect ExternalChange entries affecting this entity
    let mut ext_entries: Vec<&JournalEntry> = entries
        .iter()
        .filter(|e| {
            if let Trigger::ExternalChange { changed_entities } = &e.trigger {
                changed_entities.iter().any(|n| n == entity_name)
            } else {
                false
            }
        })
        .collect();
    ext_entries.sort_by_key(|e| e.seq);

    if ext_entries.len() <= 3 {
        return vec![];
    }

    // Extract state snapshots for this entity from each external change entry
    let states: Vec<&serde_json::Value> = ext_entries
        .iter()
        .filter_map(|e| {
            e.state_after
                .entities
                .iter()
                .find(|s| s.entity_type == entity_type && s.selector_name == entity_name)
                .map(|s| &s.fields)
        })
        .collect();

    // Collect all field names across all snapshots
    let mut all_fields: HashSet<String> = HashSet::new();
    for fields in &states {
        if let Some(obj) = fields.as_object() {
            for k in obj.keys() {
                all_fields.insert(k.clone());
            }
        }
    }

    // Check each field for oscillation (value revisits a previously seen value)
    let mut oscillating_fields: Vec<String> = Vec::new();
    for field in &all_fields {
        let values: Vec<Option<&serde_json::Value>> =
            states.iter().map(|s| s.get(field.as_str())).collect();

        let mut seen: Vec<&serde_json::Value> = Vec::new();
        let mut oscillation_count = 0usize;

        for v in values.iter().flatten() {
            if seen.contains(v) {
                oscillation_count += 1;
            } else {
                seen.push(v);
            }
        }

        if oscillation_count >= 1 {
            oscillating_fields.push(field.clone());
        }
    }

    if oscillating_fields.is_empty() {
        return vec![];
    }

    oscillating_fields.sort();
    let first_ts = ext_entries.first().map(|e| e.timestamp).unwrap_or_else(Utc::now);
    let last_ts = ext_entries.last().map(|e| e.timestamp).unwrap_or_else(Utc::now);
    let first_seq = ext_entries.first().map(|e| e.seq).unwrap_or(0);
    let last_seq = ext_entries.last().map(|e| e.seq).unwrap_or(0);
    let count = ext_entries.len();
    let related: Vec<SequenceId> = ext_entries.iter().map(|e| e.seq).collect();

    vec![Finding {
        entity_name: entity_name.to_string(),
        entity_type: entity_type.to_string(),
        severity: Severity::Warning,
        pattern: PatternKind::RecurringFlaps,
        summary: format!(
            "{} changes detected, oscillation on: {}",
            count,
            oscillating_fields.join(", ")
        ),
        details: vec![
            format!(
                "{} external changes (seq {} to {})",
                count, first_seq, last_seq
            ),
            format!("Oscillating fields: {}", oscillating_fields.join(", ")),
            format!(
                "Time range: {} to {}",
                relative_time(first_ts),
                relative_time(last_ts)
            ),
        ],
        suggested_actions: vec![
            "Investigate physical link stability".to_string(),
            "Check upstream switch or conflicting automation".to_string(),
        ],
        related_entries: related,
    }]
}

fn detect_failed_apply(
    entries: &[JournalEntry],
    _current_state: &[SerializableState],
    entity_type: &str,
    entity_name: &str,
    _last_applied: Option<&JournalEntry>,
) -> Vec<Finding> {
    entries
        .iter()
        .filter(|e| {
            matches!(
                &e.trigger,
                Trigger::PolicyApply { .. } | Trigger::DaemonStartup | Trigger::Revert { .. }
            )
        })
        .filter(|e| {
            if let ApplyOutcome::Applied { failed, .. } = &e.outcome {
                *failed > 0
            } else {
                false
            }
        })
        .filter(|e| {
            // Associate with entity via diff operations
            e.diff.operations.iter().any(|op| {
                op.entity_type == entity_type && op.entity_name == entity_name
            })
        })
        .map(|entry| {
            let (succeeded, failed, skipped) = match &entry.outcome {
                ApplyOutcome::Applied { succeeded, failed, skipped } => {
                    (*succeeded, *failed, *skipped)
                }
                _ => (0, 0, 0),
            };
            Finding {
                entity_name: entity_name.to_string(),
                entity_type: entity_type.to_string(),
                severity: Severity::Warning,
                pattern: PatternKind::FailedApply,
                summary: format!("Apply had {} failure(s) (seq {})", failed, entry.seq),
                details: vec![format!(
                    "Seq {}: {} succeeded, {} failed, {} skipped",
                    entry.seq, succeeded, failed, skipped
                )],
                suggested_actions: vec![format!(
                    "Run `netfyr history --show {}` to inspect failure details",
                    entry.seq
                )],
                related_entries: vec![entry.seq],
            }
        })
        .collect()
}

fn detect_policy_conflict(
    entries: &[JournalEntry],
    _current_state: &[SerializableState],
    entity_type: &str,
    entity_name: &str,
    _last_applied: Option<&JournalEntry>,
) -> Vec<Finding> {
    // Collect applied entries that include this entity in their diff
    let mut applied: Vec<&JournalEntry> = entries
        .iter()
        .filter(|e| matches!(&e.outcome, ApplyOutcome::Applied { .. }))
        .filter(|e| {
            e.diff
                .operations
                .iter()
                .any(|op| op.entity_type == entity_type && op.entity_name == entity_name)
        })
        .collect();
    applied.sort_by_key(|e| e.seq);

    if applied.len() < 3 {
        return vec![];
    }

    // Build value sequences per field from state_after snapshots
    let mut field_sequences: HashMap<String, Vec<Option<serde_json::Value>>> = HashMap::new();

    for entry in &applied {
        let fields = entry
            .state_after
            .entities
            .iter()
            .find(|s| s.entity_type == entity_type && s.selector_name == entity_name)
            .map(|s| &s.fields);

        let field_names: HashSet<String> = fields
            .and_then(|f| f.as_object())
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();

        for field in &field_names {
            let val = fields.and_then(|f| f.get(field.as_str())).cloned();
            field_sequences.entry(field.clone()).or_default().push(val);
        }
    }

    // Detect fields where the value oscillates 2+ times (heuristic for conflict)
    let mut conflicting_fields: Vec<String> = Vec::new();
    for (field, values) in &field_sequences {
        if values.len() < 3 {
            continue;
        }
        let mut seen: Vec<&Option<serde_json::Value>> = Vec::new();
        let mut oscillations = 0usize;
        for v in values {
            if seen.contains(&v) {
                oscillations += 1;
            } else {
                seen.push(v);
            }
        }
        if oscillations >= 2 {
            conflicting_fields.push(field.clone());
        }
    }

    if conflicting_fields.is_empty() {
        return vec![];
    }

    conflicting_fields.sort();
    let related: Vec<SequenceId> = applied.iter().map(|e| e.seq).collect();

    vec![Finding {
        entity_name: entity_name.to_string(),
        entity_type: entity_type.to_string(),
        severity: Severity::Warning,
        pattern: PatternKind::PolicyConflict,
        summary: format!(
            "May have conflicting policies for fields: {}",
            conflicting_fields.join(", ")
        ),
        details: vec![
            format!(
                "Field(s) {} oscillated across {} applies (heuristic detection)",
                conflicting_fields.join(", "),
                applied.len()
            ),
            "This may indicate overlapping policies setting the same field to different values"
                .to_string(),
        ],
        suggested_actions: vec![
            "Review overlapping policies and adjust priorities".to_string(),
            "Run `netfyr history` to see the apply sequence".to_string(),
        ],
        related_entries: related,
    }]
}

fn detect_no_recent_activity(
    entries: &[JournalEntry],
    _current_state: &[SerializableState],
    entity_type: &str,
    entity_name: &str,
    _last_applied: Option<&JournalEntry>,
) -> Vec<Finding> {
    // Check if entity appears anywhere in the windowed entries
    let in_window = entries.iter().any(|e| {
        e.state_after
            .entities
            .iter()
            .any(|s| s.entity_type == entity_type && s.selector_name == entity_name)
            || e.diff
                .operations
                .iter()
                .any(|op| op.entity_type == entity_type && op.entity_name == entity_name)
    });

    if in_window {
        return vec![];
    }

    vec![Finding {
        entity_name: entity_name.to_string(),
        entity_type: entity_type.to_string(),
        severity: Severity::Info,
        pattern: PatternKind::NoRecentActivity,
        summary: "No journal entries within the current time window".to_string(),
        details: vec![
            "This entity is managed but has no activity in the scan window".to_string(),
            "Use --since with a longer duration to see historical activity".to_string(),
        ],
        suggested_actions: vec![],
        related_entries: vec![],
    }]
}

// ── Formatting ────────────────────────────────────────────────────────────────

fn format_text(
    entity_findings: &[(String, String, Vec<Finding>)],
    healthy_entities: &[(String, String, HealthyInfo)],
) -> String {
    let mut out = String::new();

    for (_, entity_name, findings) in entity_findings {
        // Determine worst finding for the header
        let worst = findings.iter().max_by_key(|f| &f.severity).unwrap();
        let pattern_label = pattern_to_str(&worst.pattern).replace('_', " ");
        let severity_label = severity_to_str(&worst.severity);
        let header = format!("{}: {} ({})", entity_name, pattern_label, severity_label);

        let colored_header = match worst.severity {
            Severity::Critical => header.red().bold().to_string(),
            Severity::Warning => header.yellow().bold().to_string(),
            Severity::Info => header.bold().to_string(),
            Severity::Healthy => header.green().bold().to_string(),
        };
        out.push_str(&colored_header);
        out.push('\n');

        for finding in findings {
            for detail in &finding.details {
                out.push_str(&format!("  {}\n", detail));
            }
            for action in &finding.suggested_actions {
                out.push_str(&format!("  {}\n", format!("→ {}", action).cyan()));
            }
        }
        out.push('\n');
    }

    for (_, entity_name, info) in healthy_entities {
        let header = format!("{}: healthy", entity_name).green().bold().to_string();
        out.push_str(&header);
        out.push('\n');

        let mut parts: Vec<String> = Vec::new();
        if let (Some(seq), Some(ago)) = (&info.last_apply_seq, &info.last_apply_ago) {
            parts.push(format!("Last apply: {} (seq {})", ago, seq));
        }
        parts.push("no drift".to_string());
        if let Some(carrier) = info.carrier_up {
            parts.push(if carrier { "carrier up".to_string() } else { "carrier down".to_string() });
        }
        out.push_str(&format!("  {}\n", parts.join(", ")));
        out.push('\n');
    }

    out
}

fn format_json(
    entity_findings: &[(String, String, Vec<Finding>)],
    healthy_entities: &[(String, String, HealthyInfo)],
) -> Result<String> {
    let mut items: Vec<serde_json::Value> = Vec::new();

    for (_, _, findings) in entity_findings {
        for finding in findings {
            items.push(serde_json::to_value(finding)?);
        }
    }

    for (entity_type, entity_name, info) in healthy_entities {
        let mut obj = serde_json::json!({
            "entity": entity_name,
            "entity_type": entity_type,
            "severity": "healthy",
            "pattern": "healthy",
            "summary": "healthy",
            "details": [],
            "suggested_actions": [],
            "related_entries": []
        });
        if let (Some(seq), Some(ago)) = (&info.last_apply_seq, &info.last_apply_ago) {
            obj["summary"] =
                serde_json::Value::String(format!("Last apply: {} (seq {}), no drift", ago, seq));
            if let Some(carrier) = info.carrier_up {
                obj["details"] = serde_json::json!([format!(
                    "carrier {}",
                    if carrier { "up" } else { "down" }
                )]);
            }
        }
        items.push(obj);
    }

    Ok(serde_json::to_string_pretty(&items)?)
}

fn determine_exit_code(findings: &[&Finding]) -> ExitCode {
    if findings.iter().any(|f| f.severity == Severity::Critical) {
        return ExitCode::from(2u8);
    }
    if findings.iter().any(|f| f.severity == Severity::Warning) {
        return ExitCode::from(1u8);
    }
    ExitCode::SUCCESS
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn severity_to_str(s: &Severity) -> &'static str {
    match s {
        Severity::Critical => "critical",
        Severity::Warning => "warning",
        Severity::Info => "info",
        Severity::Healthy => "healthy",
    }
}

fn pattern_to_str(p: &PatternKind) -> &'static str {
    match p {
        PatternKind::ConfigurationDrift => "configuration_drift",
        PatternKind::CarrierLoss => "carrier_loss",
        PatternKind::DhcpLeaseLost => "dhcp_lease_lost",
        PatternKind::RecurringFlaps => "recurring_flaps",
        PatternKind::FailedApply => "failed_apply",
        PatternKind::PolicyConflict => "policy_conflict",
        PatternKind::NoRecentActivity => "no_recent_activity",
    }
}

fn relative_time(ts: DateTime<Utc>) -> String {
    let now = Utc::now();
    let diff = now.signed_duration_since(ts);
    let secs = diff.num_seconds().max(0);

    if secs < 60 {
        return format!("{} sec ago", secs);
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{} min ago", mins);
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{}h ago", hours);
    }
    let days = hours / 24;
    format!("{}d ago", days)
}

fn parse_diagnose_selector(s: &str) -> Result<(String, String), String> {
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
            "invalid selector key {:?}; diagnose only supports 'name' (e.g. name=eth0)",
            key
        ));
    }

    Ok((key.to_string(), value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use netfyr_journal::{SerializableDiff, SerializableDiffOp};

    // ── Test helpers ──────────────────────────────────────────────────────────────

    fn empty_diff() -> SerializableDiff {
        SerializableDiff { operations: vec![] }
    }

    fn make_ser_state(
        entity_type: &str,
        name: &str,
        fields: serde_json::Value,
    ) -> SerializableState {
        SerializableState {
            entity_type: entity_type.to_string(),
            selector_name: name.to_string(),
            fields,
        }
    }

    /// Build a PolicyApply JournalEntry with the given entity in state_after.
    fn make_applied_entry(
        seq: u64,
        entity_type: &str,
        entity_name: &str,
        fields: serde_json::Value,
        ago_secs: i64,
    ) -> JournalEntry {
        let ts = Utc::now() - Duration::seconds(ago_secs);
        JournalEntry {
            seq,
            timestamp: ts,
            trigger: Trigger::PolicyApply { source: "policy.yaml".to_string() },
            active_policies: vec![],
            diff: SerializableDiff {
                operations: vec![SerializableDiffOp {
                    kind: "modify".to_string(),
                    entity_type: entity_type.to_string(),
                    entity_name: entity_name.to_string(),
                    field_changes: vec![],
                }],
            },
            state_after: SerializableStateSet {
                entities: vec![make_ser_state(entity_type, entity_name, fields)],
            },
            outcome: ApplyOutcome::Applied { succeeded: 1, failed: 0, skipped: 0 },
        }
    }

    /// Build an ExternalChange JournalEntry.
    fn make_external_change_entry(
        seq: u64,
        entity_type: &str,
        entity_name: &str,
        fields: serde_json::Value,
        ago_secs: i64,
    ) -> JournalEntry {
        let ts = Utc::now() - Duration::seconds(ago_secs);
        JournalEntry {
            seq,
            timestamp: ts,
            trigger: Trigger::ExternalChange {
                changed_entities: vec![entity_name.to_string()],
            },
            active_policies: vec![],
            diff: empty_diff(),
            state_after: SerializableStateSet {
                entities: vec![make_ser_state(entity_type, entity_name, fields)],
            },
            outcome: ApplyOutcome::Observed,
        }
    }

    /// Build a DhcpEvent JournalEntry.
    fn make_dhcp_event_entry(
        seq: u64,
        entity_type: &str,
        entity_name: &str,
        policy_name: &str,
        event_kind: &str,
        ago_secs: i64,
    ) -> JournalEntry {
        let ts = Utc::now() - Duration::seconds(ago_secs);
        JournalEntry {
            seq,
            timestamp: ts,
            trigger: Trigger::DhcpEvent {
                policy_name: policy_name.to_string(),
                event_kind: event_kind.to_string(),
            },
            active_policies: vec![],
            diff: empty_diff(),
            state_after: SerializableStateSet {
                entities: vec![make_ser_state(entity_type, entity_name, serde_json::json!({}))],
            },
            outcome: ApplyOutcome::Observed,
        }
    }

    /// Build a PolicyApply entry that records failures.
    fn make_failed_apply_entry(
        seq: u64,
        entity_type: &str,
        entity_name: &str,
        failed: u32,
        ago_secs: i64,
    ) -> JournalEntry {
        let ts = Utc::now() - Duration::seconds(ago_secs);
        JournalEntry {
            seq,
            timestamp: ts,
            trigger: Trigger::PolicyApply { source: "policy.yaml".to_string() },
            active_policies: vec![],
            diff: SerializableDiff {
                operations: vec![SerializableDiffOp {
                    kind: "modify".to_string(),
                    entity_type: entity_type.to_string(),
                    entity_name: entity_name.to_string(),
                    field_changes: vec![],
                }],
            },
            state_after: SerializableStateSet { entities: vec![] },
            outcome: ApplyOutcome::Applied { succeeded: 1, failed, skipped: 0 },
        }
    }

    fn make_finding(severity: Severity) -> Finding {
        Finding {
            entity_name: "eth0".to_string(),
            entity_type: "ethernet".to_string(),
            severity,
            pattern: PatternKind::ConfigurationDrift,
            summary: "test".to_string(),
            details: vec![],
            suggested_actions: vec![],
            related_entries: vec![],
        }
    }

    fn text_args() -> DiagnoseArgs {
        DiagnoseArgs {
            selector: vec![],
            since: "1h".to_string(),
            output: DiagnoseOutputFormat::Text,
        }
    }

    fn json_args() -> DiagnoseArgs {
        DiagnoseArgs {
            selector: vec![],
            since: "1h".to_string(),
            output: DiagnoseOutputFormat::Json,
        }
    }

    // ── detect_configuration_drift ────────────────────────────────────────────────

    /// AC: Drift from external MTU change — output shows "configuration drift (warning)".
    #[test]
    fn test_drift_mtu_mismatch_produces_warning_finding() {
        let applied = make_applied_entry(
            10,
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 1500u64 }),
            3600,
        );
        let current = vec![make_ser_state(
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 9000u64 }),
        )];

        let findings =
            detect_configuration_drift(&[], &current, "ethernet", "eth0", Some(&applied));

        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Warning);
        assert!(matches!(f.pattern, PatternKind::ConfigurationDrift));
        let all_text = format!("{} {:?}", f.summary, f.details);
        assert!(all_text.contains("1500"), "should mention desired value 1500");
        assert!(all_text.contains("9000"), "should mention current value 9000");
        assert!(all_text.contains("mtu"), "should mention the field name");
    }

    /// AC: No drift when system matches policy — output shows "healthy".
    #[test]
    fn test_drift_no_finding_when_state_matches_policy() {
        let applied = make_applied_entry(
            10,
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 1500u64 }),
            3600,
        );
        let current = vec![make_ser_state(
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 1500u64 }),
        )];

        let findings =
            detect_configuration_drift(&[], &current, "ethernet", "eth0", Some(&applied));

        assert!(findings.is_empty(), "no drift when current matches policy");
    }

    /// AC: Drift with multiple fields — drift for mtu detected, extra addresses mentioned.
    #[test]
    fn test_drift_multiple_fields_all_appear_in_details() {
        let applied = make_applied_entry(
            10,
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 1500u64, "addresses": ["10.0.1.50/24"] }),
            3600,
        );
        let current = vec![make_ser_state(
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 9000u64, "addresses": ["10.0.1.50/24", "10.0.1.99/24"] }),
        )];

        let findings =
            detect_configuration_drift(&[], &current, "ethernet", "eth0", Some(&applied));

        assert!(!findings.is_empty(), "should detect drift on at least one field");
        let all_text = format!("{} {:?}", findings[0].summary, findings[0].details);
        assert!(all_text.contains("mtu"), "should mention mtu drift");
    }

    /// AC: No finding when no applied entry exists.
    #[test]
    fn test_drift_no_finding_when_no_last_applied() {
        let current = vec![make_ser_state(
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 9000u64 }),
        )];

        let findings = detect_configuration_drift(&[], &current, "ethernet", "eth0", None);

        assert!(findings.is_empty(), "no finding without a last applied entry");
    }

    /// AC: Drift output suggests "Run `netfyr apply` to re-converge".
    #[test]
    fn test_drift_suggests_apply_action() {
        let applied = make_applied_entry(
            10,
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 1500u64 }),
            3600,
        );
        let current = vec![make_ser_state(
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 9000u64 }),
        )];

        let findings =
            detect_configuration_drift(&[], &current, "ethernet", "eth0", Some(&applied));

        assert!(!findings.is_empty());
        let actions = findings[0].suggested_actions.join(" ");
        assert!(
            actions.contains("netfyr apply"),
            "should suggest `netfyr apply` to re-converge"
        );
    }

    /// AC: Drift suggests "netfyr revert <seq>" with the last applied seq.
    #[test]
    fn test_drift_suggests_revert_with_correct_seq() {
        let applied = make_applied_entry(
            42,
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 1500u64 }),
            3600,
        );
        let current = vec![make_ser_state(
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 9000u64 }),
        )];

        let findings =
            detect_configuration_drift(&[], &current, "ethernet", "eth0", Some(&applied));

        assert!(!findings.is_empty());
        let actions = findings[0].suggested_actions.join(" ");
        assert!(actions.contains("revert") && actions.contains("42"), "should suggest revert 42");
    }

    /// AC: Carrier is a read-only field and must not appear in drift findings.
    #[test]
    fn test_drift_skips_read_only_carrier_field() {
        // Both desired and current have different carrier values, but carrier is read-only.
        let applied = make_applied_entry(
            10,
            "ethernet",
            "eth0",
            serde_json::json!({ "carrier": true }),
            3600,
        );
        let current = vec![make_ser_state(
            "ethernet",
            "eth0",
            serde_json::json!({ "carrier": false }),
        )];

        let findings =
            detect_configuration_drift(&[], &current, "ethernet", "eth0", Some(&applied));

        // carrier is read-only (x-netfyr-writable: false), so drift should not be reported
        assert!(findings.is_empty(), "carrier is read-only, should not cause drift finding");
    }

    // ── detect_carrier_loss ───────────────────────────────────────────────────────

    /// AC: Interface lost carrier — output shows "carrier loss (critical)".
    #[test]
    fn test_carrier_loss_detected_when_current_state_has_carrier_false() {
        let ext_entry = make_external_change_entry(
            49,
            "ethernet",
            "eth0",
            serde_json::json!({ "carrier": false }),
            600,
        );
        let current =
            vec![make_ser_state("ethernet", "eth0", serde_json::json!({ "carrier": false }))];
        let entries = vec![ext_entry];

        let findings = detect_carrier_loss(&entries, &current, "ethernet", "eth0", None);

        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Critical);
        assert!(matches!(f.pattern, PatternKind::CarrierLoss));
    }

    /// AC: Carrier recovered — output does not show carrier loss.
    #[test]
    fn test_no_carrier_loss_when_carrier_has_recovered() {
        let ext_loss = make_external_change_entry(
            48,
            "ethernet",
            "eth0",
            serde_json::json!({ "carrier": false }),
            1200,
        );
        let ext_recover = make_external_change_entry(
            49,
            "ethernet",
            "eth0",
            serde_json::json!({ "carrier": true }),
            600,
        );
        // Current state shows carrier=true (recovered)
        let current =
            vec![make_ser_state("ethernet", "eth0", serde_json::json!({ "carrier": true }))];
        let entries = vec![ext_loss, ext_recover];

        let findings = detect_carrier_loss(&entries, &current, "ethernet", "eth0", None);

        assert!(findings.is_empty(), "no carrier loss when carrier has recovered");
    }

    /// AC: Output shows when carrier was lost (timestamp and seq).
    #[test]
    fn test_carrier_loss_details_include_seq_and_time() {
        let ext_entry = make_external_change_entry(
            49,
            "ethernet",
            "eth0",
            serde_json::json!({ "carrier": false }),
            600,
        );
        let current =
            vec![make_ser_state("ethernet", "eth0", serde_json::json!({ "carrier": false }))];

        let findings = detect_carrier_loss(&[ext_entry], &current, "ethernet", "eth0", None);

        assert!(!findings.is_empty());
        let all_text = format!("{:?}", findings[0].details);
        assert!(all_text.contains("49"), "should mention the seq number 49");
        assert!(
            all_text.contains("ago"),
            "should mention how long ago carrier was lost"
        );
    }

    /// AC: Output suggests checking physical cable or switch port.
    #[test]
    fn test_carrier_loss_suggests_checking_cable_or_switch_port() {
        let current =
            vec![make_ser_state("ethernet", "eth0", serde_json::json!({ "carrier": false }))];

        let findings = detect_carrier_loss(&[], &current, "ethernet", "eth0", None);

        assert!(!findings.is_empty());
        let actions = findings[0].suggested_actions.join(" ");
        assert!(
            actions.contains("cable") || actions.contains("switch port"),
            "should suggest checking cable or switch port"
        );
    }

    // ── detect_dhcp_lease_lost ────────────────────────────────────────────────────

    /// AC: DHCP lease expired without renewal — shows "DHCP lease lost (critical)".
    #[test]
    fn test_dhcp_lease_lost_when_expire_without_acquire() {
        let expire_entry = make_dhcp_event_entry(
            51,
            "ethernet",
            "eth0",
            "eth0-dhcp",
            "lease_expired",
            300,
        );
        let entries = vec![expire_entry];

        let findings = detect_dhcp_lease_lost(&entries, &[], "ethernet", "eth0", None);

        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Critical);
        assert!(matches!(f.pattern, PatternKind::DhcpLeaseLost));
    }

    /// AC: DHCP lease expired then renewed — does not show DHCP lease loss.
    #[test]
    fn test_no_dhcp_lease_lost_when_acquire_follows_expire() {
        let expire_entry = make_dhcp_event_entry(
            51,
            "ethernet",
            "eth0",
            "eth0-dhcp",
            "lease_expired",
            600,
        );
        let acquire_entry = make_dhcp_event_entry(
            52,
            "ethernet",
            "eth0",
            "eth0-dhcp",
            "lease_acquired",
            300,
        );
        let entries = vec![expire_entry, acquire_entry];

        let findings = detect_dhcp_lease_lost(&entries, &[], "ethernet", "eth0", None);

        assert!(findings.is_empty(), "no finding when lease was re-acquired after expiry");
    }

    /// AC: DHCP lease renewed also clears the finding.
    #[test]
    fn test_no_dhcp_lease_lost_when_renewal_follows_expire() {
        let expire_entry = make_dhcp_event_entry(
            51,
            "ethernet",
            "eth0",
            "eth0-dhcp",
            "lease_expired",
            600,
        );
        let renew_entry = make_dhcp_event_entry(
            52,
            "ethernet",
            "eth0",
            "eth0-dhcp",
            "lease_renewed",
            300,
        );
        let entries = vec![expire_entry, renew_entry];

        let findings = detect_dhcp_lease_lost(&entries, &[], "ethernet", "eth0", None);

        assert!(findings.is_empty(), "no finding when lease was renewed after expiry");
    }

    /// AC: Suggests checking DHCP server reachability.
    #[test]
    fn test_dhcp_lease_lost_suggests_checking_server_reachability() {
        let expire_entry = make_dhcp_event_entry(
            51,
            "ethernet",
            "eth0",
            "eth0-dhcp",
            "lease_expired",
            300,
        );

        let findings = detect_dhcp_lease_lost(&[expire_entry], &[], "ethernet", "eth0", None);

        assert!(!findings.is_empty());
        let actions = findings[0].suggested_actions.join(" ");
        assert!(
            actions.contains("DHCP") || actions.contains("reachability"),
            "should suggest checking DHCP server reachability"
        );
    }

    // ── detect_recurring_flaps ────────────────────────────────────────────────────

    /// AC: Carrier flapping — output shows "recurring flaps (warning)".
    #[test]
    fn test_recurring_flaps_detected_with_oscillating_carrier() {
        // 5 external changes with carrier alternating: false, true, false, true, false
        let entries = vec![
            make_external_change_entry(
                10,
                "ethernet",
                "eth0",
                serde_json::json!({ "carrier": false }),
                1800,
            ),
            make_external_change_entry(
                11,
                "ethernet",
                "eth0",
                serde_json::json!({ "carrier": true }),
                1500,
            ),
            make_external_change_entry(
                12,
                "ethernet",
                "eth0",
                serde_json::json!({ "carrier": false }),
                1200,
            ),
            make_external_change_entry(
                13,
                "ethernet",
                "eth0",
                serde_json::json!({ "carrier": true }),
                900,
            ),
            make_external_change_entry(
                14,
                "ethernet",
                "eth0",
                serde_json::json!({ "carrier": false }),
                600,
            ),
        ];

        let findings = detect_recurring_flaps(&entries, &[], "ethernet", "eth0", None);

        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Warning);
        assert!(matches!(f.pattern, PatternKind::RecurringFlaps));
    }

    /// AC: Output mentions the number of flaps and the affected field.
    #[test]
    fn test_recurring_flaps_mentions_affected_field_and_count() {
        let entries = vec![
            make_external_change_entry(
                10,
                "ethernet",
                "eth0",
                serde_json::json!({ "carrier": false }),
                1800,
            ),
            make_external_change_entry(
                11,
                "ethernet",
                "eth0",
                serde_json::json!({ "carrier": true }),
                1500,
            ),
            make_external_change_entry(
                12,
                "ethernet",
                "eth0",
                serde_json::json!({ "carrier": false }),
                1200,
            ),
            make_external_change_entry(
                13,
                "ethernet",
                "eth0",
                serde_json::json!({ "carrier": true }),
                900,
            ),
            make_external_change_entry(
                14,
                "ethernet",
                "eth0",
                serde_json::json!({ "carrier": false }),
                600,
            ),
        ];

        let findings = detect_recurring_flaps(&entries, &[], "ethernet", "eth0", None);

        assert!(!findings.is_empty());
        let all_text = format!("{} {:?}", findings[0].summary, findings[0].details);
        assert!(all_text.contains("carrier"), "should mention the oscillating field 'carrier'");
        assert!(all_text.contains("5"), "should mention the number of changes");
    }

    /// AC: Multiple changes without oscillation are not flaps.
    #[test]
    fn test_no_recurring_flaps_without_oscillation() {
        // 5 changes but each has a different address — no value repeats
        let entries = vec![
            make_external_change_entry(
                10,
                "ethernet",
                "eth0",
                serde_json::json!({ "addr": "10.0.0.1" }),
                1800,
            ),
            make_external_change_entry(
                11,
                "ethernet",
                "eth0",
                serde_json::json!({ "addr": "10.0.0.2" }),
                1500,
            ),
            make_external_change_entry(
                12,
                "ethernet",
                "eth0",
                serde_json::json!({ "addr": "10.0.0.3" }),
                1200,
            ),
            make_external_change_entry(
                13,
                "ethernet",
                "eth0",
                serde_json::json!({ "addr": "10.0.0.4" }),
                900,
            ),
            make_external_change_entry(
                14,
                "ethernet",
                "eth0",
                serde_json::json!({ "addr": "10.0.0.5" }),
                600,
            ),
        ];

        let findings = detect_recurring_flaps(&entries, &[], "ethernet", "eth0", None);

        assert!(findings.is_empty(), "no flaps when values never repeat");
    }

    /// AC: 3 or fewer external changes do not trigger flap detection (threshold is >3).
    #[test]
    fn test_no_recurring_flaps_with_three_or_fewer_changes() {
        let entries = vec![
            make_external_change_entry(
                10,
                "ethernet",
                "eth0",
                serde_json::json!({ "carrier": false }),
                600,
            ),
            make_external_change_entry(
                11,
                "ethernet",
                "eth0",
                serde_json::json!({ "carrier": true }),
                400,
            ),
            make_external_change_entry(
                12,
                "ethernet",
                "eth0",
                serde_json::json!({ "carrier": false }),
                200,
            ),
        ];

        let findings = detect_recurring_flaps(&entries, &[], "ethernet", "eth0", None);

        assert!(findings.is_empty(), "no flaps with 3 or fewer changes");
    }

    // ── detect_failed_apply ───────────────────────────────────────────────────────

    /// AC: Recent apply had failures — shows "failed apply (warning)".
    #[test]
    fn test_failed_apply_detected_when_outcome_has_failures() {
        let entry = make_failed_apply_entry(42, "ethernet", "eth0", 2, 1800);

        let findings = detect_failed_apply(&[entry], &[], "ethernet", "eth0", None);

        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Warning);
        assert!(matches!(f.pattern, PatternKind::FailedApply));
        let all_text = format!("{} {:?}", f.summary, f.details);
        assert!(
            all_text.contains("2") || all_text.contains("failure"),
            "should mention failure count"
        );
    }

    /// AC: All recent applies succeeded — does not show failed apply.
    #[test]
    fn test_no_failed_apply_when_all_succeed() {
        let entry = make_applied_entry(
            42,
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 1500u64 }),
            1800,
        );

        let findings = detect_failed_apply(&[entry], &[], "ethernet", "eth0", None);

        assert!(findings.is_empty(), "no finding when all applies succeeded");
    }

    /// AC: Suggests `netfyr history --show <seq>` to inspect failure details.
    #[test]
    fn test_failed_apply_suggests_history_show_with_seq() {
        let entry = make_failed_apply_entry(42, "ethernet", "eth0", 2, 1800);

        let findings = detect_failed_apply(&[entry], &[], "ethernet", "eth0", None);

        assert!(!findings.is_empty());
        let actions = findings[0].suggested_actions.join(" ");
        assert!(
            actions.contains("history") && actions.contains("42"),
            "should suggest `netfyr history --show 42`"
        );
    }

    // ── detect_no_recent_activity ─────────────────────────────────────────────────

    /// AC: Entity has no recent entries — shows "no recent activity (info)".
    #[test]
    fn test_no_recent_activity_when_entity_not_in_window() {
        // No entries in the window for this entity
        let findings = detect_no_recent_activity(&[], &[], "ethernet", "eth0", None);

        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Info);
        assert!(matches!(f.pattern, PatternKind::NoRecentActivity));
    }

    /// AC: Entity appears in window — no "no recent activity" finding.
    #[test]
    fn test_no_recent_activity_not_reported_when_entity_in_window() {
        let entry = make_applied_entry(
            10,
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 1500u64 }),
            1800,
        );

        let findings = detect_no_recent_activity(&[entry], &[], "ethernet", "eth0", None);

        assert!(findings.is_empty(), "no finding when entity appears in the window");
    }

    // ── determine_exit_code ───────────────────────────────────────────────────────

    /// AC: Exit code 2 when findings contain critical severity.
    #[test]
    fn test_exit_code_2_when_critical_findings() {
        let critical = make_finding(Severity::Critical);
        let code = determine_exit_code(&[&critical]);
        assert_eq!(code, ExitCode::from(2u8));
    }

    /// AC: Exit code 1 when findings contain only warning severity.
    #[test]
    fn test_exit_code_1_when_only_warning_findings() {
        let warning = make_finding(Severity::Warning);
        let code = determine_exit_code(&[&warning]);
        assert_eq!(code, ExitCode::from(1u8));
    }

    /// AC: Exit code 0 when all entities are healthy (no findings).
    #[test]
    fn test_exit_code_0_when_no_findings() {
        let code = determine_exit_code(&[]);
        assert_eq!(code, ExitCode::SUCCESS);
    }

    /// AC: Critical takes precedence over warning in exit code calculation.
    #[test]
    fn test_exit_code_2_when_mixed_critical_and_warning() {
        let critical = make_finding(Severity::Critical);
        let warning = make_finding(Severity::Warning);
        let code = determine_exit_code(&[&critical, &warning]);
        assert_eq!(code, ExitCode::from(2u8), "critical must take precedence over warning");
    }

    /// AC: Info findings alone return exit code 0 (info is not an error).
    #[test]
    fn test_exit_code_0_when_only_info_findings() {
        let info = make_finding(Severity::Info);
        let code = determine_exit_code(&[&info]);
        assert_eq!(code, ExitCode::SUCCESS, "info findings should not produce a non-zero exit");
    }

    // ── Severity ordering ─────────────────────────────────────────────────────────

    /// AC: Critical > Warning > Info > Healthy.
    #[test]
    fn test_severity_ordering_is_critical_warning_info_healthy() {
        assert!(Severity::Critical > Severity::Warning);
        assert!(Severity::Warning > Severity::Info);
        assert!(Severity::Info > Severity::Healthy);
    }

    // ── filter_entities_by_selector ───────────────────────────────────────────────

    /// AC: Filter by entity name shows only matching entity.
    #[test]
    fn test_filter_entities_by_name_selector() {
        let entities = vec![
            ("ethernet".to_string(), "eth0".to_string()),
            ("ethernet".to_string(), "eth1".to_string()),
        ];
        let selector = vec![("name".to_string(), "eth0".to_string())];

        let filtered = filter_entities_by_selector(&entities, &selector);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].1, "eth0");
        assert!(!filtered.iter().any(|(_, n)| n == "eth1"), "eth1 should be excluded");
    }

    /// AC: No selector shows all entities.
    #[test]
    fn test_no_selector_returns_all_entities() {
        let entities = vec![
            ("ethernet".to_string(), "eth0".to_string()),
            ("ethernet".to_string(), "eth1".to_string()),
        ];

        let filtered = filter_entities_by_selector(&entities, &[]);

        assert_eq!(filtered.len(), 2);
    }

    // ── discover_managed_entities ─────────────────────────────────────────────────

    /// AC: Primary source is the last applied entry's state_after.
    #[test]
    fn test_discover_entities_from_last_applied_state_after() {
        let applied = make_applied_entry(
            10,
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 1500u64 }),
            3600,
        );

        let entities = discover_managed_entities(&[], Some(&applied));

        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0], ("ethernet".to_string(), "eth0".to_string()));
    }

    /// AC: Falls back to window entries when no applied entry exists.
    #[test]
    fn test_discover_entities_falls_back_to_window_entries() {
        let entry = make_applied_entry(
            10,
            "ethernet",
            "eth0",
            serde_json::json!({}),
            3600,
        );

        let entities = discover_managed_entities(&[entry], None);

        assert!(!entities.is_empty());
        assert!(entities.iter().any(|(_, n)| n == "eth0"), "eth0 should be discovered");
    }

    /// AC: Empty journal with no applied entry returns empty list.
    #[test]
    fn test_discover_entities_empty_when_no_data() {
        let entities = discover_managed_entities(&[], None);
        assert!(entities.is_empty());
    }

    // ── format_text ───────────────────────────────────────────────────────────────

    /// AC: Text output groups by severity — critical before warning before healthy.
    #[test]
    fn test_format_text_critical_before_warning_before_healthy() {
        let critical_finding = Finding {
            entity_name: "eth0".to_string(),
            entity_type: "ethernet".to_string(),
            severity: Severity::Critical,
            pattern: PatternKind::CarrierLoss,
            summary: "carrier lost".to_string(),
            details: vec![],
            suggested_actions: vec![],
            related_entries: vec![],
        };
        let warning_finding = Finding {
            entity_name: "eth1".to_string(),
            entity_type: "ethernet".to_string(),
            severity: Severity::Warning,
            pattern: PatternKind::ConfigurationDrift,
            summary: "drift detected".to_string(),
            details: vec![],
            suggested_actions: vec![],
            related_entries: vec![],
        };
        let entity_findings = vec![
            ("ethernet".to_string(), "eth0".to_string(), vec![critical_finding]),
            ("ethernet".to_string(), "eth1".to_string(), vec![warning_finding]),
        ];
        let healthy = vec![(
            "ethernet".to_string(),
            "eth2".to_string(),
            HealthyInfo { last_apply_seq: None, last_apply_ago: None, carrier_up: None },
        )];

        let text = format_text(&entity_findings, &healthy);

        let eth0_pos = text.find("eth0").expect("eth0 should appear");
        let eth1_pos = text.find("eth1").expect("eth1 should appear");
        let eth2_pos = text.find("eth2").expect("eth2 should appear");
        assert!(eth0_pos < eth1_pos, "critical eth0 must precede warning eth1");
        assert!(eth1_pos < eth2_pos, "warning eth1 must precede healthy eth2");
    }

    /// AC: Text output includes severity label in header.
    #[test]
    fn test_format_text_includes_severity_label_in_header() {
        let finding = Finding {
            entity_name: "eth0".to_string(),
            entity_type: "ethernet".to_string(),
            severity: Severity::Critical,
            pattern: PatternKind::CarrierLoss,
            summary: "carrier lost".to_string(),
            details: vec!["Link is down".to_string()],
            suggested_actions: vec!["Check cable".to_string()],
            related_entries: vec![],
        };
        let entity_findings =
            vec![("ethernet".to_string(), "eth0".to_string(), vec![finding])];

        let text = format_text(&entity_findings, &[]);

        assert!(text.contains("eth0"), "entity name must appear");
        assert!(text.contains("critical"), "severity must appear in header");
    }

    /// AC: Healthy entities show "healthy" and "no drift" line.
    #[test]
    fn test_format_text_healthy_entity_shows_healthy_and_no_drift() {
        let healthy = vec![(
            "ethernet".to_string(),
            "eth0".to_string(),
            HealthyInfo {
                last_apply_seq: Some(42),
                last_apply_ago: Some("2h ago".to_string()),
                carrier_up: Some(true),
            },
        )];

        let text = format_text(&[], &healthy);

        assert!(text.contains("eth0"), "entity name must appear");
        assert!(text.contains("healthy"), "must show 'healthy'");
        assert!(text.contains("no drift"), "must show 'no drift'");
    }

    /// AC: Suggested actions are prefixed with →.
    #[test]
    fn test_format_text_suggested_actions_have_arrow_prefix() {
        let finding = Finding {
            entity_name: "eth0".to_string(),
            entity_type: "ethernet".to_string(),
            severity: Severity::Warning,
            pattern: PatternKind::ConfigurationDrift,
            summary: "drift".to_string(),
            details: vec![],
            suggested_actions: vec!["Run `netfyr apply` to re-converge".to_string()],
            related_entries: vec![],
        };
        let entity_findings =
            vec![("ethernet".to_string(), "eth0".to_string(), vec![finding])];

        let text = format_text(&entity_findings, &[]);

        assert!(text.contains("→"), "actions must be prefixed with →");
        assert!(text.contains("netfyr apply"), "action text must appear");
    }

    // ── format_json ───────────────────────────────────────────────────────────────

    /// AC: JSON output is a valid JSON array with required fields.
    #[test]
    fn test_format_json_produces_valid_array_with_required_fields() {
        let finding = Finding {
            entity_name: "eth0".to_string(),
            entity_type: "ethernet".to_string(),
            severity: Severity::Warning,
            pattern: PatternKind::ConfigurationDrift,
            summary: "Policy wants mtu=1500, system has mtu=9000".to_string(),
            details: vec!["Policy wants mtu=1500, system has mtu=9000".to_string()],
            suggested_actions: vec!["Run `netfyr apply`".to_string()],
            related_entries: vec![10, 49],
        };
        let entity_findings =
            vec![("ethernet".to_string(), "eth0".to_string(), vec![finding])];

        let json_str = format_json(&entity_findings, &[]).expect("format_json must succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("output must be valid JSON");

        assert!(parsed.is_array(), "output must be a JSON array");
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 1);

        let item = &arr[0];
        assert!(item.get("entity").is_some(), "must have 'entity' field");
        assert!(item.get("entity_type").is_some(), "must have 'entity_type' field");
        assert!(item.get("severity").is_some(), "must have 'severity' field");
        assert!(item.get("pattern").is_some(), "must have 'pattern' field");
        assert!(item.get("summary").is_some(), "must have 'summary' field");
        assert!(item.get("details").is_some(), "must have 'details' field");
        assert!(item.get("suggested_actions").is_some(), "must have 'suggested_actions' field");
        assert!(item.get("related_entries").is_some(), "must have 'related_entries' field");
    }

    /// AC: Severity and pattern values are snake_case in JSON output.
    #[test]
    fn test_format_json_severity_and_pattern_are_snake_case() {
        let finding = Finding {
            entity_name: "eth0".to_string(),
            entity_type: "ethernet".to_string(),
            severity: Severity::Critical,
            pattern: PatternKind::CarrierLoss,
            summary: "carrier lost".to_string(),
            details: vec![],
            suggested_actions: vec![],
            related_entries: vec![],
        };
        let entity_findings =
            vec![("ethernet".to_string(), "eth0".to_string(), vec![finding])];

        let json_str = format_json(&entity_findings, &[]).expect("format_json must succeed");
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed[0]["severity"].as_str(), Some("critical"));
        assert_eq!(parsed[0]["pattern"].as_str(), Some("carrier_loss"));
        assert_eq!(parsed[0]["entity"].as_str(), Some("eth0"));
        assert_eq!(parsed[0]["entity_type"].as_str(), Some("ethernet"));
    }

    /// AC: related_entries is an array of sequence IDs in JSON output.
    #[test]
    fn test_format_json_related_entries_is_array_of_u64() {
        let finding = Finding {
            entity_name: "eth0".to_string(),
            entity_type: "ethernet".to_string(),
            severity: Severity::Warning,
            pattern: PatternKind::ConfigurationDrift,
            summary: "drift".to_string(),
            details: vec![],
            suggested_actions: vec![],
            related_entries: vec![10, 49],
        };
        let entity_findings =
            vec![("ethernet".to_string(), "eth0".to_string(), vec![finding])];

        let json_str = format_json(&entity_findings, &[]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let entries = parsed[0]["related_entries"].as_array().expect("must be array");

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].as_u64(), Some(10));
        assert_eq!(entries[1].as_u64(), Some(49));
    }

    // ── run_analysis ──────────────────────────────────────────────────────────────

    /// AC: Empty journal — exit code 0.
    #[test]
    fn test_run_analysis_empty_journal_returns_success() {
        let data = CollectedData {
            entries: vec![],
            current_state: vec![],
            managed_entities: vec![],
            last_applied_entry: None,
        };

        let result = run_analysis(data, &text_args());

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), ExitCode::SUCCESS);
    }

    /// AC: Exit code 2 when critical findings exist.
    #[test]
    fn test_run_analysis_exit_code_2_with_carrier_loss_finding() {
        let ext_entry = make_external_change_entry(
            49,
            "ethernet",
            "eth0",
            serde_json::json!({ "carrier": false }),
            600,
        );
        let current =
            vec![make_ser_state("ethernet", "eth0", serde_json::json!({ "carrier": false }))];

        let data = CollectedData {
            entries: vec![ext_entry],
            current_state: current,
            managed_entities: vec![("ethernet".to_string(), "eth0".to_string())],
            last_applied_entry: None,
        };

        let result = run_analysis(data, &text_args());

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), ExitCode::from(2u8));
    }

    /// AC: Exit code 1 when warning findings exist.
    #[test]
    fn test_run_analysis_exit_code_1_with_drift_finding() {
        let applied = make_applied_entry(
            10,
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 1500u64 }),
            3600,
        );
        let current =
            vec![make_ser_state("ethernet", "eth0", serde_json::json!({ "mtu": 9000u64 }))];

        let data = CollectedData {
            entries: vec![applied.clone()],
            current_state: current,
            managed_entities: vec![("ethernet".to_string(), "eth0".to_string())],
            last_applied_entry: Some(applied),
        };

        let result = run_analysis(data, &text_args());

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), ExitCode::from(1u8));
    }

    /// AC: Exit code 0 when all entities healthy.
    #[test]
    fn test_run_analysis_exit_code_0_when_all_healthy() {
        let applied = make_applied_entry(
            10,
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 1500u64 }),
            3600,
        );
        let current =
            vec![make_ser_state("ethernet", "eth0", serde_json::json!({ "mtu": 1500u64 }))];

        let data = CollectedData {
            entries: vec![applied.clone()],
            current_state: current,
            managed_entities: vec![("ethernet".to_string(), "eth0".to_string())],
            last_applied_entry: Some(applied),
        };

        let result = run_analysis(data, &text_args());

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), ExitCode::SUCCESS);
    }

    /// AC: JSON output format returns exit code 1 for drift.
    #[test]
    fn test_run_analysis_json_output_with_drift_returns_exit_code_1() {
        let applied = make_applied_entry(
            10,
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 1500u64 }),
            3600,
        );
        let current =
            vec![make_ser_state("ethernet", "eth0", serde_json::json!({ "mtu": 9000u64 }))];

        let data = CollectedData {
            entries: vec![applied.clone()],
            current_state: current,
            managed_entities: vec![("ethernet".to_string(), "eth0".to_string())],
            last_applied_entry: Some(applied),
        };

        let result = run_analysis(data, &json_args());

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), ExitCode::from(1u8));
    }

    /// AC: Filter by selector — only selected entity findings shown, exit code reflects it.
    #[test]
    fn test_run_analysis_selector_limits_analysis_to_matching_entity() {
        // eth0 drifted, eth1 healthy; selector specifies eth0 only
        let mut applied = make_applied_entry(
            10,
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 1500u64 }),
            3600,
        );
        // Add eth1 to state_after so it's "managed"
        applied.state_after.entities.push(make_ser_state(
            "ethernet",
            "eth1",
            serde_json::json!({ "mtu": 1500u64 }),
        ));

        let current = vec![
            make_ser_state("ethernet", "eth0", serde_json::json!({ "mtu": 9000u64 })),
            make_ser_state("ethernet", "eth1", serde_json::json!({ "mtu": 1500u64 })),
        ];

        let data = CollectedData {
            entries: vec![applied.clone()],
            current_state: current,
            managed_entities: vec![
                ("ethernet".to_string(), "eth0".to_string()),
                ("ethernet".to_string(), "eth1".to_string()),
            ],
            last_applied_entry: Some(applied),
        };
        let args = DiagnoseArgs {
            selector: vec![("name".to_string(), "eth0".to_string())],
            since: "1h".to_string(),
            output: DiagnoseOutputFormat::Text,
        };

        let result = run_analysis(data, &args);

        // Only eth0 is analyzed; it has drift → exit code 1
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), ExitCode::from(1u8));
    }

    // ── Time window behavior ──────────────────────────────────────────────────────

    /// AC: Only entries within --since window are analyzed; older entries are pre-filtered.
    /// The detector receives only window entries; carrier-loss outside the window is not
    /// reported if current state shows carrier=true.
    #[test]
    fn test_carrier_loss_outside_window_not_reported_when_carrier_recovered() {
        // Simulate: the carrier-loss entry was outside the window and was filtered out
        // before calling the detector. Current state shows carrier=true (recovered).
        let current =
            vec![make_ser_state("ethernet", "eth0", serde_json::json!({ "carrier": true }))];

        // No entries passed in (they were outside the since window)
        let findings = detect_carrier_loss(&[], &current, "ethernet", "eth0", None);

        assert!(findings.is_empty(), "no carrier loss when carrier=true and no window entries");
    }

    /// AC: Wider window catches older events — entry from 2h ago in a 3h window is detected.
    #[test]
    fn test_carrier_loss_within_wider_window_is_reported() {
        let old_entry = make_external_change_entry(
            40,
            "ethernet",
            "eth0",
            serde_json::json!({ "carrier": false }),
            7200, // 2 hours old — within a 3h window
        );
        let current =
            vec![make_ser_state("ethernet", "eth0", serde_json::json!({ "carrier": false }))];

        let findings = detect_carrier_loss(&[old_entry], &current, "ethernet", "eth0", None);

        assert_eq!(findings.len(), 1, "carrier loss within wider window must be reported");
    }

    // ── parse_diagnose_selector ───────────────────────────────────────────────────

    /// AC: "name=eth0" parses to ("name", "eth0").
    #[test]
    fn test_parse_diagnose_selector_valid_name() {
        let result = parse_diagnose_selector("name=eth0");
        assert!(result.is_ok());
        let (k, v) = result.unwrap();
        assert_eq!(k, "name");
        assert_eq!(v, "eth0");
    }

    /// AC: Non-"name" key returns an error.
    #[test]
    fn test_parse_diagnose_selector_invalid_key_returns_error() {
        let result = parse_diagnose_selector("type=ethernet");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("name"), "error should mention 'name' key");
    }

    /// AC: Missing '=' returns an error mentioning key=value format.
    #[test]
    fn test_parse_diagnose_selector_no_equals_returns_error() {
        let result = parse_diagnose_selector("eth0");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("key=value"), "error should mention key=value format");
    }

    // ── detect_policy_conflict ────────────────────────────────────────────────────

    /// AC: Policy conflict detected when a field oscillates 2+ times across 4 applied entries.
    /// Oscillation pattern: mtu cycles 1500→9000→1500→9000 (two revisits = two oscillations).
    #[test]
    fn test_policy_conflict_detected_when_field_oscillates_across_four_applies() {
        let entries = vec![
            make_applied_entry(10, "ethernet", "eth0", serde_json::json!({ "mtu": 1500u64 }), 3600),
            make_applied_entry(11, "ethernet", "eth0", serde_json::json!({ "mtu": 9000u64 }), 3500),
            make_applied_entry(12, "ethernet", "eth0", serde_json::json!({ "mtu": 1500u64 }), 3400),
            make_applied_entry(13, "ethernet", "eth0", serde_json::json!({ "mtu": 9000u64 }), 3300),
        ];

        let findings = detect_policy_conflict(&entries, &[], "ethernet", "eth0", None);

        assert_eq!(findings.len(), 1, "should detect a policy conflict finding");
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Warning);
        assert!(matches!(f.pattern, PatternKind::PolicyConflict));
    }

    /// AC: Policy conflict finding details mention the oscillating field.
    #[test]
    fn test_policy_conflict_finding_mentions_oscillating_field() {
        let entries = vec![
            make_applied_entry(10, "ethernet", "eth0", serde_json::json!({ "mtu": 1500u64 }), 3600),
            make_applied_entry(11, "ethernet", "eth0", serde_json::json!({ "mtu": 9000u64 }), 3500),
            make_applied_entry(12, "ethernet", "eth0", serde_json::json!({ "mtu": 1500u64 }), 3400),
            make_applied_entry(13, "ethernet", "eth0", serde_json::json!({ "mtu": 9000u64 }), 3300),
        ];

        let findings = detect_policy_conflict(&entries, &[], "ethernet", "eth0", None);

        assert!(!findings.is_empty());
        let all_text = format!("{} {:?}", findings[0].summary, findings[0].details);
        assert!(all_text.contains("mtu"), "conflict finding must name the oscillating field 'mtu'");
    }

    /// AC: Policy conflict suggests reviewing overlapping policies.
    #[test]
    fn test_policy_conflict_suggests_reviewing_policies() {
        let entries = vec![
            make_applied_entry(10, "ethernet", "eth0", serde_json::json!({ "mtu": 1500u64 }), 3600),
            make_applied_entry(11, "ethernet", "eth0", serde_json::json!({ "mtu": 9000u64 }), 3500),
            make_applied_entry(12, "ethernet", "eth0", serde_json::json!({ "mtu": 1500u64 }), 3400),
            make_applied_entry(13, "ethernet", "eth0", serde_json::json!({ "mtu": 9000u64 }), 3300),
        ];

        let findings = detect_policy_conflict(&entries, &[], "ethernet", "eth0", None);

        assert!(!findings.is_empty());
        let actions = findings[0].suggested_actions.join(" ");
        assert!(
            actions.contains("polic"),
            "suggested actions should mention reviewing policies, got: {:?}",
            actions
        );
    }

    /// AC: Fewer than 3 applied entries do not trigger a policy conflict (threshold is ≥3).
    #[test]
    fn test_no_policy_conflict_with_fewer_than_three_applied_entries() {
        // Only 2 applies, even with oscillation — below the threshold.
        let entries = vec![
            make_applied_entry(10, "ethernet", "eth0", serde_json::json!({ "mtu": 1500u64 }), 3600),
            make_applied_entry(11, "ethernet", "eth0", serde_json::json!({ "mtu": 9000u64 }), 3500),
        ];

        let findings = detect_policy_conflict(&entries, &[], "ethernet", "eth0", None);

        assert!(findings.is_empty(), "fewer than 3 applied entries must not trigger conflict");
    }

    /// AC: Fields that change monotonically (no revisited values) are not reported as conflicts.
    #[test]
    fn test_no_policy_conflict_when_field_changes_monotonically() {
        // mtu increases uniquely across 4 applies — no value is ever revisited.
        let entries = vec![
            make_applied_entry(10, "ethernet", "eth0", serde_json::json!({ "mtu": 1000u64 }), 3600),
            make_applied_entry(11, "ethernet", "eth0", serde_json::json!({ "mtu": 1500u64 }), 3500),
            make_applied_entry(12, "ethernet", "eth0", serde_json::json!({ "mtu": 2000u64 }), 3400),
            make_applied_entry(13, "ethernet", "eth0", serde_json::json!({ "mtu": 9000u64 }), 3300),
        ];

        let findings = detect_policy_conflict(&entries, &[], "ethernet", "eth0", None);

        assert!(findings.is_empty(), "monotonically changing fields must not trigger conflict");
    }

    /// AC: Entries not for the target entity are ignored in conflict detection.
    #[test]
    fn test_no_policy_conflict_when_oscillation_is_on_different_entity() {
        // eth1 oscillates, but we're analyzing eth0 (which only has 2 stable applies).
        let entries = vec![
            make_applied_entry(10, "ethernet", "eth0", serde_json::json!({ "mtu": 1500u64 }), 3600),
            make_applied_entry(11, "ethernet", "eth0", serde_json::json!({ "mtu": 1500u64 }), 3500),
            // eth1 entries with oscillation — should not affect eth0 analysis.
            make_applied_entry(12, "ethernet", "eth1", serde_json::json!({ "mtu": 9000u64 }), 3400),
            make_applied_entry(13, "ethernet", "eth1", serde_json::json!({ "mtu": 1500u64 }), 3300),
            make_applied_entry(14, "ethernet", "eth1", serde_json::json!({ "mtu": 9000u64 }), 3200),
            make_applied_entry(15, "ethernet", "eth1", serde_json::json!({ "mtu": 1500u64 }), 3100),
        ];

        let findings = detect_policy_conflict(&entries, &[], "ethernet", "eth0", None);

        assert!(findings.is_empty(), "oscillation on eth1 must not produce conflict for eth0");
    }

    // ── run_analysis: selector matches no entity ──────────────────────────────────

    /// AC: When selector matches no managed entity, exit code is 0 (no error to report).
    #[test]
    fn test_run_analysis_no_matching_entity_for_selector_returns_success() {
        let applied = make_applied_entry(
            10,
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 1500u64 }),
            3600,
        );
        let data = CollectedData {
            entries: vec![applied.clone()],
            current_state: vec![make_ser_state(
                "ethernet",
                "eth0",
                serde_json::json!({ "mtu": 1500u64 }),
            )],
            managed_entities: vec![("ethernet".to_string(), "eth0".to_string())],
            last_applied_entry: Some(applied),
        };
        let args = DiagnoseArgs {
            selector: vec![("name".to_string(), "eth99".to_string())],
            since: "1h".to_string(),
            output: DiagnoseOutputFormat::Text,
        };

        let result = run_analysis(data, &args);

        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            ExitCode::SUCCESS,
            "selector matching no entity must return exit code 0"
        );
    }

    // ── drift with multiple fields: both fields reported ──────────────────────────

    /// AC: Drift with multiple fields — both mtu and addresses differences appear in details.
    #[test]
    fn test_drift_multiple_fields_both_mtu_and_address_changes_reported() {
        let applied = make_applied_entry(
            10,
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 1500u64, "addresses": ["10.0.1.50/24"] }),
            3600,
        );
        // Current state has different mtu AND extra address.
        let current = vec![make_ser_state(
            "ethernet",
            "eth0",
            serde_json::json!({ "mtu": 9000u64, "addresses": ["10.0.1.50/24", "10.0.1.99/24"] }),
        )];

        let findings =
            detect_configuration_drift(&[], &current, "ethernet", "eth0", Some(&applied));

        assert!(!findings.is_empty(), "should detect at least one drift finding");
        // At least two drift details should be present (mtu + addresses).
        assert!(
            findings[0].details.len() >= 2,
            "drift with two differing fields must produce at least 2 detail lines, got: {:?}",
            findings[0].details
        );
        let all_text = findings[0].details.join(" ");
        assert!(all_text.contains("mtu"), "details must mention mtu drift");
        assert!(
            all_text.contains("addresses") || all_text.contains("10.0.1.99"),
            "details must mention address drift, got: {:?}",
            findings[0].details
        );
    }
}
