//! netfyr-journal crate — history journal for recording, querying, and reverting
//! network state changes.
//!
//! Every apply operation (whether from the CLI, daemon reconciliation, DHCP
//! lease event, or external change detection) is recorded as a [`JournalEntry`]
//! in an append-only NDJSON file (`current.ndjson`). Each entry captures the
//! trigger, active policies, a field-level diff, and a full state-after
//! snapshot.
//!
//! The journal supports:
//!
//! - **Recording**: [`Journal::append`] assigns a monotonically increasing
//!   sequence number and persists the entry with file-level locking.
//! - **Querying**: [`Journal::read_recent`] and [`Journal::read_entry`]
//!   retrieve entries for display in `netfyr history`.
//! - **Reverting**: [`Journal::read_entry`] provides the `state_after` snapshot
//!   that `netfyr revert` uses to reconstruct a target state.
//! - **Rotation**: When entry count or file size exceeds configured thresholds,
//!   the current file is gzip-compressed into an `archive/` directory. Archives
//!   older than the retention period (default 90 days) are automatically
//!   deleted.
//!
//! The [`serializable`] module provides JSON-friendly representations of
//! [`StateSet`](netfyr_state::StateSet) and diff operations, decoupled from
//! the internal domain types so the on-disk format can evolve independently.

pub mod entry;
pub mod journal;
pub mod serializable;

pub use entry::{
    summarize_policies, ApplyOutcome, JournalEntry, PolicySummary, SequenceId, Trigger,
};
pub use journal::{Journal, JournalError};
pub use serializable::{
    SerializableDiff, SerializableDiffOp, SerializableFieldChange, SerializableState,
    SerializableStateSet,
};
