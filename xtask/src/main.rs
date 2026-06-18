//! xtask — workspace development automation for netfyr.
//!
//! Run via: `cargo run --package xtask -- <subcommand>`
//!
//! Subcommands:
//!   man   Generate troff man pages from the clap CLI definitions.

use clap::{CommandFactory, Parser, Subcommand};
use clap_mangen::Man;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

// ── CLI for the xtask itself ──────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "xtask", about = "Workspace development automation")]
struct Xtask {
    #[command(subcommand)]
    command: XtaskCommand,
}

#[derive(Subcommand)]
enum XtaskCommand {
    /// Generate troff man pages from the clap CLI definitions.
    ///
    /// Outputs man/netfyr.1, man/netfyr-apply.1, man/netfyr-query.1.
    /// Does not overwrite man/netfyr-daemon.8, man/netfyr.yaml.5, or
    /// man/netfyr-examples.7 (maintained by hand).
    Man,
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    let args = Xtask::parse();
    match args.command {
        XtaskCommand::Man => {
            if let Err(e) = generate_man_pages() {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }
}

// ── Man page generation ───────────────────────────────────────────────────────

fn generate_man_pages() -> Result<(), Box<dyn std::error::Error>> {
    // CARGO_MANIFEST_DIR is set to the xtask/ directory at compile time.
    // Navigate one level up to reach the workspace root, then into man/.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = manifest_dir.join("../man");

    fs::create_dir_all(&out_dir)?;

    let cmd = netfyr_cli::Cli::command();

    // ── Top-level man page: netfyr.1 ─────────────────────────────────────────
    {
        let mut buf = Vec::new();
        let man = Man::new(cmd.clone());
        man.render(&mut buf)?;
        append_exit_status(&mut buf, None)?;
        append_files(&mut buf)?;
        append_examples(&mut buf, None)?;
        append_environment(&mut buf)?;
        append_see_also(&mut buf, None)?;
        fs::write(out_dir.join("netfyr.1"), &buf)?;
        println!("Generated: man/netfyr.1");
    }

    // ── Subcommand man pages ──────────────────────────────────────────────────
    for subcmd in cmd.get_subcommands() {
        let name = format!("netfyr-{}", subcmd.get_name());
        let subcmd_name = subcmd.get_name().to_string();
        // Clone and rename so the man page header shows NETFYR-APPLY(1) etc.
        let subcmd = subcmd.clone().name(name.clone());
        let man = Man::new(subcmd);
        let mut buf = Vec::new();
        man.render(&mut buf)?;
        append_exit_status(&mut buf, Some(&subcmd_name))?;
        append_files(&mut buf)?;
        append_examples(&mut buf, Some(&subcmd_name))?;
        append_environment(&mut buf)?;
        append_see_also(&mut buf, Some(&subcmd_name))?;
        let filename = format!("{name}.1");
        fs::write(out_dir.join(&filename), &buf)?;
        println!("Generated: man/{filename}");
    }

    println!("Note: man/netfyr-daemon.8, man/netfyr.yaml.5, and man/netfyr-examples.7 are maintained by hand and were not modified.");
    Ok(())
}

// ── Troff section helpers ─────────────────────────────────────────────────────

/// Append `.SH "EXIT STATUS"` with `.TP` entries for codes 0, 1, and 2.
fn append_exit_status(buf: &mut Vec<u8>, _subcommand: Option<&str>) -> std::io::Result<()> {
    writeln!(buf, ".SH \"EXIT STATUS\"")?;
    writeln!(buf, ".TP")?;
    writeln!(buf, ".B 0")?;
    writeln!(buf, "All operations succeeded or no changes needed.")?;
    writeln!(buf, ".TP")?;
    writeln!(buf, ".B 1")?;
    writeln!(buf, "Partial failure or conflicts detected.")?;
    writeln!(buf, ".TP")?;
    writeln!(buf, ".B 2")?;
    writeln!(buf, "Total failure or fatal error.")?;
    Ok(())
}

/// Append `.SH FILES` listing the standard netfyr file paths.
fn append_files(buf: &mut Vec<u8>) -> std::io::Result<()> {
    writeln!(buf, ".SH FILES")?;
    writeln!(buf, ".TP")?;
    writeln!(buf, r".I /etc/netfyr/policies/")?;
    writeln!(buf, "Default directory for policy files.")?;
    writeln!(buf, ".TP")?;
    writeln!(buf, r".I /var/lib/netfyr/")?;
    writeln!(buf, "State directory for persistent daemon data.")?;
    Ok(())
}

/// Append `.SH EXAMPLES` with at least two usage examples per command.
fn append_examples(buf: &mut Vec<u8>, subcommand: Option<&str>) -> std::io::Result<()> {
    writeln!(buf, ".SH EXAMPLES")?;
    match subcommand {
        None => {
            // Top-level netfyr — show one example per subcommand.
            writeln!(buf, "Apply all policies in the default directory:")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, ".RS 4")?;
            writeln!(buf, ".nf")?;
            writeln!(buf, "netfyr apply /etc/netfyr/policies/")?;
            writeln!(buf, ".fi")?;
            writeln!(buf, ".RE")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, "Query current network state:")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, ".RS 4")?;
            writeln!(buf, ".nf")?;
            writeln!(buf, "netfyr query")?;
            writeln!(buf, ".fi")?;
            writeln!(buf, ".RE")?;
        }
        Some("apply") => {
            writeln!(buf, "Apply all policies in the default directory:")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, ".RS 4")?;
            writeln!(buf, ".nf")?;
            writeln!(buf, "netfyr apply /etc/netfyr/policies/")?;
            writeln!(buf, ".fi")?;
            writeln!(buf, ".RE")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, "Preview changes before applying:")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, ".RS 4")?;
            writeln!(buf, ".nf")?;
            writeln!(buf, "netfyr apply --dry-run /etc/netfyr/policies/server.yaml")?;
            writeln!(buf, ".fi")?;
            writeln!(buf, ".RE")?;
        }
        Some("query") => {
            writeln!(buf, "Query all network interfaces:")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, ".RS 4")?;
            writeln!(buf, ".nf")?;
            writeln!(buf, "netfyr query")?;
            writeln!(buf, ".fi")?;
            writeln!(buf, ".RE")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, "Query a specific interface by name, output as JSON:")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, ".RS 4")?;
            writeln!(buf, ".nf")?;
            writeln!(buf, "netfyr query -s type=ethernet -s name=eth0 -o json")?;
            writeln!(buf, ".fi")?;
            writeln!(buf, ".RE")?;
        }
        Some("history") => {
            writeln!(buf, "Show the 10 most recent history entries:")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, ".RS 4")?;
            writeln!(buf, ".nf")?;
            writeln!(buf, "netfyr history -n 10")?;
            writeln!(buf, ".fi")?;
            writeln!(buf, ".RE")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, "Show changes from the last hour triggered by policy apply:")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, ".RS 4")?;
            writeln!(buf, ".nf")?;
            writeln!(buf, "netfyr history --since 1h --trigger apply")?;
            writeln!(buf, ".fi")?;
            writeln!(buf, ".RE")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, "Show the 5 most recent entries with full timestamps:")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, ".RS 4")?;
            writeln!(buf, ".nf")?;
            writeln!(buf, "netfyr history -n 5 --absolute-timestamps")?;
            writeln!(buf, ".fi")?;
            writeln!(buf, ".RE")?;
        }
        Some("revert") => {
            writeln!(buf, "Revert to the state recorded in journal entry 42:")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, ".RS 4")?;
            writeln!(buf, ".nf")?;
            writeln!(buf, "netfyr revert 42")?;
            writeln!(buf, ".fi")?;
            writeln!(buf, ".RE")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, "Preview what a revert would change without applying:")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, ".RS 4")?;
            writeln!(buf, ".nf")?;
            writeln!(buf, "netfyr revert --dry-run 42")?;
            writeln!(buf, ".fi")?;
            writeln!(buf, ".RE")?;
        }
        Some("show") => {
            writeln!(buf, "Show system overview:")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, ".RS 4")?;
            writeln!(buf, ".nf")?;
            writeln!(buf, "netfyr show")?;
            writeln!(buf, ".fi")?;
            writeln!(buf, ".RE")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, "Show system overview as JSON for scripting:")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, ".RS 4")?;
            writeln!(buf, ".nf")?;
            writeln!(buf, "netfyr show -o json")?;
            writeln!(buf, ".fi")?;
            writeln!(buf, ".RE")?;
        }
        Some("diagnose") => {
            writeln!(buf, "Run a diagnostic check on all managed interfaces:")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, ".RS 4")?;
            writeln!(buf, ".nf")?;
            writeln!(buf, "netfyr diagnose")?;
            writeln!(buf, ".fi")?;
            writeln!(buf, ".RE")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, "Scan the last 24 hours and output findings as JSON:")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, ".RS 4")?;
            writeln!(buf, ".nf")?;
            writeln!(buf, "netfyr diagnose --since 24h -o json")?;
            writeln!(buf, ".fi")?;
            writeln!(buf, ".RE")?;
        }
        Some("completions") => {
            writeln!(buf, "Generate bash completions and install them:")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, ".RS 4")?;
            writeln!(buf, ".nf")?;
            writeln!(buf, "netfyr completions bash > ~/.local/share/bash-completion/completions/netfyr")?;
            writeln!(buf, ".fi")?;
            writeln!(buf, ".RE")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, "Generate fish completions:")?;
            writeln!(buf, ".PP")?;
            writeln!(buf, ".RS 4")?;
            writeln!(buf, ".nf")?;
            writeln!(buf, "netfyr completions fish > ~/.config/fish/completions/netfyr.fish")?;
            writeln!(buf, ".fi")?;
            writeln!(buf, ".RE")?;
        }
        Some(other) => {
            // Fallback for any future subcommands.
            writeln!(buf, "See")?;
            writeln!(buf, ".BR netfyr-{other} (1)")?;
            writeln!(buf, "for usage details.")?;
        }
    }
    Ok(())
}

/// Append `.SH ENVIRONMENT` documenting the NO_COLOR variable.
fn append_environment(buf: &mut Vec<u8>) -> std::io::Result<()> {
    writeln!(buf, ".SH ENVIRONMENT")?;
    writeln!(buf, ".TP")?;
    writeln!(buf, ".B NO_COLOR")?;
    writeln!(buf, "If set (to any value), colored output is disabled regardless of the")?;
    writeln!(buf, ".B \\-\\-color")?;
    writeln!(buf, "flag.")?;
    Ok(())
}

/// Append `.SH "SEE ALSO"` with cross-references to all netfyr man pages.
fn append_see_also(buf: &mut Vec<u8>, subcommand: Option<&str>) -> std::io::Result<()> {
    writeln!(buf, ".SH \"SEE ALSO\"")?;
    match subcommand {
        None => {
            // Top-level page — reference all subcommand and supplementary pages.
            writeln!(buf, ".BR netfyr-apply (1),")?;
            writeln!(buf, ".BR netfyr-query (1),")?;
            writeln!(buf, ".BR netfyr-show (1),")?;
            writeln!(buf, ".BR netfyr-history (1),")?;
            writeln!(buf, ".BR netfyr-revert (1),")?;
            writeln!(buf, ".BR netfyr-diagnose (1),")?;
            writeln!(buf, ".BR netfyr-completions (1),")?;
            writeln!(buf, ".BR netfyr-daemon (8),")?;
            writeln!(buf, ".BR netfyr-examples (7),")?;
            writeln!(buf, r".BR netfyr.yaml (5)")?;
        }
        Some("apply") => {
            writeln!(buf, ".BR netfyr (1),")?;
            writeln!(buf, ".BR netfyr-query (1),")?;
            writeln!(buf, ".BR netfyr-show (1),")?;
            writeln!(buf, ".BR netfyr-history (1),")?;
            writeln!(buf, ".BR netfyr-revert (1),")?;
            writeln!(buf, ".BR netfyr-daemon (8),")?;
            writeln!(buf, ".BR netfyr-examples (7),")?;
            writeln!(buf, r".BR netfyr.yaml (5)")?;
        }
        Some("query") => {
            writeln!(buf, ".BR netfyr (1),")?;
            writeln!(buf, ".BR netfyr-apply (1),")?;
            writeln!(buf, ".BR netfyr-show (1),")?;
            writeln!(buf, ".BR netfyr-history (1),")?;
            writeln!(buf, ".BR netfyr-revert (1),")?;
            writeln!(buf, ".BR netfyr-daemon (8),")?;
            writeln!(buf, ".BR netfyr-examples (7),")?;
            writeln!(buf, r".BR netfyr.yaml (5)")?;
        }
        Some(_) => {
            writeln!(buf, ".BR netfyr (1),")?;
            writeln!(buf, ".BR netfyr-apply (1),")?;
            writeln!(buf, ".BR netfyr-query (1),")?;
            writeln!(buf, ".BR netfyr-show (1),")?;
            writeln!(buf, ".BR netfyr-history (1),")?;
            writeln!(buf, ".BR netfyr-revert (1),")?;
            writeln!(buf, ".BR netfyr-daemon (8),")?;
            writeln!(buf, ".BR netfyr-examples (7),")?;
            writeln!(buf, r".BR netfyr.yaml (5)")?;
        }
    }
    Ok(())
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Invoke a troff-section helper and return its output as a UTF-8 string.
    fn render<F: FnOnce(&mut Vec<u8>) -> std::io::Result<()>>(f: F) -> String {
        let mut buf = Vec::new();
        f(&mut buf).expect("helper must not fail");
        String::from_utf8(buf).expect("output must be valid UTF-8")
    }

    // ── EXIT STATUS section ───────────────────────────────────────────────────

    /// AC: EXIT STATUS section header is emitted.
    #[test]
    fn test_exit_status_section_header_present() {
        let out = render(|buf| append_exit_status(buf, None));
        assert!(out.contains(".SH \"EXIT STATUS\""), "EXIT STATUS .SH header must be present");
    }

    /// AC: EXIT STATUS documents exit code 0 (success / no changes needed).
    #[test]
    fn test_exit_status_documents_code_0() {
        let out = render(|buf| append_exit_status(buf, None));
        assert!(out.contains(".B 0"), "EXIT STATUS must contain .B 0 for exit code 0");
        assert!(
            out.contains("succeeded") || out.contains("no changes"),
            "exit code 0 description must mention success or no-change condition"
        );
    }

    /// AC: EXIT STATUS documents exit code 1 (partial failure / conflicts).
    #[test]
    fn test_exit_status_documents_code_1() {
        let out = render(|buf| append_exit_status(buf, None));
        assert!(out.contains(".B 1"), "EXIT STATUS must contain .B 1 for exit code 1");
        let lower = out.to_lowercase();
        assert!(
            lower.contains("partial") || lower.contains("conflict"),
            "exit code 1 description must mention partial failure or conflicts"
        );
    }

    /// AC: EXIT STATUS documents exit code 2 (total failure / fatal error).
    #[test]
    fn test_exit_status_documents_code_2() {
        let out = render(|buf| append_exit_status(buf, None));
        assert!(out.contains(".B 2"), "EXIT STATUS must contain .B 2 for exit code 2");
        let lower = out.to_lowercase();
        assert!(
            lower.contains("total") || lower.contains("fatal") || lower.contains("failure"),
            "exit code 2 description must mention total failure or fatal error"
        );
    }

    /// EXIT STATUS section is emitted identically regardless of subcommand.
    #[test]
    fn test_exit_status_same_for_all_subcommands() {
        let none_out = render(|buf| append_exit_status(buf, None));
        let apply_out = render(|buf| append_exit_status(buf, Some("apply")));
        let query_out = render(|buf| append_exit_status(buf, Some("query")));
        assert_eq!(none_out, apply_out, "EXIT STATUS must be identical for top-level and apply");
        assert_eq!(none_out, query_out, "EXIT STATUS must be identical for top-level and query");
    }

    // ── FILES section ─────────────────────────────────────────────────────────

    /// AC: FILES section header is emitted.
    #[test]
    fn test_files_section_header_present() {
        let out = render(append_files);
        assert!(out.contains(".SH FILES"), "FILES .SH header must be present");
    }

    /// AC: FILES section lists /etc/netfyr/policies/ (from the spec).
    #[test]
    fn test_files_section_lists_etc_netfyr_policies() {
        let out = render(append_files);
        assert!(
            out.contains("/etc/netfyr/policies/"),
            "FILES section must list /etc/netfyr/policies/"
        );
    }

    /// FILES section also documents the daemon state directory.
    #[test]
    fn test_files_section_lists_var_lib_netfyr() {
        let out = render(append_files);
        assert!(
            out.contains("/var/lib/netfyr/"),
            "FILES section must list /var/lib/netfyr/"
        );
    }

    // ── EXAMPLES section — apply subcommand ───────────────────────────────────

    /// AC: EXAMPLES section header is emitted for the apply subcommand.
    #[test]
    fn test_apply_examples_section_header_present() {
        let out = render(|buf| append_examples(buf, Some("apply")));
        assert!(out.contains(".SH EXAMPLES"), "EXAMPLES .SH header must be present for apply");
    }

    /// AC: apply EXAMPLES must contain at least two real-world usage examples.
    /// Each example is enclosed in a .nf / .fi no-fill block.
    #[test]
    fn test_apply_examples_has_at_least_two_nf_blocks() {
        let out = render(|buf| append_examples(buf, Some("apply")));
        let nf_count = out.matches(".nf").count();
        assert!(
            nf_count >= 2,
            "apply EXAMPLES must contain at least 2 usage examples (.nf blocks); found {nf_count}"
        );
    }

    /// AC: apply EXAMPLES must include a --dry-run usage example.
    #[test]
    fn test_apply_examples_includes_dry_run_usage() {
        let out = render(|buf| append_examples(buf, Some("apply")));
        assert!(
            out.contains("--dry-run"),
            "apply EXAMPLES must show a --dry-run usage example"
        );
    }

    /// AC: apply EXAMPLES must include the standard policies directory path.
    #[test]
    fn test_apply_examples_includes_default_policies_directory() {
        let out = render(|buf| append_examples(buf, Some("apply")));
        assert!(
            out.contains("/etc/netfyr/policies/"),
            "apply EXAMPLES must reference /etc/netfyr/policies/"
        );
    }

    // ── EXAMPLES section — query subcommand ───────────────────────────────────

    /// AC: query EXAMPLES must contain at least two real-world usage examples.
    #[test]
    fn test_query_examples_has_at_least_two_nf_blocks() {
        let out = render(|buf| append_examples(buf, Some("query")));
        let nf_count = out.matches(".nf").count();
        assert!(
            nf_count >= 2,
            "query EXAMPLES must contain at least 2 usage examples (.nf blocks); found {nf_count}"
        );
    }

    // ── EXAMPLES section — top-level (None) ──────────────────────────────────

    /// AC: top-level netfyr EXAMPLES must contain at least two usage examples.
    #[test]
    fn test_toplevel_examples_has_at_least_two_nf_blocks() {
        let out = render(|buf| append_examples(buf, None));
        let nf_count = out.matches(".nf").count();
        assert!(
            nf_count >= 2,
            "top-level EXAMPLES must contain at least 2 usage examples (.nf blocks); found {nf_count}"
        );
    }

    // ── SEE ALSO section ──────────────────────────────────────────────────────

    /// AC: SEE ALSO section header is emitted.
    #[test]
    fn test_see_also_section_header_present() {
        let out = render(|buf| append_see_also(buf, None));
        assert!(out.contains(".SH \"SEE ALSO\""), "SEE ALSO .SH header must be present");
    }

    /// AC: apply SEE ALSO must cross-reference netfyr(1).
    #[test]
    fn test_see_also_apply_references_netfyr_1() {
        let out = render(|buf| append_see_also(buf, Some("apply")));
        // clap_mangen emits .BR entries; check the page name and section.
        assert!(
            out.contains("netfyr (1)") || out.contains("netfyr(1)"),
            "apply SEE ALSO must reference netfyr(1); got:\n{out}"
        );
    }

    /// AC: apply SEE ALSO must cross-reference netfyr-query(1).
    #[test]
    fn test_see_also_apply_references_netfyr_query_1() {
        let out = render(|buf| append_see_also(buf, Some("apply")));
        assert!(
            out.contains("netfyr-query (1)") || out.contains("netfyr-query(1)"),
            "apply SEE ALSO must reference netfyr-query(1); got:\n{out}"
        );
    }

    /// AC: apply SEE ALSO must cross-reference netfyr.yaml(5).
    #[test]
    fn test_see_also_apply_references_netfyr_yaml_5() {
        let out = render(|buf| append_see_also(buf, Some("apply")));
        assert!(
            out.contains("netfyr.yaml (5)") || out.contains("netfyr.yaml(5)"),
            "apply SEE ALSO must reference netfyr.yaml(5); got:\n{out}"
        );
    }

    /// AC: top-level SEE ALSO must reference netfyr-apply(1).
    #[test]
    fn test_see_also_toplevel_references_netfyr_apply_1() {
        let out = render(|buf| append_see_also(buf, None));
        assert!(
            out.contains("netfyr-apply (1)") || out.contains("netfyr-apply(1)"),
            "top-level SEE ALSO must reference netfyr-apply(1); got:\n{out}"
        );
    }

    /// AC: top-level SEE ALSO must reference netfyr-query(1).
    #[test]
    fn test_see_also_toplevel_references_netfyr_query_1() {
        let out = render(|buf| append_see_also(buf, None));
        assert!(
            out.contains("netfyr-query (1)") || out.contains("netfyr-query(1)"),
            "top-level SEE ALSO must reference netfyr-query(1); got:\n{out}"
        );
    }

    /// AC: top-level SEE ALSO must reference netfyr-examples(7).
    #[test]
    fn test_see_also_toplevel_references_netfyr_examples_7() {
        let out = render(|buf| append_see_also(buf, None));
        assert!(
            out.contains("netfyr-examples (7)") || out.contains("netfyr-examples(7)"),
            "top-level SEE ALSO must reference netfyr-examples(7); got:\n{out}"
        );
    }

    /// AC: top-level SEE ALSO must also reference netfyr.yaml(5).
    #[test]
    fn test_see_also_toplevel_references_netfyr_yaml_5() {
        let out = render(|buf| append_see_also(buf, None));
        assert!(
            out.contains("netfyr.yaml (5)") || out.contains("netfyr.yaml(5)"),
            "top-level SEE ALSO must reference netfyr.yaml(5); got:\n{out}"
        );
    }

    /// query SEE ALSO must reference both netfyr(1) and netfyr-apply(1).
    #[test]
    fn test_see_also_query_references_netfyr_and_apply() {
        let out = render(|buf| append_see_also(buf, Some("query")));
        assert!(
            out.contains("netfyr (1)") || out.contains("netfyr(1)"),
            "query SEE ALSO must reference netfyr(1)"
        );
        assert!(
            out.contains("netfyr-apply (1)") || out.contains("netfyr-apply(1)"),
            "query SEE ALSO must reference netfyr-apply(1)"
        );
    }

    // ── netfyr.yaml.5 man page content tests ─────────────────────────────────

    fn read_yaml_man_page() -> String {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let man_path = manifest_dir.join("../man/netfyr.yaml.5");
        std::fs::read_to_string(&man_path)
            .unwrap_or_else(|e| panic!("Failed to read man/netfyr.yaml.5: {e}"))
    }

    /// AC: Man page exists.
    #[test]
    fn test_yaml_man_page_exists() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let man_path = manifest_dir.join("../man/netfyr.yaml.5");
        assert!(man_path.exists(), "man/netfyr.yaml.5 must exist at {}", man_path.display());
    }

    /// AC: The NAME section contains "netfyr.yaml".
    #[test]
    fn test_yaml_man_page_name_section_contains_netfyr_yaml() {
        let content = read_yaml_man_page();
        assert!(
            content.contains(".SH NAME"),
            "man/netfyr.yaml.5 must have a NAME section"
        );
        assert!(
            content.contains("netfyr.yaml"),
            "NAME section must contain 'netfyr.yaml'"
        );
    }

    /// AC: The TH header declares section 5.
    #[test]
    fn test_yaml_man_page_is_section_5() {
        let content = read_yaml_man_page();
        assert!(
            content.contains(".TH") && content.contains(" 5 "),
            "man page header (.TH) must declare section 5"
        );
    }

    /// AC: BARE STATE FORMAT section exists and describes the flat format.
    #[test]
    fn test_yaml_man_page_bare_state_format_section_exists() {
        let content = read_yaml_man_page();
        assert!(
            content.contains("BARE STATE FORMAT"),
            "man/netfyr.yaml.5 must have a BARE STATE FORMAT section"
        );
    }

    /// AC: BARE STATE FORMAT documents the `type` field.
    #[test]
    fn test_yaml_man_page_bare_state_documents_type_field() {
        let content = read_yaml_man_page();
        // The type field must appear in the BARE STATE FORMAT section.
        let bare_start = content.find("BARE STATE FORMAT").expect("BARE STATE FORMAT section must exist");
        let policy_start = content.find("POLICY FORMAT").expect("POLICY FORMAT section must exist");
        let bare_section = &content[bare_start..policy_start];
        assert!(
            bare_section.contains("type"),
            "BARE STATE FORMAT must document the 'type' field"
        );
    }

    /// AC: BARE STATE FORMAT includes at least one example (.nf block).
    #[test]
    fn test_yaml_man_page_bare_state_format_has_example() {
        let content = read_yaml_man_page();
        let bare_start = content.find("BARE STATE FORMAT").expect("BARE STATE FORMAT section must exist");
        let policy_start = content.find("POLICY FORMAT").expect("POLICY FORMAT section must exist");
        let bare_section = &content[bare_start..policy_start];
        assert!(
            bare_section.contains(".nf"),
            "BARE STATE FORMAT must include at least one example (.nf block)"
        );
    }

    /// AC: POLICY FORMAT section exists.
    #[test]
    fn test_yaml_man_page_policy_format_section_exists() {
        let content = read_yaml_man_page();
        assert!(
            content.contains("POLICY FORMAT"),
            "man/netfyr.yaml.5 must have a POLICY FORMAT section"
        );
    }

    /// AC: POLICY FORMAT documents the `kind` field.
    #[test]
    fn test_yaml_man_page_policy_format_documents_kind() {
        let content = read_yaml_man_page();
        let policy_start = content.find("POLICY FORMAT").expect("POLICY FORMAT section must exist");
        let multi_start = content.find("MULTI-DOCUMENT").expect("MULTI-DOCUMENT section must exist");
        let policy_section = &content[policy_start..multi_start];
        assert!(
            policy_section.contains("kind"),
            "POLICY FORMAT must document the 'kind' field"
        );
    }

    /// AC: POLICY FORMAT documents the `name` field.
    #[test]
    fn test_yaml_man_page_policy_format_documents_name() {
        let content = read_yaml_man_page();
        let policy_start = content.find("POLICY FORMAT").expect("POLICY FORMAT section must exist");
        let multi_start = content.find("MULTI-DOCUMENT").expect("MULTI-DOCUMENT section must exist");
        let policy_section = &content[policy_start..multi_start];
        assert!(
            policy_section.contains("name"),
            "POLICY FORMAT must document the 'name' field"
        );
    }

    /// AC: POLICY FORMAT documents the `factory` field.
    #[test]
    fn test_yaml_man_page_policy_format_documents_factory() {
        let content = read_yaml_man_page();
        let policy_start = content.find("POLICY FORMAT").expect("POLICY FORMAT section must exist");
        let multi_start = content.find("MULTI-DOCUMENT").expect("MULTI-DOCUMENT section must exist");
        let policy_section = &content[policy_start..multi_start];
        assert!(
            policy_section.contains("factory"),
            "POLICY FORMAT must document the 'factory' field"
        );
    }

    /// AC: POLICY FORMAT documents the `priority` field.
    #[test]
    fn test_yaml_man_page_policy_format_documents_priority() {
        let content = read_yaml_man_page();
        let policy_start = content.find("POLICY FORMAT").expect("POLICY FORMAT section must exist");
        let multi_start = content.find("MULTI-DOCUMENT").expect("MULTI-DOCUMENT section must exist");
        let policy_section = &content[policy_start..multi_start];
        assert!(
            policy_section.contains("priority"),
            "POLICY FORMAT must document the 'priority' field"
        );
    }

    /// AC: POLICY FORMAT documents the `selector` field.
    #[test]
    fn test_yaml_man_page_policy_format_documents_selector() {
        let content = read_yaml_man_page();
        let policy_start = content.find("POLICY FORMAT").expect("POLICY FORMAT section must exist");
        let multi_start = content.find("MULTI-DOCUMENT").expect("MULTI-DOCUMENT section must exist");
        let policy_section = &content[policy_start..multi_start];
        assert!(
            policy_section.contains("selector"),
            "POLICY FORMAT must document the 'selector' field"
        );
    }

    /// AC: POLICY FORMAT documents the `state` field.
    #[test]
    fn test_yaml_man_page_policy_format_documents_state_field() {
        let content = read_yaml_man_page();
        let policy_start = content.find("POLICY FORMAT").expect("POLICY FORMAT section must exist");
        let multi_start = content.find("MULTI-DOCUMENT").expect("MULTI-DOCUMENT section must exist");
        let policy_section = &content[policy_start..multi_start];
        assert!(
            policy_section.contains("state"),
            "POLICY FORMAT must document the 'state' field"
        );
    }

    /// AC: POLICY FORMAT documents the `states` field.
    #[test]
    fn test_yaml_man_page_policy_format_documents_states_field() {
        let content = read_yaml_man_page();
        let policy_start = content.find("POLICY FORMAT").expect("POLICY FORMAT section must exist");
        let multi_start = content.find("MULTI-DOCUMENT").expect("MULTI-DOCUMENT section must exist");
        let policy_section = &content[policy_start..multi_start];
        assert!(
            policy_section.contains("states"),
            "POLICY FORMAT must document the 'states' field"
        );
    }

    /// AC: POLICY FORMAT documents the "static" factory type.
    #[test]
    fn test_yaml_man_page_policy_format_documents_static_factory() {
        let content = read_yaml_man_page();
        let policy_start = content.find("POLICY FORMAT").expect("POLICY FORMAT section must exist");
        let multi_start = content.find("MULTI-DOCUMENT").expect("MULTI-DOCUMENT section must exist");
        let policy_section = &content[policy_start..multi_start];
        assert!(
            policy_section.contains("static"),
            "POLICY FORMAT must document the 'static' factory type"
        );
    }

    /// AC: POLICY FORMAT documents the "dhcpv4" factory type.
    #[test]
    fn test_yaml_man_page_policy_format_documents_dhcpv4_factory() {
        let content = read_yaml_man_page();
        let policy_start = content.find("POLICY FORMAT").expect("POLICY FORMAT section must exist");
        let multi_start = content.find("MULTI-DOCUMENT").expect("MULTI-DOCUMENT section must exist");
        let policy_section = &content[policy_start..multi_start];
        assert!(
            policy_section.contains("dhcpv4"),
            "POLICY FORMAT must document the 'dhcpv4' factory type"
        );
    }

    /// AC: POLICY FORMAT includes an example for the static factory.
    #[test]
    fn test_yaml_man_page_policy_format_has_static_example() {
        let content = read_yaml_man_page();
        let policy_start = content.find("POLICY FORMAT").expect("POLICY FORMAT section must exist");
        let multi_start = content.find("MULTI-DOCUMENT").expect("MULTI-DOCUMENT section must exist");
        let policy_section = &content[policy_start..multi_start];
        assert!(
            policy_section.contains("factory: static"),
            "POLICY FORMAT must include a static factory example"
        );
    }

    /// AC: POLICY FORMAT includes an example for the dhcpv4 factory.
    #[test]
    fn test_yaml_man_page_policy_format_has_dhcpv4_example() {
        let content = read_yaml_man_page();
        let policy_start = content.find("POLICY FORMAT").expect("POLICY FORMAT section must exist");
        let multi_start = content.find("MULTI-DOCUMENT").expect("MULTI-DOCUMENT section must exist");
        let policy_section = &content[policy_start..multi_start];
        assert!(
            policy_section.contains("factory: dhcpv4") || policy_section.contains("factory: dhcpv4\\"),
            "POLICY FORMAT must include a dhcpv4 factory example"
        );
    }

    /// AC: MULTI-DOCUMENT FILES section exists and explains "---" separator.
    #[test]
    fn test_yaml_man_page_multi_document_section_exists() {
        let content = read_yaml_man_page();
        assert!(
            content.contains("MULTI-DOCUMENT"),
            "man/netfyr.yaml.5 must have a MULTI-DOCUMENT FILES section"
        );
    }

    /// AC: MULTI-DOCUMENT FILES mentions the "---" separator.
    #[test]
    fn test_yaml_man_page_multi_document_explains_separator() {
        let content = read_yaml_man_page();
        let multi_start = content.find("MULTI-DOCUMENT").expect("MULTI-DOCUMENT section must exist");
        let selectors_start = content.find("\n.SH SELECTORS").expect(".SH SELECTORS section must exist");
        let multi_section = &content[multi_start..selectors_start];
        assert!(
            multi_section.contains("---") || multi_section.contains("\\-\\-\\-"),
            "MULTI-DOCUMENT FILES section must mention the '---' separator"
        );
    }

    /// AC: MULTI-DOCUMENT FILES includes at least one example.
    #[test]
    fn test_yaml_man_page_multi_document_has_example() {
        let content = read_yaml_man_page();
        let multi_start = content.find("MULTI-DOCUMENT").expect("MULTI-DOCUMENT section must exist");
        let selectors_start = content.find("\n.SH SELECTORS").expect(".SH SELECTORS section must exist");
        let multi_section = &content[multi_start..selectors_start];
        assert!(
            multi_section.contains(".nf"),
            "MULTI-DOCUMENT FILES section must include at least one example (.nf block)"
        );
    }

    /// AC: SELECTORS section documents the `name` selector field.
    #[test]
    fn test_yaml_man_page_selectors_documents_name() {
        let content = read_yaml_man_page();
        let sel_start = content.find("\n.SH SELECTORS").expect("SELECTORS section must exist");
        let fields_start = content.find("\n.SH FIELDS").expect("FIELDS section must exist");
        let sel_section = &content[sel_start..fields_start];
        assert!(
            sel_section.contains("name"),
            "SELECTORS section must document the 'name' selector field"
        );
    }

    /// AC: SELECTORS section documents the `driver` selector field.
    #[test]
    fn test_yaml_man_page_selectors_documents_driver() {
        let content = read_yaml_man_page();
        let sel_start = content.find("\n.SH SELECTORS").expect("SELECTORS section must exist");
        let fields_start = content.find("\n.SH FIELDS").expect("FIELDS section must exist");
        let sel_section = &content[sel_start..fields_start];
        assert!(
            sel_section.contains("driver"),
            "SELECTORS section must document the 'driver' selector field"
        );
    }

    /// AC: SELECTORS section documents the `pci_path` selector field.
    #[test]
    fn test_yaml_man_page_selectors_documents_pci_path() {
        let content = read_yaml_man_page();
        let sel_start = content.find("\n.SH SELECTORS").expect("SELECTORS section must exist");
        let fields_start = content.find("\n.SH FIELDS").expect("FIELDS section must exist");
        let sel_section = &content[sel_start..fields_start];
        assert!(
            sel_section.contains("pci_path"),
            "SELECTORS section must document the 'pci_path' selector field"
        );
    }

    /// AC: SELECTORS section documents the `mac` selector field.
    #[test]
    fn test_yaml_man_page_selectors_documents_mac() {
        let content = read_yaml_man_page();
        let sel_start = content.find("\n.SH SELECTORS").expect("SELECTORS section must exist");
        let fields_start = content.find("\n.SH FIELDS").expect("FIELDS section must exist");
        let sel_section = &content[sel_start..fields_start];
        assert!(
            sel_section.contains("mac"),
            "SELECTORS section must document the 'mac' selector field"
        );
    }

    /// AC: FIELDS section documents the `mtu` ethernet field.
    #[test]
    fn test_yaml_man_page_fields_documents_mtu() {
        let content = read_yaml_man_page();
        let fields_start = content.find("\n.SH FIELDS").expect("FIELDS section must exist");
        let value_start = content.find("VALUE TYPES").expect("VALUE TYPES section must exist");
        let fields_section = &content[fields_start..value_start];
        assert!(
            fields_section.contains("mtu"),
            "FIELDS section must document the 'mtu' ethernet field"
        );
    }

    /// AC: FIELDS section documents the `addresses` ethernet field.
    #[test]
    fn test_yaml_man_page_fields_documents_addresses() {
        let content = read_yaml_man_page();
        let fields_start = content.find("\n.SH FIELDS").expect("FIELDS section must exist");
        let value_start = content.find("VALUE TYPES").expect("VALUE TYPES section must exist");
        let fields_section = &content[fields_start..value_start];
        assert!(
            fields_section.contains("addresses"),
            "FIELDS section must document the 'addresses' ethernet field"
        );
    }

    /// AC: FIELDS section documents the `routes` ethernet field.
    #[test]
    fn test_yaml_man_page_fields_documents_routes() {
        let content = read_yaml_man_page();
        let fields_start = content.find("\n.SH FIELDS").expect("FIELDS section must exist");
        let value_start = content.find("VALUE TYPES").expect("VALUE TYPES section must exist");
        let fields_section = &content[fields_start..value_start];
        assert!(
            fields_section.contains("routes"),
            "FIELDS section must document the 'routes' ethernet field"
        );
    }

    /// AC: FIELDS section documents the `state` ethernet field.
    #[test]
    fn test_yaml_man_page_fields_documents_state_field() {
        let content = read_yaml_man_page();
        let fields_start = content.find("\n.SH FIELDS").expect("FIELDS section must exist");
        let value_start = content.find("VALUE TYPES").expect("VALUE TYPES section must exist");
        let fields_section = &content[fields_start..value_start];
        assert!(
            fields_section.contains("state"),
            "FIELDS section must document the 'state' ethernet field"
        );
    }

    /// AC: VALUE TYPES section exists and maps YAML boolean to netfyr Bool.
    #[test]
    fn test_yaml_man_page_value_types_maps_boolean() {
        let content = read_yaml_man_page();
        let vt_start = content.find("VALUE TYPES").expect("VALUE TYPES section must exist");
        let files_start = content.find("\n.SH FILES").expect("FILES section must exist");
        let vt_section = &content[vt_start..files_start];
        assert!(
            vt_section.contains("Bool") || vt_section.contains("bool"),
            "VALUE TYPES section must map YAML boolean to netfyr Bool"
        );
        assert!(
            vt_section.contains("boolean") || vt_section.contains("Boolean"),
            "VALUE TYPES section must mention YAML boolean type"
        );
    }

    /// AC: VALUE TYPES section maps non-negative YAML integers to U64.
    #[test]
    fn test_yaml_man_page_value_types_maps_u64() {
        let content = read_yaml_man_page();
        let vt_start = content.find("VALUE TYPES").expect("VALUE TYPES section must exist");
        let files_start = content.find("\n.SH FILES").expect("FILES section must exist");
        let vt_section = &content[vt_start..files_start];
        assert!(
            vt_section.contains("U64"),
            "VALUE TYPES section must map non-negative integers to U64"
        );
    }

    /// AC: VALUE TYPES section maps negative YAML integers to I64.
    #[test]
    fn test_yaml_man_page_value_types_maps_i64() {
        let content = read_yaml_man_page();
        let vt_start = content.find("VALUE TYPES").expect("VALUE TYPES section must exist");
        let files_start = content.find("\n.SH FILES").expect("FILES section must exist");
        let vt_section = &content[vt_start..files_start];
        assert!(
            vt_section.contains("I64"),
            "VALUE TYPES section must map negative integers to I64"
        );
    }

    /// AC: VALUE TYPES section maps valid IPv4 strings to IpAddr.
    #[test]
    fn test_yaml_man_page_value_types_maps_ipaddr() {
        let content = read_yaml_man_page();
        let vt_start = content.find("VALUE TYPES").expect("VALUE TYPES section must exist");
        let files_start = content.find("\n.SH FILES").expect("FILES section must exist");
        let vt_section = &content[vt_start..files_start];
        assert!(
            vt_section.contains("IpAddr"),
            "VALUE TYPES section must map valid IPv4 strings to IpAddr"
        );
    }

    /// AC: VALUE TYPES section maps CIDR strings to IpNetwork.
    #[test]
    fn test_yaml_man_page_value_types_maps_ipnetwork() {
        let content = read_yaml_man_page();
        let vt_start = content.find("VALUE TYPES").expect("VALUE TYPES section must exist");
        let files_start = content.find("\n.SH FILES").expect("FILES section must exist");
        let vt_section = &content[vt_start..files_start];
        assert!(
            vt_section.contains("IpNetwork"),
            "VALUE TYPES section must map CIDR strings to IpNetwork"
        );
    }

    /// AC: VALUE TYPES section maps YAML sequences to List.
    #[test]
    fn test_yaml_man_page_value_types_maps_list() {
        let content = read_yaml_man_page();
        let vt_start = content.find("VALUE TYPES").expect("VALUE TYPES section must exist");
        let files_start = content.find("\n.SH FILES").expect("FILES section must exist");
        let vt_section = &content[vt_start..files_start];
        assert!(
            vt_section.contains("List"),
            "VALUE TYPES section must map YAML sequences to List"
        );
    }

    /// AC: VALUE TYPES section maps YAML mappings to Map.
    #[test]
    fn test_yaml_man_page_value_types_maps_map() {
        let content = read_yaml_man_page();
        let vt_start = content.find("VALUE TYPES").expect("VALUE TYPES section must exist");
        let files_start = content.find("\n.SH FILES").expect("FILES section must exist");
        let vt_section = &content[vt_start..files_start];
        assert!(
            vt_section.contains("Map"),
            "VALUE TYPES section must map YAML mappings to Map"
        );
    }

    /// AC: FILES section lists /etc/netfyr/policies/.
    #[test]
    fn test_yaml_man_page_files_section_lists_etc_netfyr_policies() {
        let content = read_yaml_man_page();
        let files_start = content.find("\n.SH FILES").expect("FILES section must exist");
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO section must exist");
        let files_section = &content[files_start..see_also_start];
        assert!(
            files_section.contains("/etc/netfyr/policies/"),
            "FILES section must list /etc/netfyr/policies/"
        );
    }

    /// AC: FILES section lists /var/lib/netfyr/policies/.
    #[test]
    fn test_yaml_man_page_files_section_lists_var_lib_netfyr_policies() {
        let content = read_yaml_man_page();
        let files_start = content.find("\n.SH FILES").expect("FILES section must exist");
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO section must exist");
        let files_section = &content[files_start..see_also_start];
        assert!(
            files_section.contains("/var/lib/netfyr/"),
            "FILES section must list /var/lib/netfyr/ (policies directory)"
        );
    }

    /// AC: The man page is hand-maintained (not auto-generated by xtask).
    #[test]
    fn test_yaml_man_page_has_hand_maintained_comment() {
        let content = read_yaml_man_page();
        let lower = content.to_lowercase();
        assert!(
            lower.contains("hand") || lower.contains("maintained") || lower.contains("do not"),
            "man/netfyr.yaml.5 should include a comment noting it is maintained by hand"
        );
    }

    // ── Generated man page file existence ─────────────────────────────────────

    fn read_generated_man_page(name: &str) -> String {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let path = manifest_dir.join("../man").join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Failed to read man/{name} (run `cargo xtask man` first): {e}"))
    }

    fn man_page_path_exists(name: &str) -> bool {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        manifest_dir.join("../man").join(name).exists()
    }

    /// AC: Generate all man pages — netfyr.1 must exist.
    #[test]
    fn test_generated_netfyr_1_exists() {
        assert!(man_page_path_exists("netfyr.1"), "man/netfyr.1 must exist (run `cargo xtask man`)");
    }

    /// AC: Generate all man pages — netfyr-apply.1 must exist.
    #[test]
    fn test_generated_netfyr_apply_1_exists() {
        assert!(man_page_path_exists("netfyr-apply.1"), "man/netfyr-apply.1 must exist");
    }

    /// AC: Generate all man pages — netfyr-query.1 must exist.
    #[test]
    fn test_generated_netfyr_query_1_exists() {
        assert!(man_page_path_exists("netfyr-query.1"), "man/netfyr-query.1 must exist");
    }

    /// AC: Generate all man pages — netfyr-history.1 must exist.
    #[test]
    fn test_generated_netfyr_history_1_exists() {
        assert!(man_page_path_exists("netfyr-history.1"), "man/netfyr-history.1 must exist");
    }

    /// AC: Generate all man pages — netfyr-revert.1 must exist.
    #[test]
    fn test_generated_netfyr_revert_1_exists() {
        assert!(man_page_path_exists("netfyr-revert.1"), "man/netfyr-revert.1 must exist");
    }

    /// AC: Generate all man pages — netfyr-diagnose.1 must exist.
    #[test]
    fn test_generated_netfyr_diagnose_1_exists() {
        assert!(man_page_path_exists("netfyr-diagnose.1"), "man/netfyr-diagnose.1 must exist (run `cargo xtask man`)");
    }

    /// AC: Generate all man pages — netfyr-completions.1 must exist.
    #[test]
    fn test_generated_netfyr_completions_1_exists() {
        assert!(man_page_path_exists("netfyr-completions.1"), "man/netfyr-completions.1 must exist (run `cargo xtask man`)");
    }

    // ── Top-level netfyr.1 content ────────────────────────────────────────────

    /// AC: Top-level man page lists all subcommands — DESCRIPTION mentions apply.
    #[test]
    fn test_netfyr_1_description_mentions_apply() {
        let content = read_generated_man_page("netfyr.1");
        let desc_start = content.find(".SH DESCRIPTION").expect("DESCRIPTION section must exist in netfyr.1");
        let options_start = content.find(".SH OPTIONS").expect("OPTIONS section must exist in netfyr.1");
        let desc = &content[desc_start..options_start];
        assert!(desc.contains("apply"), "netfyr.1 DESCRIPTION must mention the apply subcommand");
    }

    /// AC: Top-level man page lists all subcommands — DESCRIPTION mentions query.
    #[test]
    fn test_netfyr_1_description_mentions_query() {
        let content = read_generated_man_page("netfyr.1");
        let desc_start = content.find(".SH DESCRIPTION").expect("DESCRIPTION section must exist in netfyr.1");
        let options_start = content.find(".SH OPTIONS").expect("OPTIONS section must exist in netfyr.1");
        let desc = &content[desc_start..options_start];
        assert!(desc.contains("query"), "netfyr.1 DESCRIPTION must mention the query subcommand");
    }

    /// AC: Top-level SEE ALSO references all subcommand man pages.
    #[test]
    fn test_netfyr_1_see_also_references_all_subcommands() {
        let content = read_generated_man_page("netfyr.1");
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO section must exist in netfyr.1");
        let see_also = &content[see_also_start..];
        assert!(
            see_also.contains("netfyr-apply"),
            "netfyr.1 SEE ALSO must reference netfyr-apply"
        );
        assert!(
            see_also.contains("netfyr-query"),
            "netfyr.1 SEE ALSO must reference netfyr-query"
        );
        assert!(
            see_also.contains("netfyr-history"),
            "netfyr.1 SEE ALSO must reference netfyr-history"
        );
        assert!(
            see_also.contains("netfyr-revert"),
            "netfyr.1 SEE ALSO must reference netfyr-revert"
        );
    }

    // ── netfyr-apply.1 OPTIONS ────────────────────────────────────────────────

    /// AC: netfyr-apply.1 OPTIONS lists --dry-run.
    #[test]
    fn test_netfyr_apply_1_options_lists_dry_run() {
        let content = read_generated_man_page("netfyr-apply.1");
        let options_start = content.find(".SH OPTIONS").expect("OPTIONS section must exist in netfyr-apply.1");
        let next_section = content[options_start + 1..]
            .find("\n.SH ")
            .map(|i| options_start + 1 + i)
            .unwrap_or(content.len());
        let options = &content[options_start..next_section];
        assert!(
            options.contains("dry-run") || options.contains("dry\\-run"),
            "netfyr-apply.1 OPTIONS must list --dry-run; OPTIONS section:\n{options}"
        );
    }

    /// AC: netfyr-apply.1 OPTIONS documents the <path> positional argument.
    #[test]
    fn test_netfyr_apply_1_options_documents_paths_argument() {
        let content = read_generated_man_page("netfyr-apply.1");
        let options_start = content.find(".SH OPTIONS").expect("OPTIONS section must exist in netfyr-apply.1");
        let next_section = content[options_start + 1..]
            .find("\n.SH ")
            .map(|i| options_start + 1 + i)
            .unwrap_or(content.len());
        let options = &content[options_start..next_section];
        // clap_mangen renders positional args with their metavar in angle brackets
        assert!(
            options.contains("PATH") || options.contains("path"),
            "netfyr-apply.1 OPTIONS must document the paths positional argument; OPTIONS:\n{options}"
        );
    }

    // ── netfyr-apply.1 required sections ─────────────────────────────────────

    /// AC: netfyr-apply.1 EXIT STATUS documents codes 0, 1, and 2.
    #[test]
    fn test_netfyr_apply_1_exit_status_documents_all_codes() {
        let content = read_generated_man_page("netfyr-apply.1");
        let es_start = content.find("EXIT STATUS").expect("EXIT STATUS section must exist in netfyr-apply.1");
        let es = &content[es_start..];
        assert!(es.contains(".B 0") || es.contains("\\fB0\\fR"), "netfyr-apply.1 EXIT STATUS must document code 0");
        assert!(es.contains(".B 1") || es.contains("\\fB1\\fR"), "netfyr-apply.1 EXIT STATUS must document code 1");
        assert!(es.contains(".B 2") || es.contains("\\fB2\\fR"), "netfyr-apply.1 EXIT STATUS must document code 2");
    }

    /// AC: netfyr-apply.1 EXAMPLES contains at least two usage examples.
    #[test]
    fn test_netfyr_apply_1_examples_has_at_least_two_examples() {
        let content = read_generated_man_page("netfyr-apply.1");
        let ex_start = content.find(".SH EXAMPLES").expect("EXAMPLES section must exist in netfyr-apply.1");
        let ex = &content[ex_start..];
        let nf_count = ex.matches(".nf").count();
        assert!(
            nf_count >= 2,
            "netfyr-apply.1 EXAMPLES must contain at least 2 usage examples (.nf blocks); found {nf_count}"
        );
    }

    /// AC: netfyr-apply.1 FILES section lists /etc/netfyr/policies/.
    #[test]
    fn test_netfyr_apply_1_files_lists_etc_netfyr_policies() {
        let content = read_generated_man_page("netfyr-apply.1");
        let files_start = content.find(".SH FILES").expect("FILES section must exist in netfyr-apply.1");
        let files = &content[files_start..];
        assert!(
            files.contains("/etc/netfyr/policies/"),
            "netfyr-apply.1 FILES must list /etc/netfyr/policies/"
        );
    }

    /// AC: netfyr-apply.1 SEE ALSO references netfyr(1), netfyr-query(1), and netfyr.yaml(5).
    #[test]
    fn test_netfyr_apply_1_see_also_cross_references() {
        let content = read_generated_man_page("netfyr-apply.1");
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO section must exist in netfyr-apply.1");
        let see_also = &content[see_also_start..];
        assert!(
            see_also.contains("netfyr (1)") || see_also.contains("netfyr(1)"),
            "netfyr-apply.1 SEE ALSO must reference netfyr(1)"
        );
        assert!(
            see_also.contains("netfyr-query") && (see_also.contains("(1)") || see_also.contains(" 1)")),
            "netfyr-apply.1 SEE ALSO must reference netfyr-query(1)"
        );
        assert!(
            see_also.contains("netfyr.yaml") && (see_also.contains("(5)") || see_also.contains(" 5)")),
            "netfyr-apply.1 SEE ALSO must reference netfyr.yaml(5)"
        );
    }

    // ── All generated section 1 pages have required sections ─────────────────

    /// AC: All subcommand pages include EXIT STATUS, FILES, EXAMPLES, SEE ALSO.
    #[test]
    fn test_all_subcommand_pages_have_required_sections() {
        let pages = ["netfyr-apply.1", "netfyr-query.1", "netfyr-history.1", "netfyr-revert.1", "netfyr-show.1", "netfyr-diagnose.1", "netfyr-completions.1"];
        for page in pages {
            let content = read_generated_man_page(page);
            assert!(content.contains("EXIT STATUS"), "{page} must contain EXIT STATUS section");
            assert!(content.contains(".SH FILES"), "{page} must contain FILES section");
            assert!(content.contains(".SH EXAMPLES"), "{page} must contain EXAMPLES section");
            assert!(content.contains("SEE ALSO"), "{page} must contain SEE ALSO section");
        }
    }

    /// AC: netfyr-history.1 OPTIONS lists --count/-n and --since flags.
    #[test]
    fn test_netfyr_history_1_options_lists_key_flags() {
        let content = read_generated_man_page("netfyr-history.1");
        let options_start = content.find(".SH OPTIONS").expect("OPTIONS section must exist in netfyr-history.1");
        let options = &content[options_start..];
        assert!(
            options.contains("since"),
            "netfyr-history.1 OPTIONS must list --since"
        );
        assert!(
            options.contains("count") || options.contains("-n"),
            "netfyr-history.1 OPTIONS must list --count/-n"
        );
    }

    /// AC: netfyr-revert.1 OPTIONS lists --dry-run.
    #[test]
    fn test_netfyr_revert_1_options_lists_dry_run() {
        let content = read_generated_man_page("netfyr-revert.1");
        let options_start = content.find(".SH OPTIONS").expect("OPTIONS section must exist in netfyr-revert.1");
        let options = &content[options_start..];
        assert!(
            options.contains("dry-run") || options.contains("dry\\-run"),
            "netfyr-revert.1 OPTIONS must list --dry-run"
        );
    }

    // ── netfyr-examples.7 existence and content ───────────────────────────────

    fn read_examples_man_page() -> String {
        read_generated_man_page("netfyr-examples.7")
    }

    /// AC: Examples man page exists.
    #[test]
    fn test_examples_7_exists() {
        assert!(man_page_path_exists("netfyr-examples.7"), "man/netfyr-examples.7 must exist");
    }

    /// AC: Examples man page NAME section contains "netfyr-examples".
    #[test]
    fn test_examples_7_name_section_contains_netfyr_examples() {
        let content = read_examples_man_page();
        assert!(content.contains(".SH NAME"), "man/netfyr-examples.7 must have a NAME section");
        let name_start = content.find(".SH NAME").unwrap();
        let after_name = content[name_start..].find("\n.SH").map(|i| name_start + i).unwrap_or(content.len());
        let name_section = &content[name_start..after_name];
        assert!(
            name_section.contains("netfyr") && name_section.contains("examples"),
            "NAME section must identify this as the netfyr-examples page; got:\n{name_section}"
        );
    }

    /// AC: Examples man page TH header declares section 7.
    #[test]
    fn test_examples_7_is_section_7() {
        let content = read_examples_man_page();
        assert!(
            content.contains(".TH") && content.contains(" 7 "),
            "man/netfyr-examples.7 header (.TH) must declare section 7"
        );
    }

    /// AC: Examples man page is hand-maintained (contains the required comment).
    #[test]
    fn test_examples_7_has_hand_maintained_marker() {
        let content = read_examples_man_page();
        let lower = content.to_lowercase();
        assert!(
            lower.contains("hand") || lower.contains("maintained") || lower.contains("do not"),
            "man/netfyr-examples.7 must include a comment noting it is maintained by hand"
        );
    }

    /// AC: Examples man page covers "Static IP on a single interface" scenario.
    #[test]
    fn test_examples_7_has_static_ip_section() {
        let content = read_examples_man_page();
        let lower = content.to_lowercase();
        assert!(
            lower.contains("static ip") || lower.contains("static"),
            "man/netfyr-examples.7 must include a static IP example scenario"
        );
        // The static IP example should show the type, name, and addresses fields
        assert!(
            content.contains("type: ethernet"),
            "static IP example must include 'type: ethernet'"
        );
        assert!(
            content.contains("addresses"),
            "static IP example must include 'addresses' field"
        );
    }

    /// AC: Examples man page covers "Multiple interfaces in one file" scenario.
    #[test]
    fn test_examples_7_has_multiple_interfaces_section() {
        let content = read_examples_man_page();
        let lower = content.to_lowercase();
        assert!(
            lower.contains("multiple") || lower.contains("multi"),
            "man/netfyr-examples.7 must include a multiple-interfaces scenario"
        );
        // Should show the YAML document separator
        assert!(
            content.contains("---"),
            "multiple-interfaces example must include the YAML '---' document separator"
        );
    }

    /// AC: Examples man page covers "DHCP on an interface" scenario.
    #[test]
    fn test_examples_7_has_dhcp_section() {
        let content = read_examples_man_page();
        let lower = content.to_lowercase();
        assert!(
            lower.contains("dhcp"),
            "man/netfyr-examples.7 must include a DHCP scenario"
        );
        assert!(
            content.contains("factory: dhcpv4") || content.contains("dhcpv4"),
            "DHCP example must show dhcpv4 factory"
        );
    }

    /// AC: Examples man page covers "Mixed static and DHCP" scenario.
    #[test]
    fn test_examples_7_has_mixed_static_dhcp_section() {
        let content = read_examples_man_page();
        let lower = content.to_lowercase();
        assert!(
            lower.contains("mixed") || (lower.contains("static") && lower.contains("dhcp")),
            "man/netfyr-examples.7 must include a mixed static-and-DHCP scenario"
        );
    }

    /// AC: Examples man page covers "Priority override" scenario.
    #[test]
    fn test_examples_7_has_priority_override_section() {
        let content = read_examples_man_page();
        let lower = content.to_lowercase();
        assert!(
            lower.contains("priority"),
            "man/netfyr-examples.7 must include a priority override scenario"
        );
        assert!(
            content.contains("priority: 200") || content.contains("priority: 100"),
            "priority override example must show concrete priority values"
        );
    }

    /// AC: Examples man page covers "Selecting by driver" scenario.
    #[test]
    fn test_examples_7_has_selecting_by_driver_section() {
        let content = read_examples_man_page();
        let lower = content.to_lowercase();
        assert!(
            lower.contains("driver"),
            "man/netfyr-examples.7 must include a selecting-by-driver scenario"
        );
        assert!(
            content.contains("driver: ixgbe") || content.contains("driver:"),
            "driver example must show a concrete driver selector"
        );
    }

    /// AC: Examples man page covers "Dry-run workflow" scenario.
    #[test]
    fn test_examples_7_has_dry_run_workflow_section() {
        let content = read_examples_man_page();
        assert!(
            content.contains("dry") || content.contains("dry\\-run"),
            "man/netfyr-examples.7 must include a dry-run workflow scenario"
        );
    }

    /// AC: Each scenario section in examples.7 contains a copy-pasteable YAML example (.nf block).
    #[test]
    fn test_examples_7_sections_have_yaml_examples() {
        let content = read_examples_man_page();
        let nf_count = content.matches(".nf").count();
        // The spec requires at least 7 distinct scenarios, each with a YAML block.
        // Some scenarios (mixed, priority) have multiple files so more than 7 .nf blocks.
        assert!(
            nf_count >= 7,
            "man/netfyr-examples.7 must have at least 7 YAML example (.nf) blocks (one per scenario); found {nf_count}"
        );
    }

    /// AC: examples.7 SEE ALSO references the main netfyr man pages.
    #[test]
    fn test_examples_7_see_also_references_main_pages() {
        let content = read_examples_man_page();
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO section must exist in netfyr-examples.7");
        let see_also = &content[see_also_start..];
        assert!(
            see_also.contains("netfyr") && (see_also.contains("(1)") || see_also.contains(" 1)")),
            "netfyr-examples.7 SEE ALSO must reference netfyr(1)"
        );
        assert!(
            see_also.contains("netfyr.yaml") && (see_also.contains("(5)") || see_also.contains(" 5)")),
            "netfyr-examples.7 SEE ALSO must reference netfyr.yaml(5)"
        );
    }

    // ── EXAMPLES section — history subcommand ────────────────────────────────

    /// AC: history EXAMPLES section header is emitted.
    #[test]
    fn test_history_examples_section_header_present() {
        let out = render(|buf| append_examples(buf, Some("history")));
        assert!(out.contains(".SH EXAMPLES"), "EXAMPLES .SH header must be present for history");
    }

    /// AC: history EXAMPLES must contain at least two real-world usage examples.
    #[test]
    fn test_history_examples_has_at_least_two_nf_blocks() {
        let out = render(|buf| append_examples(buf, Some("history")));
        let nf_count = out.matches(".nf").count();
        assert!(
            nf_count >= 2,
            "history EXAMPLES must contain at least 2 usage examples (.nf blocks); found {nf_count}"
        );
    }

    /// AC: history EXAMPLES must mention --since flag.
    #[test]
    fn test_history_examples_includes_since_flag() {
        let out = render(|buf| append_examples(buf, Some("history")));
        assert!(
            out.contains("since"),
            "history EXAMPLES must show a --since usage example"
        );
    }

    // ── EXAMPLES section — revert subcommand ─────────────────────────────────

    /// AC: revert EXAMPLES section header is emitted.
    #[test]
    fn test_revert_examples_section_header_present() {
        let out = render(|buf| append_examples(buf, Some("revert")));
        assert!(out.contains(".SH EXAMPLES"), "EXAMPLES .SH header must be present for revert");
    }

    /// AC: revert EXAMPLES must contain at least two real-world usage examples.
    #[test]
    fn test_revert_examples_has_at_least_two_nf_blocks() {
        let out = render(|buf| append_examples(buf, Some("revert")));
        let nf_count = out.matches(".nf").count();
        assert!(
            nf_count >= 2,
            "revert EXAMPLES must contain at least 2 usage examples (.nf blocks); found {nf_count}"
        );
    }

    /// AC: revert EXAMPLES must include a --dry-run usage example.
    #[test]
    fn test_revert_examples_includes_dry_run_usage() {
        let out = render(|buf| append_examples(buf, Some("revert")));
        assert!(
            out.contains("--dry-run") || out.contains("dry"),
            "revert EXAMPLES must show a --dry-run usage example"
        );
    }

    // ── EXAMPLES section — show subcommand ───────────────────────────────────

    /// AC: show EXAMPLES section header is emitted.
    #[test]
    fn test_show_examples_section_header_present() {
        let out = render(|buf| append_examples(buf, Some("show")));
        assert!(out.contains(".SH EXAMPLES"), "EXAMPLES .SH header must be present for show");
    }

    /// AC: show EXAMPLES must contain at least two real-world usage examples.
    #[test]
    fn test_show_examples_has_at_least_two_nf_blocks() {
        let out = render(|buf| append_examples(buf, Some("show")));
        let nf_count = out.matches(".nf").count();
        assert!(
            nf_count >= 2,
            "show EXAMPLES must contain at least 2 usage examples (.nf blocks); found {nf_count}"
        );
    }

    // ── EXAMPLES section — diagnose subcommand ────────────────────────────────

    /// AC: diagnose EXAMPLES section header is emitted.
    #[test]
    fn test_diagnose_examples_section_header_present() {
        let out = render(|buf| append_examples(buf, Some("diagnose")));
        assert!(out.contains(".SH EXAMPLES"), "EXAMPLES .SH header must be present for diagnose");
    }

    /// AC: diagnose EXAMPLES must contain at least two real-world usage examples.
    #[test]
    fn test_diagnose_examples_has_at_least_two_nf_blocks() {
        let out = render(|buf| append_examples(buf, Some("diagnose")));
        let nf_count = out.matches(".nf").count();
        assert!(
            nf_count >= 2,
            "diagnose EXAMPLES must contain at least 2 usage examples (.nf blocks); found {nf_count}"
        );
    }

    // ── EXAMPLES section — completions subcommand ─────────────────────────────

    /// AC: completions EXAMPLES section header is emitted.
    #[test]
    fn test_completions_examples_section_header_present() {
        let out = render(|buf| append_examples(buf, Some("completions")));
        assert!(out.contains(".SH EXAMPLES"), "EXAMPLES .SH header must be present for completions");
    }

    /// AC: completions EXAMPLES must contain at least two real-world usage examples.
    #[test]
    fn test_completions_examples_has_at_least_two_nf_blocks() {
        let out = render(|buf| append_examples(buf, Some("completions")));
        let nf_count = out.matches(".nf").count();
        assert!(
            nf_count >= 2,
            "completions EXAMPLES must contain at least 2 usage examples (.nf blocks); found {nf_count}"
        );
    }

    // ── ENVIRONMENT section ───────────────────────────────────────────────────

    /// AC: ENVIRONMENT section header is emitted.
    #[test]
    fn test_environment_section_header_present() {
        let out = render(append_environment);
        assert!(
            out.contains(".SH ENVIRONMENT"),
            "ENVIRONMENT .SH header must be present"
        );
    }

    /// AC: ENVIRONMENT section documents NO_COLOR.
    #[test]
    fn test_environment_section_documents_no_color() {
        let out = render(append_environment);
        assert!(
            out.contains("NO_COLOR"),
            "ENVIRONMENT section must document NO_COLOR"
        );
    }

    // ── SEE ALSO completeness — apply and query ───────────────────────────────

    /// AC: apply SEE ALSO must cross-reference netfyr-history(1).
    #[test]
    fn test_see_also_apply_references_netfyr_history_1() {
        let out = render(|buf| append_see_also(buf, Some("apply")));
        assert!(
            out.contains("netfyr-history (1)") || out.contains("netfyr-history(1)"),
            "apply SEE ALSO must reference netfyr-history(1); got:\n{out}"
        );
    }

    /// AC: apply SEE ALSO must cross-reference netfyr-revert(1).
    #[test]
    fn test_see_also_apply_references_netfyr_revert_1() {
        let out = render(|buf| append_see_also(buf, Some("apply")));
        assert!(
            out.contains("netfyr-revert (1)") || out.contains("netfyr-revert(1)"),
            "apply SEE ALSO must reference netfyr-revert(1); got:\n{out}"
        );
    }

    /// AC: query SEE ALSO must cross-reference netfyr-history(1).
    #[test]
    fn test_see_also_query_references_netfyr_history_1() {
        let out = render(|buf| append_see_also(buf, Some("query")));
        assert!(
            out.contains("netfyr-history (1)") || out.contains("netfyr-history(1)"),
            "query SEE ALSO must reference netfyr-history(1); got:\n{out}"
        );
    }

    /// AC: query SEE ALSO must cross-reference netfyr-revert(1).
    #[test]
    fn test_see_also_query_references_netfyr_revert_1() {
        let out = render(|buf| append_see_also(buf, Some("query")));
        assert!(
            out.contains("netfyr-revert (1)") || out.contains("netfyr-revert(1)"),
            "query SEE ALSO must reference netfyr-revert(1); got:\n{out}"
        );
    }

    // ── Generated netfyr-show.1 content tests ─────────────────────────────────

    /// AC: man/netfyr-show.1 must exist.
    #[test]
    fn test_generated_netfyr_show_1_exists() {
        assert!(man_page_path_exists("netfyr-show.1"), "man/netfyr-show.1 must exist (run `cargo xtask man`)");
    }

    /// AC: netfyr-show.1 OPTIONS section lists --output.
    #[test]
    fn test_netfyr_show_1_options_lists_output() {
        let content = read_generated_man_page("netfyr-show.1");
        let options_start = content.find(".SH OPTIONS").expect("OPTIONS section must exist in netfyr-show.1");
        let options = &content[options_start..];
        assert!(
            options.contains("output"),
            "netfyr-show.1 OPTIONS must list --output; OPTIONS section:\n{options}"
        );
    }

    /// AC: netfyr-show.1 EXAMPLES has at least two usage examples.
    #[test]
    fn test_netfyr_show_1_examples_has_at_least_two_examples() {
        let content = read_generated_man_page("netfyr-show.1");
        let ex_start = content.find(".SH EXAMPLES").expect("EXAMPLES section must exist in netfyr-show.1");
        let ex = &content[ex_start..];
        let nf_count = ex.matches(".nf").count();
        assert!(
            nf_count >= 2,
            "netfyr-show.1 EXAMPLES must contain at least 2 usage examples (.nf blocks); found {nf_count}"
        );
    }

    /// AC: netfyr-show.1 EXIT STATUS documents codes 0 and 1.
    #[test]
    fn test_netfyr_show_1_exit_status_documents_codes_0_and_1() {
        let content = read_generated_man_page("netfyr-show.1");
        let es_start = content.find("EXIT STATUS").expect("EXIT STATUS section must exist in netfyr-show.1");
        let es = &content[es_start..];
        assert!(es.contains(".B 0") || es.contains("\\fB0\\fR"), "netfyr-show.1 EXIT STATUS must document code 0");
        assert!(es.contains(".B 1") || es.contains("\\fB1\\fR"), "netfyr-show.1 EXIT STATUS must document code 1");
    }

    /// AC: netfyr-show.1 SEE ALSO references netfyr(1) and netfyr-daemon(8).
    #[test]
    fn test_netfyr_show_1_see_also_references_netfyr_1_and_daemon_8() {
        let content = read_generated_man_page("netfyr-show.1");
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO section must exist in netfyr-show.1");
        let see_also = &content[see_also_start..];
        assert!(
            see_also.contains("netfyr (1)") || see_also.contains("netfyr(1)"),
            "netfyr-show.1 SEE ALSO must reference netfyr(1)"
        );
        assert!(
            see_also.contains("netfyr-daemon (8)") || see_also.contains("netfyr-daemon(8)"),
            "netfyr-show.1 SEE ALSO must reference netfyr-daemon(8)"
        );
    }

    /// AC: All generated pages have an ENVIRONMENT section with NO_COLOR.
    #[test]
    fn test_all_generated_pages_have_environment_section() {
        let pages = [
            "netfyr.1",
            "netfyr-apply.1",
            "netfyr-query.1",
            "netfyr-history.1",
            "netfyr-revert.1",
            "netfyr-show.1",
            "netfyr-diagnose.1",
            "netfyr-completions.1",
        ];
        for page in pages {
            let content = read_generated_man_page(page);
            assert!(
                content.contains("ENVIRONMENT"),
                "{page} must contain an ENVIRONMENT section"
            );
            assert!(
                content.contains("NO_COLOR"),
                "{page} ENVIRONMENT section must document NO_COLOR"
            );
        }
    }

    // ── netfyr-examples.7 — missing scenario coverage ────────────────────────

    /// AC: Examples man page covers "Investigating changes with history" scenario.
    #[test]
    fn test_examples_7_has_investigating_changes_with_history_section() {
        let content = read_examples_man_page();
        let lower = content.to_lowercase();
        assert!(
            lower.contains("investigat") || lower.contains("history"),
            "man/netfyr-examples.7 must include an 'Investigating changes with history' scenario"
        );
        // The section must show netfyr history command usage
        assert!(
            content.contains("netfyr history") || content.contains("netfyr history"),
            "investigating-history section must show `netfyr history` command"
        );
    }

    /// AC: Examples man page covers "External change detection" scenario.
    #[test]
    fn test_examples_7_has_external_change_detection_section() {
        let content = read_examples_man_page();
        let lower = content.to_lowercase();
        assert!(
            lower.contains("external change"),
            "man/netfyr-examples.7 must include an 'External change detection' scenario"
        );
    }

    /// AC: External change detection example shows a policy must exist for the interface first.
    #[test]
    fn test_examples_7_external_change_detection_requires_policy_first() {
        let content = read_examples_man_page();
        let start = content
            .find("EXTERNAL CHANGE")
            .expect("EXTERNAL CHANGE section must exist in netfyr-examples.7");
        let section = &content[start..];
        // The policy file that declares the interface must appear before the apply/start steps
        assert!(
            section.contains("type: ethernet") || section.contains("kind: policy"),
            "EXTERNAL CHANGE DETECTION section must show creating a policy for the interface first"
        );
        assert!(
            section.contains("netfyr apply") || section.contains("netfyr-apply"),
            "EXTERNAL CHANGE DETECTION section must show applying the policy"
        );
    }

    /// AC: External change detection example shows the complete workflow (policy → apply → external change → history).
    #[test]
    fn test_examples_7_external_change_detection_shows_complete_workflow() {
        let content = read_examples_man_page();
        let start = content
            .find("EXTERNAL CHANGE")
            .expect("EXTERNAL CHANGE section must exist in netfyr-examples.7");
        let section = &content[start..];
        // Should show ip(8) or some external tool making a change
        assert!(
            section.contains("ip link") || section.contains("ip "),
            "EXTERNAL CHANGE DETECTION must show an external tool (e.g., ip) making a change"
        );
        // Should show netfyr history to observe the recorded change
        assert!(
            section.contains("netfyr history") || section.contains("history"),
            "EXTERNAL CHANGE DETECTION must show using netfyr history to observe the change"
        );
    }

    /// AC: External change detection section in examples.7 explains managed-only limitation.
    #[test]
    fn test_examples_7_external_change_detection_explains_managed_only() {
        let content = read_examples_man_page();
        let start = content
            .find("EXTERNAL CHANGE")
            .expect("EXTERNAL CHANGE section must exist in netfyr-examples.7");
        let section = &content[start..];
        let lower = section.to_lowercase();
        assert!(
            lower.contains("policy") && (lower.contains("monitor") || lower.contains("track")),
            "EXTERNAL CHANGE DETECTION section must explain that a policy is required for monitoring"
        );
    }

    /// AC: Examples man page covers "Reverting to a previous state" scenario.
    #[test]
    fn test_examples_7_has_reverting_to_previous_state_section() {
        let content = read_examples_man_page();
        let lower = content.to_lowercase();
        assert!(
            lower.contains("revert"),
            "man/netfyr-examples.7 must include a 'Reverting to a previous state' scenario"
        );
        // The section must show netfyr revert command
        assert!(
            content.contains("netfyr revert"),
            "reverting section must show `netfyr revert` command"
        );
    }

    /// AC: Reverting section in examples.7 shows dry-run first, then apply.
    #[test]
    fn test_examples_7_reverting_section_shows_dry_run_then_apply() {
        let content = read_examples_man_page();
        let start = content
            .find("REVERTING")
            .expect("REVERTING section must exist in netfyr-examples.7");
        let section = &content[start..];
        assert!(
            section.contains("dry") || section.contains("dry\\-run"),
            "REVERTING section must show --dry-run before actually reverting"
        );
        assert!(
            section.contains("netfyr revert"),
            "REVERTING section must show the actual netfyr revert command"
        );
    }

    // ── netfyr-daemon(8) — journal rotation and retention ────────────────────

    /// AC: JOURNAL section documents rotation thresholds (entries and size).
    #[test]
    fn test_daemon_8_journal_documents_rotation_and_retention() {
        let content = read_daemon_man_page();
        let start = content.find("JOURNAL").expect("JOURNAL section must exist");
        let section = &content[start..];
        // Rotation at 10,000 entries
        assert!(
            section.contains("10,000") || section.contains("10000"),
            "JOURNAL section must mention the 10,000-entry rotation threshold"
        );
        // Rotation at 50 MB
        assert!(
            section.contains("50") && (section.contains("MB") || section.contains("mb") || section.contains("52428800")),
            "JOURNAL section must mention the 50 MB rotation threshold"
        );
        // Retention (90 days)
        assert!(
            section.contains("90") && (section.contains("day") || section.contains("retain")),
            "JOURNAL section must document the 90-day retention policy"
        );
    }

    /// AC: JOURNAL section documents that rotated files are gzip-compressed.
    #[test]
    fn test_daemon_8_journal_documents_compression() {
        let content = read_daemon_man_page();
        let start = content.find("JOURNAL").expect("JOURNAL section must exist");
        let section = &content[start..];
        assert!(
            section.contains("gzip") || section.contains("compress"),
            "JOURNAL section must document that rotated files are gzip-compressed"
        );
    }

    /// AC: JOURNAL section documents the default journal path.
    #[test]
    fn test_daemon_8_journal_documents_default_path() {
        let content = read_daemon_man_page();
        let start = content.find("JOURNAL").expect("JOURNAL section must exist");
        let section = &content[start..];
        assert!(
            section.contains("/var/lib/netfyr/journal/"),
            "JOURNAL section must document the default journal path /var/lib/netfyr/journal/"
        );
    }

    // ── Generated pages for history and revert have required sections ─────────

    /// AC: netfyr-history.1 EXAMPLES contains at least two usage examples.
    #[test]
    fn test_netfyr_history_1_examples_has_at_least_two_examples() {
        let content = read_generated_man_page("netfyr-history.1");
        let ex_start = content
            .find(".SH EXAMPLES")
            .expect("EXAMPLES section must exist in netfyr-history.1");
        let ex = &content[ex_start..];
        let nf_count = ex.matches(".nf").count();
        assert!(
            nf_count >= 2,
            "netfyr-history.1 EXAMPLES must contain at least 2 usage examples (.nf blocks); found {nf_count}"
        );
    }

    /// AC: netfyr-revert.1 EXAMPLES contains at least two usage examples.
    #[test]
    fn test_netfyr_revert_1_examples_has_at_least_two_examples() {
        let content = read_generated_man_page("netfyr-revert.1");
        let ex_start = content
            .find(".SH EXAMPLES")
            .expect("EXAMPLES section must exist in netfyr-revert.1");
        let ex = &content[ex_start..];
        let nf_count = ex.matches(".nf").count();
        assert!(
            nf_count >= 2,
            "netfyr-revert.1 EXAMPLES must contain at least 2 usage examples (.nf blocks); found {nf_count}"
        );
    }

    /// AC: netfyr-diagnose.1 EXAMPLES contains at least two usage examples.
    #[test]
    fn test_netfyr_diagnose_1_examples_has_at_least_two_examples() {
        let content = read_generated_man_page("netfyr-diagnose.1");
        let ex_start = content
            .find(".SH EXAMPLES")
            .expect("EXAMPLES section must exist in netfyr-diagnose.1");
        let ex = &content[ex_start..];
        let nf_count = ex.matches(".nf").count();
        assert!(
            nf_count >= 2,
            "netfyr-diagnose.1 EXAMPLES must contain at least 2 usage examples (.nf blocks); found {nf_count}"
        );
    }

    /// AC: netfyr-completions.1 EXAMPLES contains at least two usage examples.
    #[test]
    fn test_netfyr_completions_1_examples_has_at_least_two_examples() {
        let content = read_generated_man_page("netfyr-completions.1");
        let ex_start = content
            .find(".SH EXAMPLES")
            .expect("EXAMPLES section must exist in netfyr-completions.1");
        let ex = &content[ex_start..];
        let nf_count = ex.matches(".nf").count();
        assert!(
            nf_count >= 2,
            "netfyr-completions.1 EXAMPLES must contain at least 2 usage examples (.nf blocks); found {nf_count}"
        );
    }

    // ── netfyr-daemon(8) references in SEE ALSO ──────────────────────────────

    /// AC: Top-level SEE ALSO references netfyr-daemon(8).
    #[test]
    fn test_see_also_toplevel_references_netfyr_daemon_8() {
        let out = render(|buf| append_see_also(buf, None));
        assert!(
            out.contains("netfyr-daemon (8)") || out.contains("netfyr-daemon(8)"),
            "top-level SEE ALSO must reference netfyr-daemon(8); got:\n{out}"
        );
    }

    /// AC: apply SEE ALSO references netfyr-daemon(8).
    #[test]
    fn test_see_also_apply_references_netfyr_daemon_8() {
        let out = render(|buf| append_see_also(buf, Some("apply")));
        assert!(
            out.contains("netfyr-daemon (8)") || out.contains("netfyr-daemon(8)"),
            "apply SEE ALSO must reference netfyr-daemon(8); got:\n{out}"
        );
    }

    /// AC: query SEE ALSO references netfyr-daemon(8).
    #[test]
    fn test_see_also_query_references_netfyr_daemon_8() {
        let out = render(|buf| append_see_also(buf, Some("query")));
        assert!(
            out.contains("netfyr-daemon (8)") || out.contains("netfyr-daemon(8)"),
            "query SEE ALSO must reference netfyr-daemon(8); got:\n{out}"
        );
    }

    // ── netfyr-daemon.8 content tests ─────────────────────────────────────────

    fn read_daemon_man_page() -> String {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let man_path = manifest_dir.join("../man/netfyr-daemon.8");
        std::fs::read_to_string(&man_path)
            .unwrap_or_else(|e| panic!("Failed to read man/netfyr-daemon.8: {e}"))
    }

    /// AC: Daemon man page exists.
    #[test]
    fn test_daemon_8_exists() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let path = manifest_dir.join("../man/netfyr-daemon.8");
        assert!(path.exists(), "man/netfyr-daemon.8 must exist");
    }

    /// AC: Daemon man page TH header declares section 8.
    #[test]
    fn test_daemon_8_is_section_8() {
        let content = read_daemon_man_page();
        assert!(
            content.contains(".TH") && content.contains(" 8 "),
            "man/netfyr-daemon.8 header (.TH) must declare section 8"
        );
    }

    /// AC: Daemon man page NAME section contains "netfyr-daemon".
    #[test]
    fn test_daemon_8_name_section_contains_netfyr_daemon() {
        let content = read_daemon_man_page();
        assert!(content.contains(".SH NAME"), "man/netfyr-daemon.8 must have a NAME section");
        let lower = content.to_lowercase();
        assert!(
            lower.contains("netfyr") && lower.contains("daemon"),
            "NAME section must contain 'netfyr-daemon'"
        );
    }

    /// AC: Daemon man page is hand-maintained (contains the required comment).
    #[test]
    fn test_daemon_8_has_hand_maintained_marker() {
        let content = read_daemon_man_page();
        let lower = content.to_lowercase();
        assert!(
            lower.contains("hand") || lower.contains("maintained") || lower.contains("do not"),
            "man/netfyr-daemon.8 must include a comment noting it is maintained by hand"
        );
    }

    /// AC: Daemon man page has EXTERNAL CHANGE DETECTION section.
    #[test]
    fn test_daemon_8_has_external_change_detection_section() {
        let content = read_daemon_man_page();
        assert!(
            content.contains("EXTERNAL CHANGE DETECTION"),
            "man/netfyr-daemon.8 must have an EXTERNAL CHANGE DETECTION section"
        );
    }

    /// AC: External change detection section documents managed-only monitoring.
    #[test]
    fn test_daemon_8_external_change_documents_managed_only() {
        let content = read_daemon_man_page();
        let start = content.find("EXTERNAL CHANGE DETECTION").expect("section must exist");
        let section = &content[start..];
        let lower = section.to_lowercase();
        assert!(
            lower.contains("managed"),
            "EXTERNAL CHANGE DETECTION must explain that only managed interfaces are monitored"
        );
    }

    /// AC: External change detection section documents the 500ms debounce window.
    #[test]
    fn test_daemon_8_external_change_documents_debounce() {
        let content = read_daemon_man_page();
        let start = content.find("EXTERNAL CHANGE DETECTION").expect("section must exist");
        let section = &content[start..];
        assert!(
            section.contains("500"),
            "EXTERNAL CHANGE DETECTION must mention the 500ms debounce window"
        );
    }

    /// AC: External change detection section documents no automatic re-reconciliation.
    #[test]
    fn test_daemon_8_external_change_documents_no_rereconciliation() {
        let content = read_daemon_man_page();
        let start = content.find("EXTERNAL CHANGE DETECTION").expect("section must exist");
        let section = &content[start..];
        let lower = section.to_lowercase();
        assert!(
            lower.contains("does not") || lower.contains("no automatic"),
            "EXTERNAL CHANGE DETECTION must document that the daemon does not re-apply state"
        );
    }

    /// AC: External change detection section documents monitored properties.
    #[test]
    fn test_daemon_8_external_change_documents_monitored_properties() {
        let content = read_daemon_man_page();
        let start = content.find("EXTERNAL CHANGE DETECTION").expect("section must exist");
        let section = &content[start..];
        assert!(section.contains("mtu"), "section must mention mtu");
        assert!(section.contains("state"), "section must mention state");
        assert!(section.contains("flags"), "section must mention flags");
        let lower = section.to_lowercase();
        assert!(
            lower.contains("ipv4") || lower.contains("address"),
            "section must mention IPv4 addresses"
        );
    }

    /// AC: Daemon man page has JOURNAL section.
    #[test]
    fn test_daemon_8_has_journal_section() {
        let content = read_daemon_man_page();
        assert!(
            content.contains("JOURNAL"),
            "man/netfyr-daemon.8 must have a JOURNAL section"
        );
    }

    /// AC: JOURNAL section documents NDJSON format.
    #[test]
    fn test_daemon_8_journal_documents_ndjson() {
        let content = read_daemon_man_page();
        let start = content.find("JOURNAL").expect("JOURNAL section must exist");
        let section = &content[start..];
        assert!(
            section.contains("NDJSON") || section.contains("ndjson"),
            "JOURNAL section must mention NDJSON format"
        );
    }

    /// AC: JOURNAL section references netfyr-history and netfyr-revert.
    #[test]
    fn test_daemon_8_journal_references_history_and_revert() {
        let content = read_daemon_man_page();
        let start = content.find("JOURNAL").expect("JOURNAL section must exist");
        let section = &content[start..];
        assert!(
            section.contains("netfyr history") || section.contains("netfyr-history"),
            "JOURNAL section must reference netfyr-history"
        );
        assert!(
            section.contains("netfyr revert") || section.contains("netfyr-revert"),
            "JOURNAL section must reference netfyr-revert"
        );
    }

    /// AC: Daemon man page has ENVIRONMENT section.
    #[test]
    fn test_daemon_8_has_environment_section() {
        let content = read_daemon_man_page();
        assert!(
            content.contains("ENVIRONMENT"),
            "man/netfyr-daemon.8 must have an ENVIRONMENT section"
        );
    }

    /// AC: ENVIRONMENT section lists all six environment variables.
    #[test]
    fn test_daemon_8_environment_lists_all_variables() {
        let content = read_daemon_man_page();
        let start = content.find("ENVIRONMENT").expect("ENVIRONMENT section must exist");
        let section = &content[start..];
        assert!(section.contains("NETFYR_SOCKET_PATH"), "ENVIRONMENT must list NETFYR_SOCKET_PATH");
        assert!(section.contains("NETFYR_POLICY_DIR"), "ENVIRONMENT must list NETFYR_POLICY_DIR");
        assert!(section.contains("NETFYR_JOURNAL_DIR"), "ENVIRONMENT must list NETFYR_JOURNAL_DIR");
        assert!(section.contains("NETFYR_JOURNAL_MAX_ENTRIES"), "ENVIRONMENT must list NETFYR_JOURNAL_MAX_ENTRIES");
        assert!(section.contains("NETFYR_JOURNAL_MAX_SIZE"), "ENVIRONMENT must list NETFYR_JOURNAL_MAX_SIZE");
        assert!(section.contains("NETFYR_JOURNAL_RETENTION_DAYS"), "ENVIRONMENT must list NETFYR_JOURNAL_RETENTION_DAYS");
    }

    /// AC: Daemon man page has FILES section.
    #[test]
    fn test_daemon_8_has_files_section() {
        let content = read_daemon_man_page();
        assert!(
            content.contains(".SH FILES"),
            "man/netfyr-daemon.8 must have a FILES section"
        );
    }

    /// AC: Daemon man page has SEE ALSO section.
    #[test]
    fn test_daemon_8_has_see_also_section() {
        let content = read_daemon_man_page();
        assert!(
            content.contains("SEE ALSO"),
            "man/netfyr-daemon.8 must have a SEE ALSO section"
        );
    }

    // ── netfyr.yaml.5 — spec 503 additional coverage ─────────────────────────

    /// AC 503: Man page renders without troff/groff errors.
    /// Runs groff (or nroff) on the file and asserts the exit code is 0.
    /// Skipped automatically if neither groff nor nroff is present on PATH.
    #[test]
    fn test_yaml_man_page_renders_without_groff_errors() {
        use std::process::Command;

        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let man_path = manifest_dir.join("../man/netfyr.yaml.5");

        // Try groff first, then nroff as a fallback.
        for program in &["groff", "nroff"] {
            let which = Command::new("which").arg(program).output();
            if which.map(|o| o.status.success()).unwrap_or(false) {
                let output = Command::new(program)
                    .args(["-man", "-Tutf8"])
                    .arg(&man_path)
                    .output()
                    .unwrap_or_else(|e| panic!("Failed to spawn {program}: {e}"));
                assert!(
                    output.status.success(),
                    "{program} exited with non-zero status rendering man/netfyr.yaml.5:\nstderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let stderr = String::from_utf8_lossy(&output.stderr);
                let warnings: Vec<&str> = stderr
                    .lines()
                    .filter(|l| l.contains("warning") || l.contains("error"))
                    .collect();
                assert!(
                    warnings.is_empty(),
                    "{program} produced warnings/errors rendering man/netfyr.yaml.5:\n{}",
                    warnings.join("\n")
                );
                return;
            }
        }
        // Neither groff nor nroff is available — skip gracefully.
        eprintln!("WARNING: neither groff nor nroff found; skipping troff rendering test");
    }

    /// AC 503: VALUE TYPES section maps YAML strings to netfyr String.
    #[test]
    fn test_yaml_man_page_value_types_maps_string() {
        let content = read_yaml_man_page();
        let vt_start = content.find("VALUE TYPES").expect("VALUE TYPES section must exist");
        let files_start = content.find("\n.SH FILES").expect("FILES section must exist");
        let vt_section = &content[vt_start..files_start];
        assert!(
            vt_section.contains("String"),
            "VALUE TYPES section must map plain YAML strings to netfyr String; section:\n{vt_section}"
        );
    }

    /// AC 503: netfyr.yaml.5 has a SEE ALSO section.
    #[test]
    fn test_yaml_man_page_has_see_also_section() {
        let content = read_yaml_man_page();
        assert!(
            content.contains("SEE ALSO"),
            "man/netfyr.yaml.5 must have a SEE ALSO section"
        );
    }

    /// AC 503: SEE ALSO in netfyr.yaml.5 references netfyr(1).
    #[test]
    fn test_yaml_man_page_see_also_references_netfyr_1() {
        let content = read_yaml_man_page();
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO section must exist");
        let see_also = &content[see_also_start..];
        assert!(
            see_also.contains("netfyr (1)") || see_also.contains("netfyr(1)"),
            "man/netfyr.yaml.5 SEE ALSO must reference netfyr(1)"
        );
    }

    /// AC 503: SEE ALSO in netfyr.yaml.5 references netfyr-apply(1).
    /// Troff source may use \- for the hyphen, so we check both forms.
    #[test]
    fn test_yaml_man_page_see_also_references_netfyr_apply_1() {
        let content = read_yaml_man_page();
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO section must exist");
        let see_also = &content[see_also_start..];
        assert!(
            see_also.contains("netfyr-apply") || see_also.contains("netfyr\\-apply"),
            "man/netfyr.yaml.5 SEE ALSO must reference netfyr-apply(1)"
        );
    }

    /// AC 503: SEE ALSO in netfyr.yaml.5 references netfyr-daemon(8).
    /// Troff source may use \- for the hyphen, so we check both forms.
    #[test]
    fn test_yaml_man_page_see_also_references_netfyr_daemon_8() {
        let content = read_yaml_man_page();
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO section must exist");
        let see_also = &content[see_also_start..];
        assert!(
            see_also.contains("netfyr-daemon") || see_also.contains("netfyr\\-daemon"),
            "man/netfyr.yaml.5 SEE ALSO must reference netfyr-daemon(8)"
        );
    }

    /// AC 503: SEE ALSO in netfyr.yaml.5 references netfyr-examples(7).
    /// Troff source may use \- for the hyphen, so we check both forms.
    #[test]
    fn test_yaml_man_page_see_also_references_netfyr_examples_7() {
        let content = read_yaml_man_page();
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO section must exist");
        let see_also = &content[see_also_start..];
        assert!(
            see_also.contains("netfyr-examples") || see_also.contains("netfyr\\-examples"),
            "man/netfyr.yaml.5 SEE ALSO must reference netfyr-examples(7)"
        );
    }

    /// AC 503: BARE STATE FORMAT mentions both selector properties and configuration properties.
    #[test]
    fn test_yaml_man_page_bare_state_format_documents_selector_and_config_properties() {
        let content = read_yaml_man_page();
        let bare_start = content.find("BARE STATE FORMAT").expect("BARE STATE FORMAT section must exist");
        let policy_start = content.find("POLICY FORMAT").expect("POLICY FORMAT section must exist");
        let bare_section = &content[bare_start..policy_start];
        let lower = bare_section.to_lowercase();
        assert!(
            lower.contains("selector"),
            "BARE STATE FORMAT must mention selector properties"
        );
        assert!(
            lower.contains("config") || lower.contains("configuration"),
            "BARE STATE FORMAT must mention configuration properties"
        );
    }

    /// AC 503: BARE STATE FORMAT documents the ethernet entity type is supported.
    #[test]
    fn test_yaml_man_page_bare_state_format_documents_ethernet_type() {
        let content = read_yaml_man_page();
        let bare_start = content.find("BARE STATE FORMAT").expect("BARE STATE FORMAT section must exist");
        let policy_start = content.find("POLICY FORMAT").expect("POLICY FORMAT section must exist");
        let bare_section = &content[bare_start..policy_start];
        assert!(
            bare_section.contains("ethernet"),
            "BARE STATE FORMAT must document 'ethernet' as a supported entity type"
        );
    }

    /// AC 503: POLICY FORMAT documents that priority defaults to 100.
    #[test]
    fn test_yaml_man_page_policy_format_documents_priority_default_100() {
        let content = read_yaml_man_page();
        let policy_start = content.find("POLICY FORMAT").expect("POLICY FORMAT section must exist");
        let multi_start = content.find("MULTI-DOCUMENT").expect("MULTI-DOCUMENT section must exist");
        let policy_section = &content[policy_start..multi_start];
        assert!(
            policy_section.contains("100"),
            "POLICY FORMAT must document the default priority value of 100"
        );
    }

    /// AC 503: POLICY FORMAT documents that `state` and `states` are mutually exclusive.
    #[test]
    fn test_yaml_man_page_policy_format_documents_state_states_mutual_exclusion() {
        let content = read_yaml_man_page();
        let policy_start = content.find("POLICY FORMAT").expect("POLICY FORMAT section must exist");
        let multi_start = content.find("MULTI-DOCUMENT").expect("MULTI-DOCUMENT section must exist");
        let policy_section = &content[policy_start..multi_start];
        let lower = policy_section.to_lowercase();
        assert!(
            lower.contains("mutually exclusive") || lower.contains("mutual"),
            "POLICY FORMAT must note that 'state' and 'states' are mutually exclusive"
        );
    }

    /// AC 503: SELECTORS section documents that all fields are AND-ed (all must match).
    #[test]
    fn test_yaml_man_page_selectors_documents_and_logic() {
        let content = read_yaml_man_page();
        let sel_start = content.find("\n.SH SELECTORS").expect("SELECTORS section must exist");
        let fields_start = content.find("\n.SH FIELDS").expect("FIELDS section must exist");
        let sel_section = &content[sel_start..fields_start];
        let lower = sel_section.to_lowercase();
        assert!(
            lower.contains("and") || lower.contains("all"),
            "SELECTORS section must document that all specified fields must match (AND logic)"
        );
    }

    /// AC 503: FIELDS section documents IPv4 CIDR notation for addresses.
    #[test]
    fn test_yaml_man_page_fields_addresses_documents_cidr_notation() {
        let content = read_yaml_man_page();
        let fields_start = content.find("\n.SH FIELDS").expect("FIELDS section must exist");
        let value_start = content.find("VALUE TYPES").expect("VALUE TYPES section must exist");
        let fields_section = &content[fields_start..value_start];
        assert!(
            fields_section.contains("CIDR") || fields_section.contains("cidr"),
            "FIELDS addresses documentation must mention CIDR notation"
        );
        assert!(
            fields_section.contains("IPv4") || fields_section.contains("ipv4"),
            "FIELDS addresses documentation must mention IPv4"
        );
    }

    /// AC 503: VALUE TYPES section notes that IPv6 is not supported.
    #[test]
    fn test_yaml_man_page_value_types_documents_ipv6_not_supported() {
        let content = read_yaml_man_page();
        let vt_start = content.find("VALUE TYPES").expect("VALUE TYPES section must exist");
        let files_start = content.find("\n.SH FILES").expect("FILES section must exist");
        let vt_section = &content[vt_start..files_start];
        let lower = vt_section.to_lowercase();
        assert!(
            lower.contains("ipv6"),
            "VALUE TYPES section must note that IPv6 is not supported"
        );
    }

    /// AC 503: DESCRIPTION section mentions /etc/netfyr/policies/ as the config directory.
    #[test]
    fn test_yaml_man_page_description_mentions_policies_directory() {
        let content = read_yaml_man_page();
        let desc_start = content.find(".SH DESCRIPTION").expect("DESCRIPTION section must exist");
        let bare_start = content.find("BARE STATE FORMAT").expect("BARE STATE FORMAT must exist");
        let desc_section = &content[desc_start..bare_start];
        assert!(
            desc_section.contains("/etc/netfyr/policies/"),
            "DESCRIPTION must mention /etc/netfyr/policies/ as the config directory"
        );
    }

    /// AC 503: DESCRIPTION section explains that "---" separates multiple documents.
    #[test]
    fn test_yaml_man_page_description_explains_multi_document_separator() {
        let content = read_yaml_man_page();
        let desc_start = content.find(".SH DESCRIPTION").expect("DESCRIPTION section must exist");
        let bare_start = content.find("BARE STATE FORMAT").expect("BARE STATE FORMAT must exist");
        let desc_section = &content[desc_start..bare_start];
        assert!(
            desc_section.contains("---") || desc_section.contains("\\-\\-\\-"),
            "DESCRIPTION must mention the '---' document separator"
        );
    }

    // ── Idempotency and non-overwrite ─────────────────────────────────────────

    /// AC: Regeneration is idempotent — running cargo xtask man twice produces identical output.
    /// AC: examples.7 is not overwritten by generate_man_pages().
    #[test]
    fn test_regeneration_is_idempotent_and_does_not_overwrite_examples_7() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let man_dir = manifest_dir.join("../man");

        let read_file = |name: &str| -> String {
            std::fs::read_to_string(man_dir.join(name))
                .unwrap_or_else(|e| panic!("Failed to read man/{name}: {e}"))
        };

        // Capture state after first generation (the files already exist from `cargo xtask man`).
        let before: Vec<(&str, String)> = vec![
            ("netfyr.1", read_file("netfyr.1")),
            ("netfyr-apply.1", read_file("netfyr-apply.1")),
            ("netfyr-query.1", read_file("netfyr-query.1")),
            ("netfyr-history.1", read_file("netfyr-history.1")),
            ("netfyr-revert.1", read_file("netfyr-revert.1")),
            ("netfyr-show.1", read_file("netfyr-show.1")),
            ("netfyr-diagnose.1", read_file("netfyr-diagnose.1")),
            ("netfyr-completions.1", read_file("netfyr-completions.1")),
            ("netfyr-examples.7", read_file("netfyr-examples.7")),
            ("netfyr-daemon.8", read_file("netfyr-daemon.8")),
        ];

        // Run generation a second time.
        generate_man_pages().expect("generate_man_pages must succeed on second invocation");

        // Verify all files are identical.
        for (name, before_content) in &before {
            let after_content = read_file(name);
            assert_eq!(
                *before_content, after_content,
                "man/{name} must be identical after second `cargo xtask man` run (idempotency)"
            );
        }
    }

    // ── Troff rendering helper ────────────────────────────────────────────────

    /// Run groff/nroff on a man page file and assert it renders without
    /// warnings or errors.  Silently skips if neither tool is on PATH.
    fn assert_man_page_renders_clean(man_path: &std::path::Path) {
        use std::process::Command;
        for program in &["groff", "nroff"] {
            let which = Command::new("which").arg(program).output();
            if which.map(|o| o.status.success()).unwrap_or(false) {
                let output = Command::new(program)
                    .args(["-man", "-Tutf8"])
                    .arg(man_path)
                    .output()
                    .unwrap_or_else(|e| panic!("Failed to spawn {program}: {e}"));
                assert!(
                    output.status.success(),
                    "{program} exited with non-zero status rendering {}:\nstderr: {}",
                    man_path.display(),
                    String::from_utf8_lossy(&output.stderr)
                );
                let stderr = String::from_utf8_lossy(&output.stderr);
                let warnings: Vec<&str> = stderr
                    .lines()
                    .filter(|l| l.contains("warning") || l.contains("error"))
                    .collect();
                assert!(
                    warnings.is_empty(),
                    "{program} produced warnings/errors rendering {}:\n{}",
                    man_path.display(),
                    warnings.join("\n")
                );
                return;
            }
        }
        eprintln!(
            "WARNING: neither groff nor nroff found; skipping render test for {}",
            man_path.display()
        );
    }

    // ── Man page rendering — generated section 1 pages ────────────────────────

    /// AC: Generated netfyr.1 renders without troff warnings or errors.
    #[test]
    fn test_netfyr_1_renders_without_groff_errors() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert_man_page_renders_clean(&manifest_dir.join("../man/netfyr.1"));
    }

    /// AC: Generated netfyr-apply.1 renders without troff warnings or errors.
    #[test]
    fn test_netfyr_apply_1_renders_without_groff_errors() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert_man_page_renders_clean(&manifest_dir.join("../man/netfyr-apply.1"));
    }

    /// AC: Generated netfyr-query.1 renders without troff warnings or errors.
    #[test]
    fn test_netfyr_query_1_renders_without_groff_errors() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert_man_page_renders_clean(&manifest_dir.join("../man/netfyr-query.1"));
    }

    /// AC: Generated netfyr-show.1 renders without troff warnings or errors.
    #[test]
    fn test_netfyr_show_1_renders_without_groff_errors() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert_man_page_renders_clean(&manifest_dir.join("../man/netfyr-show.1"));
    }

    /// AC: Generated netfyr-history.1 renders without troff warnings or errors.
    #[test]
    fn test_netfyr_history_1_renders_without_groff_errors() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert_man_page_renders_clean(&manifest_dir.join("../man/netfyr-history.1"));
    }

    /// AC: Generated netfyr-revert.1 renders without troff warnings or errors.
    #[test]
    fn test_netfyr_revert_1_renders_without_groff_errors() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert_man_page_renders_clean(&manifest_dir.join("../man/netfyr-revert.1"));
    }

    /// AC: Generated netfyr-diagnose.1 renders without troff warnings or errors.
    #[test]
    fn test_netfyr_diagnose_1_renders_without_groff_errors() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert_man_page_renders_clean(&manifest_dir.join("../man/netfyr-diagnose.1"));
    }

    /// AC: Generated netfyr-completions.1 renders without troff warnings or errors.
    #[test]
    fn test_netfyr_completions_1_renders_without_groff_errors() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert_man_page_renders_clean(&manifest_dir.join("../man/netfyr-completions.1"));
    }

    // ── Man page rendering — hand-written pages ───────────────────────────────

    /// AC: Hand-written netfyr-daemon.8 renders without troff warnings or errors.
    #[test]
    fn test_daemon_8_renders_without_groff_errors() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert_man_page_renders_clean(&manifest_dir.join("../man/netfyr-daemon.8"));
    }

    /// AC: Hand-written netfyr-examples.7 renders without troff warnings or errors.
    #[test]
    fn test_examples_7_renders_without_groff_errors() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert_man_page_renders_clean(&manifest_dir.join("../man/netfyr-examples.7"));
    }

    // ── netfyr.1 DESCRIPTION completeness ────────────────────────────────────

    /// AC: Top-level man page DESCRIPTION mentions the "show" subcommand.
    /// Spec: "the DESCRIPTION section mentions apply, query, and show subcommands"
    #[test]
    fn test_netfyr_1_description_mentions_show() {
        let content = read_generated_man_page("netfyr.1");
        let desc_start = content
            .find(".SH DESCRIPTION")
            .expect("DESCRIPTION section must exist in netfyr.1");
        let next_section = content[desc_start + 1..]
            .find("\n.SH ")
            .map(|i| desc_start + 1 + i)
            .unwrap_or(content.len());
        let desc = &content[desc_start..next_section];
        assert!(
            desc.contains("show"),
            "netfyr.1 DESCRIPTION must mention the show subcommand; section:\n{desc}"
        );
    }

    // ── netfyr.1 SEE ALSO — all subcommands must be listed ───────────────────

    /// AC: netfyr.1 SEE ALSO references netfyr-show(1).
    #[test]
    fn test_netfyr_1_see_also_references_netfyr_show_1() {
        let content = read_generated_man_page("netfyr.1");
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO must exist in netfyr.1");
        let see_also = &content[see_also_start..];
        assert!(
            see_also.contains("netfyr-show"),
            "netfyr.1 SEE ALSO must reference netfyr-show(1)"
        );
    }

    /// AC: netfyr.1 SEE ALSO references netfyr-diagnose(1).
    /// Spec: "the SEE ALSO section references all subcommand man pages"
    #[test]
    fn test_netfyr_1_see_also_references_netfyr_diagnose_1() {
        let content = read_generated_man_page("netfyr.1");
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO must exist in netfyr.1");
        let see_also = &content[see_also_start..];
        assert!(
            see_also.contains("netfyr-diagnose"),
            "netfyr.1 SEE ALSO must reference netfyr-diagnose(1)"
        );
    }

    /// AC: netfyr.1 SEE ALSO references netfyr-completions(1).
    /// Spec: "the SEE ALSO section references all subcommand man pages"
    #[test]
    fn test_netfyr_1_see_also_references_netfyr_completions_1() {
        let content = read_generated_man_page("netfyr.1");
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO must exist in netfyr.1");
        let see_also = &content[see_also_start..];
        assert!(
            see_also.contains("netfyr-completions"),
            "netfyr.1 SEE ALSO must reference netfyr-completions(1)"
        );
    }

    // ── append_see_also(None) completeness — unit-level ──────────────────────

    /// AC: append_see_also(None) includes netfyr-show(1).
    #[test]
    fn test_see_also_toplevel_helper_references_netfyr_show_1() {
        let out = render(|buf| append_see_also(buf, None));
        assert!(
            out.contains("netfyr-show (1)") || out.contains("netfyr-show(1)"),
            "top-level SEE ALSO helper must reference netfyr-show(1); got:\n{out}"
        );
    }

    /// AC: append_see_also(None) includes netfyr-diagnose(1).
    /// Spec requires all subcommand pages to be referenced in the top-level page.
    #[test]
    fn test_see_also_toplevel_helper_references_netfyr_diagnose_1() {
        let out = render(|buf| append_see_also(buf, None));
        assert!(
            out.contains("netfyr-diagnose (1)") || out.contains("netfyr-diagnose(1)"),
            "top-level SEE ALSO helper must reference netfyr-diagnose(1); got:\n{out}"
        );
    }

    /// AC: append_see_also(None) includes netfyr-completions(1).
    /// Spec requires all subcommand pages to be referenced in the top-level page.
    #[test]
    fn test_see_also_toplevel_helper_references_netfyr_completions_1() {
        let out = render(|buf| append_see_also(buf, None));
        assert!(
            out.contains("netfyr-completions (1)") || out.contains("netfyr-completions(1)"),
            "top-level SEE ALSO helper must reference netfyr-completions(1); got:\n{out}"
        );
    }

    // ── netfyr-daemon.8 SEE ALSO references ──────────────────────────────────

    /// AC: Daemon man page SEE ALSO references netfyr(1).
    #[test]
    fn test_daemon_8_see_also_references_netfyr_1() {
        let content = read_daemon_man_page();
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO must exist in netfyr-daemon.8");
        let see_also = &content[see_also_start..];
        assert!(
            see_also.contains("netfyr (1)") || see_also.contains("netfyr(1)"),
            "netfyr-daemon.8 SEE ALSO must reference netfyr(1)"
        );
    }

    /// AC: Daemon man page SEE ALSO references netfyr-apply(1).
    #[test]
    fn test_daemon_8_see_also_references_netfyr_apply_1() {
        let content = read_daemon_man_page();
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO must exist in netfyr-daemon.8");
        let see_also = &content[see_also_start..];
        assert!(
            see_also.contains("netfyr-apply") || see_also.contains("netfyr\\-apply"),
            "netfyr-daemon.8 SEE ALSO must reference netfyr-apply(1)"
        );
    }

    /// AC: Daemon man page SEE ALSO references netfyr-history(1).
    #[test]
    fn test_daemon_8_see_also_references_netfyr_history_1() {
        let content = read_daemon_man_page();
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO must exist in netfyr-daemon.8");
        let see_also = &content[see_also_start..];
        assert!(
            see_also.contains("netfyr-history") || see_also.contains("netfyr\\-history"),
            "netfyr-daemon.8 SEE ALSO must reference netfyr-history(1)"
        );
    }

    /// AC: Daemon man page SEE ALSO references netfyr-revert(1).
    #[test]
    fn test_daemon_8_see_also_references_netfyr_revert_1() {
        let content = read_daemon_man_page();
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO must exist in netfyr-daemon.8");
        let see_also = &content[see_also_start..];
        assert!(
            see_also.contains("netfyr-revert") || see_also.contains("netfyr\\-revert"),
            "netfyr-daemon.8 SEE ALSO must reference netfyr-revert(1)"
        );
    }

    // ── netfyr-examples.7 SEE ALSO references ────────────────────────────────

    /// AC: Examples man page SEE ALSO references netfyr-daemon(8).
    #[test]
    fn test_examples_7_see_also_references_netfyr_daemon_8() {
        let content = read_examples_man_page();
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO must exist in netfyr-examples.7");
        let see_also = &content[see_also_start..];
        assert!(
            see_also.contains("netfyr-daemon") || see_also.contains("netfyr\\-daemon"),
            "netfyr-examples.7 SEE ALSO must reference netfyr-daemon(8)"
        );
    }

    /// AC: Examples man page SEE ALSO references netfyr-apply(1).
    #[test]
    fn test_examples_7_see_also_references_netfyr_apply_1() {
        let content = read_examples_man_page();
        let see_also_start = content.find("SEE ALSO").expect("SEE ALSO must exist in netfyr-examples.7");
        let see_also = &content[see_also_start..];
        assert!(
            see_also.contains("netfyr-apply") || see_also.contains("netfyr\\-apply"),
            "netfyr-examples.7 SEE ALSO must reference netfyr-apply(1)"
        );
    }

    // ── netfyr-apply.1 EXAMPLES — dry-run in a named scenario ────────────────

    /// AC: The dry-run usage in apply EXAMPLES shows the policies directory path.
    #[test]
    fn test_apply_examples_dry_run_shows_policies_path() {
        let out = render(|buf| append_examples(buf, Some("apply")));
        // Both examples must be present: the dry-run one should reference a .yaml path.
        assert!(
            out.contains("--dry-run") && out.contains("/etc/netfyr/policies/"),
            "apply EXAMPLES must contain both --dry-run and /etc/netfyr/policies/ in the same section"
        );
    }

    // ── netfyr-daemon.8 DESCRIPTION ──────────────────────────────────────────

    /// AC: Daemon man page DESCRIPTION mentions the Varlink socket.
    #[test]
    fn test_daemon_8_description_mentions_varlink() {
        let content = read_daemon_man_page();
        let desc_start = content.find(".SH DESCRIPTION").expect("DESCRIPTION must exist in netfyr-daemon.8");
        let next_section = content[desc_start + 1..]
            .find("\n.SH ")
            .map(|i| desc_start + 1 + i)
            .unwrap_or(content.len());
        let desc = &content[desc_start..next_section];
        assert!(
            desc.contains("Varlink") || desc.contains("varlink") || desc.contains("socket"),
            "netfyr-daemon.8 DESCRIPTION must mention the Varlink socket"
        );
    }

    /// AC: Daemon man page DESCRIPTION mentions the journal.
    #[test]
    fn test_daemon_8_description_mentions_journal() {
        let content = read_daemon_man_page();
        let desc_start = content.find(".SH DESCRIPTION").expect("DESCRIPTION must exist in netfyr-daemon.8");
        let next_section = content[desc_start + 1..]
            .find("\n.SH ")
            .map(|i| desc_start + 1 + i)
            .unwrap_or(content.len());
        let desc = &content[desc_start..next_section];
        assert!(
            desc.contains("journal") || desc.contains("Journal"),
            "netfyr-daemon.8 DESCRIPTION must mention the journal"
        );
    }

    // ── netfyr-daemon.8 FILES section ────────────────────────────────────────

    /// AC: Daemon man page FILES section lists the Varlink socket path.
    #[test]
    fn test_daemon_8_files_lists_varlink_socket() {
        let content = read_daemon_man_page();
        let files_start = content.find(".SH FILES").expect("FILES section must exist in netfyr-daemon.8");
        let files = &content[files_start..];
        assert!(
            files.contains("/run/netfyr/netfyr.sock"),
            "netfyr-daemon.8 FILES must list /run/netfyr/netfyr.sock"
        );
    }

    /// AC: Daemon man page FILES section lists the journal directory.
    #[test]
    fn test_daemon_8_files_lists_journal_directory() {
        let content = read_daemon_man_page();
        let files_start = content.find(".SH FILES").expect("FILES section must exist in netfyr-daemon.8");
        let files = &content[files_start..];
        assert!(
            files.contains("/var/lib/netfyr/journal/"),
            "netfyr-daemon.8 FILES must list /var/lib/netfyr/journal/"
        );
    }

    /// AC: Daemon man page FILES section lists the policies directory.
    #[test]
    fn test_daemon_8_files_lists_etc_netfyr_policies() {
        let content = read_daemon_man_page();
        let files_start = content.find(".SH FILES").expect("FILES section must exist in netfyr-daemon.8");
        let files = &content[files_start..];
        assert!(
            files.contains("/etc/netfyr/policies/"),
            "netfyr-daemon.8 FILES must list /etc/netfyr/policies/"
        );
    }

    // ── netfyr-examples.7 has copy-pasteable examples in each scenario ────────

    /// AC: Static IP scenario in examples.7 includes mtu and routes fields.
    #[test]
    fn test_examples_7_static_ip_example_is_complete() {
        let content = read_examples_man_page();
        let section_start = content
            .find("STATIC IP")
            .expect("STATIC IP section must exist in netfyr-examples.7");
        let next_section = content[section_start + 1..]
            .find("\n.SH ")
            .map(|i| section_start + 1 + i)
            .unwrap_or(content.len());
        let section = &content[section_start..next_section];
        assert!(section.contains("mtu"), "STATIC IP section must include mtu field");
        assert!(section.contains("routes"), "STATIC IP section must include routes field");
        assert!(section.contains(".nf"), "STATIC IP section must have a copy-pasteable example (.nf block)");
    }

    /// AC: DHCP scenario in examples.7 shows the complete policy syntax.
    #[test]
    fn test_examples_7_dhcp_example_shows_complete_policy() {
        let content = read_examples_man_page();
        let section_start = content
            .find("DHCP ON AN INTERFACE")
            .expect("DHCP ON AN INTERFACE section must exist in netfyr-examples.7");
        let next_section = content[section_start + 1..]
            .find("\n.SH ")
            .map(|i| section_start + 1 + i)
            .unwrap_or(content.len());
        let section = &content[section_start..next_section];
        assert!(
            section.contains("kind: policy"),
            "DHCP section must show 'kind: policy' in the example"
        );
        assert!(
            section.contains("selector:"),
            "DHCP section must show selector field"
        );
    }

    /// AC: PRIORITY OVERRIDE scenario shows two concrete files with different priorities.
    #[test]
    fn test_examples_7_priority_override_shows_two_files() {
        let content = read_examples_man_page();
        let section_start = content
            .find("PRIORITY OVERRIDE")
            .expect("PRIORITY OVERRIDE section must exist in netfyr-examples.7");
        let next_section = content[section_start + 1..]
            .find("\n.SH ")
            .map(|i| section_start + 1 + i)
            .unwrap_or(content.len());
        let section = &content[section_start..next_section];
        let nf_count = section.matches(".nf").count();
        assert!(
            nf_count >= 2,
            "PRIORITY OVERRIDE must show at least 2 example files (.nf blocks); found {nf_count}"
        );
        assert!(
            section.contains("priority: 200") || section.contains("priority: 100"),
            "PRIORITY OVERRIDE must show concrete priority values"
        );
    }

    // ── Hand-written file non-overwrite ───────────────────────────────────────

    /// AC: Generate all man pages — does not overwrite hand-written netfyr.yaml.5.
    /// The idempotency test covers daemon.8 and examples.7; this test covers yaml.5.
    #[test]
    fn test_generate_man_pages_does_not_overwrite_netfyr_yaml_5() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let path = manifest_dir.join("../man/netfyr.yaml.5");
        let before = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Failed to read man/netfyr.yaml.5: {e}"));

        generate_man_pages().expect("generate_man_pages must succeed");

        let after = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Failed to read man/netfyr.yaml.5 after generation: {e}"));

        assert_eq!(
            before, after,
            "generate_man_pages() must not modify the hand-maintained man/netfyr.yaml.5"
        );
    }

    // ── Stay in sync with CLI — dynamic clap reflection ──────────────────────

    /// AC: Man pages stay in sync with CLI — all clap subcommands have a .1 man page.
    /// If a new subcommand is added to Cli, `cargo xtask man` must produce a file for it.
    #[test]
    fn test_all_clap_subcommands_have_generated_man_pages() {
        use clap::CommandFactory;
        let cmd = netfyr_cli::Cli::command();
        for subcmd in cmd.get_subcommands() {
            let name = subcmd.get_name();
            let filename = format!("netfyr-{name}.1");
            assert!(
                man_page_path_exists(&filename),
                "man/{filename} must exist for the '{name}' clap subcommand (run `cargo xtask man`)"
            );
        }
    }

    /// AC: Man pages stay in sync with CLI — netfyr.1 mentions every subcommand
    /// name defined in the Cli struct.
    #[test]
    fn test_netfyr_1_mentions_every_clap_subcommand() {
        use clap::CommandFactory;
        let content = read_generated_man_page("netfyr.1");
        let cmd = netfyr_cli::Cli::command();
        for subcmd in cmd.get_subcommands() {
            let name = subcmd.get_name();
            assert!(
                content.contains(name),
                "netfyr.1 must mention the '{name}' subcommand (it is defined in Cli but missing from the page)"
            );
        }
    }

    /// AC: Man pages stay in sync with CLI — netfyr-apply.1 OPTIONS section
    /// contains every long flag defined in the clap apply subcommand.
    ///
    /// If a new flag is added to ApplyArgs in clap, re-running `cargo xtask man`
    /// must include it in the generated page.
    #[test]
    fn test_netfyr_apply_1_options_contain_all_clap_flags() {
        use clap::CommandFactory;
        let content = read_generated_man_page("netfyr-apply.1");
        let cmd = netfyr_cli::Cli::command();
        let apply_cmd = cmd
            .find_subcommand("apply")
            .expect("apply subcommand must be registered in Cli");

        for arg in apply_cmd.get_arguments() {
            if let Some(long) = arg.get_long() {
                // Skip flags injected by clap itself that don't appear in man pages.
                if long == "help" || long == "version" {
                    continue;
                }
                assert!(
                    content.contains(long),
                    "netfyr-apply.1 must document the --{long} flag \
                     (defined in clap but missing from the generated page)"
                );
            }
        }
    }

    /// AC: Man pages stay in sync with CLI — netfyr-history.1 OPTIONS section
    /// contains every long flag defined in the clap history subcommand.
    #[test]
    fn test_netfyr_history_1_options_contain_all_clap_flags() {
        use clap::CommandFactory;
        let content = read_generated_man_page("netfyr-history.1");
        let cmd = netfyr_cli::Cli::command();
        let sub = cmd
            .find_subcommand("history")
            .expect("history subcommand must be registered in Cli");

        for arg in sub.get_arguments() {
            if let Some(long) = arg.get_long() {
                if long == "help" || long == "version" {
                    continue;
                }
                assert!(
                    content.contains(long),
                    "netfyr-history.1 must document the --{long} flag \
                     (defined in clap but missing from the generated page)"
                );
            }
        }
    }

    /// AC: Man pages stay in sync with CLI — netfyr-show.1 OPTIONS section
    /// contains every long flag defined in the clap show subcommand.
    #[test]
    fn test_netfyr_show_1_options_contain_all_clap_flags() {
        use clap::CommandFactory;
        let content = read_generated_man_page("netfyr-show.1");
        let cmd = netfyr_cli::Cli::command();
        let sub = cmd
            .find_subcommand("show")
            .expect("show subcommand must be registered in Cli");

        for arg in sub.get_arguments() {
            if let Some(long) = arg.get_long() {
                if long == "help" || long == "version" {
                    continue;
                }
                assert!(
                    content.contains(long),
                    "netfyr-show.1 must document the --{long} flag \
                     (defined in clap but missing from the generated page)"
                );
            }
        }
    }

    /// AC: Man pages stay in sync with CLI — netfyr-revert.1 OPTIONS section
    /// contains every long flag defined in the clap revert subcommand.
    #[test]
    fn test_netfyr_revert_1_options_contain_all_clap_flags() {
        use clap::CommandFactory;
        let content = read_generated_man_page("netfyr-revert.1");
        let cmd = netfyr_cli::Cli::command();
        let sub = cmd
            .find_subcommand("revert")
            .expect("revert subcommand must be registered in Cli");

        for arg in sub.get_arguments() {
            if let Some(long) = arg.get_long() {
                if long == "help" || long == "version" {
                    continue;
                }
                assert!(
                    content.contains(long),
                    "netfyr-revert.1 must document the --{long} flag \
                     (defined in clap but missing from the generated page)"
                );
            }
        }
    }
}
