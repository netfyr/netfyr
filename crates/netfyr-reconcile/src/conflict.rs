use std::collections::HashMap;

use netfyr_state::{FieldValue, Value};

use crate::{EntityKey, FieldName, PolicyId};

// ── ConflictContribution ──────────────────────────────────────────────────────

/// One policy's contribution to a field conflict.
#[derive(Clone, Debug)]
pub struct ConflictContribution {
    /// The policy whose value is in conflict.
    pub policy_id: PolicyId,
    /// The value that this policy provided (including provenance metadata).
    pub value: FieldValue,
}

// ── Conflict ──────────────────────────────────────────────────────────────────

/// A field-level conflict detected during reconciliation.
///
/// Occurs when two or more policies at the same highest priority provide
/// different values for the same field on the same entity.
#[derive(Clone, Debug)]
pub struct Conflict {
    /// The entity where the conflict occurs: `(entity_type, selector.key())`.
    pub entity_key: EntityKey,
    /// The name of the conflicting field.
    pub field_name: FieldName,
    /// The priority level at which the conflict occurs.
    pub priority: u32,
    /// All conflicting contributions, one per policy at the highest priority.
    pub contributions: Vec<ConflictContribution>,
}

// ── ConflictReport ────────────────────────────────────────────────────────────

/// A collection of field-level conflicts detected during a reconciliation run.
#[derive(Clone, Debug, Default)]
pub struct ConflictReport {
    /// Each element represents one unresolvable field conflict.
    pub conflicts: Vec<Conflict>,
}

impl ConflictReport {
    /// Returns a new, empty `ConflictReport`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if no conflicts were detected.
    pub fn is_empty(&self) -> bool {
        self.conflicts.is_empty()
    }

    /// Returns the number of conflicts.
    pub fn len(&self) -> usize {
        self.conflicts.len()
    }

    /// Groups conflicts by entity key for display or further processing.
    pub fn by_entity(&self) -> HashMap<EntityKey, Vec<&Conflict>> {
        let mut map: HashMap<EntityKey, Vec<&Conflict>> = HashMap::new();
        for conflict in &self.conflicts {
            map.entry(conflict.entity_key.clone()).or_default().push(conflict);
        }
        map
    }

    /// Formats a human-readable summary of all conflicts.
    ///
    /// Returns an empty string if there are no conflicts.
    ///
    /// Example output:
    /// ```text
    /// CONFLICTS:
    ///   ethernet eth0:
    ///     mtu: policy "eth0-team-a" sets 9000, policy "eth0-team-b" sets 1500 (both priority 100)
    /// ```
    pub fn summary(&self) -> String {
        if self.conflicts.is_empty() {
            return String::new();
        }

        let mut out = String::from("CONFLICTS:\n");

        // Group by entity and iterate in a stable order.
        let by_entity = self.by_entity();
        let mut entity_keys: Vec<&EntityKey> = by_entity.keys().collect();
        entity_keys.sort();

        for entity_key in entity_keys {
            let conflicts = &by_entity[entity_key];
            let (entity_type, selector_key) = entity_key;
            out.push_str(&format!("  {} {}:\n", entity_type, selector_key));

            // Sort fields alphabetically for deterministic output.
            let mut sorted_conflicts: Vec<&&Conflict> = conflicts.iter().collect();
            sorted_conflicts.sort_by_key(|c| &c.field_name);

            for conflict in sorted_conflicts {
                // Build the contribution descriptions.
                let contribs: Vec<String> = conflict
                    .contributions
                    .iter()
                    .map(|c| format!("policy \"{}\" sets {}", c.policy_id, c.value.value))
                    .collect();

                let contribs_str = contribs.join(", ");

                let priority_note = if conflict.contributions.len() == 2 {
                    format!("(both priority {})", conflict.priority)
                } else {
                    format!("(all priority {})", conflict.priority)
                };

                out.push_str(&format!(
                    "    {}: {} {}\n",
                    conflict.field_name, contribs_str, priority_note
                ));
            }
        }

        out
    }
}

// ── Conflict-aware equality ───────────────────────────────────────────────────

/// Checks whether two `Value`s are equal for conflict-detection purposes.
///
/// For `Value::List`, comparison is **order-insensitive**: the two lists are
/// compared as multisets (by sorting their string representations).  This
/// prevents false conflicts when two policies provide the same set of addresses
/// in different order.
///
/// For all other variants, standard `PartialEq` is used.
pub fn values_equal_for_conflict(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::List(la), Value::List(lb)) => {
            if la.len() != lb.len() {
                return false;
            }
            // Sort by Display representation — all Value variants implement Display.
            // Within a single field, list elements should be the same type, so
            // Display gives a stable, deterministic sort key.
            let mut sa: Vec<String> = la.iter().map(|v| v.to_string()).collect();
            let mut sb: Vec<String> = lb.iter().map(|v| v.to_string()).collect();
            sa.sort();
            sb.sort();
            sa == sb
        }
        _ => a == b,
    }
}
