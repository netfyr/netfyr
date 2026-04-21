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
    /// Does not overwrite man/netfyr.yaml.5 or man/netfyr-examples.7 (maintained by hand).
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
        append_see_also(&mut buf, Some(&subcmd_name))?;
        let filename = format!("{name}.1");
        fs::write(out_dir.join(&filename), &buf)?;
        println!("Generated: man/{filename}");
    }

    println!("Note: man/netfyr.yaml.5 and man/netfyr-examples.7 are maintained by hand and were not modified.");
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
        Some(other) => {
            // Fallback for any future subcommands.
            writeln!(buf, "See")?;
            writeln!(buf, ".BR netfyr-{other} (1)")?;
            writeln!(buf, "for usage details.")?;
        }
    }
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
            writeln!(buf, ".BR netfyr-examples (7),")?;
            writeln!(buf, r".BR netfyr.yaml (5)")?;
        }
        Some("apply") => {
            writeln!(buf, ".BR netfyr (1),")?;
            writeln!(buf, ".BR netfyr-query (1),")?;
            writeln!(buf, ".BR netfyr-examples (7),")?;
            writeln!(buf, r".BR netfyr.yaml (5)")?;
        }
        Some("query") => {
            writeln!(buf, ".BR netfyr (1),")?;
            writeln!(buf, ".BR netfyr-apply (1),")?;
            writeln!(buf, ".BR netfyr-examples (7),")?;
            writeln!(buf, r".BR netfyr.yaml (5)")?;
        }
        Some(_) => {
            writeln!(buf, ".BR netfyr (1),")?;
            writeln!(buf, ".BR netfyr-apply (1),")?;
            writeln!(buf, ".BR netfyr-query (1),")?;
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
}
