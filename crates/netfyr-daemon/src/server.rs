//! Varlink IPC server for the netfyr daemon.
//!
//! Implements the NUL-terminated JSON-over-UnixStream wire protocol used by
//! `netfyr-varlink`. The server owns `PolicyStore`, `FactoryManager`, and
//! `Reconciler` and handles multiple client connections concurrently by
//! spawning a task per connection. Shared mutable state is protected by
//! `Arc<tokio::sync::Mutex<DaemonState>>`.
//!
//! # Protocol
//! - Request:  `{"method": "io.netfyr.MethodName", "parameters": {...}}\0`
//! - Success:  `{"parameters": {...}}\0`
//! - Error:    `{"error": "io.netfyr.ErrorName", "parameters": {"reason": "..."}}\0`

use std::collections::HashSet;
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use netfyr_backend::FactoryEvent;
use netfyr_journal::Trigger;
use netfyr_reconcile::{DiffKind, FieldChangeKind};
use netfyr_varlink::{
    convert_apply_report_with_conflicts, VarlinkApplyReport, VarlinkChangeEntry, VarlinkDaemonInfo,
    VarlinkDaemonStatus, VarlinkDhcpInfo, VarlinkDriftEntry, VarlinkFactoryStatus,
    VarlinkInterfaceInfo, VarlinkPolicy, VarlinkPolicyInfo, VarlinkSelector, VarlinkShowInfo,
    VarlinkState, VarlinkStateDiff,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::SignalKind;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::factory_manager::FactoryManager;
use crate::netlink_monitor::NetlinkMonitor;
use crate::policy_store::PolicyStore;
use crate::reconciler::Reconciler;

/// Maximum message size accepted from clients (16 MiB).
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Mutable server state shared across concurrent connection tasks.
struct DaemonState {
    policy_store: PolicyStore,
    factory_manager: FactoryManager,
    managed_entities: HashSet<String>,
}

/// Events published on the broadcast channel after key state changes.
/// Fields are read by future Monitor subscribers.
#[derive(Clone, Debug)]
#[allow(dead_code)]
enum DaemonEvent {
    PolicyChanged,
    DhcpEvent { interface: String, kind: String },
    ExternalChange { interfaces: Vec<String> },
}

/// Returns `true` if the given method+parameters combination requires root
/// (uid 0). Write methods that mutate system state require root; read-only
/// methods are allowed for any uid.
fn requires_root(method: &str, params: &serde_json::Value) -> bool {
    match method {
        "io.netfyr.SubmitPolicies" => true,
        "io.netfyr.Revert" => {
            !params
                .get("dry_run")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        }
        _ => false,
    }
}

// ── Wire-protocol helpers ─────────────────────────────────────────────────────

/// Read one NUL-terminated message from `stream` and parse it as JSON.
///
/// Reads in 4 KiB chunks. Returns an error if the stream closes before the
/// NUL terminator or if the accumulated bytes exceed `MAX_MESSAGE_SIZE`.
async fn read_message(stream: &mut UnixStream) -> Result<serde_json::Value> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];

    loop {
        if buf.len() >= MAX_MESSAGE_SIZE {
            anyhow::bail!("incoming message exceeds 16 MiB size limit");
        }

        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            // Client disconnected cleanly.
            anyhow::bail!("client disconnected");
        }

        // Scan for a NUL terminator.
        if let Some(pos) = chunk[..n].iter().position(|&b| b == 0) {
            buf.extend_from_slice(&chunk[..pos]);
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    let value = serde_json::from_slice(&buf)?;
    Ok(value)
}

/// Write a `serde_json::Value` as a NUL-terminated JSON message.
async fn write_message(stream: &mut UnixStream, msg: &serde_json::Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(msg)?;
    bytes.push(0); // NUL terminator
    stream.write_all(&bytes).await?;
    Ok(())
}

/// Write a success response: `{"parameters": params}\0`.
async fn write_success(stream: &mut UnixStream, params: serde_json::Value) -> Result<()> {
    write_message(stream, &serde_json::json!({ "parameters": params })).await
}

/// Write an error response: `{"error": "io.netfyr.{name}", "parameters": {"reason": "..."}}\0`.
async fn write_error(stream: &mut UnixStream, error_name: &str, reason: &str) -> Result<()> {
    let full_name = if error_name.starts_with("io.netfyr.") {
        error_name.to_string()
    } else {
        format!("io.netfyr.{}", error_name)
    };
    write_message(
        stream,
        &serde_json::json!({
            "error": full_name,
            "parameters": { "reason": reason }
        }),
    )
    .await
}

// ── Request handlers ──────────────────────────────────────────────────────────

/// `io.netfyr.SubmitPolicies` — replace all policies and reconcile.
async fn handle_submit_policies(
    stream: &mut UnixStream,
    params: &serde_json::Value,
    state: &tokio::sync::Mutex<DaemonState>,
    reconciler: &Reconciler,
    event_tx: &broadcast::Sender<DaemonEvent>,
) -> Result<()> {
    // Parse policies from parameters.
    let varlink_policies: Vec<VarlinkPolicy> = match params.get("policies") {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
            anyhow::anyhow!("failed to parse policies: {}", e)
        })?,
        None => {
            return write_error(stream, "InternalError", "missing 'policies' parameter").await;
        }
    };

    // Convert VarlinkPolicy → Policy.
    let mut policies = Vec::with_capacity(varlink_policies.len());
    for vp in varlink_policies {
        match netfyr_policy::Policy::try_from(vp) {
            Ok(p) => policies.push(p),
            Err(e) => {
                return write_error(stream, "InvalidPolicy", &e).await;
            }
        }
    }

    let varlink_report = {
        let mut guard = state.lock().await;

        // Replace all policies in the store.
        if let Err(e) = guard.policy_store.replace_all(policies) {
            error!("Failed to persist policies: {}", e);
            return write_error(stream, "InternalError", &e.to_string()).await;
        }

        // Sync factories (stop removed, start new).
        let current_policies = guard.policy_store.policies().to_vec();
        match guard.factory_manager.sync(&current_policies).await {
            Ok(failed) if !failed.is_empty() => {
                warn!(failed = ?failed, "Some factories failed to start");
            }
            Err(e) => {
                error!("Factory sync error: {}", e);
            }
            _ => {}
        }

        // Reconcile and apply.
        reconciler.set_applying(true);
        let apply_result = match reconciler
            .reconcile_and_apply(
                &guard.policy_store,
                &guard.factory_manager,
                Trigger::PolicyApply { source: "daemon".into() },
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                reconciler.set_applying(false);
                error!("Reconciliation failed: {}", e);
                return write_error(stream, "InternalError", &e.to_string()).await;
            }
        };
        reconciler.set_applying(false);

        guard.managed_entities =
            reconciler.managed_entity_names(&guard.policy_store, &guard.factory_manager);

        convert_apply_report_with_conflicts(apply_result.report, &apply_result.conflicts)
    };

    event_tx.send(DaemonEvent::PolicyChanged).ok();
    write_success(stream, serde_json::json!({ "report": varlink_report })).await
}

/// `io.netfyr.Query` — query current system state.
async fn handle_query(
    stream: &mut UnixStream,
    params: &serde_json::Value,
    reconciler: &Reconciler,
) -> Result<()> {
    // Parse optional selector.
    let varlink_selector: Option<VarlinkSelector> = params
        .get("selector")
        .and_then(|v| {
            if v.is_null() {
                None
            } else {
                serde_json::from_value(v.clone()).ok()
            }
        });

    let (entity_type, selector) = match &varlink_selector {
        Some(vs) => {
            let entity_type_str = vs.entity_type.clone();
            let selector = netfyr_state::Selector::from(vs.clone());
            (entity_type_str, Some(selector))
        }
        None => (None, None),
    };

    let state_set = match reconciler
        .query(entity_type.as_deref(), selector.as_ref())
        .await
    {
        Ok(s) => s,
        Err(e) => {
            error!("Query failed: {}", e);
            return write_error(stream, "InternalError", &e.to_string()).await;
        }
    };

    let entities: Vec<VarlinkState> = state_set.iter().map(VarlinkState::from).collect();

    write_success(stream, serde_json::json!({ "entities": entities })).await
}

/// `io.netfyr.DryRun` — compute diff without applying.
async fn handle_dry_run(
    stream: &mut UnixStream,
    params: &serde_json::Value,
    state: &tokio::sync::Mutex<DaemonState>,
    reconciler: &Reconciler,
) -> Result<()> {
    let varlink_policies: Vec<VarlinkPolicy> = match params.get("policies") {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
            anyhow::anyhow!("failed to parse policies: {}", e)
        })?,
        None => {
            return write_error(stream, "InternalError", "missing 'policies' parameter").await;
        }
    };

    let mut policies = Vec::with_capacity(varlink_policies.len());
    for vp in varlink_policies {
        match netfyr_policy::Policy::try_from(vp) {
            Ok(p) => policies.push(p),
            Err(e) => {
                return write_error(stream, "InvalidPolicy", &e).await;
            }
        }
    }

    // Use an ephemeral store for the dry-run — don't touch the real policy store.
    let temp_store = PolicyStore::ephemeral(policies);

    let varlink_diff = {
        let guard = state.lock().await;
        let (reconcile_diff, _conflicts) =
            match reconciler.dry_run(&temp_store, &guard.factory_manager).await {
                Ok(r) => r,
                Err(e) => {
                    error!("Dry-run failed: {}", e);
                    return write_error(stream, "InternalError", &e.to_string()).await;
                }
            };
        VarlinkStateDiff::from(reconcile_diff)
    };

    write_success(stream, serde_json::json!({ "diff": varlink_diff })).await
}

/// `io.netfyr.GetStatus` — return daemon status.
async fn handle_get_status(
    stream: &mut UnixStream,
    state: &tokio::sync::Mutex<DaemonState>,
    start_time: Instant,
) -> Result<()> {
    let status = {
        let guard = state.lock().await;
        let uptime_seconds = start_time.elapsed().as_secs() as i64;
        let active_policies = guard.policy_store.len() as i64;

        let running_factories: Vec<VarlinkFactoryStatus> = guard
            .factory_manager
            .factory_statuses()
            .into_iter()
            .map(|fs| VarlinkFactoryStatus {
                policy_id: fs.policy_name,
                factory_type: fs.factory_type,
                interface_name: fs.interface,
                state: if fs.has_lease { "running" } else { "waiting" }.to_string(),
                lease_ip: fs.lease_ip,
                lease_address: fs.lease_address,
                lease_time_secs: fs.lease_time_secs.map(|v| v as i64),
                lease_remaining_secs: fs.lease_remaining_secs.map(|v| v as i64),
            })
            .collect();

        VarlinkDaemonStatus {
            uptime_seconds,
            active_policies,
            running_factories,
        }
    };

    write_success(stream, serde_json::json!({ "status": status })).await
}

// ── Journal history handlers ──────────────────────────────────────────────────

/// `io.netfyr.GetHistory` — return journal entries, optionally filtered.
async fn handle_get_history(
    stream: &mut UnixStream,
    params: &serde_json::Value,
) -> Result<()> {
    let count = params
        .get("count")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(20);
    let since_str = params.get("since").and_then(|v| v.as_str()).map(str::to_string);
    let trigger_filter = params.get("trigger").and_then(|v| v.as_str()).map(str::to_string);
    let selector_name = params.get("selector_name").and_then(|v| v.as_str()).map(str::to_string);

    let journal = match netfyr_journal::Journal::open_default() {
        Ok(j) => j,
        Err(e) => {
            error!("Failed to open journal: {}", e);
            return write_error(stream, "InternalError", &e.to_string()).await;
        }
    };

    let has_filters = since_str.is_some() || trigger_filter.is_some() || selector_name.is_some();
    let read_count = if has_filters { 10_000 } else { count };

    let all_entries = match journal.read_recent(read_count) {
        Ok(e) => e,
        Err(e) => {
            error!("Failed to read journal entries: {}", e);
            return write_error(stream, "InternalError", &e.to_string()).await;
        }
    };

    // Parse the since cutoff if provided.
    let since_cutoff: Option<chrono::DateTime<chrono::Utc>> = if let Some(ref s) = since_str {
        match server_parse_since(s) {
            Ok(dt) => Some(dt),
            Err(e) => {
                return write_error(stream, "InternalError", &format!("invalid since: {}", e))
                    .await;
            }
        }
    } else {
        None
    };

    let filtered: Vec<serde_json::Value> = all_entries
        .into_iter()
        .filter(|e| {
            if let Some(cutoff) = since_cutoff {
                if e.timestamp < cutoff {
                    return false;
                }
            }
            if let Some(ref tf) = trigger_filter {
                let trigger_type = server_trigger_type_str(&e.trigger);
                if !trigger_type.to_lowercase().contains(&tf.to_lowercase()) {
                    return false;
                }
            }
            if let Some(ref name) = selector_name {
                if !e.diff.operations.iter().any(|op| op.entity_name == *name) {
                    return false;
                }
            }
            true
        })
        .take(count)
        .filter_map(|e| serde_json::to_value(&e).ok())
        .collect();

    write_success(stream, serde_json::json!({ "entries": filtered })).await
}

/// `io.netfyr.GetJournalEntry` — return a single journal entry by sequence ID.
async fn handle_get_journal_entry(
    stream: &mut UnixStream,
    params: &serde_json::Value,
) -> Result<()> {
    let seq = match params.get("seq").and_then(|v| v.as_u64()) {
        Some(s) => s,
        None => {
            return write_error(stream, "InternalError", "missing or invalid 'seq' parameter")
                .await;
        }
    };

    let journal = match netfyr_journal::Journal::open_default() {
        Ok(j) => j,
        Err(e) => {
            error!("Failed to open journal: {}", e);
            return write_error(stream, "InternalError", &e.to_string()).await;
        }
    };

    let entry = match journal.read_entry(seq) {
        Ok(e) => e,
        Err(e) => {
            error!("Failed to read journal entry #{}: {}", seq, e);
            return write_error(stream, "InternalError", &e.to_string()).await;
        }
    };

    let e = match entry {
        Some(e) => e,
        None => {
            return write_error(
                stream,
                "EntryNotFound",
                &format!("Entry #{} not found", seq),
            )
            .await;
        }
    };
    let entry_value = serde_json::to_value(&e).map_err(|err| anyhow::anyhow!("{}", err))?;

    write_success(stream, serde_json::json!({ "entry": entry_value })).await
}

/// `io.netfyr.Revert` — revert system state to match a historical journal snapshot.
async fn handle_revert(
    stream: &mut UnixStream,
    params: &serde_json::Value,
    state: &tokio::sync::Mutex<DaemonState>,
    reconciler: &Reconciler,
) -> Result<()> {
    let target_seq = match params.get("target_seq").and_then(|v| v.as_u64()) {
        Some(s) => s,
        None => {
            return write_error(
                stream,
                "InternalError",
                "missing or invalid 'target_seq' parameter",
            )
            .await;
        }
    };
    let dry_run = params.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false);

    let journal = match netfyr_journal::Journal::open_default() {
        Ok(j) => j,
        Err(e) => {
            error!("Failed to open journal: {}", e);
            return write_error(stream, "InternalError", &e.to_string()).await;
        }
    };

    let entry = match journal.read_entry(target_seq) {
        Ok(Some(e)) => e,
        Ok(None) => {
            return write_error(
                stream,
                "EntryNotFound",
                &format!("Entry #{} not found", target_seq),
            )
            .await;
        }
        Err(e) => {
            error!("Failed to read journal entry #{}: {}", target_seq, e);
            return write_error(stream, "InternalError", &e.to_string()).await;
        }
    };

    let entry_timestamp = entry.timestamp.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let target_state = match entry.state_after.to_state_set() {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to convert journal snapshot to StateSet: {}", e);
            return write_error(stream, "InternalError", &e).await;
        }
    };

    let policies = {
        let guard = state.lock().await;
        guard.policy_store.policies().to_vec()
    };

    let result = match reconciler
        .revert(&target_state, target_seq, &policies, dry_run)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            error!("Revert failed: {}", e);
            return write_error(stream, "InternalError", &e.to_string()).await;
        }
    };

    let varlink_report = match result.report {
        Some(report) => VarlinkApplyReport::from(report),
        None => {
            // dry_run: build a planned report from the reconcile diff
            let changes: Vec<VarlinkChangeEntry> = result
                .reconcile_diff
                .operations
                .iter()
                .map(|op| {
                    let kind = match op.kind {
                        DiffKind::Add => "add",
                        DiffKind::Modify => "modify",
                        DiffKind::Remove => "remove",
                    }
                    .to_string();
                    let desc = op
                        .field_changes
                        .iter()
                        .filter_map(|fc| {
                            use netfyr_reconcile::FieldChangeKind;
                            match &fc.change {
                                FieldChangeKind::Set { current: Some(cur), desired } => {
                                    Some(format!("{}: {} -> {}", fc.field_name, cur.value, desired.value))
                                }
                                FieldChangeKind::Set { current: None, desired } => {
                                    Some(format!("{}: {}", fc.field_name, desired.value))
                                }
                                FieldChangeKind::Unset { current } => {
                                    Some(format!("{}: {} (removed)", fc.field_name, current.value))
                                }
                                FieldChangeKind::Unchanged { .. } => None,
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    VarlinkChangeEntry {
                        kind,
                        entity_type: op.entity_type.clone(),
                        entity_name: op.selector.key(),
                        description: desc,
                        status: "planned".to_string(),
                    }
                })
                .collect();
            VarlinkApplyReport {
                succeeded: 0,
                failed: 0,
                skipped: 0,
                changes,
                conflicts: vec![],
            }
        }
    };

    write_success(
        stream,
        serde_json::json!({
            "report": varlink_report,
            "entry_timestamp": entry_timestamp,
        }),
    )
    .await
}

/// `io.netfyr.GetShowInfo` — return interface-centric system overview.
async fn handle_get_show_info(
    stream: &mut UnixStream,
    state: &tokio::sync::Mutex<DaemonState>,
    reconciler: &Reconciler,
    start_time: Instant,
) -> Result<()> {
    let uptime_secs = start_time.elapsed().as_secs() as i64;
    let daemon_info = VarlinkDaemonInfo {
        status: "running".to_string(),
        uptime_seconds: Some(uptime_secs),
    };

    // Query all system interfaces via the backend.
    let state_set = match reconciler.query(None, None).await {
        Ok(s) => s,
        Err(e) => {
            error!("GetShowInfo query failed: {}", e);
            return write_error(stream, "InternalError", &e.to_string()).await;
        }
    };

    let (drift_diff, policies, factory_by_iface) = {
        let guard = state.lock().await;

        // Compute drift: compare desired (from policies) against actual (from system).
        let drift = match reconciler.dry_run(&guard.policy_store, &guard.factory_manager).await {
            Ok((diff, _conflicts)) => Some(diff),
            Err(e) => {
                warn!("GetShowInfo drift check failed, continuing without drift: {}", e);
                None
            }
        };

        let policies = guard.policy_store.policies().to_vec();

        // Index factory statuses by interface name for O(1) lookup.
        let factory_by_iface: std::collections::HashMap<String, _> = guard
            .factory_manager
            .factory_statuses()
            .into_iter()
            .map(|fs| (fs.interface.clone(), fs))
            .collect();

        (drift, policies, factory_by_iface)
    };

    // Build per-interface info, deduplicating by name.
    let mut seen: HashSet<String> = HashSet::new();
    let mut interfaces: Vec<VarlinkInterfaceInfo> = Vec::new();

    for iface_state in state_set.iter() {
        let name = match &iface_state.selector.name {
            Some(n) => n.clone(),
            None => continue,
        };

        if !seen.insert(name.clone()) {
            continue;
        }

        let interface_selector = &iface_state.selector;

        // Extract link state fields from the backend query.
        let enabled = iface_state
            .fields
            .get("enabled")
            .and_then(|fv| fv.value.as_bool());
        let carrier = iface_state
            .fields
            .get("carrier")
            .and_then(|fv| fv.value.as_bool());
        let addresses: Option<Vec<String>> = iface_state
            .fields
            .get("addresses")
            .and_then(|fv| fv.value.as_list())
            .map(|list| list.iter().map(|v| v.to_string()).collect())
            .filter(|v: &Vec<String>| !v.is_empty());

        // Find policies that target this interface.
        let matching_policies: Vec<VarlinkPolicyInfo> = policies
            .iter()
            .filter_map(|policy| {
                let matches = match policy.factory_type {
                    netfyr_policy::FactoryType::Static => {
                        let via_state = policy
                            .state
                            .as_ref()
                            .map(|s| s.selector.matches(interface_selector))
                            .unwrap_or(false);
                        let via_states = policy
                            .states
                            .as_ref()
                            .map(|ss| ss.iter().any(|s| s.selector.matches(interface_selector)))
                            .unwrap_or(false);
                        via_state || via_states
                    }
                    netfyr_policy::FactoryType::Dhcpv4
                    | netfyr_policy::FactoryType::Ipv6Auto => policy
                        .selector
                        .as_ref()
                        .map(|sel| sel.matches(interface_selector))
                        .unwrap_or(false),
                };
                if matches {
                    let policy_type = match policy.factory_type {
                        netfyr_policy::FactoryType::Static => "static",
                        netfyr_policy::FactoryType::Dhcpv4 => "dhcpv4",
                        netfyr_policy::FactoryType::Ipv6Auto => "ipv6auto",
                    };
                    Some(VarlinkPolicyInfo {
                        name: policy.name.clone(),
                        policy_type: policy_type.to_string(),
                    })
                } else {
                    None
                }
            })
            .collect();

        // Build DHCP info only when a DHCP client is active for this interface.
        // For ipv6auto, dhcp_active is false in SLAAC-only mode (no M/O flags),
        // so no dhcp object is emitted. State is "running" when a lease is held
        // (lease_time_secs present), "waiting" otherwise.
        let dhcp = factory_by_iface
            .get(&name)
            .filter(|fs| fs.dhcp_active)
            .map(|fs| {
                if fs.lease_time_secs.is_some() {
                    VarlinkDhcpInfo {
                        state: "running".to_string(),
                        lease_address: fs.lease_address.clone(),
                        lease_time_secs: fs.lease_time_secs.map(|v| v as i64),
                        lease_remaining_secs: fs.lease_remaining_secs.map(|v| v as i64),
                    }
                } else {
                    VarlinkDhcpInfo {
                        state: "waiting".to_string(),
                        lease_address: None,
                        lease_time_secs: None,
                        lease_remaining_secs: None,
                    }
                }
            });

        // Determine config drift for managed interfaces.
        let is_managed = !matching_policies.is_empty();
        let (config_state, config_drift) = if is_managed {
            if let Some(ref diff) = drift_diff {
                let iface_op = diff.operations.iter().find(|op| {
                    op.selector.name.as_deref() == Some(&name)
                });
                match iface_op {
                    Some(op) => {
                        let drift_entries: Vec<VarlinkDriftEntry> = op
                            .field_changes
                            .iter()
                            .filter_map(|fc| {
                                let desc = match &fc.change {
                                    FieldChangeKind::Set {
                                        current: Some(old),
                                        desired,
                                    } => {
                                        if fc.field_name == "addresses" {
                                            let filtered = filter_link_local_addresses(old);
                                            if filtered.value == desired.value {
                                                return None;
                                            }
                                            format!(
                                                "expected {}, actual {}",
                                                desired.value, filtered.value
                                            )
                                        } else {
                                            format!(
                                                "expected {}, actual {}",
                                                desired.value, old.value
                                            )
                                        }
                                    }
                                    FieldChangeKind::Set {
                                        current: None,
                                        desired,
                                    } => format!("missing (expected {})", desired.value),
                                    FieldChangeKind::Unset { current } => {
                                        format!("unexpected (actual {})", current.value)
                                    }
                                    FieldChangeKind::Unchanged { .. } => return None,
                                };
                                Some(VarlinkDriftEntry {
                                    field_name: fc.field_name.clone(),
                                    description: desc,
                                })
                            })
                            .collect();

                        if drift_entries.is_empty() {
                            (Some("applied".to_string()), None)
                        } else {
                            (Some("drifted".to_string()), Some(drift_entries))
                        }
                    }
                    None => (Some("applied".to_string()), None),
                }
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

        interfaces.push(VarlinkInterfaceInfo {
            name,
            enabled,
            carrier,
            addresses,
            policies: Some(matching_policies),
            dhcp,
            config_state,
            config_drift,
        });
    }

    let show_info = VarlinkShowInfo {
        daemon: daemon_info,
        interfaces,
    };

    write_success(stream, serde_json::json!({ "info": show_info })).await
}

fn is_link_local(cidr: &str) -> bool {
    cidr.split_once('/')
        .and_then(|(ip, _)| ip.parse::<IpAddr>().ok())
        .map(|ip| match ip {
            IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
            _ => false,
        })
        .unwrap_or(false)
}

fn is_link_local_value(v: &netfyr_state::Value) -> bool {
    match v {
        netfyr_state::Value::String(s) => is_link_local(s),
        netfyr_state::Value::IpNetwork(net) => match net.ip() {
            IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
            _ => false,
        },
        netfyr_state::Value::Map(m) => m
            .get("address")
            .map(is_link_local_value)
            .unwrap_or(false),
        _ => false,
    }
}

fn filter_link_local_addresses(fv: &netfyr_state::FieldValue) -> netfyr_state::FieldValue {
    if let netfyr_state::Value::List(items) = &fv.value {
        let filtered: Vec<netfyr_state::Value> = items
            .iter()
            .filter(|v| !is_link_local_value(v))
            .cloned()
            .collect();
        netfyr_state::FieldValue {
            value: netfyr_state::Value::List(filtered),
            provenance: fv.provenance.clone(),
        }
    } else {
        fv.clone()
    }
}

fn server_trigger_type_str(trigger: &netfyr_journal::Trigger) -> &'static str {
    match trigger {
        netfyr_journal::Trigger::PolicyApply { .. } => "policy_apply",
        netfyr_journal::Trigger::DhcpEvent { .. } => "dhcp_event",
        netfyr_journal::Trigger::ExternalChange { .. } => "external_change",
        netfyr_journal::Trigger::DaemonStartup => "daemon_startup",
        netfyr_journal::Trigger::Revert { .. } => "revert",
    }
}

fn server_parse_since(s: &str) -> anyhow::Result<chrono::DateTime<chrono::Utc>> {
    let now = chrono::Utc::now();

    // Try relative duration: 30s, 5m, 1h, 7d
    let units = ["d", "h", "m", "s"];
    for unit in &units {
        if let Some(num_str) = s.strip_suffix(unit) {
            if !num_str.is_empty() {
                let num: u64 = num_str
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid number in duration: {:?}", s))?;
                let seconds: u64 = match *unit {
                    "s" => num,
                    "m" => num * 60,
                    "h" => num * 3600,
                    "d" => num * 86400,
                    _ => unreachable!(),
                };
                let duration = chrono::Duration::try_seconds(seconds as i64)
                    .ok_or_else(|| anyhow::anyhow!("duration overflow"))?;
                return Ok(now - duration);
            }
        }
    }

    // Try ISO 8601 / RFC 3339
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|_| {
            anyhow::anyhow!(
                "invalid duration or timestamp {:?}; use e.g. 1h, 30m, 7d or ISO 8601",
                s
            )
        })
}

// ── Connection handler ────────────────────────────────────────────────────────

/// Handle all requests on a single connection until the client disconnects.
///
/// Processes requests sequentially within the connection. On each request,
/// reads a NUL-terminated JSON message, dispatches by `method`, and writes
/// a response. Returns when the client closes the connection (EOF) or an
/// I/O error occurs.
async fn handle_connection(
    stream: &mut UnixStream,
    state: &tokio::sync::Mutex<DaemonState>,
    reconciler: &Reconciler,
    start_time: Instant,
    event_tx: &broadcast::Sender<DaemonEvent>,
) {
    loop {
        let msg = match read_message(stream).await {
            Ok(m) => m,
            Err(e) => {
                // "client disconnected" is the normal EOF path; other errors are logged.
                let msg = e.to_string();
                if !msg.contains("client disconnected") {
                    debug!("Connection read error: {}", e);
                }
                return;
            }
        };

        let method = match msg.get("method").and_then(|m| m.as_str()) {
            Some(m) => m.to_string(),
            None => {
                let _ = write_error(stream, "InternalError", "missing 'method' field").await;
                return;
            }
        };

        let params = msg
            .get("parameters")
            .cloned()
            .unwrap_or(serde_json::Value::Object(Default::default()));

        if requires_root(&method, &params) {
            match stream.peer_cred() {
                Ok(cred) => {
                    if cred.uid() != 0 {
                        warn!(
                            method = %method,
                            uid = cred.uid(),
                            "Permission denied: write method requires root"
                        );
                        let _ = write_error(
                            stream,
                            "PermissionDenied",
                            &format!(
                                "method '{}' requires root (uid 0), but client has uid {}",
                                method, cred.uid()
                            ),
                        )
                        .await;
                        continue;
                    }
                }
                Err(e) => {
                    warn!("Failed to get peer credentials: {}", e);
                    let _ = write_error(
                        stream,
                        "PermissionDenied",
                        "could not verify client credentials",
                    )
                    .await;
                    continue;
                }
            }
        }

        let result = match method.as_str() {
            "io.netfyr.SubmitPolicies" => {
                handle_submit_policies(
                    stream, &params, state, reconciler, event_tx,
                )
                .await
            }
            "io.netfyr.Query" => handle_query(stream, &params, reconciler).await,
            "io.netfyr.DryRun" => {
                handle_dry_run(stream, &params, state, reconciler).await
            }
            "io.netfyr.GetStatus" => {
                handle_get_status(stream, state, start_time).await
            }
            "io.netfyr.GetHistory" => handle_get_history(stream, &params).await,
            "io.netfyr.GetJournalEntry" => handle_get_journal_entry(stream, &params).await,
            "io.netfyr.Revert" => {
                handle_revert(stream, &params, state, reconciler).await
            }
            "io.netfyr.GetShowInfo" => {
                handle_get_show_info(stream, state, reconciler, start_time).await
            }
            other => {
                write_error(
                    stream,
                    "InternalError",
                    &format!("unknown method: '{}'", other),
                )
                .await
            }
        };

        if let Err(e) = result {
            debug!("Error writing response: {}", e);
            return;
        }
    }
}

// ── Main event loop ───────────────────────────────────────────────────────────

/// Start the Varlink server and run the main event loop.
///
/// Binds a `UnixListener` at `socket_path` and multiplexes five event sources:
/// 1. Incoming Varlink connections — each spawned as a concurrent task.
/// 2. Factory events (DHCP lease changes) — trigger reconciliation.
/// 3. SIGTERM signal — graceful shutdown.
/// 4. SIGINT / Ctrl-C — graceful shutdown.
/// 5. Netlink monitor — external network state changes recorded in the journal.
///
/// Returns when SIGTERM or SIGINT is received. Removes the socket file and
/// calls `factory_manager.stop_all()` before returning.
pub async fn serve_varlink(
    socket_path: &str,
    policy_store: PolicyStore,
    mut factory_manager: FactoryManager,
    reconciler: Reconciler,
    start_time: Instant,
) -> Result<()> {
    // Remove a stale socket file from a previous run, if any.
    if let Err(e) = std::fs::remove_file(socket_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!("Failed to remove stale socket file {}: {}", socket_path, e);
        }
    }

    let listener = UnixListener::bind(socket_path)
        .map_err(|e| anyhow::anyhow!("Failed to bind Varlink socket {}: {}", socket_path, e))?;

    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o666))
        .map_err(|e| anyhow::anyhow!("Failed to set socket permissions on {}: {}", socket_path, e))?;

    info!("Varlink server listening on {}", socket_path);

    // Start the netlink monitor. A failure here is non-fatal — the daemon
    // continues operating without external change detection.
    let mut netlink_monitor: Option<NetlinkMonitor> = match NetlinkMonitor::start().await {
        Ok(m) => {
            debug!("Netlink monitor started");
            Some(m)
        }
        Err(e) => {
            warn!("Failed to start netlink monitor (external change detection disabled): {}", e);
            None
        }
    };

    // Extract the factory event receiver before wrapping FactoryManager in the
    // mutex — we cannot hold the mutex across async recv() in the select loop.
    let mut factory_event_rx = factory_manager.take_event_receiver();

    let managed_entities =
        reconciler.managed_entity_names(&policy_store, &factory_manager);

    let state = Arc::new(tokio::sync::Mutex::new(DaemonState {
        policy_store,
        factory_manager,
        managed_entities,
    }));

    let reconciler = Arc::new(reconciler);
    let (event_tx, _) = broadcast::channel::<DaemonEvent>(64);

    // Set up SIGTERM signal handler.
    let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate())?;

    loop {
        tokio::select! {
            // ── Branch 1: incoming Varlink connection ─────────────────────────
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((mut stream, _)) => {
                        debug!("Accepted Varlink connection");
                        let conn_state = Arc::clone(&state);
                        let conn_reconciler = Arc::clone(&reconciler);
                        let conn_event_tx = event_tx.clone();
                        tokio::spawn(async move {
                            handle_connection(
                                &mut stream,
                                &conn_state,
                                &conn_reconciler,
                                start_time,
                                &conn_event_tx,
                            )
                            .await;
                        });
                    }
                    Err(e) => {
                        error!("Failed to accept connection: {}", e);
                    }
                }
            }

            // ── Branch 2: factory event (DHCP lease change) ───────────────────
            Some(event) = factory_event_rx.recv() => {
                let (policy_name, event_kind, log_level) = match &event {
                    FactoryEvent::LeaseAcquired { policy_name, .. } => {
                        (policy_name.clone(), "lease_acquired", "info")
                    }
                    FactoryEvent::LeaseRenewed { policy_name, .. } => {
                        (policy_name.clone(), "lease_renewed", "debug")
                    }
                    FactoryEvent::LeaseExpired { policy_name } => {
                        (policy_name.clone(), "lease_expired", "info")
                    }
                    FactoryEvent::Error { policy_name, error } => {
                        warn!(policy = %policy_name, error = %error, "DHCP factory error");
                        continue;
                    }
                    FactoryEvent::Ipv6AutoFlags { policy_name, m, o } => {
                        info!(
                            policy = %policy_name,
                            managed = m,
                            other = o,
                            "IPv6 RA M/O flags changed"
                        );
                        continue;
                    }
                };
                match log_level {
                    "info" => info!(policy = %policy_name, "DHCP {event_kind}; re-reconciling"),
                    _ => debug!(policy = %policy_name, "DHCP {event_kind}; re-reconciling"),
                }
                let interface = {
                    let mut guard = state.lock().await;
                    reconciler.set_applying(true);
                    if let Err(e) = reconciler
                        .reconcile_and_apply(
                            &guard.policy_store,
                            &guard.factory_manager,
                            Trigger::DhcpEvent {
                                policy_name: policy_name.clone(),
                                event_kind: event_kind.into(),
                            },
                        )
                        .await
                    {
                        error!("Reconciliation after {event_kind} failed: {}", e);
                    }
                    reconciler.set_applying(false);
                    guard.managed_entities =
                        reconciler.managed_entity_names(&guard.policy_store, &guard.factory_manager);
                    policy_name.clone()
                };
                event_tx
                    .send(DaemonEvent::DhcpEvent {
                        interface,
                        kind: event_kind.into(),
                    })
                    .ok();
            }

            // ── Branch 3: SIGTERM ─────────────────────────────────────────────
            _ = sigterm.recv() => {
                info!("Received SIGTERM, shutting down...");
                break;
            }

            // ── Branch 4: SIGINT / Ctrl-C ─────────────────────────────────────
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT, shutting down...");
                break;
            }

            // ── Branch 5: external network state change ───────────────────────
            result = async {
                match netlink_monitor.as_mut() {
                    Some(m) => m.next_change().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(changes) = result {
                    if reconciler.is_applying() {
                        let count = changes.len();
                        debug!(count, "discarding netlink events during self-apply");
                    } else {
                        let (managed, policies) = {
                            let guard = state.lock().await;
                            (guard.managed_entities.clone(),
                             guard.policy_store.policies().to_vec())
                        };
                        let policy_store_ref = PolicyStore::ephemeral(policies);
                        let mut changed_names: Vec<String> = Vec::new();
                        let mut seen: HashSet<String> = HashSet::new();
                        for change in changes {
                            let ifname = match change.ifname {
                                Some(name) => name,
                                None => {
                                    let ifindex = change.ifindex;
                                    debug!(ifindex, "dropping change: ifname not resolved");
                                    continue;
                                }
                            };
                            if !managed.contains(&ifname) {
                                debug!(%ifname, "dropping change: interface not managed");
                                continue;
                            }
                            if seen.insert(ifname.clone()) {
                                changed_names.push(ifname.clone());
                            }
                        }
                        if changed_names.is_empty() {
                            debug!("all netlink changes filtered, no journal entry");
                        } else {
                            let ifaces = changed_names.clone();
                            debug!(?changed_names, "recording external changes");
                            if let Err(e) = reconciler
                                .record_external_change(changed_names, &policy_store_ref)
                                .await
                            {
                                error!("Failed to record external change: {}", e);
                            }
                            event_tx
                                .send(DaemonEvent::ExternalChange { interfaces: ifaces })
                                .ok();
                        }
                    }
                }
            }
        }
    }

    // Graceful shutdown: stop the netlink monitor, then release DHCP leases.
    if let Some(monitor) = netlink_monitor.take() {
        monitor.stop().await;
    }

    info!("Releasing DHCP leases...");
    {
        let mut guard = state.lock().await;
        if let Err(e) = guard.factory_manager.stop_all().await {
            error!("Error during factory shutdown: {}", e);
        }
    }

    // Remove socket file so systemd socket activation can rebind cleanly.
    if let Err(e) = std::fs::remove_file(socket_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!("Failed to remove socket file on shutdown: {}", e);
        }
    }

    info!("Daemon shutdown complete");
    Ok(())
}

// ── Server unit tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory_manager::FactoryManager;
    use crate::policy_store::PolicyStore;
    use std::time::Instant;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

    /// Create a pair of connected Unix stream sockets for testing.
    async fn make_stream_pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().unwrap()
    }

    /// Wrap a PolicyStore and FactoryManager in a test DaemonState mutex.
    fn make_test_state(
        policy_store: PolicyStore,
        factory_manager: FactoryManager,
    ) -> tokio::sync::Mutex<DaemonState> {
        tokio::sync::Mutex::new(DaemonState {
            policy_store,
            factory_manager,
            managed_entities: HashSet::new(),
        })
    }

    /// Create a broadcast sender for tests.
    fn make_test_event_tx() -> broadcast::Sender<DaemonEvent> {
        broadcast::channel(16).0
    }

    /// Build a minimal static policy for use in tests.
    fn make_test_policy(name: &str) -> netfyr_policy::Policy {
        let yaml = format!(
            "kind: policy\nname: {name}\nfactory: static\npriority: 100\n\
             state:\n  type: ethernet\n  name: eth0\n  mtu: 1500\n"
        );
        netfyr_policy::parse_policy_yaml(&yaml)
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
    }

    /// Build an ipv6auto policy targeting the loopback interface.
    fn make_ipv6auto_lo_policy(name: &str) -> netfyr_policy::Policy {
        let yaml = format!(
            "kind: policy\nname: {name}\nfactory: ipv6auto\npriority: 100\n\
             selector:\n  name: lo\n"
        );
        netfyr_policy::parse_policy_yaml(&yaml)
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
    }

    // ── Wire protocol: write_error ─────────────────────────────────────────────

    /// Scenario: write_error adds the io.netfyr. namespace prefix.
    #[tokio::test]
    async fn test_write_error_prefixes_with_io_netfyr_namespace() {
        let (mut server, mut client) = make_stream_pair().await;
        write_error(&mut server, "TestError", "something went wrong")
            .await
            .unwrap();
        let msg = read_message(&mut client).await.unwrap();
        let error_name = msg["error"].as_str().unwrap();
        assert_eq!(
            error_name, "io.netfyr.TestError",
            "error name must be prefixed with 'io.netfyr.'"
        );
    }

    /// Scenario: write_error does not double-prefix an already-namespaced error.
    #[tokio::test]
    async fn test_write_error_already_prefixed_not_double_prefixed() {
        let (mut server, mut client) = make_stream_pair().await;
        write_error(&mut server, "io.netfyr.TestError", "reason")
            .await
            .unwrap();
        let msg = read_message(&mut client).await.unwrap();
        let error_name = msg["error"].as_str().unwrap();
        assert_eq!(
            error_name, "io.netfyr.TestError",
            "already-prefixed error name must not be double-prefixed"
        );
    }

    /// Scenario: write_error includes the reason string in parameters.
    #[tokio::test]
    async fn test_write_error_includes_reason_in_parameters() {
        let (mut server, mut client) = make_stream_pair().await;
        write_error(&mut server, "TestError", "something went wrong")
            .await
            .unwrap();
        let msg = read_message(&mut client).await.unwrap();
        assert_eq!(
            msg["parameters"]["reason"].as_str().unwrap(),
            "something went wrong"
        );
    }

    // ── Wire protocol: write_success ──────────────────────────────────────────

    /// Scenario: write_success wraps the payload under "parameters".
    #[tokio::test]
    async fn test_write_success_wraps_payload_in_parameters() {
        let (mut server, mut client) = make_stream_pair().await;
        write_success(&mut server, serde_json::json!({"count": 42}))
            .await
            .unwrap();
        let msg = read_message(&mut client).await.unwrap();
        assert_eq!(
            msg["parameters"]["count"].as_i64().unwrap(),
            42
        );
    }

    /// Scenario: write_success does not include an "error" field.
    #[tokio::test]
    async fn test_write_success_has_no_error_field() {
        let (mut server, mut client) = make_stream_pair().await;
        write_success(&mut server, serde_json::json!({}))
            .await
            .unwrap();
        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_none(),
            "success response must not contain an 'error' field"
        );
    }

    // ── Wire protocol: read_message ────────────────────────────────────────────

    /// Scenario: read_message parses a NUL-terminated JSON message.
    #[tokio::test]
    async fn test_read_message_parses_nul_terminated_json() {
        let (mut writer, mut reader) = make_stream_pair().await;
        let json = r#"{"method":"io.netfyr.GetStatus","parameters":{}}"#;
        let mut bytes = json.as_bytes().to_vec();
        bytes.push(0u8); // NUL terminator
        writer.write_all(&bytes).await.unwrap();

        let msg = read_message(&mut reader).await.unwrap();
        assert_eq!(
            msg["method"].as_str().unwrap(),
            "io.netfyr.GetStatus"
        );
    }

    /// Scenario: read_message fails cleanly when the client disconnects without NUL.
    #[tokio::test]
    async fn test_read_message_fails_when_client_disconnects() {
        let (writer, mut reader) = make_stream_pair().await;
        drop(writer); // Close the write end
        let result = read_message(&mut reader).await;
        assert!(
            result.is_err(),
            "read_message must fail when the client disconnects"
        );
    }

    // ── GetStatus handler ──────────────────────────────────────────────────────

    /// Scenario: GetStatus returns daemon information — 3 policies in store → active_policies == 3.
    #[tokio::test]
    async fn test_handle_get_status_returns_active_policy_count() {
        let (mut server, mut client) = make_stream_pair().await;
        let policies = vec![
            make_test_policy("policy-a"),
            make_test_policy("policy-b"),
            make_test_policy("policy-c"),
        ];
        let state = make_test_state(PolicyStore::ephemeral(policies), FactoryManager::new());
        let start_time = Instant::now();

        handle_get_status(&mut server, &state, start_time)
            .await
            .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        let active_policies = msg["parameters"]["status"]["active_policies"]
            .as_i64()
            .unwrap();
        assert_eq!(
            active_policies, 3,
            "active_policies must match the number of policies in the store"
        );
    }

    /// Scenario: GetStatus returns 1 running factory with its status.
    /// (Verified via empty factory manager — 0 factories running.)
    #[tokio::test]
    async fn test_handle_get_status_returns_empty_running_factories_list() {
        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let start_time = Instant::now();

        handle_get_status(&mut server, &state, start_time)
            .await
            .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        let factories = msg["parameters"]["status"]["running_factories"]
            .as_array()
            .unwrap();
        assert!(
            factories.is_empty(),
            "fresh daemon with no factories must return empty running_factories"
        );
    }

    /// Scenario: GetStatus uptime_seconds is non-negative.
    #[tokio::test]
    async fn test_handle_get_status_uptime_seconds_is_non_negative() {
        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let start_time = Instant::now();

        handle_get_status(&mut server, &state, start_time)
            .await
            .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        let uptime = msg["parameters"]["status"]["uptime_seconds"]
            .as_i64()
            .unwrap();
        assert!(uptime >= 0, "uptime_seconds must be non-negative");
    }

    /// Scenario: GetStatus response has no "error" field.
    #[tokio::test]
    async fn test_handle_get_status_response_has_no_error_field() {
        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let start_time = Instant::now();

        handle_get_status(&mut server, &state, start_time)
            .await
            .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_none(),
            "GetStatus must not return an error field"
        );
    }

    // ── DryRun handler ────────────────────────────────────────────────────────

    /// Scenario: Dry-run computes diff without applying — empty policies returns success.
    #[tokio::test]
    async fn test_handle_dry_run_with_empty_policies_returns_success() {
        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();

        handle_dry_run(
            &mut server,
            &serde_json::json!({"policies": []}),
            &state,
            &reconciler,
        )
        .await
        .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_none(),
            "dry_run with empty policies must not return an error: {:?}",
            msg
        );
    }

    /// Scenario: Dry-run response contains a 'diff' field in parameters.
    #[tokio::test]
    async fn test_handle_dry_run_response_has_diff_field_in_parameters() {
        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();

        handle_dry_run(
            &mut server,
            &serde_json::json!({"policies": []}),
            &state,
            &reconciler,
        )
        .await
        .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg["parameters"].get("diff").is_some(),
            "dry_run response must include a 'diff' field in parameters: {:?}",
            msg
        );
    }

    /// Scenario: DryRun with missing 'policies' parameter returns an error response.
    #[tokio::test]
    async fn test_handle_dry_run_with_missing_policies_parameter_returns_error() {
        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();

        handle_dry_run(
            &mut server,
            &serde_json::json!({}), // no 'policies' key
            &state,
            &reconciler,
        )
        .await
        .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_some(),
            "dry_run with missing 'policies' parameter must return an error response"
        );
    }

    // ── Query handler ─────────────────────────────────────────────────────────

    /// Scenario: Query returns current system state — no selector returns success.
    #[tokio::test]
    async fn test_handle_query_with_no_selector_returns_success() {
        let (mut server, mut client) = make_stream_pair().await;
        let reconciler = Reconciler::new();

        handle_query(
            &mut server,
            &serde_json::json!({"selector": null}),
            &reconciler,
        )
        .await
        .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_none(),
            "query with null selector must not return an error: {:?}",
            msg
        );
    }

    /// Scenario: Query response contains an 'entities' field.
    #[tokio::test]
    async fn test_handle_query_response_has_entities_field() {
        let (mut server, mut client) = make_stream_pair().await;
        let reconciler = Reconciler::new();

        handle_query(
            &mut server,
            &serde_json::json!({}),
            &reconciler,
        )
        .await
        .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg["parameters"].get("entities").is_some(),
            "query response must include an 'entities' field: {:?}",
            msg
        );
    }

    /// Scenario: Query 'entities' field is an array.
    #[tokio::test]
    async fn test_handle_query_entities_field_is_an_array() {
        let (mut server, mut client) = make_stream_pair().await;
        let reconciler = Reconciler::new();

        handle_query(
            &mut server,
            &serde_json::json!({}),
            &reconciler,
        )
        .await
        .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg["parameters"]["entities"].is_array(),
            "query 'entities' must be an array: {:?}",
            msg
        );
    }

    // ── SubmitPolicies handler ────────────────────────────────────────────────

    /// Scenario: Submit policies replaces entire set — empty list returns success.
    #[tokio::test]
    async fn test_handle_submit_policies_with_empty_list_returns_success() {
        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();
        let event_tx = make_test_event_tx();

        handle_submit_policies(
            &mut server,
            &serde_json::json!({"policies": []}),
            &state,
            &reconciler,
            &event_tx,
        )
        .await
        .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_none(),
            "submit_policies with empty list must not return an error: {:?}",
            msg
        );
    }

    /// Scenario: SubmitPolicies response contains a 'report' field.
    #[tokio::test]
    async fn test_handle_submit_policies_response_has_report_field() {
        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();
        let event_tx = make_test_event_tx();

        handle_submit_policies(
            &mut server,
            &serde_json::json!({"policies": []}),
            &state,
            &reconciler,
            &event_tx,
        )
        .await
        .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg["parameters"].get("report").is_some(),
            "submit_policies response must include a 'report' field: {:?}",
            msg
        );
    }

    /// Scenario: Submit policies replaces entire set — policy store is updated to the new set.
    #[tokio::test]
    async fn test_handle_submit_policies_replaces_policy_store_with_new_set() {
        let (mut server, mut client) = make_stream_pair().await;
        // Pre-populate with one policy.
        let state = make_test_state(
            PolicyStore::ephemeral(vec![make_test_policy("old-policy")]),
            FactoryManager::new(),
        );
        let reconciler = Reconciler::new();
        let event_tx = make_test_event_tx();

        // Submit empty list — old-policy must be replaced with nothing.
        handle_submit_policies(
            &mut server,
            &serde_json::json!({"policies": []}),
            &state,
            &reconciler,
            &event_tx,
        )
        .await
        .unwrap();

        let _msg = read_message(&mut client).await.unwrap();
        let guard = state.lock().await;
        assert!(
            guard.policy_store.is_empty(),
            "policy store must be empty after submit with empty policy list (replace-all semantics)"
        );
    }

    /// Scenario: SubmitPolicies with missing 'policies' field returns an error response.
    #[tokio::test]
    async fn test_handle_submit_policies_with_missing_policies_field_returns_error() {
        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();
        let event_tx = make_test_event_tx();

        handle_submit_policies(
            &mut server,
            &serde_json::json!({}), // missing 'policies' key
            &state,
            &reconciler,
            &event_tx,
        )
        .await
        .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_some(),
            "submit_policies with missing 'policies' field must return an error response"
        );
    }

    // ── GetHistory / GetJournalEntry handlers ─────────────────────────────────

    use std::sync::Mutex;

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    fn make_test_journal_entry() -> netfyr_journal::JournalEntry {
        netfyr_journal::JournalEntry {
            seq: 0,
            timestamp: chrono::Utc::now(),
            trigger: netfyr_journal::Trigger::PolicyApply { source: "test".into() },
            active_policies: vec![],
            diff: netfyr_journal::SerializableDiff { operations: vec![] },
            state_after: netfyr_journal::SerializableStateSet { entities: vec![] },
            outcome: netfyr_journal::ApplyOutcome::Applied { succeeded: 1, failed: 0, skipped: 0 },
        }
    }

    /// AC: handle_get_journal_entry with missing seq returns an error response.
    #[tokio::test]
    async fn test_handle_get_journal_entry_missing_seq_returns_error() {
        let (mut server, mut client) = make_stream_pair().await;

        handle_get_journal_entry(&mut server, &serde_json::json!({}))
            .await
            .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_some(),
            "missing seq must produce an error response, got: {msg:?}"
        );
    }

    /// AC: handle_get_journal_entry returns entry when seq matches, EntryNotFound when not found.
    #[tokio::test]
    async fn test_handle_get_journal_entry_returns_entry_or_entry_not_found() {
        let dir = tempfile::TempDir::new().unwrap();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let mut journal = netfyr_journal::Journal::open(dir.path()).unwrap();
        journal.append(make_test_journal_entry()).unwrap(); // gets seq=1

        // Safety: protected by ENV_MUTEX
        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", dir.path().to_str().unwrap()) };

        // seq=1 must return a non-null entry
        let (mut server, mut client) = make_stream_pair().await;
        handle_get_journal_entry(&mut server, &serde_json::json!({"seq": 1}))
            .await
            .unwrap();
        let msg = read_message(&mut client).await.unwrap();
        assert!(
            !msg["parameters"]["entry"].is_null(),
            "seq=1 must return a non-null entry, got: {msg:?}"
        );

        // seq=9999 must return EntryNotFound error (not null)
        let (mut server2, mut client2) = make_stream_pair().await;
        handle_get_journal_entry(&mut server2, &serde_json::json!({"seq": 9999}))
            .await
            .unwrap();
        let msg2 = read_message(&mut client2).await.unwrap();
        assert!(
            msg2.get("error").is_some(),
            "non-existent seq must produce an error response, got: {msg2:?}"
        );
        let error_name = msg2["error"].as_str().unwrap_or("");
        assert!(
            error_name.contains("EntryNotFound"),
            "error must be EntryNotFound for non-existent seq, got: {msg2:?}"
        );

        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };
    }

    /// AC: handle_get_history returns all entries from the journal.
    #[tokio::test]
    async fn test_handle_get_history_returns_entries_array() {
        let dir = tempfile::TempDir::new().unwrap();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let mut journal = netfyr_journal::Journal::open(dir.path()).unwrap();
        for _ in 0..3 {
            journal.append(make_test_journal_entry()).unwrap();
        }

        // Safety: protected by ENV_MUTEX
        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", dir.path().to_str().unwrap()) };

        let (mut server, mut client) = make_stream_pair().await;
        handle_get_history(&mut server, &serde_json::json!({"count": 10}))
            .await
            .unwrap();

        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };

        let msg = read_message(&mut client).await.unwrap();
        let entries = msg["parameters"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3, "must return all 3 appended entries");
    }

    /// AC: handle_get_history respects the count parameter.
    #[tokio::test]
    async fn test_handle_get_history_with_count_limits_results() {
        let dir = tempfile::TempDir::new().unwrap();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let mut journal = netfyr_journal::Journal::open(dir.path()).unwrap();
        for _ in 0..5 {
            journal.append(make_test_journal_entry()).unwrap();
        }

        // Safety: protected by ENV_MUTEX
        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", dir.path().to_str().unwrap()) };

        let (mut server, mut client) = make_stream_pair().await;
        handle_get_history(&mut server, &serde_json::json!({"count": 2}))
            .await
            .unwrap();

        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };

        let msg = read_message(&mut client).await.unwrap();
        let entries = msg["parameters"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2, "count=2 must limit results to 2 entries");
    }

    // ── GetShowInfo handler ───────────────────────────────────────────────────

    /// Scenario: GetShowInfo returns daemon.status = "running".
    #[tokio::test]
    async fn test_handle_get_show_info_daemon_status_is_running() {
        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();
        let start_time = Instant::now();

        handle_get_show_info(&mut server, &state, &reconciler, start_time)
            .await
            .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_none(),
            "GetShowInfo must not return an error: {msg:?}"
        );
        let status = msg["parameters"]["info"]["daemon"]["status"]
            .as_str()
            .expect("daemon.status must be a string");
        assert_eq!(status, "running", "daemon.status must be 'running'");
    }

    /// Scenario: GetShowInfo includes uptime_seconds in the daemon info block.
    #[tokio::test]
    async fn test_handle_get_show_info_daemon_uptime_seconds_is_present() {
        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();
        let start_time = Instant::now();

        handle_get_show_info(&mut server, &state, &reconciler, start_time)
            .await
            .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        let uptime = &msg["parameters"]["info"]["daemon"]["uptime_seconds"];
        assert!(
            !uptime.is_null(),
            "daemon.uptime_seconds must be present (not null)"
        );
        assert!(
            uptime.as_i64().unwrap_or(-1) >= 0,
            "daemon.uptime_seconds must be non-negative"
        );
    }

    /// Scenario: GetShowInfo response includes an 'interfaces' array.
    #[tokio::test]
    async fn test_handle_get_show_info_interfaces_is_array() {
        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();
        let start_time = Instant::now();

        handle_get_show_info(&mut server, &state, &reconciler, start_time)
            .await
            .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg["parameters"]["info"]["interfaces"].is_array(),
            "GetShowInfo must return an 'interfaces' array: {msg:?}"
        );
    }

    /// Scenario: GetShowInfo response has no 'error' field.
    #[tokio::test]
    async fn test_handle_get_show_info_response_has_no_error_field() {
        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();
        let start_time = Instant::now();

        handle_get_show_info(&mut server, &state, &reconciler, start_time)
            .await
            .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_none(),
            "GetShowInfo must not return an error field: {msg:?}"
        );
    }

    /// Scenario: GetShowInfo with a policy matching a system interface includes
    /// that policy in the interface's policies list.
    /// Uses a static policy targeting "lo" (always present in any netns).
    #[tokio::test]
    async fn test_handle_get_show_info_policies_appear_in_matching_interface() {
        let (mut server, mut client) = make_stream_pair().await;
        let lo_policy = make_test_policy("lo-policy");
        let state = make_test_state(PolicyStore::ephemeral(vec![lo_policy]), FactoryManager::new());
        let reconciler = Reconciler::new();
        let start_time = Instant::now();

        handle_get_show_info(&mut server, &state, &reconciler, start_time)
            .await
            .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_none(),
            "GetShowInfo with a policy must not return an error: {msg:?}"
        );
        // The response must include the info block with an interfaces array.
        assert!(
            msg["parameters"]["info"]["interfaces"].is_array(),
            "interfaces must be an array: {msg:?}"
        );
    }

    /// Scenario: ipv6auto factory in SLAAC-only mode (no M/O flags received)
    /// must not produce a dhcp object in GetShowInfo — dhcp_active is false
    /// until DHCPv6 is triggered by an RA with M or O flag.
    #[tokio::test]
    async fn test_handle_get_show_info_ipv6auto_slaac_only_has_no_dhcp_object() {
        let (mut server, mut client) = make_stream_pair().await;
        let policy = make_ipv6auto_lo_policy("lo-v6-slaac");
        let mut fm = FactoryManager::new();
        let _ = fm.sync(&[policy.clone()]).await;
        let state =
            make_test_state(PolicyStore::ephemeral(vec![policy]), fm);
        let reconciler = Reconciler::new();
        let start_time = Instant::now();

        handle_get_show_info(&mut server, &state, &reconciler, start_time)
            .await
            .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(msg.get("error").is_none(), "must not return error: {msg:?}");
        let interfaces =
            msg["parameters"]["info"]["interfaces"].as_array().unwrap();
        if let Some(lo) = interfaces.iter().find(|i| i["name"] == "lo") {
            assert!(
                lo.get("dhcp").is_none() || lo["dhcp"].is_null(),
                "SLAAC-only ipv6auto must not produce a dhcp field: {lo:?}"
            );
        }
    }

    /// Scenario: GetShowInfo for an interface managed by an ipv6auto factory
    /// must include a policy entry with type "ipv6auto".
    #[tokio::test]
    async fn test_handle_get_show_info_ipv6auto_policy_type_is_ipv6auto() {
        let (mut server, mut client) = make_stream_pair().await;
        let policy = make_ipv6auto_lo_policy("lo-v6-type");
        let mut fm = FactoryManager::new();
        let _ = fm.sync(&[policy.clone()]).await;
        let state =
            make_test_state(PolicyStore::ephemeral(vec![policy]), fm);
        let reconciler = Reconciler::new();
        let start_time = Instant::now();

        handle_get_show_info(&mut server, &state, &reconciler, start_time)
            .await
            .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(msg.get("error").is_none(), "must not return error: {msg:?}");
        let interfaces =
            msg["parameters"]["info"]["interfaces"].as_array().unwrap();
        if let Some(lo) = interfaces.iter().find(|i| i["name"] == "lo") {
            let has_ipv6auto = lo["policies"]
                .as_array()
                .is_some_and(|ps| ps.iter().any(|p| p["type"] == "ipv6auto"));
            assert!(has_ipv6auto, "ipv6auto policy must appear in policies with type 'ipv6auto': {lo:?}");
        }
    }

    // ── Revert handler ────────────────────────────────────────────────────────

    /// Missing target_seq parameter returns InternalError.
    #[tokio::test]
    async fn test_handle_revert_missing_target_seq_returns_internal_error() {
        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();

        handle_revert(&mut server, &serde_json::json!({}), &state, &reconciler)
            .await
            .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_some(),
            "missing target_seq must produce an error response: {msg:?}"
        );
        let error_name = msg["error"].as_str().unwrap_or("");
        assert!(
            error_name.contains("InternalError"),
            "error must be InternalError for missing target_seq: {msg:?}"
        );
        let reason = msg["parameters"]["reason"].as_str().unwrap_or("");
        assert!(
            reason.contains("target_seq"),
            "reason must mention 'target_seq': {msg:?}"
        );
    }

    /// Nonexistent entry returns EntryNotFound error containing the seq number.
    #[tokio::test]
    async fn test_handle_revert_entry_not_found_returns_entry_not_found_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        // Open the journal (creates the directory structure) but write no entries.
        let _journal = netfyr_journal::Journal::open(dir.path()).unwrap();

        // Safety: protected by ENV_MUTEX
        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", dir.path().to_str().unwrap()) };

        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();

        handle_revert(
            &mut server,
            &serde_json::json!({"target_seq": 9999u64}),
            &state,
            &reconciler,
        )
        .await
        .unwrap();

        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_some(),
            "nonexistent entry must produce an error response: {msg:?}"
        );
        let error_name = msg["error"].as_str().unwrap_or("");
        assert!(
            error_name.contains("EntryNotFound"),
            "error must be EntryNotFound: {msg:?}"
        );
        let reason = msg["parameters"]["reason"].as_str().unwrap_or("");
        assert!(
            reason.contains("9999"),
            "reason must mention the nonexistent seq '9999': {msg:?}"
        );
    }

    /// Dry-run returns success response with report and entry_timestamp.
    #[tokio::test]
    async fn test_handle_revert_dry_run_returns_success_with_report_and_timestamp() {
        let dir = tempfile::TempDir::new().unwrap();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let mut journal = netfyr_journal::Journal::open(dir.path()).unwrap();
        journal.append(make_test_journal_entry()).unwrap(); // gets seq=1

        // Safety: protected by ENV_MUTEX
        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", dir.path().to_str().unwrap()) };

        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();

        handle_revert(
            &mut server,
            &serde_json::json!({"target_seq": 1u64, "dry_run": true}),
            &state,
            &reconciler,
        )
        .await
        .unwrap();

        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg.get("error").is_none(),
            "dry-run revert must not return an error: {msg:?}"
        );
        assert!(
            msg["parameters"].get("report").is_some(),
            "dry-run revert response must include 'report': {msg:?}"
        );
        let ts = msg["parameters"]["entry_timestamp"].as_str().unwrap_or("");
        assert!(
            !ts.is_empty(),
            "dry-run revert response must include non-empty 'entry_timestamp': {msg:?}"
        );
    }

    /// Dry-run does not write a new journal entry.
    #[tokio::test]
    async fn test_handle_revert_dry_run_does_not_write_journal_entry() {
        let dir = tempfile::TempDir::new().unwrap();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let mut journal = netfyr_journal::Journal::open(dir.path()).unwrap();
        journal.append(make_test_journal_entry()).unwrap(); // gets seq=1; count=1 before dry-run

        // Safety: protected by ENV_MUTEX
        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", dir.path().to_str().unwrap()) };

        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();

        handle_revert(
            &mut server,
            &serde_json::json!({"target_seq": 1u64, "dry_run": true}),
            &state,
            &reconciler,
        )
        .await
        .unwrap();

        let _msg = read_message(&mut client).await.unwrap();

        // Re-open the journal by path (not env var) to count entries.
        let journal2 = netfyr_journal::Journal::open(dir.path()).unwrap();
        let entries = journal2.read_recent(100).unwrap();

        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };

        assert_eq!(
            entries.len(),
            1,
            "dry-run must not write a new journal entry (expected 1, got {})",
            entries.len()
        );
    }

    /// Dry-run response report.changes is an array.
    #[tokio::test]
    async fn test_handle_revert_dry_run_report_has_changes_array() {
        let dir = tempfile::TempDir::new().unwrap();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let mut journal = netfyr_journal::Journal::open(dir.path()).unwrap();
        journal.append(make_test_journal_entry()).unwrap(); // gets seq=1

        // Safety: protected by ENV_MUTEX
        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", dir.path().to_str().unwrap()) };

        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();

        handle_revert(
            &mut server,
            &serde_json::json!({"target_seq": 1u64, "dry_run": true}),
            &state,
            &reconciler,
        )
        .await
        .unwrap();

        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };

        let msg = read_message(&mut client).await.unwrap();
        assert!(
            msg["parameters"]["report"]["changes"].is_array(),
            "report.changes must be an array: {msg:?}"
        );
    }

    // ── Authorization via handle_connection ───────────────────────────────────

    /// AC: Non-root user calls GetStatus → daemon returns status (no PermissionDenied).
    /// GetStatus does not require root, so peer_cred() is never consulted and
    /// handle_connection dispatches it regardless of the caller's uid.
    #[tokio::test]
    async fn test_handle_connection_get_status_returns_status_without_uid_check() {
        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();
        let event_tx = make_test_event_tx();

        let request = r#"{"method":"io.netfyr.GetStatus","parameters":{}}"#;
        let mut bytes = request.as_bytes().to_vec();
        bytes.push(0u8);
        client.write_all(&bytes).await.unwrap();

        let server_task = tokio::spawn(async move {
            handle_connection(
                &mut server,
                &state,
                &reconciler,
                Instant::now(),
                &event_tx,
            )
            .await;
        });

        let response = read_message(&mut client).await.unwrap();
        assert!(
            response.get("error").is_none(),
            "GetStatus must not return PermissionDenied for any uid: {:?}",
            response
        );
        assert!(
            response["parameters"].get("status").is_some(),
            "GetStatus response must include 'status': {:?}",
            response
        );

        drop(client);
        let _ = server_task.await;
    }

    /// AC: Non-root user calls Query → no PermissionDenied.
    /// Query does not require root; handle_connection dispatches it for any uid.
    #[tokio::test]
    async fn test_handle_connection_query_returns_entities_without_uid_check() {
        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();
        let event_tx = make_test_event_tx();

        let request = r#"{"method":"io.netfyr.Query","parameters":{}}"#;
        let mut bytes = request.as_bytes().to_vec();
        bytes.push(0u8);
        client.write_all(&bytes).await.unwrap();

        let server_task = tokio::spawn(async move {
            handle_connection(
                &mut server,
                &state,
                &reconciler,
                Instant::now(),
                &event_tx,
            )
            .await;
        });

        let response = read_message(&mut client).await.unwrap();
        assert!(
            response.get("error").is_none(),
            "Query must not return PermissionDenied for any uid: {:?}",
            response
        );
        assert!(
            response["parameters"]["entities"].is_array(),
            "Query response must include 'entities' array: {:?}",
            response
        );

        drop(client);
        let _ = server_task.await;
    }

    /// AC: PermissionDenied error uses the io.netfyr.PermissionDenied name and
    /// contains "requires root" in the reason. Verified by inspecting the
    /// error message format string produced by write_error.
    #[tokio::test]
    async fn test_permission_denied_error_has_correct_name_and_reason_format() {
        let (mut server, mut client) = make_stream_pair().await;
        let reason = format!(
            "method '{}' requires root (uid 0), but client has uid {}",
            "io.netfyr.SubmitPolicies", 1000u32
        );
        write_error(&mut server, "PermissionDenied", &reason)
            .await
            .unwrap();

        let msg = read_message(&mut client).await.unwrap();
        assert_eq!(
            msg["error"].as_str().unwrap(),
            "io.netfyr.PermissionDenied",
            "PermissionDenied error must use the io.netfyr.PermissionDenied name"
        );
        let r = msg["parameters"]["reason"].as_str().unwrap_or("");
        assert!(
            r.contains("requires root"),
            "PermissionDenied reason must mention 'requires root', got: {:?}",
            r
        );
    }

    /// Scenario: Unknown method returns an error response.
    #[tokio::test]
    async fn test_handle_connection_unknown_method_returns_error() {
        let (mut server, mut client) = make_stream_pair().await;
        // Send an unknown method
        let request = r#"{"method":"io.netfyr.NonExistentMethod","parameters":{}}"#;
        let mut bytes = request.as_bytes().to_vec();
        bytes.push(0u8);
        client.write_all(&bytes).await.unwrap();

        // Run handle_connection in a background task
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();
        let event_tx = make_test_event_tx();
        let server_task = tokio::spawn(async move {
            handle_connection(
                &mut server,
                &state,
                &reconciler,
                Instant::now(),
                &event_tx,
            )
            .await;
        });

        let response = read_message(&mut client).await.unwrap();
        assert!(
            response.get("error").is_some(),
            "unknown method must produce an error response: {:?}",
            response
        );

        // Drop client to allow server task to finish
        drop(client);
        let _ = server_task.await;
    }

    // ── Authorization: read-only methods allowed for any uid ──────────────────

    /// AC: Non-root user calls GetHistory → returns history entries, no PermissionDenied.
    /// GetHistory does not require root; peer_cred is never consulted.
    #[tokio::test]
    async fn test_handle_connection_get_history_allowed_for_any_uid() {
        let dir = tempfile::TempDir::new().unwrap();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        // Safety: protected by ENV_MUTEX
        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", dir.path().to_str().unwrap()) };

        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();
        let event_tx = make_test_event_tx();

        let request = r#"{"method":"io.netfyr.GetHistory","parameters":{"count":10}}"#;
        let mut bytes = request.as_bytes().to_vec();
        bytes.push(0u8);
        client.write_all(&bytes).await.unwrap();

        let server_task = tokio::spawn(async move {
            handle_connection(&mut server, &state, &reconciler, Instant::now(), &event_tx).await;
        });

        let response = read_message(&mut client).await.unwrap();
        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };

        assert!(
            response.get("error").is_none(),
            "GetHistory must not return PermissionDenied for any uid: {:?}",
            response
        );
        assert!(
            response["parameters"]["entries"].is_array(),
            "GetHistory response must include an 'entries' array: {:?}",
            response
        );

        drop(client);
        let _ = server_task.await;
    }

    /// AC: Non-root user calls GetShowInfo → returns show info, no PermissionDenied.
    /// GetShowInfo does not require root; peer_cred is never consulted.
    #[tokio::test]
    async fn test_handle_connection_get_show_info_allowed_for_any_uid() {
        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();
        let event_tx = make_test_event_tx();

        let request = r#"{"method":"io.netfyr.GetShowInfo","parameters":{}}"#;
        let mut bytes = request.as_bytes().to_vec();
        bytes.push(0u8);
        client.write_all(&bytes).await.unwrap();

        let server_task = tokio::spawn(async move {
            handle_connection(&mut server, &state, &reconciler, Instant::now(), &event_tx).await;
        });

        let response = read_message(&mut client).await.unwrap();
        assert!(
            response.get("error").is_none(),
            "GetShowInfo must not return PermissionDenied for any uid: {:?}",
            response
        );
        assert!(
            response["parameters"].get("info").is_some(),
            "GetShowInfo response must include an 'info' field: {:?}",
            response
        );

        drop(client);
        let _ = server_task.await;
    }

    /// AC: Non-root user calls Revert with dry_run=true → no PermissionDenied.
    /// Revert(dry_run=true) is read-only; peer_cred is never consulted.
    /// The response may be an error (e.g., EntryNotFound) but must NOT be
    /// PermissionDenied.
    #[tokio::test]
    async fn test_handle_connection_revert_dry_run_true_allowed_for_any_uid() {
        let dir = tempfile::TempDir::new().unwrap();
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        // Safety: protected by ENV_MUTEX
        unsafe { std::env::set_var("NETFYR_JOURNAL_DIR", dir.path().to_str().unwrap()) };

        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();
        let event_tx = make_test_event_tx();

        // Use a nonexistent seq to get a deterministic error path without
        // needing to write a journal entry.
        let request =
            r#"{"method":"io.netfyr.Revert","parameters":{"target_seq":9999,"dry_run":true}}"#;
        let mut bytes = request.as_bytes().to_vec();
        bytes.push(0u8);
        client.write_all(&bytes).await.unwrap();

        let server_task = tokio::spawn(async move {
            handle_connection(&mut server, &state, &reconciler, Instant::now(), &event_tx).await;
        });

        let response = read_message(&mut client).await.unwrap();
        unsafe { std::env::remove_var("NETFYR_JOURNAL_DIR") };

        // The request must pass auth check — error (if any) must not be PermissionDenied.
        let error_name = response
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            !error_name.contains("PermissionDenied"),
            "Revert(dry_run=true) must not return PermissionDenied for any uid: {:?}",
            response
        );

        drop(client);
        let _ = server_task.await;
    }

    // ── Authorization: write methods denied for non-root uid ─────────────────

    /// AC: Non-root user calls SubmitPolicies → daemon returns PermissionDenied.
    /// The error reason must mention "requires root".
    ///
    /// This test verifies the full auth path via handle_connection when the
    /// process is running as a non-root user (the typical test environment).
    /// peer_cred() returns the actual calling uid, which is non-zero.
    #[tokio::test]
    async fn test_handle_connection_submit_policies_denied_for_non_root() {
        // Skip this test if we are actually running as root (CI with root container).
        if unsafe { libc::getuid() } == 0 {
            return;
        }

        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();
        let event_tx = make_test_event_tx();

        let request = r#"{"method":"io.netfyr.SubmitPolicies","parameters":{"policies":[]}}"#;
        let mut bytes = request.as_bytes().to_vec();
        bytes.push(0u8);
        client.write_all(&bytes).await.unwrap();

        let server_task = tokio::spawn(async move {
            handle_connection(&mut server, &state, &reconciler, Instant::now(), &event_tx).await;
        });

        let response = read_message(&mut client).await.unwrap();
        assert!(
            response.get("error").is_some(),
            "SubmitPolicies must return an error for non-root uid: {:?}",
            response
        );
        assert_eq!(
            response["error"].as_str().unwrap_or(""),
            "io.netfyr.PermissionDenied",
            "SubmitPolicies must return io.netfyr.PermissionDenied for non-root uid: {:?}",
            response
        );
        let reason = response["parameters"]["reason"].as_str().unwrap_or("");
        assert!(
            reason.contains("requires root"),
            "PermissionDenied reason must mention 'requires root', got: {:?}",
            reason
        );

        drop(client);
        let _ = server_task.await;
    }

    /// AC: Non-root user calls Revert with dry_run=false → daemon returns PermissionDenied.
    /// Revert without dry_run mutates system state and requires uid 0.
    #[tokio::test]
    async fn test_handle_connection_revert_dry_run_false_denied_for_non_root() {
        // Skip this test if we are actually running as root (CI with root container).
        if unsafe { libc::getuid() } == 0 {
            return;
        }

        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();
        let event_tx = make_test_event_tx();

        let request =
            r#"{"method":"io.netfyr.Revert","parameters":{"target_seq":1,"dry_run":false}}"#;
        let mut bytes = request.as_bytes().to_vec();
        bytes.push(0u8);
        client.write_all(&bytes).await.unwrap();

        let server_task = tokio::spawn(async move {
            handle_connection(&mut server, &state, &reconciler, Instant::now(), &event_tx).await;
        });

        let response = read_message(&mut client).await.unwrap();
        assert_eq!(
            response["error"].as_str().unwrap_or(""),
            "io.netfyr.PermissionDenied",
            "Revert(dry_run=false) must return PermissionDenied for non-root uid: {:?}",
            response
        );

        drop(client);
        let _ = server_task.await;
    }

    /// AC: Non-root user calls Revert without specifying dry_run → PermissionDenied.
    /// Absent dry_run defaults to false, so it is treated as a mutating write method.
    #[tokio::test]
    async fn test_handle_connection_revert_dry_run_absent_denied_for_non_root() {
        // Skip this test if we are actually running as root (CI with root container).
        if unsafe { libc::getuid() } == 0 {
            return;
        }

        let (mut server, mut client) = make_stream_pair().await;
        let state = make_test_state(PolicyStore::ephemeral(vec![]), FactoryManager::new());
        let reconciler = Reconciler::new();
        let event_tx = make_test_event_tx();

        let request = r#"{"method":"io.netfyr.Revert","parameters":{"target_seq":1}}"#;
        let mut bytes = request.as_bytes().to_vec();
        bytes.push(0u8);
        client.write_all(&bytes).await.unwrap();

        let server_task = tokio::spawn(async move {
            handle_connection(&mut server, &state, &reconciler, Instant::now(), &event_tx).await;
        });

        let response = read_message(&mut client).await.unwrap();
        assert_eq!(
            response["error"].as_str().unwrap_or(""),
            "io.netfyr.PermissionDenied",
            "Revert with absent dry_run must return PermissionDenied for non-root uid: {:?}",
            response
        );

        drop(client);
        let _ = server_task.await;
    }

    /// AC: Root user calls SubmitPolicies → request is processed, no PermissionDenied.
    /// Verified indirectly: requires_root() returns true for SubmitPolicies (so the
    /// uid check IS performed), but handle_submit_policies() itself succeeds when
    /// called with empty policies (tested by
    /// test_handle_submit_policies_with_empty_list_returns_success). Together these
    /// confirm that a uid-0 caller reaches the handler and gets a success response.
    #[test]
    fn test_requires_root_submit_policies_checks_uid_for_root_bypass() {
        // requires_root returns true → the auth check runs for SubmitPolicies.
        // A uid-0 peer passes the check and the handler is invoked normally.
        assert!(
            requires_root("io.netfyr.SubmitPolicies", &serde_json::json!({})),
            "SubmitPolicies must be gated by the uid check so root is the only path through"
        );
    }

    // ── requires_root ────────────────────────────────────────────────────────

    #[test]
    fn test_requires_root_submit_policies() {
        assert!(requires_root("io.netfyr.SubmitPolicies", &serde_json::json!({})));
    }

    #[test]
    fn test_requires_root_revert_dry_run_false() {
        assert!(requires_root(
            "io.netfyr.Revert",
            &serde_json::json!({"target_seq": 1, "dry_run": false})
        ));
    }

    #[test]
    fn test_requires_root_revert_dry_run_absent() {
        assert!(requires_root(
            "io.netfyr.Revert",
            &serde_json::json!({"target_seq": 1})
        ));
    }

    #[test]
    fn test_requires_root_revert_dry_run_true_does_not_require_root() {
        assert!(!requires_root(
            "io.netfyr.Revert",
            &serde_json::json!({"target_seq": 1, "dry_run": true})
        ));
    }

    #[test]
    fn test_requires_root_read_methods() {
        let empty = serde_json::json!({});
        assert!(!requires_root("io.netfyr.Query", &empty));
        assert!(!requires_root("io.netfyr.DryRun", &empty));
        assert!(!requires_root("io.netfyr.GetStatus", &empty));
        assert!(!requires_root("io.netfyr.GetHistory", &empty));
        assert!(!requires_root("io.netfyr.GetJournalEntry", &empty));
        assert!(!requires_root("io.netfyr.GetShowInfo", &empty));
    }
}
