//! Deterministic reconciliation-plan rendering.

use serde_json::{Value as JsonValue, json};

use netfyr_state::plan::InterfaceId;
use netfyr_state::{Match, Value};

use crate::{Diagnostic, ReconcilePlan};

pub fn render_text(plan: &ReconcilePlan) -> String {
    let mut text = String::new();
    for change in &plan.diff().changes {
        let source = plan
            .provenance
            .iter()
            .find(|entry| entry.target == change.target && entry.field == change.field)
            .and_then(|entry| entry.source.as_deref())
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
            "identity": format!("{:?}", conflict.identity),
            "winner": conflict.winner.source,
        }),
        Diagnostic::UnmatchedPolicy(unmatched) => {
            json!({
                "kind": "unmatched-policy",
                "policy": unmatched.policy,
                "selector": selector_json(&unmatched.selector),
            })
        }
        Diagnostic::BindingConflict(conflict) => json!({
            "kind": "binding-conflict",
            "interface": interface_json(&conflict.target),
            "policies": conflict.policies.iter().map(|policy| &policy.name).collect::<Vec<_>>(),
        }),
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

pub(crate) fn diagnostic_text(diagnostic: &Diagnostic) -> String {
    match diagnostic {
        Diagnostic::ValueConflict(conflict) => format!(
            "value conflict on {}: {:?}",
            conflict.target.name, conflict.identity
        ),
        Diagnostic::UnmatchedPolicy(unmatched) => {
            format!(
                "policy {} selector {} matched no interfaces",
                unmatched.policy, unmatched.selector
            )
        }
        Diagnostic::BindingConflict(conflict) => format!(
            "interface {} matched policies {}",
            conflict.target.name,
            conflict
                .policies
                .iter()
                .map(|policy| policy.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}
