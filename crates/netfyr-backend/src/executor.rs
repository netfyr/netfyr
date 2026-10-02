use std::collections::HashMap;
use std::future::Future;
use std::net::Ipv4Addr;
use std::pin::Pin;
use std::time::{Duration, Instant};

use libc::{IFA_F_DEPRECATED, IFA_F_PERMANENT, IFA_F_SECONDARY, RT_SCOPE_HOST};
use netfyr_state::plan::{
    AddressObservation, DeviceObservation, Diagnostic, InterfaceId, Ipv4Address, NamespaceId,
    Observation, Operation, OperationKind, StateDiff,
};
use netfyr_state::{SchemaRegistry, Source, State, Value};

use crate::linux::LinuxTransport;
use crate::{
    ApplyReport, BackendError, BackendErrorKind, DryRunReport, ETHERNET, FailedOperation,
    OperationContext, PlannedOperation, PlannedStatus, SkipReason, SkippedOperation,
};

type IoFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

pub(crate) trait Io {
    fn namespace(&self) -> NamespaceId;
    fn inventory(&mut self) -> IoFuture<'_, Result<Observation, BackendError>>;
    fn execute<'a>(
        &'a mut self,
        target: &'a InterfaceId,
        kind: &'a OperationKind,
    ) -> IoFuture<'a, Result<(), BackendError>>;
}

impl Io for LinuxTransport {
    fn namespace(&self) -> NamespaceId {
        LinuxTransport::namespace(self)
    }
    fn inventory(&mut self) -> IoFuture<'_, Result<Observation, BackendError>> {
        Box::pin(LinuxTransport::inventory(self, |_| false))
    }
    fn execute<'a>(
        &'a mut self,
        target: &'a InterfaceId,
        kind: &'a OperationKind,
    ) -> IoFuture<'a, Result<(), BackendError>> {
        Box::pin(LinuxTransport::execute(self, target, kind))
    }
}

pub(crate) fn failed_report(diff: &StateDiff, failure: BackendError) -> ApplyReport {
    let mut report = ApplyReport {
        diagnostics: diff.diagnostics.clone(),
        ..ApplyReport::default()
    };
    for (index, op) in diff.operations.iter().enumerate() {
        let context = OperationContext::new(index, op);
        if op.depends_on.is_empty() {
            report.failed.push(FailedOperation {
                context,
                error: failure.clone(),
            });
        } else {
            report.skipped.push(SkippedOperation {
                context,
                reason: SkipReason::DependencyFailed {
                    operations: op.depends_on.clone(),
                },
            });
        }
    }
    report
}

pub(crate) fn preview_report(diff: &StateDiff, report: ApplyReport) -> DryRunReport {
    let mut operations: Vec<_> = report
        .succeeded
        .into_iter()
        .map(|context| PlannedOperation {
            context,
            status: PlannedStatus::Write,
        })
        .chain(report.skipped.into_iter().map(|entry| PlannedOperation {
            context: entry.context,
            status: PlannedStatus::Skip(entry.reason),
        }))
        .chain(report.failed.iter().map(|entry| PlannedOperation {
            context: entry.context.clone(),
            status: PlannedStatus::Fail,
        }))
        .collect();
    operations.sort_by_key(|entry| entry.context.index);
    DryRunReport {
        changes: diff.changes.clone(),
        operations,
        failed: report.failed,
        diagnostics: report.diagnostics,
    }
}

pub(crate) async fn apply_plan(
    io: &mut impl Io,
    diff: &StateDiff,
    cancelled: &dyn Fn() -> bool,
) -> ApplyReport {
    run(io, diff, false, cancelled).await
}

pub(crate) async fn preview_plan(io: &mut impl Io, diff: &StateDiff) -> DryRunReport {
    let report = run(io, diff, true, &|| false).await;
    preview_report(diff, report)
}

// `None` is a read-only field assignment, which is skipped rather than written.
fn decode(
    kind: &OperationKind,
    schema: &SchemaRegistry,
) -> Result<Option<OperationKind>, BackendError> {
    let decoded = match kind {
        OperationKind::SetField { field, value } => {
            let (fragment, name) = field.split_once('.').unwrap_or(("base", field.as_str()));
            let info = schema.field_info(fragment, name).ok_or_else(|| {
                BackendError::new(
                    BackendErrorKind::UnsupportedOperation,
                    format!("unknown field {field}"),
                )
            })?;
            if !info.writable {
                return Ok(None);
            }
            match (field.as_str(), value) {
                ("mtu", Value::U64(mtu)) => {
                    OperationKind::SetMtu(u32::try_from(*mtu).map_err(|_| {
                        BackendError::new(
                            BackendErrorKind::UnsupportedOperation,
                            "MTU overflows u32",
                        )
                    })?)
                }
                ("enabled", Value::Bool(enabled)) => OperationKind::SetEnabled(*enabled),
                _ => {
                    return Err(BackendError::new(
                        BackendErrorKind::UnsupportedOperation,
                        format!(
                            "unsupported assignment to {field}; use explicit address operations"
                        ),
                    ));
                }
            }
        }
        OperationKind::AddEntity | OperationKind::RemoveEntity => {
            return Err(BackendError::new(
                BackendErrorKind::UnsupportedOperation,
                "whole-interface lifecycle operations are not supported",
            ));
        }
        other => other.clone(),
    };
    match &decoded {
        OperationKind::SetMtu(mtu) => {
            let mut state = State::new(Source::Kernel);
            state
                .fields
                .insert("mtu".into(), Value::U64(u64::from(*mtu)));
            let errors = schema.validate_writable(&state);
            if !errors.is_empty() {
                return Err(BackendError::new(
                    BackendErrorKind::UnsupportedOperation,
                    errors
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; "),
                ));
            }
        }
        OperationKind::AddIpv4(addr) | OperationKind::ReplaceIpv4(addr) => {
            addr.validate().map_err(|reason| {
                BackendError::new(BackendErrorKind::UnsupportedOperation, reason.to_string())
            })?;
        }
        OperationKind::RemoveIpv4(addr) if addr.group().is_none() => {
            return Err(BackendError::new(
                BackendErrorKind::UnsupportedOperation,
                "IPv4 prefix length exceeds 32",
            ));
        }
        _ => {}
    }
    Ok(Some(decoded))
}

fn locate(
    observation: &Observation,
    identity: &InterfaceId,
    namespace: NamespaceId,
) -> Result<usize, BackendError> {
    if identity.namespace != namespace {
        return Err(BackendError::new(
            BackendErrorKind::StaleState,
            "plan namespace differs from backend namespace",
        ));
    }
    let position = observation
        .devices
        .iter()
        .position(|device| device.identity.index == identity.index)
        .ok_or_else(|| {
            BackendError::new(
                BackendErrorKind::NotFound,
                format!(
                    "interface {} (index {}) disappeared",
                    identity.name, identity.index
                ),
            )
        })?;
    let device = &observation.devices[position];
    if device.identity != *identity {
        return Err(BackendError::new(
            BackendErrorKind::StaleState,
            format!(
                "interface {} was replaced or its identity changed",
                identity.name
            ),
        ));
    }
    if device.identity.device_type != ETHERNET {
        return Err(BackendError::new(
            BackendErrorKind::UnsupportedEntityType,
            format!(
                "{} is {}, which this backend cannot manage",
                identity.name, device.identity.device_type
            ),
        ));
    }
    Ok(position)
}

fn whole_seconds(duration: Duration) -> u32 {
    duration.as_secs().min(u64::from(u32::MAX)) as u32
}

fn decay(seconds: u32, elapsed: u32) -> u32 {
    if seconds == u32::MAX {
        seconds
    } else {
        seconds.saturating_sub(elapsed)
    }
}

fn lifetime_matches(expected: u32, actual: u32, elapsed: Duration) -> bool {
    if expected == u32::MAX || actual == u32::MAX {
        return expected == actual;
    }
    // Kernel lifetimes are whole seconds sampled during the dump, shortly
    // before observed_at: allow one second beyond the truncated elapsed range.
    let floor = i64::from(whole_seconds(elapsed));
    let ceil = floor + i64::from(elapsed.subsec_nanos() > 0);
    let consumed = i64::from(expected) - i64::from(actual);
    (floor - 1..=ceil + 1).contains(&consumed) || (actual == 0 && i64::from(expected) <= ceil + 1)
}

fn address_matches(
    expected: &AddressObservation,
    actual: &AddressObservation,
    elapsed: Duration,
) -> bool {
    let (valid, preferred) = expected.address.lifetimes();
    let (actual_valid, actual_preferred) = actual.address.lifetimes();
    let deprecation_matches = expected.flags & IFA_F_DEPRECATED == actual.flags & IFA_F_DEPRECATED
        || (actual_preferred == 0 && lifetime_matches(preferred, 0, elapsed));
    expected.address.same_identity(&actual.address)
        && expected.scope == actual.scope
        && expected.peer == actual.peer
        && (expected.flags & !IFA_F_DEPRECATED) == (actual.flags & !IFA_F_DEPRECATED)
        && deprecation_matches
        && lifetime_matches(valid, actual_valid, elapsed)
        && lifetime_matches(preferred, actual_preferred, elapsed)
}

#[derive(Clone)]
struct ExpectedGroup {
    addresses: Vec<AddressObservation>,
    observed_at: Instant,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct GroupKey {
    target: InterfaceId,
    network: Ipv4Addr,
    prefix_len: u8,
}

impl GroupKey {
    fn new(op: &Operation, addr: &Ipv4Address) -> Result<Self, BackendError> {
        let (network, prefix_len) = addr.group().ok_or_else(|| {
            BackendError::new(BackendErrorKind::UnsupportedOperation, "invalid prefix")
        })?;
        Ok(Self {
            target: op.target.clone(),
            network,
            prefix_len,
        })
    }

    fn contains(&self, address: &Ipv4Address) -> bool {
        address.group() == Some((self.network, self.prefix_len))
    }
}

fn group_addresses(device: &DeviceObservation, key: &GroupKey) -> Vec<AddressObservation> {
    device
        .addresses
        .iter()
        .filter(|entry| key.contains(&entry.address))
        .cloned()
        .collect()
}

fn group_matches(expected: &ExpectedGroup, actual: &[AddressObservation], now: Instant) -> bool {
    let elapsed = now.saturating_duration_since(expected.observed_at);
    expected.addresses.len() == actual.len()
        && expected
            .addresses
            .iter()
            .zip(actual)
            .all(|(old, new)| address_matches(old, new, elapsed))
}

// Only a caller precondition establishes a group, so removing or replacing an
// existing address needs one on this operation or an earlier one.
fn validate_group(
    op: &Operation,
    device: &DeviceObservation,
    kind: &OperationKind,
    groups: &mut HashMap<GroupKey, ExpectedGroup>,
) -> Result<(), BackendError> {
    let Some(addr) = kind.ipv4_address() else {
        return Ok(());
    };
    if !device.addresses_complete {
        return Err(BackendError::new(
            BackendErrorKind::QueryFailed,
            "IPv4 inventory is incomplete",
        ));
    }
    let key = GroupKey::new(op, addr)?;
    if let Some(precondition) = &op.address_group {
        if precondition
            .addresses
            .iter()
            .any(|entry| !key.contains(&entry.address))
        {
            return Err(BackendError::new(
                BackendErrorKind::StaleState,
                "address precondition describes a different prefix group",
            ));
        }
        groups.entry(key.clone()).or_insert_with(|| ExpectedGroup {
            addresses: precondition.addresses.clone(),
            observed_at: precondition.observed_at,
        });
    }
    match groups.get(&key) {
        Some(expected) => {
            if !group_matches(expected, &group_addresses(device, &key), device.observed_at) {
                return Err(BackendError::new(
                    BackendErrorKind::StaleState,
                    "address group membership, ordering or attributes changed; obtain a fresh plan",
                ));
            }
        }
        None if matches!(
            kind,
            OperationKind::RemoveIpv4(_) | OperationKind::ReplaceIpv4(_)
        ) && device
            .addresses
            .iter()
            .any(|entry| entry.address.same_identity(addr)) =>
        {
            return Err(BackendError::new(
                BackendErrorKind::StaleState,
                "destructive address operation requires a complete group precondition",
            ));
        }
        None => {}
    }
    Ok(())
}

fn satisfied(
    kind: &OperationKind,
    device: &DeviceObservation,
    op: &Operation,
) -> Result<bool, BackendError> {
    match kind {
        OperationKind::SetMtu(mtu) => {
            Ok(device.state.fields.get("mtu") == Some(&Value::U64(u64::from(*mtu))))
        }
        OperationKind::SetEnabled(enabled) => {
            Ok(device.state.fields.get("enabled") == Some(&Value::Bool(*enabled)))
        }
        OperationKind::RemoveIpv4(addr) => Ok(!device
            .addresses
            .iter()
            .any(|entry| entry.address.same_identity(addr))),
        OperationKind::AddIpv4(addr) | OperationKind::ReplaceIpv4(addr) => {
            let Some(existing) = device
                .addresses
                .iter()
                .find(|entry| entry.address.same_identity(addr))
            else {
                if matches!(kind, OperationKind::ReplaceIpv4(_)) {
                    return Err(BackendError::new(
                        BackendErrorKind::NotFound,
                        "address replacement requires an existing address",
                    ));
                }
                return Ok(false);
            };
            let (valid, preferred) = addr.lifetimes();
            let (actual_valid, actual_preferred) = existing.address.lifetimes();
            let elapsed = op.address_group.as_ref().and_then(|group| {
                group
                    .addresses
                    .iter()
                    .any(|entry| entry.address == *addr)
                    .then(|| {
                        device
                            .observed_at
                            .saturating_duration_since(group.observed_at)
                    })
            });
            let lifetimes_equal = match elapsed {
                Some(elapsed) => {
                    lifetime_matches(valid, actual_valid, elapsed)
                        && lifetime_matches(preferred, actual_preferred, elapsed)
                }
                None => valid == actual_valid && preferred == actual_preferred,
            };
            if lifetimes_equal {
                Ok(true)
            } else if matches!(kind, OperationKind::AddIpv4(_)) {
                Err(BackendError::new(
                    BackendErrorKind::ApplyFailed,
                    "address exists with different lifetimes; an explicit replacement is required",
                ))
            } else {
                Ok(false)
            }
        }
        _ => Ok(false),
    }
}

fn removal_is_covered(
    diff: &StateDiff,
    index: usize,
    target: &InterfaceId,
    addr: &Ipv4Address,
    eligible: &[bool],
) -> bool {
    let mut runnable = eligible[..index].to_vec();
    runnable.push(true);
    for (position, operation) in diff.operations.iter().enumerate().skip(index + 1) {
        runnable.push(
            operation
                .depends_on
                .iter()
                .all(|dependency| *dependency < position && runnable[*dependency]),
        );
    }
    diff.operations
        .iter()
        .enumerate()
        .skip(index + 1)
        .any(|(position, op)| {
            runnable[position]
                && op.target == *target
                && matches!(&op.kind, OperationKind::RemoveIpv4(other) if addr.same_identity(other))
        })
}

// The kernel deletes or promotes a primary's secondaries; each must be removed
// later in the plan, and within the primary's local prefix group.
fn check_cascade(
    diff: &StateDiff,
    index: usize,
    device: &DeviceObservation,
    kind: &OperationKind,
    eligible: &[bool],
) -> Result<(), BackendError> {
    let OperationKind::RemoveIpv4(addr) = kind else {
        return Ok(());
    };
    let Some(primary) = device
        .addresses
        .iter()
        .find(|entry| entry.address.same_identity(addr) && entry.flags & IFA_F_SECONDARY == 0)
    else {
        return Ok(());
    };
    let uncovered = device.addresses.iter().any(|secondary| {
        secondary.flags & IFA_F_SECONDARY != 0
            && (secondary.kernel_group(), secondary.scope)
                == (primary.kernel_group(), primary.scope)
            && (secondary.address.group() != addr.group()
                || !removal_is_covered(diff, index, &device.identity, &secondary.address, eligible))
    });
    if uncovered {
        return Err(BackendError::new(
            BackendErrorKind::StaleState,
            "primary removal could delete a secondary outside its executable prefix-group plan; remove peer-group secondaries first",
        ));
    }
    Ok(())
}

fn lifetime_flags(addr: &Ipv4Address) -> u32 {
    let (valid, preferred) = addr.lifetimes();
    (if valid == u32::MAX {
        IFA_F_PERMANENT
    } else {
        0
    }) | if preferred == 0 { IFA_F_DEPRECATED } else { 0 }
}

fn project(device: &mut DeviceObservation, kind: &OperationKind) {
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
        OperationKind::RemoveIpv4(addr) => device
            .addresses
            .retain(|entry| !entry.address.same_identity(addr)),
        OperationKind::AddIpv4(addr) => {
            let secondary = device
                .addresses
                .iter()
                .any(|entry| entry.kernel_group() == addr.group());
            let scope = if addr.ip.is_loopback() {
                RT_SCOPE_HOST
            } else {
                0
            };
            device.addresses.push(AddressObservation {
                address: addr.clone(),
                flags: lifetime_flags(addr) | if secondary { IFA_F_SECONDARY } else { 0 },
                scope,
                peer: None,
            });
        }
        OperationKind::ReplaceIpv4(addr) => {
            if let Some(existing) = device
                .addresses
                .iter_mut()
                .find(|entry| entry.address.same_identity(addr))
            {
                existing.address = addr.clone();
                existing.flags =
                    existing.flags & !(IFA_F_PERMANENT | IFA_F_DEPRECATED) | lifetime_flags(addr);
            }
        }
        _ => {}
    }
}

fn rebase_lifetimes(device: &mut DeviceObservation, now: Instant) {
    let elapsed = whole_seconds(now.saturating_duration_since(device.observed_at));
    for entry in &mut device.addresses {
        let (valid, preferred) = entry.address.lifetimes();
        entry.address.valid_lft = Some(decay(valid, elapsed));
        entry.address.preferred_lft = Some(decay(preferred, elapsed));
        if entry.address.preferred_lft == Some(0) {
            entry.flags |= IFA_F_DEPRECATED;
        }
    }
    device.observed_at = now;
}

fn collateral(device: &DeviceObservation, kind: &OperationKind) -> Option<Diagnostic> {
    let message = match kind {
        OperationKind::SetMtu(mtu) if *mtu < 1280 => {
            "an MTU below 1280 disables IPv6 on the device and removes its IPv6 addresses"
        }
        OperationKind::SetEnabled(false) => {
            "disabling the device removes its IPv4 routes and, unless keep_addr_on_down is set, its IPv6 addresses"
        }
        _ => return None,
    };
    Some(Diagnostic {
        target: Some(device.identity.clone()),
        field: kind.field().map(str::to_owned),
        message: message.into(),
    })
}

fn unconfirmed(message: &str, cause: BackendError) -> BackendError {
    let mut error = BackendError::new(
        BackendErrorKind::OutcomeUnknown,
        format!("{message}: {}", cause.message),
    );
    error.errno = cause.errno;
    error.extack = cause.extack;
    error
}

fn cancelled_error(message: &str) -> BackendError {
    BackendError::new(BackendErrorKind::ApplyFailed, message)
}

struct Execution<'a, I: Io> {
    io: &'a mut I,
    diff: &'a StateDiff,
    preview: bool,
    namespace: NamespaceId,
    schema: SchemaRegistry,
    groups: HashMap<GroupKey, ExpectedGroup>,
    eligible: Vec<bool>,
    // Valid until the next write; preview projects onto it instead.
    current: Option<Observation>,
    diagnostics: Vec<Diagnostic>,
}

impl<I: Io> Execution<'_, I> {
    fn absorb(&mut self, observation: &Observation) {
        for diagnostic in &observation.diagnostics {
            if !self.diagnostics.contains(diagnostic) {
                self.diagnostics.push(diagnostic.clone());
            }
        }
    }

    async fn observe(&mut self) -> Result<&mut Observation, BackendError> {
        let observation = match self.current.take() {
            Some(observation) => observation,
            None => {
                let observation = self.io.inventory().await?;
                self.absorb(&observation);
                observation
            }
        };
        Ok(self.current.insert(observation))
    }

    async fn step(
        &mut self,
        index: usize,
        op: &Operation,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<SkipReason>, BackendError> {
        if cancelled() {
            return Err(cancelled_error(
                "plan execution cancelled; earlier writes may have applied",
            ));
        }
        let kind = decode(&op.kind, &self.schema)?;
        let namespace = self.namespace;
        let observation = self.observe().await?;
        let position = locate(observation, &op.target, namespace)?;
        let device = observation.devices[position].clone();
        let Some(kind) = kind else {
            return Ok(Some(SkipReason::ReadOnly));
        };
        validate_group(op, &device, &kind, &mut self.groups)?;
        if satisfied(&kind, &device, op)? {
            return Ok(Some(SkipReason::AlreadySatisfied));
        }
        check_cascade(self.diff, index, &device, &kind, &self.eligible)?;
        let mut predicted = device.clone();
        if self.preview {
            rebase_lifetimes(&mut predicted, Instant::now());
            project(&mut predicted, &kind);
            if let Some(observation) = self.current.as_mut() {
                observation.devices[position] = predicted.clone();
            }
        } else {
            if cancelled() {
                return Err(cancelled_error("plan execution cancelled before write"));
            }
            self.current = None;
            self.io.execute(&op.target, &kind).await?;
            rebase_lifetimes(&mut predicted, Instant::now());
            project(&mut predicted, &kind);
            if let Some(addr) = kind.ipv4_address() {
                predicted = self
                    .verify_address_write(index, op, &kind, addr, predicted)
                    .await?;
            }
        }
        if let Some(diagnostic) = collateral(&device, &kind) {
            self.diagnostics.push(diagnostic);
        }
        let key = kind
            .ipv4_address()
            .map(|addr| GroupKey::new(op, addr))
            .transpose()?;
        if let Some(key) = key.filter(|key| self.groups.contains_key(key)) {
            let addresses = group_addresses(&predicted, &key);
            self.groups.insert(
                key,
                ExpectedGroup {
                    addresses,
                    observed_at: predicted.observed_at,
                },
            );
        }
        Ok(None)
    }

    async fn verify_address_write(
        &mut self,
        index: usize,
        op: &Operation,
        kind: &OperationKind,
        addr: &Ipv4Address,
        predicted: DeviceObservation,
    ) -> Result<DeviceObservation, BackendError> {
        let after = self
            .io
            .inventory()
            .await
            .map_err(|error| unconfirmed("write acknowledged but not re-observed", error))?;
        self.absorb(&after);
        let actual = &after.devices[locate(&after, &op.target, self.namespace)?];
        if !actual.addresses_complete {
            return Err(BackendError::new(
                BackendErrorKind::OutcomeUnknown,
                "write acknowledged but the address inventory is incomplete",
            ));
        }
        let key = GroupKey::new(op, addr)?;
        let mut expected = group_addresses(&predicted, &key);
        let actual_group = group_addresses(actual, &key);
        if matches!(kind, OperationKind::RemoveIpv4(_)) {
            expected.retain(|entry| {
                let absent = !actual_group
                    .iter()
                    .any(|other| entry.address.same_identity(&other.address));
                !(absent
                    && entry.flags & IFA_F_SECONDARY != 0
                    && removal_is_covered(
                        self.diff,
                        index,
                        &op.target,
                        &entry.address,
                        &self.eligible,
                    ))
            });
            // With promote_secondaries the surviving first member becomes primary.
            if let (Some(first), Some(actual_first)) = (expected.first_mut(), actual_group.first())
            {
                if first.address.same_identity(&actual_first.address)
                    && actual_first.flags & IFA_F_SECONDARY == 0
                {
                    first.flags &= !IFA_F_SECONDARY;
                }
            }
        }
        let expected = ExpectedGroup {
            addresses: expected,
            observed_at: predicted.observed_at,
        };
        let outside_survives = predicted
            .addresses
            .iter()
            .filter(|entry| !key.contains(&entry.address))
            .all(|entry| {
                actual
                    .addresses
                    .iter()
                    .any(|other| entry.address.same_identity(&other.address))
            });
        if !group_matches(&expected, &actual_group, actual.observed_at) || !outside_survives {
            return Err(BackendError::new(
                BackendErrorKind::StaleState,
                "addresses changed unexpectedly during the write; earlier changes may have applied",
            ));
        }
        let actual = actual.clone();
        self.current = Some(after);
        Ok(actual)
    }
}

async fn run(
    io: &mut impl Io,
    diff: &StateDiff,
    preview: bool,
    cancelled: &dyn Fn() -> bool,
) -> ApplyReport {
    let namespace = io.namespace();
    let mut execution = Execution {
        io,
        diff,
        preview,
        namespace,
        schema: SchemaRegistry::new(),
        groups: HashMap::new(),
        eligible: vec![false; diff.operations.len()],
        current: None,
        diagnostics: diff.diagnostics.clone(),
    };
    let mut report = ApplyReport::default();
    for (index, op) in diff.operations.iter().enumerate() {
        let context = OperationContext::new(index, op);
        if op.depends_on.iter().any(|dependency| *dependency >= index) {
            report.failed.push(FailedOperation {
                context,
                error: BackendError::new(
                    BackendErrorKind::UnsupportedOperation,
                    "dependencies must identify earlier operation indices",
                ),
            });
            continue;
        }
        let blocked: Vec<_> = op
            .depends_on
            .iter()
            .copied()
            .filter(|dependency| !execution.eligible[*dependency])
            .collect();
        if !blocked.is_empty() {
            report.skipped.push(SkippedOperation {
                context,
                reason: SkipReason::DependencyFailed {
                    operations: blocked,
                },
            });
            continue;
        }
        match execution.step(index, op, cancelled).await {
            Ok(skip) => {
                execution.eligible[index] = true;
                match skip {
                    Some(reason) => report.skipped.push(SkippedOperation { context, reason }),
                    None => report.succeeded.push(context),
                }
            }
            Err(error) => report.failed.push(FailedOperation { context, error }),
        }
    }
    report.diagnostics = execution.diagnostics;
    report
}

#[cfg(test)]
mod tests;
