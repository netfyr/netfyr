//! Observe a Linux network namespace and execute explicit, ordered operations.

mod executor;
mod linux;
mod registry;
mod report;
mod worker;

use std::future::Future;
use std::pin::Pin;

use netfyr_state::Match;
use netfyr_state::plan::{NamespaceId, Observation, OperationKind, StateDiff};

pub use registry::BackendRegistry;
pub use report::{
    ApplyReport, BackendError, BackendErrorKind, DryRunReport, FailedOperation, OperationContext,
    PlannedOperation, PlannedStatus, SkipReason, SkippedOperation,
};
pub use worker::NetlinkBackend;

pub const ETHERNET: &str = "ethernet";

pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait NetworkBackend: Send + Sync {
    /// The network namespace every query and write of this backend uses.
    fn namespace(&self) -> NamespaceId;

    /// Device types this backend may modify.
    fn supported_entities(&self) -> &'static [&'static str];

    fn supports_operation(&self, kind: &OperationKind) -> bool {
        !matches!(kind, OperationKind::AddEntity | OperationKind::RemoveEntity)
    }

    /// No matches returns `NotFound` for name selectors, otherwise an empty observation.
    fn query<'a>(
        &'a self,
        selector: &'a Match,
    ) -> BackendFuture<'a, Result<Observation, BackendError>>;

    fn query_all(&self) -> BackendFuture<'_, Result<Observation, BackendError>> {
        Box::pin(async move { self.query(&Match::default()).await })
    }

    /// Execute the plan in order and report every operation. Dropping the
    /// future stops before the next write; a write already submitted can still
    /// complete without being reported.
    fn apply<'a>(&'a self, diff: &'a StateDiff) -> BackendFuture<'a, ApplyReport>;

    /// Validate against projected state without writing.
    fn dry_run<'a>(&'a self, diff: &'a StateDiff) -> BackendFuture<'a, DryRunReport>;
}
