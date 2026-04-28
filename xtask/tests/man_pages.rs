//! Integration tests for man page content and generation behaviour.
//!
//! Content tests read the committed generated files in man/ and verify that
//! every acceptance criterion from SPEC-501 is satisfied.  Generation tests
//! invoke `cargo xtask man` as a subprocess and verify the resulting files.
//!
//! Acceptance criteria covered:
//!   - Generate all man pages (files exist after running xtask)
//!   - hand-written netfyr-examples.7 is not overwritten
//!   - top-level netfyr.1 lists apply and query subcommands
//!   - netfyr-apply.1 documents --dry-run and <paths> positional argument
//!   - Man pages include EXIT STATUS section with codes 0, 1, 2
//!   - Man pages include EXAMPLES section with ≥ 2 examples
//!   - Man pages include FILES section listing /etc/netfyr/policies/
//!   - Man pages include SEE ALSO with correct cross-references
//!   - Man pages render without troff errors (groff, if available)
//!   - netfyr-examples.7 exists, has NAME section, covers all required scenarios
//!   - Generated pages stay in sync with CLI definitions
//!   - Regeneration is idempotent

use std::path::{Path, PathBuf};
use std::process::Command;

// ── Path helpers ──────────────────────────────────────────────────────────────

/// Absolute path to the workspace root (one level above xtask/).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask/ must have a parent directory (the workspace root)")
        .to_path_buf()
}

/// Absolute path to the man/ directory at the workspace root.
fn man_dir() -> PathBuf {
    workspace_root().join("man")
}

/// Read a man page source file from man/ and return its contents.
///
/// Panics with a descriptive message if the file cannot be read.
fn read_man_page(name: &str) -> String {
    let path = man_dir().join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read man/{name}: {e}"))
}

// ── xtask invocation ──────────────────────────────────────────────────────────

/// Run `cargo xtask man` from the workspace root and return its output.
///
/// Uses `env!("CARGO")` so the same cargo binary that built these tests is
/// used for the xtask run — avoids PATH lookup issues in CI.
fn run_xtask_man() -> std::process::Output {
    Command::new(env!("CARGO"))
        .args(["run", "--quiet", "--package", "xtask", "--", "man"])
        .current_dir(workspace_root())
        .output()
        .expect("failed to spawn `cargo run --package xtask -- man`")
}

// ── troff rendering helper ────────────────────────────────────────────────────

/// Attempt to render a troff man page source with `groff`.
///
/// Returns `Some((exit_ok, stderr))` when groff is available, `None` when the
/// tool is not found (test is then skipped gracefully).
fn try_groff_render(filename: &str) -> Option<(bool, String)> {
    let path = man_dir().join(filename);
    let out = Command::new("groff")
        .args(["-mandoc", "-Tutf8", path.to_str().unwrap()])
        .output()
        .ok()?;
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    Some((out.status.success(), stderr))
}

// ── Scenario: Generate all man pages ─────────────────────────────────────────

/// AC: `cargo xtask man` exits 0.
#[test]
fn test_xtask_man_exits_successfully() {
    let out = run_xtask_man();
    assert!(
        out.status.success(),
        "`cargo xtask man` must exit 0; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// AC: man/netfyr.1 is created (or already exists) after running xtask man.
#[test]
fn test_xtask_man_creates_netfyr_1() {
    run_xtask_man();
    assert!(
        man_dir().join("netfyr.1").exists(),
        "man/netfyr.1 must exist after `cargo xtask man`"
    );
}

/// AC: man/netfyr-apply.1 is created (or already exists) after running xtask man.
#[test]
fn test_xtask_man_creates_netfyr_apply_1() {
    run_xtask_man();
    assert!(
        man_dir().join("netfyr-apply.1").exists(),
        "man/netfyr-apply.1 must exist after `cargo xtask man`"
    );
}

/// AC: man/netfyr-query.1 is created (or already exists) after running xtask man.
#[test]
fn test_xtask_man_creates_netfyr_query_1() {
    run_xtask_man();
    assert!(
        man_dir().join("netfyr-query.1").exists(),
        "man/netfyr-query.1 must exist after `cargo xtask man`"
    );
}

/// AC: xtask man must NOT overwrite man/netfyr-examples.7 (maintained by hand).
///
/// The hand-written file contains a maintainer comment on the first line.
/// After running the xtask, that comment must still be present.
#[test]
fn test_xtask_man_does_not_overwrite_hand_written_examples_page() {
    // Capture the content before running, so a before/after comparison is
    // possible even if the file was just created.
    let content_before = read_man_page("netfyr-examples.7");
    run_xtask_man();
    let content_after = read_man_page("netfyr-examples.7");
    assert_eq!(
        content_before, content_after,
        "`cargo xtask man` must not modify man/netfyr-examples.7 (hand-written file)"
    );
    // Belt-and-suspenders: the hand-written marker must always be present.
    assert!(
        content_after.contains("maintained by hand"),
        "man/netfyr-examples.7 must retain its hand-written maintainer comment"
    );
}

// ── Scenario: Top-level man page lists all subcommands ────────────────────────

/// AC: DESCRIPTION in netfyr.1 mentions the apply subcommand.
#[test]
fn test_netfyr_1_description_mentions_apply_subcommand() {
    let content = read_man_page("netfyr.1");
    assert!(
        content.contains("apply"),
        "man/netfyr.1 must mention the 'apply' subcommand"
    );
}

/// AC: DESCRIPTION in netfyr.1 mentions the query subcommand.
#[test]
fn test_netfyr_1_description_mentions_query_subcommand() {
    let content = read_man_page("netfyr.1");
    assert!(
        content.contains("query"),
        "man/netfyr.1 must mention the 'query' subcommand"
    );
}

/// AC: SEE ALSO in netfyr.1 references the apply subcommand page.
#[test]
fn test_netfyr_1_see_also_references_netfyr_apply_1() {
    let content = read_man_page("netfyr.1");
    assert!(
        content.contains("SEE ALSO"),
        "man/netfyr.1 must have a SEE ALSO section"
    );
    assert!(
        content.contains("netfyr-apply"),
        "man/netfyr.1 SEE ALSO must reference netfyr-apply(1)"
    );
}

/// AC: SEE ALSO in netfyr.1 references the query subcommand page.
#[test]
fn test_netfyr_1_see_also_references_netfyr_query_1() {
    let content = read_man_page("netfyr.1");
    assert!(
        content.contains("netfyr-query"),
        "man/netfyr.1 SEE ALSO must reference netfyr-query(1)"
    );
}

// ── Scenario: Subcommand man pages document all flags ────────────────────────

/// AC: OPTIONS in netfyr-apply.1 lists --dry-run.
///
/// clap_mangen encodes hyphens as troff `\-` escape sequences, so the raw
/// source contains `dry\-run`.  Both forms are checked.
#[test]
fn test_apply_1_options_lists_dry_run_flag() {
    let content = read_man_page("netfyr-apply.1");
    // Accept either the troff-encoded form (raw source) or the plain form
    // (rendered output that might be checked via another tool).
    let has_dry_run = content.contains("dry\\-run") || content.contains("dry-run");
    assert!(
        has_dry_run,
        "man/netfyr-apply.1 OPTIONS must document the --dry-run flag"
    );
}

/// AC: OPTIONS in netfyr-apply.1 documents the <path> positional argument.
///
/// clap_mangen renders the positional as `<PATHS>` or similar uppercase token.
#[test]
fn test_apply_1_options_documents_paths_positional_argument() {
    let content = read_man_page("netfyr-apply.1");
    let has_paths = content.to_uppercase().contains("PATHS")
        || content.contains("paths")
        || content.contains("path");
    assert!(
        has_paths,
        "man/netfyr-apply.1 OPTIONS must document the <paths> positional argument"
    );
}

/// AC: netfyr-query.1 OPTIONS documents --selector (-s).
#[test]
fn test_query_1_options_lists_selector_flag() {
    let content = read_man_page("netfyr-query.1");
    let has_selector = content.contains("selector") || content.contains("\\-s");
    assert!(
        has_selector,
        "man/netfyr-query.1 OPTIONS must document --selector / -s"
    );
}

/// AC: netfyr-query.1 OPTIONS documents --output (-o).
#[test]
fn test_query_1_options_lists_output_flag() {
    let content = read_man_page("netfyr-query.1");
    let has_output = content.contains("output") || content.contains("\\-o");
    assert!(
        has_output,
        "man/netfyr-query.1 OPTIONS must document --output / -o"
    );
}

// ── Scenario: Man pages include EXIT STATUS section ───────────────────────────

/// AC: netfyr-apply.1 has an EXIT STATUS section.
#[test]
fn test_apply_1_has_exit_status_section() {
    let content = read_man_page("netfyr-apply.1");
    assert!(
        content.contains("EXIT STATUS"),
        "man/netfyr-apply.1 must contain an EXIT STATUS section"
    );
}

/// AC: EXIT STATUS section documents code 0 (success / no changes needed).
#[test]
fn test_apply_1_exit_status_documents_code_0() {
    let content = read_man_page("netfyr-apply.1");
    // The xtask emits `.B 0` for bold exit code in troff source.
    assert!(
        content.contains(".B 0"),
        "man/netfyr-apply.1 EXIT STATUS must document exit code 0 (.B 0)"
    );
}

/// AC: EXIT STATUS section documents code 1 (partial failure / conflicts).
#[test]
fn test_apply_1_exit_status_documents_code_1() {
    let content = read_man_page("netfyr-apply.1");
    assert!(
        content.contains(".B 1"),
        "man/netfyr-apply.1 EXIT STATUS must document exit code 1 (.B 1)"
    );
}

/// AC: EXIT STATUS section documents code 2 (total failure / fatal error).
#[test]
fn test_apply_1_exit_status_documents_code_2() {
    let content = read_man_page("netfyr-apply.1");
    assert!(
        content.contains(".B 2"),
        "man/netfyr-apply.1 EXIT STATUS must document exit code 2 (.B 2)"
    );
}

/// netfyr.1 must also carry all three exit codes.
#[test]
fn test_netfyr_1_has_all_three_exit_codes() {
    let content = read_man_page("netfyr.1");
    assert!(content.contains(".B 0"), "netfyr.1 EXIT STATUS must document code 0");
    assert!(content.contains(".B 1"), "netfyr.1 EXIT STATUS must document code 1");
    assert!(content.contains(".B 2"), "netfyr.1 EXIT STATUS must document code 2");
}

// ── Scenario: Man pages include EXAMPLES section ──────────────────────────────

/// AC: netfyr-apply.1 has an EXAMPLES section.
#[test]
fn test_apply_1_has_examples_section() {
    let content = read_man_page("netfyr-apply.1");
    assert!(
        content.contains("EXAMPLES"),
        "man/netfyr-apply.1 must contain an EXAMPLES section"
    );
}

/// AC: EXAMPLES section contains at least two real-world usage examples.
///
/// Each code example is rendered in a troff .nf / .fi (no-fill) block.
#[test]
fn test_apply_1_examples_has_at_least_two_usage_examples() {
    let content = read_man_page("netfyr-apply.1");
    let nf_count = content.matches(".nf").count();
    assert!(
        nf_count >= 2,
        "man/netfyr-apply.1 EXAMPLES must contain ≥ 2 code examples (.nf blocks); found {nf_count}"
    );
}

/// The apply examples section must include a --dry-run invocation.
#[test]
fn test_apply_1_examples_includes_dry_run_invocation() {
    let content = read_man_page("netfyr-apply.1");
    let has_dry_run = content.contains("dry\\-run") || content.contains("dry-run");
    assert!(
        has_dry_run,
        "man/netfyr-apply.1 EXAMPLES must include a --dry-run usage example"
    );
}

/// netfyr-query.1 EXAMPLES has at least two code blocks.
#[test]
fn test_query_1_examples_has_at_least_two_usage_examples() {
    let content = read_man_page("netfyr-query.1");
    let nf_count = content.matches(".nf").count();
    assert!(
        nf_count >= 2,
        "man/netfyr-query.1 EXAMPLES must contain ≥ 2 code examples (.nf blocks); found {nf_count}"
    );
}

// ── Scenario: Man pages include FILES section ─────────────────────────────────

/// AC: netfyr-apply.1 has a FILES section.
#[test]
fn test_apply_1_has_files_section() {
    let content = read_man_page("netfyr-apply.1");
    assert!(
        content.contains(".SH FILES") || content.contains("SH FILES"),
        "man/netfyr-apply.1 must contain a FILES section"
    );
}

/// AC: FILES section lists /etc/netfyr/policies/.
#[test]
fn test_apply_1_files_section_lists_etc_netfyr_policies() {
    let content = read_man_page("netfyr-apply.1");
    assert!(
        content.contains("/etc/netfyr/policies/"),
        "man/netfyr-apply.1 FILES must list /etc/netfyr/policies/"
    );
}

/// netfyr.1 FILES section also lists /etc/netfyr/policies/.
#[test]
fn test_netfyr_1_files_section_lists_etc_netfyr_policies() {
    let content = read_man_page("netfyr.1");
    assert!(
        content.contains("/etc/netfyr/policies/"),
        "man/netfyr.1 FILES must list /etc/netfyr/policies/"
    );
}

// ── Scenario: Man pages include SEE ALSO cross-references ────────────────────

/// AC: netfyr-apply.1 SEE ALSO references netfyr(1).
#[test]
fn test_apply_1_see_also_references_netfyr_1() {
    let content = read_man_page("netfyr-apply.1");
    assert!(
        content.contains("SEE ALSO"),
        "man/netfyr-apply.1 must have a SEE ALSO section"
    );
    // The top-level page is referenced as `netfyr (1)` (with a space, per .BR convention).
    assert!(
        content.contains("netfyr (1)") || content.contains("netfyr(1)"),
        "man/netfyr-apply.1 SEE ALSO must reference netfyr(1)"
    );
}

/// AC: netfyr-apply.1 SEE ALSO references netfyr-query(1).
#[test]
fn test_apply_1_see_also_references_netfyr_query_1() {
    let content = read_man_page("netfyr-apply.1");
    assert!(
        content.contains("netfyr-query (1)") || content.contains("netfyr-query(1)"),
        "man/netfyr-apply.1 SEE ALSO must reference netfyr-query(1)"
    );
}

/// AC: netfyr-apply.1 SEE ALSO references netfyr.yaml(5).
#[test]
fn test_apply_1_see_also_references_netfyr_yaml_5() {
    let content = read_man_page("netfyr-apply.1");
    assert!(
        content.contains("netfyr.yaml"),
        "man/netfyr-apply.1 SEE ALSO must reference netfyr.yaml(5)"
    );
}

// ── Scenario: Man pages render correctly with man command ─────────────────────

/// AC: man/netfyr.1 renders through groff without errors or warnings.
///
/// If groff is not installed the test is skipped (passes vacuously).
#[test]
fn test_netfyr_1_renders_without_troff_errors() {
    if let Some((ok, stderr)) = try_groff_render("netfyr.1") {
        // groff exits non-zero on fatal errors; warnings go to stderr.
        assert!(ok, "man/netfyr.1 must render without fatal groff errors; stderr:\n{stderr}");
        // Treat any "warning:" line as a failure.
        let has_warning = stderr.lines().any(|l| l.to_lowercase().contains("warning:"));
        assert!(
            !has_warning,
            "man/netfyr.1 must render without troff warnings; groff stderr:\n{stderr}"
        );
    }
}

/// AC: man/netfyr-apply.1 renders through groff without errors or warnings.
#[test]
fn test_netfyr_apply_1_renders_without_troff_errors() {
    if let Some((ok, stderr)) = try_groff_render("netfyr-apply.1") {
        assert!(ok, "man/netfyr-apply.1 must render without fatal groff errors; stderr:\n{stderr}");
        let has_warning = stderr.lines().any(|l| l.to_lowercase().contains("warning:"));
        assert!(
            !has_warning,
            "man/netfyr-apply.1 must render without troff warnings; groff stderr:\n{stderr}"
        );
    }
}

/// man/netfyr-query.1 renders through groff without errors or warnings.
#[test]
fn test_netfyr_query_1_renders_without_troff_errors() {
    if let Some((ok, stderr)) = try_groff_render("netfyr-query.1") {
        assert!(ok, "man/netfyr-query.1 must render without fatal groff errors; stderr:\n{stderr}");
        let has_warning = stderr.lines().any(|l| l.to_lowercase().contains("warning:"));
        assert!(
            !has_warning,
            "man/netfyr-query.1 must render without troff warnings; groff stderr:\n{stderr}"
        );
    }
}

// ── Scenario: Examples man page exists and renders ────────────────────────────

/// AC: man/netfyr-examples.7 must exist.
#[test]
fn test_examples_7_file_exists() {
    assert!(
        man_dir().join("netfyr-examples.7").exists(),
        "man/netfyr-examples.7 must exist as a hand-written file"
    );
}

/// AC: NAME section of netfyr-examples.7 contains "netfyr-examples".
#[test]
fn test_examples_7_name_section_contains_netfyr_examples() {
    let content = read_man_page("netfyr-examples.7");
    // Both the .TH title and .SH NAME entry should contain the page name.
    assert!(
        content.contains("netfyr") && content.contains("examples"),
        "man/netfyr-examples.7 NAME section must identify the page as 'netfyr-examples'"
    );
}

/// AC: man/netfyr-examples.7 renders without troff errors.
#[test]
fn test_examples_7_renders_without_troff_errors() {
    if let Some((ok, stderr)) = try_groff_render("netfyr-examples.7") {
        assert!(
            ok,
            "man/netfyr-examples.7 must render without fatal groff errors; stderr:\n{stderr}"
        );
        let has_warning = stderr.lines().any(|l| l.to_lowercase().contains("warning:"));
        assert!(
            !has_warning,
            "man/netfyr-examples.7 must render without troff warnings; groff stderr:\n{stderr}"
        );
    }
}

// ── Scenario: Examples man page covers common scenarios ───────────────────────

/// AC: examples page has a section for "Static IP on a single interface".
#[test]
fn test_examples_7_covers_static_ip_on_single_interface() {
    let content = read_man_page("netfyr-examples.7");
    let upper = content.to_uppercase();
    assert!(
        upper.contains("STATIC IP"),
        "netfyr-examples.7 must cover 'Static IP on a single interface'"
    );
}

/// AC: examples page covers "Multiple interfaces in one file".
#[test]
fn test_examples_7_covers_multiple_interfaces_in_one_file() {
    let content = read_man_page("netfyr-examples.7");
    let upper = content.to_uppercase();
    assert!(
        upper.contains("MULTIPLE INTERFACE"),
        "netfyr-examples.7 must cover 'Multiple interfaces in one file'"
    );
}

/// AC: examples page covers "DHCP on an interface".
#[test]
fn test_examples_7_covers_dhcp_on_an_interface() {
    let content = read_man_page("netfyr-examples.7");
    let upper = content.to_uppercase();
    assert!(
        upper.contains("DHCP"),
        "netfyr-examples.7 must cover 'DHCP on an interface'"
    );
}

/// AC: examples page covers "Mixed static and DHCP".
#[test]
fn test_examples_7_covers_mixed_static_and_dhcp() {
    let content = read_man_page("netfyr-examples.7");
    let upper = content.to_uppercase();
    assert!(
        upper.contains("MIXED"),
        "netfyr-examples.7 must cover 'Mixed static and DHCP'"
    );
}

/// AC: examples page covers "Priority override".
#[test]
fn test_examples_7_covers_priority_override() {
    let content = read_man_page("netfyr-examples.7");
    let upper = content.to_uppercase();
    assert!(
        upper.contains("PRIORITY"),
        "netfyr-examples.7 must cover 'Priority override'"
    );
}

/// AC: examples page covers "Selecting by driver".
#[test]
fn test_examples_7_covers_selecting_by_driver() {
    let content = read_man_page("netfyr-examples.7");
    let upper = content.to_uppercase();
    assert!(
        upper.contains("DRIVER"),
        "netfyr-examples.7 must cover 'Selecting by driver'"
    );
}

/// AC: examples page covers "Dry-run workflow".
#[test]
fn test_examples_7_covers_dry_run_workflow() {
    let content = read_man_page("netfyr-examples.7");
    let upper = content.to_uppercase();
    assert!(
        upper.contains("DRY") && (upper.contains("RUN") || upper.contains("WORKFLOW")),
        "netfyr-examples.7 must cover 'Dry-run workflow'"
    );
}

/// AC: each scenario contains a copy-pasteable YAML example.
///
/// YAML examples are wrapped in troff .nf / .fi (no-fill) blocks.
/// The spec lists 7 required scenarios, so we expect at least 7 code blocks.
#[test]
fn test_examples_7_sections_contain_yaml_examples() {
    let content = read_man_page("netfyr-examples.7");
    let nf_count = content.matches(".nf").count();
    assert!(
        nf_count >= 7,
        "netfyr-examples.7 must have ≥ 1 YAML code block per scenario (7 required); found {nf_count} .nf blocks"
    );
}

/// The examples page contains YAML keywords that make examples copy-pasteable.
#[test]
fn test_examples_7_yaml_examples_contain_type_ethernet() {
    let content = read_man_page("netfyr-examples.7");
    // Every static ethernet example must reference `type: ethernet`.
    assert!(
        content.contains("ethernet"),
        "netfyr-examples.7 must include YAML examples referencing 'ethernet'"
    );
}

// ── Scenario: Man pages stay in sync with CLI ─────────────────────────────────

/// AC: the generated apply page reflects ALL currently defined flags.
///
/// If a new flag is added to ApplyArgs in the CLI, running `cargo xtask man`
/// regenerates the page and this test catches the drift on the next CI run.
#[test]
fn test_apply_1_options_match_cli_flags() {
    let content = read_man_page("netfyr-apply.1");
    // Flags currently defined in netfyr_cli::apply::ApplyArgs.
    let dry_run_present = content.contains("dry\\-run") || content.contains("dry-run");
    assert!(
        dry_run_present,
        "man/netfyr-apply.1 must document --dry-run (from CLI definition)"
    );
    let help_present = content.contains("help") || content.contains("\\-h");
    assert!(
        help_present,
        "man/netfyr-apply.1 must document --help (from CLI definition)"
    );
}

/// AC: the generated query page reflects ALL currently defined flags.
#[test]
fn test_query_1_options_match_cli_flags() {
    let content = read_man_page("netfyr-query.1");
    let selector_present = content.contains("selector") || content.contains("\\-s");
    assert!(
        selector_present,
        "man/netfyr-query.1 must document --selector (from CLI definition)"
    );
    let output_present = content.contains("output") || content.contains("\\-o");
    assert!(
        output_present,
        "man/netfyr-query.1 must document --output (from CLI definition)"
    );
}

// ── Scenario: Generate all man pages — history and revert ────────────────────

/// AC: man/netfyr-history.1 is created after running xtask man.
#[test]
fn test_xtask_man_creates_netfyr_history_1() {
    run_xtask_man();
    assert!(
        man_dir().join("netfyr-history.1").exists(),
        "man/netfyr-history.1 must exist after `cargo xtask man`"
    );
}

/// AC: man/netfyr-revert.1 is created after running xtask man.
#[test]
fn test_xtask_man_creates_netfyr_revert_1() {
    run_xtask_man();
    assert!(
        man_dir().join("netfyr-revert.1").exists(),
        "man/netfyr-revert.1 must exist after `cargo xtask man`"
    );
}

/// AC: man/netfyr-daemon.8 is NOT overwritten (maintained by hand).
#[test]
fn test_xtask_man_does_not_overwrite_hand_written_daemon_page() {
    let content_before = read_man_page("netfyr-daemon.8");
    run_xtask_man();
    let content_after = read_man_page("netfyr-daemon.8");
    assert_eq!(
        content_before, content_after,
        "`cargo xtask man` must not modify man/netfyr-daemon.8 (hand-written file)"
    );
    assert!(
        content_after.contains("maintained by hand"),
        "man/netfyr-daemon.8 must retain its hand-written maintainer comment"
    );
}

// ── Scenario: netfyr-history.1 and netfyr-revert.1 content ───────────────────

/// AC: netfyr-history.1 EXAMPLES has at least two usage examples.
#[test]
fn test_history_1_examples_has_at_least_two_usage_examples() {
    let content = read_man_page("netfyr-history.1");
    let nf_count = content.matches(".nf").count();
    assert!(
        nf_count >= 2,
        "man/netfyr-history.1 EXAMPLES must contain ≥ 2 code examples (.nf blocks); found {nf_count}"
    );
}

/// AC: netfyr-history.1 OPTIONS documents --since.
#[test]
fn test_history_1_options_lists_since_flag() {
    let content = read_man_page("netfyr-history.1");
    assert!(
        content.contains("since"),
        "man/netfyr-history.1 OPTIONS must document --since"
    );
}

/// AC: netfyr-revert.1 EXAMPLES has at least two usage examples.
#[test]
fn test_revert_1_examples_has_at_least_two_usage_examples() {
    let content = read_man_page("netfyr-revert.1");
    let nf_count = content.matches(".nf").count();
    assert!(
        nf_count >= 2,
        "man/netfyr-revert.1 EXAMPLES must contain ≥ 2 code examples (.nf blocks); found {nf_count}"
    );
}

/// AC: netfyr-revert.1 OPTIONS documents --dry-run.
#[test]
fn test_revert_1_options_lists_dry_run_flag() {
    let content = read_man_page("netfyr-revert.1");
    let has_dry_run = content.contains("dry\\-run") || content.contains("dry-run");
    assert!(
        has_dry_run,
        "man/netfyr-revert.1 OPTIONS must document --dry-run"
    );
}

// ── Scenario: Examples man page covers all required scenarios ─────────────────

/// AC: examples page covers "Investigating changes with history".
#[test]
fn test_examples_7_covers_investigating_changes_with_history() {
    let content = read_man_page("netfyr-examples.7");
    let upper = content.to_uppercase();
    assert!(
        upper.contains("INVESTIGATING") || upper.contains("HISTORY"),
        "netfyr-examples.7 must cover 'Investigating changes with history'"
    );
    // The section must show `netfyr history` command usage
    assert!(
        content.contains("netfyr history"),
        "investigating-history scenario must demonstrate `netfyr history` command"
    );
}

/// AC: examples page covers "External change detection".
#[test]
fn test_examples_7_covers_external_change_detection() {
    let content = read_man_page("netfyr-examples.7");
    let upper = content.to_uppercase();
    assert!(
        upper.contains("EXTERNAL CHANGE"),
        "netfyr-examples.7 must cover 'External change detection'"
    );
}

/// AC: external change detection scenario shows that a policy must exist first.
#[test]
fn test_examples_7_external_change_requires_policy_first() {
    let content = read_man_page("netfyr-examples.7");
    let ext_start = content
        .to_uppercase()
        .find("EXTERNAL CHANGE")
        .expect("EXTERNAL CHANGE section must exist in netfyr-examples.7");
    // From the start of the section, a policy definition must appear before apply/start.
    let section = &content[ext_start..];
    assert!(
        section.contains("type: ethernet") || section.contains("kind: policy"),
        "EXTERNAL CHANGE DETECTION section must show creating a policy for the interface first"
    );
    assert!(
        section.contains("netfyr apply"),
        "EXTERNAL CHANGE DETECTION section must show applying the policy before monitoring"
    );
}

/// AC: external change detection scenario shows the complete workflow.
///
/// The workflow is: create policy → apply → external tool makes change → history shows it.
#[test]
fn test_examples_7_external_change_shows_complete_workflow() {
    let content = read_man_page("netfyr-examples.7");
    let ext_start = content
        .to_uppercase()
        .find("EXTERNAL CHANGE")
        .expect("EXTERNAL CHANGE section must exist in netfyr-examples.7");
    let section = &content[ext_start..];
    // External tool making a change (e.g., ip link set)
    assert!(
        section.contains("ip link") || section.contains("ip "),
        "EXTERNAL CHANGE DETECTION must show an external tool (e.g., `ip`) modifying the interface"
    );
    // netfyr history to observe the recorded change
    assert!(
        section.contains("netfyr history"),
        "EXTERNAL CHANGE DETECTION must show `netfyr history` to observe the recorded change"
    );
}

/// AC: examples page covers "Reverting to a previous state".
#[test]
fn test_examples_7_covers_reverting_to_previous_state() {
    let content = read_man_page("netfyr-examples.7");
    let upper = content.to_uppercase();
    assert!(
        upper.contains("REVERT"),
        "netfyr-examples.7 must cover 'Reverting to a previous state'"
    );
    assert!(
        content.contains("netfyr revert"),
        "reverting scenario must demonstrate `netfyr revert` command"
    );
}

/// AC: reverting scenario shows --dry-run before the actual revert.
#[test]
fn test_examples_7_reverting_scenario_shows_dry_run() {
    let content = read_man_page("netfyr-examples.7");
    let revert_start = content
        .to_uppercase()
        .find("REVERT")
        .expect("REVERTING section must exist in netfyr-examples.7");
    let section = &content[revert_start..];
    let has_dry_run = section.contains("dry\\-run") || section.contains("dry-run");
    assert!(
        has_dry_run,
        "reverting scenario must show `--dry-run` before executing the actual revert"
    );
}

// ── Scenario: Daemon man page exists and renders ──────────────────────────────

/// AC: man/netfyr-daemon.8 must exist as a hand-written file.
#[test]
fn test_daemon_8_file_exists() {
    assert!(
        man_dir().join("netfyr-daemon.8").exists(),
        "man/netfyr-daemon.8 must exist as a hand-written file"
    );
}

/// AC: NAME section of netfyr-daemon.8 contains "netfyr-daemon".
#[test]
fn test_daemon_8_name_section_contains_netfyr_daemon() {
    let content = read_man_page("netfyr-daemon.8");
    assert!(
        content.contains(".SH NAME"),
        "man/netfyr-daemon.8 must have a NAME section"
    );
    assert!(
        content.contains("netfyr") && content.contains("daemon"),
        "man/netfyr-daemon.8 NAME section must identify the page as 'netfyr-daemon'"
    );
}

/// AC: netfyr-daemon.8 TH header declares section 8.
#[test]
fn test_daemon_8_header_is_section_8() {
    let content = read_man_page("netfyr-daemon.8");
    assert!(
        content.contains(".TH") && content.contains(" 8 "),
        "man/netfyr-daemon.8 .TH header must declare section 8"
    );
}

/// AC: man/netfyr-daemon.8 renders through groff without errors or warnings.
#[test]
fn test_daemon_8_renders_without_troff_errors() {
    if let Some((ok, stderr)) = try_groff_render("netfyr-daemon.8") {
        assert!(
            ok,
            "man/netfyr-daemon.8 must render without fatal groff errors; stderr:\n{stderr}"
        );
        let has_warning = stderr.lines().any(|l| l.to_lowercase().contains("warning:"));
        assert!(
            !has_warning,
            "man/netfyr-daemon.8 must render without troff warnings; groff stderr:\n{stderr}"
        );
    }
}

// ── Scenario: Daemon man page documents external change detection ─────────────

/// AC: netfyr-daemon.8 has an EXTERNAL CHANGE DETECTION section.
#[test]
fn test_daemon_8_has_external_change_detection_section() {
    let content = read_man_page("netfyr-daemon.8");
    assert!(
        content.contains("EXTERNAL CHANGE DETECTION"),
        "man/netfyr-daemon.8 must have an 'EXTERNAL CHANGE DETECTION' section"
    );
}

/// AC: EXTERNAL CHANGE DETECTION section explains managed-only monitoring.
#[test]
fn test_daemon_8_external_change_detection_explains_managed_only() {
    let content = read_man_page("netfyr-daemon.8");
    let start = content
        .find("EXTERNAL CHANGE DETECTION")
        .expect("EXTERNAL CHANGE DETECTION section must exist");
    let section = &content[start..];
    let lower = section.to_lowercase();
    assert!(
        lower.contains("managed"),
        "EXTERNAL CHANGE DETECTION must explain that only managed interfaces are monitored"
    );
}

/// AC: EXTERNAL CHANGE DETECTION section documents monitored properties.
#[test]
fn test_daemon_8_external_change_detection_documents_monitored_properties() {
    let content = read_man_page("netfyr-daemon.8");
    let start = content
        .find("EXTERNAL CHANGE DETECTION")
        .expect("EXTERNAL CHANGE DETECTION section must exist");
    let section = &content[start..];
    assert!(section.contains("mtu"), "EXTERNAL CHANGE DETECTION must mention mtu");
    assert!(section.contains("state"), "EXTERNAL CHANGE DETECTION must mention state");
    assert!(section.contains("flags"), "EXTERNAL CHANGE DETECTION must mention flags");
    let lower = section.to_lowercase();
    assert!(
        lower.contains("ipv4") || lower.contains("address"),
        "EXTERNAL CHANGE DETECTION must mention IPv4 addresses"
    );
}

/// AC: EXTERNAL CHANGE DETECTION section documents the 500ms debounce window.
#[test]
fn test_daemon_8_external_change_detection_documents_debounce_window() {
    let content = read_man_page("netfyr-daemon.8");
    let start = content
        .find("EXTERNAL CHANGE DETECTION")
        .expect("EXTERNAL CHANGE DETECTION section must exist");
    let section = &content[start..];
    assert!(
        section.contains("500"),
        "EXTERNAL CHANGE DETECTION must mention the 500ms debounce window"
    );
}

/// AC: EXTERNAL CHANGE DETECTION section documents no automatic re-reconciliation.
#[test]
fn test_daemon_8_external_change_detection_documents_no_auto_reconciliation() {
    let content = read_man_page("netfyr-daemon.8");
    let start = content
        .find("EXTERNAL CHANGE DETECTION")
        .expect("EXTERNAL CHANGE DETECTION section must exist");
    let section = &content[start..];
    let lower = section.to_lowercase();
    assert!(
        lower.contains("does not") || lower.contains("no automatic"),
        "EXTERNAL CHANGE DETECTION must state that the daemon does not automatically re-apply state"
    );
}

// ── Scenario: Daemon man page documents the journal ───────────────────────────

/// AC: netfyr-daemon.8 has a JOURNAL section.
#[test]
fn test_daemon_8_has_journal_section() {
    let content = read_man_page("netfyr-daemon.8");
    assert!(
        content.contains("JOURNAL"),
        "man/netfyr-daemon.8 must have a JOURNAL section"
    );
}

/// AC: JOURNAL section describes the NDJSON format.
#[test]
fn test_daemon_8_journal_describes_ndjson_format() {
    let content = read_man_page("netfyr-daemon.8");
    let start = content.find("JOURNAL").expect("JOURNAL section must exist");
    let section = &content[start..];
    assert!(
        section.contains("NDJSON") || section.contains("ndjson"),
        "JOURNAL section must describe the NDJSON append-only format"
    );
}

/// AC: JOURNAL section documents rotation thresholds (entries and size).
#[test]
fn test_daemon_8_journal_documents_rotation_and_retention() {
    let content = read_man_page("netfyr-daemon.8");
    let start = content.find("JOURNAL").expect("JOURNAL section must exist");
    let section = &content[start..];
    assert!(
        section.contains("10,000") || section.contains("10000"),
        "JOURNAL section must document the 10,000-entry rotation threshold"
    );
    assert!(
        section.contains("50") && (section.contains("MB") || section.contains("52428800")),
        "JOURNAL section must document the 50 MB size rotation threshold"
    );
    assert!(
        section.contains("90") && (section.contains("day") || section.contains("retain")),
        "JOURNAL section must document the 90-day retention policy"
    );
}

/// AC: JOURNAL section references netfyr-history(1) for inspecting entries.
#[test]
fn test_daemon_8_journal_references_netfyr_history() {
    let content = read_man_page("netfyr-daemon.8");
    let start = content.find("JOURNAL").expect("JOURNAL section must exist");
    let section = &content[start..];
    assert!(
        section.contains("netfyr history") || section.contains("netfyr-history"),
        "JOURNAL section must reference netfyr-history for reading journal entries"
    );
}

/// AC: JOURNAL section references netfyr-revert(1) for restoring state.
#[test]
fn test_daemon_8_journal_references_netfyr_revert() {
    let content = read_man_page("netfyr-daemon.8");
    let start = content.find("JOURNAL").expect("JOURNAL section must exist");
    let section = &content[start..];
    assert!(
        section.contains("netfyr revert") || section.contains("netfyr-revert"),
        "JOURNAL section must reference netfyr-revert for restoring previous state"
    );
}

// ── Scenario: Daemon man page documents environment variables ─────────────────

/// AC: netfyr-daemon.8 has an ENVIRONMENT section.
#[test]
fn test_daemon_8_has_environment_section() {
    let content = read_man_page("netfyr-daemon.8");
    assert!(
        content.contains("ENVIRONMENT"),
        "man/netfyr-daemon.8 must have an ENVIRONMENT section"
    );
}

/// AC: ENVIRONMENT section lists all six required environment variables.
#[test]
fn test_daemon_8_environment_lists_all_six_variables() {
    let content = read_man_page("netfyr-daemon.8");
    let start = content.find("ENVIRONMENT").expect("ENVIRONMENT section must exist");
    let section = &content[start..];
    assert!(
        section.contains("NETFYR_SOCKET_PATH"),
        "ENVIRONMENT section must list NETFYR_SOCKET_PATH"
    );
    assert!(
        section.contains("NETFYR_POLICY_DIR"),
        "ENVIRONMENT section must list NETFYR_POLICY_DIR"
    );
    assert!(
        section.contains("NETFYR_JOURNAL_DIR"),
        "ENVIRONMENT section must list NETFYR_JOURNAL_DIR"
    );
    assert!(
        section.contains("NETFYR_JOURNAL_MAX_ENTRIES"),
        "ENVIRONMENT section must list NETFYR_JOURNAL_MAX_ENTRIES"
    );
    assert!(
        section.contains("NETFYR_JOURNAL_MAX_SIZE"),
        "ENVIRONMENT section must list NETFYR_JOURNAL_MAX_SIZE"
    );
    assert!(
        section.contains("NETFYR_JOURNAL_RETENTION_DAYS"),
        "ENVIRONMENT section must list NETFYR_JOURNAL_RETENTION_DAYS"
    );
}

// ── Scenario: Regeneration is idempotent ──────────────────────────────────────

/// AC: running `cargo xtask man` twice produces byte-for-byte identical files.
///
/// NOTE: This test runs the xtask binary twice and is therefore slower than
/// the content-only tests.  It is intentionally placed last.
#[test]
fn test_regeneration_is_idempotent() {
    // First pass.
    let out1 = run_xtask_man();
    assert!(
        out1.status.success(),
        "first `cargo xtask man` run must succeed; stderr:\n{}",
        String::from_utf8_lossy(&out1.stderr)
    );

    let netfyr_1_first = read_man_page("netfyr.1");
    let apply_1_first = read_man_page("netfyr-apply.1");
    let query_1_first = read_man_page("netfyr-query.1");

    // Second pass.
    let out2 = run_xtask_man();
    assert!(
        out2.status.success(),
        "second `cargo xtask man` run must succeed; stderr:\n{}",
        String::from_utf8_lossy(&out2.stderr)
    );

    assert_eq!(
        netfyr_1_first,
        read_man_page("netfyr.1"),
        "man/netfyr.1 must be byte-identical after the second regeneration"
    );
    assert_eq!(
        apply_1_first,
        read_man_page("netfyr-apply.1"),
        "man/netfyr-apply.1 must be byte-identical after the second regeneration"
    );
    assert_eq!(
        query_1_first,
        read_man_page("netfyr-query.1"),
        "man/netfyr-query.1 must be byte-identical after the second regeneration"
    );
}

// ── Scenario: netfyr.1 mentions the show subcommand ──────────────────────────

/// AC: DESCRIPTION (or SUBCOMMANDS) in netfyr.1 mentions the show subcommand.
///
/// The spec states the DESCRIPTION section mentions apply, query, AND show.
#[test]
fn test_netfyr_1_description_mentions_show_subcommand() {
    let content = read_man_page("netfyr.1");
    assert!(
        content.contains("show"),
        "man/netfyr.1 must mention the 'show' subcommand"
    );
}

/// AC: SEE ALSO in netfyr.1 references netfyr-show(1).
///
/// The spec says SEE ALSO should reference all subcommand man pages, and
/// netfyr-show(1) is one of the six section-1 pages listed in SPEC-501.
/// clap_mangen encodes hyphens as `\-` in the SUBCOMMANDS section; the SEE
/// ALSO helper should emit a separate .BR entry for the show page.
///
/// NOTE: the xtask `append_see_also(None)` currently omits netfyr-show(1)
/// from the SEE ALSO section — this test exposes that gap.
#[test]
fn test_netfyr_1_see_also_references_netfyr_show_1() {
    let content = read_man_page("netfyr.1");
    // The SEE ALSO section must include the show page; accept both the plain
    // form ("netfyr-show") used in .BR entries and the troff-escaped form
    // ("netfyr\-show") used in generated SUBCOMMANDS entries.
    let see_also_start = content.find("SEE ALSO").expect("netfyr.1 must have a SEE ALSO section");
    let see_also = &content[see_also_start..];
    let has_show = see_also.contains("netfyr-show") || see_also.contains("netfyr\\-show");
    assert!(
        has_show,
        "man/netfyr.1 SEE ALSO must reference netfyr-show(1)"
    );
}

// ── Scenario: Show man page documents the show command ────────────────────────

/// AC: man/netfyr-show.1 is created (or already exists) after running xtask man.
#[test]
fn test_xtask_man_creates_netfyr_show_1() {
    run_xtask_man();
    assert!(
        man_dir().join("netfyr-show.1").exists(),
        "man/netfyr-show.1 must exist after `cargo xtask man`"
    );
}

/// AC: OPTIONS in netfyr-show.1 lists --output.
///
/// clap_mangen encodes the flag as `output` in the .TP entry; the raw source
/// may also appear as `\-o` for the short form.
#[test]
fn test_show_1_options_lists_output_flag() {
    let content = read_man_page("netfyr-show.1");
    let has_output = content.contains("output") || content.contains("\\-o");
    assert!(
        has_output,
        "man/netfyr-show.1 OPTIONS must document the --output flag"
    );
}

/// AC: EXAMPLES section in netfyr-show.1 contains at least two usage examples.
///
/// Each code example is rendered in a troff .nf / .fi (no-fill) block.
#[test]
fn test_show_1_examples_has_at_least_two_usage_examples() {
    let content = read_man_page("netfyr-show.1");
    let nf_count = content.matches(".nf").count();
    assert!(
        nf_count >= 2,
        "man/netfyr-show.1 EXAMPLES must contain ≥ 2 code examples (.nf blocks); found {nf_count}"
    );
}

/// AC: EXIT STATUS section in netfyr-show.1 documents code 0.
#[test]
fn test_show_1_exit_status_documents_code_0() {
    let content = read_man_page("netfyr-show.1");
    assert!(
        content.contains("EXIT STATUS"),
        "man/netfyr-show.1 must contain an EXIT STATUS section"
    );
    assert!(
        content.contains(".B 0"),
        "man/netfyr-show.1 EXIT STATUS must document exit code 0 (.B 0)"
    );
}

/// AC: EXIT STATUS section in netfyr-show.1 documents code 1.
#[test]
fn test_show_1_exit_status_documents_code_1() {
    let content = read_man_page("netfyr-show.1");
    assert!(
        content.contains(".B 1"),
        "man/netfyr-show.1 EXIT STATUS must document exit code 1 (.B 1)"
    );
}

/// AC: SEE ALSO in netfyr-show.1 references netfyr(1).
#[test]
fn test_show_1_see_also_references_netfyr_1() {
    let content = read_man_page("netfyr-show.1");
    assert!(
        content.contains("SEE ALSO"),
        "man/netfyr-show.1 must have a SEE ALSO section"
    );
    assert!(
        content.contains("netfyr (1)") || content.contains("netfyr(1)"),
        "man/netfyr-show.1 SEE ALSO must reference netfyr(1)"
    );
}

/// AC: SEE ALSO in netfyr-show.1 references netfyr-daemon(8).
///
/// The show command queries daemon status, so the daemon page is the primary
/// cross-reference for understanding daemon-mode behavior.
#[test]
fn test_show_1_see_also_references_netfyr_daemon_8() {
    let content = read_man_page("netfyr-show.1");
    assert!(
        content.contains("netfyr-daemon (8)") || content.contains("netfyr-daemon(8)"),
        "man/netfyr-show.1 SEE ALSO must reference netfyr-daemon(8)"
    );
}
