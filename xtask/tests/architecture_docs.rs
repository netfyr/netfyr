//! Integration tests for architecture documentation.
//!
//! Verifies that docs/architecture.md and docs/workflows.md exist and satisfy
//! all acceptance criteria from SPEC-504.
//!
//! Acceptance criteria covered:
//!   - docs/architecture.md exists with Mermaid crate dependency graph
//!   - docs/architecture.md describes the four-layer architecture
//!   - docs/architecture.md shows standalone and daemon mode diagrams
//!   - docs/architecture.md contains a Mermaid data model class diagram
//!   - docs/architecture.md contains a key concepts reference table
//!   - docs/workflows.md exists with sequence diagrams for all six workflows
//!   - Crate dependency graph includes nodes for all 9 library crates
//!   - netfyr-test-utils is shown as a dev-dependency (dashed arrow)

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask/ must have a parent directory (the workspace root)")
        .to_path_buf()
}

fn read_architecture_md() -> String {
    let path = workspace_root().join("docs").join("architecture.md");
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Cannot read docs/architecture.md: {e}"))
}

fn read_workflows_md() -> String {
    let path = workspace_root().join("docs").join("workflows.md");
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Cannot read docs/workflows.md: {e}"))
}

// ─── Scenario: Architecture document exists ───────────────────────────────────

#[test]
fn architecture_md_exists() {
    let path = workspace_root().join("docs").join("architecture.md");
    assert!(
        path.exists(),
        "docs/architecture.md does not exist at {}",
        path.display()
    );
}

#[test]
fn architecture_md_contains_mermaid_crate_dependency_graph() {
    let content = read_architecture_md();
    assert!(
        content.contains("```mermaid"),
        "docs/architecture.md must contain at least one ```mermaid code block"
    );
    assert!(
        content.to_lowercase().contains("graph td"),
        "docs/architecture.md must contain a 'graph TD' Mermaid crate dependency diagram"
    );
}

#[test]
fn architecture_md_describes_four_layer_architecture() {
    let content = read_architecture_md();
    // The spec defines Layer 0 through Layer 3 by name.
    for layer in ["Layer 0", "Layer 1", "Layer 2", "Layer 3"] {
        assert!(
            content.contains(layer),
            "docs/architecture.md must describe '{layer}' as part of the four-layer architecture"
        );
    }
}

#[test]
fn architecture_md_shows_standalone_mode_diagram() {
    let content = read_architecture_md();
    assert!(
        content.to_lowercase().contains("standalone"),
        "docs/architecture.md must describe standalone mode"
    );
    assert!(
        content.contains("graph LR") || content.contains("graph lr"),
        "docs/architecture.md must use a 'graph LR' Mermaid diagram for mode illustrations"
    );
}

#[test]
fn architecture_md_shows_daemon_mode_diagram() {
    let content = read_architecture_md();
    assert!(
        content.to_lowercase().contains("daemon mode")
            || content.to_lowercase().contains("daemon-mode"),
        "docs/architecture.md must describe daemon mode"
    );
}

#[test]
fn architecture_md_contains_mermaid_data_model_class_diagram() {
    let content = read_architecture_md();
    assert!(
        content.contains("classDiagram"),
        "docs/architecture.md must contain a Mermaid 'classDiagram' for the data model"
    );
}

#[test]
fn architecture_md_contains_key_concepts_reference_table() {
    let content = read_architecture_md();
    // A Markdown table has lines starting and ending with '|'.
    let has_table = content.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.starts_with('|') && trimmed.ends_with('|')
    });
    assert!(
        has_table,
        "docs/architecture.md must contain a Markdown reference table for key concepts"
    );
}

// ─── Scenario: Workflow document exists ───────────────────────────────────────

#[test]
fn workflows_md_exists() {
    let path = workspace_root().join("docs").join("workflows.md");
    assert!(
        path.exists(),
        "docs/workflows.md does not exist at {}",
        path.display()
    );
}

#[test]
fn workflows_md_contains_sequence_diagram_for_standalone_apply() {
    let content = read_workflows_md();
    assert!(
        content.contains("sequenceDiagram"),
        "docs/workflows.md must contain 'sequenceDiagram' blocks"
    );
    assert!(
        content.to_lowercase().contains("standalone"),
        "docs/workflows.md must contain a sequence diagram for standalone apply mode"
    );
}

#[test]
fn workflows_md_contains_sequence_diagram_for_daemon_apply() {
    let content = read_workflows_md();
    assert!(
        content.to_lowercase().contains("daemon"),
        "docs/workflows.md must contain a sequence diagram for daemon-mode apply"
    );
    assert!(
        content.to_lowercase().contains("varlink"),
        "docs/workflows.md daemon-mode apply diagram must reference Varlink"
    );
}

#[test]
fn workflows_md_contains_sequence_diagram_for_query() {
    let content = read_workflows_md();
    assert!(
        content.to_lowercase().contains("query"),
        "docs/workflows.md must contain a sequence diagram for the query workflow"
    );
}

#[test]
fn workflows_md_contains_sequence_diagram_for_revert() {
    let content = read_workflows_md();
    assert!(
        content.to_lowercase().contains("revert"),
        "docs/workflows.md must contain a sequence diagram for the revert workflow"
    );
}

#[test]
fn workflows_md_contains_sequence_diagram_for_daemon_startup() {
    let content = read_workflows_md();
    let lower = content.to_lowercase();
    assert!(
        lower.contains("startup")
            || lower.contains("start up")
            || lower.contains("initializ"),
        "docs/workflows.md must contain a sequence diagram for daemon startup"
    );
}

#[test]
fn workflows_md_contains_sequence_diagram_for_dhcp_lease_lifecycle() {
    let content = read_workflows_md();
    let lower = content.to_lowercase();
    assert!(
        lower.contains("dhcp"),
        "docs/workflows.md must contain a sequence diagram for the DHCP lease lifecycle"
    );
    assert!(
        lower.contains("lease"),
        "docs/workflows.md DHCP section must describe the lease lifecycle"
    );
}

// ─── Scenario: Diagrams reference all workspace crates ────────────────────────

const LIBRARY_CRATES: &[&str] = &[
    "netfyr-state",
    "netfyr-policy",
    "netfyr-reconcile",
    "netfyr-backend",
    "netfyr-journal",
    "netfyr-varlink",
    "netfyr-cli",
    "netfyr-daemon",
    "netfyr-test-utils",
];

#[test]
fn dependency_graph_includes_all_nine_library_crates() {
    let content = read_architecture_md();
    for crate_name in LIBRARY_CRATES {
        assert!(
            content.contains(crate_name),
            "docs/architecture.md crate dependency graph must include node for '{crate_name}'"
        );
    }
}

#[test]
fn dependency_graph_shows_netfyr_test_utils_as_dev_dependency_with_dashed_arrow() {
    let content = read_architecture_md();
    assert!(
        content.contains("netfyr-test-utils"),
        "docs/architecture.md must include a node for 'netfyr-test-utils'"
    );
    // The spec requires dashed arrows (-.-> in Mermaid) for dev-dependencies.
    assert!(
        content.contains("-.->"),
        "docs/architecture.md must use dashed arrows (-.-> in Mermaid) for dev-dependencies \
         such as netfyr-test-utils"
    );
}
