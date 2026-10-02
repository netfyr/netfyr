use std::io::{self, BufRead, Write};
use std::os::unix::fs::MetadataExt;

use netfyr_backend::{
    BackendError, FailedOperation, NetlinkBackend, NetworkBackend, OperationContext, PlannedStatus,
    SkipReason,
};
use netfyr_state::plan::{
    AddressGroupPrecondition, AddressObservation, Diagnostic, FieldChange, InterfaceId,
    Ipv4Address, Observation, Operation, OperationKind, StateDiff,
};
use netfyr_state::{Match, Source, Value};
use serde_json::{Value as Json, json};

fn value_json(value: &Value) -> Json {
    match value {
        Value::String(value) => json!(value),
        Value::U64(value) => json!(value),
        Value::I64(value) => json!(value),
        Value::Bool(value) => json!(value),
        Value::IpAddr(value) => json!(value.to_string()),
        Value::IpNetwork((value, prefix)) => json!(format!("{value}/{prefix}")),
        Value::List(values) => Json::Array(values.iter().map(value_json).collect()),
        Value::Map(values) => Json::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), value_json(value)))
                .collect(),
        ),
    }
}

fn identity_json(target: &InterfaceId) -> Json {
    json!({
        "namespace": {"device": target.namespace.device, "inode": target.namespace.inode},
        "index": target.index, "generation": target.generation, "name": target.name,
        "type": target.device_type, "mac": target.mac,
    })
}

fn context_json(context: &OperationContext) -> Json {
    json!({
        "index": context.index, "target": identity_json(&context.target),
        "kind": context.kind.kind_name(), "field": context.kind.field(),
    })
}

fn error_json(error: &BackendError) -> Json {
    json!({
        "kind": format!("{:?}", error.kind), "message": error.message,
        "errno": error.errno, "extack": error.extack,
        "context": error.context.as_deref().map(context_json),
    })
}

fn failed_json(failed: &FailedOperation) -> Json {
    json!({"context": context_json(&failed.context), "error": error_json(&failed.error)})
}

fn reason_name(reason: &SkipReason) -> &'static str {
    match reason {
        SkipReason::AlreadySatisfied => "AlreadySatisfied",
        SkipReason::ReadOnly => "ReadOnly",
        SkipReason::DependencyFailed { .. } => "DependencyFailed",
    }
}

fn diagnostics_json(diagnostics: &[Diagnostic]) -> Vec<Json> {
    diagnostics
        .iter()
        .map(|diagnostic| {
            json!({
                "target": diagnostic.target.as_ref().map(identity_json),
                "field": diagnostic.field, "message": diagnostic.message,
            })
        })
        .collect()
}

fn observation_json(observation: &Observation) -> Json {
    json!({
        "interfaces": observation.devices.iter().map(|device| json!({
            "name": device.identity.name, "type": device.state.device_type,
            "source": format!("{:?}", device.state.source),
            "identity": identity_json(&device.identity),
            "fields": device.state.fields.iter()
                .map(|(key, value)| (key.clone(), value_json(value)))
                .collect::<serde_json::Map<_, _>>(),
            "addresses_complete": device.addresses_complete,
        })).collect::<Vec<_>>(),
        "diagnostics": diagnostics_json(&observation.diagnostics),
    })
}

fn string_field(input: &Json, field: &str) -> Result<String, String> {
    input[field]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("missing string {field}"))
}

fn scalar(input: &Json) -> Result<Value, String> {
    if let Some(value) = input.as_bool() {
        Ok(Value::Bool(value))
    } else if let Some(value) = input.as_u64() {
        Ok(Value::U64(value))
    } else if let Some(value) = input.as_str() {
        Ok(Value::String(value.to_owned()))
    } else {
        Err("expected scalar value".to_owned())
    }
}

fn unsigned(input: &Json, name: &str) -> Result<u64, String> {
    input
        .as_u64()
        .ok_or_else(|| format!("{name} must be an unsigned integer"))
}

fn optional_u32(input: &Json, name: &str) -> Result<Option<u32>, String> {
    input
        .get(name)
        .map(|value| {
            u32::try_from(unsigned(value, name)?).map_err(|_| format!("{name} exceeds u32"))
        })
        .transpose()
}

fn address(input: &Json) -> Result<Ipv4Address, String> {
    let address = string_field(input, "address")?;
    let (ip, prefix) = address.split_once('/').ok_or("address needs a prefix")?;
    Ok(Ipv4Address {
        ip: ip.parse().map_err(|_| "invalid IPv4 address")?,
        prefix_len: prefix.parse().map_err(|_| "invalid prefix")?,
        valid_lft: optional_u32(input, "valid_lft")?,
        preferred_lft: optional_u32(input, "preferred_lft")?,
    })
}

fn plan(input: &Json, cached: &Observation) -> Result<StateDiff, String> {
    let operations = input["operations"]
        .as_array()
        .ok_or("operations must be an array")?;
    let mut projected = cached.devices.clone();
    let mut diff = StateDiff::default();
    for item in operations {
        let name = string_field(item, "target")?;
        let device = projected
            .iter_mut()
            .find(|device| device.identity.name == name)
            .ok_or_else(|| format!("target {name} was not queried"))?;
        let kind = match string_field(item, "kind")?.as_str() {
            "mtu" => OperationKind::SetMtu(
                u32::try_from(unsigned(&item["value"], "MTU")?).map_err(|_| "MTU exceeds u32")?,
            ),
            "enabled" => {
                OperationKind::SetEnabled(item["value"].as_bool().ok_or("enabled must be boolean")?)
            }
            "add" => OperationKind::AddIpv4(address(item)?),
            "remove" => OperationKind::RemoveIpv4(address(item)?),
            "replace" => OperationKind::ReplaceIpv4(address(item)?),
            "readonly" | "unknown" => OperationKind::SetField {
                field: string_field(item, "field")?,
                value: scalar(&item["value"])?,
            },
            _ => return Err("unknown probe operation".to_owned()),
        };
        let address_group = match &kind {
            OperationKind::RemoveIpv4(address) | OperationKind::ReplaceIpv4(address) => {
                address.group().ok_or("invalid address group")?;
                Some(AddressGroupPrecondition {
                    addresses: device
                        .addresses
                        .iter()
                        .filter(|observed| observed.address.group() == address.group())
                        .cloned()
                        .collect(),
                    observed_at: device.observed_at,
                })
            }
            _ => None,
        };
        let desired = match &kind {
            OperationKind::SetMtu(value) => Some(Value::U64(u64::from(*value))),
            OperationKind::SetEnabled(value) => Some(Value::Bool(*value)),
            OperationKind::SetField { value, .. } => Some(value.clone()),
            _ => None,
        };
        if let Some(field) = kind.field() {
            diff.changes.push(FieldChange {
                target: device.identity.clone(),
                field: field.to_owned(),
                old: device.state.fields.get(field).cloned(),
                desired,
                source: Source::Static {
                    policy: "acceptance".to_owned(),
                },
            });
        }
        diff.operations.push(Operation {
            target: device.identity.clone(),
            kind: kind.clone(),
            address_group,
            depends_on: item["dependencies"]
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .map(|value| {
                            usize::try_from(unsigned(value, "dependency")?)
                                .map_err(|_| "dependency exceeds usize".to_owned())
                        })
                        .collect()
                })
                .transpose()?
                .unwrap_or_default(),
        });
        match kind {
            OperationKind::RemoveIpv4(address) => device
                .addresses
                .retain(|observed| !observed.address.same_identity(&address)),
            OperationKind::AddIpv4(address) => {
                if !device
                    .addresses
                    .iter()
                    .any(|observed| observed.address.same_identity(&address))
                {
                    let secondary = device
                        .addresses
                        .iter()
                        .any(|observed| observed.kernel_group() == address.group());
                    let (valid, preferred) = address.lifetimes();
                    let flags = [
                        (secondary, libc::IFA_F_SECONDARY),
                        (valid == u32::MAX, libc::IFA_F_PERMANENT),
                        (preferred == 0, libc::IFA_F_DEPRECATED),
                    ]
                    .into_iter()
                    .filter(|(set, _)| *set)
                    .fold(0, |flags, (_, flag)| flags | flag);
                    device.addresses.push(AddressObservation {
                        address,
                        flags,
                        scope: 0,
                        peer: None,
                    });
                }
            }
            OperationKind::ReplaceIpv4(address) => {
                if let Some(observed) = device
                    .addresses
                    .iter_mut()
                    .find(|observed| observed.address.same_identity(&address))
                {
                    observed.address = address;
                }
            }
            _ => {}
        }
    }
    Ok(diff)
}

fn cache_query(cached: &mut Observation, fresh: &Observation) {
    for device in &fresh.devices {
        cached
            .devices
            .retain(|old| old.identity.name != device.identity.name);
        cached.devices.push(device.clone());
    }
}

async fn request(
    input: &Json,
    backends: &[NetlinkBackend],
    cache: &mut [Observation],
) -> Result<Json, String> {
    let index = usize::try_from(
        input
            .get("backend")
            .map(|value| unsigned(value, "backend index"))
            .transpose()?
            .unwrap_or(0),
    )
    .map_err(|_| "backend index exceeds usize")?;
    let backend = backends.get(index).ok_or("unknown backend index")?;
    match string_field(input, "op")?.as_str() {
        "query_pair" => {
            let second = backends.get(1).ok_or("query_pair requires two backends")?;
            let (first, second) =
                futures_util::future::join(backends[0].query_all(), second.query_all()).await;
            match (first, second) {
                (Ok(first), Ok(second)) => {
                    cache_query(&mut cache[0], &first);
                    cache_query(&mut cache[1], &second);
                    Ok(
                        json!({"observations": [observation_json(&first), observation_json(&second)]}),
                    )
                }
                (Err(error), _) | (_, Err(error)) => Ok(json!({"error": error_json(&error)})),
            }
        }
        "query" => {
            let selector = &input["selector"];
            let selector = Match {
                name: selector["name"].as_str().map(str::to_owned),
                r#type: selector["type"].as_str().map(str::to_owned),
                mac: selector["mac"].as_str().map(str::to_owned),
                driver: selector["driver"].as_str().map(str::to_owned),
                pci_path: selector["pci_path"].as_str().map(str::to_owned),
            };
            match backend.query(&selector).await {
                Ok(observation) => {
                    cache_query(&mut cache[index], &observation);
                    Ok(observation_json(&observation))
                }
                Err(error) => Ok(json!({"error": error_json(&error)})),
            }
        }
        "apply" => {
            let diff = plan(input, &cache[index])?;
            let report = backend.apply(&diff).await;
            Ok(json!({
                "success": report.is_success(),
                "succeeded": report.succeeded.iter().map(context_json).collect::<Vec<_>>(),
                "failed": report.failed.iter().map(failed_json).collect::<Vec<_>>(),
                "skipped": report.skipped.iter().map(|entry| json!({
                    "context": context_json(&entry.context), "reason": reason_name(&entry.reason),
                })).collect::<Vec<_>>(),
                "diagnostics": diagnostics_json(&report.diagnostics),
            }))
        }
        "dry_run" => {
            let diff = plan(input, &cache[index])?;
            let report = backend.dry_run(&diff).await;
            Ok(json!({
                "success": report.is_success(),
                "operations": report.operations.iter().map(|entry| json!({
                    "context": context_json(&entry.context),
                    "skip": match &entry.status {
                        PlannedStatus::Skip(reason) => Some(reason_name(reason)),
                        _ => None,
                    },
                })).collect::<Vec<_>>(),
                "changes": report.changes.iter().map(|change| json!({
                    "target": identity_json(&change.target), "field": change.field,
                    "old": change.old.as_ref().map(value_json), "desired": change.desired.as_ref().map(value_json),
                })).collect::<Vec<_>>(),
                "diagnostics": diagnostics_json(&report.diagnostics),
                "failed": report.failed.iter().map(failed_json).collect::<Vec<_>>(),
            }))
        }
        _ => Err("unknown probe command".to_owned()),
    }
}

fn open_test_namespace(path: String) -> Result<NetlinkBackend, Box<dyn std::error::Error>> {
    let own = std::fs::metadata("/proc/self/ns/net")?;
    let target = std::fs::metadata(&path)?;
    if (own.dev(), own.ino()) == (target.dev(), target.ino()) {
        return Err(format!("{path} is the probe's own network namespace").into());
    }
    Ok(NetlinkBackend::open(path)?)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let backends = std::env::args()
        .skip(1)
        .map(open_test_namespace)
        .collect::<Result<Vec<_>, _>>()?;
    if backends.is_empty() {
        return Err("pass at least one namespace path".into());
    }
    let mut cache = vec![Observation::default(); backends.len()];
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let response = match serde_json::from_str::<Json>(&line?) {
            Ok(input) => runtime
                .block_on(request(&input, &backends, &mut cache))
                .unwrap_or_else(|message| json!({"probe_error": message})),
            Err(error) => json!({"probe_error": format!("invalid request: {error}")}),
        };
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}
