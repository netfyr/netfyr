//! Pure reconciliation from policy contributions to backend-executable plans.

mod address;
mod bind;
mod diff;
mod error;
mod merge;
mod render;
mod types;

pub use error::ReconcileError;
pub use render::{render_json, render_text};
pub use types::*;

use std::collections::{BTreeMap, BTreeSet};
use std::time::SystemTime;

use netfyr_state::plan::{Diagnostic as BackendDiagnostic, InterfaceId, Observation, StateDiff};
use netfyr_state::{IndexedValidationError, SchemaRegistry, State, StateMetadata};
use uuid::Uuid;

/// Reconcile policy contributions against an observation. Every executable
/// target is copied from the observation, including namespace, index, and
/// generation; callers pass `plan.diff()` directly to the backend.
pub fn reconcile(
    contributions: &[Contribution],
    observation: &Observation,
    registry: &SchemaRegistry,
    options: &MergeOptions,
) -> Result<ReconcilePlan, ReconcileError> {
    let mut seen = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    for contribution in contributions {
        if !seen.insert(contribution.key.as_str()) {
            duplicates.insert(contribution.key.clone());
        }
    }
    if !duplicates.is_empty() {
        return Err(ReconcileError::DuplicateContributionKeys(
            duplicates.into_iter().collect(),
        ));
    }

    let mut validation_errors = Vec::new();
    for (state_index, contribution) in contributions.iter().enumerate() {
        for error in registry.validate_writable(&scratch_state(contribution)) {
            validation_errors.push(IndexedValidationError { state_index, error });
        }
    }
    if !validation_errors.is_empty() {
        return Err(ReconcileError::Validation(validation_errors));
    }

    let binding = bind::bind(contributions, observation);
    if !binding.conflicts.is_empty() {
        return Err(ReconcileError::BindingConflicts(binding.conflicts));
    }

    let mut changes = Vec::new();
    let mut operations = Vec::new();
    let mut provenance = Vec::new();
    let mut value_conflicts = Vec::new();
    for device in &binding.devices {
        let merged = merge::merge(
            &device.observation.identity,
            &device.contributions,
            &device.observation.state.fields,
        );
        value_conflicts.extend(merged.conflicts);
        let source_by_key: BTreeMap<_, _> = device
            .contributions
            .iter()
            .map(|contribution| (contribution.key.as_str(), contribution.source.clone()))
            .collect();
        let output = diff::diff_device(
            device.observation,
            &merged.fields,
            &merged.leaf_sources,
            &merged.list_elements,
            &source_by_key,
            registry,
        )?;
        let base = operations.len();
        changes.extend(output.changes);
        provenance.extend(output.provenance);
        for mut operation in output.operations {
            operation
                .depends_on
                .iter_mut()
                .for_each(|dependency| *dependency += base);
            operations.push(operation);
        }
    }

    value_conflicts.sort_by_key(|conflict| {
        (
            conflict.target.namespace.device,
            conflict.target.namespace.inode,
            conflict.target.index,
            format!("{:?}", conflict.identity),
        )
    });
    if options.conflict_policy == ConflictPolicy::Reject && !value_conflicts.is_empty() {
        return Err(ReconcileError::ValueConflicts(value_conflicts));
    }
    let mut diagnostics: Vec<_> = value_conflicts
        .into_iter()
        .map(Diagnostic::ValueConflict)
        .collect();
    diagnostics.extend(
        binding
            .unmatched
            .into_iter()
            .map(Diagnostic::UnmatchedPolicy),
    );
    let mut diff = StateDiff {
        changes,
        operations,
        diagnostics: Vec::new(),
    };
    diff.diagnostics = diagnostics
        .iter()
        .map(|diagnostic| BackendDiagnostic {
            target: diagnostic_target(diagnostic),
            field: diagnostic_field(diagnostic),
            message: render::diagnostic_text(diagnostic),
        })
        .collect();
    Ok(ReconcilePlan::new(diff, diagnostics, provenance))
}

fn scratch_state(contribution: &Contribution) -> State {
    State {
        device_type: contribution.device_type.clone(),
        match_spec: contribution.match_spec.clone(),
        fields: contribution.fields.clone(),
        source: contribution.source.clone(),
        priority: contribution.priority,
        metadata: StateMetadata {
            id: Uuid::nil(),
            created_at: SystemTime::UNIX_EPOCH,
        },
    }
}

fn diagnostic_target(diagnostic: &Diagnostic) -> Option<InterfaceId> {
    match diagnostic {
        Diagnostic::ValueConflict(conflict) => Some(conflict.target.clone()),
        Diagnostic::BindingConflict(conflict) => Some(conflict.target.clone()),
        Diagnostic::UnmatchedPolicy(_) => None,
    }
}

fn diagnostic_field(diagnostic: &Diagnostic) -> Option<String> {
    match diagnostic {
        Diagnostic::ValueConflict(ValueConflict {
            identity: ConflictIdentity::Field { path },
            ..
        }) => Some(path.clone()),
        _ => None,
    }
}
