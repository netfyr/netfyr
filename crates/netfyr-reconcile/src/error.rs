use std::fmt;

use netfyr_state::IndexedValidationError;
use netfyr_state::plan::InterfaceId;

use crate::ValueConflict;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconcileError {
    Validation(Vec<IndexedValidationError>),
    DuplicateContributionKeys(Vec<String>),
    IncompleteAddressInventory {
        target: Box<InterfaceId>,
    },
    UnsafeAddressGroup {
        target: Box<InterfaceId>,
        message: String,
    },
    InvalidAddress {
        target: Box<InterfaceId>,
        message: String,
    },
    ValueConflicts(Vec<ValueConflict>),
}

impl fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(_) => f.write_str("contributions failed schema validation"),
            Self::DuplicateContributionKeys(keys) => {
                write!(f, "duplicate contribution keys: {}", keys.join(", "))
            }
            Self::IncompleteAddressInventory { target } => {
                write!(f, "{}: IPv4 inventory is incomplete", target.name)
            }
            Self::UnsafeAddressGroup { target, message } => {
                write!(f, "{}: unsafe address group: {message}", target.name)
            }
            Self::InvalidAddress { target, message } => {
                write!(f, "{}: invalid IPv4 address: {message}", target.name)
            }
            Self::ValueConflicts(conflicts) => write!(f, "{} value conflict(s)", conflicts.len()),
        }
    }
}

impl std::error::Error for ReconcileError {}
