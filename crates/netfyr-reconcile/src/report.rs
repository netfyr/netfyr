//! Human-readable and machine-readable diff report for dry-run output.
//!
//! [`DiffReport`] wraps a [`StateDiff`] and adds:
//! - `unchanged_entities`: entity keys present in both desired and actual states
//!   that produced no diff operation (they are identical).
//! - Three output formatters: `format_text` (human-readable terminal output),
//!   `format_yaml`, and `format_json`.

use std::collections::HashSet;

use netfyr_state::{StateSet, Value};
use serde::Serialize;

use crate::diff::{DiffKind, DiffOperation, FieldChangeKind, StateDiff};
use crate::EntityKey;

// ── DiffReport ────────────────────────────────────────────────────────────────

/// A presentable diff report combining operations and unchanged-entity context.
///
/// Constructed via [`DiffReport::new`] from a [`StateDiff`] and the two
/// [`StateSet`]s that were compared. The `unchanged_entities` list is computed
/// automatically: entities present in both desired and actual with no diff
/// operation appear here.
#[derive(Clone, Debug, Serialize)]
pub struct DiffReport {
    /// Ordered list of entity-level operations (Add, Remove, Modify).
    pub operations: Vec<DiffOperation>,
    /// Entity keys that appear in both desired and actual with no changes.
    pub unchanged_entities: Vec<EntityKey>,
}

impl DiffReport {
    /// Constructs a [`DiffReport`] from a [`StateDiff`] and the two source sets.
    ///
    /// `unchanged_entities` is derived by finding entity keys present in both
    /// `desired` and `actual` that have no corresponding operation in `diff`.
    /// The list is sorted for deterministic output.
    pub fn new(diff: StateDiff, desired: &StateSet, actual: &StateSet) -> DiffReport {
        // Build the set of entity keys that have an operation.
        let operated_keys: HashSet<(String, String)> = diff
            .operations
            .iter()
            .map(|op| (op.entity_type.clone(), op.selector.key()))
            .collect();

        // Collect keys present in both desired and actual (the intersection).
        let desired_keys: HashSet<(String, String)> = desired.entities().into_iter().collect();
        let actual_keys: HashSet<(String, String)> = actual.entities().into_iter().collect();

        let mut unchanged_entities: Vec<EntityKey> = desired_keys
            .intersection(&actual_keys)
            .filter(|key| !operated_keys.contains(*key))
            .cloned()
            .collect();

        // Sort for deterministic output: (entity_type, selector_key) alphabetically.
        unchanged_entities.sort();

        DiffReport {
            operations: diff.operations,
            unchanged_entities,
        }
    }

    /// Returns `true` if there are no operations (ignores unchanged_entities).
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// Formats the diff as a human-readable text suitable for terminal output.
    ///
    /// Prefix conventions:
    /// - `+` — entity or field being added.
    /// - `-` — entity or field being removed.
    /// - `~` — entity being modified (header only).
    /// - `    -field: old` / `    +field: new` — scalar value changed.
    /// - List fields show a header (`    field:`) then per-element `+`/`-` lines.
    /// - `+   field: value` — field being added to existing entity.
    /// - `-   field: value` — field being removed from existing entity.
    /// - (4 spaces) `field: value` — field unchanged (shown for context in Modify).
    ///
    /// Operations are sorted by `(entity_type, selector_key)` for determinism.
    /// Unchanged entities are listed at the end as `No changes: ...`.
    pub fn format_text(&self) -> String {
        // Sort operations for deterministic output.
        let mut ops: Vec<&DiffOperation> = self.operations.iter().collect();
        ops.sort_by(|a, b| {
            a.entity_type
                .cmp(&b.entity_type)
                .then_with(|| a.selector.key().cmp(&b.selector.key()))
        });

        let mut lines: Vec<String> = Vec::new();

        for op in ops {
            let selector_key = op.selector.key();
            match op.kind {
                DiffKind::Add => {
                    lines.push(format!("+ {} {}", op.entity_type, selector_key));
                    let mut emitted: HashSet<String> = HashSet::new();
                    for change in &op.field_changes {
                        if let FieldChangeKind::Set { desired, .. } = &change.change {
                            if let Some((prefix, subname)) = change.field_name.split_once('.') {
                                if emitted.insert(prefix.to_string()) {
                                    lines.push(format!("+   {}:", prefix));
                                }
                                lines.push(format!("+     {}: {}", subname, desired.value));
                            } else {
                                lines.push(format!("+   {}: {}", change.field_name, desired.value));
                            }
                        }
                    }
                }
                DiffKind::Remove => {
                    lines.push(format!("- {} {}", op.entity_type, selector_key));
                    let mut emitted: HashSet<String> = HashSet::new();
                    for change in &op.field_changes {
                        if let FieldChangeKind::Unset { current } = &change.change {
                            if let Some((prefix, subname)) = change.field_name.split_once('.') {
                                if emitted.insert(prefix.to_string()) {
                                    lines.push(format!("-   {}:", prefix));
                                }
                                lines.push(format!("-     {}: {}", subname, current.value));
                            } else {
                                lines.push(format!("-   {}: {}", change.field_name, current.value));
                            }
                        }
                    }
                }
                DiffKind::Modify => {
                    lines.push(format!("~ {} {}", op.entity_type, selector_key));
                    let mut emitted: HashSet<String> = HashSet::new();
                    for change in &op.field_changes {
                        let is_sub = change.field_name.contains('.');
                        match &change.change {
                            FieldChangeKind::Set { current: Some(old), desired: new } => {
                                if is_sub {
                                    let (prefix, subname) = change.field_name.split_once('.').unwrap();
                                    if emitted.insert(prefix.to_string()) {
                                        lines.push(format!("    {}:", prefix));
                                    }
                                    if matches!(&old.value, Value::List(_)) || matches!(&new.value, Value::List(_)) {
                                        format_value_list_diff(&mut lines, subname, &old.value, &new.value, "      ");
                                    } else {
                                        lines.push(format!("      -{}: {}", subname, old.value));
                                        lines.push(format!("      +{}: {}", subname, new.value));
                                    }
                                } else if matches!(&old.value, Value::List(_)) || matches!(&new.value, Value::List(_)) {
                                    format_value_list_diff(&mut lines, &change.field_name, &old.value, &new.value, "    ");
                                } else {
                                    lines.push(format!("    -{}: {}", change.field_name, old.value));
                                    lines.push(format!("    +{}: {}", change.field_name, new.value));
                                }
                            }
                            FieldChangeKind::Set { current: None, desired: new } => {
                                if is_sub {
                                    let (prefix, subname) = change.field_name.split_once('.').unwrap();
                                    if emitted.insert(prefix.to_string()) {
                                        lines.push(format!("    {}:", prefix));
                                    }
                                    lines.push(format!("+     {}: {}", subname, new.value));
                                } else {
                                    lines.push(format!("+   {}: {}", change.field_name, new.value));
                                }
                            }
                            FieldChangeKind::Unset { current } => {
                                if is_sub {
                                    let (prefix, subname) = change.field_name.split_once('.').unwrap();
                                    if emitted.insert(prefix.to_string()) {
                                        lines.push(format!("    {}:", prefix));
                                    }
                                    lines.push(format!("-     {}: {}", subname, current.value));
                                } else {
                                    lines.push(format!("-   {}: {}", change.field_name, current.value));
                                }
                            }
                            FieldChangeKind::Unchanged { value } => {
                                if is_sub {
                                    let (prefix, subname) = change.field_name.split_once('.').unwrap();
                                    if emitted.insert(prefix.to_string()) {
                                        lines.push(format!("    {}:", prefix));
                                    }
                                    lines.push(format!("      {}: {}", subname, value.value));
                                } else {
                                    lines.push(format!("    {}: {}", change.field_name, value.value));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Unchanged entities at the end.
        if !self.unchanged_entities.is_empty() {
            let labels: Vec<String> = self
                .unchanged_entities
                .iter()
                .map(|(et, sk)| format!("{} {}", et, sk))
                .collect();
            lines.push(format!("No changes: {}", labels.join(", ")));
        }

        lines.join("\n")
    }

    /// Formats the diff as YAML.
    ///
    /// The full `FieldValue` (including provenance) is included, which is useful
    /// for machine consumption where provenance context is valuable.
    pub fn format_yaml(&self) -> String {
        serde_yaml::to_string(self).unwrap_or_default()
    }

    /// Formats the diff as pretty-printed JSON.
    ///
    /// The full `FieldValue` (including provenance) is included.
    pub fn format_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

fn format_value_list_diff(lines: &mut Vec<String>, field_name: &str, old: &Value, new: &Value, indent: &str) {
    let empty = Vec::new();
    let old_items = match old { Value::List(v) => v.as_slice(), _ => &empty };
    let new_items = match new { Value::List(v) => v.as_slice(), _ => &empty };

    lines.push(format!("{indent}{}:", field_name));
    for item in new_items {
        if !old_items.contains(item) {
            lines.push(format!("  {indent}+{}", format_value_element(item)));
        }
    }
    for item in old_items {
        if !new_items.contains(item) {
            lines.push(format!("  {indent}-{}", format_value_element(item)));
        }
    }
}

fn format_value_element(v: &Value) -> String {
    match v {
        Value::Map(map) => {
            if let Some(Value::IpNetwork(dest)) = map.get("destination") {
                let mut parts = vec![format!("{dest}")];
                if let Some(Value::U64(n)) = map.get("metric") {
                    if *n != 0 { parts.push(format!("metric {n}")); }
                }
                if let Some(Value::U64(m)) = map.get("mtu") {
                    parts.push(format!("mtu {m}"));
                }
                if let Some(Value::U64(t)) = map.get("table") {
                    parts.push(format!("table {t}"));
                }
                if let Some(Value::U64(t)) = map.get("tos") {
                    if *t != 0 { parts.push(format!("tos {t}")); }
                }
                return parts.join(" ");
            }
            // Address map: extract bare CIDR string from the "address" key.
            // Only applies when no "destination" key is present (to avoid
            // ambiguity with route maps that may also carry an address field).
            if !map.contains_key("destination") {
                if let Some(addr) = map.get("address") {
                    return format!("{}", addr);
                }
            }
            format!("{}", v)
        }
        _ => format!("{}", v),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::generate_diff;
    use netfyr_state::{FieldValue, Provenance, SchemaRegistry, Selector, State, StateMetadata, StateSet, Value};

    // ── Test helpers ──────────────────────────────────────────────────────────

    fn fv(v: Value) -> FieldValue {
        FieldValue { value: v, provenance: Provenance::KernelDefault }
    }

    fn make_state(entity_type: &str, name: &str, fields: Vec<(&str, Value)>) -> State {
        let mut s = State {
            entity_type: entity_type.to_string(),
            selector: Selector::with_name(name),
            fields: Default::default(),
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 100,
        };
        for (k, v) in fields {
            s.fields.insert(k.to_string(), fv(v));
        }
        s
    }

    fn addr_list(addrs: &[&str]) -> Value {
        Value::List(addrs.iter().map(|s| Value::String(s.to_string())).collect())
    }

    // ── Scenario: Human-readable format shows additions with + prefix ─────────

    #[test]
    fn test_format_text_shows_additions_with_plus_prefix() {
        let mut desired = StateSet::new();
        desired.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));
        let actual = StateSet::new();
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);
        let text = report.format_text();

        assert!(
            text.contains("+ ethernet eth0"),
            "Add operation header must start with '+ ethernet eth0', got:\n{text}"
        );
        assert!(
            text.contains("+   mtu: 1500"),
            "Add operation field must use '+   mtu: 1500', got:\n{text}"
        );
    }

    #[test]
    fn test_format_text_add_with_list_field_shows_list_value() {
        let mut desired = StateSet::new();
        desired.insert(make_state(
            "ethernet",
            "eth0",
            vec![("addresses", addr_list(&["10.0.2.50/24"]))],
        ));
        let actual = StateSet::new();
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);
        let text = report.format_text();

        assert!(
            text.contains("+   addresses: [10.0.2.50/24]"),
            "list value must be formatted as [...], got:\n{text}"
        );
    }

    // ── Scenario: Human-readable format shows removals with - prefix ──────────

    #[test]
    fn test_format_text_shows_removals_with_minus_prefix() {
        let desired = StateSet::new();
        let mut actual = StateSet::new();
        actual.insert(make_state(
            "vlan",
            "bond0.200",
            vec![
                ("vlan-id", Value::U64(200)),
                ("parent", Value::String("bond0".to_string())),
            ],
        ));
        let schema = SchemaRegistry::new();
        let managed: std::collections::HashSet<(String, String)> =
            [("vlan".to_string(), "bond0.200".to_string())].into_iter().collect();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);
        let text = report.format_text();

        assert!(
            text.contains("- vlan bond0.200"),
            "Remove operation header must start with '- vlan bond0.200', got:\n{text}"
        );
        assert!(
            text.contains("-   vlan-id: 200"),
            "Remove operation field must use '-   vlan-id: 200', got:\n{text}"
        );
    }

    // ── Scenario: Human-readable format shows modifications with arrow ─────────

    #[test]
    fn test_format_text_shows_modifications_with_tilde_header() {
        let mut desired = StateSet::new();
        desired.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))]));
        let mut actual = StateSet::new();
        actual.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);
        let text = report.format_text();

        assert!(
            text.contains("~ ethernet eth0"),
            "Modify operation header must start with '~ ethernet eth0', got:\n{text}"
        );
    }

    #[test]
    fn test_format_text_shows_scalar_change_as_unified_diff_lines() {
        let mut desired = StateSet::new();
        desired.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))]));
        let mut actual = StateSet::new();
        actual.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);
        let text = report.format_text();

        assert!(
            text.contains("    -mtu: 1500"),
            "scalar change must show '    -mtu: 1500' line, got:\n{text}"
        );
        assert!(
            text.contains("    +mtu: 9000"),
            "scalar change must show '    +mtu: 9000' line, got:\n{text}"
        );
    }

    #[test]
    fn test_format_text_shows_added_field_in_modify_with_plus_prefix() {
        // addresses added to existing eth0
        let mut desired = StateSet::new();
        desired.insert(make_state(
            "ethernet",
            "eth0",
            vec![("mtu", Value::U64(1500)), ("addresses", addr_list(&["10.0.1.51/24"]))],
        ));
        let mut actual = StateSet::new();
        actual.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);
        let text = report.format_text();

        // Field added inside Modify shows as "+   addresses: ..."
        assert!(
            text.contains("+   addresses:"),
            "newly added field in a Modify op must use '+   addresses:' prefix, got:\n{text}"
        );
    }

    #[test]
    fn test_format_text_shows_removed_field_in_modify_with_minus_prefix() {
        // addresses removed from eth0
        let mut desired = StateSet::new();
        desired.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));
        let mut actual = StateSet::new();
        actual.insert(make_state(
            "ethernet",
            "eth0",
            vec![("mtu", Value::U64(1500)), ("addresses", addr_list(&["10.0.1.99/24"]))],
        ));
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);
        let text = report.format_text();

        // Field removed inside Modify shows as "-   addresses: ..."
        assert!(
            text.contains("-   addresses:"),
            "removed field in a Modify op must use '-   addresses:' prefix, got:\n{text}"
        );
    }

    #[test]
    fn test_format_text_shows_unchanged_field_in_modify_with_no_prefix() {
        // mtu unchanged, addresses changed
        let mut desired = StateSet::new();
        desired.insert(make_state(
            "ethernet",
            "eth0",
            vec![("mtu", Value::U64(1500)), ("addresses", addr_list(&["10.0.1.51/24"]))],
        ));
        let mut actual = StateSet::new();
        actual.insert(make_state(
            "ethernet",
            "eth0",
            vec![("mtu", Value::U64(1500)), ("addresses", addr_list(&["10.0.1.50/24"]))],
        ));
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);
        let text = report.format_text();

        // Unchanged field shown for context with 4-space indent (no prefix char)
        assert!(
            text.contains("    mtu: 1500"),
            "unchanged field must use 4-space indent with no prefix, got:\n{text}"
        );
    }

    // ── Scenario: List field changes as per-element diff ────────────────────────

    #[test]
    fn test_format_text_shows_list_field_changes_as_per_element_diff() {
        let mut desired = StateSet::new();
        desired.insert(make_state(
            "ethernet",
            "eth0",
            vec![("addresses", addr_list(&["10.0.1.50/24", "10.0.1.51/24"]))],
        ));
        let mut actual = StateSet::new();
        actual.insert(make_state(
            "ethernet",
            "eth0",
            vec![("addresses", addr_list(&["10.0.1.50/24", "10.0.1.99/24"]))],
        ));
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);
        let text = report.format_text();

        assert!(
            text.contains("    addresses:"),
            "list field must show header line '    addresses:', got:\n{text}"
        );
        assert!(
            text.contains("      +10.0.1.51/24"),
            "added element must show '      +10.0.1.51/24', got:\n{text}"
        );
        assert!(
            text.contains("      -10.0.1.99/24"),
            "removed element must show '      -10.0.1.99/24', got:\n{text}"
        );
        assert!(
            !text.contains("10.0.1.50/24"),
            "unchanged element must not appear, got:\n{text}"
        );
    }

    // ── DiffReport::is_empty ──────────────────────────────────────────────────

    #[test]
    fn test_diff_report_is_empty_when_both_states_empty() {
        let desired = StateSet::new();
        let actual = StateSet::new();
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);

        assert!(report.is_empty(), "DiffReport must be empty when both states are empty");
    }

    #[test]
    fn test_diff_report_is_empty_when_states_are_identical() {
        let mut desired = StateSet::new();
        desired.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));
        let mut actual = StateSet::new();
        actual.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);

        assert!(report.is_empty(), "DiffReport must be empty when states are identical");
    }

    #[test]
    fn test_diff_report_is_not_empty_when_there_are_operations() {
        let mut desired = StateSet::new();
        desired.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))]));
        let actual = StateSet::new();
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);

        assert!(!report.is_empty(), "DiffReport must not be empty when there are operations");
    }

    // ── DiffReport::unchanged_entities ───────────────────────────────────────

    #[test]
    fn test_diff_report_unchanged_entities_listed_for_identical_entity() {
        let mut desired = StateSet::new();
        desired.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));
        let mut actual = StateSet::new();
        actual.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);

        assert_eq!(report.unchanged_entities.len(), 1, "should have one unchanged entity");
        assert_eq!(
            report.unchanged_entities[0],
            ("ethernet".to_string(), "eth0".to_string()),
            "unchanged entity must be (ethernet, eth0)"
        );
    }

    #[test]
    fn test_diff_report_unchanged_entities_empty_when_all_entities_changed() {
        let mut desired = StateSet::new();
        desired.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))]));
        let mut actual = StateSet::new();
        actual.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);

        assert!(
            report.unchanged_entities.is_empty(),
            "no unchanged entities when all are modified"
        );
    }

    #[test]
    fn test_diff_report_unchanged_entities_sorted_deterministically() {
        let mut desired = StateSet::new();
        desired.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));
        desired.insert(make_state("bond", "bond0", vec![("mode", Value::String("802.3ad".to_string()))]));
        let mut actual = StateSet::new();
        actual.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));
        actual.insert(make_state("bond", "bond0", vec![("mode", Value::String("802.3ad".to_string()))]));
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);

        // Both entities are unchanged; list must be sorted
        assert_eq!(report.unchanged_entities.len(), 2);
        let mut sorted = report.unchanged_entities.clone();
        sorted.sort();
        assert_eq!(
            report.unchanged_entities, sorted,
            "unchanged_entities must be sorted for deterministic output"
        );
    }

    // ── format_text shows "No changes" footer for unchanged entities ──────────

    #[test]
    fn test_format_text_shows_no_changes_footer_for_unchanged_entities() {
        let mut desired = StateSet::new();
        desired.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));
        let mut actual = StateSet::new();
        actual.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);
        let text = report.format_text();

        assert!(
            text.contains("No changes:"),
            "format_text must contain 'No changes:' footer for unchanged entities, got:\n{text}"
        );
        assert!(
            text.contains("ethernet eth0"),
            "footer must list 'ethernet eth0' as unchanged, got:\n{text}"
        );
    }

    #[test]
    fn test_format_text_shows_no_changes_footer_only_for_unchanged_not_for_changed() {
        // eth0 is modified, eth1 is unchanged
        let mut desired = StateSet::new();
        desired.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))]));
        desired.insert(make_state("ethernet", "eth1", vec![("mtu", Value::U64(1500))]));
        let mut actual = StateSet::new();
        actual.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));
        actual.insert(make_state("ethernet", "eth1", vec![("mtu", Value::U64(1500))]));
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);
        let text = report.format_text();

        // eth0 must appear as a modify operation, not in No changes
        assert!(text.contains("~ ethernet eth0"), "eth0 must appear as Modify, got:\n{text}");
        // eth1 must appear in the No changes footer
        assert!(text.contains("No changes:"), "must have No changes footer, got:\n{text}");
        assert!(
            text.contains("ethernet eth1"),
            "No changes footer must list eth1, got:\n{text}"
        );
    }

    // ── Scenario: Unmanaged entity not in unchanged_entities ─────────────────

    #[test]
    fn test_unmanaged_entity_not_in_unchanged_entities() {
        // Scenario: Unmanaged entity in actual but not desired is completely ignored
        // "ethernet/eth1 is not in unchanged_entities either (completely ignored)"
        // An entity present only in actual and NOT in managed_entities must not appear
        // in unchanged_entities — it is fully invisible to the report.
        let desired = StateSet::new();
        let mut actual = StateSet::new();
        actual.insert(make_state(
            "ethernet",
            "eth1",
            vec![
                ("mtu", Value::U64(1500)),
                ("enabled", Value::Bool(true)),
            ],
        ));
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new(); // eth1 NOT managed

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);

        assert!(
            report.unchanged_entities.is_empty(),
            "unmanaged entity must not appear in unchanged_entities, got: {:?}",
            report.unchanged_entities
        );
        assert!(report.is_empty(), "report must be empty for unmanaged-only actual state");
    }

    #[test]
    fn test_unmanaged_entity_not_in_format_text_output() {
        // Unmanaged entity should be invisible — format_text must not mention it at all.
        let desired = StateSet::new();
        let mut actual = StateSet::new();
        actual.insert(make_state("ethernet", "eth1", vec![("mtu", Value::U64(1500))]));
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);
        let text = report.format_text();

        assert!(
            !text.contains("eth1"),
            "unmanaged eth1 must not appear in format_text output, got:\n{text}"
        );
    }

    // ── Scenario: Unmanaged entity absent from unchanged_entities with managed present ─

    #[test]
    fn test_unchanged_entity_present_but_unmanaged_entity_absent_from_unchanged_list() {
        // eth0 is in both desired and actual (unchanged — managed)
        // eth1 is only in actual and NOT managed (completely ignored)
        // unchanged_entities must contain eth0 but NOT eth1
        let mut desired = StateSet::new();
        desired.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));
        let mut actual = StateSet::new();
        actual.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]));
        actual.insert(make_state("ethernet", "eth1", vec![("mtu", Value::U64(9000))])); // unmanaged
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new(); // no managed needed for eth0 (it's in desired)

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);

        assert_eq!(
            report.unchanged_entities.len(),
            1,
            "only one unchanged entity expected (eth0); eth1 is unmanaged"
        );
        assert_eq!(
            report.unchanged_entities[0],
            ("ethernet".to_string(), "eth0".to_string()),
            "unchanged entity must be (ethernet, eth0)"
        );
    }

    // ── Scenarios 19 & 20: List comparison keys in format_text ───────────────

    /// Build a `Value::Map` address entry (address + valid_lft) via serde_yaml.
    fn make_addr_map(address: &str, valid_lft: u64) -> Value {
        use netfyr_state::yaml::deserialize_value;
        let yaml_str = format!("address: \"{address}\"\nvalid_lft: {valid_lft}");
        let yaml_val: serde_yaml::Value = serde_yaml::from_str(&yaml_str).expect("valid yaml");
        deserialize_value(&yaml_val).expect("valid Value")
    }

    #[test]
    fn test_format_text_list_comparison_keys_same_address_no_output() {
        // Criterion 19: desired and actual share the same address key (10.0.1.50/24)
        // but differ in valid_lft. Because x-netfyr-comparison-keys=["address"] treats
        // them as equal, there must be no operations and no output at all.
        let desired_addrs = Value::List(vec![make_addr_map("10.0.1.50/24", 1800)]);
        let actual_addrs = Value::List(vec![make_addr_map("10.0.1.50/24", 3600)]);

        let mut desired = StateSet::new();
        desired.insert(make_state("ethernet", "eth0", vec![("addresses", desired_addrs)]));
        let mut actual = StateSet::new();
        actual.insert(make_state("ethernet", "eth0", vec![("addresses", actual_addrs)]));

        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);

        assert!(report.is_empty(), "report must be empty: same address key, valid_lft ignored");
        // The only output should be the "No changes" footer — no + / - / ~ lines.
        let text = report.format_text();
        assert!(
            !text.contains('+') || text.contains("No changes:"),
            "with matching address keys there must be no + diff lines in format_text, got:\n{text}"
        );
        assert!(
            !text.contains("    -") && !text.contains("    +"),
            "no field-level diff lines expected, got:\n{text}"
        );
    }

    #[test]
    fn test_format_text_list_comparison_keys_different_address_shows_add_remove_elements() {
        // Criterion 20: desired has 10.0.1.51/24, actual has 10.0.1.50/24.
        // The "address" key differs → Modify op; format_text must show
        // the per-element diff with +10.0.1.51/24 and -10.0.1.50/24.
        let desired_addrs = Value::List(vec![make_addr_map("10.0.1.51/24", 3600)]);
        let actual_addrs = Value::List(vec![make_addr_map("10.0.1.50/24", 3600)]);

        let mut desired = StateSet::new();
        desired.insert(make_state("ethernet", "eth0", vec![("addresses", desired_addrs)]));
        let mut actual = StateSet::new();
        actual.insert(make_state("ethernet", "eth0", vec![("addresses", actual_addrs)]));

        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);
        let text = report.format_text();

        assert!(
            text.contains("    addresses:"),
            "list field header '    addresses:' must appear in format_text, got:\n{text}"
        );
        assert!(
            text.contains("      +10.0.1.51/24"),
            "added address element must show '      +10.0.1.51/24', got:\n{text}"
        );
        assert!(
            text.contains("      -10.0.1.50/24"),
            "removed address element must show '      -10.0.1.50/24', got:\n{text}"
        );
    }

    // ── format_yaml and format_json round-trip ────────────────────────────────

    #[test]
    fn test_format_yaml_produces_non_empty_string_for_nonempty_diff() {
        let mut desired = StateSet::new();
        desired.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))]));
        let actual = StateSet::new();
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);
        let yaml = report.format_yaml();

        assert!(!yaml.is_empty(), "format_yaml must produce non-empty output for a non-empty diff");
        assert!(yaml.contains("Add"), "YAML output must mention Add operation kind");
    }

    #[test]
    fn test_format_json_produces_valid_json_for_nonempty_diff() {
        let mut desired = StateSet::new();
        desired.insert(make_state("ethernet", "eth0", vec![("mtu", Value::U64(9000))]));
        let actual = StateSet::new();
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);
        let json_str = report.format_json();

        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("format_json must produce valid JSON");
        assert!(
            parsed.get("operations").is_some(),
            "JSON output must have 'operations' field"
        );
    }

    // ── Helpers for ipv4 / ipv6 sub-object tests ──────────────────────────────

    fn yaml_val(yaml: &str) -> Value {
        use netfyr_state::yaml::deserialize_value;
        let sv: serde_yaml::Value = serde_yaml::from_str(yaml).expect("valid yaml");
        deserialize_value(&sv).expect("valid Value")
    }

    // ── Criterion 12: format_text Add with ipv4 sub-object shows indented block

    #[test]
    fn test_format_text_add_with_ipv4_sub_object_shows_indented_sub_object() {
        // Criterion 12: when an entity is added and it has an ipv4 sub-object,
        // format_text must emit "+   ipv4:" as a header and "+     addresses: ..."
        // with the sub-field further indented, NOT a raw "ipv4: {addresses: [...]}" blob.
        let mut desired = StateSet::new();
        desired.insert(make_state(
            "ethernet", "eth0",
            vec![("ipv4", yaml_val("addresses:\n  - \"10.0.1.50/24\""))],
        ));
        let actual = StateSet::new();
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);
        let text = report.format_text();

        // Add header
        assert!(text.contains("+ ethernet eth0"), "must show Add header: {text:?}");

        // Sub-object group header "+   ipv4:"  (+ + 3 spaces + "ipv4:")
        assert!(
            text.contains("+   ipv4:"),
            "format_text for Add must emit '+   ipv4:' sub-object header, got: {text:?}"
        );

        // Sub-field "+     addresses: ..." (+ + 5 spaces + "addresses:")
        assert!(
            text.contains("+     addresses:"),
            "format_text for Add must emit '+     addresses:' sub-field, got: {text:?}"
        );

        // The whole IPv4 address must appear somewhere in the output
        assert!(
            text.contains("10.0.1.50/24"),
            "format_text for Add must include the IPv4 address value, got: {text:?}"
        );
    }

    // ── Criterion 15: format_text Modify ipv4 list diff shows sub-object indent

    #[test]
    fn test_format_text_modify_ipv4_addresses_list_diff_shows_sub_object_indentation() {
        // Criterion 15: when an entity is modified and ipv4.addresses changes,
        // format_text must emit the addresses as a per-element diff under "    ipv4:".
        // The list diff section uses "      addresses:" as the sub-field header,
        // then "        +<new>" and "        -<old>" for element changes.
        let old_addr = "10.0.1.99/24";
        let new_addr = "10.0.1.51/24";

        let mut desired = StateSet::new();
        desired.insert(make_state(
            "ethernet", "eth0",
            vec![("ipv4", yaml_val(&format!("addresses:\n  - \"{new_addr}\"")))],
        ));
        let mut actual = StateSet::new();
        actual.insert(make_state(
            "ethernet", "eth0",
            vec![("ipv4", yaml_val(&format!("addresses:\n  - \"{old_addr}\"")))],
        ));
        let schema = SchemaRegistry::new();
        let managed = std::collections::HashSet::<(String, String)>::new();

        let diff = generate_diff(&desired, &actual, &managed, &schema);
        let report = DiffReport::new(diff, &desired, &actual);
        let text = report.format_text();

        // Modify header
        assert!(text.contains("~ ethernet eth0"), "must show Modify header: {text:?}");

        // Sub-object group header "    ipv4:"  (4 spaces)
        assert!(
            text.contains("    ipv4:"),
            "format_text for Modify must emit '    ipv4:' sub-object header, got: {text:?}"
        );

        // List diff sub-field "      addresses:"  (6 spaces)
        assert!(
            text.contains("      addresses:"),
            "format_text for Modify must emit '      addresses:' list diff header, got: {text:?}"
        );

        // Added element "        +10.0.1.51/24"  (8 spaces + "+")
        assert!(
            text.contains(&format!("        +{new_addr}")),
            "format_text must show added address as '        +{new_addr}', got: {text:?}"
        );

        // Removed element "        -10.0.1.99/24"  (8 spaces + "-")
        assert!(
            text.contains(&format!("        -{old_addr}")),
            "format_text must show removed address as '        -{old_addr}', got: {text:?}"
        );
    }
}
