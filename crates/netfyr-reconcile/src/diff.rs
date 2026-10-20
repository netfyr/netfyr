use std::collections::BTreeMap;

use indexmap::IndexMap;
use netfyr_state::plan::{DeviceObservation, FieldChange, Operation, OperationKind};
use netfyr_state::{SchemaRegistry, Source, Value};

use crate::{ChangeProvenance, ReconcileError, address};

const ADDRESS_PATH: &str = "ipv4.addresses";

pub(super) struct DeviceDiff {
    pub changes: Vec<FieldChange>,
    pub operations: Vec<Operation>,
    pub provenance: Vec<ChangeProvenance>,
}

pub(super) fn diff_device(
    device: &DeviceObservation,
    effective: &IndexMap<String, Value>,
    leaf_sources: &IndexMap<String, String>,
    list_elements: &IndexMap<String, Vec<String>>,
    source_by_key: &BTreeMap<&str, Source>,
    registry: &SchemaRegistry,
) -> Result<DeviceDiff, ReconcileError> {
    let mut paths = Vec::new();
    collect_leaves(effective, "", &mut paths);
    paths.sort();
    let mut changes = Vec::new();
    let mut operations = Vec::new();
    let mut provenance = Vec::new();
    for path in paths {
        if path == ADDRESS_PATH {
            continue;
        }
        let Some(desired) = lookup(effective, &path) else {
            continue;
        };
        let old = lookup(&device.state.fields, &path);
        if old == Some(desired) || !registry.is_writable(&path) {
            continue;
        }
        let source_key = leaf_sources.get(&path);
        let source = source_key
            .and_then(|key| source_by_key.get(key.as_str()))
            .cloned()
            .unwrap_or(Source::Kernel);
        let kind = match path.as_str() {
            "mtu" => as_mtu(desired).map(OperationKind::SetMtu),
            "enabled" => match desired {
                Value::Bool(enabled) => Some(OperationKind::SetEnabled(*enabled)),
                _ => None,
            },
            _ => None,
        };
        changes.push(FieldChange {
            target: device.identity.clone(),
            field: path.clone(),
            old: old.cloned(),
            desired: Some(desired.clone()),
            source,
        });
        provenance.push(ChangeProvenance {
            target: device.identity.clone(),
            field: path,
            source: source_key.cloned(),
            element_sources: None,
        });
        if let Some(kind) = kind {
            operations.push(Operation {
                target: device.identity.clone(),
                kind,
                depends_on: Vec::new(),
                address_group: None,
            });
        }
    }
    if let Some(desired) = lookup(effective, ADDRESS_PATH) {
        let address_source_keys = list_elements.get(ADDRESS_PATH).map(Vec::as_slice);
        // The first element's contribution normally supplies the field's
        // source; an explicit empty list has no element, so its owner —
        // the contribution that set the empty list — attributes the
        // removals it causes instead of falling back to the kernel.
        let source_key = address_source_keys
            .and_then(|keys| keys.first())
            .or_else(|| leaf_sources.get(ADDRESS_PATH));
        let source = source_key
            .and_then(|key| source_by_key.get(key.as_str()))
            .cloned()
            .unwrap_or(Source::Kernel);
        let (change, address_operations) = address::plan_addresses(device, desired, source)?;
        if !address_operations.is_empty() {
            let insert_at = changes
                .iter()
                .position(|change| change.field.as_str() > ADDRESS_PATH)
                .unwrap_or(changes.len());
            changes.insert(insert_at, change);
            provenance.push(ChangeProvenance {
                target: device.identity.clone(),
                field: ADDRESS_PATH.into(),
                source: source_key.cloned(),
                element_sources: address_source_keys.map(<[String]>::to_vec),
            });
            // Address dependencies index the address-only vector; shift
            // them past this device's scalar operations so they address
            // rows of the combined operation list.
            let base = operations.len();
            operations.extend(address_operations.into_iter().map(|mut operation| {
                operation
                    .depends_on
                    .iter_mut()
                    .for_each(|dependency| *dependency += base);
                operation
            }));
        }
    }
    Ok(DeviceDiff {
        changes,
        operations,
        provenance,
    })
}

fn collect_leaves(map: &IndexMap<String, Value>, prefix: &str, output: &mut Vec<String>) {
    for (field, value) in map {
        let path = if prefix.is_empty() {
            field.clone()
        } else {
            format!("{prefix}.{field}")
        };
        match value {
            Value::Map(map) => collect_leaves(map, &path, output),
            _ => output.push(path),
        }
    }
}

fn lookup<'a>(map: &'a IndexMap<String, Value>, path: &str) -> Option<&'a Value> {
    let mut fields = path.split('.');
    let mut value = map.get(fields.next()?)?;
    for field in fields {
        value = match value {
            Value::Map(map) => map.get(field)?,
            _ => return None,
        };
    }
    Some(value)
}

fn as_mtu(value: &Value) -> Option<u32> {
    match value {
        Value::U64(value) => u32::try_from(*value).ok(),
        Value::I64(value) => u32::try_from(*value).ok(),
        _ => None,
    }
}
