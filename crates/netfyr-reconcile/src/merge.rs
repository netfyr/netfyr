//! Merging the contributions bound to one device — from any number of
//! policies — against the observed baseline (FR-004..FR-006).
//!
//! Two phases, one result:
//!
//! 1. The bound contributions merge into an empty result in precedence
//!    order (highest priority, then key): maps merge key by key, lists
//!    union — the address field by `ip` identity, any other list
//!    structurally, and within one contribution's list as across
//!    contributions — and scalars keep the first value, with later
//!    disagreements recorded for conflict detection (FR-012).
//! 2. The observed device fields overlay only the keys the contributions
//!    did not set, so observed configuration the policy leaves untouched
//!    survives (FR-006). Lists never cross the phase boundary: a
//!    contribution's list replaces the observed list wholesale.
//!
//! Provenance is tracked per leaf and per list element (FR-005); it is
//! output metadata only and never feeds the merge.

use indexmap::IndexMap;

use netfyr_state::Value;
use netfyr_state::plan::InterfaceId;

use crate::{CompetingValue, ConflictIdentity, Contribution, ValueConflict};

/// The address field's path; its list elements merge by address identity
/// rather than structural equality.
const ADDRESS_PATH: &str = "ipv4.addresses";

/// The effective desired state of one bound device, with provenance.
pub(super) struct Merged {
    /// The effective desired fields, in stable order (contribution
    /// insertion order, then overlay order).
    pub fields: IndexMap<String, Value>,
    /// The winning contribution key for each scalar leaf path; for a
    /// list path, the first contribution to set the field, including an
    /// empty one.
    pub leaf_sources: IndexMap<String, String>,
    /// The winning contribution key per element of each list path,
    /// aligned with the merged list.
    pub list_elements: IndexMap<String, Vec<String>>,
    /// Value conflicts among the contributions, in deterministic order.
    pub conflicts: Vec<ValueConflict>,
}

/// Accumulated provenance and conflict evidence from the contribution
/// phase. `scalar_seen` and `address_seen` keep every contribution value
/// at each path / address identity — agreeing ones included — so a
/// conflict is reported only where contributors actually disagree.
#[derive(Default)]
struct Ctx {
    leaf_sources: IndexMap<String, String>,
    list_elements: IndexMap<String, Vec<String>>,
    /// Path -> (contribution key, value), in precedence order.
    scalar_seen: IndexMap<String, Vec<(String, Value)>>,
    /// `"{ip}/{prefix}"` -> (contribution key, address object, ip).
    address_seen: IndexMap<String, Vec<(String, Value, Value)>>,
}

impl Ctx {
    /// Build the value for a position no contribution has set yet,
    /// recording provenance and conflict evidence. Lists deduplicate on
    /// insertion exactly as they do when a later contribution unions into
    /// them (FR-004: the ordered union keeps the first occurrence of each
    /// element, also within one contribution's list). A map is inserted
    /// wholesale, so the recursion must record its inner leaves and
    /// lists, or nested scalars would lose their winning source (FR-005)
    /// and a later conflicting value would be the only recorded evidence
    /// (FR-012).
    fn fresh_value(&mut self, path: &str, key: &str, value: &Value) -> Value {
        match value {
            Value::Map(entries) => Value::Map(
                entries
                    .iter()
                    .map(|(field, inner)| {
                        (
                            field.clone(),
                            self.fresh_value(&join(path, field), key, inner),
                        )
                    })
                    .collect(),
            ),
            Value::List(items) => {
                // The first contribution to set the field owns it, also
                // when its list is empty (FR-005): an explicit empty list
                // must still attribute the removals it causes.
                self.leaf_sources
                    .entry(path.to_string())
                    .or_insert_with(|| key.to_string());
                let mut existing = Vec::new();
                self.on_union(path, key, &mut existing, items);
                Value::List(existing)
            }
            scalar => {
                self.leaf_sources.insert(path.to_string(), key.to_string());
                self.scalar_seen
                    .entry(path.to_string())
                    .or_default()
                    .push((key.to_string(), scalar.clone()));
                scalar.clone()
            }
        }
    }

    /// Two lists at the same path: union `incoming`'s elements into
    /// `existing`. Address-list elements are identity-merged by `ip` —
    /// the first occurrence is kept and a later one is evidence for a
    /// conflict; other lists dedupe structurally, keeping the first
    /// occurrence of each element.
    fn on_union(&mut self, path: &str, key: &str, existing: &mut Vec<Value>, incoming: &[Value]) {
        for element in incoming {
            let identity = if path == ADDRESS_PATH {
                address_ip(element)
            } else {
                None
            };
            let present = match identity {
                Some((addr, prefix)) => existing
                    .iter()
                    .any(|e| address_ip(e).is_some_and(|(a, p)| a == addr && p == prefix)),
                None => existing.iter().any(|e| e == element),
            };
            if present {
                // First occurrence (higher precedence) keeps its
                // provenance; the later value is conflict evidence only
                // for the address field.
                if path == ADDRESS_PATH {
                    self.record_address(key, element);
                }
            } else {
                existing.push(element.clone());
                self.list_elements
                    .entry(path.to_string())
                    .or_default()
                    .push(key.to_string());
                if path == ADDRESS_PATH {
                    self.record_address(key, element);
                }
            }
        }
    }

    /// A value arrived at a path already occupied by an earlier
    /// contribution: the existing value stays (first wins); the incoming
    /// scalar is recorded as conflict evidence. Type mismatches (scalar
    /// vs map, scalar vs list) are rejected by schema validation before
    /// the merge runs.
    fn on_scalar_conflict(&mut self, path: &str, key: &str, incoming: &Value) {
        match incoming {
            Value::Map(_) | Value::List(_) => {}
            scalar => {
                self.scalar_seen
                    .entry(path.to_string())
                    .or_default()
                    .push((key.to_string(), scalar.clone()));
            }
        }
    }

    /// Record an address-list element under its `ip` identity.
    fn record_address(&mut self, key: &str, element: &Value) {
        let Some((addr, prefix)) = address_ip(element) else {
            return;
        };
        let identity = format!("{addr}/{prefix}");
        self.address_seen.entry(identity).or_default().push((
            key.to_string(),
            element.clone(),
            Value::IpNetwork((*addr, prefix)),
        ));
    }

    /// The value conflicts among the contributions: any scalar path or
    /// address identity with two or more recorded values that do not all
    /// agree. Contributors are kept in precedence order, so the first is
    /// the winner (FR-012).
    fn conflicts(&self, target: &InterfaceId) -> Vec<ValueConflict> {
        let mut out = Vec::new();
        for (path, values) in &self.scalar_seen {
            if values.len() >= 2 && values[1..].iter().any(|(_, v)| v != &values[0].1) {
                out.push(ValueConflict {
                    target: target.clone(),
                    identity: ConflictIdentity::Field { path: path.clone() },
                    competing: values
                        .iter()
                        .map(|(source, value)| CompetingValue {
                            source: source.clone(),
                            value: value.clone(),
                        })
                        .collect(),
                    winner: CompetingValue {
                        source: values[0].0.clone(),
                        value: values[0].1.clone(),
                    },
                });
            }
        }
        for (_, entries) in &self.address_seen {
            if entries.len() >= 2 && entries[1..].iter().any(|(_, v, _)| v != &entries[0].1) {
                out.push(ValueConflict {
                    target: target.clone(),
                    identity: ConflictIdentity::Address {
                        ip: entries[0].2.clone(),
                    },
                    competing: entries
                        .iter()
                        .map(|(source, value, _)| CompetingValue {
                            source: source.clone(),
                            value: value.clone(),
                        })
                        .collect(),
                    winner: CompetingValue {
                        source: entries[0].0.clone(),
                        value: entries[0].1.clone(),
                    },
                });
            }
        }
        out
    }
}

/// Merge one bound device: all contributions whose selector matched it,
/// in precedence order, then the observed baseline for keys they left
/// unset.
pub(super) fn merge(
    target: &InterfaceId,
    contributions: &[&Contribution],
    observed: &IndexMap<String, Value>,
) -> Merged {
    let mut ctx = Ctx::default();
    let mut fields: IndexMap<String, Value> = IndexMap::new();
    for contribution in contributions {
        merge_child(
            &mut ctx,
            &mut fields,
            "",
            &contribution.key,
            &contribution.fields,
        );
    }
    overlay_observed(&mut fields, observed);
    let conflicts = ctx.conflicts(target);
    Merged {
        fields,
        leaf_sources: ctx.leaf_sources,
        list_elements: ctx.list_elements,
        conflicts,
    }
}

/// Recursively merge one contribution's `fields` into `map` rooted at
/// `prefix`, recording provenance for every newly inserted position.
fn merge_child(
    ctx: &mut Ctx,
    map: &mut IndexMap<String, Value>,
    prefix: &str,
    key: &str,
    fields: &IndexMap<String, Value>,
) {
    for (field, value) in fields {
        let path = join(prefix, field);
        match map.get_mut(field) {
            None => {
                map.insert(field.clone(), ctx.fresh_value(&path, key, value));
            }
            Some(existing) => merge_pair(ctx, existing, key, value, &path),
        }
    }
}

/// Merge `incoming` into an `existing` slot already occupied at `path`:
/// maps merge recursively, lists union, anything else conflicts
/// first-wins.
fn merge_pair(ctx: &mut Ctx, existing: &mut Value, key: &str, incoming: &Value, path: &str) {
    match (existing, incoming) {
        (Value::Map(existing_map), Value::Map(incoming_map)) => {
            for (field, value) in incoming_map {
                let sub_path = join(path, field);
                match existing_map.get_mut(field) {
                    None => {
                        existing_map.insert(field.clone(), ctx.fresh_value(&sub_path, key, value));
                    }
                    Some(slot) => merge_pair(ctx, slot, key, value, &sub_path),
                }
            }
        }
        (Value::List(existing_list), Value::List(incoming_list)) => {
            ctx.on_union(path, key, existing_list, incoming_list);
        }
        _ => ctx.on_scalar_conflict(path, key, incoming),
    }
}

/// Phase 2: observed values fill the keys the contributions left unset.
/// Maps merge recursively (a partial contribution map keeps the observed
/// keys it did not set); otherwise the contribution's value wins,
/// including whole lists.
fn overlay_observed(desired: &mut IndexMap<String, Value>, observed: &IndexMap<String, Value>) {
    for (field, observed_value) in observed {
        match desired.get_mut(field) {
            None => {
                desired.insert(field.clone(), observed_value.clone());
            }
            Some(Value::Map(desired_map)) if matches!(observed_value, Value::Map(_)) => {
                overlay_into(desired_map, observed_value);
            }
            _ => {}
        }
    }
}

/// The recursive body of [`overlay_observed`] for one nested map level.
fn overlay_into(desired: &mut IndexMap<String, Value>, observed: &Value) {
    let Value::Map(observed_map) = observed else {
        return;
    };
    for (field, value) in observed_map {
        match desired.get_mut(field) {
            None => {
                desired.insert(field.clone(), value.clone());
            }
            Some(Value::Map(inner)) if matches!(value, Value::Map(_)) => overlay_into(inner, value),
            _ => {}
        }
    }
}

/// Join a dot path prefix with one more field.
fn join(prefix: &str, field: &str) -> String {
    if prefix.is_empty() {
        field.to_string()
    } else {
        format!("{prefix}.{field}")
    }
}

fn address_ip(address: &Value) -> Option<(&std::net::IpAddr, u8)> {
    match address {
        Value::Map(map) => match map.get("ip") {
            Some(Value::IpNetwork((address, prefix))) => Some((address, *prefix)),
            _ => None,
        },
        _ => None,
    }
}
