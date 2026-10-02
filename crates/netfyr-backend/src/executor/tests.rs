use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::FutureExt;
use netfyr_state::plan::{AddressGroupPrecondition, FieldChange};

use super::*;

struct MockIo {
    observation: Observation,
    attempts: Vec<(InterfaceId, OperationKind)>,
    inventory_calls: usize,
    fail_inventory_at: Option<usize>,
    fail_execute_at: Option<usize>,
    inventory_override_at: Option<(usize, Observation)>,
    cancel_after_write: Option<Arc<AtomicBool>>,
    cancel_on_inventory: Option<Arc<AtomicBool>>,
    cascade_primary: bool,
}

impl MockIo {
    fn new(devices: Vec<DeviceObservation>) -> Self {
        Self {
            observation: Observation {
                devices,
                diagnostics: Vec::new(),
            },
            attempts: Vec::new(),
            inventory_calls: 0,
            fail_inventory_at: None,
            fail_execute_at: None,
            inventory_override_at: None,
            cancel_after_write: None,
            cancel_on_inventory: None,
            cascade_primary: false,
        }
    }
}

fn kernel_lifetime_flags(address: &Ipv4Address) -> u32 {
    let (valid, preferred) = address.lifetimes();
    (if valid == u32::MAX {
        IFA_F_PERMANENT
    } else {
        0
    }) | if preferred == 0 { IFA_F_DEPRECATED } else { 0 }
}

// net/ipv4/devinet.c: a new address is secondary when an existing one with
// the same mask has its peer, or local address, inside the new prefix.
fn kernel_secondary(existing: &[AddressObservation], address: &Ipv4Address) -> bool {
    let mask = u32::MAX
        .checked_shl(u32::from(32 - address.prefix_len))
        .unwrap_or(0);
    existing.iter().any(|entry| {
        let other = u32::from(entry.peer.unwrap_or(entry.address.ip));
        entry.address.prefix_len == address.prefix_len
            && (other ^ u32::from(address.ip)) & mask == 0
    })
}

impl Io for MockIo {
    fn namespace(&self) -> NamespaceId {
        namespace()
    }

    fn inventory(&mut self) -> IoFuture<'_, Result<Observation, BackendError>> {
        Box::pin(async move {
            self.inventory_calls += 1;
            if let Some((call, observation)) = &self.inventory_override_at {
                if *call == self.inventory_calls {
                    self.observation = observation.clone();
                }
            }
            if let Some(cancelled) = &self.cancel_on_inventory {
                cancelled.store(true, Ordering::SeqCst);
            }
            if self.fail_inventory_at == Some(self.inventory_calls) {
                return Err(BackendError::new(
                    BackendErrorKind::QueryFailed,
                    "injected inventory failure",
                ));
            }
            Ok(self.observation.clone())
        })
    }

    fn execute<'a>(
        &'a mut self,
        target: &'a InterfaceId,
        kind: &'a OperationKind,
    ) -> IoFuture<'a, Result<(), BackendError>> {
        Box::pin(async move {
            self.attempts.push((target.clone(), kind.clone()));
            if self.fail_execute_at == Some(self.attempts.len()) {
                let mut failure =
                    BackendError::new(BackendErrorKind::PermissionDenied, "injected write failure");
                failure.errno = Some(libc::EPERM);
                failure.extack = Some("fixture rejected this operation".into());
                return Err(failure);
            }
            let device = self
                .observation
                .devices
                .iter_mut()
                .find(|device| device.identity == *target)
                .unwrap();
            match kind {
                OperationKind::SetMtu(mtu) => {
                    device
                        .state
                        .fields
                        .insert("mtu".into(), Value::U64(u64::from(*mtu)));
                }
                OperationKind::SetEnabled(enabled) => {
                    device
                        .state
                        .fields
                        .insert("enabled".into(), Value::Bool(*enabled));
                }
                OperationKind::AddIpv4(address) => {
                    let secondary = kernel_secondary(&device.addresses, address);
                    device.addresses.push(AddressObservation {
                        address: address.clone(),
                        flags: kernel_lifetime_flags(address)
                            | if secondary { IFA_F_SECONDARY } else { 0 },
                        scope: 0,
                        peer: None,
                    });
                }
                OperationKind::RemoveIpv4(address) => {
                    let same = |entry: &AddressObservation| {
                        entry.address.ip == address.ip
                            && entry.address.prefix_len == address.prefix_len
                    };
                    let primary = device
                        .addresses
                        .iter()
                        .any(|entry| same(entry) && entry.flags & IFA_F_SECONDARY == 0);
                    device.addresses.retain(|entry| {
                        if self.cascade_primary && primary {
                            entry.address.group() != address.group()
                        } else {
                            !same(entry)
                        }
                    });
                    if primary {
                        if let Some(first) = device
                            .addresses
                            .iter_mut()
                            .find(|entry| entry.address.group() == address.group())
                        {
                            first.flags &= !IFA_F_SECONDARY;
                        }
                    }
                }
                OperationKind::ReplaceIpv4(address) => {
                    let entry = device
                        .addresses
                        .iter_mut()
                        .find(|entry| {
                            entry.address.ip == address.ip
                                && entry.address.prefix_len == address.prefix_len
                        })
                        .unwrap();
                    entry.address = address.clone();
                    entry.flags = (entry.flags & !(IFA_F_PERMANENT | IFA_F_DEPRECATED))
                        | kernel_lifetime_flags(address);
                }
                unexpected => panic!("executor sent an undecoded operation: {unexpected:?}"),
            }
            if let Some(cancelled) = &self.cancel_after_write {
                cancelled.store(true, Ordering::SeqCst);
            }
            Ok(())
        })
    }
}

fn namespace() -> NamespaceId {
    NamespaceId {
        device: 4,
        inode: 9,
    }
}

fn device(index: u32) -> DeviceObservation {
    let mut state = State::new(Source::Kernel);
    state.device_type = ETHERNET.into();
    state.fields.insert("mtu".into(), Value::U64(1500));
    state.fields.insert("enabled".into(), Value::Bool(true));
    DeviceObservation {
        identity: InterfaceId {
            namespace: namespace(),
            index,
            generation: 1,
            name: format!("veth{index}"),
            device_type: ETHERNET.into(),
            mac: Some(format!("02:00:00:00:00:{index:02x}")),
        },
        state,
        addresses: Vec::new(),
        addresses_complete: true,
        observed_at: Instant::now(),
    }
}

fn address(last: u8) -> Ipv4Address {
    Ipv4Address {
        ip: Ipv4Addr::new(192, 0, 2, last),
        prefix_len: 24,
        valid_lft: None,
        preferred_lft: None,
    }
}

fn observed(address: Ipv4Address, secondary: bool) -> AddressObservation {
    AddressObservation {
        flags: kernel_lifetime_flags(&address) | if secondary { IFA_F_SECONDARY } else { 0 },
        address,
        scope: 0,
        peer: None,
    }
}

fn op(device: &DeviceObservation, kind: OperationKind, depends_on: &[usize]) -> Operation {
    Operation {
        target: device.identity.clone(),
        kind,
        depends_on: depends_on.into(),
        address_group: None,
    }
}

fn group_op(device: &DeviceObservation, kind: OperationKind, depends_on: &[usize]) -> Operation {
    let mut operation = op(device, kind, depends_on);
    operation.address_group = Some(AddressGroupPrecondition {
        addresses: device
            .addresses
            .iter()
            .filter(|entry| entry.address.ip.octets()[0..3] == [192, 0, 2])
            .cloned()
            .collect(),
        observed_at: device.observed_at,
    });
    operation
}

fn plan(operations: Vec<Operation>) -> StateDiff {
    StateDiff {
        operations,
        ..StateDiff::default()
    }
}

fn apply(io: &mut MockIo, operations: Vec<Operation>) -> ApplyReport {
    apply_plan(io, &plan(operations), &|| false)
        .now_or_never()
        .expect("mock I/O is immediately ready")
}

fn preview(io: &mut MockIo, operations: Vec<Operation>) -> DryRunReport {
    preview_plan(io, &plan(operations))
        .now_or_never()
        .expect("mock I/O is immediately ready")
}

fn indices(contexts: &[OperationContext]) -> Vec<usize> {
    contexts.iter().map(|context| context.index).collect()
}

fn failed_indices(failed: &[FailedOperation]) -> Vec<usize> {
    failed.iter().map(|entry| entry.context.index).collect()
}

fn failed_kinds(failed: &[FailedOperation]) -> Vec<BackendErrorKind> {
    failed.iter().map(|entry| entry.error.kind).collect()
}

fn statuses(report: &DryRunReport) -> Vec<PlannedStatus> {
    report
        .operations
        .iter()
        .map(|entry| entry.status.clone())
        .collect()
}

#[test]
fn failure_blocks_transitive_dependencies_but_keeps_independent_order() {
    let first = device(1);
    let second = device(2);
    let operations = vec![
        op(&first, OperationKind::SetMtu(9000), &[]),
        op(&first, OperationKind::SetEnabled(false), &[0]),
        op(&second, OperationKind::SetMtu(1800), &[]),
        op(&second, OperationKind::SetEnabled(false), &[2]),
        op(&first, OperationKind::AddIpv4(address(1)), &[1]),
    ];
    let mut io = MockIo::new(vec![first.clone(), second.clone()]);
    io.fail_execute_at = Some(1);
    let report = apply(&mut io, operations);
    assert!(!report.is_success());
    assert_eq!(indices(&report.succeeded), [2, 3]);
    assert_eq!(failed_indices(&report.failed), [0]);
    assert_eq!(report.failed[0].context.target, first.identity);
    assert_eq!(report.failed[0].error.errno, Some(libc::EPERM));
    assert_eq!(
        report.failed[0].error.extack.as_deref(),
        Some("fixture rejected this operation")
    );
    assert_eq!(
        report
            .skipped
            .iter()
            .map(|entry| (entry.context.index, entry.reason.clone()))
            .collect::<Vec<_>>(),
        [
            (
                1,
                SkipReason::DependencyFailed {
                    operations: vec![0]
                }
            ),
            (
                4,
                SkipReason::DependencyFailed {
                    operations: vec![1]
                }
            )
        ]
    );
    assert_eq!(
        io.attempts
            .iter()
            .map(|(_, kind)| kind.clone())
            .collect::<Vec<_>>(),
        [
            OperationKind::SetMtu(9000),
            OperationKind::SetMtu(1800),
            OperationKind::SetEnabled(false)
        ]
    );
    assert_eq!(
        io.observation.devices[0].state.fields["mtu"],
        Value::U64(1500)
    );
    assert_eq!(
        io.observation.devices[1].state.fields["enabled"],
        Value::Bool(false)
    );
}

#[test]
fn dry_run_reports_changes_and_operations_without_claiming_writes() {
    let target = device(1);
    let original = target.clone();
    let diff = StateDiff {
        changes: vec![FieldChange {
            target: target.identity.clone(),
            field: "mtu".into(),
            old: Some(Value::U64(1500)),
            desired: Some(Value::U64(9000)),
            source: Source::Static {
                policy: "test".into(),
            },
        }],
        operations: vec![
            op(&target, OperationKind::SetMtu(9000), &[]),
            op(&target, OperationKind::AddIpv4(address(1)), &[0]),
        ],
        diagnostics: vec![Diagnostic {
            target: Some(target.identity.clone()),
            field: None,
            message: "test diagnostic".into(),
        }],
    };
    let mut io = MockIo::new(vec![target]);
    let report = preview_plan(&mut io, &diff).now_or_never().unwrap();
    assert!(report.is_success());
    assert_eq!(report.changes, diff.changes);
    assert_eq!(report.diagnostics, diff.diagnostics);
    assert_eq!(
        statuses(&report),
        [PlannedStatus::Write, PlannedStatus::Write]
    );
    assert!(io.attempts.is_empty());
    assert_eq!(io.inventory_calls, 1);
    assert!(io.observation.devices[0].state.content_eq(&original.state));
    assert_eq!(io.observation.devices[0].addresses, original.addresses);
}

#[test]
fn preview_marks_failed_operations_and_blocked_dependents() {
    let target = device(1);
    let mut io = MockIo::new(vec![target.clone()]);
    let report = preview(
        &mut io,
        vec![
            op(&target, OperationKind::SetMtu(9000), &[]),
            op(&target, OperationKind::SetMtu(10), &[]),
            op(&target, OperationKind::SetEnabled(false), &[1]),
            op(&target, OperationKind::SetMtu(9000), &[]),
        ],
    );
    assert!(!report.is_success());
    assert_eq!(
        statuses(&report),
        [
            PlannedStatus::Write,
            PlannedStatus::Fail,
            PlannedStatus::Skip(SkipReason::DependencyFailed {
                operations: vec![1]
            }),
            PlannedStatus::Skip(SkipReason::AlreadySatisfied),
        ]
    );
    assert_eq!(failed_indices(&report.failed), [1]);
    assert!(io.attempts.is_empty());
}

#[test]
fn stale_namespace_identity_absence_and_unsupported_kind_never_write() {
    for case in 0..4 {
        let mut target = device(1);
        let mut operation = op(&target, OperationKind::SetMtu(9000), &[]);
        let expected = match case {
            0 => {
                operation.target.namespace.inode += 1;
                BackendErrorKind::StaleState
            }
            1 => {
                target.identity.generation += 1;
                BackendErrorKind::StaleState
            }
            2 => {
                target.identity.index += 1;
                BackendErrorKind::NotFound
            }
            _ => {
                target.identity.device_type = "bridge".into();
                operation.target = target.identity.clone();
                BackendErrorKind::UnsupportedEntityType
            }
        };
        let mut io = MockIo::new(vec![target.clone()]);
        let report = apply(&mut io, vec![operation.clone()]);
        assert_eq!(failed_kinds(&report.failed), [expected], "case {case}");
        let dry = preview(&mut io, vec![operation]);
        assert_eq!(failed_kinds(&dry.failed), [expected], "case {case}");
        assert!(io.attempts.is_empty());
    }
}

#[test]
fn malformed_dependencies_fail_and_do_not_block_unrelated_operations() {
    let target = device(1);
    for dependency in [0, 1, usize::MAX] {
        let mut io = MockIo::new(vec![target.clone()]);
        let report = apply(
            &mut io,
            vec![
                op(&target, OperationKind::SetMtu(9000), &[dependency]),
                op(&target, OperationKind::SetEnabled(false), &[0]),
                op(&target, OperationKind::SetMtu(1800), &[]),
            ],
        );
        assert_eq!(
            failed_kinds(&report.failed),
            [BackendErrorKind::UnsupportedOperation]
        );
        assert_eq!(
            report.skipped[0].reason,
            SkipReason::DependencyFailed {
                operations: vec![0]
            }
        );
        assert_eq!(indices(&report.succeeded), [2]);
        assert_eq!(io.attempts.len(), 1);
    }
}

#[test]
fn read_only_fields_skip_but_unknown_fields_and_lifecycle_fail() {
    let target = device(1);
    let mut io = MockIo::new(vec![target.clone()]);
    let kinds = [
        OperationKind::SetField {
            field: "carrier".into(),
            value: Value::Bool(false),
        },
        OperationKind::SetField {
            field: "ethernet.autoneg".into(),
            value: Value::Bool(false),
        },
        OperationKind::SetField {
            field: "unknown".into(),
            value: Value::Bool(false),
        },
        OperationKind::SetField {
            field: "ipv4.addresses".into(),
            value: Value::List(Vec::new()),
        },
        OperationKind::AddEntity,
        OperationKind::RemoveEntity,
    ];
    let report = apply(
        &mut io,
        kinds
            .into_iter()
            .map(|kind| op(&target, kind, &[]))
            .collect(),
    );
    assert_eq!(
        report
            .skipped
            .iter()
            .map(|entry| (entry.context.index, entry.reason.clone()))
            .collect::<Vec<_>>(),
        [(0, SkipReason::ReadOnly), (1, SkipReason::ReadOnly)]
    );
    assert_eq!(failed_indices(&report.failed), [2, 3, 4, 5]);
    assert!(
        failed_kinds(&report.failed)
            .iter()
            .all(|kind| *kind == BackendErrorKind::UnsupportedOperation)
    );
    assert!(io.attempts.is_empty());
}

#[test]
fn mtu_schema_bounds_and_generic_assignment_are_enforced() {
    let target = device(1);
    let field = |field: &str, value| OperationKind::SetField {
        field: field.into(),
        value,
    };
    for kind in [
        OperationKind::SetMtu(0),
        OperationKind::SetMtu(67),
        OperationKind::SetMtu(65536),
        OperationKind::SetMtu(u32::MAX),
        field("mtu", Value::U64(u64::from(u32::MAX) + 1)),
        field("mtu", Value::Bool(true)),
    ] {
        let mut io = MockIo::new(vec![target.clone()]);
        let report = apply(&mut io, vec![op(&target, kind.clone(), &[])]);
        assert_eq!(
            failed_kinds(&report.failed),
            [BackendErrorKind::UnsupportedOperation],
            "{kind:?}"
        );
        assert!(io.attempts.is_empty());
    }
    for mtu in [68, 65535] {
        let mut io = MockIo::new(vec![target.clone()]);
        let report = apply(
            &mut io,
            vec![op(&target, field("mtu", Value::U64(mtu)), &[])],
        );
        assert!(report.is_success());
        assert_eq!(io.attempts[0].1, OperationKind::SetMtu(mtu as u32));
        assert_eq!(
            io.observation.devices[0].state.fields["mtu"],
            Value::U64(mtu)
        );
    }
    let mut io = MockIo::new(vec![target.clone()]);
    let report = apply(
        &mut io,
        vec![op(&target, field("enabled", Value::Bool(false)), &[])],
    );
    assert!(report.is_success());
    assert_eq!(io.attempts[0].1, OperationKind::SetEnabled(false));
}

#[test]
fn invalid_addresses_are_rejected_before_inventory() {
    let target = device(1);
    let mut long = address(1);
    long.prefix_len = 33;
    let mut zero = address(1);
    zero.valid_lft = Some(0);
    let mut inverted = address(1);
    inverted.valid_lft = Some(10);
    inverted.preferred_lft = Some(11);
    for kind in [
        OperationKind::AddIpv4(long.clone()),
        OperationKind::ReplaceIpv4(zero),
        OperationKind::AddIpv4(inverted),
        OperationKind::RemoveIpv4(long),
    ] {
        let mut io = MockIo::new(vec![target.clone()]);
        let report = apply(&mut io, vec![op(&target, kind.clone(), &[])]);
        assert_eq!(
            failed_kinds(&report.failed),
            [BackendErrorKind::UnsupportedOperation],
            "{kind:?}"
        );
        assert_eq!(io.inventory_calls, 0);
        assert!(io.attempts.is_empty());
    }
}

#[test]
fn identical_address_lifetimes_skip_but_conflicting_addition_requires_replace() {
    let mut target = device(1);
    let mut finite = address(1);
    finite.valid_lft = Some(120);
    finite.preferred_lft = Some(60);
    target.addresses.push(observed(finite.clone(), false));
    let mut different = finite.clone();
    different.valid_lft = Some(240);
    let mut io = MockIo::new(vec![target.clone()]);
    let report = apply(
        &mut io,
        vec![
            op(&target, OperationKind::AddIpv4(finite.clone()), &[]),
            op(&target, OperationKind::AddIpv4(different.clone()), &[]),
        ],
    );
    assert_eq!(report.skipped[0].reason, SkipReason::AlreadySatisfied);
    assert_eq!(
        failed_kinds(&report.failed),
        [BackendErrorKind::ApplyFailed]
    );
    assert!(io.attempts.is_empty());
    let report = apply(
        &mut io,
        vec![group_op(
            &target,
            OperationKind::ReplaceIpv4(different.clone()),
            &[],
        )],
    );
    assert!(report.is_success());
    assert_eq!(io.observation.devices[0].addresses[0].address, different);
}

#[test]
fn replacing_an_absent_address_is_not_found() {
    let target = device(1);
    let mut io = MockIo::new(vec![target.clone()]);
    let report = apply(
        &mut io,
        vec![group_op(
            &target,
            OperationKind::ReplaceIpv4(address(1)),
            &[],
        )],
    );
    assert_eq!(failed_kinds(&report.failed), [BackendErrorKind::NotFound]);
    assert!(io.attempts.is_empty());
}

#[test]
fn absent_removal_and_current_scalar_values_are_satisfied_without_writes() {
    let target = device(1);
    let mut io = MockIo::new(vec![target.clone()]);
    let report = apply(
        &mut io,
        vec![
            op(&target, OperationKind::RemoveIpv4(address(1)), &[]),
            op(&target, OperationKind::SetMtu(1500), &[0]),
            op(&target, OperationKind::SetEnabled(true), &[1]),
        ],
    );
    assert!(report.is_success());
    assert_eq!(
        report
            .skipped
            .iter()
            .map(|entry| (entry.context.index, entry.reason.clone()))
            .collect::<Vec<_>>(),
        [
            (0, SkipReason::AlreadySatisfied),
            (1, SkipReason::AlreadySatisfied),
            (2, SkipReason::AlreadySatisfied)
        ]
    );
    assert!(io.attempts.is_empty());
}

#[test]
fn one_observation_serves_operations_until_a_write() {
    let target = device(1);
    let mut io = MockIo::new(vec![target.clone()]);
    let report = apply(
        &mut io,
        vec![
            op(&target, OperationKind::RemoveIpv4(address(9)), &[]),
            op(&target, OperationKind::SetMtu(1500), &[]),
            op(&target, OperationKind::AddIpv4(address(1)), &[]),
            op(&target, OperationKind::SetMtu(1500), &[]),
            op(&target, OperationKind::SetMtu(9000), &[]),
            op(&target, OperationKind::SetEnabled(true), &[]),
        ],
    );
    assert!(report.is_success());
    assert_eq!(indices(&report.succeeded), [2, 4]);
    // The shared inventory, the add's verification, then one after the MTU write.
    assert_eq!(io.inventory_calls, 3);
}

#[test]
fn destructive_address_requests_need_complete_unchanged_group_preconditions() {
    let mut target = device(1);
    target.addresses.push(observed(address(1), false));
    let valid = group_op(&target, OperationKind::RemoveIpv4(address(1)), &[]);
    let mut foreign = address(1);
    foreign.ip = Ipv4Addr::new(198, 51, 100, 1);
    for case in 0..7 {
        let mut changed = target.clone();
        let mut operation = valid.clone();
        match case {
            0 => operation.address_group = None,
            1 => changed.addresses.push(observed(address(2), true)),
            2 => changed.addresses[0].scope = libc::RT_SCOPE_LINK,
            3 => changed.addresses[0].peer = Some(Ipv4Addr::new(192, 0, 2, 9)),
            4 => changed.addresses[0].flags ^= IFA_F_DEPRECATED,
            5 => changed.addresses[0].address.valid_lft = Some(3600),
            _ => operation
                .address_group
                .as_mut()
                .unwrap()
                .addresses
                .push(observed(foreign.clone(), false)),
        }
        let mut io = MockIo::new(vec![changed]);
        let report = apply(&mut io, vec![operation]);
        assert_eq!(
            failed_kinds(&report.failed),
            [BackendErrorKind::StaleState],
            "case {case}"
        );
        assert!(io.attempts.is_empty());
    }
}

#[test]
fn an_unguarded_addition_does_not_authorize_a_later_removal() {
    let mut target = device(1);
    target.addresses = vec![observed(address(1), false), observed(address(2), true)];
    for removal in [
        OperationKind::RemoveIpv4(address(2)),
        OperationKind::ReplaceIpv4(address(2)),
    ] {
        let mut io = MockIo::new(vec![target.clone()]);
        let report = apply(
            &mut io,
            vec![
                op(&target, OperationKind::AddIpv4(address(5)), &[]),
                op(&target, removal.clone(), &[]),
            ],
        );
        assert_eq!(indices(&report.succeeded), [0]);
        assert_eq!(
            failed_kinds(&report.failed),
            [BackendErrorKind::StaleState],
            "{removal:?}"
        );
        assert_eq!(io.attempts.len(), 1);
        assert!(
            io.observation.devices[0]
                .addresses
                .iter()
                .any(|entry| entry.address.same_identity(&address(2)))
        );
    }
}

#[test]
fn incomplete_address_inventory_blocks_addresses_but_allows_an_independent_mtu() {
    let mut target = device(1);
    target.addresses_complete = false;
    let mut io = MockIo::new(vec![target.clone()]);
    let report = apply(
        &mut io,
        vec![
            op(&target, OperationKind::AddIpv4(address(1)), &[]),
            op(&target, OperationKind::RemoveIpv4(address(2)), &[]),
            op(&target, OperationKind::ReplaceIpv4(address(3)), &[]),
            op(&target, OperationKind::SetMtu(1800), &[]),
        ],
    );
    assert_eq!(
        failed_kinds(&report.failed),
        [BackendErrorKind::QueryFailed; 3]
    );
    assert_eq!(indices(&report.succeeded), [3]);
    assert_eq!(io.attempts.len(), 1);
}

#[test]
fn inventory_failures_never_become_success() {
    let first = device(1);
    let second = device(2);
    for (fail_at, kind) in [
        (1, BackendErrorKind::QueryFailed),
        (2, BackendErrorKind::OutcomeUnknown),
    ] {
        let mut io = MockIo::new(vec![first.clone(), second.clone()]);
        io.fail_inventory_at = Some(fail_at);
        let report = apply(
            &mut io,
            vec![
                op(&first, OperationKind::AddIpv4(address(1)), &[]),
                op(&first, OperationKind::SetEnabled(false), &[0]),
                op(&second, OperationKind::SetMtu(1800), &[]),
            ],
        );
        assert!(!report.is_success());
        assert_eq!(failed_indices(&report.failed), [0]);
        assert_eq!(failed_kinds(&report.failed), [kind]);
        assert_eq!(
            report.skipped[0].reason,
            SkipReason::DependencyFailed {
                operations: vec![0]
            }
        );
        assert_eq!(indices(&report.succeeded), [2]);
        assert_eq!(
            io.observation.devices[0].addresses.len(),
            usize::from(fail_at == 2)
        );
        assert_eq!(
            io.observation.devices[0].state.fields["enabled"],
            Value::Bool(true)
        );
    }
}

#[test]
fn post_write_verification_rejects_unexpected_kernel_state() {
    let mut target = device(1);
    let mut other_prefix = address(7);
    other_prefix.ip = Ipv4Addr::new(198, 51, 100, 7);
    target.addresses = vec![observed(other_prefix.clone(), false)];
    let mut after = target.clone();
    after.addresses = vec![observed(other_prefix, false), observed(address(1), false)];
    let mut extra = after.clone();
    extra.addresses.push(observed(address(9), true));
    let mut lost_outside = after.clone();
    lost_outside.addresses.remove(0);
    let mut incomplete = after.clone();
    incomplete.addresses_complete = false;
    let mut replaced = after.clone();
    replaced.identity.generation += 1;
    for (observation, kind) in [
        (extra, BackendErrorKind::StaleState),
        (lost_outside, BackendErrorKind::StaleState),
        (incomplete, BackendErrorKind::OutcomeUnknown),
        (replaced, BackendErrorKind::StaleState),
    ] {
        let mut io = MockIo::new(vec![target.clone()]);
        io.inventory_override_at = Some((
            2,
            Observation {
                devices: vec![observation],
                diagnostics: Vec::new(),
            },
        ));
        let report = apply(
            &mut io,
            vec![op(&target, OperationKind::AddIpv4(address(1)), &[])],
        );
        assert_eq!(failed_kinds(&report.failed), [kind]);
        assert_eq!(io.attempts.len(), 1);
    }
}

#[test]
fn uncovered_primary_cascade_is_refused_before_any_write() {
    let mut target = device(1);
    target.addresses = vec![observed(address(1), false), observed(address(2), true)];
    let mut io = MockIo::new(vec![target.clone()]);
    io.cascade_primary = true;
    let report = apply(
        &mut io,
        vec![group_op(
            &target,
            OperationKind::RemoveIpv4(address(1)),
            &[],
        )],
    );
    assert_eq!(failed_kinds(&report.failed), [BackendErrorKind::StaleState]);
    assert!(io.attempts.is_empty());
    assert_eq!(io.observation.devices[0].addresses, target.addresses);
}

#[test]
fn a_secondary_in_another_scope_is_not_part_of_the_cascade() {
    let mut target = device(1);
    let mut link_scoped = observed(address(2), true);
    link_scoped.scope = libc::RT_SCOPE_LINK;
    target.addresses = vec![observed(address(1), false), link_scoped];
    let mut io = MockIo::new(vec![target.clone()]);
    let report = apply(
        &mut io,
        vec![group_op(
            &target,
            OperationKind::RemoveIpv4(address(1)),
            &[],
        )],
    );
    assert_eq!(indices(&report.succeeded), [0], "{report:?}");
}

#[test]
fn explicit_group_rebuild_handles_kernel_cascade_and_preserves_unrelated_prefix() {
    let mut target = device(1);
    let mut unrelated = address(77);
    unrelated.ip = Ipv4Addr::new(198, 51, 100, 77);
    let unrelated = observed(unrelated, false);
    target.addresses = vec![
        observed(address(1), false),
        observed(address(2), true),
        unrelated.clone(),
    ];
    let operations = vec![
        group_op(&target, OperationKind::RemoveIpv4(address(1)), &[]),
        group_op(&target, OperationKind::RemoveIpv4(address(2)), &[0]),
        group_op(&target, OperationKind::AddIpv4(address(2)), &[1]),
        group_op(&target, OperationKind::AddIpv4(address(1)), &[2]),
    ];
    let mut preview_io = MockIo::new(vec![target.clone()]);
    let report = preview(&mut preview_io, operations.clone());
    assert!(report.is_success());
    // Preview cannot read promote_secondaries, so it does not model the cascade
    // that makes the second removal already satisfied when applied.
    assert_eq!(statuses(&report), vec![PlannedStatus::Write; 4]);
    assert!(preview_io.attempts.is_empty());
    assert_eq!(
        preview_io.observation.devices[0].addresses,
        target.addresses
    );

    let mut io = MockIo::new(vec![target.clone()]);
    io.cascade_primary = true;
    let report = apply(&mut io, operations);
    assert!(report.is_success(), "{report:?}");
    assert_eq!(indices(&report.succeeded), [0, 2, 3]);
    assert_eq!(report.skipped[0].context.index, 1);
    assert_eq!(report.skipped[0].reason, SkipReason::AlreadySatisfied);
    assert_eq!(
        io.observation.devices[0].addresses,
        [
            unrelated,
            observed(address(2), false),
            observed(address(1), true)
        ]
    );
    assert_eq!(io.observation.devices[0].identity, target.identity);
    assert!(io.observation.devices[0].state.content_eq(&target.state));
}

#[test]
fn promoted_secondary_is_accepted_after_primary_removal() {
    let mut target = device(1);
    target.addresses = vec![observed(address(1), false), observed(address(2), true)];
    let mut io = MockIo::new(vec![target.clone()]);
    let report = apply(
        &mut io,
        vec![
            group_op(&target, OperationKind::RemoveIpv4(address(1)), &[]),
            group_op(&target, OperationKind::RemoveIpv4(address(2)), &[0]),
        ],
    );
    assert_eq!(indices(&report.succeeded), [0, 1], "{report:?}");
    assert!(io.observation.devices[0].addresses.is_empty());
}

#[test]
fn deprecated_addresses_can_be_added_and_replaced() {
    let target = device(1);
    let mut deprecated = address(1);
    deprecated.valid_lft = Some(120);
    deprecated.preferred_lft = Some(0);
    let mut io = MockIo::new(vec![target.clone()]);
    let report = apply(
        &mut io,
        vec![op(&target, OperationKind::AddIpv4(deprecated.clone()), &[])],
    );
    assert!(report.is_success(), "{report:?}");
    let before = io.observation.devices[0].clone();
    assert_eq!(
        before.addresses[0].flags & IFA_F_DEPRECATED,
        IFA_F_DEPRECATED
    );
    deprecated.preferred_lft = Some(60);
    let report = apply(
        &mut io,
        vec![group_op(
            &before,
            OperationKind::ReplaceIpv4(deprecated),
            &[],
        )],
    );
    assert!(report.is_success(), "{report:?}");
    assert_eq!(
        io.observation.devices[0].addresses[0].flags & IFA_F_DEPRECATED,
        0
    );
}

#[test]
fn projection_marks_addresses_deprecated_once_preferred_lifetime_runs_out() {
    let mut target = device(1);
    target.observed_at -= Duration::from_secs(2);
    let mut finite = address(1);
    finite.valid_lft = Some(10);
    finite.preferred_lft = Some(1);
    target.addresses = vec![observed(finite, false)];
    let now = target.observed_at + Duration::from_secs(2);
    rebase_lifetimes(&mut target, now);
    let entry = &target.addresses[0];
    assert_eq!(entry.address.valid_lft, Some(8));
    assert_eq!(entry.address.preferred_lft, Some(0));
    assert_eq!(entry.flags & IFA_F_DEPRECATED, IFA_F_DEPRECATED);
    assert_eq!(target.observed_at, now);
}

#[test]
fn fresh_group_drift_after_a_success_blocks_removal_without_widening_it() {
    let mut target = device(1);
    target.addresses = vec![observed(address(1), false)];
    let mut changed = target.clone();
    changed
        .addresses
        .extend([observed(address(2), true), observed(address(3), true)]);
    let mut io = MockIo::new(vec![target.clone()]);
    // Call 3 is the removal's inventory; the MTU write reuses call 2.
    io.inventory_override_at = Some((
        3,
        Observation {
            devices: vec![changed.clone()],
            diagnostics: Vec::new(),
        },
    ));
    let report = apply(
        &mut io,
        vec![
            group_op(&target, OperationKind::AddIpv4(address(2)), &[]),
            op(&target, OperationKind::SetMtu(1800), &[]),
            group_op(&target, OperationKind::RemoveIpv4(address(2)), &[0]),
        ],
    );
    assert_eq!(indices(&report.succeeded), [0, 1]);
    assert_eq!(failed_indices(&report.failed), [2]);
    assert_eq!(failed_kinds(&report.failed), [BackendErrorKind::StaleState]);
    assert_eq!(io.attempts.len(), 2);
    assert_eq!(io.observation.devices[0].addresses, changed.addresses);
}

#[test]
fn natural_lifetime_decay_and_deprecation_do_not_make_a_group_stale() {
    let mut original = device(1);
    original.observed_at -= Duration::from_secs(2);
    let mut finite = address(1);
    finite.valid_lft = Some(10);
    finite.preferred_lft = Some(1);
    original.addresses.push(observed(finite.clone(), false));
    let removal = group_op(&original, OperationKind::RemoveIpv4(finite.clone()), &[]);
    let mut current = original.clone();
    current.observed_at += Duration::from_secs(2);
    finite.valid_lft = Some(8);
    finite.preferred_lft = Some(0);
    current.addresses = vec![observed(finite, false)];
    let mut io = MockIo::new(vec![current]);
    let report = apply(&mut io, vec![removal]);
    assert!(report.is_success(), "{report:?}");
    assert_eq!(indices(&report.succeeded), [0]);
    assert!(io.observation.devices[0].addresses.is_empty());
}

#[test]
fn lifetime_tolerance_covers_truncation_but_not_a_changed_lifetime() {
    let ten = Duration::from_secs(10);
    let partial = Duration::from_millis(10_400);
    assert!(lifetime_matches(100, 90, ten));
    assert!(lifetime_matches(100, 91, ten));
    assert!(lifetime_matches(100, 89, ten));
    assert!(!lifetime_matches(100, 88, ten));
    assert!(!lifetime_matches(100, 92, ten));
    assert!(lifetime_matches(100, 88, partial));
    assert!(!lifetime_matches(100, 87, partial));
    assert!(lifetime_matches(5, 0, ten));
    assert!(!lifetime_matches(u32::MAX, 100, ten));
    assert!(!lifetime_matches(100, u32::MAX, ten));
    assert!(lifetime_matches(u32::MAX, u32::MAX, ten));

    let mut original = device(1);
    original.observed_at -= ten;
    let mut finite = address(1);
    finite.valid_lft = Some(100);
    original.addresses.push(observed(finite.clone(), false));
    let removal = group_op(&original, OperationKind::RemoveIpv4(finite.clone()), &[]);
    let mut current = original.clone();
    current.observed_at += ten;
    finite.valid_lft = Some(87);
    finite.preferred_lft = Some(87);
    current.addresses = vec![observed(finite, false)];
    let mut io = MockIo::new(vec![current]);
    let report = apply(&mut io, vec![removal]);
    assert_eq!(failed_kinds(&report.failed), [BackendErrorKind::StaleState]);
    assert!(io.attempts.is_empty());
}

#[test]
fn a_new_finite_address_lifetime_starts_at_the_write_not_the_earlier_inventory() {
    let mut target = device(1);
    target.observed_at -= Duration::from_secs(3);
    let mut finite = address(1);
    finite.valid_lft = Some(120);
    finite.preferred_lft = Some(60);
    let mut after = target.clone();
    after.observed_at = Instant::now();
    after.addresses = vec![observed(finite.clone(), false)];
    let mut io = MockIo::new(vec![target.clone()]);
    io.inventory_override_at = Some((
        2,
        Observation {
            devices: vec![after],
            diagnostics: Vec::new(),
        },
    ));
    let report = apply(
        &mut io,
        vec![op(&target, OperationKind::AddIpv4(finite), &[])],
    );
    assert!(report.is_success(), "{report:?}");
    assert_eq!(indices(&report.succeeded), [0]);
}

#[test]
fn a_dependency_blocked_secondary_removal_cannot_authorize_primary_cascade() {
    let mut target = device(1);
    target.addresses = vec![observed(address(1), false), observed(address(2), true)];
    let other = device(2);
    let mut io = MockIo::new(vec![target.clone(), other.clone()]);
    io.fail_execute_at = Some(1);
    io.cascade_primary = true;
    let report = apply(
        &mut io,
        vec![
            op(&other, OperationKind::SetMtu(1800), &[]),
            group_op(&target, OperationKind::RemoveIpv4(address(1)), &[]),
            group_op(&target, OperationKind::RemoveIpv4(address(2)), &[0]),
        ],
    );
    assert_eq!(failed_indices(&report.failed), [0, 1]);
    assert_eq!(report.skipped[0].context.index, 2);
    assert_eq!(
        report.skipped[0].reason,
        SkipReason::DependencyFailed {
            operations: vec![0]
        }
    );
    assert_eq!(io.attempts.len(), 1);
    assert_eq!(io.observation.devices[0].addresses, target.addresses);
}

#[test]
fn cancellation_preserves_completed_work_and_blocks_all_unattempted_writes() {
    let target = device(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut io = MockIo::new(vec![target.clone()]);
    io.cancel_after_write = Some(Arc::clone(&cancelled));
    let diff = plan(vec![
        op(&target, OperationKind::SetMtu(1800), &[]),
        op(&target, OperationKind::SetEnabled(false), &[]),
        op(&target, OperationKind::AddIpv4(address(1)), &[1]),
        op(&target, OperationKind::SetMtu(9000), &[]),
    ]);
    let report = apply_plan(&mut io, &diff, &|| cancelled.load(Ordering::SeqCst))
        .now_or_never()
        .unwrap();
    assert!(!report.is_success());
    assert_eq!(indices(&report.succeeded), [0]);
    assert_eq!(failed_indices(&report.failed), [1, 3]);
    assert_eq!(
        failed_kinds(&report.failed),
        [BackendErrorKind::ApplyFailed; 2]
    );
    assert_eq!(report.skipped[0].context.index, 2);
    assert_eq!(
        report.skipped[0].reason,
        SkipReason::DependencyFailed {
            operations: vec![1]
        }
    );
    assert_eq!(io.attempts.len(), 1);
    assert_eq!(
        io.observation.devices[0].state.fields["mtu"],
        Value::U64(1800)
    );
    assert_eq!(
        io.observation.devices[0].state.fields["enabled"],
        Value::Bool(true)
    );
    assert!(io.observation.devices[0].addresses.is_empty());
}

#[test]
fn cancellation_during_inventory_stops_before_the_write() {
    let target = device(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut io = MockIo::new(vec![target.clone()]);
    io.cancel_on_inventory = Some(Arc::clone(&cancelled));
    let report = apply_plan(
        &mut io,
        &plan(vec![op(&target, OperationKind::SetMtu(1800), &[])]),
        &|| cancelled.load(Ordering::SeqCst),
    )
    .now_or_never()
    .unwrap();
    assert_eq!(
        failed_kinds(&report.failed),
        [BackendErrorKind::ApplyFailed]
    );
    assert_eq!(io.inventory_calls, 1);
    assert!(io.attempts.is_empty());
}

#[test]
fn peer_group_cascade_cannot_remove_a_secondary_outside_the_local_group_plan() {
    let mut target = device(1);
    let mut primary_address = address(1);
    primary_address.ip = Ipv4Addr::new(10, 0, 0, 1);
    let mut secondary_address = address(1);
    secondary_address.ip = Ipv4Addr::new(10, 1, 0, 1);
    let mut primary = observed(primary_address.clone(), false);
    primary.peer = Some(Ipv4Addr::new(198, 51, 100, 1));
    let mut secondary = observed(secondary_address, true);
    secondary.peer = Some(Ipv4Addr::new(198, 51, 100, 2));
    target.addresses = vec![primary.clone(), secondary];
    let mut removal = op(&target, OperationKind::RemoveIpv4(primary_address), &[]);
    removal.address_group = Some(AddressGroupPrecondition {
        addresses: vec![primary],
        observed_at: target.observed_at,
    });
    let mut io = MockIo::new(vec![target.clone()]);
    let report = apply(&mut io, vec![removal.clone()]);
    assert_eq!(failed_kinds(&report.failed), [BackendErrorKind::StaleState]);
    let dry = preview(&mut io, vec![removal]);
    assert_eq!(failed_kinds(&dry.failed), [BackendErrorKind::StaleState]);
    assert!(io.attempts.is_empty());
    assert_eq!(io.observation.devices[0].addresses, target.addresses);
}

#[test]
fn a_one_second_lifetime_difference_without_a_timestamped_precondition_is_not_satisfied() {
    let mut target = device(1);
    let mut existing = address(1);
    existing.valid_lft = Some(120);
    existing.preferred_lft = Some(60);
    target.addresses = vec![observed(existing.clone(), false)];
    for valid_differs in [false, true] {
        let mut requested = existing.clone();
        if valid_differs {
            requested.valid_lft = Some(121);
        } else {
            requested.preferred_lft = Some(61);
        }
        let mut io = MockIo::new(vec![target.clone()]);
        let report = apply(
            &mut io,
            vec![op(&target, OperationKind::AddIpv4(requested), &[])],
        );
        assert_eq!(
            failed_kinds(&report.failed),
            [BackendErrorKind::ApplyFailed]
        );
        assert!(report.succeeded.is_empty());
        assert!(report.skipped.is_empty());
        assert!(io.attempts.is_empty());
        assert_eq!(io.observation.devices[0].addresses, target.addresses);
    }
}

#[test]
fn a_forever_lifetime_never_matches_a_finite_one() {
    let mut original = device(1);
    original.addresses.push(observed(address(1), false));
    let removal = group_op(&original, OperationKind::RemoveIpv4(address(1)), &[]);
    let mut finite = address(1);
    finite.valid_lft = Some(u32::MAX - 1);
    let mut current = original.clone();
    current.addresses = vec![AddressObservation {
        address: finite,
        ..original.addresses[0].clone()
    }];
    let mut io = MockIo::new(vec![current]);
    let report = apply(&mut io, vec![removal]);
    assert_eq!(failed_kinds(&report.failed), [BackendErrorKind::StaleState]);
}

#[test]
fn side_effects_and_observation_diagnostics_are_reported_once() {
    let target = device(1);
    let issue = Diagnostic {
        target: None,
        field: Some("type".into()),
        message: "fixture".into(),
    };
    for previewing in [false, true] {
        let mut io = MockIo::new(vec![target.clone()]);
        io.observation.diagnostics = vec![issue.clone()];
        let operations = vec![
            op(&target, OperationKind::SetMtu(1280), &[]),
            op(&target, OperationKind::SetMtu(1279), &[]),
            op(&target, OperationKind::SetEnabled(false), &[]),
        ];
        let diagnostics = if previewing {
            preview(&mut io, operations).diagnostics
        } else {
            apply(&mut io, operations).diagnostics
        };
        assert_eq!(diagnostics[0], issue);
        assert_eq!(
            diagnostics[1..]
                .iter()
                .map(|diagnostic| diagnostic.field.as_deref())
                .collect::<Vec<_>>(),
            [Some("mtu"), Some("enabled")]
        );
        assert!(
            diagnostics[1..]
                .iter()
                .all(|diagnostic| diagnostic.target.as_ref() == Some(&target.identity))
        );
    }
}

#[test]
fn an_unreachable_worker_fails_roots_and_blocks_dependents() {
    let target = device(1);
    let diff = StateDiff {
        diagnostics: vec![Diagnostic {
            target: None,
            field: None,
            message: "planned".into(),
        }],
        ..plan(vec![
            op(&target, OperationKind::SetMtu(1800), &[]),
            op(&target, OperationKind::SetEnabled(false), &[0]),
        ])
    };
    let failure = BackendError::new(BackendErrorKind::OutcomeUnknown, "worker stopped");
    let report = failed_report(&diff, failure.clone());
    assert_eq!(failed_indices(&report.failed), [0]);
    assert_eq!(report.failed[0].error, failure);
    assert_eq!(
        report.skipped[0].reason,
        SkipReason::DependencyFailed {
            operations: vec![0]
        }
    );
    assert_eq!(report.diagnostics, diff.diagnostics);
    let dry = preview_report(&diff, report);
    assert_eq!(
        statuses(&dry),
        [
            PlannedStatus::Fail,
            PlannedStatus::Skip(SkipReason::DependencyFailed {
                operations: vec![0]
            })
        ]
    );
}
