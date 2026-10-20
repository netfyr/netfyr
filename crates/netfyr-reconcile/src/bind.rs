use std::collections::{BTreeMap, BTreeSet};

use netfyr_state::Match;
use netfyr_state::plan::{DeviceObservation, Observation};

use crate::{Contribution, UnmatchedPolicy};

pub(super) struct BoundDevice<'a> {
    pub observation: &'a DeviceObservation,
    /// Every contribution whose own selector matched this device, from any
    /// policy, in precedence order (FR-002: every matching contribution is
    /// retained; several policies may configure one device).
    pub contributions: Vec<&'a Contribution>,
}

pub(super) struct Binding<'a> {
    pub devices: Vec<BoundDevice<'a>>,
    pub unmatched: Vec<UnmatchedPolicy>,
}

/// FR-003's selector-specificity rank: the most specific field a selector
/// sets, `name` > `mac` > `pci_path` > `driver` > `type` in that order;
/// an empty selector is least specific.
fn specificity(match_spec: &Match) -> u8 {
    if match_spec.name.is_some() {
        5
    } else if match_spec.mac.is_some() {
        4
    } else if match_spec.pci_path.is_some() {
        3
    } else if match_spec.driver.is_some() {
        2
    } else if match_spec.r#type.is_some() {
        1
    } else {
        0
    }
}

pub(super) fn bind<'a>(
    contributions: &'a [Contribution],
    observation: &'a Observation,
) -> Binding<'a> {
    let mut ordered: Vec<_> = contributions.iter().collect();
    // FR-003: explicit priority first, higher first; equal priorities
    // compare the most specific selector field, then the policy name in
    // bytewise lexical order, then a stable contribution key.
    ordered.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| specificity(&right.match_spec).cmp(&specificity(&left.match_spec)))
            .then_with(|| left.policy.cmp(&right.policy))
            .then_with(|| left.key.cmp(&right.key))
    });
    let mut devices = Vec::new();
    let mut matched = BTreeSet::new();
    for device in &observation.devices {
        let matches: Vec<_> = ordered
            .iter()
            .filter(|contribution| contribution.match_spec.matches(&device.state.match_spec))
            .copied()
            .collect();
        if matches.is_empty() {
            continue;
        }
        matched.extend(matches.iter().map(|contribution| contribution.key.as_str()));
        devices.push(BoundDevice {
            observation: device,
            contributions: matches,
        });
    }
    devices.sort_by_key(|device| {
        (
            device.observation.identity.namespace.device,
            device.observation.identity.namespace.inode,
            device.observation.identity.index,
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
        unmatched: unmatched_by_policy.into_values().flatten().collect(),
    }
}
