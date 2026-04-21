//! Implementation of the `netfyr revert <seq>` subcommand.
//!
//! Two runtime modes are supported, detected automatically:
//!
//! 1. **Daemon-free**: daemon not reachable → apply directly via the local backend.
//! 2. **Daemon**: daemon is running → delegate via Varlink `io.netfyr.Revert`.

use std::collections::HashSet;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Args;
use colored::Colorize;
use netfyr_journal::{
    summarize_policies, ApplyOutcome, Journal, JournalEntry, SerializableDiff,
    SerializableStateSet, Trigger,
};
use netfyr_reconcile::{generate_diff, EntityKey, StateDiff as ReconcileDiff};
use netfyr_state::diff::diff as compute_state_diff;
use netfyr_state::{SchemaRegistry, StateSet};
use netfyr_varlink::{VarlinkApplyReport, VarlinkClient, VarlinkError};

use crate::apply::{create_backend_registry, determine_exit_code, display_apply_report};
use netfyr_reconcile::ConflictReport;

// ── CLI argument struct ───────────────────────────────────────────────────────

#[derive(Args)]
pub struct RevertArgs {
    /// Sequence ID of the journal entry to revert to.
    /// System state will be restored to match this entry's state_after snapshot.
    pub target: u64,

    /// Preview the changes without applying.
    #[arg(long)]
    pub dry_run: bool,
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// Run the `revert` subcommand.
pub async fn run_revert(args: RevertArgs) -> Result<ExitCode> {
    let socket_path = crate::daemon_socket_path();

    match VarlinkClient::connect(&socket_path).await {
        Ok(client) => {
            return run_revert_daemon(client, args).await;
        }
        Err(VarlinkError::ConnectionFailed(_)) => {
            // Daemon not running — fall through to daemon-free mode.
        }
        Err(e) => {
            return Err(anyhow::Error::from(e)
                .context("unexpected error connecting to daemon socket"));
        }
    }

    run_revert_standalone(args).await
}

// ── Daemon mode ───────────────────────────────────────────────────────────────

async fn run_revert_daemon(mut client: VarlinkClient, args: RevertArgs) -> Result<ExitCode> {
    let (report, entry_timestamp) = match client.revert(args.target, args.dry_run).await {
        Ok(r) => r,
        Err(VarlinkError::EntryNotFound(msg)) => {
            eprintln!("Error: {}", msg);
            return Ok(ExitCode::from(1u8));
        }
        Err(e) => {
            return Err(anyhow::Error::from(e).context("revert via daemon failed"));
        }
    };

    println!(
        "Reverting to state from entry #{} ({} UTC)",
        args.target, entry_timestamp
    );

    if args.dry_run {
        display_varlink_revert_dry_run(&report);
        return Ok(ExitCode::from(if report.changes.is_empty() { 0u8 } else { 1u8 }));
    }

    display_varlink_revert_report(&report);

    eprintln!(
        "Warning: the active policy set was not changed. The daemon may re-apply"
    );
    eprintln!("the current desired state on the next reconciliation cycle.");

    Ok(varlink_revert_exit_code(&report))
}

// ── Daemon-free mode ──────────────────────────────────────────────────────────

async fn run_revert_standalone(args: RevertArgs) -> Result<ExitCode> {
    let mut journal = Journal::open_default().context("failed to open journal")?;

    let entry = match journal
        .read_entry(args.target)
        .context("failed to read journal entry")?
    {
        Some(e) => e,
        None => {
            eprintln!("Error: Entry #{} not found", args.target);
            return Ok(ExitCode::from(1u8));
        }
    };

    println!(
        "Reverting to state from entry #{} ({} UTC)",
        entry.seq,
        entry.timestamp.format("%Y-%m-%d %H:%M:%S")
    );

    let target_state = entry
        .state_after
        .to_state_set()
        .map_err(|e| anyhow::anyhow!("failed to decode journal snapshot: {}", e))?;

    let registry = create_backend_registry();
    let actual_state = registry
        .query_all()
        .await
        .context("failed to query current system state")?;

    let schema = SchemaRegistry::default();
    let managed_entities: HashSet<EntityKey> = target_state.entities().into_iter().collect();

    let reconcile_diff: ReconcileDiff =
        generate_diff(&target_state, &actual_state, &managed_entities, &schema);

    // Restrict actual state to only entities present in the target snapshot.
    let mut managed_actual = StateSet::new();
    for (entity_type, selector_key) in target_state.entities() {
        if let Some(state) = actual_state.get(&entity_type, &selector_key) {
            managed_actual.insert(state.clone());
        }
    }

    let state_diff = compute_state_diff(&managed_actual, &target_state);

    if args.dry_run {
        if state_diff.is_empty() {
            println!("No changes needed. System is already in the target state.");
            return Ok(ExitCode::SUCCESS);
        }
        println!("Changes that would be applied:");
        display_revert_dry_run(&reconcile_diff);
        return Ok(ExitCode::from(1u8));
    }

    if state_diff.is_empty() {
        println!("No changes needed. System is already in the target state.");
        return Ok(ExitCode::SUCCESS);
    }

    let apply_report = registry
        .apply(&state_diff)
        .await
        .context("failed to apply revert changes")?;

    // Record revert in journal (non-fatal on failure).
    let policies_vec: Vec<netfyr_policy::Policy> = vec![];
    let revert_entry = JournalEntry {
        seq: 0,
        timestamp: chrono::Utc::now(),
        trigger: Trigger::Revert { target_seq: args.target },
        active_policies: summarize_policies(&policies_vec),
        diff: SerializableDiff::from(&reconcile_diff),
        state_after: SerializableStateSet::from(&target_state),
        outcome: ApplyOutcome::Applied {
            succeeded: apply_report.succeeded.len() as u32,
            failed: apply_report.failed.len() as u32,
            skipped: apply_report.skipped.len() as u32,
        },
    };
    if let Err(e) = journal.append(revert_entry) {
        tracing::warn!("Failed to write revert journal entry: {}", e);
    }

    display_apply_report(&apply_report, &ConflictReport::new());

    Ok(determine_exit_code(&apply_report, &ConflictReport::new()))
}

// ── Display helpers ───────────────────────────────────────────────────────────

fn display_revert_dry_run(diff: &ReconcileDiff) {
    use netfyr_reconcile::{DiffKind, FieldChangeKind};

    for op in &diff.operations {
        let (prefix, header) = match op.kind {
            DiffKind::Add => (
                "+".green().to_string(),
                format!("+ {} {}", op.entity_type, op.selector.key()).green().to_string(),
            ),
            DiffKind::Remove => (
                "-".red().to_string(),
                format!("- {} {}", op.entity_type, op.selector.key()).red().to_string(),
            ),
            DiffKind::Modify => (
                "~".yellow().to_string(),
                format!("~ {} {}", op.entity_type, op.selector.key()).yellow().to_string(),
            ),
        };
        let _ = prefix;
        println!("  {}", header);

        for fc in &op.field_changes {
            match &fc.change {
                FieldChangeKind::Set { current: Some(cur), desired } => {
                    println!(
                        "  {}",
                        format!("~   {}: {} \u{2192} {}", fc.field_name, cur.value, desired.value)
                            .yellow()
                    );
                }
                FieldChangeKind::Set { current: None, desired } => {
                    println!(
                        "  {}",
                        format!("+   {}: {}", fc.field_name, desired.value).green()
                    );
                }
                FieldChangeKind::Unset { current } => {
                    println!(
                        "  {}",
                        format!("-   {}: {}", fc.field_name, current.value).red()
                    );
                }
                FieldChangeKind::Unchanged { .. } => {}
            }
        }
    }
}

fn display_varlink_revert_dry_run(report: &VarlinkApplyReport) {
    if report.changes.is_empty() {
        println!("No changes needed. System is already in the target state.");
        return;
    }
    println!("Changes that would be applied:");
    for entry in &report.changes {
        let colored_line = match entry.kind.as_str() {
            "add" => format!("  + {} {}: {}", entry.entity_type, entry.entity_name, entry.description)
                .green()
                .to_string(),
            "remove" => format!("  - {} {}: {}", entry.entity_type, entry.entity_name, entry.description)
                .red()
                .to_string(),
            _ => format!("  ~ {} {}: {}", entry.entity_type, entry.entity_name, entry.description)
                .yellow()
                .to_string(),
        };
        println!("{}", colored_line);
    }
}

fn display_varlink_revert_report(report: &VarlinkApplyReport) {
    for entry in &report.changes {
        let line = match entry.status.as_str() {
            "applied" => {
                let prefix = match entry.kind.as_str() {
                    "add" => "+".green().to_string(),
                    "modify" => "~".yellow().to_string(),
                    "remove" => "-".red().to_string(),
                    _ => "?".normal().to_string(),
                };
                format!(
                    "  {} {} {}: {}",
                    prefix, entry.entity_type, entry.entity_name, entry.description
                )
            }
            "failed" => format!(
                "  {} {} {}: {}",
                "x".red(),
                entry.entity_type,
                entry.entity_name,
                entry.description
            ),
            "skipped" => format!(
                "  {} {} {}: {}",
                "s".dimmed(),
                entry.entity_type,
                entry.entity_name,
                entry.description
            ),
            _ => format!("  ? {} {}", entry.entity_type, entry.entity_name),
        };
        println!("{}", line);
    }

    let succeeded = report.succeeded;
    let failed = report.failed;

    if failed == 0 {
        println!(
            "{}",
            format!("Applied {} changes.", succeeded).green()
        );
    } else if succeeded > 0 {
        println!(
            "{}",
            format!(
                "Applied {} of {} changes. {} failed.",
                succeeded,
                succeeded + failed,
                failed
            )
            .yellow()
        );
    } else {
        println!("{}", format!("All {} changes failed.", failed).red());
    }
}

fn varlink_revert_exit_code(report: &VarlinkApplyReport) -> ExitCode {
    if report.failed > 0 && report.succeeded == 0 {
        ExitCode::from(2u8)
    } else if report.failed > 0 {
        ExitCode::from(1u8)
    } else {
        ExitCode::SUCCESS
    }
}
