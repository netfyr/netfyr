//! Deterministic reconciliation-plan rendering.

use serde_json::{Value as JsonValue, json};

use netfyr_state::plan::{FieldChange, InterfaceId};
use netfyr_state::{Match, Value};

use crate::{ConflictIdentity, Diagnostic, ReconcilePlan};

/// The winning contribution key recorded for one changed field, if any.
fn change_source<'a>(plan: &'a ReconcilePlan, change: &FieldChange) -> Option<&'a str> {
    plan.provenance
        .iter()
        .find(|entry| entry.target == change.target && entry.field == change.field)
        .and_then(|entry| entry.source.as_deref())
}

pub fn render_text(plan: &ReconcilePlan) -> String {
    let mut text = String::new();
    for change in &plan.diff().changes {
        let source = change_source(plan, change)
            .map(|source| format!(" (source: {source})"))
            .unwrap_or_default();
        text.push_str(&format!(
            "{}/{} {}: {} -> {}{}\n",
            change.target.namespace.inode,
            change.target.name,
            change.field,
            change
                .old
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            change
                .desired
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            source,
        ));
    }
    for diagnostic in &plan.diagnostics {
        text.push_str(&format!("diagnostic: {}\n", diagnostic_text(diagnostic)));
    }
    text
}

pub fn render_json(plan: &ReconcilePlan) -> JsonValue {
    json!({
        "schema_version": 1,
        "changes": plan.diff().changes.iter().map(|change| json!({
            "interface": interface_json(&change.target),
            "field": change.field,
            "old": change.old.as_ref().map(value_json),
            "desired": change.desired.as_ref().map(value_json),
            "source": change_source(plan, change),
        })).collect::<Vec<_>>(),
        "operations": plan.diff().operations.iter().map(|operation| json!({
            "interface": interface_json(&operation.target),
            "kind": operation.kind.kind_name(),
            "depends_on": operation.depends_on,
            "address_group": operation.address_group.as_ref().map(|group| group.addresses.iter().map(|entry| address_text(&entry.address)).collect::<Vec<_>>()),
            "address": operation.kind.ipv4_address().map(address_text),
        })).collect::<Vec<_>>(),
        "diagnostics": plan.diagnostics.iter().map(diagnostic_json).collect::<Vec<_>>(),
    })
}

fn address_text(address: &netfyr_state::plan::Ipv4Address) -> String {
    format!("{}/{}", address.ip, address.prefix_len)
}

fn interface_json(interface: &InterfaceId) -> JsonValue {
    json!({
        "namespace": { "device": interface.namespace.device, "inode": interface.namespace.inode },
        "index": interface.index,
        "generation": interface.generation,
        "name": interface.name,
    })
}

fn value_json(value: &Value) -> JsonValue {
    match value {
        Value::String(value) => json!(value),
        Value::U64(value) => json!(value),
        Value::I64(value) => json!(value),
        Value::Bool(value) => json!(value),
        Value::IpAddr(value) => json!(value.to_string()),
        Value::IpNetwork((value, prefix)) => json!(format!("{value}/{prefix}")),
        Value::List(values) => JsonValue::Array(values.iter().map(value_json).collect()),
        Value::Map(values) => JsonValue::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), value_json(value)))
                .collect(),
        ),
    }
}

fn diagnostic_json(diagnostic: &Diagnostic) -> JsonValue {
    match diagnostic {
        Diagnostic::ValueConflict(conflict) => json!({
            "kind": "value-conflict",
            "interface": interface_json(&conflict.target),
            "identity": identity_json(&conflict.identity),
            "competing": conflict.competing.iter().map(|competing| json!({
                "source": &competing.source,
                "value": value_json(&competing.value),
            })).collect::<Vec<_>>(),
            "winner": json!({
                "source": &conflict.winner.source,
                "value": value_json(&conflict.winner.value),
            }),
        }),
        Diagnostic::UnmatchedPolicy(unmatched) => {
            json!({
                "kind": "unmatched-policy",
                "policy": unmatched.policy,
                "selector": selector_json(&unmatched.selector),
            })
        }
    }
}

fn selector_json(selector: &Match) -> JsonValue {
    let mut fields = serde_json::Map::new();
    for (name, value) in [
        ("name", &selector.name),
        ("type", &selector.r#type),
        ("driver", &selector.driver),
        ("pci_path", &selector.pci_path),
        ("mac", &selector.mac),
    ] {
        if let Some(value) = value {
            fields.insert(name.into(), json!(value));
        }
    }
    JsonValue::Object(fields)
}

/// The conflicted field path or address identity, as structured JSON.
fn identity_json(identity: &ConflictIdentity) -> JsonValue {
    match identity {
        ConflictIdentity::Field { path } => json!({ "field": path }),
        ConflictIdentity::Address { ip } => json!({ "address": value_json(ip) }),
    }
}

pub(crate) fn diagnostic_text(diagnostic: &Diagnostic) -> String {
    match diagnostic {
        Diagnostic::ValueConflict(conflict) => {
            let identity = match &conflict.identity {
                ConflictIdentity::Field { path } => format!("field {path}"),
                ConflictIdentity::Address { ip } => format!("address {ip}"),
            };
            let winner = format!("{} ({})", conflict.winner.source, conflict.winner.value);
            let losers: Vec<_> = conflict
                .competing
                .iter()
                .filter(|competing| {
                    competing.source != conflict.winner.source
                        || competing.value != conflict.winner.value
                })
                .map(|competing| format!("{} ({})", competing.source, competing.value))
                .collect();
            format!(
                "value conflict on {} {identity}: {winner} wins over {}",
                conflict.target.name,
                losers.join(", ")
            )
        }
        Diagnostic::UnmatchedPolicy(unmatched) => {
            format!(
                "policy {} selector {} matched no interfaces",
                unmatched.policy, unmatched.selector
            )
        }
    }
}
