//! Library target for `netfyr-cli`.
//!
//! Exposes [`Cli`] and [`Commands`] so that tooling (e.g. the `xtask` crate)
//! can call `Cli::command()` via clap's `CommandFactory` trait to generate
//! documentation artifacts such as man pages.

pub mod apply;
pub mod history;
pub mod query;
pub mod revert;

pub use apply::run_apply;
pub use history::run_history;
pub use query::run_query;
pub use revert::run_revert;

use clap::{Parser, Subcommand};

/// Unix socket path for the netfyr daemon's Varlink API.
/// Override with `NETFYR_SOCKET_PATH` environment variable (used in tests and
/// non-systemd deployments that place the socket at a custom path).
pub(crate) fn daemon_socket_path() -> String {
    std::env::var("NETFYR_SOCKET_PATH")
        .unwrap_or_else(|_| "/run/netfyr/netfyr.sock".to_string())
}

/// Declarative Linux network configuration.
///
/// netfyr manages network interfaces through declarative YAML policy files.
/// Policies are reconciled into an effective desired state that is applied to
/// the kernel via netlink.
///
/// Two operational modes are supported and detected automatically.
/// In standalone mode the netfyr daemon is not running and static policies
/// are applied directly using the netlink backend.
/// In daemon mode policies are submitted to the daemon via Varlink; the daemon
/// reconciles and applies them, including support for dynamic factories such
/// as DHCPv4.
///
/// Subcommands:
///
///   apply    Load and apply policy files to the system.
///
///   query    Query current network state from the kernel or daemon.
///
///   history  Show journal history of state changes.
#[derive(Parser)]
#[command(name = "netfyr", about = "Declarative Linux network configuration")]
#[command(subcommand_required = true, arg_required_else_help = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Apply network policies to the system
    ///
    /// Load policy definitions from YAML files or directories, reconcile them
    /// into an effective desired state, query the current system state, generate
    /// a diff, and apply the changes.
    ///
    /// If the netfyr daemon is running, policies are submitted to the daemon
    /// via Varlink. Otherwise, static policies are applied directly.
    ///
    /// If --dry-run is given, show what would change without applying.
    Apply(apply::ApplyArgs),

    /// Query current system network state
    ///
    /// Query the current network state for all supported entity types. If the
    /// netfyr daemon is running, the query is forwarded to the daemon via
    /// Varlink. Otherwise, the kernel is queried directly via netlink.
    ///
    /// Use --selector (-s) to filter results by entity type, interface name,
    /// driver, MAC address, or PCI path. Multiple selectors are combined with
    /// AND logic. Use --output (-o) to select yaml (default) or json output.
    Query(query::QueryArgs),

    /// Show journal history of state changes
    ///
    /// Display a log of reconciliation events recorded by the journal.
    /// Shows what changed, when, and why. Supports filtering by time,
    /// trigger type, and entity name.
    ///
    /// If the netfyr daemon is running, history is retrieved via Varlink.
    /// Otherwise, journal files are read directly.
    History(history::HistoryArgs),

    /// Revert system state to match a journal snapshot
    ///
    /// Reads the target entry's state_after snapshot, computes the diff from
    /// the current system state to the target state, and applies it.
    ///
    /// If the netfyr daemon is running, the revert is executed via Varlink.
    /// Otherwise, changes are applied directly.
    ///
    /// Use --dry-run to preview changes without applying them.
    Revert(revert::RevertArgs),
}
