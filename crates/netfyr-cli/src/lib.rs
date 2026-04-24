//! Library target for `netfyr-cli`.
//!
//! Exposes [`Cli`] and [`Commands`] so that tooling (e.g. the `xtask` crate)
//! can call `Cli::command()` via clap's `CommandFactory` trait to generate
//! documentation artifacts such as man pages.

pub mod apply;
pub mod completions;
pub mod history;
pub mod query;
pub mod revert;

pub use apply::run_apply;
pub use completions::run_completions;
pub use history::run_history;
pub use query::run_query;
pub use revert::run_revert;

use clap::{Parser, Subcommand, ValueEnum};

/// Unix socket path for the netfyr daemon's Varlink API.
/// Override with `NETFYR_SOCKET_PATH` environment variable (used in tests and
/// non-systemd deployments that place the socket at a custom path).
pub(crate) fn daemon_socket_path() -> String {
    std::env::var("NETFYR_SOCKET_PATH")
        .unwrap_or_else(|_| "/run/netfyr/netfyr.sock".to_string())
}

/// Controls whether terminal output uses ANSI color codes.
#[derive(Clone, ValueEnum)]
pub enum ColorMode {
    /// Enable colors when stdout is a TTY (default).
    Auto,
    /// Always enable colors, even when piped.
    Always,
    /// Disable colors.
    Never,
}

/// Resolve the requested color mode and configure the `colored` crate accordingly.
///
/// `NO_COLOR` (https://no-color.org/) always wins — if set, colors are disabled
/// regardless of `--color`.  For `Auto`, colored's built-in TTY detection is
/// used without calling `set_override`.
pub fn resolve_color_mode(mode: &ColorMode) {
    if std::env::var_os("NO_COLOR").is_some() {
        colored::control::set_override(false);
        return;
    }
    match mode {
        ColorMode::Always => colored::control::set_override(true),
        ColorMode::Never => colored::control::set_override(false),
        ColorMode::Auto => {}
    }
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
    /// Control color output: auto (default), always, never.
    #[arg(long, global = true, default_value = "auto")]
    pub color: ColorMode,

    #[command(subcommand)]
    pub command: Commands,
}

#[cfg(test)]
mod tests {
    use super::{resolve_color_mode, ColorMode};

    /// AC: Color mode "never" must not panic (smoke test for global override).
    #[test]
    fn test_resolve_color_mode_never_does_not_panic() {
        resolve_color_mode(&ColorMode::Never);
        // Undo the global override so this test does not affect others.
        colored::control::unset_override();
    }

    /// AC: Color mode "always" must not panic.
    #[test]
    fn test_resolve_color_mode_always_does_not_panic() {
        resolve_color_mode(&ColorMode::Always);
        colored::control::unset_override();
    }

    /// AC: Color mode "auto" must not panic (uses TTY auto-detection, no override).
    #[test]
    fn test_resolve_color_mode_auto_does_not_panic() {
        resolve_color_mode(&ColorMode::Auto);
        // Auto does not set an override, so nothing to undo.
    }

    /// AC: "If the NO_COLOR environment variable is set (any value), colors are
    /// disabled regardless of the --color flag."
    /// Even with --color=always, NO_COLOR must force colors off.
    #[test]
    fn test_resolve_color_mode_no_color_env_var_disables_colors_even_with_always_flag() {
        // Ensure NO_COLOR is absent initially.
        // SAFETY: single-threaded env manipulation guarded by test isolation.
        unsafe { std::env::remove_var("NO_COLOR") };
        colored::control::set_override(true); // start with colors explicitly on

        // Set NO_COLOR to any value.
        unsafe { std::env::set_var("NO_COLOR", "1") };
        resolve_color_mode(&ColorMode::Always);

        // After resolve_color_mode with NO_COLOR set, colored string must NOT
        // contain ANSI escape sequences.
        use colored::Colorize;
        let colored_output = "test".red().to_string();
        assert_eq!(
            colored_output, "test",
            "NO_COLOR env var must disable colors even when --color=always; \
             got: {:?}",
            colored_output
        );

        // Cleanup.
        unsafe { std::env::remove_var("NO_COLOR") };
        colored::control::unset_override();
    }

    /// AC: --color=never disables colors (colored string has no ANSI codes).
    #[test]
    fn test_resolve_color_mode_never_disables_colored_output() {
        // Ensure NO_COLOR is absent so it does not interfere.
        unsafe { std::env::remove_var("NO_COLOR") };
        // Start with colors explicitly on so the test is meaningful.
        colored::control::set_override(true);

        resolve_color_mode(&ColorMode::Never);

        use colored::Colorize;
        let colored_output = "test".red().to_string();
        assert_eq!(
            colored_output, "test",
            "--color=never must disable ANSI codes; got: {:?}",
            colored_output
        );

        // Cleanup.
        colored::control::unset_override();
    }

    /// AC: --color=always enables colors (colored string has ANSI codes).
    #[test]
    fn test_resolve_color_mode_always_enables_colored_output() {
        // Ensure NO_COLOR is absent so it does not interfere.
        unsafe { std::env::remove_var("NO_COLOR") };
        // Start with colors explicitly off so the test is meaningful.
        colored::control::set_override(false);

        resolve_color_mode(&ColorMode::Always);

        use colored::Colorize;
        let colored_output = "test".red().to_string();
        assert_ne!(
            colored_output, "test",
            "--color=always must produce ANSI-coded output; got: {:?}",
            colored_output
        );

        // Cleanup.
        colored::control::unset_override();
    }

    // ── Clap CLI parsing / structural tests ───────────────────────────────────

    use clap::Parser;
    use super::Cli;

    /// AC "No subcommand shows usage help and exit code 2":
    /// Running `netfyr` with no arguments must fail with a clap error so that
    /// the binary exits with code 2. With `subcommand_required = true,
    /// arg_required_else_help = true`, clap signals this via an error.
    #[test]
    fn test_cli_no_subcommand_produces_clap_error() {
        let result = Cli::try_parse_from(["netfyr"]);
        assert!(
            result.is_err(),
            "invoking `netfyr` with no subcommand must fail; \
             with subcommand_required=true clap returns an error so the binary exits 2"
        );
    }

    /// AC "No path arguments shows error and exit code 2":
    /// Running `netfyr apply` with no paths must fail because `paths` has
    /// `required = true` on the `ApplyArgs` field.
    #[test]
    fn test_cli_apply_no_paths_produces_clap_error() {
        let result = Cli::try_parse_from(["netfyr", "apply"]);
        assert!(
            result.is_err(),
            "invoking `netfyr apply` without any path arguments must fail; \
             ApplyArgs.paths has required=true so clap returns an error (exit 2)"
        );
    }

    /// AC "--color is a global flag": it must be accepted before the subcommand.
    #[test]
    fn test_cli_color_flag_before_subcommand_is_accepted() {
        let result = Cli::try_parse_from(["netfyr", "--color", "never", "apply", "/tmp/dummy.yaml"]);
        assert!(
            result.is_ok(),
            "`--color never` before the subcommand must be accepted as a global flag; \
             got: {:?}",
            result.err()
        );
        let cli = result.unwrap();
        assert!(
            matches!(cli.color, ColorMode::Never),
            "`--color never` must parse to ColorMode::Never"
        );
    }

    /// AC "--color is a global flag": it must also be accepted after the subcommand name.
    #[test]
    fn test_cli_color_flag_after_subcommand_is_accepted() {
        let result = Cli::try_parse_from(["netfyr", "apply", "--color", "always", "/tmp/dummy.yaml"]);
        assert!(
            result.is_ok(),
            "`--color always` after the subcommand must be accepted as a global flag; \
             got: {:?}",
            result.err()
        );
        let cli = result.unwrap();
        assert!(
            matches!(cli.color, ColorMode::Always),
            "`--color always` after subcommand must parse to ColorMode::Always"
        );
    }

    /// AC "--color defaults to auto": omitting `--color` must default to `Auto`.
    #[test]
    fn test_cli_color_flag_default_is_auto() {
        let result = Cli::try_parse_from(["netfyr", "apply", "/tmp/dummy.yaml"]);
        assert!(
            result.is_ok(),
            "parsing `netfyr apply /tmp/dummy.yaml` without --color must succeed; \
             got: {:?}",
            result.err()
        );
        let cli = result.unwrap();
        assert!(
            matches!(cli.color, ColorMode::Auto),
            "omitting --color must default to ColorMode::Auto"
        );
    }

    /// AC "--color rejects invalid values": unrecognized values must be rejected.
    #[test]
    fn test_cli_color_flag_invalid_value_is_rejected() {
        let result = Cli::try_parse_from(["netfyr", "--color", "rainbow", "apply", "/tmp/dummy.yaml"]);
        assert!(
            result.is_err(),
            "`--color rainbow` must be rejected as an invalid ColorMode value"
        );
    }
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

    /// Generate shell completion scripts
    ///
    /// Print a completion script for the specified shell to stdout.
    /// Redirect the output to the appropriate file for your shell.
    ///
    /// Example (bash):
    ///   netfyr completions bash > ~/.local/share/bash-completion/completions/netfyr
    Completions(completions::CompletionsArgs),
}
