use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr};

use indexmap::IndexMap;
use netfyr_state::plan::{
    AddressGroupPrecondition, AddressObservation, DeviceObservation, FieldChange, InterfaceId,
    Ipv4Address, Operation, OperationKind,
};
use netfyr_state::{Source, Value};

use crate::ReconcileError;

const ADDRESS_PATH: &str = "ipv4.addresses";

type Group = (Ipv4Addr, u8);

/// Check every supplied address before merging can discard duplicate CIDRs.
/// Schema validation has already checked the field and element shapes.
pub(super) fn validate_fields(
    fields: &IndexMap<String, Value>,
    target: &InterfaceId,
) -> Result<(), ReconcileError> {
    let Some(Value::Map(ipv4)) = fields.get("ipv4") else {
        return Ok(());
    };
    let Some(Value::List(addresses)) = ipv4.get("addresses") else {
        return Ok(());
    };
    for address in addresses {
        parse_address(address, target)?;
    }
    Ok(())
}

pub(super) fn plan_addresses(
    device: &DeviceObservation,
    desired: &Value,
    source: Source,
) -> Result<(FieldChange, Vec<Operation>), ReconcileError> {
    let desired = parse_desired(desired, &device.identity)?;
    let observed = &device.addresses;
    // An explicitly managed address list requires a complete, safely grouped
    // observation even when the visible entries happen to match the policy.
    if !device.addresses_complete {
        return Err(ReconcileError::IncompleteAddressInventory {
            target: Box::new(device.identity.clone()),
        });
    }
    for address in observed {
        let Some(group) = address.address.group() else {
            return Err(ReconcileError::UnsafeAddressGroup {
                target: Box::new(device.identity.clone()),
                message: "observed address has an invalid prefix".into(),
            });
        };
        if address.peer.is_some() || address.scope != 0 || address.kernel_group() != Some(group) {
            return Err(ReconcileError::UnsafeAddressGroup {
                target: Box::new(device.identity.clone()),
                message: "peer, scope, or kernel address grouping cannot be expressed safely"
                    .into(),
            });
        }
    }
    if same_addresses(observed, &desired) {
        return Ok((address_change(device, &desired, source), Vec::new()));
    }
    let observed_groups = groups_observed(observed);
    let desired_groups = groups_desired(&desired);
    let mut group_keys = BTreeSet::new();
    group_keys.extend(observed_groups.keys().copied());
    group_keys.extend(desired_groups.keys().copied());
    let mut operations = Vec::new();
    for group in group_keys {
        let current = observed_groups.get(&group).cloned().unwrap_or_default();
        let wanted = desired_groups.get(&group).cloned().unwrap_or_default();
        let offset = operations.len();
        operations.extend(plan_group(
            &device.identity,
            &current,
            &wanted,
            device.observed_at,
            offset,
        ));
    }
    Ok((address_change(device, &desired, source), operations))
}

fn parse_desired(value: &Value, target: &InterfaceId) -> Result<Vec<Ipv4Address>, ReconcileError> {
    let Value::List(values) = value else {
        return Err(ReconcileError::InvalidAddress {
            target: Box::new(target.clone()),
            message: "ipv4.addresses must be a list".into(),
        });
    };
    values
        .iter()
        .map(|value| parse_address(value, target))
        .collect()
}

fn parse_address(value: &Value, target: &InterfaceId) -> Result<Ipv4Address, ReconcileError> {
    let Value::Map(fields) = value else {
        return invalid(target, "address must be an object");
    };
    let Some(Value::IpNetwork((IpAddr::V4(ip), prefix_len))) = fields.get("ip") else {
        return invalid(target, "address ip must be an IPv4 CIDR");
    };
    let address = Ipv4Address {
        ip: *ip,
        prefix_len: *prefix_len,
        valid_lft: lifetime(fields, "valid_lft", target)?,
        preferred_lft: lifetime(fields, "preferred_lft", target)?,
    };
    address
        .validate()
        .map_err(|error| ReconcileError::InvalidAddress {
            target: Box::new(target.clone()),
            message: error.to_string(),
        })?;
    Ok(address)
}

fn lifetime(
    fields: &IndexMap<String, Value>,
    name: &str,
    target: &InterfaceId,
) -> Result<Option<u32>, ReconcileError> {
    match fields.get(name) {
        None => Ok(None),
        Some(Value::U64(value)) => {
            u32::try_from(*value)
                .map(Some)
                .map_err(|_| ReconcileError::InvalidAddress {
                    target: Box::new(target.clone()),
                    message: format!("{name} exceeds u32"),
                })
        }
        Some(Value::I64(value)) if *value >= 0 => {
            u32::try_from(*value as u64)
                .map(Some)
                .map_err(|_| ReconcileError::InvalidAddress {
                    target: Box::new(target.clone()),
                    message: format!("{name} exceeds u32"),
                })
        }
        Some(_) => invalid(target, &format!("{name} must be a non-negative u32")),
    }
}

fn invalid<T>(target: &InterfaceId, message: &str) -> Result<T, ReconcileError> {
    Err(ReconcileError::InvalidAddress {
        target: Box::new(target.clone()),
        message: message.into(),
    })
}

fn same_addresses(observed: &[AddressObservation], desired: &[Ipv4Address]) -> bool {
    observed.len() == desired.len()
        && observed.iter().zip(desired).all(|(actual, expected)| {
            actual.address.same_identity(expected)
                && actual.address.lifetimes() == expected.lifetimes()
        })
}

fn groups_observed(addresses: &[AddressObservation]) -> BTreeMap<Group, Vec<AddressObservation>> {
    let mut groups = BTreeMap::new();
    for address in addresses {
        if let Some(group) = address.address.group() {
            groups
                .entry(group)
                .or_insert_with(Vec::new)
                .push(address.clone());
        }
    }
    groups
}

fn groups_desired(addresses: &[Ipv4Address]) -> BTreeMap<Group, Vec<Ipv4Address>> {
    let mut groups = BTreeMap::new();
    for address in addresses {
        if let Some(group) = address.group() {
            groups
                .entry(group)
                .or_insert_with(Vec::new)
                .push(address.clone());
        }
    }
    groups
}

fn plan_group(
    target: &InterfaceId,
    observed: &[AddressObservation],
    desired: &[Ipv4Address],
    observed_at: std::time::Instant,
    offset: usize,
) -> Vec<Operation> {
    if observed.len() == desired.len()
        && observed
            .iter()
            .zip(desired)
            .all(|(actual, expected)| actual.address == *expected)
    {
        return Vec::new();
    }
    let observed_identities: Vec<_> = observed.iter().map(|entry| &entry.address).collect();
    let survivors: Vec<_> = desired
        .iter()
        .map(|address| {
            observed_identities
                .iter()
                .position(|actual| actual.same_identity(address))
        })
        .collect();
    let survivor_positions: Vec<_> = survivors.iter().flatten().copied().collect();
    let ordered = survivor_positions.windows(2).all(|pair| pair[0] < pair[1]);
    let same_primary = observed
        .first()
        .map(|entry| &entry.address)
        .zip(desired.first())
        .is_some_and(|(actual, expected)| actual.same_identity(expected));
    let last_survivor = survivors.iter().rposition(Option::is_some);
    let additions_are_suffix = survivors.iter().enumerate().all(|(index, survivor)| {
        survivor.is_some() || last_survivor.is_none_or(|last| index > last)
    });
    let rebuild = !same_primary || !ordered || !additions_are_suffix;
    let mut kinds = Vec::new();
    if rebuild {
        kinds.extend(
            observed
                .iter()
                .map(|entry| OperationKind::RemoveIpv4(entry.address.clone())),
        );
        kinds.extend(desired.iter().cloned().map(OperationKind::AddIpv4));
    } else {
        for actual in observed {
            if !desired
                .iter()
                .any(|expected| actual.address.same_identity(expected))
            {
                kinds.push(OperationKind::RemoveIpv4(actual.address.clone()));
            }
        }
        for (desired, survivor) in desired.iter().zip(survivors) {
            match survivor {
                Some(index) if observed[index].address.lifetimes() != desired.lifetimes() => {
                    kinds.push(OperationKind::ReplaceIpv4(desired.clone()))
                }
                Some(_) => {}
                None => kinds.push(OperationKind::AddIpv4(desired.clone())),
            }
        }
    }
    kinds
        .into_iter()
        .enumerate()
        .map(|(index, kind)| Operation {
            target: target.clone(),
            kind,
            depends_on: if index == 0 {
                Vec::new()
            } else {
                vec![offset + index - 1]
            },
            address_group: (index == 0).then(|| AddressGroupPrecondition {
                addresses: observed.to_vec(),
                observed_at,
            }),
        })
        .collect()
}

fn address_change(
    device: &DeviceObservation,
    desired: &[Ipv4Address],
    source: Source,
) -> FieldChange {
    FieldChange {
        target: device.identity.clone(),
        field: ADDRESS_PATH.into(),
        old: Some(Value::List(
            device.addresses.iter().map(address_value).collect(),
        )),
        desired: Some(Value::List(desired.iter().map(address_value).collect())),
        source,
    }
}

fn address_value(address: impl IntoAddressRef) -> Value {
    let address = address.address_ref();
    let mut fields = IndexMap::new();
    fields.insert(
        "ip".into(),
        Value::IpNetwork((IpAddr::V4(address.ip), address.prefix_len)),
    );
    if let Some(valid) = address.valid_lft {
        fields.insert("valid_lft".into(), Value::U64(u64::from(valid)));
    }
    if let Some(preferred) = address.preferred_lft {
        fields.insert("preferred_lft".into(), Value::U64(u64::from(preferred)));
    }
    Value::Map(fields)
}

trait IntoAddressRef {
    fn address_ref(&self) -> &Ipv4Address;
}
impl IntoAddressRef for &AddressObservation {
    fn address_ref(&self) -> &Ipv4Address {
        &self.address
    }
}
impl IntoAddressRef for &Ipv4Address {
    fn address_ref(&self) -> &Ipv4Address {
        self
    }
}
