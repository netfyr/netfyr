use std::fmt;
use std::io;

use netfyr_state::plan::{Diagnostic, FieldChange, InterfaceId, Operation, OperationKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendErrorKind {
    UnsupportedEntityType,
    UnsupportedOperation,
    QueryFailed,
    /// The kernel rejected the write.
    ApplyFailed,
    /// The write was sent but its result could not be confirmed; it may have
    /// taken effect. Query before retrying or compensating.
    OutcomeUnknown,
    NotFound,
    PermissionDenied,
    StaleState,
    /// A backend already owns the entity type in that namespace.
    Conflict,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationContext {
    pub index: usize,
    pub target: InterfaceId,
    pub kind: OperationKind,
}

impl OperationContext {
    pub fn new(index: usize, operation: &Operation) -> Self {
        Self {
            index,
            target: operation.target.clone(),
            kind: operation.kind.clone(),
        }
    }
}

/// Kernel errno and extended ACK details accompany failures when available.
/// Errors outside report entries may carry their own `context`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendError {
    pub kind: BackendErrorKind,
    pub message: String,
    pub errno: Option<i32>,
    pub extack: Option<String>,
    pub context: Option<Box<OperationContext>>,
}

impl BackendError {
    pub fn new(kind: BackendErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            errno: None,
            extack: None,
            context: None,
        }
    }

    pub(crate) fn from_io(fallback: BackendErrorKind, error: io::Error) -> Self {
        let errno = error.raw_os_error();
        let kind = match errno {
            Some(libc::EPERM | libc::EACCES) => BackendErrorKind::PermissionDenied,
            Some(libc::ENODEV | libc::ENXIO | libc::ENOENT) => BackendErrorKind::NotFound,
            _ => fallback,
        };
        let mut result = Self::new(kind, error.to_string());
        result.errno = errno;
        result
    }

    pub(crate) fn at(mut self, context: &OperationContext) -> Self {
        self.context = Some(Box::new(context.clone()));
        self
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(context) = &self.context {
            write!(
                f,
                "operation {} ({} on {}): ",
                context.index,
                context.kind.kind_name(),
                context.target.name
            )?;
        }
        write!(f, "{:?}: {}", self.kind, self.message)?;
        if let Some(errno) = self.errno {
            write!(f, " (errno {errno})")?;
        }
        if let Some(extack) = &self.extack {
            write!(f, ": {extack}")?;
        }
        Ok(())
    }
}

impl std::error::Error for BackendError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkipReason {
    AlreadySatisfied,
    ReadOnly,
    /// The listed earlier operations failed or were themselves blocked.
    DependencyFailed {
        operations: Vec<usize>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedOperation {
    pub context: OperationContext,
    pub error: BackendError,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedOperation {
    pub context: OperationContext,
    pub reason: SkipReason,
}

#[derive(Clone, Debug, Default)]
pub struct ApplyReport {
    pub succeeded: Vec<OperationContext>,
    pub failed: Vec<FailedOperation>,
    pub skipped: Vec<SkippedOperation>,
    /// Observation issues and kernel side effects.
    pub diagnostics: Vec<Diagnostic>,
}

impl ApplyReport {
    /// True when nothing failed or was blocked by a failed dependency.
    pub fn is_success(&self) -> bool {
        no_failures(
            &self.failed,
            self.skipped.iter().map(|entry| Some(&entry.reason)),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlannedStatus {
    Write,
    Skip(SkipReason),
    /// Validation failed; the matching entry in `DryRunReport::failed` says why.
    Fail,
}

#[derive(Clone, Debug)]
pub struct PlannedOperation {
    pub context: OperationContext,
    pub status: PlannedStatus,
}

#[derive(Clone, Debug, Default)]
pub struct DryRunReport {
    pub changes: Vec<FieldChange>,
    /// Every operation in plan order.
    pub operations: Vec<PlannedOperation>,
    pub diagnostics: Vec<Diagnostic>,
    pub failed: Vec<FailedOperation>,
}

impl DryRunReport {
    /// True when nothing would fail or be blocked by a failed dependency.
    pub fn is_success(&self) -> bool {
        no_failures(
            &self.failed,
            self.operations.iter().map(|entry| match &entry.status {
                PlannedStatus::Skip(reason) => Some(reason),
                _ => None,
            }),
        )
    }
}

fn no_failures<'a>(
    failed: &[FailedOperation],
    mut skips: impl Iterator<Item = Option<&'a SkipReason>>,
) -> bool {
    failed.is_empty()
        && !skips.any(|skip| matches!(skip, Some(SkipReason::DependencyFailed { .. })))
}
