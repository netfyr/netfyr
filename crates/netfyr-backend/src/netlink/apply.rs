//! Apply and dry-run logic for ethernet interfaces via rtnetlink.
//!
//! Translates `StateDiff` operations into netlink requests that modify running
//! kernel networking state. Each operation is executed independently; errors are
//! captured in the report rather than propagated (continue-and-report mode).

use std::net::IpAddr;

use futures::{StreamExt, TryStreamExt};
use indexmap::IndexMap;
use netfyr_state::{entity_types::ETHERNET, DiffOp, FieldValue, Selector, State, StateDiff, Value};
use netlink_packet_route::address::{AddressAttribute, AddressMessage, CacheInfo};
use netlink_packet_route::route::{RouteAddress, RouteAttribute, RouteMessage};
use netlink_packet_route::RouteNetlinkMessage;
use rtnetlink::packet_core::{
    NetlinkMessage, NetlinkPayload, NLM_F_ACK, NLM_F_APPEND, NLM_F_CREATE, NLM_F_REQUEST,
};
use rtnetlink::{Handle, IpVersion, LinkUnspec, RouteMessageBuilder};
use tracing::warn;

use crate::report::{
    AppliedOperation, ApplyReport, DiffOpKind, DryRunReport, FailedOperation, FieldChange,
    FieldChangeKind, PlannedChange, SkippedOperation,
};
use crate::BackendError;

use super::interface::query_interfaces;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Default route metric applied when the desired state does not specify one.
const DEFAULT_ROUTE_METRIC: u32 = 100;

/// Fields this backend can write. Any field not in this set is read-only and
/// will be skipped with reason "read-only field" (defensive check — SPEC-203
/// should exclude read-only fields from the diff before they reach here).
const WRITABLE_FIELDS: &[&str] = &["mtu", "enabled", "addresses", "routes"];

// ── Public entry points ───────────────────────────────────────────────────────

/// Apply all ethernet operations in `diff` to the running kernel state.
///
/// Never returns `Err` — all per-operation errors are captured in the report.
pub async fn apply_ethernet(
    handle: &Handle,
    diff: &StateDiff,
) -> Result<ApplyReport, BackendError> {
    let mut report = ApplyReport::new();

    for op in diff.ops() {
        if op.entity_type() != ETHERNET {
            continue;
        }
        let (applied, failed, skipped) = apply_one_op(handle, op).await;
        report.succeeded.extend(applied);
        report.failed.extend(failed);
        report.skipped.extend(skipped);
    }

    Ok(report)
}

/// Simulate applying `diff` without modifying kernel state.
///
/// Queries current interface state to build before/after `PlannedChange` entries.
/// Operations on non-existent interfaces are added to `report.skipped`.
pub async fn dry_run_ethernet(
    handle: &Handle,
    diff: &StateDiff,
) -> Result<DryRunReport, BackendError> {
    let mut report = DryRunReport::new();

    for op in diff.ops() {
        if op.entity_type() != ETHERNET {
            continue;
        }

        let selector = op.selector();
        let name = match selector.name.as_deref() {
            Some(n) => n,
            None => {
                report.skipped.push(SkippedOperation {
                    operation: DiffOpKind::from(op),
                    entity_type: op.entity_type().to_string(),
                    selector: selector.clone(),
                    reason: "selector has no name".to_string(),
                });
                continue;
            }
        };

        let current_state = match get_current_state(handle, name).await {
            Ok(s) => s,
            Err(BackendError::NotFound { .. }) => {
                report.skipped.push(SkippedOperation {
                    operation: DiffOpKind::from(op),
                    entity_type: op.entity_type().to_string(),
                    selector: selector.clone(),
                    reason: format!("interface not found: {name}"),
                });
                continue;
            }
            Err(e) => return Err(e),
        };

        let field_changes = build_planned_changes(op, &current_state);
        report.changes.push(PlannedChange {
            operation: DiffOpKind::from(op),
            entity_type: op.entity_type().to_string(),
            selector: selector.clone(),
            field_changes,
        });
    }

    Ok(report)
}

// ── Operation dispatch ────────────────────────────────────────────────────────

async fn apply_one_op(
    handle: &Handle,
    op: &DiffOp,
) -> (
    Vec<AppliedOperation>,
    Vec<FailedOperation>,
    Vec<SkippedOperation>,
) {
    match op {
        DiffOp::Add {
            entity_type,
            selector,
            fields,
        } => apply_add(handle, entity_type, selector, fields).await,
        DiffOp::Modify {
            entity_type,
            selector,
            changed_fields,
            removed_fields,
        } => apply_modify(handle, entity_type, selector, changed_fields, removed_fields).await,
        DiffOp::Remove {
            entity_type,
            selector,
        } => apply_remove(handle, entity_type, selector).await,
    }
}

// ── Per-kind apply functions ──────────────────────────────────────────────────

async fn apply_add(
    handle: &Handle,
    entity_type: &str,
    selector: &Selector,
    fields: &IndexMap<String, FieldValue>,
) -> (
    Vec<AppliedOperation>,
    Vec<FailedOperation>,
    Vec<SkippedOperation>,
) {
    let name = match selector.name.as_deref() {
        Some(n) => n,
        None => {
            return fail_op(
                DiffOpKind::Add,
                entity_type,
                selector,
                BackendError::Internal("selector has no name".to_string()),
                fields.keys().cloned().collect(),
            );
        }
    };

    // Physical ethernet interfaces cannot be created — they must already exist.
    let index = match resolve_link_index(handle, name).await {
        Ok(idx) => idx,
        Err(e) => {
            return fail_op(
                DiffOpKind::Add,
                entity_type,
                selector,
                e,
                fields.keys().cloned().collect(),
            );
        }
    };

    let current_state = match get_current_state(handle, name).await {
        Ok(s) => s,
        Err(e) => {
            return fail_op(
                DiffOpKind::Add,
                entity_type,
                selector,
                e,
                fields.keys().cloned().collect(),
            );
        }
    };

    let (fields_changed, mut failures, skipped) =
        apply_modify_fields(handle, index, name, &current_state, fields, &[]).await;

    if failures.is_empty() {
        (
            vec![AppliedOperation {
                operation: DiffOpKind::Add,
                entity_type: entity_type.to_string(),
                selector: selector.clone(),
                fields_changed,
            }],
            failures,
            skipped,
        )
    } else {
        let first = failures.remove(0);
        (
            vec![],
            vec![FailedOperation {
                operation: DiffOpKind::Add,
                entity_type: entity_type.to_string(),
                selector: selector.clone(),
                error: first.error,
                fields: fields.keys().cloned().collect(),
            }],
            skipped,
        )
    }
}

async fn apply_modify(
    handle: &Handle,
    entity_type: &str,
    selector: &Selector,
    changed_fields: &IndexMap<String, FieldValue>,
    removed_fields: &[String],
) -> (
    Vec<AppliedOperation>,
    Vec<FailedOperation>,
    Vec<SkippedOperation>,
) {
    let name = match selector.name.as_deref() {
        Some(n) => n,
        None => {
            return fail_op(
                DiffOpKind::Modify,
                entity_type,
                selector,
                BackendError::Internal("selector has no name".to_string()),
                changed_fields.keys().cloned().collect(),
            );
        }
    };

    let index = match resolve_link_index(handle, name).await {
        Ok(idx) => idx,
        Err(e) => {
            return fail_op(
                DiffOpKind::Modify,
                entity_type,
                selector,
                e,
                changed_fields.keys().cloned().collect(),
            );
        }
    };

    let current_state = match get_current_state(handle, name).await {
        Ok(s) => s,
        Err(BackendError::NotFound { .. }) => {
            // Interface exists (resolve_link_index succeeded) but is not an
            // ethernet type (e.g. loopback). Use an empty state so kernel
            // operations are still attempted — the kernel will return the
            // appropriate error (e.g. EPERM for non-root).
            State {
                entity_type: entity_type.to_string(),
                selector: selector.clone(),
                fields: IndexMap::new(),
                metadata: netfyr_state::StateMetadata::default(),
                policy_ref: None,
                priority: 0,
            }
        }
        Err(e) => {
            return fail_op(
                DiffOpKind::Modify,
                entity_type,
                selector,
                e,
                changed_fields.keys().cloned().collect(),
            );
        }
    };

    let (fields_changed, mut failures, skipped) =
        apply_modify_fields(handle, index, name, &current_state, changed_fields, removed_fields)
            .await;

    if failures.is_empty() {
        (
            vec![AppliedOperation {
                operation: DiffOpKind::Modify,
                entity_type: entity_type.to_string(),
                selector: selector.clone(),
                fields_changed,
            }],
            failures,
            skipped,
        )
    } else {
        let first = failures.remove(0);
        (
            vec![],
            vec![FailedOperation {
                operation: DiffOpKind::Modify,
                entity_type: entity_type.to_string(),
                selector: selector.clone(),
                error: first.error,
                fields: changed_fields.keys().cloned().collect(),
            }],
            skipped,
        )
    }
}

async fn apply_remove(
    handle: &Handle,
    entity_type: &str,
    selector: &Selector,
) -> (
    Vec<AppliedOperation>,
    Vec<FailedOperation>,
    Vec<SkippedOperation>,
) {
    let name = match selector.name.as_deref() {
        Some(n) => n,
        None => {
            return fail_op(
                DiffOpKind::Remove,
                entity_type,
                selector,
                BackendError::Internal("selector has no name".to_string()),
                vec![],
            );
        }
    };

    let index = match resolve_link_index(handle, name).await {
        Ok(idx) => idx,
        Err(e) => return fail_op(DiffOpKind::Remove, entity_type, selector, e, vec![]),
    };

    let mut fields_changed: Vec<String> = vec![];

    // Remove all addresses first.
    match query_address_messages(handle, index).await {
        Ok(msgs) => {
            for msg in msgs {
                match handle.address().del(msg).execute().await {
                    Ok(()) => fields_changed.push("addresses".to_string()),
                    Err(ref e) if is_not_found_error(e) => {} // already gone
                    Err(e) => {
                        warn!("Failed to delete address on {name}: {e}");
                        return fail_op(
                            DiffOpKind::Remove,
                            entity_type,
                            selector,
                            map_netlink_error(e, &format!("del address on {name}")),
                            vec!["addresses".to_string()],
                        );
                    }
                }
            }
        }
        Err(e) => {
            return fail_op(
                DiffOpKind::Remove,
                entity_type,
                selector,
                e,
                vec!["addresses".to_string()],
            )
        }
    }

    // Remove all routes associated with the interface.
    match query_route_messages(handle, index).await {
        Ok(msgs) => {
            for msg in msgs {
                match handle.route().del(msg).execute().await {
                    Ok(()) => fields_changed.push("routes".to_string()),
                    Err(ref e) if is_not_found_error(e) => {} // already gone
                    Err(e) => {
                        warn!("Failed to delete route on {name}: {e}");
                        return fail_op(
                            DiffOpKind::Remove,
                            entity_type,
                            selector,
                            map_netlink_error(e, &format!("del route on {name}")),
                            vec!["routes".to_string()],
                        );
                    }
                }
            }
        }
        Err(e) => {
            return fail_op(
                DiffOpKind::Remove,
                entity_type,
                selector,
                e,
                vec!["routes".to_string()],
            )
        }
    }

    // Set link down. Physical interfaces are never deleted from the system.
    match handle
        .link()
        .change(LinkUnspec::new_with_index(index).down().build())
        .execute()
        .await
    {
        Ok(()) => fields_changed.push("enabled".to_string()),
        Err(e) => {
            return fail_op(
                DiffOpKind::Remove,
                entity_type,
                selector,
                map_netlink_error(e, &format!("set link down on {name}")),
                vec!["enabled".to_string()],
            );
        }
    }

    fields_changed.dedup();
    (
        vec![AppliedOperation {
            operation: DiffOpKind::Remove,
            entity_type: entity_type.to_string(),
            selector: selector.clone(),
            fields_changed,
        }],
        vec![],
        vec![],
    )
}

// ── Core field-application engine ─────────────────────────────────────────────

/// Apply field changes to an interface. Returns `(fields_changed, failures, skipped)`.
///
/// Field changes are applied in prescribed order:
/// 1. Link-level (mtu, enabled)
/// 2. Addresses (remove before add to preserve YAML ordering)
/// 3. Routes
///
/// Read-only fields produce `SkippedOperation` entries. Errors are collected and
/// do not abort remaining field changes within the same operation.
async fn apply_modify_fields(
    handle: &Handle,
    index: u32,
    name: &str,
    current_state: &State,
    changed_fields: &IndexMap<String, FieldValue>,
    removed_fields: &[String],
) -> (Vec<String>, Vec<FailedOperation>, Vec<SkippedOperation>) {
    let mut fields_changed: Vec<String> = vec![];
    let mut failures: Vec<FailedOperation> = vec![];
    let mut skipped: Vec<SkippedOperation> = vec![];

    // ── Phase 1: Link-level ───────────────────────────────────────────────────

    if let Some(fv) = changed_fields.get("mtu") {
        if let Some(desired) = fv.value.as_u64() {
            let current = current_state
                .fields
                .get("mtu")
                .and_then(|f| f.value.as_u64())
                .unwrap_or(0);
            if desired == current {
                skipped.push(SkippedOperation {
                    operation: DiffOpKind::Modify,
                    entity_type: ETHERNET.to_string(),
                    selector: Selector::with_name(name),
                    reason: format!("mtu already at desired value ({desired})"),
                });
            } else {
                match handle
                    .link()
                    .change(
                        LinkUnspec::new_with_index(index)
                            .mtu(desired as u32)
                            .build(),
                    )
                    .execute()
                    .await
                {
                    Ok(()) => fields_changed.push("mtu".to_string()),
                    Err(e) => failures.push(make_field_failure(
                        name,
                        map_netlink_error(e, &format!("set mtu on {name}")),
                        "mtu",
                    )),
                }
            }
        }
    }

    if let Some(fv) = changed_fields.get("enabled") {
        match fv.value.as_bool() {
            Some(true) => {
                match handle
                    .link()
                    .change(LinkUnspec::new_with_index(index).up().build())
                    .execute()
                    .await
                {
                    Ok(()) => fields_changed.push("enabled".to_string()),
                    Err(e) => failures.push(make_field_failure(
                        name,
                        map_netlink_error(e, &format!("set link up on {name}")),
                        "enabled",
                    )),
                }
            }
            Some(false) => {
                match handle
                    .link()
                    .change(LinkUnspec::new_with_index(index).down().build())
                    .execute()
                    .await
                {
                    Ok(()) => fields_changed.push("enabled".to_string()),
                    Err(e) => failures.push(make_field_failure(
                        name,
                        map_netlink_error(e, &format!("set link down on {name}")),
                        "enabled",
                    )),
                }
            }
            None => {
                skipped.push(SkippedOperation {
                    operation: DiffOpKind::Modify,
                    entity_type: ETHERNET.to_string(),
                    selector: Selector::with_name(name),
                    reason: "enabled field must be a boolean".to_string(),
                });
            }
        }
    }

    // ── Phase 2: Addresses ────────────────────────────────────────────────────

    let addr_in_changed = changed_fields.contains_key("addresses");
    let addr_in_removed = removed_fields.iter().any(|f| f == "addresses");

    if addr_in_changed || addr_in_removed {
        // Full desired address list; empty when field is being removed.
        // Keep original Value items so we can extract lifetimes when adding.
        let desired_items: Vec<&Value> = if let Some(fv) = changed_fields.get("addresses") {
            fv.value.as_list().map(|list| list.iter().collect()).unwrap_or_default()
        } else {
            vec![]
        };
        let desired_addrs: Vec<String> = desired_items.iter().filter_map(|v| addr_to_cidr(v)).collect();

        let current_addrs: Vec<String> = current_state
            .fields
            .get("addresses")
            .and_then(|fv| fv.value.as_list())
            .map(|list| {
                list.iter()
                    .filter_map(addr_to_cidr)
                    .collect()
            })
            .unwrap_or_default();

        let to_add: Vec<(usize, &String)> = desired_addrs
            .iter()
            .enumerate()
            .filter(|(_, a)| !current_addrs.contains(a))
            .collect();
        let to_remove: Vec<&String> = current_addrs
            .iter()
            .filter(|a| !desired_addrs.contains(a))
            .filter(|a| !is_link_local(a))
            .collect();
        let to_replace: Vec<(usize, &String)> = desired_addrs
            .iter()
            .enumerate()
            .filter(|(idx, a)| {
                current_addrs.contains(a)
                    && desired_items.get(*idx).and_then(|v| addr_valid_lft(v)).is_some()
            })
            .collect();

        // Remove unwanted addresses first, then add new ones in desired order.
        // This ensures the kernel's address list order matches YAML order, and
        // the first address in the policy becomes the primary (source) address.
        if !to_remove.is_empty() {
            match query_address_messages(handle, index).await {
                Ok(msgs) => {
                    for cidr in &to_remove {
                        match parse_cidr(cidr) {
                            Ok((ip, prefix)) => {
                                match find_address_message(&msgs, ip, prefix) {
                                    Some(msg) => {
                                        match handle
                                            .address()
                                            .del(msg.clone())
                                            .execute()
                                            .await
                                        {
                                            Ok(()) => {
                                                fields_changed.push("addresses".to_string())
                                            }
                                            Err(ref e) if is_not_found_error(e) => {
                                                skipped.push(SkippedOperation {
                                                    operation: DiffOpKind::Modify,
                                                    entity_type: ETHERNET.to_string(),
                                                    selector: Selector::with_name(name),
                                                    reason: format!(
                                                        "address {cidr} not present"
                                                    ),
                                                });
                                            }
                                            Err(e) => failures.push(make_field_failure(
                                                name,
                                                map_netlink_error(
                                                    e,
                                                    &format!("del address {cidr} on {name}"),
                                                ),
                                                "addresses",
                                            )),
                                        }
                                    }
                                    None => {
                                        skipped.push(SkippedOperation {
                                            operation: DiffOpKind::Modify,
                                            entity_type: ETHERNET.to_string(),
                                            selector: Selector::with_name(name),
                                            reason: format!("address {cidr} not present"),
                                        });
                                    }
                                }
                            }
                            Err(e) => failures.push(make_field_failure(name, e, "addresses")),
                        }
                    }
                }
                Err(e) => failures.push(make_field_failure(name, e, "addresses")),
            }
        }

        // Add new addresses in the order they appear in the desired state.
        for (idx, cidr) in &to_add {
            match parse_cidr(cidr) {
                Ok((ip, prefix)) => {
                    let mut req = handle.address().add(index, ip, prefix);
                    if let Some(orig_value) = desired_items.get(*idx) {
                        if let (Some(valid), Some(preferred)) = (addr_valid_lft(orig_value), addr_preferred_lft(orig_value)) {
                            let mut ci = CacheInfo::default();
                            ci.ifa_preferred = preferred;
                            ci.ifa_valid = valid;
                            req.message_mut().attributes.push(
                                AddressAttribute::CacheInfo(ci),
                            );
                        }
                    }
                    match req.execute().await {
                        Ok(()) => fields_changed.push("addresses".to_string()),
                        Err(ref e) if is_eexist(e) => {
                            skipped.push(SkippedOperation {
                                operation: DiffOpKind::Modify,
                                entity_type: ETHERNET.to_string(),
                                selector: Selector::with_name(name),
                                reason: format!("address {cidr} already present"),
                            });
                        }
                        Err(e) => failures.push(make_field_failure(
                            name,
                            map_netlink_error(e, &format!("add address {cidr} on {name}")),
                            "addresses",
                        )),
                    }
                }
                Err(e) => failures.push(make_field_failure(name, e, "addresses")),
            }
        }

        // Replace existing addresses that carry lifetime attributes.
        // Uses `ip addr replace` semantics to update CacheInfo in-place.
        for (idx, cidr) in &to_replace {
            match parse_cidr(cidr) {
                Ok((ip, prefix)) => {
                    let mut req = handle.address().add(index, ip, prefix).replace();
                    if let Some(orig_value) = desired_items.get(*idx) {
                        if let (Some(valid), Some(preferred)) = (addr_valid_lft(orig_value), addr_preferred_lft(orig_value)) {
                            let mut ci = CacheInfo::default();
                            ci.ifa_preferred = preferred;
                            ci.ifa_valid = valid;
                            req.message_mut().attributes.push(
                                AddressAttribute::CacheInfo(ci),
                            );
                        }
                    }
                    match req.execute().await {
                        Ok(()) => fields_changed.push("addresses".to_string()),
                        Err(e) => failures.push(make_field_failure(
                            name,
                            map_netlink_error(e, &format!("replace address {cidr} on {name}")),
                            "addresses",
                        )),
                    }
                }
                Err(e) => failures.push(make_field_failure(name, e, "addresses")),
            }
        }
    }

    // ── Phase 3: Routes ───────────────────────────────────────────────────────

    let route_in_changed = changed_fields.contains_key("routes");
    let route_in_removed = removed_fields.iter().any(|f| f == "routes");

    if route_in_changed || route_in_removed {
        // Full desired route list; empty when field is being removed.
        let desired_routes: Vec<Value> = if let Some(fv) = changed_fields.get("routes") {
            fv.value.as_list().cloned().unwrap_or_default()
        } else {
            vec![]
        };

        let current_routes: Vec<Value> = current_state
            .fields
            .get("routes")
            .and_then(|fv| fv.value.as_list())
            .cloned()
            .unwrap_or_default();

        let to_add: Vec<&Value> = desired_routes
            .iter()
            .filter(|r| !current_routes.contains(r))
            .collect();
        let to_remove: Vec<&Value> = current_routes
            .iter()
            .filter(|r| !desired_routes.contains(r))
            .filter(|r| !is_kernel_route(r))
            .collect();

        // Add new routes.
        for route_val in &to_add {
            if let Some(map) = route_val.as_map() {
                match extract_route_fields(map) {
                    Ok(rf) => {
                        let dst_ip = rf.dst_ip;
                        let dst_prefix = rf.dst_prefix;
                        match add_route(handle, index, &rf).await {
                            Ok(()) => fields_changed.push("routes".to_string()),
                            Err(ref e) if is_eexist_backend(e) => {
                                skipped.push(SkippedOperation {
                                    operation: DiffOpKind::Modify,
                                    entity_type: ETHERNET.to_string(),
                                    selector: Selector::with_name(name),
                                    reason: format!(
                                        "route {dst_ip}/{dst_prefix} already present"
                                    ),
                                });
                            }
                            Err(e) => failures.push(make_field_failure(name, e, "routes")),
                        }
                    }
                    Err(e) => failures.push(make_field_failure(name, e, "routes")),
                }
            }
        }

        // Remove unwanted routes.
        if !to_remove.is_empty() {
            match query_route_messages(handle, index).await {
                Ok(route_msgs) => {
                    for route_val in &to_remove {
                        if let Some(map) = route_val.as_map() {
                            match extract_route_fields(map) {
                                Ok(rf) => {
                                    match find_route_message(
                                        &route_msgs,
                                        rf.dst_ip,
                                        rf.dst_prefix,
                                        rf.gateway,
                                    ) {
                                        Some(msg) => {
                                            match handle
                                                .route()
                                                .del(msg.clone())
                                                .execute()
                                                .await
                                            {
                                                Ok(()) => {
                                                    fields_changed.push("routes".to_string())
                                                }
                                                Err(ref e) if is_not_found_error(e) => {
                                                    fields_changed.push("routes".to_string());
                                                }
                                                Err(e) => failures.push(make_field_failure(
                                                    name,
                                                    map_netlink_error(
                                                        e,
                                                        &format!("del route on {name}"),
                                                    ),
                                                    "routes",
                                                )),
                                            }
                                        }
                                        None => {
                                            fields_changed.push("routes".to_string());
                                        }
                                    }
                                }
                                Err(e) => failures.push(make_field_failure(name, e, "routes")),
                            }
                        }
                    }
                }
                Err(e) => failures.push(make_field_failure(name, e, "routes")),
            }
        }
    }

    // ── Phase 4: Read-only field defensive check ──────────────────────────────

    for field in changed_fields.keys() {
        if !WRITABLE_FIELDS.contains(&field.as_str()) {
            skipped.push(SkippedOperation {
                operation: DiffOpKind::Modify,
                entity_type: ETHERNET.to_string(),
                selector: Selector::with_name(name),
                reason: "read-only field".to_string(),
            });
        }
    }
    for field in removed_fields {
        if !WRITABLE_FIELDS.contains(&field.as_str()) {
            skipped.push(SkippedOperation {
                operation: DiffOpKind::Modify,
                entity_type: ETHERNET.to_string(),
                selector: Selector::with_name(name),
                reason: "read-only field".to_string(),
            });
        }
    }

    fields_changed.dedup();
    (fields_changed, failures, skipped)
}

// ── Netlink query helpers ─────────────────────────────────────────────────────

/// Resolve a link name to its kernel interface index.
async fn resolve_link_index(handle: &Handle, name: &str) -> Result<u32, BackendError> {
    let mut stream = handle
        .link()
        .get()
        .match_name(name.to_string())
        .execute();
    if let Some(msg) = stream
        .try_next()
        .await
        .map_err(|e| BackendError::QueryFailed {
            entity_type: ETHERNET.to_string(),
            source: Box::new(e),
        })?
    {
        return Ok(msg.header.index);
    }
    Err(BackendError::NotFound {
        entity_type: ETHERNET.to_string(),
        selector: Box::new(Selector::with_name(name)),
    })
}

/// Query the current `State` for a named interface (for delta computation).
async fn get_current_state(handle: &Handle, name: &str) -> Result<State, BackendError> {
    let sel = Selector::with_name(name);
    let state_set = query_interfaces(handle, None, Some(&sel)).await?;
    // Collect the first state before `state_set` is dropped.
    let first = state_set.iter().next().cloned();
    first.ok_or_else(|| BackendError::NotFound {
        entity_type: ETHERNET.to_string(),
        selector: Box::new(sel),
    })
}

/// Return all `AddressMessage` objects for the given interface index.
async fn query_address_messages(
    handle: &Handle,
    index: u32,
) -> Result<Vec<AddressMessage>, BackendError> {
    let mut msgs = Vec::new();
    let mut stream = handle.address().get().execute();
    while let Some(msg) = stream
        .try_next()
        .await
        .map_err(|e| BackendError::QueryFailed {
            entity_type: ETHERNET.to_string(),
            source: Box::new(e),
        })?
    {
        if msg.header.index == index {
            msgs.push(msg);
        }
    }
    Ok(msgs)
}

/// Return all `RouteMessage` objects whose output interface matches `index`.
async fn query_route_messages(
    handle: &Handle,
    index: u32,
) -> Result<Vec<RouteMessage>, BackendError> {
    let mut msgs = Vec::new();

    for ip_version in [IpVersion::V4, IpVersion::V6] {
        let mut route_msg = RouteMessage::default();
        route_msg.header.address_family = match ip_version {
            IpVersion::V4 => netlink_packet_route::AddressFamily::Inet,
            IpVersion::V6 => netlink_packet_route::AddressFamily::Inet6,
        };

        let mut stream = handle.route().get(route_msg).execute();
        while let Some(msg) = stream
            .try_next()
            .await
            .map_err(|e| BackendError::QueryFailed {
                entity_type: ETHERNET.to_string(),
                source: Box::new(e),
            })?
        {
            let oif = msg.attributes.iter().find_map(|attr| {
                if let RouteAttribute::Oif(idx) = attr {
                    Some(*idx)
                } else {
                    None
                }
            });
            if oif == Some(index) {
                msgs.push(msg);
            }
        }
    }

    Ok(msgs)
}

// ── Message matching helpers ──────────────────────────────────────────────────

/// Find an `AddressMessage` matching the given IP and prefix length.
///
/// String-based comparison handles both IPv4 and IPv6 inner types generically.
fn find_address_message(
    messages: &[AddressMessage],
    ip: IpAddr,
    prefix: u8,
) -> Option<&AddressMessage> {
    let ip_str = ip.to_string();
    messages.iter().find(|msg| {
        if msg.header.prefix_len != prefix {
            return false;
        }
        msg.attributes.iter().any(|attr| {
            if let AddressAttribute::Address(a) = attr {
                format!("{a}") == ip_str
            } else {
                false
            }
        })
    })
}

/// Find a `RouteMessage` matching destination prefix and optional gateway.
fn find_route_message(
    messages: &[RouteMessage],
    dst_ip: IpAddr,
    dst_prefix: u8,
    gateway: Option<IpAddr>,
) -> Option<&RouteMessage> {
    let dst_str = dst_ip.to_string();
    let gw_str = gateway.map(|g| g.to_string());

    messages.iter().find(|msg| {
        if msg.header.destination_prefix_length != dst_prefix {
            return false;
        }

        let msg_dst = msg.attributes.iter().find_map(|attr| {
            if let RouteAttribute::Destination(addr) = attr {
                route_address_to_string(addr)
            } else {
                None
            }
        });

        // No explicit Destination attribute → default route (0.0.0.0/0 or ::/0).
        let dst_matches = match msg_dst {
            Some(ref s) => s == &dst_str,
            None => match dst_ip {
                IpAddr::V4(v4) => v4.is_unspecified(),
                IpAddr::V6(v6) => v6.is_unspecified(),
            },
        };
        if !dst_matches {
            return false;
        }

        let msg_gw = msg.attributes.iter().find_map(|attr| {
            if let RouteAttribute::Gateway(addr) = attr {
                route_address_to_string(addr)
            } else {
                None
            }
        });

        match (gw_str.as_deref(), msg_gw.as_deref()) {
            (Some(gw), Some(mg)) => gw == mg,
            (None, None) => true,
            _ => false,
        }
    })
}

fn route_address_to_string(addr: &RouteAddress) -> Option<String> {
    match addr {
        RouteAddress::Inet(v4) => Some(v4.to_string()),
        RouteAddress::Inet6(v6) => Some(v6.to_string()),
        _ => None,
    }
}

// ── Route add ─────────────────────────────────────────────────────────────────

/// Issue a netlink route-add for the given destination/gateway/OIF/metric.
///
/// Uses `RouteMessageBuilder<IpAddr>` (rtnetlink 0.20) which sets the correct
/// defaults: RT_TABLE_MAIN, RTPROT_STATIC, RT_SCOPE_UNIVERSE, RTN_UNICAST.
///
/// Sends the message with `NLM_F_APPEND` (equivalent to `ip route append`)
/// so that multiple routes with the same destination and metric but different
/// gateways can coexist in the routing table.
async fn add_route(
    handle: &Handle,
    index: u32,
    rf: &RouteFields,
) -> Result<(), BackendError> {
    let builder = RouteMessageBuilder::<IpAddr>::new();

    let builder = builder
        .destination_prefix(rf.dst_ip, rf.dst_prefix)
        .map_err(|e| BackendError::Internal(format!("invalid route destination: {e}")))?;

    let builder = if let Some(gw) = rf.gateway {
        builder
            .gateway(gw)
            .map_err(|e| BackendError::Internal(format!("invalid route gateway: {e}")))?
    } else {
        builder
    };

    let builder = if let Some(t) = rf.table {
        builder.table_id(t)
    } else {
        builder
    };

    let mut msg = builder
        .output_interface(index)
        .priority(rf.metric)
        .build();

    msg.header.tos = rf.tos;

    if let Some(m) = rf.mtu {
        msg.attributes.push(
            netlink_packet_route::route::RouteAttribute::Metrics(
                vec![netlink_packet_route::route::RouteMetric::Mtu(m)]
            )
        );
    }

    // Bypass RouteAddRequest to use NLM_F_APPEND instead of NLM_F_EXCL.
    let mut req = NetlinkMessage::from(RouteNetlinkMessage::NewRoute(msg));
    req.header.flags = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_APPEND;

    let mut handle = handle.clone();
    let mut response = handle
        .request(req)
        .map_err(|e| map_netlink_error(e, &format!("add route via index {index}")))?;
    while let Some(message) = response.next().await {
        if let NetlinkPayload::Error(err) = message.payload {
            return Err(map_netlink_error(
                rtnetlink::Error::NetlinkError(err),
                &format!("add route via index {index}"),
            ));
        }
    }
    Ok(())
}

// ── Error classification ──────────────────────────────────────────────────────

/// Extract the positive errno from an rtnetlink error, if present.
///
/// `ErrorMessage.code` is `Option<NonZeroI32>` where the raw value is the
/// negative errno as sent by the kernel (e.g., -17 for EEXIST).
fn extract_errno(err: &rtnetlink::Error) -> Option<i32> {
    if let rtnetlink::Error::NetlinkError(msg) = err {
        msg.code.map(|c| -c.get())
    } else {
        None
    }
}

/// Map an `rtnetlink::Error` to a `BackendError`.
fn map_netlink_error(err: rtnetlink::Error, operation: &str) -> BackendError {
    let errno = extract_errno(&err);
    match errno {
        Some(1) | Some(13) => {
            // EPERM / EACCES
            BackendError::PermissionDenied(format!("{operation}: permission denied"))
        }
        Some(19) => {
            // ENODEV
            BackendError::NotFound {
                entity_type: ETHERNET.to_string(),
                selector: Box::new(Selector::new()),
            }
        }
        _ => BackendError::ApplyFailed {
            operation: operation.to_string(),
            source: Box::new(err),
        },
    }
}

/// Returns `true` if the error is EEXIST (17) — used for idempotent add.
fn is_eexist(err: &rtnetlink::Error) -> bool {
    extract_errno(err) == Some(17)
}

/// Returns `true` if the `BackendError` wraps an EEXIST from `add_route`.
fn is_eexist_backend(err: &BackendError) -> bool {
    // add_route returns BackendError directly, so check ApplyFailed's source.
    if let BackendError::ApplyFailed { source, .. } = err {
        if let Some(rt_err) = source.downcast_ref::<rtnetlink::Error>() {
            return is_eexist(rt_err);
        }
    }
    false
}

/// Returns `true` for errors that indicate the object is already absent.
///
/// errno values: ENOENT=2, ESRCH=3, ENODEV=19, EADDRNOTAVAIL=99.
fn is_not_found_error(err: &rtnetlink::Error) -> bool {
    matches!(extract_errno(err), Some(2 | 3 | 19 | 99))
}

// ── Parsing helpers ───────────────────────────────────────────────────────────

/// Parse a CIDR string (e.g., `"10.0.1.50/24"`) into `(IpAddr, prefix_len)`.
fn parse_cidr(cidr: &str) -> Result<(IpAddr, u8), BackendError> {
    let (ip_str, prefix_str) = cidr
        .split_once('/')
        .ok_or_else(|| BackendError::Internal(format!("invalid CIDR: {cidr}")))?;
    let ip: IpAddr = ip_str
        .parse()
        .map_err(|e| BackendError::Internal(format!("invalid IP in CIDR {cidr}: {e}")))?;
    let prefix: u8 = prefix_str
        .parse()
        .map_err(|e| BackendError::Internal(format!("invalid prefix in CIDR {cidr}: {e}")))?;
    Ok((ip, prefix))
}

/// Convert a `Value` to its string representation, handling String, IpNetwork,
/// and IpAddr variants. YAML policy files produce IpNetwork/IpAddr; the kernel
/// query layer produces String. Both must be accepted.
fn value_to_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::IpNetwork(net) => Some(net.to_string()),
        Value::IpAddr(ip) => Some(ip.to_string()),
        _ => None,
    }
}

fn addr_to_cidr(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Map(m) => value_to_str(m.get("address")?),
        Value::IpNetwork(net) => Some(net.to_string()),
        Value::IpAddr(ip) => Some(ip.to_string()),
        _ => None,
    }
}

fn addr_valid_lft(v: &Value) -> Option<u32> {
    v.as_map()?.get("valid_lft")?.as_u64().map(|n| n as u32)
}

fn addr_preferred_lft(v: &Value) -> Option<u32> {
    v.as_map()?.get("preferred_lft")?.as_u64().map(|n| n as u32)
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

fn is_kernel_route(route_val: &Value) -> bool {
    route_val
        .as_map()
        .and_then(|m| m.get("protocol"))
        .and_then(|v| v.as_str())
        .map(|s| s == "kernel")
        .unwrap_or(false)
}

struct RouteFields {
    dst_ip: IpAddr,
    dst_prefix: u8,
    gateway: Option<IpAddr>,
    metric: u32,
    mtu: Option<u32>,
    table: Option<u32>,
    tos: u8,
}

fn extract_route_fields(
    map: &IndexMap<String, Value>,
) -> Result<RouteFields, BackendError> {
    let destination = map
        .get("destination")
        .and_then(value_to_str)
        .ok_or_else(|| BackendError::Internal("route missing destination".to_string()))?;

    let (dst_ip, dst_prefix) = parse_cidr(&destination)?;

    let gateway = map
        .get("gateway")
        .and_then(value_to_str)
        .map(|s| {
            s.parse::<IpAddr>()
                .map_err(|e| BackendError::Internal(format!("invalid gateway: {e}")))
        })
        .transpose()?;

    let metric = map
        .get("metric")
        .and_then(|v| v.as_u64())
        .map(|m| m as u32)
        .unwrap_or(DEFAULT_ROUTE_METRIC);

    let mtu = map.get("mtu").and_then(|v| v.as_u64()).map(|m| m as u32);
    let table = map.get("table").and_then(|v| v.as_u64()).map(|t| t as u32);
    let tos = map.get("tos").and_then(|v| v.as_u64()).map(|t| t as u8).unwrap_or(0);

    Ok(RouteFields { dst_ip, dst_prefix, gateway, metric, mtu, table, tos })
}

// ── Dry-run helpers ───────────────────────────────────────────────────────────

/// Build `FieldChange` entries from a `DiffOp` and the current kernel state.
fn build_planned_changes(op: &DiffOp, current_state: &State) -> Vec<FieldChange> {
    let mut field_changes = Vec::new();

    match op {
        DiffOp::Add { fields, .. } => {
            for (field_name, fv) in fields {
                field_changes.push(FieldChange {
                    field: field_name.clone(),
                    current: None,
                    desired: Some(fv.value.clone()),
                    kind: FieldChangeKind::Set,
                });
            }
        }
        DiffOp::Modify {
            changed_fields,
            removed_fields,
            ..
        } => {
            for (field_name, fv) in changed_fields {
                let current = current_state
                    .fields
                    .get(field_name)
                    .map(|f| f.value.clone());
                let kind = if current.is_some() {
                    FieldChangeKind::Modify
                } else {
                    FieldChangeKind::Set
                };
                field_changes.push(FieldChange {
                    field: field_name.clone(),
                    current,
                    desired: Some(fv.value.clone()),
                    kind,
                });
            }
            for field_name in removed_fields {
                let current = current_state
                    .fields
                    .get(field_name)
                    .map(|f| f.value.clone());
                field_changes.push(FieldChange {
                    field: field_name.clone(),
                    current,
                    desired: None,
                    kind: FieldChangeKind::Unset,
                });
            }
        }
        DiffOp::Remove { .. } => {
            for (field_name, fv) in &current_state.fields {
                field_changes.push(FieldChange {
                    field: field_name.clone(),
                    current: Some(fv.value.clone()),
                    desired: None,
                    kind: FieldChangeKind::Unset,
                });
            }
        }
    }

    field_changes
}

// ── Construction helpers ──────────────────────────────────────────────────────

/// Construct a `([], [failure], [])` triple for a top-level operation failure.
fn fail_op(
    op: DiffOpKind,
    entity_type: &str,
    selector: &Selector,
    error: BackendError,
    fields: Vec<String>,
) -> (
    Vec<AppliedOperation>,
    Vec<FailedOperation>,
    Vec<SkippedOperation>,
) {
    (
        vec![],
        vec![FailedOperation {
            operation: op,
            entity_type: entity_type.to_string(),
            selector: selector.clone(),
            error,
            fields,
        }],
        vec![],
    )
}

/// Construct a `FailedOperation` for a single-field error during modify.
fn make_field_failure(name: &str, error: BackendError, field: &str) -> FailedOperation {
    FailedOperation {
        operation: DiffOpKind::Modify,
        entity_type: "ethernet".to_string(),
        selector: Selector::with_name(name),
        error,
        fields: vec![field.to_string()],
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use netfyr_state::{FieldValue, Provenance, Selector, StateMetadata, Value};

    // ── Helper constructors ───────────────────────────────────────────────────

    fn kernel_default(v: Value) -> FieldValue {
        FieldValue {
            value: v,
            provenance: Provenance::KernelDefault,
        }
    }

    fn empty_state(name: &str) -> State {
        State {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name(name),
            fields: IndexMap::new(),
            metadata: StateMetadata::new(),
            policy_ref: None,
            priority: 100,
        }
    }

    // ── parse_cidr ────────────────────────────────────────────────────────────

    /// Scenario: valid IPv4 CIDR parses into (IpAddr, prefix_len).
    #[test]
    fn test_parse_cidr_valid_ipv4() {
        let (ip, prefix) = parse_cidr("10.0.1.50/24").expect("valid IPv4 CIDR must parse");
        assert_eq!(ip.to_string(), "10.0.1.50");
        assert_eq!(prefix, 24);
    }

    /// Default route 0.0.0.0/0 must parse successfully.
    #[test]
    fn test_parse_cidr_default_route() {
        let (ip, prefix) = parse_cidr("0.0.0.0/0").expect("default route CIDR must parse");
        assert_eq!(prefix, 0);
        assert!(ip.is_unspecified(), "default route IP must be unspecified");
    }

    /// Valid IPv6 CIDR must parse.
    #[test]
    fn test_parse_cidr_valid_ipv6() {
        let (ip, prefix) = parse_cidr("::1/128").expect("IPv6 CIDR must parse");
        assert_eq!(prefix, 128);
        assert!(ip.is_loopback(), "::1 must be identified as loopback");
    }

    /// CIDR without a slash must return an error.
    #[test]
    fn test_parse_cidr_missing_slash_returns_error() {
        let result = parse_cidr("10.0.1.50");
        assert!(result.is_err(), "CIDR without slash must fail");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("invalid CIDR") || err_msg.contains("10.0.1.50"),
            "Error must mention the invalid input; got: {err_msg}"
        );
    }

    /// CIDR with an invalid IP address must return an error.
    #[test]
    fn test_parse_cidr_invalid_ip_returns_error() {
        let result = parse_cidr("not-an-ip/24");
        assert!(result.is_err(), "Invalid IP in CIDR must fail");
    }

    /// CIDR with a non-numeric prefix must return an error.
    #[test]
    fn test_parse_cidr_invalid_prefix_returns_error() {
        let result = parse_cidr("10.0.1.1/abc");
        assert!(result.is_err(), "Non-numeric prefix in CIDR must fail");
    }

    // ── extract_route_fields ──────────────────────────────────────────────────

    /// Scenario: valid route map with destination and gateway.
    #[test]
    fn test_extract_route_fields_valid_with_gateway() {
        let mut map = IndexMap::new();
        map.insert(
            "destination".to_string(),
            Value::String("10.100.0.0/24".to_string()),
        );
        map.insert(
            "gateway".to_string(),
            Value::String("10.99.0.1".to_string()),
        );

        let rf = extract_route_fields(&map).expect("valid route map must parse");
        assert_eq!(rf.dst_ip.to_string(), "10.100.0.0");
        assert_eq!(rf.dst_prefix, 24);
        let gw = rf.gateway.expect("gateway must be Some");
        assert_eq!(gw.to_string(), "10.99.0.1");
        assert_eq!(rf.metric, DEFAULT_ROUTE_METRIC, "metric must default to DEFAULT_ROUTE_METRIC when absent");
        assert_eq!(rf.mtu, None);
        assert_eq!(rf.table, None);
        assert_eq!(rf.tos, 0);
    }

    /// Route map without a gateway must parse with gateway == None.
    #[test]
    fn test_extract_route_fields_valid_without_gateway() {
        let mut map = IndexMap::new();
        map.insert(
            "destination".to_string(),
            Value::String("10.100.0.0/24".to_string()),
        );

        let rf = extract_route_fields(&map).expect("route without gateway must parse");
        assert!(rf.gateway.is_none(), "gateway must be None when absent from map");
        assert_eq!(rf.metric, DEFAULT_ROUTE_METRIC);
    }

    /// Route map with default destination 0.0.0.0/0 and gateway must parse.
    #[test]
    fn test_extract_route_fields_default_route_with_gateway() {
        let mut map = IndexMap::new();
        map.insert(
            "destination".to_string(),
            Value::String("0.0.0.0/0".to_string()),
        );
        map.insert(
            "gateway".to_string(),
            Value::String("10.0.0.1".to_string()),
        );

        let rf = extract_route_fields(&map).expect("default route map must parse");
        assert!(rf.dst_ip.is_unspecified());
        assert_eq!(rf.dst_prefix, 0);
        assert!(rf.gateway.is_some());
    }

    /// Missing destination field must return an error.
    #[test]
    fn test_extract_route_fields_missing_destination_returns_error() {
        let mut map = IndexMap::new();
        map.insert(
            "gateway".to_string(),
            Value::String("10.99.0.1".to_string()),
        );

        let result = extract_route_fields(&map);
        assert!(result.is_err(), "Missing destination must return error");
    }

    /// Invalid destination CIDR must return an error.
    #[test]
    fn test_extract_route_fields_invalid_destination_returns_error() {
        let mut map = IndexMap::new();
        map.insert(
            "destination".to_string(),
            Value::String("not-a-cidr".to_string()),
        );

        let result = extract_route_fields(&map);
        assert!(result.is_err(), "Invalid destination CIDR must return error");
    }

    /// Explicit metric in route map is returned instead of the default.
    #[test]
    fn test_extract_route_fields_explicit_metric() {
        let mut map = IndexMap::new();
        map.insert(
            "destination".to_string(),
            Value::String("10.100.0.0/24".to_string()),
        );
        map.insert("metric".to_string(), Value::U64(200));

        let rf = extract_route_fields(&map).expect("route with explicit metric must parse");
        assert_eq!(rf.metric, 200, "explicit metric must be returned");
    }

    /// Invalid gateway IP must return an error.
    #[test]
    fn test_extract_route_fields_invalid_gateway_returns_error() {
        let mut map = IndexMap::new();
        map.insert(
            "destination".to_string(),
            Value::String("10.100.0.0/24".to_string()),
        );
        map.insert(
            "gateway".to_string(),
            Value::String("not-an-ip".to_string()),
        );

        let result = extract_route_fields(&map);
        assert!(result.is_err(), "Invalid gateway IP must return error");
    }

    /// Route with mtu, table, and tos extracts all fields correctly.
    #[test]
    fn test_extract_route_fields_with_mtu_table_tos() {
        let mut map = IndexMap::new();
        map.insert("destination".to_string(), Value::String("10.0.0.0/8".to_string()));
        map.insert("metric".to_string(), Value::U64(100));
        map.insert("mtu".to_string(), Value::U64(1400));
        map.insert("table".to_string(), Value::U64(200));
        map.insert("tos".to_string(), Value::U64(16));

        let rf = extract_route_fields(&map).expect("route with mtu/table/tos must parse");
        assert_eq!(rf.mtu, Some(1400));
        assert_eq!(rf.table, Some(200));
        assert_eq!(rf.tos, 16);
    }

    /// Link-local IPv6 addresses are detected correctly.
    #[test]
    fn test_is_link_local() {
        assert!(is_link_local("fe80::1/64"));
        assert!(is_link_local("fe80::dead:beef/128"));
        assert!(!is_link_local("fd00::1/64"));
        assert!(!is_link_local("10.0.1.1/24"));
        assert!(!is_link_local("::1/128"));
    }

    // ── build_planned_changes ─────────────────────────────────────────────────

    /// Scenario: Modify op — changed field produces a FieldChange with kind=Modify
    /// and correct before/after values.
    #[test]
    fn test_build_planned_changes_modify_existing_field_produces_modify_kind() {
        let mut changed_fields = IndexMap::new();
        changed_fields.insert("mtu".to_string(), kernel_default(Value::U64(9000)));

        let op = DiffOp::Modify {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            changed_fields,
            removed_fields: vec![],
        };

        let mut current = empty_state("eth0");
        current
            .fields
            .insert("mtu".to_string(), kernel_default(Value::U64(1500)));

        let changes = build_planned_changes(&op, &current);

        assert_eq!(changes.len(), 1, "One FieldChange for mtu");
        let fc = &changes[0];
        assert_eq!(fc.field, "mtu");
        assert_eq!(
            fc.current,
            Some(Value::U64(1500)),
            "current must be the kernel value 1500"
        );
        assert_eq!(
            fc.desired,
            Some(Value::U64(9000)),
            "desired must be the requested value 9000"
        );
        assert_eq!(
            fc.kind,
            FieldChangeKind::Modify,
            "kind must be Modify when field exists in current state"
        );
    }

    /// Modify op — field that is new (not in current state) gets kind=Set.
    #[test]
    fn test_build_planned_changes_modify_new_field_produces_set_kind() {
        let mut changed_fields = IndexMap::new();
        changed_fields.insert(
            "addresses".to_string(),
            kernel_default(Value::List(vec![Value::String("10.0.1.1/24".to_string())])),
        );

        let op = DiffOp::Modify {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            changed_fields,
            removed_fields: vec![],
        };

        // Current state has no "addresses" field.
        let current = empty_state("eth0");
        let changes = build_planned_changes(&op, &current);

        assert_eq!(changes.len(), 1);
        let fc = &changes[0];
        assert_eq!(fc.field, "addresses");
        assert!(fc.current.is_none(), "Set kind must have no current value");
        assert!(fc.desired.is_some(), "Set kind must have a desired value");
        assert_eq!(
            fc.kind,
            FieldChangeKind::Set,
            "kind must be Set when field does not exist in current state"
        );
    }

    /// Modify op — removed field produces kind=Unset with current value and no desired.
    #[test]
    fn test_build_planned_changes_removed_field_produces_unset_kind() {
        let op = DiffOp::Modify {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            changed_fields: IndexMap::new(),
            removed_fields: vec!["addresses".to_string()],
        };

        let mut current = empty_state("eth0");
        current.fields.insert(
            "addresses".to_string(),
            kernel_default(Value::List(vec![Value::String("10.0.1.1/24".to_string())])),
        );

        let changes = build_planned_changes(&op, &current);

        let fc = changes
            .iter()
            .find(|fc| fc.field == "addresses")
            .expect("addresses field change must be present");
        assert_eq!(
            fc.kind,
            FieldChangeKind::Unset,
            "Removed field must produce Unset kind"
        );
        assert!(fc.current.is_some(), "Unset kind must have a current value");
        assert!(fc.desired.is_none(), "Unset kind must have no desired value");
    }

    /// Add op — produces FieldChange with kind=Set and no current value.
    #[test]
    fn test_build_planned_changes_add_op_produces_set_kind_no_current() {
        let mut fields = IndexMap::new();
        fields.insert("mtu".to_string(), kernel_default(Value::U64(1500)));

        let op = DiffOp::Add {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
            fields,
        };

        let current = empty_state("eth0");
        let changes = build_planned_changes(&op, &current);

        assert_eq!(changes.len(), 1, "One FieldChange for mtu");
        let fc = &changes[0];
        assert_eq!(fc.field, "mtu");
        assert!(fc.current.is_none(), "Add op has no current value");
        assert_eq!(fc.desired, Some(Value::U64(1500)));
        assert_eq!(fc.kind, FieldChangeKind::Set);
    }

    /// Remove op — each current field produces FieldChange with kind=Unset and no desired.
    #[test]
    fn test_build_planned_changes_remove_op_produces_unset_for_each_current_field() {
        let op = DiffOp::Remove {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
        };

        let mut current = empty_state("eth0");
        current
            .fields
            .insert("mtu".to_string(), kernel_default(Value::U64(1500)));
        current.fields.insert(
            "enabled".to_string(),
            kernel_default(Value::Bool(true)),
        );

        let changes = build_planned_changes(&op, &current);

        assert_eq!(
            changes.len(),
            2,
            "Remove op produces one Unset per current field"
        );
        for fc in &changes {
            assert_eq!(
                fc.kind,
                FieldChangeKind::Unset,
                "All Remove field changes must be Unset kind"
            );
            assert!(fc.current.is_some(), "Unset kind must have current value");
            assert!(fc.desired.is_none(), "Unset kind must have no desired");
        }
    }

    /// Remove op on an empty current state produces an empty changes list.
    #[test]
    fn test_build_planned_changes_remove_op_empty_current_produces_empty_changes() {
        let op = DiffOp::Remove {
            entity_type: "ethernet".to_string(),
            selector: Selector::with_name("eth0"),
        };
        let current = empty_state("eth0");
        let changes = build_planned_changes(&op, &current);
        assert!(
            changes.is_empty(),
            "Remove op on empty current state must produce no field changes"
        );
    }

    // ── value_to_str ──────────────────────────────────────────────────────────

    /// Value::String is returned as-is (the common case for kernel-queried CIDR strings).
    #[test]
    fn test_value_to_str_string_variant_returns_inner_string() {
        let v = Value::String("10.0.1.50/24".to_string());
        assert_eq!(
            value_to_str(&v),
            Some("10.0.1.50/24".to_string()),
            "Value::String must be returned unchanged"
        );
    }

    /// Value::IpAddr (from YAML bare IP) returns its dotted-decimal representation.
    #[test]
    fn test_value_to_str_ipaddr_variant_returns_dotted_decimal() {
        use std::net::Ipv4Addr;
        let v = Value::IpAddr(std::net::IpAddr::V4(Ipv4Addr::new(10, 0, 1, 50)));
        assert_eq!(
            value_to_str(&v),
            Some("10.0.1.50".to_string()),
            "Value::IpAddr must be formatted as dotted-decimal"
        );
    }

    /// Value::U64 (e.g., mtu) is not a string-like type — must return None.
    #[test]
    fn test_value_to_str_u64_variant_returns_none() {
        let v = Value::U64(1500);
        assert_eq!(
            value_to_str(&v),
            None,
            "Value::U64 must return None from value_to_str"
        );
    }

    /// Value::Bool (e.g., carrier) is not a string-like type — must return None.
    #[test]
    fn test_value_to_str_bool_variant_returns_none() {
        let v = Value::Bool(true);
        assert_eq!(
            value_to_str(&v),
            None,
            "Value::Bool must return None from value_to_str"
        );
    }

    /// Value::I64 is not a string-like type — must return None.
    #[test]
    fn test_value_to_str_i64_variant_returns_none() {
        let v = Value::I64(-1);
        assert_eq!(
            value_to_str(&v),
            None,
            "Value::I64 must return None from value_to_str"
        );
    }

    /// Value::List is not a string-like type — must return None.
    #[test]
    fn test_value_to_str_list_variant_returns_none() {
        let v = Value::List(vec![Value::String("10.0.1.1/24".to_string())]);
        assert_eq!(
            value_to_str(&v),
            None,
            "Value::List must return None from value_to_str"
        );
    }

    /// Value::Map is not a string-like type — must return None.
    #[test]
    fn test_value_to_str_map_variant_returns_none() {
        let mut map = IndexMap::new();
        map.insert("destination".to_string(), Value::String("0.0.0.0/0".to_string()));
        let v = Value::Map(map);
        assert_eq!(
            value_to_str(&v),
            None,
            "Value::Map must return None from value_to_str"
        );
    }

    /// Empty string is a valid Value::String — must return Some("").
    #[test]
    fn test_value_to_str_empty_string_variant_returns_some_empty() {
        let v = Value::String(String::new());
        assert_eq!(
            value_to_str(&v),
            Some(String::new()),
            "Value::String(\"\") must return Some(\"\")"
        );
    }

    // ── addr_to_cidr ────────────────────────────────────────────────────────

    #[test]
    fn test_addr_to_cidr_from_string() {
        let v = Value::String("10.0.1.50/24".to_string());
        assert_eq!(addr_to_cidr(&v), Some("10.0.1.50/24".to_string()));
    }

    #[test]
    fn test_addr_to_cidr_from_map() {
        let mut m = IndexMap::new();
        m.insert("address".to_string(), Value::String("10.0.1.50/24".to_string()));
        m.insert("valid_lft".to_string(), Value::U64(3600));
        let v = Value::Map(m);
        assert_eq!(addr_to_cidr(&v), Some("10.0.1.50/24".to_string()));
    }

    #[test]
    fn test_addr_to_cidr_from_ip_addr() {
        let v = Value::IpAddr(std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 1, 1)));
        assert_eq!(addr_to_cidr(&v), Some("10.0.1.1".to_string()));
    }

    #[test]
    fn test_addr_to_cidr_from_non_address() {
        let v = Value::U64(1500);
        assert_eq!(addr_to_cidr(&v), None);
    }

    // ── is_kernel_route ───────────────────────────────────────────────────────

    /// Scenario: Modify operation must not remove kernel routes.
    /// A route map with protocol="kernel" is detected as a kernel route.
    #[test]
    fn test_is_kernel_route_returns_true_for_protocol_kernel() {
        let mut m = IndexMap::new();
        m.insert("destination".to_string(), Value::String("10.0.0.0/8".to_string()));
        m.insert("protocol".to_string(), Value::String("kernel".to_string()));
        let v = Value::Map(m);
        assert!(is_kernel_route(&v), "route with protocol='kernel' must be detected as a kernel route");
    }

    /// A route map with protocol="static" is NOT a kernel route.
    #[test]
    fn test_is_kernel_route_returns_false_for_static_protocol() {
        let mut m = IndexMap::new();
        m.insert("destination".to_string(), Value::String("0.0.0.0/0".to_string()));
        m.insert("protocol".to_string(), Value::String("static".to_string()));
        let v = Value::Map(m);
        assert!(!is_kernel_route(&v), "route with protocol='static' must not be a kernel route");
    }

    /// A route map without a protocol field is NOT a kernel route.
    #[test]
    fn test_is_kernel_route_returns_false_when_protocol_absent() {
        let mut m = IndexMap::new();
        m.insert("destination".to_string(), Value::String("10.0.0.0/24".to_string()));
        m.insert("gateway".to_string(), Value::String("10.0.0.1".to_string()));
        let v = Value::Map(m);
        assert!(!is_kernel_route(&v), "route without protocol field must not be a kernel route");
    }

    /// A non-map Value (e.g., a plain string) is never a kernel route.
    #[test]
    fn test_is_kernel_route_returns_false_for_non_map_value() {
        let v = Value::String("10.0.0.0/24".to_string());
        assert!(!is_kernel_route(&v), "non-map value must not be detected as a kernel route");
    }

    /// A map with protocol set to a numeric value is NOT a kernel route.
    #[test]
    fn test_is_kernel_route_returns_false_for_numeric_protocol() {
        let mut m = IndexMap::new();
        m.insert("destination".to_string(), Value::String("0.0.0.0/0".to_string()));
        m.insert("protocol".to_string(), Value::U64(4));
        let v = Value::Map(m);
        assert!(!is_kernel_route(&v), "numeric protocol field must not be treated as 'kernel'");
    }

    // ── addr_valid_lft ────────────────────────────────────────────────────────

    /// Scenario: Map-format addresses with valid_lft have CacheInfo set.
    /// addr_valid_lft must extract the valid_lft field from a map Value.
    #[test]
    fn test_addr_valid_lft_returns_value_from_map() {
        let mut m = IndexMap::new();
        m.insert("address".to_string(), Value::String("10.0.1.50/24".to_string()));
        m.insert("valid_lft".to_string(), Value::U64(3600));
        let v = Value::Map(m);
        assert_eq!(
            addr_valid_lft(&v),
            Some(3600),
            "addr_valid_lft must return 3600 when valid_lft=3600 is in the map"
        );
    }

    /// addr_valid_lft returns None when the valid_lft field is absent from the map.
    #[test]
    fn test_addr_valid_lft_returns_none_when_field_missing() {
        let mut m = IndexMap::new();
        m.insert("address".to_string(), Value::String("10.0.1.50/24".to_string()));
        let v = Value::Map(m);
        assert_eq!(
            addr_valid_lft(&v),
            None,
            "addr_valid_lft must return None when valid_lft is absent"
        );
    }

    /// addr_valid_lft returns None for a plain-string address value (no map).
    #[test]
    fn test_addr_valid_lft_returns_none_for_non_map_value() {
        let v = Value::String("10.0.1.50/24".to_string());
        assert_eq!(
            addr_valid_lft(&v),
            None,
            "addr_valid_lft must return None for a non-map Value"
        );
    }

    /// addr_valid_lft returns None when valid_lft is present but has a non-integer type.
    #[test]
    fn test_addr_valid_lft_returns_none_when_field_is_non_integer() {
        let mut m = IndexMap::new();
        m.insert("address".to_string(), Value::String("10.0.1.50/24".to_string()));
        m.insert("valid_lft".to_string(), Value::String("forever".to_string()));
        let v = Value::Map(m);
        assert_eq!(
            addr_valid_lft(&v),
            None,
            "addr_valid_lft must return None when valid_lft is a non-integer"
        );
    }

    // ── addr_preferred_lft ────────────────────────────────────────────────────

    /// addr_preferred_lft extracts the preferred_lft field from a map Value.
    #[test]
    fn test_addr_preferred_lft_returns_value_from_map() {
        let mut m = IndexMap::new();
        m.insert("address".to_string(), Value::String("10.0.1.50/24".to_string()));
        m.insert("preferred_lft".to_string(), Value::U64(1800));
        let v = Value::Map(m);
        assert_eq!(
            addr_preferred_lft(&v),
            Some(1800),
            "addr_preferred_lft must return 1800 when preferred_lft=1800 is in the map"
        );
    }

    /// addr_preferred_lft returns None when preferred_lft is absent from the map.
    #[test]
    fn test_addr_preferred_lft_returns_none_when_field_missing() {
        let mut m = IndexMap::new();
        m.insert("address".to_string(), Value::String("10.0.1.50/24".to_string()));
        let v = Value::Map(m);
        assert_eq!(
            addr_preferred_lft(&v),
            None,
            "addr_preferred_lft must return None when preferred_lft is absent"
        );
    }

    /// addr_preferred_lft returns None for a plain-string value (no map).
    #[test]
    fn test_addr_preferred_lft_returns_none_for_non_map_value() {
        let v = Value::String("10.0.1.50/24".to_string());
        assert_eq!(
            addr_preferred_lft(&v),
            None,
            "addr_preferred_lft must return None for a non-map Value"
        );
    }

    /// A map with both valid_lft and preferred_lft returns both correctly.
    #[test]
    fn test_addr_lft_both_fields_present_in_map() {
        let mut m = IndexMap::new();
        m.insert("address".to_string(), Value::String("10.0.1.50/24".to_string()));
        m.insert("valid_lft".to_string(), Value::U64(7200));
        m.insert("preferred_lft".to_string(), Value::U64(3600));
        let v = Value::Map(m);
        assert_eq!(addr_valid_lft(&v), Some(7200), "valid_lft must be 7200");
        assert_eq!(addr_preferred_lft(&v), Some(3600), "preferred_lft must be 3600");
    }

    // ── WRITABLE_FIELDS constant ──────────────────────────────────────────────

    /// Scenario: Modify operation skips read-only fields (defensive).
    /// The WRITABLE_FIELDS constant must include the expected writable fields.
    #[test]
    fn test_writable_fields_contains_mtu_enabled_addresses_routes() {
        let writable = WRITABLE_FIELDS;
        for expected in &["mtu", "enabled", "addresses", "routes"] {
            assert!(
                writable.contains(expected),
                "WRITABLE_FIELDS must contain '{}'; got: {:?}",
                expected, writable
            );
        }
    }

    /// Read-only fields carrier, speed, mac, driver, dns_servers must NOT be in WRITABLE_FIELDS.
    #[test]
    fn test_writable_fields_does_not_contain_carrier_speed_mac_driver() {
        let writable = WRITABLE_FIELDS;
        for read_only in &["carrier", "speed", "mac", "driver", "dns_servers", "name"] {
            assert!(
                !writable.contains(read_only),
                "WRITABLE_FIELDS must NOT contain '{}'; got: {:?}",
                read_only, writable
            );
        }
    }
}
