//! netfyr-backend: trait-based abstraction layer between the reconciliation engine
//! and kernel I/O. Provides `NetworkBackend`, report types, `BackendError`, and
//! `BackendRegistry`.
//!
//! # Design decisions
//!
//! - **Backend registry.** [`BackendRegistry`] maps entity types (e.g.
//!   `"ethernet"`) to [`NetworkBackend`] implementations. Today there is only
//!   one backend (`NetlinkBackend`), but the registry pattern allows adding
//!   backends for other entity types (bonds, VLANs, bridges) without changing
//!   the reconciliation or CLI layers.
//!
//! - **Async trait via `async-trait`.** [`NetworkBackend`] uses `#[async_trait]`
//!   because the trait needs to be object-safe for `dyn` dispatch in
//!   `BackendRegistry`. The performance cost is negligible compared to the
//!   netlink I/O it wraps.
//!
//! - **DHCP factory in backend, not policy.** The [`Dhcpv4Factory`]
//!   lives here rather than in `netfyr-policy` because it performs runtime
//!   network I/O (spawning a DHCP client, receiving packets) — fundamentally
//!   a backend concern. The policy crate defines what a DHCPv4 policy *is*;
//!   the backend crate implements how it *runs*.

pub mod dhcp;
pub mod netlink;
pub mod registry;
pub mod report;
pub mod trait_;

pub use dhcp::{interface_exists, lease_to_state, Dhcpv4Factory, DhcpLease, FactoryEvent, LeaseTimingInfo};
pub use registry::BackendRegistry;
pub use report::{
    AppliedOperation, ApplyReport, DiffOpKind, DryRunReport, FailedOperation, FieldChange,
    FieldChangeKind, PlannedChange, SkippedOperation,
};
pub use netlink::NetlinkBackend;
pub use trait_::NetworkBackend;

use netfyr_state::{EntityType, Selector};

// ── BackendError ──────────────────────────────────────────────────────────────

/// Errors produced by `NetworkBackend` implementations and the `BackendRegistry`.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// The requested entity type is not handled by this backend.
    #[error("unsupported entity type: {0}")]
    UnsupportedEntityType(EntityType),

    /// A query operation failed for the given entity type.
    #[error("query failed for entity type {entity_type}")]
    QueryFailed {
        entity_type: EntityType,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// An apply operation failed for the given operation description.
    #[error("apply failed for operation: {operation}")]
    ApplyFailed {
        operation: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The requested entity was not found.
    // Selector is boxed to keep BackendError's in-line size small (clippy::result_large_err).
    #[error("entity not found: {entity_type} {selector:?}")]
    NotFound {
        entity_type: EntityType,
        selector: Box<Selector>,
    },

    /// The backend lacks permission to perform the operation.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// An internal error occurred.
    #[error("internal error: {0}")]
    Internal(String),
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use netfyr_state::Selector;

    /// Scenario: Query for non-existent interface returns NotFound.
    /// BackendError::NotFound display must include the entity type.
    #[test]
    fn test_backend_error_not_found_display_contains_entity_type() {
        let sel = Selector::with_name("eth99");
        let err = BackendError::NotFound {
            entity_type: "ethernet".to_string(),
            selector: Box::new(sel),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("ethernet"),
            "NotFound display must contain the entity type; got: {msg}"
        );
    }

    /// BackendError::UnsupportedEntityType display must include the entity type name.
    #[test]
    fn test_backend_error_unsupported_entity_type_display_contains_name() {
        let err = BackendError::UnsupportedEntityType("wifi".to_string());
        let msg = err.to_string();
        assert!(
            msg.contains("wifi"),
            "UnsupportedEntityType display must contain the entity type name; got: {msg}"
        );
    }

    /// Scenario: If permission is denied, BackendError::PermissionDenied display
    /// must include the provided reason string.
    #[test]
    fn test_backend_error_permission_denied_display_contains_reason() {
        let err = BackendError::PermissionDenied("operation not permitted".to_string());
        let msg = err.to_string();
        assert!(
            msg.contains("operation not permitted"),
            "PermissionDenied display must contain the reason; got: {msg}"
        );
    }

    /// BackendError::Internal display must include the error detail string.
    #[test]
    fn test_backend_error_internal_display_contains_detail() {
        let err = BackendError::Internal("unexpected null handle".to_string());
        let msg = err.to_string();
        assert!(
            msg.contains("unexpected null handle"),
            "Internal display must contain the detail; got: {msg}"
        );
    }

    /// BackendError::QueryFailed display must include the entity type.
    #[test]
    fn test_backend_error_query_failed_display_contains_entity_type() {
        let err = BackendError::QueryFailed {
            entity_type: "ethernet".to_string(),
            source: Box::new(std::io::Error::new(std::io::ErrorKind::Other, "io error")),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("ethernet"),
            "QueryFailed display must contain the entity type; got: {msg}"
        );
    }

    /// BackendError::ApplyFailed display must include the operation description.
    #[test]
    fn test_backend_error_apply_failed_display_contains_operation() {
        let err = BackendError::ApplyFailed {
            operation: "modify eth0".to_string(),
            source: Box::new(std::io::Error::new(std::io::ErrorKind::Other, "io error")),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("modify eth0"),
            "ApplyFailed display must contain the operation; got: {msg}"
        );
    }

    /// BackendError variants are Debug-formattable (required by #[derive(Debug)]).
    #[test]
    fn test_backend_error_variants_are_debug_formattable() {
        let sel = Selector::with_name("eth0");
        let errors: Vec<Box<dyn std::fmt::Debug>> = vec![
            Box::new(BackendError::UnsupportedEntityType("wifi".to_string())),
            Box::new(BackendError::NotFound {
                entity_type: "ethernet".to_string(),
                selector: Box::new(sel),
            }),
            Box::new(BackendError::PermissionDenied("denied".to_string())),
            Box::new(BackendError::Internal("detail".to_string())),
        ];
        for err in &errors {
            assert!(
                !format!("{:?}", err).is_empty(),
                "Debug output must be non-empty for {:?}",
                err
            );
        }
    }
}
