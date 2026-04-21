//! netfyr-varlink crate — Varlink IPC API for the netfyr daemon.
//!
//! Implements the `io.netfyr` Varlink interface (defined in `io.netfyr.varlink`)
//! using a manual JSON-over-Unix-socket protocol on top of `tokio`. Wire types
//! and domain-type conversions live in [`types`]; the async client is in [`client`].

pub mod client;
pub mod types;

pub use client::{VarlinkClient, VarlinkError};
pub use types::{
    VarlinkApplyReport, VarlinkChangeEntry, VarlinkConflictEntry, VarlinkDaemonStatus,
    VarlinkDiffOperation, VarlinkFactoryStatus, VarlinkFieldChange, VarlinkPolicy,
    VarlinkSelector, VarlinkState, VarlinkStateDef, VarlinkStateDiff,
    convert_apply_report_with_conflicts, json_to_state_fields, json_to_value,
    state_fields_to_json, value_to_json,
};

/// Default path for the daemon's Varlink Unix socket.
///
/// Created by the daemon on startup (or by systemd socket activation).
/// The CLI uses this path for auto-detection: if `VarlinkClient::connect`
/// succeeds at this path, daemon mode is used.
pub const DEFAULT_SOCKET_PATH: &str = "/run/netfyr/netfyr.sock";

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Scenario: Interface definition file is valid ───────────────────────────

    /// Scenario: Interface definition file is valid — file exists at expected path.
    #[test]
    fn test_varlink_interface_file_exists() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let varlink_file = std::path::Path::new(manifest_dir)
            .join("src")
            .join("io.netfyr.varlink");
        assert!(
            varlink_file.exists(),
            "Varlink interface file must exist at src/io.netfyr.varlink, checked: {}",
            varlink_file.display()
        );
    }

    /// Scenario: Interface definition file defines exactly 4 methods.
    #[test]
    fn test_varlink_interface_file_defines_four_methods() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let varlink_file = std::path::Path::new(manifest_dir)
            .join("src")
            .join("io.netfyr.varlink");
        let content = std::fs::read_to_string(&varlink_file)
            .expect("should be able to read io.netfyr.varlink");

        let method_count = content
            .lines()
            .filter(|line| line.trim_start().starts_with("method "))
            .count();

        assert_eq!(
            method_count, 7,
            "Interface must define exactly 7 methods (SubmitPolicies, Query, DryRun, GetStatus, GetHistory, GetJournalEntry, Revert), found {method_count}"
        );
    }

    /// Scenario: Interface defines SubmitPolicies method.
    #[test]
    fn test_varlink_interface_defines_submit_policies_method() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let content = std::fs::read_to_string(
            std::path::Path::new(manifest_dir).join("src").join("io.netfyr.varlink"),
        )
        .unwrap();
        assert!(
            content.contains("method SubmitPolicies"),
            "Interface must define SubmitPolicies method"
        );
    }

    /// Scenario: Interface defines Query method.
    #[test]
    fn test_varlink_interface_defines_query_method() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let content = std::fs::read_to_string(
            std::path::Path::new(manifest_dir).join("src").join("io.netfyr.varlink"),
        )
        .unwrap();
        assert!(content.contains("method Query"), "Interface must define Query method");
    }

    /// Scenario: Interface defines DryRun method.
    #[test]
    fn test_varlink_interface_defines_dry_run_method() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let content = std::fs::read_to_string(
            std::path::Path::new(manifest_dir).join("src").join("io.netfyr.varlink"),
        )
        .unwrap();
        assert!(content.contains("method DryRun"), "Interface must define DryRun method");
    }

    /// Scenario: Interface defines GetStatus method.
    #[test]
    fn test_varlink_interface_defines_get_status_method() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let content = std::fs::read_to_string(
            std::path::Path::new(manifest_dir).join("src").join("io.netfyr.varlink"),
        )
        .unwrap();
        assert!(content.contains("method GetStatus"), "Interface must define GetStatus method");
    }

    /// DEFAULT_SOCKET_PATH constant is set to the expected daemon socket path.
    #[test]
    fn test_default_socket_path_is_run_netfyr_sock() {
        assert_eq!(
            DEFAULT_SOCKET_PATH,
            "/run/netfyr/netfyr.sock",
            "DEFAULT_SOCKET_PATH must point to /run/netfyr/netfyr.sock"
        );
    }

    /// Interface file declares the io.netfyr interface name at the top.
    #[test]
    fn test_varlink_interface_file_declares_correct_interface_name() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let content = std::fs::read_to_string(
            std::path::Path::new(manifest_dir).join("src").join("io.netfyr.varlink"),
        )
        .unwrap();
        assert!(
            content.contains("interface io.netfyr"),
            "Interface file must start with 'interface io.netfyr'"
        );
    }

    /// Interface file defines the three required error types.
    #[test]
    fn test_varlink_interface_file_defines_required_errors() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let content = std::fs::read_to_string(
            std::path::Path::new(manifest_dir).join("src").join("io.netfyr.varlink"),
        )
        .unwrap();
        assert!(content.contains("error InvalidPolicy"), "Interface must define InvalidPolicy error");
        assert!(content.contains("error BackendError"), "Interface must define BackendError error");
        assert!(content.contains("error InternalError"), "Interface must define InternalError error");
    }
}
