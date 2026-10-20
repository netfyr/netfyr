//! Reconciliation-specific inputs, diagnostics, provenance, and plans.

use indexmap::IndexMap;
use netfyr_state::plan::{InterfaceId, StateDiff};
use netfyr_state::{Match, Source, Value};

/// One source contribution to a policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contribution {
    pub policy: String,
    pub key: String,
    pub device_type: String,
    pub match_spec: Match,
    pub fields: IndexMap<String, Value>,
    pub source: Source,
    pub priority: i32,
}

impl Contribution {
    pub fn new(
        policy: impl Into<String>,
        key: impl Into<String>,
        source: Source,
        device_type: impl Into<String>,
        match_spec: Match,
        fields: IndexMap<String, Value>,
        priority: i32,
    ) -> Self {
        Self {
            policy: policy.into(),
            key: key.into(),
            device_type: device_type.into(),
            match_spec,
            fields,
            source,
            priority,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConflictPolicy {
    #[default]
    Resolve,
    Reject,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MergeOptions {
    pub conflict_policy: ConflictPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompetingValue {
    pub source: String,
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictIdentity {
    Field { path: String },
    Address { ip: Value },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValueConflict {
    pub target: InterfaceId,
    pub identity: ConflictIdentity,
    pub competing: Vec<CompetingValue>,
    pub winner: CompetingValue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictingPolicy {
    pub name: String,
    pub selector: Match,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingConflict {
    pub target: InterfaceId,
    pub policies: Vec<ConflictingPolicy>,
}

/// One selector that matched no device, even if another selector from the
/// same policy matched. Identical selectors within a policy are reported once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmatchedPolicy {
    pub policy: String,
    pub selector: Match,
}

/// Structured reconciliation diagnostics. These remain separate from the
/// backend's generic diagnostics, which receive a textual mirror.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Diagnostic {
    ValueConflict(ValueConflict),
    UnmatchedPolicy(UnmatchedPolicy),
    BindingConflict(BindingConflict),
}

/// Contribution-key provenance for one backend field change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeProvenance {
    pub target: InterfaceId,
    pub field: String,
    pub source: Option<String>,
    pub element_sources: Option<Vec<String>>,
}

/// Reconciliation output. The contained `StateDiff` is directly executable by
/// `NetworkBackend::{dry_run,apply}`; reconcile-specific metadata stays here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcilePlan {
    diff: StateDiff,
    pub diagnostics: Vec<Diagnostic>,
    pub provenance: Vec<ChangeProvenance>,
}

impl ReconcilePlan {
    pub(crate) fn new(
        diff: StateDiff,
        diagnostics: Vec<Diagnostic>,
        provenance: Vec<ChangeProvenance>,
    ) -> Self {
        Self {
            diff,
            diagnostics,
            provenance,
        }
    }

    pub fn diff(&self) -> &StateDiff {
        &self.diff
    }

    pub fn into_diff(self) -> StateDiff {
        self.diff
    }
}
