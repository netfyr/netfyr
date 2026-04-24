use clap::CommandFactory;
use clap_complete::Shell;
use std::process::ExitCode;

use crate::Cli;

#[derive(clap::Args)]
pub struct CompletionsArgs {
    /// Shell to generate completions for
    #[arg(value_enum)]
    pub shell: Shell,
}

pub fn run_completions(args: CompletionsArgs) -> ExitCode {
    let mut cmd = Cli::command();
    clap_complete::generate(args.shell, &mut cmd, "netfyr", &mut std::io::stdout());
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use clap::Parser;

    fn generate_bash_completion() -> String {
        let mut cmd = Cli::command();
        let mut buf = Vec::new();
        clap_complete::generate(Shell::Bash, &mut cmd, "netfyr", &mut buf);
        String::from_utf8(buf).expect("completion output must be valid UTF-8")
    }

    /// AC: exit code is 0 when `netfyr completions bash` is run.
    /// Verified by confirming run_completions does not panic and returns normally.
    #[test]
    fn test_generate_bash_completion_exit_code_success() {
        let args = CompletionsArgs { shell: Shell::Bash };
        // run_completions writes to stdout; that's acceptable in unit tests.
        run_completions(args);
        // If we reach here, the function returned without panicking — SUCCESS.
    }

    /// AC: output is non-empty.
    #[test]
    fn test_generate_bash_completion_output_is_non_empty() {
        let output = generate_bash_completion();
        assert!(!output.is_empty(), "bash completion script must not be empty");
    }

    /// AC: output contains "netfyr" (the command name).
    #[test]
    fn test_generate_bash_completion_contains_command_name_netfyr() {
        let output = generate_bash_completion();
        assert!(
            output.contains("netfyr"),
            "completion script must contain the command name 'netfyr'"
        );
    }

    /// AC: output is a bash script (contains bash-specific completion syntax).
    #[test]
    fn test_generate_bash_completion_output_is_a_bash_script() {
        let output = generate_bash_completion();
        assert!(
            output.contains("complete") || output.contains("#!/"),
            "output must be a bash completion script containing 'complete' or a shebang"
        );
    }

    // ── Subcommand presence ───────────────────────────────────────────────────

    /// AC: completion script contains "apply" subcommand.
    #[test]
    fn test_generate_bash_completion_contains_apply_subcommand() {
        let output = generate_bash_completion();
        assert!(
            output.contains("apply"),
            "completion script must contain the 'apply' subcommand"
        );
    }

    /// AC: completion script contains "query" subcommand.
    #[test]
    fn test_generate_bash_completion_contains_query_subcommand() {
        let output = generate_bash_completion();
        assert!(
            output.contains("query"),
            "completion script must contain the 'query' subcommand"
        );
    }

    /// AC: completion script contains "history" subcommand.
    #[test]
    fn test_generate_bash_completion_contains_history_subcommand() {
        let output = generate_bash_completion();
        assert!(
            output.contains("history"),
            "completion script must contain the 'history' subcommand"
        );
    }

    /// AC: completion script contains "revert" subcommand.
    #[test]
    fn test_generate_bash_completion_contains_revert_subcommand() {
        let output = generate_bash_completion();
        assert!(
            output.contains("revert"),
            "completion script must contain the 'revert' subcommand"
        );
    }

    /// AC: completion script contains "completions" subcommand.
    #[test]
    fn test_generate_bash_completion_contains_completions_subcommand() {
        let output = generate_bash_completion();
        assert!(
            output.contains("completions"),
            "completion script must contain the 'completions' subcommand"
        );
    }

    // ── Global flag presence ──────────────────────────────────────────────────

    /// AC: completion script contains global flag "--color".
    #[test]
    fn test_generate_bash_completion_contains_color_global_flag() {
        let output = generate_bash_completion();
        assert!(
            output.contains("--color"),
            "completion script must contain the '--color' global flag"
        );
    }

    // ── Subcommand flag presence ──────────────────────────────────────────────

    /// AC: completion script contains "--selector" flag.
    #[test]
    fn test_generate_bash_completion_contains_selector_flag() {
        let output = generate_bash_completion();
        assert!(
            output.contains("--selector"),
            "completion script must contain the '--selector' flag"
        );
    }

    /// AC: completion script contains "--output" flag.
    #[test]
    fn test_generate_bash_completion_contains_output_flag() {
        let output = generate_bash_completion();
        assert!(
            output.contains("--output"),
            "completion script must contain the '--output' flag"
        );
    }

    /// AC: completion script contains "--dry-run" flag.
    #[test]
    fn test_generate_bash_completion_contains_dry_run_flag() {
        let output = generate_bash_completion();
        assert!(
            output.contains("--dry-run"),
            "completion script must contain the '--dry-run' flag"
        );
    }

    /// AC: completion script contains "--trigger" flag.
    #[test]
    fn test_generate_bash_completion_contains_trigger_flag() {
        let output = generate_bash_completion();
        assert!(
            output.contains("--trigger"),
            "completion script must contain the '--trigger' flag"
        );
    }

    /// AC: completion script contains "--since" flag.
    #[test]
    fn test_generate_bash_completion_contains_since_flag() {
        let output = generate_bash_completion();
        assert!(
            output.contains("--since"),
            "completion script must contain the '--since' flag"
        );
    }

    /// AC: completion script contains "--show" flag.
    #[test]
    fn test_generate_bash_completion_contains_show_flag() {
        let output = generate_bash_completion();
        assert!(
            output.contains("--show"),
            "completion script must contain the '--show' flag"
        );
    }

    /// AC: completion script contains "--count" flag.
    #[test]
    fn test_generate_bash_completion_contains_count_flag() {
        let output = generate_bash_completion();
        assert!(
            output.contains("--count"),
            "completion script must contain the '--count' flag"
        );
    }

    /// AC: completion script contains "--absolute-timestamps" flag.
    #[test]
    fn test_generate_bash_completion_contains_absolute_timestamps_flag() {
        let output = generate_bash_completion();
        assert!(
            output.contains("--absolute-timestamps"),
            "completion script must contain the '--absolute-timestamps' flag"
        );
    }

    // ── Enum value presence (for interactive Tab completion) ──────────────────

    /// AC (interactive): "auto", "always", "never" offered for --color.
    /// clap_complete embeds ValueEnum variants in the script for Tab completion.
    #[test]
    fn test_generate_bash_completion_contains_color_enum_values() {
        let output = generate_bash_completion();
        assert!(
            output.contains("auto"),
            "completion script must include 'auto' as a --color value"
        );
        assert!(
            output.contains("always"),
            "completion script must include 'always' as a --color value"
        );
        assert!(
            output.contains("never"),
            "completion script must include 'never' as a --color value"
        );
    }

    /// AC (interactive): "yaml" and "json" offered for --output.
    #[test]
    fn test_generate_bash_completion_contains_output_format_enum_values() {
        let output = generate_bash_completion();
        assert!(
            output.contains("yaml"),
            "completion script must include 'yaml' as an --output value"
        );
        assert!(
            output.contains("json"),
            "completion script must include 'json' as an --output value"
        );
    }

    // ── Invalid shell argument ────────────────────────────────────────────────

    /// AC: invalid shell argument causes non-zero exit; clap rejects it at parse time.
    #[test]
    fn test_invalid_shell_argument_is_rejected_by_clap_with_error() {
        let result = crate::Cli::try_parse_from(["netfyr", "completions", "invalid_shell"]);
        assert!(
            result.is_err(),
            "clap must reject 'invalid_shell' as a Shell value and return an error"
        );
        let err = result.err().expect("must be an error").to_string();
        assert!(
            !err.is_empty(),
            "error message must be non-empty to indicate what went wrong"
        );
    }

    /// AC: invalid shell error message indicates the value is not valid.
    #[test]
    fn test_invalid_shell_argument_error_message_indicates_invalid_value() {
        let result = crate::Cli::try_parse_from(["netfyr", "completions", "invalid_shell"]);
        let err = result.err().expect("must be an error").to_string();
        // clap reports "invalid value" or "possible values" in the error
        assert!(
            err.to_lowercase().contains("invalid")
                || err.to_lowercase().contains("possible")
                || err.to_lowercase().contains("variant"),
            "error must indicate the value is not valid; got: {:?}",
            err
        );
    }
}
