//! Implementation of the `netfyr apply` subcommand.
//!
//! Two runtime modes are supported, detected automatically:
//!
//! 1. **Daemon-free**: Connect to daemon fails → static policies only, apply directly.
//! 2. **Daemon**: Connect succeeds → submit policies via Varlink, daemon reconciles.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Args;
use colored::Colorize;
use netfyr_journal::{
    summarize_policies, ApplyOutcome, Journal, JournalEntry, SerializableDiff,
    SerializableStateSet, Trigger,
};

use netfyr_backend::{ApplyReport, BackendRegistry, DiffOpKind, NetlinkBackend};
use netfyr_policy::{
    load_policy_dir, load_policy_file, FactoryType, PolicySet, StaticFactory, StateFactory,
};
use netfyr_reconcile::{
    generate_diff, merge, ConflictReport, DiffReport, EntityKey, PolicyId, PolicyInput,
    StateDiff as ReconcileDiff,
};
// Import the state-level diff function via its full module path to avoid the
// name ambiguity between the `diff` module and the re-exported `diff` function.
use netfyr_state::diff::diff as compute_state_diff;
use netfyr_state::{SchemaRegistry, State, StateDiff as StateDiffState, StateSet};
use netfyr_varlink::{
    VarlinkApplyReport, VarlinkClient, VarlinkError, VarlinkPolicy, VarlinkStateDiff,
};

/// Unix socket path for the netfyr daemon's Varlink API.
/// Override with `NETFYR_SOCKET_PATH` environment variable (used in tests and
/// non-systemd deployments that place the socket at a custom path).
fn daemon_socket_path() -> String {
    std::env::var("NETFYR_SOCKET_PATH")
        .unwrap_or_else(|_| "/run/netfyr/netfyr.sock".to_string())
}

// ── CLI argument struct ───────────────────────────────────────────────────────

#[derive(Args)]
pub struct ApplyArgs {
    /// Paths to YAML files or directories containing policies
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,

    /// Show what would change without applying
    #[arg(long)]
    pub dry_run: bool,
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// Run the `apply` subcommand.
///
/// Loads policies from `args.paths`, detects daemon vs. daemon-free mode,
/// and either applies changes locally or delegates to the daemon via Varlink.
pub async fn run_apply(args: ApplyArgs) -> Result<ExitCode> {
    // 1. Load all policies from the provided paths.
    let policy_set = load_policies(&args.paths)?;

    // 1a. Validate all policy states before applying.
    let schema = SchemaRegistry::default();
    validate_policies(&policy_set, &schema)?;

    // 2. Detect runtime mode: try connecting to the daemon socket.
    let socket_path = daemon_socket_path();
    match VarlinkClient::connect(&socket_path).await {
        Ok(client) => {
            // Daemon is running — delegate all work to it.
            return run_apply_daemon(client, &policy_set, args.dry_run).await;
        }
        Err(VarlinkError::ConnectionFailed(_)) => {
            // Socket not found or connection refused — fall through to daemon-free mode.
        }
        Err(e) => {
            return Err(anyhow::Error::from(e).context("unexpected error connecting to daemon socket"));
        }
    }

    // ── Daemon-free mode ──────────────────────────────────────────────────────

    // 3. Reject non-static policies — they require the daemon.
    let non_static: Vec<&netfyr_policy::Policy> = policy_set
        .iter()
        .filter(|p| p.factory_type != FactoryType::Static)
        .collect();
    if !non_static.is_empty() {
        let mut msg = String::new();
        for policy in &non_static {
            let factory_type = format!("{:?}", policy.factory_type).to_lowercase();
            msg.push_str(&format!(
                "policy \"{}\" uses factory \"{}\" which requires the netfyr daemon.\n",
                policy.name, factory_type
            ));
        }
        msg.push_str("Start the daemon with: systemctl start netfyr");
        bail!("{}", msg);
    }

    // 4. Convert each policy into a PolicyInput for the reconciliation engine.
    let inputs = policies_to_inputs(&policy_set)?;

    // Compute managed_entities before merge() consumes the inputs.
    let managed_entities: HashSet<EntityKey> = inputs
        .iter()
        .flat_map(|input| input.state_set.entities())
        .collect();

    // 5. Reconcile: merge all inputs into an effective state, detecting conflicts.
    let reconciliation = merge(inputs);

    // 6. Query the current system state.
    let registry = create_backend_registry();
    let actual_state = registry
        .query_all()
        .await
        .context("failed to query current system state via netlink")?;

    let effective_state = &reconciliation.effective_state;

    // 7. Compute diffs:
    //    - reconcile diff: rich per-field diff for display (old→new values)
    //    - state diff: lightweight diff consumed by registry.apply()
    //
    // generate_diff(desired, actual, managed_entities, schema) — desired first, then actual
    // compute_state_diff(from, to) — from=actual, to=desired
    let reconcile_diff: ReconcileDiff =
        generate_diff(effective_state, &actual_state, &managed_entities, &schema);

    // Restrict actual_state to only managed entities before computing state_diff.
    // This matches the daemon's reconciler behavior: only entities covered by an
    // active policy can receive Remove operations — unmanaged interfaces are
    // left completely untouched.
    let mut managed_actual = StateSet::new();
    for (entity_type, selector_key) in effective_state.entities() {
        if let Some(state) = actual_state.get(&entity_type, &selector_key) {
            managed_actual.insert(state.clone());
        }
    }
    let state_diff: StateDiffState = compute_state_diff(&managed_actual, effective_state, &schema);

    // 8. Dry-run: display planned changes and exit without applying.
    if args.dry_run {
        let is_empty = !reconcile_diff.has_meaningful_changes();
        if !reconciliation.conflicts.is_empty() {
            print_conflicts(&reconciliation.conflicts);
        }
        let diff_report = DiffReport::new(reconcile_diff, effective_state, &actual_state);
        display_dry_run_report(&diff_report, is_empty);
        let code: u8 = if is_empty { 0 } else { 1 };
        return Ok(ExitCode::from(code));
    }

    // 9. No changes — exit early.
    if !reconcile_diff.has_meaningful_changes() {
        if reconciliation.conflicts.is_empty() {
            println!("No changes needed. System is already in desired state.");
            return Ok(ExitCode::SUCCESS);
        } else {
            // Conflicting fields prevented all desired changes; nothing applicable left.
            print_conflicts(&reconciliation.conflicts);
            return Ok(ExitCode::from(1u8));
        }
    }

    // 10. Apply the diff.
    let apply_report = registry
        .apply(&state_diff)
        .await
        .context("failed to apply changes via netlink")?;

    // 10a. Write journal entry (non-fatal on failure).
    {
        let source = args
            .paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let policies_vec: Vec<netfyr_policy::Policy> = policy_set.iter().cloned().collect();
        match Journal::open_default() {
            Ok(mut journal) => {
                let mut serializable_diff = SerializableDiff::from(&reconcile_diff);
                apply_outcomes(&mut serializable_diff, &apply_report);
                let entry = JournalEntry {
                    seq: 0,
                    timestamp: chrono::Utc::now(),
                    trigger: Trigger::PolicyApply { source },
                    active_policies: summarize_policies(&policies_vec),
                    diff: serializable_diff,
                    state_after: SerializableStateSet::from(effective_state),
                    outcome: ApplyOutcome::Applied {
                        succeeded: apply_report.succeeded.len() as u32,
                        failed: apply_report.failed.len() as u32,
                        skipped: apply_report.skipped.len() as u32,
                    },
                };
                if let Err(e) = journal.append(entry) {
                    tracing::warn!("Failed to write journal entry: {}", e);
                }
            }
            Err(e) => {
                tracing::warn!("Failed to open journal: {}", e);
            }
        }
    }

    // 11. Display results.
    display_apply_report(&apply_report, &reconciliation.conflicts);

    // 12. Return exit code.
    Ok(determine_exit_code(&apply_report, &reconciliation.conflicts))
}

// ── Daemon mode ───────────────────────────────────────────────────────────────

async fn run_apply_daemon(
    mut client: VarlinkClient,
    policy_set: &PolicySet,
    dry_run: bool,
) -> Result<ExitCode> {
    let policy_count = policy_set.len();
    let policies: Vec<VarlinkPolicy> = policy_set.iter().map(VarlinkPolicy::from).collect();

    if dry_run {
        let diff = client
            .dry_run(policies)
            .await
            .context("daemon dry-run failed")?;
        let is_empty = diff.operations.is_empty();
        display_varlink_diff(&diff, is_empty);
        return Ok(ExitCode::from(if is_empty { 0u8 } else { 1u8 }));
    }

    let report = match client.submit_policies(policies).await {
        Ok(r) => r,
        Err(VarlinkError::PermissionDenied(msg)) => {
            eprintln!("Error: {}", msg);
            eprintln!("Hint: run as root to apply policies (e.g. sudo netfyr apply ...)");
            return Ok(ExitCode::from(1u8));
        }
        Err(e) => return Err(anyhow::Error::from(e).context("failed to submit policies to daemon")),
    };

    display_varlink_apply_report(&report, policy_count);
    Ok(daemon_exit_code(&report))
}

// ── Policy loading ────────────────────────────────────────────────────────────

/// Load all policies from the given paths (files or directories).
///
/// For files: parses them directly. For directories: recursively finds all
/// `.yaml`/`.yml` files. Fails on missing paths or duplicate policy names
/// across paths.
fn load_policies(paths: &[PathBuf]) -> Result<PolicySet> {
    let mut policy_set = PolicySet::new();

    for path in paths {
        if !path.exists() {
            bail!("path not found: {}", path.display());
        }

        let policies = if path.is_dir() {
            let set = load_policy_dir(path).with_context(|| {
                format!("failed to load policy directory: {}", path.display())
            })?;
            set.iter().cloned().collect::<Vec<_>>()
        } else {
            load_policy_file(path).with_context(|| {
                format!("failed to load policy file: {}", path.display())
            })?
        };

        for policy in policies {
            if policy_set.get(&policy.name).is_some() {
                bail!(
                    "duplicate policy name '{}' (from {})",
                    policy.name,
                    path.display()
                );
            }
            policy_set.insert(policy);
        }
    }

    Ok(policy_set)
}

// ── Validation ───────────────────────────────────────────────────────────────

/// Validate all states in every policy. Returns an error listing all violations
/// so the user sees the full picture before any changes are applied.
fn validate_policies(policy_set: &PolicySet, schema: &SchemaRegistry) -> Result<()> {
    let mut all_errors: Vec<String> = Vec::new();

    for policy in policy_set.iter() {
        let states: Vec<&State> = policy
            .state
            .iter()
            .chain(policy.states.iter().flatten())
            .collect();

        for state in states {
            if let Err(errs) = schema.validate(state) {
                for err in errs.errors() {
                    all_errors.push(format!(
                        "policy '{}': field '{}': {}",
                        policy.name, err.field, err.message
                    ));
                }
            }
        }
    }

    if all_errors.is_empty() {
        Ok(())
    } else {
        bail!("{}", all_errors.join("\n"))
    }
}

// ── Reconciliation helpers ────────────────────────────────────────────────────

/// Convert each static policy in the set into a `PolicyInput` for the
/// reconciliation engine by running it through `StaticFactory`.
fn policies_to_inputs(policy_set: &PolicySet) -> Result<Vec<PolicyInput>> {
    let factory = StaticFactory;
    let mut inputs = Vec::new();

    for policy in policy_set.iter() {
        let state_set = factory.produce(policy).with_context(|| {
            format!("failed to produce state for policy '{}'", policy.name)
        })?;
        inputs.push(PolicyInput {
            policy_id: PolicyId::from(policy.name.clone()),
            priority: policy.priority,
            state_set,
        });
    }

    Ok(inputs)
}

// ── Backend registry ──────────────────────────────────────────────────────────

pub(crate) fn create_backend_registry() -> BackendRegistry {
    let mut registry = BackendRegistry::new();
    // NetlinkBackend is the only backend; registration cannot fail for a single backend.
    registry
        .register(Arc::new(NetlinkBackend::new()))
        .expect("failed to register NetlinkBackend");
    registry
}

// ── Exit code logic ───────────────────────────────────────────────────────────

/// Map `ApplyReport` + `ConflictReport` to an exit code.
///
/// - `2`: total failure (no operations succeeded, at least one failed)
/// - `1`: partial failure or conflicts detected
/// - `0`: all operations succeeded, no conflicts
pub(crate) fn determine_exit_code(report: &ApplyReport, conflicts: &ConflictReport) -> ExitCode {
    if report.is_total_failure() {
        ExitCode::from(2u8)
    } else if report.is_partial() || !conflicts.is_empty() {
        ExitCode::from(1u8)
    } else {
        ExitCode::SUCCESS
    }
}

/// Map `VarlinkApplyReport` to an exit code for daemon mode.
fn daemon_exit_code(report: &VarlinkApplyReport) -> ExitCode {
    if report.failed > 0 && report.succeeded == 0 {
        ExitCode::from(2u8)
    } else if report.failed > 0 || !report.conflicts.is_empty() {
        ExitCode::from(1u8)
    } else {
        ExitCode::SUCCESS
    }
}

// ── Display: daemon-free apply ────────────────────────────────────────────────

/// Print conflict warnings to stderr.
fn print_conflicts(conflicts: &ConflictReport) {
    let n = conflicts.len();
    let word = if n == 1 { "conflict" } else { "conflicts" };
    eprintln!(
        "{}",
        format!(
            "Warning: {} field {} detected. Conflicting fields were not applied.",
            n, word
        )
        .yellow()
    );
    for c in &conflicts.conflicts {
        let (entity_type, entity_name) = &c.entity_key;
        let policies: Vec<String> = c
            .contributions
            .iter()
            .map(|cc| format!("policy \"{}\" sets {}", cc.policy_id, cc.value.value))
            .collect();
        let priority_note = if c.contributions.len() == 2 {
            format!("(both priority {})", c.priority)
        } else {
            format!("(all priority {})", c.priority)
        };
        eprintln!(
            "  {} {} {}: {} {}",
            entity_type,
            entity_name,
            c.field_name,
            policies.join(", "),
            priority_note
        );
    }
}

/// Display the result of a dry-run (daemon-free mode).
fn display_dry_run_report(report: &DiffReport, is_empty: bool) {
    if is_empty {
        println!("No changes needed (dry run).");
        return;
    }
    let n = report.operations.len();
    let word = if n == 1 { "change" } else { "changes" };
    println!(
        "{}",
        format!("Dry run: {} {} would be applied.", n, word).yellow()
    );
    let text = report.format_text();
    if !text.is_empty() {
        // Indent the diff text for readability.
        for line in text.lines() {
            let colored = if line.starts_with('+') {
                format!("  {}", line).green().to_string()
            } else if line.starts_with('-') {
                format!("  {}", line).red().to_string()
            } else if line.starts_with('~') {
                format!("  {}", line).yellow().to_string()
            } else {
                format!("  {}", line)
            };
            println!("{}", colored);
        }
    }
}

/// Display the result of an apply operation (daemon-free mode).
pub fn display_apply_report(report: &ApplyReport, conflicts: &ConflictReport) {
    // Conflicts first.
    if !conflicts.is_empty() {
        print_conflicts(conflicts);
    }

    // Per-operation lines.
    for op in &report.succeeded {
        let prefix = match op.operation {
            DiffOpKind::Add => "+".green().to_string(),
            DiffOpKind::Modify => "~".yellow().to_string(),
            DiffOpKind::Remove => "-".red().to_string(),
        };
        let fields = if op.fields_changed.is_empty() {
            String::new()
        } else {
            format!(": {}", op.fields_changed.join(", "))
        };
        println!("  {} {} {}{}", prefix, op.entity_type, op.selector.key(), fields);
    }
    for op in &report.failed {
        println!(
            "  {} {} {}: {}",
            "x".red(),
            op.entity_type,
            op.selector.key(),
            op.error
        );
    }
    for op in &report.skipped {
        println!(
            "  {} {} {}: {}",
            "s".dimmed(),
            op.entity_type,
            op.selector.key(),
            op.reason
        );
    }

    // Summary line.
    let succeeded = report.succeeded.len();
    let failed = report.failed.len();
    let total = succeeded + failed + report.skipped.len();

    if failed == 0 && succeeded == 0 {
        // Nothing happened (all skipped or empty).
        return;
    }

    if failed == 0 {
        let added = report
            .succeeded
            .iter()
            .filter(|op| op.operation == DiffOpKind::Add)
            .count();
        let modified = report
            .succeeded
            .iter()
            .filter(|op| op.operation == DiffOpKind::Modify)
            .count();
        let removed = report
            .succeeded
            .iter()
            .filter(|op| op.operation == DiffOpKind::Remove)
            .count();

        let mut parts = Vec::new();
        if added > 0 {
            parts.push(format!("{} added", added));
        }
        if modified > 0 {
            parts.push(format!("{} modified", modified));
        }
        if removed > 0 {
            parts.push(format!("{} removed", removed));
        }

        let suffix = if parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", parts.join(", "))
        };
        println!("{}", format!("Applied {} changes{}.", succeeded, suffix).green());
    } else if succeeded > 0 {
        println!(
            "{}",
            format!("Applied {} of {} changes. {} failed.", succeeded, total, failed).yellow()
        );
    } else {
        println!(
            "{}",
            format!("All {} changes failed.", failed).red()
        );
    }
}

// ── Display: daemon mode ──────────────────────────────────────────────────────

/// Display the result of a daemon-mode apply.
fn display_varlink_apply_report(report: &VarlinkApplyReport, policy_count: usize) {
    // Conflict warnings first.
    if !report.conflicts.is_empty() {
        let n = report.conflicts.len();
        let word = if n == 1 { "conflict" } else { "conflicts" };
        eprintln!(
            "{}",
            format!(
                "Warning: {} field {} detected. Conflicting fields were not applied.",
                n, word
            )
            .yellow()
        );
        for c in &report.conflicts {
            eprintln!(
                "  {} {} {}: {:?} -> {:?}",
                c.entity_type, c.entity_name, c.field_name, c.policies, c.values
            );
        }
    }

    // Per-change lines.
    for entry in &report.changes {
        let (prefix, colored_line) = match entry.status.as_str() {
            "applied" => {
                let prefix = match entry.kind.as_str() {
                    "add" => "+".green().to_string(),
                    "modify" => "~".yellow().to_string(),
                    "remove" => "-".red().to_string(),
                    _ => "?".normal().to_string(),
                };
                let line = format!(
                    "  {} {} {}: {}",
                    prefix, entry.entity_type, entry.entity_name, entry.description
                );
                (prefix, line)
            }
            "failed" => {
                let prefix = "x".red().to_string();
                let line = format!(
                    "  {} {} {}: {}",
                    prefix, entry.entity_type, entry.entity_name, entry.description
                );
                (prefix, line)
            }
            "skipped" => {
                let prefix = "s".dimmed().to_string();
                let line = format!(
                    "  {} {} {}: {}",
                    prefix, entry.entity_type, entry.entity_name, entry.description
                );
                (prefix, line)
            }
            _ => {
                let prefix = "?".normal().to_string();
                let line = format!(
                    "  {} {} {}",
                    prefix, entry.entity_type, entry.entity_name
                );
                (prefix, line)
            }
        };
        let _ = prefix; // suppress unused warning if colored_line already contains it
        println!("{}", colored_line);
    }

    // Summary line.
    let policy_word = if policy_count == 1 { "policy" } else { "policies" };
    let succeeded = report.succeeded;
    let failed = report.failed;

    if failed == 0 {
        println!(
            "{}",
            format!(
                "Submitted {} {} to daemon. Applied {} changes.",
                policy_count, policy_word, succeeded
            )
            .green()
        );
    } else if succeeded > 0 {
        println!(
            "{}",
            format!(
                "Submitted {} {} to daemon. Applied {} of {} changes. {} failed.",
                policy_count,
                policy_word,
                succeeded,
                succeeded + failed,
                failed
            )
            .yellow()
        );
    } else {
        println!(
            "{}",
            format!(
                "Submitted {} {} to daemon. All {} changes failed.",
                policy_count, policy_word, failed
            )
            .red()
        );
    }
}

/// Display the result of a daemon-mode dry-run.
fn display_varlink_diff(diff: &VarlinkStateDiff, is_empty: bool) {
    if is_empty {
        println!("No changes needed (dry run).");
        return;
    }

    let n = diff.operations.len();
    let word = if n == 1 { "change" } else { "changes" };
    println!(
        "{}",
        format!("Dry run: {} {} would be applied.", n, word).yellow()
    );

    for op in &diff.operations {
        let (prefix, header) = match op.kind.as_str() {
            "add" => (
                "+".green().to_string(),
                format!("+ {} {}", op.entity_type, op.entity_name),
            ),
            "remove" => (
                "-".red().to_string(),
                format!("- {} {}", op.entity_type, op.entity_name),
            ),
            _ => (
                "~".yellow().to_string(),
                format!("~ {} {}", op.entity_type, op.entity_name),
            ),
        };
        let _ = prefix; // already embedded in the header string
        let colored_header = if op.kind == "add" {
            header.green().to_string()
        } else if op.kind == "remove" {
            header.red().to_string()
        } else {
            header.yellow().to_string()
        };
        println!("  {}", colored_header);

        for fc in &op.field_changes {
            match fc.change_kind.as_str() {
                "set" => {
                    if let Some(current) = &fc.current {
                        let line = format!(
                            "~   {}: {} \u{2192} {}",
                            fc.field_name,
                            current,
                            fc.desired.as_ref().map(|v| v.to_string()).unwrap_or_default()
                        );
                        println!("  {}", line.yellow());
                    } else {
                        let line = format!(
                            "+   {}: {}",
                            fc.field_name,
                            fc.desired.as_ref().map(|v| v.to_string()).unwrap_or_default()
                        );
                        println!("  {}", line.green());
                    }
                }
                "unset" => {
                    let line = format!(
                        "-   {}: {}",
                        fc.field_name,
                        fc.current.as_ref().map(|v| v.to_string()).unwrap_or_default()
                    );
                    println!("  {}", line.red());
                }
                "unchanged" => {
                    println!(
                        "      {}: {}",
                        fc.field_name,
                        fc.current.as_ref().map(|v| v.to_string()).unwrap_or_default()
                    );
                }
                _ => {}
            }
        }
    }
}

// ── Journal helpers ───────────────────────────────────────────────────────────

/// Map per-entity apply results onto the field changes in a `SerializableDiff`.
///
/// `ApplyReport` tracks outcomes at entity-operation granularity. When an entity
/// succeeds, all its fields are "applied"; when it fails, all are "failed". Any
/// diff operation not present in any report category defaults to "skipped".
fn apply_outcomes(diff: &mut SerializableDiff, report: &ApplyReport) {
    for applied in &report.succeeded {
        let key = applied.selector.key();
        for op in &mut diff.operations {
            if op.entity_type == applied.entity_type && op.entity_name == key {
                for fc in &mut op.field_changes {
                    fc.outcome = Some("applied".to_string());
                }
            }
        }
    }
    for failed in &report.failed {
        let key = failed.selector.key();
        for op in &mut diff.operations {
            if op.entity_type == failed.entity_type && op.entity_name == key {
                for fc in &mut op.field_changes {
                    fc.outcome = Some("failed".to_string());
                }
            }
        }
    }
    for skipped in &report.skipped {
        let key = skipped.selector.key();
        for op in &mut diff.operations {
            if op.entity_type == skipped.entity_type && op.entity_name == key {
                for fc in &mut op.field_changes {
                    fc.outcome = Some("skipped".to_string());
                }
            }
        }
    }
    // Unmatched operations default to "skipped".
    for op in &mut diff.operations {
        for fc in &mut op.field_changes {
            if fc.outcome.is_none() {
                fc.outcome = Some("skipped".to_string());
            }
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use netfyr_backend::{AppliedOperation, ApplyReport, BackendError, DiffOpKind, FailedOperation};
    use netfyr_reconcile::{Conflict, ConflictReport};
    use netfyr_state::Selector;
    use netfyr_varlink::{VarlinkApplyReport, VarlinkConflictEntry};

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn make_applied(entity_type: &str, name: &str) -> AppliedOperation {
        AppliedOperation {
            operation: DiffOpKind::Modify,
            entity_type: entity_type.to_string(),
            selector: Selector::with_name(name),
            fields_changed: vec!["mtu".to_string()],
        }
    }

    fn make_failed(entity_type: &str, name: &str) -> FailedOperation {
        FailedOperation {
            operation: DiffOpKind::Modify,
            entity_type: entity_type.to_string(),
            selector: Selector::with_name(name),
            error: BackendError::Internal("interface not found".to_string()),
            fields: vec!["mtu".to_string()],
        }
    }

    fn empty_conflict_report() -> ConflictReport {
        ConflictReport::new()
    }

    fn conflict_report_with_one() -> ConflictReport {
        ConflictReport {
            conflicts: vec![Conflict {
                entity_key: ("ethernet".to_string(), "eth0".to_string()),
                field_name: "mtu".to_string(),
                priority: 100,
                contributions: vec![],
            }],
        }
    }

    fn varlink_report(succeeded: i64, failed: i64, conflict_count: usize) -> VarlinkApplyReport {
        VarlinkApplyReport {
            succeeded,
            failed,
            skipped: 0,
            changes: vec![],
            conflicts: (0..conflict_count)
                .map(|i| VarlinkConflictEntry {
                    entity_type: "ethernet".to_string(),
                    entity_name: format!("eth{}", i),
                    field_name: "mtu".to_string(),
                    policies: vec!["policy-a".to_string(), "policy-b".to_string()],
                    values: vec!["1500".to_string(), "9000".to_string()],
                })
                .collect(),
        }
    }

    // ── Imports for load_policies and async tests ─────────────────────────────
    use std::fs;
    use netfyr_policy::FactoryType as PolicyFactoryType;

    // ── load_policies tests ───────────────────────────────────────────────────

    /// AC "Path does not exist shows error" — error message contains "path not found".
    #[test]
    fn test_load_policies_nonexistent_path_returns_error_with_path_not_found() {
        let result = load_policies(&[PathBuf::from("/nonexistent-path-xyz-123/eth0.yaml")]);
        assert!(result.is_err(), "nonexistent path must return Err");
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("path not found"),
            "error must contain 'path not found', got: {}",
            err
        );
    }

    /// AC "Path does not exist shows error" — path is included in the error message.
    #[test]
    fn test_load_policies_nonexistent_path_error_includes_path_in_message() {
        let path = PathBuf::from("/tmp/netfyr-test-nonexistent-xyz-abc/eth0.yaml");
        let result = load_policies(&[path]);
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("netfyr-test-nonexistent-xyz-abc"),
            "error must include the path, got: {}",
            err
        );
    }

    /// AC "Bare state YAML is auto-wrapped into static policy" — factory type is Static.
    #[test]
    fn test_load_policies_bare_state_yaml_factory_type_is_static() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eth0.yaml");
        fs::write(&path, "type: ethernet\nname: eth0\nmtu: 1500\n").unwrap();

        let policy_set = load_policies(&[path]).unwrap();
        let policy = policy_set.get("eth0").expect("policy 'eth0' must exist");
        assert_eq!(
            policy.factory_type,
            PolicyFactoryType::Static,
            "bare state must be auto-wrapped as Static factory"
        );
    }

    /// AC "Bare state YAML is auto-wrapped into static policy" — default priority is 100.
    #[test]
    fn test_load_policies_bare_state_yaml_default_priority_is_100() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eth0.yaml");
        fs::write(&path, "type: ethernet\nname: eth0\nmtu: 1500\n").unwrap();

        let policy_set = load_policies(&[path]).unwrap();
        let policy = policy_set.get("eth0").expect("policy 'eth0' must exist");
        assert_eq!(policy.priority, 100, "auto-wrapped bare state must use default priority 100");
    }

    /// AC "Bare state YAML is auto-wrapped into static policy" — name derived from filename.
    #[test]
    fn test_load_policies_bare_state_policy_name_derived_from_filename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("myinterface.yaml");
        fs::write(&path, "type: ethernet\nname: eth0\nmtu: 1500\n").unwrap();

        let policy_set = load_policies(&[path]).unwrap();
        assert!(
            policy_set.get("myinterface").is_some(),
            "policy name must be 'myinterface' (derived from filename, without extension)"
        );
    }

    /// AC "Apply all files in a directory" — directory loads all YAML files.
    #[test]
    fn test_load_policies_directory_loads_multiple_yaml_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("eth0.yaml"), "type: ethernet\nname: eth0\nmtu: 1500\n").unwrap();
        fs::write(dir.path().join("eth1.yaml"), "type: ethernet\nname: eth1\nmtu: 9000\n").unwrap();

        let policy_set = load_policies(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(policy_set.len(), 2, "directory with two YAML files must produce two policies");
    }

    /// AC "Apply all files in a directory" — each entity is accessible by derived policy name.
    #[test]
    fn test_load_policies_directory_each_policy_accessible_by_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("eth0.yaml"), "type: ethernet\nname: eth0\nmtu: 1500\n").unwrap();
        fs::write(dir.path().join("eth1.yaml"), "type: ethernet\nname: eth1\nmtu: 9000\n").unwrap();

        let policy_set = load_policies(&[dir.path().to_path_buf()]).unwrap();
        assert!(policy_set.get("eth0").is_some(), "policy 'eth0' must be loaded from eth0.yaml");
        assert!(policy_set.get("eth1").is_some(), "policy 'eth1' must be loaded from eth1.yaml");
    }

    /// AC "YAML parse error returns exit code 2" — invalid YAML returns Err.
    #[test]
    fn test_load_policies_invalid_yaml_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        // This YAML has a mapping key with no value followed by a broken structure.
        fs::write(&path, ": broken: [unclosed\n").unwrap();

        let result = load_policies(&[path]);
        assert!(result.is_err(), "invalid YAML must return Err");
    }

    /// AC "DHCP policy without daemon fails with clear error" — DHCP policy loaded correctly.
    #[test]
    fn test_load_policies_dhcpv4_policy_file_loads_with_dhcpv4_factory_type() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eth0-dhcp.yaml");
        fs::write(
            &path,
            "kind: policy\nname: eth0-dhcp\nfactory: dhcpv4\npriority: 100\nselector:\n  name: eth0\n",
        )
        .unwrap();

        let policy_set = load_policies(&[path]).unwrap();
        let policy = policy_set.get("eth0-dhcp").expect("policy 'eth0-dhcp' must exist");
        assert_eq!(
            policy.factory_type,
            PolicyFactoryType::Dhcpv4,
            "DHCP policy file must produce a Dhcpv4 factory type"
        );
    }

    // ── run_apply async tests ─────────────────────────────────────────────────

    /// AC "DHCP policy without daemon fails with clear error" — run_apply returns Err
    /// mentioning "requires the netfyr daemon" and "systemctl start netfyr".
    ///
    /// Note: this test mutates NETFYR_SOCKET_PATH so daemon-free mode is forced.
    /// All other tests that read NETFYR_SOCKET_PATH will also see a nonexistent path,
    /// which is acceptable since daemon-free mode is the fallback for any failing socket.
    #[tokio::test]
    async fn test_run_apply_dhcp_policy_without_daemon_returns_error_with_daemon_message() {
        let dir = tempfile::tempdir().unwrap();
        let policy_file = dir.path().join("eth0-dhcp.yaml");
        fs::write(
            &policy_file,
            "kind: policy\nname: eth0-dhcp\nfactory: dhcpv4\npriority: 100\nselector:\n  name: eth0\n",
        )
        .unwrap();

        // Point socket path at a file that does not exist so connection fails immediately.
        let socket_path = dir.path().join("daemon.sock").to_string_lossy().into_owned();
        // SAFETY: test-only env mutation; any nonexistent path causes daemon-free fallback.
        unsafe { std::env::set_var("NETFYR_SOCKET_PATH", &socket_path) };

        let args = ApplyArgs { paths: vec![policy_file], dry_run: false };
        let result = run_apply(args).await;

        assert!(result.is_err(), "DHCP policy without daemon must return Err");
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("requires the netfyr daemon"),
            "error must mention 'requires the netfyr daemon', got: {}",
            err_msg
        );
        assert!(
            err_msg.contains("systemctl start netfyr"),
            "error must include 'systemctl start netfyr', got: {}",
            err_msg
        );
    }

    // ── determine_exit_code tests ─────────────────────────────────────────────

    /// AC: exit code 0 when all operations succeed with no conflicts.
    #[test]
    fn test_determine_exit_code_all_succeeded_no_conflicts_returns_exit_0() {
        let mut report = ApplyReport::new();
        report.succeeded.push(make_applied("ethernet", "eth0"));
        let conflicts = empty_conflict_report();
        assert_eq!(
            determine_exit_code(&report, &conflicts),
            ExitCode::SUCCESS,
            "all succeeded, no conflicts must return exit 0"
        );
    }

    /// AC: exit code 1 when some operations succeed and some fail (partial failure).
    #[test]
    fn test_determine_exit_code_partial_failure_returns_exit_1() {
        let mut report = ApplyReport::new();
        report.succeeded.push(make_applied("ethernet", "eth0"));
        report.failed.push(make_failed("ethernet", "eth99"));
        let conflicts = empty_conflict_report();
        assert_eq!(
            determine_exit_code(&report, &conflicts),
            ExitCode::from(1u8),
            "partial failure (some succeeded, some failed) must return exit 1"
        );
    }

    /// AC: exit code 2 when all operations fail (total failure).
    #[test]
    fn test_determine_exit_code_total_failure_returns_exit_2() {
        let mut report = ApplyReport::new();
        report.failed.push(make_failed("ethernet", "eth99"));
        let conflicts = empty_conflict_report();
        assert_eq!(
            determine_exit_code(&report, &conflicts),
            ExitCode::from(2u8),
            "total failure (no succeeded, at least one failed) must return exit 2"
        );
    }

    /// AC: exit code 1 when conflicts are detected even if all applicable changes succeeded.
    #[test]
    fn test_determine_exit_code_conflicts_but_no_failures_returns_exit_1() {
        let mut report = ApplyReport::new();
        report.succeeded.push(make_applied("ethernet", "eth0"));
        let conflicts = conflict_report_with_one();
        assert_eq!(
            determine_exit_code(&report, &conflicts),
            ExitCode::from(1u8),
            "conflicts detected must cause exit 1 even when no apply failures"
        );
    }

    /// Edge: empty report with no conflicts exits 0 (no-op is success).
    #[test]
    fn test_determine_exit_code_empty_report_no_conflicts_returns_exit_0() {
        let report = ApplyReport::new();
        let conflicts = empty_conflict_report();
        assert_eq!(
            determine_exit_code(&report, &conflicts),
            ExitCode::SUCCESS,
            "empty report with no conflicts is treated as success"
        );
    }

    /// Edge: conflicts alone (no failures) produce exit 1, not exit 2.
    #[test]
    fn test_determine_exit_code_only_conflicts_no_failures_returns_exit_1_not_2() {
        let report = ApplyReport::new();
        let conflicts = conflict_report_with_one();
        // is_total_failure() is false (no failures at all), so we get exit 1
        assert_eq!(
            determine_exit_code(&report, &conflicts),
            ExitCode::from(1u8),
            "conflicts alone produce exit 1, not exit 2"
        );
    }

    // ── daemon_exit_code tests ────────────────────────────────────────────────

    /// AC: daemon mode exit 0 when all changes applied, no conflicts.
    #[test]
    fn test_daemon_exit_code_all_succeeded_no_conflicts_returns_exit_0() {
        let report = varlink_report(3, 0, 0);
        assert_eq!(
            daemon_exit_code(&report),
            ExitCode::SUCCESS,
            "daemon mode: all succeeded, no conflicts must return exit 0"
        );
    }

    /// AC: daemon mode exit 0 when zero changes and no conflicts (already in desired state).
    #[test]
    fn test_daemon_exit_code_zero_changes_no_conflicts_returns_exit_0() {
        let report = varlink_report(0, 0, 0);
        assert_eq!(
            daemon_exit_code(&report),
            ExitCode::SUCCESS,
            "daemon mode: zero changes and no failures must return exit 0"
        );
    }

    /// AC: daemon mode exit 1 when some operations failed but some succeeded (partial).
    #[test]
    fn test_daemon_exit_code_partial_failure_returns_exit_1() {
        let report = varlink_report(2, 1, 0);
        assert_eq!(
            daemon_exit_code(&report),
            ExitCode::from(1u8),
            "daemon mode partial failure must return exit 1"
        );
    }

    /// AC: daemon mode exit 2 when all operations failed.
    #[test]
    fn test_daemon_exit_code_total_failure_returns_exit_2() {
        let report = varlink_report(0, 1, 0);
        assert_eq!(
            daemon_exit_code(&report),
            ExitCode::from(2u8),
            "daemon mode total failure must return exit 2"
        );
    }

    /// AC: daemon mode exit 1 when conflicts detected even if all changes applied.
    #[test]
    fn test_daemon_exit_code_conflicts_present_returns_exit_1() {
        let report = varlink_report(2, 0, 1);
        assert_eq!(
            daemon_exit_code(&report),
            ExitCode::from(1u8),
            "daemon mode: conflicts present must return exit 1"
        );
    }

    /// Edge: daemon mode with both failures and conflicts still returns exit 1 (partial).
    #[test]
    fn test_daemon_exit_code_partial_failure_with_conflicts_returns_exit_1() {
        let report = varlink_report(1, 1, 1);
        assert_eq!(
            daemon_exit_code(&report),
            ExitCode::from(1u8),
            "daemon mode partial failure with conflicts must return exit 1"
        );
    }

    // ── Additional load_policies edge-case tests ──────────────────────────────

    /// AC "Apply a single YAML policy file" — a single valid YAML file produces exactly one policy.
    #[test]
    fn test_load_policies_single_valid_file_returns_one_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eth0.yaml");
        fs::write(&path, "type: ethernet\nname: eth0\nmtu: 1500\n").unwrap();

        let policy_set = load_policies(&[path]).unwrap();
        assert_eq!(policy_set.len(), 1, "single YAML file must produce exactly one policy");
    }

    /// AC "Apply all files in a directory" — two explicit file paths are combined into one policy set.
    #[test]
    fn test_load_policies_two_file_paths_combined_into_single_policy_set() {
        let dir = tempfile::tempdir().unwrap();
        let path0 = dir.path().join("eth0.yaml");
        let path1 = dir.path().join("eth1.yaml");
        fs::write(&path0, "type: ethernet\nname: eth0\nmtu: 1500\n").unwrap();
        fs::write(&path1, "type: ethernet\nname: eth1\nmtu: 9000\n").unwrap();

        let policy_set = load_policies(&[path0, path1]).unwrap();
        assert_eq!(policy_set.len(), 2, "two file paths must produce two policies in combined set");
        assert!(policy_set.get("eth0").is_some(), "policy 'eth0' must be in combined set");
        assert!(policy_set.get("eth1").is_some(), "policy 'eth1' must be in combined set");
    }

    /// Edge: loading an empty directory returns an empty policy set without error.
    #[test]
    fn test_load_policies_empty_directory_returns_empty_policy_set() {
        let dir = tempfile::tempdir().unwrap();
        let policy_set = load_policies(&[dir.path().to_path_buf()]).unwrap();
        assert!(policy_set.is_empty(), "empty directory must yield an empty policy set");
    }

    /// Edge: two paths that both produce a policy with the same name return a duplicate error.
    #[test]
    fn test_load_policies_duplicate_policy_name_across_paths_returns_error() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        // Both files have stem "eth0" so both produce policy name "eth0" (bare-state auto-wrap).
        let path1 = dir1.path().join("eth0.yaml");
        let path2 = dir2.path().join("eth0.yaml");
        fs::write(&path1, "type: ethernet\nname: eth0\nmtu: 1500\n").unwrap();
        fs::write(&path2, "type: ethernet\nname: eth0\nmtu: 9000\n").unwrap();

        let result = load_policies(&[path1, path2]);
        assert!(result.is_err(), "duplicate policy names across paths must return Err");
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("duplicate"),
            "error must mention 'duplicate', got: {}",
            err
        );
    }

    // ── display_apply_report smoke tests ─────────────────────────────────────
    //
    // These tests verify that display_apply_report does not panic for various
    // combinations of succeeded/failed operations and conflicts.

    /// AC "Apply a single YAML policy file" — display_apply_report does not panic for empty report.
    #[test]
    fn test_display_apply_report_no_panic_empty_report_no_conflicts() {
        let report = ApplyReport::new();
        let conflicts = empty_conflict_report();
        display_apply_report(&report, &conflicts);
    }

    /// Smoke: display_apply_report handles a single add operation without panic.
    #[test]
    fn test_display_apply_report_no_panic_single_add_operation() {
        let mut report = ApplyReport::new();
        report.succeeded.push(AppliedOperation {
            operation: DiffOpKind::Add,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth1"),
            fields_changed: vec!["mtu".to_string()],
        });
        let conflicts = empty_conflict_report();
        display_apply_report(&report, &conflicts);
    }

    /// Smoke: display_apply_report handles a remove operation without panic.
    #[test]
    fn test_display_apply_report_no_panic_single_remove_operation() {
        let mut report = ApplyReport::new();
        report.succeeded.push(AppliedOperation {
            operation: DiffOpKind::Remove,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth1"),
            fields_changed: vec![],
        });
        let conflicts = empty_conflict_report();
        display_apply_report(&report, &conflicts);
    }

    /// AC "Partial failure reports mixed results" — display_apply_report does not panic for
    /// partial failure (some succeeded, some failed).
    #[test]
    fn test_display_apply_report_no_panic_partial_failure() {
        let mut report = ApplyReport::new();
        report.succeeded.push(make_applied("ethernet", "eth0"));
        report.failed.push(make_failed("ethernet", "eth99"));
        let conflicts = empty_conflict_report();
        display_apply_report(&report, &conflicts);
    }

    /// AC "Total failure returns exit code 2" — display_apply_report does not panic for total failure.
    #[test]
    fn test_display_apply_report_no_panic_total_failure() {
        let mut report = ApplyReport::new();
        report.failed.push(make_failed("ethernet", "eth99"));
        let conflicts = empty_conflict_report();
        display_apply_report(&report, &conflicts);
    }

    /// AC "Conflicts are reported as warnings" — display_apply_report does not panic with conflicts.
    #[test]
    fn test_display_apply_report_no_panic_with_conflict_warning() {
        let report = ApplyReport::new();
        let conflicts = conflict_report_with_one();
        display_apply_report(&report, &conflicts);
    }

    /// Smoke: display_apply_report handles mixed add/modify/remove operations without panic.
    #[test]
    fn test_display_apply_report_no_panic_mixed_operation_types() {
        let mut report = ApplyReport::new();
        report.succeeded.push(AppliedOperation {
            operation: DiffOpKind::Add,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            fields_changed: vec!["mtu".to_string()],
        });
        report.succeeded.push(make_applied("ethernet", "eth1"));
        report.succeeded.push(AppliedOperation {
            operation: DiffOpKind::Remove,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth2"),
            fields_changed: vec![],
        });
        let conflicts = empty_conflict_report();
        display_apply_report(&report, &conflicts);
    }

    /// Smoke: display_apply_report handles operations with no fields_changed without panic.
    #[test]
    fn test_display_apply_report_no_panic_operation_with_empty_fields_changed() {
        let mut report = ApplyReport::new();
        report.succeeded.push(AppliedOperation {
            operation: DiffOpKind::Modify,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            fields_changed: vec![],
        });
        let conflicts = empty_conflict_report();
        display_apply_report(&report, &conflicts);
    }

    // ── apply_outcomes tests ──────────────────────────────────────────────────
    //
    // AC: apply_outcomes maps per-field results onto diff.

    use netfyr_backend::SkippedOperation;
    use netfyr_journal::{SerializableDiff, SerializableDiffOp, SerializableFieldChange};

    fn make_skipped(entity_type: &str, name: &str) -> SkippedOperation {
        SkippedOperation {
            operation: DiffOpKind::Modify,
            entity_type: entity_type.to_string(),
            selector: Selector::with_name(name),
            reason: "already in desired state".to_string(),
        }
    }

    fn make_diff_op(entity_type: &str, entity_name: &str, field_name: &str) -> SerializableDiffOp {
        SerializableDiffOp {
            kind: "modify".to_string(),
            entity_type: entity_type.to_string(),
            entity_name: entity_name.to_string(),
            field_changes: vec![SerializableFieldChange {
                field_name: field_name.to_string(),
                change_kind: "set".to_string(),
                current: Some(serde_json::json!(1500u64)),
                desired: Some(serde_json::json!(9000u64)),
                outcome: None,
            }],
        }
    }

    /// AC: apply_outcomes sets outcome="applied" for succeeded entities,
    /// "failed" for failed entities, and "skipped" for skipped entities.
    #[test]
    fn test_apply_outcomes_maps_per_entity_results_to_field_outcomes() {
        let mut diff = SerializableDiff {
            operations: vec![
                make_diff_op("ethernet", "eth0", "mtu"),      // will succeed
                make_diff_op("ethernet", "eth1", "addresses"), // will fail
                make_diff_op("ethernet", "eth2", "routes"),    // will be skipped
            ],
        };

        let mut report = ApplyReport::new();
        report.succeeded.push(make_applied("ethernet", "eth0"));
        report.failed.push(make_failed("ethernet", "eth1"));
        report.skipped.push(make_skipped("ethernet", "eth2"));

        apply_outcomes(&mut diff, &report);

        let eth0_op = diff.operations.iter().find(|op| op.entity_name == "eth0").unwrap();
        assert_eq!(
            eth0_op.field_changes[0].outcome.as_deref(),
            Some("applied"),
            "eth0 mtu field must have outcome='applied' after succeed"
        );

        let eth1_op = diff.operations.iter().find(|op| op.entity_name == "eth1").unwrap();
        assert_eq!(
            eth1_op.field_changes[0].outcome.as_deref(),
            Some("failed"),
            "eth1 addresses field must have outcome='failed' after failure"
        );

        let eth2_op = diff.operations.iter().find(|op| op.entity_name == "eth2").unwrap();
        assert_eq!(
            eth2_op.field_changes[0].outcome.as_deref(),
            Some("skipped"),
            "eth2 routes field must have outcome='skipped' after skip"
        );
    }

    /// AC: apply_outcomes sets unmatched operations to "skipped" by default.
    #[test]
    fn test_apply_outcomes_unmatched_operations_default_to_skipped() {
        let mut diff = SerializableDiff {
            operations: vec![make_diff_op("ethernet", "eth99", "mtu")],
        };

        // Empty report — eth99 not in any result category.
        let report = ApplyReport::new();
        apply_outcomes(&mut diff, &report);

        assert_eq!(
            diff.operations[0].field_changes[0].outcome.as_deref(),
            Some("skipped"),
            "unmatched operation (not in any result category) must default to outcome='skipped'"
        );
    }

    /// AC: apply_outcomes handles multiple field changes per entity, setting all to the same outcome.
    #[test]
    fn test_apply_outcomes_sets_all_field_changes_in_entity_to_same_outcome() {
        let mut diff = SerializableDiff {
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
                        outcome: None,
                    },
                    SerializableFieldChange {
                        field_name: "addresses".to_string(),
                        change_kind: "set".to_string(),
                        current: Some(serde_json::json!([])),
                        desired: Some(serde_json::json!(["10.0.0.1/24"])),
                        outcome: None,
                    },
                    SerializableFieldChange {
                        field_name: "routes".to_string(),
                        change_kind: "set".to_string(),
                        current: Some(serde_json::json!([])),
                        desired: Some(serde_json::json!([])),
                        outcome: None,
                    },
                ],
            }],
        };

        let mut report = ApplyReport::new();
        report.succeeded.push(make_applied("ethernet", "eth0"));

        apply_outcomes(&mut diff, &report);

        for fc in &diff.operations[0].field_changes {
            assert_eq!(
                fc.outcome.as_deref(),
                Some("applied"),
                "all field changes in a succeeded entity must have outcome='applied', field={}",
                fc.field_name
            );
        }
    }

    /// AC: apply_outcomes on an empty report with an empty diff produces no panics.
    #[test]
    fn test_apply_outcomes_empty_diff_and_empty_report_no_panic() {
        let mut diff = SerializableDiff { operations: vec![] };
        let report = ApplyReport::new();
        apply_outcomes(&mut diff, &report);
        assert!(diff.operations.is_empty());
    }

    /// AC: apply_outcomes only matches operations by entity_type AND entity_name.
    /// Same name but different entity_type must not match.
    #[test]
    fn test_apply_outcomes_entity_type_is_used_in_matching() {
        let mut diff = SerializableDiff {
            operations: vec![SerializableDiffOp {
                kind: "modify".to_string(),
                entity_type: "bond".to_string(),
                entity_name: "eth0".to_string(),
                field_changes: vec![SerializableFieldChange {
                    field_name: "mtu".to_string(),
                    change_kind: "set".to_string(),
                    current: None,
                    desired: Some(serde_json::json!(9000u64)),
                    outcome: None,
                }],
            }],
        };

        // Report has "ethernet/eth0" succeeded — but diff has "bond/eth0" → no match.
        let mut report = ApplyReport::new();
        report.succeeded.push(make_applied("ethernet", "eth0"));

        apply_outcomes(&mut diff, &report);

        // bond/eth0 was not matched by ethernet/eth0 → defaults to "skipped".
        assert_eq!(
            diff.operations[0].field_changes[0].outcome.as_deref(),
            Some("skipped"),
            "diff entity with different entity_type must not match a report entry; should default to 'skipped'"
        );
    }

    // ── validate_policies tests ───────────────────────────────────────────────
    //
    // validate_policies is invoked in run_apply before any system interaction.
    // Validation errors produce exit code 2 (via Err propagated to main).

    use netfyr_state::{FieldValue, Provenance, SchemaRegistry, StateMetadata, Value};
    use netfyr_policy::{Policy, PolicySet as ValidationPolicySet};

    fn make_validation_state(entity_type: &str, name: &str, fields: Vec<(&str, Value)>) -> State {
        let mut state_fields = indexmap::IndexMap::new();
        for (k, v) in fields {
            state_fields.insert(
                k.to_string(),
                FieldValue { value: v, provenance: Provenance::KernelDefault },
            );
        }
        State {
            entity_type: entity_type.to_string(),
            selector: Selector::with_name(name),
            fields: state_fields,
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 0,
        }
    }

    fn make_validation_policy(policy_name: &str, state: State) -> Policy {
        Policy {
            name: policy_name.to_string(),
            factory_type: PolicyFactoryType::Static,
            priority: 100,
            state: Some(state),
            states: None,
            selector: None,
        }
    }

    fn make_validation_policy_set(policy: Policy) -> ValidationPolicySet {
        let mut ps = ValidationPolicySet::new();
        ps.insert(policy);
        ps
    }

    /// AC "YAML parse error returns exit code 2" — validate_policies passes for a
    /// structurally valid ethernet state (mtu within allowed range).
    #[test]
    fn test_validate_policies_valid_ethernet_state_returns_ok() {
        let state = make_validation_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]);
        let ps = make_validation_policy_set(make_validation_policy("eth0", state));
        let schema = SchemaRegistry::default();
        assert!(
            validate_policies(&ps, &schema).is_ok(),
            "valid ethernet mtu=1500 must pass schema validation"
        );
    }

    /// AC "YAML parse error / validation error returns exit code 2" — a state with
    /// an mtu below the minimum of 68 must fail validation.
    #[test]
    fn test_validate_policies_mtu_below_schema_minimum_returns_error() {
        // Ethernet schema: mtu minimum is 68, maximum is 65535.
        let state = make_validation_state("ethernet", "eth0", vec![("mtu", Value::U64(0))]);
        let ps = make_validation_policy_set(make_validation_policy("eth0-low-mtu", state));
        let schema = SchemaRegistry::default();
        assert!(
            validate_policies(&ps, &schema).is_err(),
            "mtu=0 is below the ethernet schema minimum of 68 and must fail validation"
        );
    }

    /// AC "YAML parse error / validation error returns exit code 2" — a state with
    /// an unknown field (additionalProperties: false) must fail validation and the
    /// error message must include both the policy name and the unknown field name.
    #[test]
    fn test_validate_policies_unknown_field_returns_error_with_policy_name_and_field_name() {
        let state = make_validation_state(
            "ethernet",
            "eth0",
            vec![
                ("mtu", Value::U64(1500)),
                ("completely_unknown_field_xyz", Value::U64(42)),
            ],
        );
        let ps = make_validation_policy_set(make_validation_policy("my-policy", state));
        let schema = SchemaRegistry::default();
        let result = validate_policies(&ps, &schema);
        assert!(result.is_err(), "unknown field must fail validation");
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("my-policy"),
            "error must include policy name 'my-policy'; got: {}",
            err
        );
        assert!(
            err.contains("completely_unknown_field_xyz"),
            "error must include unknown field name 'completely_unknown_field_xyz'; got: {}",
            err
        );
    }

    /// Error message format: "policy '{}': field '{}': {}" as specified in the
    /// error handling section of the spec.
    #[test]
    fn test_validate_policies_error_message_uses_policy_name_and_field_prefix_format() {
        let state = make_validation_state(
            "ethernet",
            "eth0",
            vec![("unrecognised_field", Value::String("bad".to_string()))],
        );
        let ps = make_validation_policy_set(make_validation_policy("test-policy", state));
        let schema = SchemaRegistry::default();
        let result = validate_policies(&ps, &schema);
        assert!(result.is_err(), "unknown field must fail validation");
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("policy 'test-policy'"),
            "error message must use format \"policy 'test-policy'\"; got: {}",
            err
        );
    }

    /// AC: multiple validation errors across multiple policies are all collected
    /// and reported together (not just the first error).
    #[test]
    fn test_validate_policies_multiple_invalid_states_collects_all_errors() {
        let state1 = make_validation_state(
            "ethernet",
            "eth0",
            vec![("unknown_field_one", Value::U64(1))],
        );
        let state2 = make_validation_state(
            "ethernet",
            "eth1",
            vec![("unknown_field_two", Value::U64(2))],
        );
        let mut ps = ValidationPolicySet::new();
        ps.insert(make_validation_policy("policy-one", state1));
        ps.insert(make_validation_policy("policy-two", state2));
        let schema = SchemaRegistry::default();
        let result = validate_policies(&ps, &schema);
        assert!(result.is_err(), "both invalid states must cause validation to fail");
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("policy-one") || err.contains("policy-two"),
            "error must mention at least one policy name; got: {}",
            err
        );
    }

    /// Edge: an empty policy set passes validation (nothing to validate).
    #[test]
    fn test_validate_policies_empty_policy_set_returns_ok() {
        let ps = ValidationPolicySet::new();
        let schema = SchemaRegistry::default();
        assert!(
            validate_policies(&ps, &schema).is_ok(),
            "empty policy set must pass validation"
        );
    }

    // ── display_dry_run_report smoke tests ────────────────────────────────────
    //
    // AC "Dry-run shows diff without applying" and "Dry-run with no changes needed".
    // These verify that the display function does not panic for both the empty
    // and non-empty paths — the output itself is consumed by stdout.

    use netfyr_reconcile::generate_diff;
    use std::collections::HashSet;

    fn empty_diff_report() -> DiffReport {
        let schema = SchemaRegistry::default();
        let desired = StateSet::new();
        let actual = StateSet::new();
        let managed: HashSet<EntityKey> = HashSet::new();
        let diff = generate_diff(&desired, &actual, &managed, &schema);
        DiffReport::new(diff, &desired, &actual)
    }

    /// AC "Dry-run with no changes needed" — display_dry_run_report does not panic
    /// when the diff is empty and is_empty=true.
    #[test]
    fn test_display_dry_run_report_no_panic_when_no_changes() {
        let report = empty_diff_report();
        display_dry_run_report(&report, true);
    }

    /// AC "Dry-run with no changes needed" — display_dry_run_report also does not
    /// panic when called with is_empty=false on an empty diff (edge case).
    #[test]
    fn test_display_dry_run_report_no_panic_empty_diff_is_empty_false() {
        let report = empty_diff_report();
        display_dry_run_report(&report, false);
    }

    /// AC "Dry-run shows diff without applying" — display_dry_run_report does not panic
    /// when the diff contains actual operations (Add with one field change).
    #[test]
    fn test_display_dry_run_report_no_panic_with_diff_operations() {
        use netfyr_state::{FieldValue, Provenance, StateMetadata, Value};

        let schema = SchemaRegistry::default();

        // Build a desired state with mtu=9000; leave actual state empty → Add operation.
        let mut desired = StateSet::new();
        let mut fields = indexmap::IndexMap::new();
        fields.insert(
            "mtu".to_string(),
            FieldValue { value: Value::U64(9000), provenance: Provenance::KernelDefault },
        );
        desired.insert(State {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            fields,
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 100,
        });
        let actual = StateSet::new();

        let mut managed: HashSet<EntityKey> = HashSet::new();
        managed.insert(("ethernet".to_string(), "eth0".to_string()));

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let is_empty = !diff.has_meaningful_changes();
        let report = DiffReport::new(diff, &desired, &actual);

        // Must not panic regardless of whether any changes were detected.
        display_dry_run_report(&report, is_empty);
    }
}
