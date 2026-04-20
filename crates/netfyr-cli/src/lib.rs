//! Library target for `netfyr-cli`.
//!
//! Exposes [`Cli`] and [`Commands`] so that tooling (e.g. the `xtask` crate)
//! can call `Cli::command()` via clap's `CommandFactory` trait to generate
//! documentation artifacts such as man pages.

pub mod apply;
pub mod query;

pub use apply::run_apply;
pub use query::run_query;

use clap::{Parser, Subcommand};

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
///   apply   Load and apply policy files to the system.
///
///   query   Query current network state from the kernel or daemon.
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
}
