use std::collections::{BTreeMap, BTreeSet};

use netfyr_state::plan::{DeviceObservation, Observation};

use crate::{BindingConflict, ConflictingPolicy, Contribution, UnmatchedPolicy};

pub(super) struct BoundDevice<'a> {
    pub observation: &'a DeviceObservation,
    /// The contributions whose own selector matched this device, in
    /// precedence order (FR-002: every matching contribution is retained
    /// through the selector that matched).
    pub contributions: Vec<&'a Contribution>,
}

pub(super) struct Binding<'a> {
    pub devices: Vec<BoundDevice<'a>>,
    pub conflicts: Vec<BindingConflict>,
    pub unmatched: Vec<UnmatchedPolicy>,
}

pub(super) fn bind<'a>(
    contributions: &'a [Contribution],
    observation: &'a Observation,
) -> Binding<'a> {
    let mut ordered: Vec<_> = contributions.iter().collect();
    ordered.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left.key.cmp(&right.key))
    });
    let mut devices = Vec::new();
    let mut conflicts = Vec::new();
    let mut matched = BTreeSet::new();
    for device in &observation.devices {
        let matches: Vec<_> = ordered
            .iter()
            .filter(|contribution| contribution.match_spec.matches(&device.state.match_spec))
            .copied()
            .collect();
        // The distinct policies matching this device, in precedence order.
        let mut policies: Vec<&str> = Vec::new();
        for contribution in &matches {
            if !policies.contains(&contribution.policy.as_str()) {
                policies.push(contribution.policy.as_str());
            }
        }
        match policies.as_slice() {
            [] => {}
            [_] => {
                matched.extend(matches.iter().map(|contribution| contribution.key.as_str()));
                devices.push(BoundDevice {
                    observation: device,
                    contributions: matches,
                });
            }
            _ => {
                matched.extend(matches.iter().map(|contribution| contribution.key.as_str()));
                policies.sort_unstable();
                conflicts.push(BindingConflict {
                    target: device.identity.clone(),
                    policies: policies
                        .into_iter()
                        .map(|name| ConflictingPolicy {
                            // The selector that actually matched, not the
                            // policy's highest-precedence one.
                            selector: matches
                                .iter()
                                .find(|contribution| contribution.policy == name)
                                .expect("a listed policy matched this device")
                                .match_spec
                                .clone(),
                            name: name.to_string(),
                        })
                        .collect(),
                });
            }
        }
    }
    devices.sort_by_key(|device| {
        (
            device.observation.identity.namespace.device,
            device.observation.identity.namespace.inode,
            device.observation.identity.index,
        )
    });
    conflicts.sort_by_key(|conflict| {
        (
            conflict.target.namespace.device,
            conflict.target.namespace.inode,
            conflict.target.index,
        )
    });
    // A policy may have both matching and unmatched selectors. Report
    // each distinct unmatched selector once, even when static and
    // provider contributions share it.
    let mut unmatched_by_policy: BTreeMap<&str, Vec<UnmatchedPolicy>> = BTreeMap::new();
    for contribution in ordered {
        if matched.contains(contribution.key.as_str()) {
            continue;
        }
        let unmatched = unmatched_by_policy
            .entry(contribution.policy.as_str())
            .or_default();
        if !unmatched
            .iter()
            .any(|entry| entry.selector == contribution.match_spec)
        {
            unmatched.push(UnmatchedPolicy {
                policy: contribution.policy.clone(),
                selector: contribution.match_spec.clone(),
            });
        }
    }
    Binding {
        devices,
        conflicts,
        unmatched: unmatched_by_policy.into_values().flatten().collect(),
    }
}
