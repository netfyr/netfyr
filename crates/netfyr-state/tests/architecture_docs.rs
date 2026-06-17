use std::fs;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent() // netfyr-state -> crates
        .unwrap()
        .parent() // crates -> workspace root
        .unwrap()
        .to_path_buf()
}

fn read_doc(filename: &str) -> String {
    let path = workspace_root().join("docs").join(filename);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read docs/{filename}: {e}"))
}

// --- Scenario: Architecture document exists ---

#[test]
fn architecture_md_exists() {
    let path = workspace_root().join("docs").join("architecture.md");
    assert!(
        path.exists(),
        "docs/architecture.md must exist but was not found at {}",
        path.display()
    );
}

#[test]
fn architecture_md_contains_mermaid_crate_dependency_graph() {
    let content = read_doc("architecture.md");
    assert!(
        content.contains("```mermaid"),
        "docs/architecture.md must contain at least one Mermaid block (```mermaid)"
    );
    assert!(
        content.contains("graph TD"),
        "docs/architecture.md must contain a `graph TD` Mermaid diagram for the crate dependency graph"
    );
}

#[test]
fn architecture_md_describes_four_layer_architecture() {
    let content = read_doc("architecture.md");
    // Layer 0 / Foundation
    assert!(
        content.contains("Layer 0") || content.contains("Foundation"),
        "docs/architecture.md must describe Layer 0 (Foundation)"
    );
    // Layer 1 / Domain logic
    assert!(
        content.contains("Layer 1") || content.contains("Domain"),
        "docs/architecture.md must describe Layer 1 (Domain logic)"
    );
    // Layer 2 / I/O
    assert!(
        content.contains("Layer 2") || content.contains("I/O"),
        "docs/architecture.md must describe Layer 2 (I/O)"
    );
    // Layer 3 / Binaries
    assert!(
        content.contains("Layer 3") || content.contains("Binar"),
        "docs/architecture.md must describe Layer 3 (Binaries)"
    );
}

#[test]
fn architecture_md_shows_standalone_mode_diagram() {
    let content = read_doc("architecture.md");
    assert!(
        content.to_lowercase().contains("standalone"),
        "docs/architecture.md must describe standalone mode"
    );
    assert!(
        content.contains("graph LR"),
        "docs/architecture.md must contain a `graph LR` diagram for mode architecture"
    );
}

#[test]
fn architecture_md_shows_daemon_mode_diagram() {
    let content = read_doc("architecture.md");
    assert!(
        content.to_lowercase().contains("daemon"),
        "docs/architecture.md must describe daemon mode"
    );
    // Two graph LR diagrams are required (standalone + daemon)
    let graph_lr_count = content.matches("graph LR").count();
    assert!(
        graph_lr_count >= 2,
        "docs/architecture.md must contain at least two `graph LR` diagrams (standalone and daemon mode), found {graph_lr_count}"
    );
}

#[test]
fn architecture_md_contains_data_model_class_diagram() {
    let content = read_doc("architecture.md");
    assert!(
        content.contains("classDiagram"),
        "docs/architecture.md must contain a Mermaid `classDiagram` for the data model"
    );
    // Verify core data model types appear
    for type_name in &["State", "Selector", "Value", "Provenance"] {
        assert!(
            content.contains(type_name),
            "docs/architecture.md classDiagram must include type `{type_name}`"
        );
    }
}

#[test]
fn architecture_md_contains_key_concepts_reference_table() {
    let content = read_doc("architecture.md");
    // Markdown tables use pipe characters
    assert!(
        content.contains("| "),
        "docs/architecture.md must contain a key concepts reference table (Markdown table with '| ' columns)"
    );
    // Table should have a header separator row
    assert!(
        content.contains("|---") || content.contains("| ---"),
        "docs/architecture.md key concepts table must have a Markdown header separator row"
    );
}

// --- Scenario: Workflow document exists ---

#[test]
fn workflows_md_exists() {
    let path = workspace_root().join("docs").join("workflows.md");
    assert!(
        path.exists(),
        "docs/workflows.md must exist but was not found at {}",
        path.display()
    );
}

#[test]
fn workflows_md_contains_sequence_diagrams() {
    let content = read_doc("workflows.md");
    assert!(
        content.contains("sequenceDiagram"),
        "docs/workflows.md must contain Mermaid sequenceDiagram blocks"
    );
}

#[test]
fn workflows_md_contains_sequence_diagram_for_standalone_apply() {
    let content = read_doc("workflows.md");
    let lower = content.to_lowercase();
    assert!(
        lower.contains("standalone") && lower.contains("apply"),
        "docs/workflows.md must contain a sequence diagram for standalone apply"
    );
}

#[test]
fn workflows_md_contains_sequence_diagram_for_daemon_mode_apply() {
    let content = read_doc("workflows.md");
    let lower = content.to_lowercase();
    assert!(
        lower.contains("daemon") && lower.contains("apply"),
        "docs/workflows.md must contain a sequence diagram for daemon-mode apply"
    );
}

#[test]
fn workflows_md_contains_sequence_diagram_for_query() {
    let content = read_doc("workflows.md");
    let lower = content.to_lowercase();
    assert!(
        lower.contains("query"),
        "docs/workflows.md must contain a sequence diagram for query"
    );
}

#[test]
fn workflows_md_contains_sequence_diagram_for_revert() {
    let content = read_doc("workflows.md");
    let lower = content.to_lowercase();
    assert!(
        lower.contains("revert"),
        "docs/workflows.md must contain a sequence diagram for revert"
    );
}

#[test]
fn workflows_md_contains_sequence_diagram_for_daemon_startup() {
    let content = read_doc("workflows.md");
    let lower = content.to_lowercase();
    assert!(
        lower.contains("startup") || (lower.contains("daemon") && lower.contains("start")),
        "docs/workflows.md must contain a sequence diagram for daemon startup"
    );
}

#[test]
fn workflows_md_contains_sequence_diagram_for_dhcp_lease_lifecycle() {
    let content = read_doc("workflows.md");
    let lower = content.to_lowercase();
    assert!(
        lower.contains("dhcp"),
        "docs/workflows.md must contain a sequence diagram for DHCP lease lifecycle"
    );
    assert!(
        lower.contains("lease"),
        "docs/workflows.md DHCP section must describe lease lifecycle"
    );
}

#[test]
fn workflows_md_has_six_or_more_sequence_diagrams() {
    let content = read_doc("workflows.md");
    let count = content.matches("sequenceDiagram").count();
    assert!(
        count >= 6,
        "docs/workflows.md must contain at least 6 sequenceDiagram blocks \
         (standalone apply, daemon apply, query, revert, daemon startup, DHCP lease), found {count}"
    );
}

// --- Scenario: Diagrams reference all workspace crates ---

const ALL_LIBRARY_CRATES: &[&str] = &[
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
fn architecture_md_dependency_graph_includes_all_nine_library_crates() {
    let content = read_doc("architecture.md");
    for crate_name in ALL_LIBRARY_CRATES {
        assert!(
            content.contains(crate_name),
            "docs/architecture.md must include a node for crate '{crate_name}' in the dependency graph"
        );
    }
}

#[test]
fn architecture_md_shows_test_utils_as_dev_dependency() {
    let content = read_doc("architecture.md");
    assert!(
        content.contains("netfyr-test-utils"),
        "docs/architecture.md must include netfyr-test-utils"
    );
    // The spec requires dashed arrows for dev-dependencies; in Mermaid that is `-.->`.
    assert!(
        content.contains("-.->"),
        "docs/architecture.md must use dashed Mermaid arrows (`-.->`) for dev-dependencies \
         (netfyr-test-utils is a dev-dependency)"
    );
}
