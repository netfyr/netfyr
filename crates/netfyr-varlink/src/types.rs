//! Wire-format types for the Varlink API.
//!
//! These types map directly to the Varlink IDL defined in `io.netfyr.varlink`.
//! They are distinct from the domain types in `netfyr-state`, `netfyr-policy`,
//! etc. — conversion between wire types and domain types is handled by the
//! `From`/`TryFrom` impls in this module.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use netfyr_backend::{AppliedOperation, ApplyReport, FailedOperation, SkippedOperation};
use netfyr_policy::{FactoryType, Policy};
use netfyr_reconcile::{
    Conflict,
    DiffKind,
    FieldChangeKind as ReconcileFieldChangeKind,
    StateDiff as ReconcileStateDiff,
};
use netfyr_state::{FieldValue, MacAddr, Provenance, Selector, State, StateMetadata, Value};

// ── VarlinkSelector ───────────────────────────────────────────────────────────

/// Wire-format selector. The `type` field corresponds to `Selector.type_`.
/// Field is renamed because `type` is a reserved keyword in Rust.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VarlinkSelector {
    /// Technology type filter (e.g., `"ethernet"`). Renamed from Rust keyword `type`.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub entity_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// MAC address as a lowercase colon-separated hex string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_path: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,
}

impl From<&Selector> for VarlinkSelector {
    fn from(sel: &Selector) -> Self {
        VarlinkSelector {
            entity_type: sel.type_.clone(),
            name: sel.name.clone(),
            driver: sel.driver.clone(),
            // Serialize MacAddr to lowercase colon-separated string.
            mac: sel.mac.as_ref().map(|m| m.to_string()),
            pci_path: sel.pci_path.clone(),
            labels: sel.labels.clone(),
        }
    }
}

impl From<VarlinkSelector> for Selector {
    fn from(vs: VarlinkSelector) -> Self {
        Selector {
            type_: vs.entity_type,
            name: vs.name,
            driver: vs.driver,
            // Parse MAC string; if parsing fails, treat as absent.
            mac: vs.mac.as_deref().and_then(|s| s.parse::<MacAddr>().ok()),
            pci_path: vs.pci_path,
            labels: vs.labels,
        }
    }
}

// ── VarlinkStateDef ───────────────────────────────────────────────────────────

/// Wire-format state definition, used inside `VarlinkPolicy` to carry inline
/// desired state. Fields are serialized as a flat JSON object.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VarlinkStateDef {
    pub entity_type: String,
    pub selector: VarlinkSelector,
    pub fields: serde_json::Map<String, serde_json::Value>,
}

impl From<&State> for VarlinkStateDef {
    fn from(state: &State) -> Self {
        VarlinkStateDef {
            entity_type: state.entity_type.clone(),
            selector: VarlinkSelector::from(&state.selector),
            fields: state_fields_to_json(&state.fields),
        }
    }
}

impl TryFrom<VarlinkStateDef> for State {
    type Error = String;

    fn try_from(sd: VarlinkStateDef) -> Result<Self, Self::Error> {
        let fields = json_to_state_fields(&sd.fields)?;
        Ok(State {
            entity_type: sd.entity_type,
            selector: Selector::from(sd.selector),
            fields,
            metadata: StateMetadata::new(),
            // Provenance and priority are set by the daemon during policy processing.
            policy_ref: None,
            priority: 0,
        })
    }
}

// ── VarlinkState ──────────────────────────────────────────────────────────────

/// Wire-format state returned by `Query`. Represents actual observed system state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VarlinkState {
    pub entity_type: String,
    pub selector: VarlinkSelector,
    pub fields: serde_json::Map<String, serde_json::Value>,
}

impl From<&State> for VarlinkState {
    fn from(state: &State) -> Self {
        VarlinkState {
            entity_type: state.entity_type.clone(),
            selector: VarlinkSelector::from(&state.selector),
            // Only the value is included; provenance/metadata are internal details.
            fields: state_fields_to_json(&state.fields),
        }
    }
}

// ── VarlinkPolicy ─────────────────────────────────────────────────────────────

/// Wire-format policy, used in `SubmitPolicies` and `DryRun` requests.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VarlinkPolicy {
    pub name: String,
    pub factory: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<VarlinkSelector>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<VarlinkStateDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub states: Option<Vec<VarlinkStateDef>>,
}

impl From<&Policy> for VarlinkPolicy {
    fn from(policy: &Policy) -> Self {
        let factory = match policy.factory_type {
            FactoryType::Static => "static",
            FactoryType::Dhcpv4 => "dhcpv4",
        };
        VarlinkPolicy {
            name: policy.name.clone(),
            factory: factory.to_string(),
            priority: Some(i64::from(policy.priority)),
            selector: policy.selector.as_ref().map(VarlinkSelector::from),
            state: policy.state.as_ref().map(VarlinkStateDef::from),
            states: policy.states.as_ref().map(|states| {
                states.iter().map(VarlinkStateDef::from).collect()
            }),
        }
    }
}

impl TryFrom<VarlinkPolicy> for Policy {
    type Error = String;

    fn try_from(vp: VarlinkPolicy) -> Result<Self, Self::Error> {
        let factory_type = match vp.factory.as_str() {
            "static" => FactoryType::Static,
            "dhcpv4" => FactoryType::Dhcpv4,
            other => return Err(format!("unknown factory type: '{other}'")),
        };

        let priority: u32 = vp
            .priority
            .map(|p| {
                u32::try_from(p)
                    .map_err(|_| format!("priority out of u32 range: {p}"))
            })
            .transpose()?
            .unwrap_or(100);

        let selector = vp.selector.map(Selector::from);

        let state = vp
            .state
            .map(State::try_from)
            .transpose()?;

        let states = vp
            .states
            .map(|sds| {
                sds.into_iter()
                    .map(State::try_from)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;

        Ok(Policy {
            name: vp.name,
            factory_type,
            priority,
            state,
            states,
            selector,
        })
    }
}

// ── VarlinkChangeEntry ────────────────────────────────────────────────────────

/// Wire-format representation of a single apply operation result.
/// Flattens `AppliedOperation`, `FailedOperation`, and `SkippedOperation`
/// into a unified entry with a `status` discriminant.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VarlinkChangeEntry {
    /// Operation kind: `"add"`, `"modify"`, or `"remove"`.
    pub kind: String,
    pub entity_type: String,
    /// Stable string key from the entity's selector.
    pub entity_name: String,
    /// Human-readable description of the change or failure.
    pub description: String,
    /// `"applied"`, `"failed"`, or `"skipped"`.
    pub status: String,
}

impl From<&AppliedOperation> for VarlinkChangeEntry {
    fn from(op: &AppliedOperation) -> Self {
        let description = if op.fields_changed.is_empty() {
            "no fields changed".to_string()
        } else {
            format!("changed fields: {}", op.fields_changed.join(", "))
        };
        VarlinkChangeEntry {
            kind: op.operation.to_string(),
            entity_type: op.entity_type.clone(),
            entity_name: op.selector.key(),
            description,
            status: "applied".to_string(),
        }
    }
}

impl From<&FailedOperation> for VarlinkChangeEntry {
    fn from(op: &FailedOperation) -> Self {
        VarlinkChangeEntry {
            kind: op.operation.to_string(),
            entity_type: op.entity_type.clone(),
            entity_name: op.selector.key(),
            description: op.error.to_string(),
            status: "failed".to_string(),
        }
    }
}

impl From<&SkippedOperation> for VarlinkChangeEntry {
    fn from(op: &SkippedOperation) -> Self {
        VarlinkChangeEntry {
            kind: op.operation.to_string(),
            entity_type: op.entity_type.clone(),
            entity_name: op.selector.key(),
            description: op.reason.clone(),
            status: "skipped".to_string(),
        }
    }
}

// ── VarlinkConflictEntry ──────────────────────────────────────────────────────

/// Wire-format representation of a field conflict detected during reconciliation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VarlinkConflictEntry {
    pub entity_type: String,
    pub entity_name: String,
    pub field_name: String,
    /// Policy IDs contributing conflicting values.
    pub policies: Vec<String>,
    /// String representations of the conflicting values.
    pub values: Vec<String>,
}

impl From<&Conflict> for VarlinkConflictEntry {
    fn from(conflict: &Conflict) -> Self {
        let (entity_type, entity_name) = conflict.entity_key.clone();
        let policies = conflict
            .contributions
            .iter()
            .map(|c| c.policy_id.to_string())
            .collect();
        let values = conflict
            .contributions
            .iter()
            .map(|c| c.value.value.to_string())
            .collect();
        VarlinkConflictEntry {
            entity_type,
            entity_name,
            field_name: conflict.field_name.clone(),
            policies,
            values,
        }
    }
}

// ── VarlinkApplyReport ────────────────────────────────────────────────────────

/// Wire-format apply report returned by `SubmitPolicies`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VarlinkApplyReport {
    pub succeeded: i64,
    pub failed: i64,
    pub skipped: i64,
    pub changes: Vec<VarlinkChangeEntry>,
    pub conflicts: Vec<VarlinkConflictEntry>,
}

impl From<ApplyReport> for VarlinkApplyReport {
    fn from(report: ApplyReport) -> Self {
        let succeeded = report.succeeded.len() as i64;
        let failed = report.failed.len() as i64;
        let skipped = report.skipped.len() as i64;

        let mut changes: Vec<VarlinkChangeEntry> = Vec::new();
        for op in &report.succeeded {
            changes.push(VarlinkChangeEntry::from(op));
        }
        for op in &report.failed {
            changes.push(VarlinkChangeEntry::from(op));
        }
        for op in &report.skipped {
            changes.push(VarlinkChangeEntry::from(op));
        }

        VarlinkApplyReport {
            succeeded,
            failed,
            skipped,
            changes,
            conflicts: Vec::new(),
        }
    }
}

/// Converts an `ApplyReport` combined with a `ConflictReport` into a
/// `VarlinkApplyReport` that includes conflict entries.
pub fn convert_apply_report_with_conflicts(
    report: ApplyReport,
    conflicts: &netfyr_reconcile::ConflictReport,
) -> VarlinkApplyReport {
    let mut result = VarlinkApplyReport::from(report);
    result.conflicts = conflicts
        .conflicts
        .iter()
        .map(VarlinkConflictEntry::from)
        .collect();
    result
}

// ── VarlinkFieldChange ────────────────────────────────────────────────────────

/// Wire-format representation of a single field change within a diff operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VarlinkFieldChange {
    pub field_name: String,
    /// `"set"`, `"unset"`, or `"unchanged"`.
    pub change_kind: String,
    /// Current value before the change (present for `set`-with-old-value, `unset`, `unchanged`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<serde_json::Value>,
    /// Desired value after the change (present for `set`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desired: Option<serde_json::Value>,
}

impl From<&netfyr_reconcile::FieldChange> for VarlinkFieldChange {
    fn from(fc: &netfyr_reconcile::FieldChange) -> Self {
        match &fc.change {
            ReconcileFieldChangeKind::Set { current, desired } => VarlinkFieldChange {
                field_name: fc.field_name.clone(),
                change_kind: "set".to_string(),
                current: current.as_ref().map(|fv| value_to_json(&fv.value)),
                desired: Some(value_to_json(&desired.value)),
            },
            ReconcileFieldChangeKind::Unset { current } => VarlinkFieldChange {
                field_name: fc.field_name.clone(),
                change_kind: "unset".to_string(),
                current: Some(value_to_json(&current.value)),
                desired: None,
            },
            ReconcileFieldChangeKind::Unchanged { value } => VarlinkFieldChange {
                field_name: fc.field_name.clone(),
                change_kind: "unchanged".to_string(),
                current: Some(value_to_json(&value.value)),
                desired: None,
            },
        }
    }
}

// ── VarlinkDiffOperation ──────────────────────────────────────────────────────

/// Wire-format representation of a single entity-level diff operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VarlinkDiffOperation {
    /// `"add"`, `"remove"`, or `"modify"`.
    pub kind: String,
    pub entity_type: String,
    /// Stable string key from the entity's selector.
    pub entity_name: String,
    pub field_changes: Vec<VarlinkFieldChange>,
}

impl From<&netfyr_reconcile::DiffOperation> for VarlinkDiffOperation {
    fn from(op: &netfyr_reconcile::DiffOperation) -> Self {
        let kind = match op.kind {
            DiffKind::Add => "add",
            DiffKind::Remove => "remove",
            DiffKind::Modify => "modify",
        };
        VarlinkDiffOperation {
            kind: kind.to_string(),
            entity_type: op.entity_type.clone(),
            entity_name: op.selector.key(),
            field_changes: op.field_changes.iter().map(VarlinkFieldChange::from).collect(),
        }
    }
}

// ── VarlinkStateDiff ──────────────────────────────────────────────────────────

/// Wire-format diff returned by `DryRun`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VarlinkStateDiff {
    pub operations: Vec<VarlinkDiffOperation>,
}

impl From<ReconcileStateDiff> for VarlinkStateDiff {
    fn from(diff: ReconcileStateDiff) -> Self {
        VarlinkStateDiff {
            operations: diff.operations.iter().map(VarlinkDiffOperation::from).collect(),
        }
    }
}

// ── VarlinkFactoryStatus ──────────────────────────────────────────────────────

/// Wire-format status of a single running factory (e.g., a DHCPv4 factory).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VarlinkFactoryStatus {
    pub policy_id: String,
    pub factory_type: String,
    pub interface_name: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_ip: Option<String>,
    /// Full CIDR address from the lease (e.g., `"192.168.122.63/24"`).
    /// Present when state is `"running"`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub lease_address: Option<String>,
    /// Total lease duration in seconds as granted by the DHCP server.
    /// Present when state is `"running"`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub lease_time_secs: Option<i64>,
    /// Seconds until the lease expires, computed at query time.
    /// Present when state is `"running"`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub lease_remaining_secs: Option<i64>,
}

// ── VarlinkDaemonStatus ───────────────────────────────────────────────────────

/// Wire-format daemon status returned by `GetStatus`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VarlinkDaemonStatus {
    pub uptime_seconds: i64,
    pub active_policies: i64,
    pub running_factories: Vec<VarlinkFactoryStatus>,
}

// ── Helper functions ──────────────────────────────────────────────────────────

/// Converts a domain `Value` to a `serde_json::Value` for wire serialization.
///
/// Uses `serde_json::to_value` which works because `Value` implements `Serialize`.
/// IP addresses and networks serialize as strings.
pub fn value_to_json(v: &Value) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

/// Converts a `serde_json::Value` back to a domain `Value` for wire deserialization.
///
/// Uses `serde_json::from_value` with the untagged `Value` enum, which tries
/// variants in declaration order: Bool, U64, I64, IpNetwork, IpAddr, List, Map, String.
/// IP-format strings deserialize as `IpAddr`/`IpNetwork`, not `String`.
pub fn json_to_value(v: serde_json::Value) -> Result<Value, String> {
    serde_json::from_value(v).map_err(|e| format!("failed to parse field value: {e}"))
}

/// Extracts the `value` from each `FieldValue` in the map and serializes to JSON.
/// Provenance, metadata, and other internal fields are stripped — the wire format
/// only carries observable values.
pub fn state_fields_to_json(
    fields: &IndexMap<String, FieldValue>,
) -> serde_json::Map<String, serde_json::Value> {
    fields
        .iter()
        .map(|(k, fv)| (k.clone(), value_to_json(&fv.value)))
        .collect()
}

/// Wraps each JSON value in a `FieldValue` with `KernelDefault` provenance.
///
/// Note: The daemon is responsible for setting the correct provenance
/// (`UserConfigured`) when processing submitted policies. `KernelDefault` is
/// used here as a neutral placeholder during wire deserialization.
pub fn json_to_state_fields(
    fields: &serde_json::Map<String, serde_json::Value>,
) -> Result<IndexMap<String, FieldValue>, String> {
    let mut result = IndexMap::new();
    for (k, v) in fields {
        let value = json_to_value(v.clone())?;
        result.insert(
            k.clone(),
            FieldValue {
                value,
                provenance: Provenance::KernelDefault,
            },
        );
    }
    Ok(result)
}

// ── VarlinkShowInfo ───────────────────────────────────────────────────────────

/// Wire-format response for `GetShowInfo`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VarlinkShowInfo {
    pub daemon: VarlinkDaemonInfo,
    pub interfaces: Vec<VarlinkInterfaceInfo>,
}

/// Wire-format daemon information within `GetShowInfo`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VarlinkDaemonInfo {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_seconds: Option<i64>,
}

/// Wire-format per-interface information within `GetShowInfo`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VarlinkInterfaceInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carrier: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addresses: Option<Vec<String>>,
    /// Absent when in daemon-free mode (CLI fabricates locally).
    /// Present but possibly empty when the daemon has policy data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policies: Option<Vec<VarlinkPolicyInfo>>,
    /// Present only for interfaces with a DHCP factory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dhcp: Option<VarlinkDhcpInfo>,
    /// `"applied"` or `"drifted"` — only present for managed interfaces.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_state: Option<String>,
    /// Per-field drift details — only present when `config_state` is `"drifted"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_drift: Option<Vec<VarlinkDriftEntry>>,
}

/// Wire-format drift entry describing a single field mismatch.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VarlinkDriftEntry {
    pub field_name: String,
    pub description: String,
}

/// Wire-format policy reference within `InterfaceInfo`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VarlinkPolicyInfo {
    pub name: String,
    /// Factory type: `"static"` or `"dhcpv4"`. Uses `#[serde(rename)]`
    /// because `type` is a reserved keyword in Rust.
    #[serde(rename = "type")]
    pub policy_type: String,
}

/// Wire-format DHCP state within `InterfaceInfo`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VarlinkDhcpInfo {
    /// `"running"` when a lease is active, `"waiting"` otherwise.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_time_secs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_remaining_secs: Option<i64>,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use netfyr_backend::{AppliedOperation, ApplyReport, BackendError, DiffOpKind, FailedOperation, SkippedOperation};
    use netfyr_policy::{FactoryType, Policy};
    use netfyr_reconcile::{
        Conflict, ConflictContribution, ConflictReport, DiffKind, DiffOperation,
        FieldChange as ReconcileFieldChange, FieldChangeKind as ReconcileFieldChangeKind,
        PolicyId, StateDiff as ReconcileStateDiff,
    };
    use netfyr_state::{Provenance, Selector, State, StateMetadata, Value};

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn fv(v: Value) -> FieldValue {
        FieldValue { value: v, provenance: Provenance::KernelDefault }
    }

    fn make_state(entity_type: &str, name: &str, fields: Vec<(&str, Value)>) -> State {
        let mut field_map = IndexMap::new();
        for (k, v) in fields {
            field_map.insert(k.to_string(), fv(v));
        }
        State {
            entity_type: entity_type.to_string(),
            selector: Selector::with_name(name),
            fields: field_map,
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 100,
        }
    }

    fn make_static_policy(name: &str, state: State) -> Policy {
        Policy {
            name: name.to_string(),
            factory_type: FactoryType::Static,
            priority: 100,
            state: Some(state),
            states: None,
            selector: None,
        }
    }

    fn make_dhcpv4_policy(name: &str, interface: &str) -> Policy {
        Policy {
            name: name.to_string(),
            factory_type: FactoryType::Dhcpv4,
            priority: 100,
            state: None,
            states: None,
            selector: Some(Selector::with_name(interface)),
        }
    }

    // ── VarlinkSelector ───────────────────────────────────────────────────────

    /// Selector with name-only converts to VarlinkSelector with name only.
    #[test]
    fn test_varlink_selector_from_name_only_selector() {
        let sel = Selector::with_name("eth0");
        let vs = VarlinkSelector::from(&sel);
        assert_eq!(vs.name, Some("eth0".to_string()));
        assert!(vs.entity_type.is_none());
        assert!(vs.driver.is_none());
        assert!(vs.mac.is_none());
        assert!(vs.pci_path.is_none());
    }

    /// Selector with all fields converts all fields to VarlinkSelector.
    #[test]
    fn test_varlink_selector_from_selector_with_all_fields() {
        let sel = Selector {
            name: Some("eth0".to_string()),
            type_: Some("ethernet".to_string()),
            driver: Some("ixgbe".to_string()),
            pci_path: Some("0000:03:00.0".to_string()),
            mac: Some("aa:bb:cc:dd:ee:ff".parse().unwrap()),
            ..Default::default()
        };
        let vs = VarlinkSelector::from(&sel);
        assert_eq!(vs.name, Some("eth0".to_string()));
        assert_eq!(vs.entity_type, Some("ethernet".to_string()));
        assert_eq!(vs.driver, Some("ixgbe".to_string()));
        assert_eq!(vs.pci_path, Some("0000:03:00.0".to_string()));
        assert_eq!(vs.mac, Some("aa:bb:cc:dd:ee:ff".to_string()));
    }

    /// MAC address is serialized as lowercase colon-separated string.
    #[test]
    fn test_varlink_selector_mac_serialized_as_lowercase_colon_separated() {
        let sel = Selector {
            mac: Some("AA:BB:CC:DD:EE:FF".parse().unwrap()),
            ..Default::default()
        };
        let vs = VarlinkSelector::from(&sel);
        assert_eq!(vs.mac.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
    }

    /// VarlinkSelector converts back to Selector preserving non-MAC fields.
    #[test]
    fn test_varlink_selector_into_selector_preserves_name_driver_entity_type() {
        let vs = VarlinkSelector {
            entity_type: Some("ethernet".to_string()),
            name: Some("eth0".to_string()),
            driver: Some("ixgbe".to_string()),
            mac: None,
            pci_path: Some("0000:03:00.0".to_string()),
            ..Default::default()
        };
        let sel = Selector::from(vs);
        assert_eq!(sel.name, Some("eth0".to_string()));
        assert_eq!(sel.type_, Some("ethernet".to_string()));
        assert_eq!(sel.driver, Some("ixgbe".to_string()));
        assert_eq!(sel.pci_path, Some("0000:03:00.0".to_string()));
    }

    /// Invalid MAC string in VarlinkSelector is treated as absent (None).
    #[test]
    fn test_varlink_selector_invalid_mac_string_parsed_as_none() {
        let vs = VarlinkSelector { mac: Some("not-a-mac".to_string()), ..Default::default() };
        let sel = Selector::from(vs);
        assert!(sel.mac.is_none(), "invalid MAC string must parse as None");
    }

    /// entity_type field serializes with JSON key "type" (not "entity_type").
    #[test]
    fn test_varlink_selector_entity_type_serializes_as_type_json_key() {
        let vs = VarlinkSelector {
            entity_type: Some("ethernet".to_string()),
            name: Some("eth0".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&vs).unwrap();
        assert!(
            json.contains("\"type\":\"ethernet\""),
            "entity_type must serialize as JSON key 'type', got: {json}"
        );
        assert!(
            !json.contains("\"entity_type\""),
            "'entity_type' must not appear as a raw JSON key, got: {json}"
        );
    }

    /// Selector → VarlinkSelector → Selector roundtrip preserves all fields.
    #[test]
    fn test_varlink_selector_roundtrip_name_only() {
        let sel = Selector::with_name("eth0");
        let vs = VarlinkSelector::from(&sel);
        let restored = Selector::from(vs);
        assert_eq!(restored.name, sel.name);
        assert_eq!(restored.type_, sel.type_);
        assert_eq!(restored.driver, sel.driver);
        assert_eq!(restored.mac, sel.mac);
        assert_eq!(restored.pci_path, sel.pci_path);
    }

    // ── VarlinkStateDef ───────────────────────────────────────────────────────

    /// State converts to VarlinkStateDef with correct entity_type and selector.
    #[test]
    fn test_varlink_state_def_from_state_preserves_entity_type_and_selector() {
        let state = make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]);
        let vsd = VarlinkStateDef::from(&state);
        assert_eq!(vsd.entity_type, "ethernet");
        assert_eq!(vsd.selector.name, Some("eth0".to_string()));
    }

    /// State fields are serialized as a flat JSON object in VarlinkStateDef.
    #[test]
    fn test_varlink_state_def_fields_serialized_as_json_object() {
        let state = make_state(
            "ethernet",
            "eth0",
            vec![("mtu", Value::U64(1500)), ("speed", Value::U64(1000))],
        );
        let vsd = VarlinkStateDef::from(&state);
        assert_eq!(vsd.fields.get("mtu"), Some(&serde_json::json!(1500u64)));
        assert_eq!(vsd.fields.get("speed"), Some(&serde_json::json!(1000u64)));
    }

    /// State::try_from(VarlinkStateDef) roundtrip preserves entity_type and field values.
    #[test]
    fn test_state_try_from_varlink_state_def_roundtrip() {
        let original = make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]);
        let vsd = VarlinkStateDef::from(&original);
        let restored = State::try_from(vsd).expect("should convert back to State");
        assert_eq!(restored.entity_type, original.entity_type);
        assert_eq!(restored.selector.name, original.selector.name);
        assert_eq!(restored.fields["mtu"].value, Value::U64(1500));
    }

    // ── VarlinkState ──────────────────────────────────────────────────────────

    /// Scenario: Query returns entity states — each has entity_type, selector, and fields.
    #[test]
    fn test_varlink_state_from_state_has_entity_type_selector_and_fields() {
        let state = make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]);
        let vs = VarlinkState::from(&state);
        assert_eq!(vs.entity_type, "ethernet");
        assert_eq!(vs.selector.name, Some("eth0".to_string()));
        assert_eq!(vs.fields.get("mtu"), Some(&serde_json::json!(1500u64)));
    }

    // ── VarlinkPolicy ─────────────────────────────────────────────────────────

    /// Static policy converts to VarlinkPolicy with factory = "static".
    #[test]
    fn test_varlink_policy_from_static_policy_has_factory_static() {
        let policy = make_static_policy(
            "eth0",
            make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]),
        );
        let vp = VarlinkPolicy::from(&policy);
        assert_eq!(vp.factory, "static");
        assert_eq!(vp.name, "eth0");
    }

    /// DHCPv4 policy converts to VarlinkPolicy with factory = "dhcpv4".
    #[test]
    fn test_varlink_policy_from_dhcpv4_policy_has_factory_dhcpv4() {
        let policy = make_dhcpv4_policy("eth0-dhcp", "eth0");
        let vp = VarlinkPolicy::from(&policy);
        assert_eq!(vp.factory, "dhcpv4");
    }

    /// VarlinkPolicy includes priority as Some(100).
    #[test]
    fn test_varlink_policy_from_policy_includes_priority() {
        let policy = make_static_policy(
            "eth0",
            make_state("ethernet", "eth0", vec![]),
        );
        let vp = VarlinkPolicy::from(&policy);
        assert_eq!(vp.priority, Some(100));
    }

    /// Static policy roundtrip: Policy → VarlinkPolicy → Policy preserves name, factory, priority.
    #[test]
    fn test_policy_try_from_varlink_policy_static_roundtrip() {
        let original = make_static_policy(
            "eth0-policy",
            make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]),
        );
        let vp = VarlinkPolicy::from(&original);
        let restored = Policy::try_from(vp).expect("should convert back to Policy");
        assert_eq!(restored.name, original.name);
        assert_eq!(restored.factory_type, original.factory_type);
        assert_eq!(restored.priority, original.priority);
    }

    /// DHCPv4 policy roundtrip preserves factory type and selector name.
    #[test]
    fn test_policy_try_from_varlink_policy_dhcpv4_roundtrip_selector() {
        let original = make_dhcpv4_policy("eth0-dhcp", "eth0");
        let vp = VarlinkPolicy::from(&original);
        let restored = Policy::try_from(vp).expect("should convert DHCPv4 policy back");
        assert_eq!(restored.factory_type, FactoryType::Dhcpv4);
        assert_eq!(
            restored.selector.as_ref().and_then(|s| s.name.as_deref()),
            Some("eth0")
        );
    }

    /// Scenario: SubmitPolicies with invalid policy — unknown factory type returns Err.
    #[test]
    fn test_policy_try_from_unknown_factory_type_returns_error() {
        let vp = VarlinkPolicy {
            name: "test".to_string(),
            factory: "unknown_factory".to_string(),
            priority: None,
            selector: None,
            state: None,
            states: None,
        };
        let result = Policy::try_from(vp);
        assert!(result.is_err(), "unknown factory type must return Err");
        let err = result.unwrap_err();
        assert!(
            err.contains("unknown factory type"),
            "error must describe the problem, got: {err}"
        );
    }

    /// Scenario: Type conversion roundtrip — static policy with state preserves all fields.
    #[test]
    fn test_policy_conversion_roundtrip_static_with_state() {
        let state = make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]);
        let original = Policy {
            name: "eth0-policy".to_string(),
            factory_type: FactoryType::Static,
            priority: 150,
            state: Some(state),
            states: None,
            selector: None,
        };
        let vp = VarlinkPolicy::from(&original);
        let restored = Policy::try_from(vp).expect("roundtrip must succeed");
        assert_eq!(restored.name, "eth0-policy");
        assert_eq!(restored.factory_type, FactoryType::Static);
        assert_eq!(restored.priority, 150);
        let restored_state = restored.state.expect("state must be Some after roundtrip");
        assert_eq!(restored_state.entity_type, "ethernet");
        assert_eq!(restored_state.fields["mtu"].value, Value::U64(1500));
    }

    /// Type conversion roundtrip — DHCPv4 policy with selector.
    #[test]
    fn test_policy_conversion_roundtrip_dhcpv4_with_selector() {
        let original = Policy {
            name: "eth0-dhcp".to_string(),
            factory_type: FactoryType::Dhcpv4,
            priority: 100,
            state: None,
            states: None,
            selector: Some(Selector::with_name("eth0")),
        };
        let vp = VarlinkPolicy::from(&original);
        let restored = Policy::try_from(vp).expect("DHCPv4 roundtrip must succeed");
        assert_eq!(restored.name, "eth0-dhcp");
        assert_eq!(restored.factory_type, FactoryType::Dhcpv4);
        assert_eq!(restored.selector.unwrap().name, Some("eth0".to_string()));
    }

    // ── VarlinkChangeEntry ────────────────────────────────────────────────────

    /// AppliedOperation converts to VarlinkChangeEntry with status "applied".
    #[test]
    fn test_varlink_change_entry_from_applied_operation_has_status_applied() {
        let op = AppliedOperation {
            operation: DiffOpKind::Add,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            fields_changed: vec!["mtu".to_string()],
        };
        let entry = VarlinkChangeEntry::from(&op);
        assert_eq!(entry.status, "applied");
        assert_eq!(entry.kind, "add");
        assert_eq!(entry.entity_type, "ethernet");
        assert_eq!(entry.entity_name, "eth0");
    }

    /// FailedOperation converts to VarlinkChangeEntry with status "failed".
    #[test]
    fn test_varlink_change_entry_from_failed_operation_has_status_failed() {
        let op = FailedOperation {
            operation: DiffOpKind::Modify,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            error: BackendError::Internal("test error".to_string()),
            fields: vec!["mtu".to_string()],
        };
        let entry = VarlinkChangeEntry::from(&op);
        assert_eq!(entry.status, "failed");
        assert_eq!(entry.kind, "modify");
        assert_eq!(entry.entity_name, "eth0");
    }

    /// SkippedOperation converts to VarlinkChangeEntry with status "skipped".
    #[test]
    fn test_varlink_change_entry_from_skipped_operation_has_status_skipped() {
        let op = SkippedOperation {
            operation: DiffOpKind::Remove,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            reason: "already in desired state".to_string(),
        };
        let entry = VarlinkChangeEntry::from(&op);
        assert_eq!(entry.status, "skipped");
        assert_eq!(entry.kind, "remove");
        assert_eq!(entry.entity_name, "eth0");
    }

    // ── VarlinkApplyReport ────────────────────────────────────────────────────

    /// Scenario: ApplyReport conversion preserves all fields — succeeded/failed/skipped counts.
    #[test]
    fn test_varlink_apply_report_from_apply_report_preserves_counts() {
        let mut report = ApplyReport::new();
        report.succeeded.push(AppliedOperation {
            operation: DiffOpKind::Add,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            fields_changed: vec![],
        });
        report.succeeded.push(AppliedOperation {
            operation: DiffOpKind::Modify,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth1"),
            fields_changed: vec!["mtu".to_string()],
        });
        report.failed.push(FailedOperation {
            operation: DiffOpKind::Remove,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth2"),
            error: BackendError::Internal("fail".to_string()),
            fields: vec![],
        });

        let var_report = VarlinkApplyReport::from(report);
        assert_eq!(var_report.succeeded, 2, "succeeded count must be 2");
        assert_eq!(var_report.failed, 1, "failed count must be 1");
        assert_eq!(var_report.skipped, 0, "skipped count must be 0");
    }

    /// ApplyReport conversion includes all change entries (succeeded + failed + skipped).
    #[test]
    fn test_varlink_apply_report_changes_include_all_operation_statuses() {
        let mut report = ApplyReport::new();
        report.succeeded.push(AppliedOperation {
            operation: DiffOpKind::Add,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            fields_changed: vec!["mtu".to_string()],
        });
        report.skipped.push(SkippedOperation {
            operation: DiffOpKind::Modify,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth1"),
            reason: "no change needed".to_string(),
        });

        let var_report = VarlinkApplyReport::from(report);
        assert_eq!(var_report.changes.len(), 2, "changes must include 1 applied + 1 skipped");
        let statuses: Vec<&str> = var_report.changes.iter().map(|e| e.status.as_str()).collect();
        assert!(statuses.contains(&"applied"), "changes must include 'applied' status");
        assert!(statuses.contains(&"skipped"), "changes must include 'skipped' status");
    }

    // ── VarlinkConflictEntry ──────────────────────────────────────────────────

    /// Scenario: ApplyReport conversion preserves all fields — conflict entry preserves
    /// entity, field, policies, and values.
    #[test]
    fn test_varlink_conflict_entry_from_conflict_preserves_entity_field_policies_values() {
        let conflict = Conflict {
            entity_key: ("ethernet".to_string(), "eth0".to_string()),
            field_name: "mtu".to_string(),
            priority: 100,
            contributions: vec![
                ConflictContribution {
                    policy_id: PolicyId::from("policy-a"),
                    value: fv(Value::U64(1500)),
                },
                ConflictContribution {
                    policy_id: PolicyId::from("policy-b"),
                    value: fv(Value::U64(9000)),
                },
            ],
        };
        let entry = VarlinkConflictEntry::from(&conflict);
        assert_eq!(entry.entity_type, "ethernet");
        assert_eq!(entry.entity_name, "eth0");
        assert_eq!(entry.field_name, "mtu");
        assert_eq!(entry.policies.len(), 2);
        assert!(entry.policies.contains(&"policy-a".to_string()));
        assert!(entry.policies.contains(&"policy-b".to_string()));
        assert_eq!(entry.values.len(), 2);
        assert!(entry.values.iter().any(|v| v == "1500"), "values must contain '1500'");
        assert!(entry.values.iter().any(|v| v == "9000"), "values must contain '9000'");
    }

    /// Scenario: ApplyReport conversion preserves all fields —
    /// 2 succeeded, 1 failed, 0 skipped, and 1 conflict in a single test.
    ///
    /// This is the canonical scenario from the acceptance criteria:
    /// "Given an ApplyReport with 2 succeeded, 1 failed, 0 skipped, and 1 conflict
    ///  When converted to varlink::ApplyReport
    ///  Then succeeded=2, failed=1, skipped=0
    ///  And the conflict entry preserves entity, field, policies, and values"
    #[test]
    fn test_apply_report_conversion_full_scenario_two_succeeded_one_failed_one_conflict() {
        let mut report = ApplyReport::new();
        // 2 succeeded
        report.succeeded.push(AppliedOperation {
            operation: DiffOpKind::Add,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            fields_changed: vec!["mtu".to_string()],
        });
        report.succeeded.push(AppliedOperation {
            operation: DiffOpKind::Modify,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth1"),
            fields_changed: vec!["speed".to_string()],
        });
        // 1 failed
        report.failed.push(FailedOperation {
            operation: DiffOpKind::Remove,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth2"),
            error: BackendError::Internal("netlink error".to_string()),
            fields: vec!["mtu".to_string()],
        });
        // 0 skipped (none added)

        // 1 conflict
        let conflict_report = ConflictReport {
            conflicts: vec![Conflict {
                entity_key: ("ethernet".to_string(), "eth0".to_string()),
                field_name: "mtu".to_string(),
                priority: 100,
                contributions: vec![
                    ConflictContribution {
                        policy_id: PolicyId::from("policy-x"),
                        value: fv(Value::U64(1500)),
                    },
                    ConflictContribution {
                        policy_id: PolicyId::from("policy-y"),
                        value: fv(Value::U64(9000)),
                    },
                ],
            }],
        };

        let var_report = convert_apply_report_with_conflicts(report, &conflict_report);

        // Counts
        assert_eq!(var_report.succeeded, 2, "succeeded must be 2");
        assert_eq!(var_report.failed, 1, "failed must be 1");
        assert_eq!(var_report.skipped, 0, "skipped must be 0");

        // All 3 operations appear as change entries (2 applied + 1 failed)
        assert_eq!(var_report.changes.len(), 3, "changes must have 3 entries (2 applied + 1 failed)");
        let applied_entries: Vec<_> = var_report.changes.iter().filter(|e| e.status == "applied").collect();
        let failed_entries: Vec<_> = var_report.changes.iter().filter(|e| e.status == "failed").collect();
        assert_eq!(applied_entries.len(), 2, "must have 2 applied change entries");
        assert_eq!(failed_entries.len(), 1, "must have 1 failed change entry");

        // Conflict preserves entity, field, policies, and values
        assert_eq!(var_report.conflicts.len(), 1, "must have 1 conflict entry");
        let conflict = &var_report.conflicts[0];
        assert_eq!(conflict.entity_type, "ethernet", "conflict entity_type must be 'ethernet'");
        assert_eq!(conflict.entity_name, "eth0", "conflict entity_name must be 'eth0'");
        assert_eq!(conflict.field_name, "mtu", "conflict field_name must be 'mtu'");
        assert_eq!(conflict.policies.len(), 2, "conflict must list 2 policies");
        assert!(
            conflict.policies.contains(&"policy-x".to_string()),
            "conflict must include policy-x"
        );
        assert!(
            conflict.policies.contains(&"policy-y".to_string()),
            "conflict must include policy-y"
        );
        assert_eq!(conflict.values.len(), 2, "conflict must list 2 values");
        assert!(
            conflict.values.iter().any(|v| v == "1500"),
            "conflict values must include '1500'"
        );
        assert!(
            conflict.values.iter().any(|v| v == "9000"),
            "conflict values must include '9000'"
        );
    }

    /// convert_apply_report_with_conflicts includes conflict entries in the report.
    #[test]
    fn test_convert_apply_report_with_conflicts_includes_conflict_entries() {
        let mut report = ApplyReport::new();
        report.succeeded.push(AppliedOperation {
            operation: DiffOpKind::Add,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            fields_changed: vec![],
        });

        let conflict_report = ConflictReport {
            conflicts: vec![Conflict {
                entity_key: ("ethernet".to_string(), "eth0".to_string()),
                field_name: "mtu".to_string(),
                priority: 100,
                contributions: vec![
                    ConflictContribution {
                        policy_id: PolicyId::from("a"),
                        value: fv(Value::U64(1500)),
                    },
                    ConflictContribution {
                        policy_id: PolicyId::from("b"),
                        value: fv(Value::U64(9000)),
                    },
                ],
            }],
        };

        let var_report = convert_apply_report_with_conflicts(report, &conflict_report);
        assert_eq!(var_report.succeeded, 1);
        assert_eq!(var_report.conflicts.len(), 1, "must include 1 conflict entry");
        assert_eq!(var_report.conflicts[0].entity_type, "ethernet");
        assert_eq!(var_report.conflicts[0].field_name, "mtu");
    }

    // ── value_to_json / json_to_value helpers ─────────────────────────────────

    /// value_to_json converts U64 to JSON number.
    #[test]
    fn test_value_to_json_u64_produces_json_number() {
        let j = value_to_json(&Value::U64(1500));
        assert_eq!(j, serde_json::json!(1500u64));
    }

    /// value_to_json converts String to JSON string.
    #[test]
    fn test_value_to_json_string_produces_json_string() {
        let j = value_to_json(&Value::String("eth0".to_string()));
        assert_eq!(j, serde_json::json!("eth0"));
    }

    /// value_to_json converts Bool to JSON boolean.
    #[test]
    fn test_value_to_json_bool_produces_json_bool() {
        let j = value_to_json(&Value::Bool(true));
        assert_eq!(j, serde_json::json!(true));
    }

    /// json_to_value converts JSON number to Value::U64.
    #[test]
    fn test_json_to_value_number_produces_u64() {
        let v = json_to_value(serde_json::json!(1500u64)).unwrap();
        assert_eq!(v, Value::U64(1500));
    }

    /// json_to_value converts JSON string to Value::String.
    #[test]
    fn test_json_to_value_string_produces_value_string() {
        let v = json_to_value(serde_json::json!("hello")).unwrap();
        assert_eq!(v, Value::String("hello".to_string()));
    }

    /// value_to_json + json_to_value roundtrip for U64.
    #[test]
    fn test_value_roundtrip_through_json_u64() {
        let original = Value::U64(9000);
        let json = value_to_json(&original);
        let restored = json_to_value(json).unwrap();
        assert_eq!(original, restored);
    }

    /// value_to_json + json_to_value roundtrip for Bool.
    #[test]
    fn test_value_roundtrip_through_json_bool() {
        let original = Value::Bool(false);
        let json = value_to_json(&original);
        let restored = json_to_value(json).unwrap();
        assert_eq!(original, restored);
    }

    /// state_fields_to_json produces a flat JSON map of field values (no provenance).
    #[test]
    fn test_state_fields_to_json_produces_flat_json_object() {
        let state = make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]);
        let json_map = state_fields_to_json(&state.fields);
        assert_eq!(
            json_map.get("mtu"),
            Some(&serde_json::json!(1500u64)),
            "mtu must serialize as JSON number 1500"
        );
        // Provenance must not appear in the wire format.
        assert!(
            !json_map.contains_key("provenance"),
            "provenance must not appear in the serialized fields"
        );
    }

    /// json_to_state_fields wraps values in FieldValue with KernelDefault provenance.
    #[test]
    fn test_json_to_state_fields_uses_kernel_default_provenance() {
        let mut map = serde_json::Map::new();
        map.insert("mtu".to_string(), serde_json::json!(1500u64));

        let fields = json_to_state_fields(&map).expect("must parse successfully");
        let field_value = fields.get("mtu").expect("mtu field must be present");
        assert_eq!(field_value.value, Value::U64(1500));
        assert_eq!(field_value.provenance, Provenance::KernelDefault);
    }

    // ── VarlinkFieldChange ────────────────────────────────────────────────────

    /// Set without current value (new field) produces change_kind = "set" with current = None.
    #[test]
    fn test_varlink_field_change_from_set_without_current() {
        let fc = ReconcileFieldChange {
            field_name: "mtu".to_string(),
            change: ReconcileFieldChangeKind::Set {
                current: None,
                desired: fv(Value::U64(1500)),
            },
        };
        let vfc = VarlinkFieldChange::from(&fc);
        assert_eq!(vfc.field_name, "mtu");
        assert_eq!(vfc.change_kind, "set");
        assert!(vfc.current.is_none(), "current must be None for a new field");
        assert_eq!(vfc.desired, Some(serde_json::json!(1500u64)));
    }

    /// Set with current value (changed field) produces change_kind = "set" with both values.
    #[test]
    fn test_varlink_field_change_from_set_with_current() {
        let fc = ReconcileFieldChange {
            field_name: "mtu".to_string(),
            change: ReconcileFieldChangeKind::Set {
                current: Some(fv(Value::U64(1500))),
                desired: fv(Value::U64(9000)),
            },
        };
        let vfc = VarlinkFieldChange::from(&fc);
        assert_eq!(vfc.change_kind, "set");
        assert_eq!(vfc.current, Some(serde_json::json!(1500u64)));
        assert_eq!(vfc.desired, Some(serde_json::json!(9000u64)));
    }

    /// Unset field produces change_kind = "unset" with current but no desired.
    #[test]
    fn test_varlink_field_change_from_unset_has_change_kind_unset() {
        let fc = ReconcileFieldChange {
            field_name: "speed".to_string(),
            change: ReconcileFieldChangeKind::Unset { current: fv(Value::U64(1000)) },
        };
        let vfc = VarlinkFieldChange::from(&fc);
        assert_eq!(vfc.change_kind, "unset");
        assert_eq!(vfc.current, Some(serde_json::json!(1000u64)));
        assert!(vfc.desired.is_none(), "desired must be None for Unset");
    }

    /// Unchanged field produces change_kind = "unchanged" with current but no desired.
    #[test]
    fn test_varlink_field_change_from_unchanged_has_change_kind_unchanged() {
        let fc = ReconcileFieldChange {
            field_name: "mtu".to_string(),
            change: ReconcileFieldChangeKind::Unchanged { value: fv(Value::U64(1500)) },
        };
        let vfc = VarlinkFieldChange::from(&fc);
        assert_eq!(vfc.change_kind, "unchanged");
        assert_eq!(vfc.current, Some(serde_json::json!(1500u64)));
        assert!(vfc.desired.is_none(), "desired must be None for Unchanged");
    }

    // ── VarlinkDiffOperation ──────────────────────────────────────────────────

    /// Add operation converts to VarlinkDiffOperation with kind = "add".
    #[test]
    fn test_varlink_diff_operation_from_add_has_kind_add() {
        let op = DiffOperation {
            kind: DiffKind::Add,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            field_changes: vec![],
        };
        let vop = VarlinkDiffOperation::from(&op);
        assert_eq!(vop.kind, "add");
        assert_eq!(vop.entity_type, "ethernet");
        assert_eq!(vop.entity_name, "eth0");
    }

    /// Remove operation converts to VarlinkDiffOperation with kind = "remove".
    #[test]
    fn test_varlink_diff_operation_from_remove_has_kind_remove() {
        let op = DiffOperation {
            kind: DiffKind::Remove,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            field_changes: vec![],
        };
        let vop = VarlinkDiffOperation::from(&op);
        assert_eq!(vop.kind, "remove");
    }

    /// Modify operation converts to VarlinkDiffOperation with kind = "modify".
    #[test]
    fn test_varlink_diff_operation_from_modify_has_kind_modify() {
        let op = DiffOperation {
            kind: DiffKind::Modify,
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            field_changes: vec![],
        };
        let vop = VarlinkDiffOperation::from(&op);
        assert_eq!(vop.kind, "modify");
    }

    // ── VarlinkStateDiff ──────────────────────────────────────────────────────

    /// Scenario: DryRun returns diff — ReconcileStateDiff converts to VarlinkStateDiff.
    #[test]
    fn test_varlink_state_diff_from_reconcile_state_diff_preserves_operations() {
        let diff = ReconcileStateDiff {
            operations: vec![DiffOperation {
                kind: DiffKind::Modify,
                entity_type: "ethernet".to_string(),
                selector: Selector::with_name("eth0"),
                field_changes: vec![ReconcileFieldChange {
                    field_name: "mtu".to_string(),
                    change: ReconcileFieldChangeKind::Set {
                        current: Some(fv(Value::U64(1500))),
                        desired: fv(Value::U64(9000)),
                    },
                }],
            }],
        };
        let vdiff = VarlinkStateDiff::from(diff);
        assert_eq!(vdiff.operations.len(), 1);
        assert_eq!(vdiff.operations[0].kind, "modify");
        assert_eq!(vdiff.operations[0].entity_type, "ethernet");
        assert_eq!(vdiff.operations[0].entity_name, "eth0");
        assert_eq!(vdiff.operations[0].field_changes.len(), 1);
        assert_eq!(vdiff.operations[0].field_changes[0].field_name, "mtu");
        assert_eq!(vdiff.operations[0].field_changes[0].change_kind, "set");
    }

    /// Empty ReconcileStateDiff converts to empty VarlinkStateDiff.
    #[test]
    fn test_varlink_state_diff_from_empty_reconcile_diff_is_empty() {
        let diff = ReconcileStateDiff { operations: vec![] };
        let vdiff = VarlinkStateDiff::from(diff);
        assert!(vdiff.operations.is_empty(), "empty ReconcileStateDiff must produce empty VarlinkStateDiff");
    }

    // ── Interface definition file ─────────────────────────────────────────────

    /// Scenario: Interface definition file is valid — defines the 4 required
    /// methods: SubmitPolicies, Query, DryRun, GetShowInfo.
    #[test]
    fn test_varlink_interface_file_defines_required_methods() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let varlink_file = std::path::Path::new(manifest_dir)
            .join("src")
            .join("io.netfyr.varlink");

        let content = std::fs::read_to_string(&varlink_file)
            .expect("io.netfyr.varlink must exist and be readable");

        assert!(
            content.contains("interface io.netfyr"),
            "interface must be named 'io.netfyr'"
        );

        for method in &["SubmitPolicies", "Query", "DryRun", "GetShowInfo"] {
            assert!(
                content.contains(&format!("method {method}")),
                "interface must define method '{method}'"
            );
        }
    }

    /// Interface definition file defines the 3 standard error types.
    #[test]
    fn test_varlink_interface_file_defines_required_error_types() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let varlink_file = std::path::Path::new(manifest_dir)
            .join("src")
            .join("io.netfyr.varlink");

        let content = std::fs::read_to_string(&varlink_file)
            .expect("io.netfyr.varlink must exist and be readable");

        for error in &["InvalidPolicy", "BackendError", "InternalError"] {
            assert!(
                content.contains(&format!("error {error}")),
                "interface must define error '{error}'"
            );
        }
    }

    // ── Comprehensive roundtrip tests ────────────────────────────────────────
    //
    // These tests populate every field and use every Value variant to catch
    // silent field loss when core types gain new fields or new Value variants.

    /// Selector → VarlinkSelector → JSON → VarlinkSelector → Selector preserves
    /// all wire-visible fields including labels.
    #[test]
    fn test_selector_full_roundtrip_through_json_preserves_all_wire_fields() {
        let original = Selector {
            name: Some("eth0".to_string()),
            type_: Some("ethernet".to_string()),
            driver: Some("ixgbe".to_string()),
            pci_path: Some("0000:03:00.0".to_string()),
            mac: Some("aa:bb:cc:dd:ee:ff".parse().unwrap()),
            labels: [("env".to_string(), "prod".to_string())].into(),
        };

        let wire = VarlinkSelector::from(&original);
        let json = serde_json::to_value(&wire).unwrap();
        let wire2: VarlinkSelector = serde_json::from_value(json).unwrap();
        let restored = Selector::from(wire2);

        assert_eq!(restored.name, original.name);
        assert_eq!(restored.type_, original.type_);
        assert_eq!(restored.driver, original.driver);
        assert_eq!(restored.pci_path, original.pci_path);
        assert_eq!(restored.mac, original.mac);
        assert_eq!(restored.labels, original.labels, "labels must survive wire roundtrip");
    }

    /// State → VarlinkStateDef → JSON → VarlinkStateDef → State roundtrip
    /// with every Value variant (U64, I64, Bool, String, IpNetwork, IpAddr, List, Map).
    #[test]
    fn test_state_full_roundtrip_through_json_all_value_types() {
        let mut field_map = IndexMap::new();
        let pairs: Vec<(&str, Value)> = vec![
            ("mtu", Value::U64(9000)),
            ("priority", Value::I64(-1)),
            ("enabled", Value::Bool(true)),
            ("description", Value::String("primary uplink".to_string())),
            (
                "gateway",
                Value::IpAddr("10.0.0.1".parse().unwrap()),
            ),
            (
                "network",
                Value::IpNetwork("10.0.0.0/24".parse().unwrap()),
            ),
            (
                "addresses",
                Value::List(vec![
                    Value::IpNetwork("10.0.0.2/24".parse().unwrap()),
                    Value::IpNetwork("10.0.0.3/24".parse().unwrap()),
                ]),
            ),
            (
                "options",
                Value::Map(IndexMap::from([
                    ("arp".to_string(), Value::Bool(true)),
                    ("cost".to_string(), Value::U64(100)),
                ])),
            ),
        ];
        for (k, v) in &pairs {
            field_map.insert(k.to_string(), fv(v.clone()));
        }

        let original = State {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            fields: field_map,
            metadata: StateMetadata::new(),
            policy_ref: Some("test-policy".to_string()),
            priority: 200,
        };

        let wire = VarlinkStateDef::from(&original);
        let json = serde_json::to_value(&wire).unwrap();
        let wire2: VarlinkStateDef = serde_json::from_value(json).unwrap();
        let restored = State::try_from(wire2).expect("roundtrip must succeed");

        assert_eq!(restored.entity_type, original.entity_type);
        assert_eq!(restored.selector.name, original.selector.name);
        for (name, value) in &pairs {
            assert_eq!(
                &restored.fields[*name].value, value,
                "field '{name}' must survive roundtrip"
            );
        }
    }

    /// Policy with `states` (plural) roundtrips through JSON without losing entries.
    #[test]
    fn test_policy_with_states_plural_roundtrip_through_json() {
        let original = Policy {
            name: "multi-state".to_string(),
            factory_type: FactoryType::Static,
            priority: 150,
            state: None,
            states: Some(vec![
                make_state("ethernet", "eth0", vec![("mtu", Value::U64(1500))]),
                make_state("ethernet", "eth1", vec![("mtu", Value::U64(9000))]),
            ]),
            selector: None,
        };

        let wire = VarlinkPolicy::from(&original);
        let json = serde_json::to_value(&wire).unwrap();
        let wire2: VarlinkPolicy = serde_json::from_value(json).unwrap();
        let restored = Policy::try_from(wire2).expect("roundtrip must succeed");

        assert_eq!(restored.name, "multi-state");
        assert_eq!(restored.priority, 150);
        let states = restored.states.expect("states must be Some");
        assert_eq!(states.len(), 2);
        assert_eq!(states[0].selector.name, Some("eth0".to_string()));
        assert_eq!(states[1].selector.name, Some("eth1".to_string()));
        assert_eq!(states[0].fields["mtu"].value, Value::U64(1500));
        assert_eq!(states[1].fields["mtu"].value, Value::U64(9000));
    }

    // ── Varlink interface file tests ─────────────────────────────────────────

    /// Interface definition file defines key composite types (Policy, Selector,
    /// ApplyReport, StateDiff, ShowInfo).
    #[test]
    fn test_varlink_interface_file_defines_required_composite_types() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let varlink_file = std::path::Path::new(manifest_dir)
            .join("src")
            .join("io.netfyr.varlink");

        let content = std::fs::read_to_string(&varlink_file)
            .expect("io.netfyr.varlink must exist and be readable");

        for type_name in &["Policy", "Selector", "ApplyReport", "StateDiff", "ShowInfo"] {
            assert!(
                content.contains(&format!("type {type_name}")),
                "interface must define type '{type_name}'"
            );
        }
    }
}

