use std::net::{IpAddr, Ipv4Addr};
use std::time::Instant;

use indexmap::IndexMap;
use netfyr_backend::{BackendError, BackendFuture, DryRunReport, NetworkBackend};
use netfyr_reconcile::{
    ConflictIdentity, ConflictPolicy, Contribution, Diagnostic, MergeOptions, ReconcileError,
    reconcile, render_json, render_text,
};
use netfyr_state::plan::{
    AddressObservation, DeviceObservation, InterfaceId, Ipv4Address, NamespaceId, Observation,
    OperationKind, StateDiff,
};
use netfyr_state::{Match, SchemaRegistry, Source, State, Value};

fn interface_named(name: &str, index: u32) -> InterfaceId {
    InterfaceId {
        namespace: NamespaceId {
            device: 1,
            inode: 2,
        },
        index,
        generation: 3,
        name: name.into(),
        device_type: "ethernet".into(),
        mac: Some("02:00:00:00:00:01".into()),
    }
}

fn interface() -> InterfaceId {
    interface_named("eth0", 7)
}

fn device_observation(
    name: &str,
    index: u32,
    addresses: Vec<AddressObservation>,
    complete: bool,
) -> DeviceObservation {
    let mut state = State::new(Source::Kernel);
    state.device_type = "ethernet".into();
    state.match_spec = Match {
        name: Some(name.into()),
        r#type: Some("ethernet".into()),
        driver: Some("ixgbe".into()),
        mac: Some("02:00:00:00:00:01".into()),
        ..Match::default()
    };
    state.fields.insert("mtu".into(), Value::U64(1500));
    DeviceObservation {
        identity: interface_named(name, index),
        state,
        addresses,
        addresses_complete: complete,
        observed_at: Instant::now(),
    }
}

fn observation_of(devices: Vec<DeviceObservation>) -> Observation {
    Observation {
        devices,
        diagnostics: Vec::new(),
    }
}

fn observation(addresses: Vec<AddressObservation>, complete: bool) -> Observation {
    observation_of(vec![device_observation("eth0", 7, addresses, complete)])
}

fn contribution(fields: IndexMap<String, Value>) -> Contribution {
    Contribution::new(
        "p",
        "p",
        Source::Static { policy: "p".into() },
        "ethernet",
        Match {
            name: Some("eth0".into()),
            ..Match::default()
        },
        fields,
        100,
    )
}

fn address(ip: Ipv4Addr, valid: Option<u64>, preferred: Option<u64>) -> Value {
    let mut map = IndexMap::new();
    map.insert("ip".into(), Value::IpNetwork((IpAddr::V4(ip), 24)));
    if let Some(valid) = valid {
        map.insert("valid_lft".into(), Value::U64(valid));
    }
    if let Some(preferred) = preferred {
        map.insert("preferred_lft".into(), Value::U64(preferred));
    }
    Value::Map(map)
}

fn address_fields(addresses: Vec<Value>) -> IndexMap<String, Value> {
    let mut ipv4 = IndexMap::new();
    ipv4.insert("addresses".into(), Value::List(addresses));
    let mut fields = IndexMap::new();
    fields.insert("ipv4".into(), Value::Map(ipv4));
    fields
}

#[test]
fn plans_exact_identity_and_backend_operations() {
    let mut fields = address_fields(vec![address(Ipv4Addr::new(192, 0, 2, 10), None, None)]);
    fields.insert("mtu".into(), Value::U64(9000));
    let plan = reconcile(
        &[contribution(fields)],
        &observation(Vec::new(), true),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    assert!(
        plan.diff()
            .operations
            .iter()
            .all(|operation| operation.target == interface())
    );
    assert!(matches!(
        plan.diff().operations[0].kind,
        OperationKind::SetMtu(9000)
    ));
    assert!(matches!(
        plan.diff().operations[1].kind,
        OperationKind::AddIpv4(_)
    ));
    assert_eq!(
        plan.diff().operations[1]
            .address_group
            .as_ref()
            .unwrap()
            .addresses,
        Vec::<AddressObservation>::new()
    );
}

#[test]
fn contributions_bind_through_their_own_selectors() {
    // One policy, two contributions with distinct selectors: each device
    // receives only the contribution whose own selector matched it
    // (FR-002), not every contribution of the bound policy.
    let mut eth0_fields = IndexMap::new();
    eth0_fields.insert("mtu".into(), Value::U64(9000));
    let mut eth1_fields = IndexMap::new();
    eth1_fields.insert("mtu".into(), Value::U64(1400));
    let contributions = vec![
        Contribution::new(
            "p",
            "p:eth0",
            Source::Static { policy: "p".into() },
            "ethernet",
            Match {
                name: Some("eth0".into()),
                ..Match::default()
            },
            eth0_fields,
            100,
        ),
        Contribution::new(
            "p",
            "p:eth1",
            Source::Static { policy: "p".into() },
            "ethernet",
            Match {
                name: Some("eth1".into()),
                ..Match::default()
            },
            eth1_fields,
            100,
        ),
    ];
    let plan = reconcile(
        &contributions,
        &observation_of(vec![
            device_observation("eth0", 7, Vec::new(), true),
            device_observation("eth1", 8, Vec::new(), true),
        ]),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    let mtu_by_device: Vec<(&str, &Value)> = plan
        .diff()
        .changes
        .iter()
        .map(|change| {
            (
                change.target.name.as_str(),
                change.desired.as_ref().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        mtu_by_device,
        vec![("eth0", &Value::U64(9000)), ("eth1", &Value::U64(1400))]
    );
    for operation in &plan.diff().operations {
        match operation.target.index {
            7 => assert!(matches!(operation.kind, OperationKind::SetMtu(9000))),
            8 => assert!(matches!(operation.kind, OperationKind::SetMtu(1400))),
            _ => panic!("operation targeted an unexpected device"),
        }
    }
}

#[test]
fn unmatched_selector_reports_policy_without_inventing_devices() {
    // US1-4: a selector with no matching device produces no operation and
    // reports the unmatched policy; it does not invent a device.
    let mut fields = IndexMap::new();
    fields.insert("mtu".into(), Value::U64(9000));
    let contribution = Contribution::new(
        "q",
        "q",
        Source::Static { policy: "q".into() },
        "ethernet",
        Match {
            name: Some("eth9".into()),
            ..Match::default()
        },
        fields,
        100,
    );
    let plan = reconcile(
        &[contribution],
        &observation(Vec::new(), true),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    assert!(plan.diff().changes.is_empty());
    assert!(plan.diff().operations.is_empty());
    assert!(matches!(
        &plan.diagnostics[..],
        [Diagnostic::UnmatchedPolicy(unmatched)]
            if unmatched.policy == "q"
                && unmatched.selector.name.as_deref() == Some("eth9")
    ));
}

#[test]
fn unmatched_sibling_selector_is_reported() {
    let mut fields = IndexMap::new();
    fields.insert("mtu".into(), Value::U64(9000));
    let mut matched = contribution(fields);
    matched.key = "p:eth0".into();
    let mut unmatched = matched.clone();
    unmatched.key = "p:eth9".into();
    unmatched.match_spec.name = Some("eth9".into());
    for contributions in [
        vec![matched.clone(), unmatched.clone()],
        vec![unmatched, matched],
    ] {
        let plan = reconcile(
            &contributions,
            &observation(Vec::new(), true),
            &SchemaRegistry::new(),
            &MergeOptions::default(),
        )
        .unwrap();
        assert_eq!(plan.diff().operations.len(), 1);
        assert_eq!(plan.diff().operations[0].target, interface());
        assert!(matches!(
            plan.diagnostics.as_slice(),
            [Diagnostic::UnmatchedPolicy(unmatched)]
                if unmatched.policy == "p"
                    && unmatched.selector.name.as_deref() == Some("eth9")
        ));
        assert_eq!(
            render_json(&plan)["diagnostics"][0]["selector"]["name"],
            "eth9"
        );
        let message = "policy p selector name=eth9 matched no interfaces";
        assert!(render_text(&plan).contains(message));
        assert_eq!(plan.diff().diagnostics[0].message, message);
    }
}

#[test]
fn unmatched_selectors_are_distinct_and_deduplicated_per_policy() {
    let mut fields = IndexMap::new();
    fields.insert("mtu".into(), Value::U64(9000));
    let mut first = contribution(fields);
    first.key = "p:eth8".into();
    first.match_spec.name = Some("eth8".into());
    let mut second = first.clone();
    second.key = "p:eth9".into();
    second.match_spec.name = Some("eth9".into());
    let mut duplicate = second.clone();
    duplicate.key = "p:provider".into();
    duplicate.source = Source::Dhcp { policy: "p".into() };
    duplicate.priority = 50;
    let mut other_policy = second.clone();
    other_policy.policy = "q".into();
    other_policy.key = "q".into();
    other_policy.source = Source::Static { policy: "q".into() };
    let contributions = vec![first, second, duplicate, other_policy];
    for contributions in [
        contributions.clone(),
        contributions.into_iter().rev().collect(),
    ] {
        let plan = reconcile(
            &contributions,
            &observation_of(Vec::new()),
            &SchemaRegistry::new(),
            &MergeOptions::default(),
        )
        .unwrap();
        assert!(plan.diff().operations.is_empty());
        let selectors: Vec<_> = plan
            .diagnostics
            .iter()
            .map(|diagnostic| match diagnostic {
                Diagnostic::UnmatchedPolicy(unmatched) => (
                    unmatched.policy.as_str(),
                    unmatched.selector.name.as_deref().unwrap(),
                ),
                _ => panic!("expected an unmatched-selector diagnostic"),
            })
            .collect();
        assert_eq!(selectors, vec![("p", "eth8"), ("p", "eth9"), ("q", "eth9")]);
    }
}

#[test]
fn differently_named_policies_matching_one_device_both_contribute() {
    // US1-3 / FR-002: a name-selected and a MAC-selected policy that both
    // identify eth0 contribute to the same device; disjoint fields from
    // both are retained instead of one policy rejecting the other.
    let mut mtu_fields = IndexMap::new();
    mtu_fields.insert("mtu".into(), Value::U64(9000));
    let mut enabled_fields = IndexMap::new();
    enabled_fields.insert("enabled".into(), Value::Bool(false));
    let contributions = vec![
        Contribution::new(
            "mtu-policy",
            "mtu-policy",
            Source::Static {
                policy: "mtu-policy".into(),
            },
            "ethernet",
            Match {
                name: Some("eth0".into()),
                ..Match::default()
            },
            mtu_fields,
            100,
        ),
        Contribution::new(
            "enabled-policy",
            "enabled-policy",
            Source::Static {
                policy: "enabled-policy".into(),
            },
            "ethernet",
            Match {
                mac: Some("02:00:00:00:00:01".into()),
                ..Match::default()
            },
            enabled_fields,
            100,
        ),
    ];
    let plan = reconcile(
        &contributions,
        &observation(Vec::new(), true),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    let changed: Vec<(&str, &Value)> = plan
        .diff()
        .changes
        .iter()
        .map(|change| (change.field.as_str(), change.desired.as_ref().unwrap()))
        .collect();
    assert_eq!(
        changed,
        vec![("enabled", &Value::Bool(false)), ("mtu", &Value::U64(9000))]
    );
    assert!(plan.diagnostics.is_empty());
    assert!(plan.diff().diagnostics.is_empty());
}

#[test]
fn cross_policy_field_conflict_resolves_or_rejects() {
    // FR-002 / FR-012: two policies configuring the same field of one
    // device are per-field value conflicts decided by precedence, not
    // binding errors.
    let mut high_fields = IndexMap::new();
    high_fields.insert("mtu".into(), Value::U64(9000));
    let mut low_fields = IndexMap::new();
    low_fields.insert("mtu".into(), Value::U64(1400));
    let contributions = vec![
        Contribution::new(
            "a",
            "a",
            Source::Static { policy: "a".into() },
            "ethernet",
            Match {
                name: Some("eth0".into()),
                ..Match::default()
            },
            high_fields,
            100,
        ),
        Contribution::new(
            "b",
            "b",
            Source::Static { policy: "b".into() },
            "ethernet",
            Match {
                name: Some("eth0".into()),
                ..Match::default()
            },
            low_fields,
            100,
        ),
    ];
    let plan = reconcile(
        &contributions,
        &observation(Vec::new(), true),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    assert!(matches!(
        &plan.diagnostics[..],
        [Diagnostic::ValueConflict(conflict)]
            if matches!(conflict.identity, ConflictIdentity::Field { ref path } if path == "mtu")
                && conflict.winner.source == "a"
    ));
    assert_eq!(
        plan.diff().changes[0].desired.as_ref().unwrap(),
        &Value::U64(9000)
    );
    let error = reconcile(
        &contributions,
        &observation(Vec::new(), true),
        &SchemaRegistry::new(),
        &MergeOptions {
            conflict_policy: ConflictPolicy::Reject,
        },
    )
    .unwrap_err();
    assert!(matches!(error, ReconcileError::ValueConflicts(_)));
}

#[test]
fn equal_priority_breaks_ties_by_selector_specificity() {
    // US2-3: equal-priority conflicting MTUs selected by driver and by
    // name: the name-selected contribution wins, even when its key sorts
    // after the driver-selected one.
    let mut driver_fields = IndexMap::new();
    driver_fields.insert("mtu".into(), Value::U64(9000));
    let mut name_fields = IndexMap::new();
    name_fields.insert("mtu".into(), Value::U64(1400));
    let driver = Contribution::new(
        "a",
        "a",
        Source::Static { policy: "a".into() },
        "ethernet",
        Match {
            driver: Some("ixgbe".into()),
            ..Match::default()
        },
        driver_fields,
        100,
    );
    let name = Contribution::new(
        "z",
        "z",
        Source::Static { policy: "z".into() },
        "ethernet",
        Match {
            name: Some("eth0".into()),
            ..Match::default()
        },
        name_fields,
        100,
    );
    for contributions in [vec![driver.clone(), name.clone()], vec![name, driver]] {
        let plan = reconcile(
            &contributions,
            &observation(Vec::new(), true),
            &SchemaRegistry::new(),
            &MergeOptions::default(),
        )
        .unwrap();
        assert_eq!(
            plan.diff().changes[0].desired.as_ref().unwrap(),
            &Value::U64(1400),
            "the name-selected contribution must win at equal priority"
        );
        assert!(matches!(
            &plan.diagnostics[..],
            [Diagnostic::ValueConflict(conflict)] if conflict.winner.source == "z"
        ));
    }
}

#[test]
fn equal_priority_equal_specificity_breaks_ties_by_policy_name() {
    // US2-4: equal-priority, equally specific policies: the policy name
    // breaks the tie before the contribution key, regardless of input
    // order.
    fn policy(name: &str, key: &str, mtu: u64) -> Contribution {
        let mut fields = IndexMap::new();
        fields.insert("mtu".into(), Value::U64(mtu));
        Contribution::new(
            name,
            key,
            Source::Static {
                policy: name.into(),
            },
            "ethernet",
            Match {
                name: Some("eth0".into()),
                ..Match::default()
            },
            fields,
            100,
        )
    }
    // `aaa` sorts before `zzz` by policy name even though its key sorts
    // after `zzz`'s.
    for contributions in [
        vec![policy("zzz", "a", 9000), policy("aaa", "z", 1400)],
        vec![policy("aaa", "z", 1400), policy("zzz", "a", 9000)],
    ] {
        let plan = reconcile(
            &contributions,
            &observation(Vec::new(), true),
            &SchemaRegistry::new(),
            &MergeOptions::default(),
        )
        .unwrap();
        assert_eq!(
            plan.diff().changes[0].desired.as_ref().unwrap(),
            &Value::U64(1400),
            "the policy named `aaa` must win the tie"
        );
        assert!(matches!(
            &plan.diagnostics[..],
            [Diagnostic::ValueConflict(conflict)] if conflict.winner.source == "z"
        ));
    }
}

#[test]
fn priority_beats_selector_specificity() {
    // US2-2: a higher explicit priority wins regardless of selector
    // specificity.
    let mut name_fields = IndexMap::new();
    name_fields.insert("mtu".into(), Value::U64(9000));
    let mut driver_fields = IndexMap::new();
    driver_fields.insert("mtu".into(), Value::U64(1400));
    let contributions = vec![
        Contribution::new(
            "a",
            "a",
            Source::Static { policy: "a".into() },
            "ethernet",
            Match {
                name: Some("eth0".into()),
                ..Match::default()
            },
            name_fields,
            100,
        ),
        Contribution::new(
            "z",
            "z",
            Source::Static { policy: "z".into() },
            "ethernet",
            Match {
                driver: Some("ixgbe".into()),
                ..Match::default()
            },
            driver_fields,
            200,
        ),
    ];
    let plan = reconcile(
        &contributions,
        &observation(Vec::new(), true),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    assert_eq!(
        plan.diff().changes[0].desired.as_ref().unwrap(),
        &Value::U64(1400),
        "the higher-priority contribution must win"
    );
}

#[test]
fn managed_addresses_require_complete_inventory_and_valid_lifetimes() {
    let fields = address_fields(vec![address(Ipv4Addr::new(192, 0, 2, 10), None, None)]);
    assert!(matches!(
        reconcile(
            &[contribution(fields.clone())],
            &observation(Vec::new(), false),
            &SchemaRegistry::new(),
            &MergeOptions::default()
        ),
        Err(ReconcileError::IncompleteAddressInventory { .. })
    ));
    let visible = AddressObservation {
        address: Ipv4Address {
            ip: Ipv4Addr::new(192, 0, 2, 10),
            prefix_len: 24,
            valid_lft: None,
            preferred_lft: None,
        },
        flags: 0,
        scope: 0,
        peer: None,
    };
    assert!(matches!(
        reconcile(
            &[contribution(fields)],
            &observation(vec![visible], false),
            &SchemaRegistry::new(),
            &MergeOptions::default()
        ),
        Err(ReconcileError::IncompleteAddressInventory { .. })
    ));
    let invalid = address_fields(vec![address(Ipv4Addr::new(192, 0, 2, 10), Some(0), None)]);
    assert!(matches!(
        reconcile(
            &[contribution(invalid)],
            &observation(Vec::new(), true),
            &SchemaRegistry::new(),
            &MergeOptions::default()
        ),
        Err(ReconcileError::InvalidAddress { .. })
    ));
}

#[test]
fn omitted_forever_lifetimes_do_not_replace_observed_forever_address() {
    let actual = Ipv4Address {
        ip: Ipv4Addr::new(192, 0, 2, 10),
        prefix_len: 24,
        valid_lft: Some(u32::MAX),
        preferred_lft: Some(u32::MAX),
    };
    let observed = AddressObservation {
        address: actual,
        flags: 0,
        scope: 0,
        peer: None,
    };
    let plan = reconcile(
        &[contribution(address_fields(vec![address(
            Ipv4Addr::new(192, 0, 2, 10),
            None,
            None,
        )]))],
        &observation(vec![observed], true),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    assert!(plan.diff().operations.is_empty());
}

#[test]
fn group_operations_have_snapshot_and_a_complete_dependency_chain() {
    let first = Ipv4Address {
        ip: Ipv4Addr::new(192, 0, 2, 10),
        prefix_len: 24,
        valid_lft: None,
        preferred_lft: None,
    };
    let second = Ipv4Address {
        ip: Ipv4Addr::new(192, 0, 2, 11),
        prefix_len: 24,
        valid_lft: None,
        preferred_lft: None,
    };
    let observed = vec![
        AddressObservation {
            address: second.clone(),
            flags: 0,
            scope: 0,
            peer: None,
        },
        AddressObservation {
            address: first.clone(),
            flags: 0,
            scope: 0,
            peer: None,
        },
    ];
    let plan = reconcile(
        &[contribution(address_fields(vec![
            address(first.ip, None, None),
            address(second.ip, None, None),
        ]))],
        &observation(observed, true),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    let operations = &plan.diff().operations;
    assert_eq!(operations.len(), 4);
    assert!(operations[0].address_group.is_some());
    assert_eq!(operations[1].depends_on, vec![0]);
    assert_eq!(operations[2].depends_on, vec![1]);
    assert_eq!(operations[3].depends_on, vec![2]);
}

#[test]
fn address_dependencies_account_for_prepended_scalar_operations() {
    // A scalar change is planned before the address operations, so the
    // address chain must be offset by it: the second removal depends on
    // the first removal, not on the mtu assignment (failure propagation
    // and the backend's cascade checks walk these edges).
    let first = Ipv4Address {
        ip: Ipv4Addr::new(192, 0, 2, 10),
        prefix_len: 24,
        valid_lft: None,
        preferred_lft: None,
    };
    let second = Ipv4Address {
        ip: Ipv4Addr::new(192, 0, 2, 11),
        prefix_len: 24,
        valid_lft: None,
        preferred_lft: None,
    };
    let observed = vec![
        AddressObservation {
            address: second.clone(),
            flags: 0,
            scope: 0,
            peer: None,
        },
        AddressObservation {
            address: first.clone(),
            flags: 0,
            scope: 0,
            peer: None,
        },
    ];
    let mut fields = address_fields(vec![
        address(first.ip, None, None),
        address(second.ip, None, None),
    ]);
    fields.insert("mtu".into(), Value::U64(9000));
    let plan = reconcile(
        &[contribution(fields)],
        &observation(observed, true),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    let operations = &plan.diff().operations;
    assert_eq!(operations.len(), 5);
    assert!(matches!(operations[0].kind, OperationKind::SetMtu(9000)));
    assert!(matches!(operations[1].kind, OperationKind::RemoveIpv4(_)));
    assert!(operations[1].address_group.is_some());
    assert_eq!(operations[2].depends_on, vec![1]);
    assert_eq!(operations[3].depends_on, vec![2]);
    assert_eq!(operations[4].depends_on, vec![3]);
    assert!(operations[2..].iter().all(|operation| {
        operation
            .depends_on
            .iter()
            .all(|dependency| *dependency >= 1)
    }));
}

#[test]
fn invalid_duplicate_addresses_are_rejected_before_merge() {
    let ip = Ipv4Addr::new(192, 0, 2, 10);
    let overflow = u64::from(u32::MAX) + 1;
    for invalid in [
        address(ip, Some(0), None),
        address(ip, Some(overflow), None),
        address(ip, Some(3600), Some(3601)),
        address(ip, None, Some(overflow)),
    ] {
        for conflict_policy in [ConflictPolicy::Resolve, ConflictPolicy::Reject] {
            let options = MergeOptions { conflict_policy };
            let inputs = vec![contribution(address_fields(vec![
                address(ip, None, None),
                invalid.clone(),
            ]))];
            let high = contribution(address_fields(vec![address(ip, None, None)]));
            let low = Contribution::new(
                "p",
                "p:provider",
                Source::Dhcp { policy: "p".into() },
                "ethernet",
                high.match_spec.clone(),
                address_fields(vec![invalid.clone()]),
                50,
            );
            for contributions in [inputs, vec![high, low]] {
                let result = reconcile(
                    &contributions,
                    &observation(Vec::new(), true),
                    &SchemaRegistry::new(),
                    &options,
                );
                assert!(
                    matches!(&result, Err(ReconcileError::InvalidAddress { target, .. })
                        if **target == interface()),
                    "invalid duplicate {invalid:?} must fail in {conflict_policy:?} mode: {result:?}"
                );
            }
        }
    }
}

#[test]
fn duplicate_addresses_within_one_contribution_collapse_to_one() {
    // FR-004: the ordered union keeps the first occurrence of each
    // element, also when a single policy lists the same CIDR twice: one
    // copy in the desired state, one add operation, no conflict.
    let fields = address_fields(vec![
        address(Ipv4Addr::new(192, 0, 2, 10), None, None),
        address(Ipv4Addr::new(192, 0, 2, 10), None, None),
    ]);
    let plan = reconcile(
        &[contribution(fields)],
        &observation(Vec::new(), true),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    let additions = plan
        .diff()
        .operations
        .iter()
        .filter(|operation| matches!(operation.kind, OperationKind::AddIpv4(_)))
        .count();
    assert_eq!(additions, 1);
    assert!(plan.diagnostics.is_empty());
}

#[test]
fn duplicate_addresses_with_different_attributes_conflict_within_one_contribution() {
    // FR-012: duplicate-address attributes are structured conflicts; the
    // first occurrence supplies the complete winning address object.
    let fields = address_fields(vec![
        address(Ipv4Addr::new(192, 0, 2, 10), None, None),
        address(Ipv4Addr::new(192, 0, 2, 10), Some(3600), None),
    ]);
    let plan = reconcile(
        &[contribution(fields)],
        &observation(Vec::new(), true),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    assert!(matches!(
        &plan.diagnostics[..],
        [Diagnostic::ValueConflict(conflict)]
            if matches!(conflict.identity, ConflictIdentity::Address { .. })
                && conflict.competing.len() == 2
    ));
    assert!(
        plan.diff()
            .operations
            .iter()
            .all(|operation| match &operation.kind {
                OperationKind::AddIpv4(address) => address.valid_lft.is_none(),
                _ => true,
            })
    );
}

#[test]
fn explicit_empty_address_list_attributes_removals_to_its_contribution() {
    // FR-005: an explicit empty address list has no first element, yet
    // the removals it causes must carry the contributing policy's source
    // instead of falling back to kernel provenance.
    let observed = vec![AddressObservation {
        address: Ipv4Address {
            ip: Ipv4Addr::new(192, 0, 2, 10),
            prefix_len: 24,
            valid_lft: None,
            preferred_lft: None,
        },
        flags: 0,
        scope: 0,
        peer: None,
    }];
    let plan = reconcile(
        &[contribution(address_fields(vec![]))],
        &observation(observed, true),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    let removals = plan
        .diff()
        .operations
        .iter()
        .filter(|operation| matches!(operation.kind, OperationKind::RemoveIpv4(_)))
        .count();
    assert_eq!(removals, 1);
    let change = &plan.diff().changes[0];
    assert_eq!(change.field, "ipv4.addresses");
    assert_eq!(change.source, Source::Static { policy: "p".into() });
    assert_eq!(plan.provenance[0].source.as_deref(), Some("p"));
}

#[test]
fn empty_higher_precedence_list_keeps_winning_address_source() {
    let high = contribution(address_fields(vec![]));
    let low = Contribution::new(
        "p",
        "p:provider",
        Source::Dhcp { policy: "p".into() },
        "ethernet",
        high.match_spec.clone(),
        address_fields(vec![address(Ipv4Addr::new(192, 0, 2, 10), None, None)]),
        50,
    );
    for contributions in [vec![high.clone(), low.clone()], vec![low, high]] {
        let plan = reconcile(
            &contributions,
            &observation(Vec::new(), true),
            &SchemaRegistry::new(),
            &MergeOptions::default(),
        )
        .unwrap();
        assert_eq!(plan.diff().changes.len(), 1);
        assert_eq!(
            plan.diff().changes[0].source,
            Source::Dhcp { policy: "p".into() }
        );
        assert_eq!(plan.provenance[0].source.as_deref(), Some("p:provider"));
        assert_eq!(
            plan.provenance[0].element_sources.as_deref(),
            Some(&["p:provider".to_string()][..])
        );
        assert!(render_text(&plan).contains("(source: p:provider)"));
    }
}

#[test]
fn rejects_peer_or_scope_groups_that_backend_cannot_rebuild_safely() {
    let actual = Ipv4Address {
        ip: Ipv4Addr::new(192, 0, 2, 10),
        prefix_len: 24,
        valid_lft: None,
        preferred_lft: None,
    };
    let observed = AddressObservation {
        address: actual,
        flags: 0,
        scope: 0,
        peer: Some(Ipv4Addr::new(198, 51, 100, 10)),
    };
    assert!(matches!(
        reconcile(
            &[contribution(address_fields(vec![]))],
            &observation(vec![observed], true),
            &SchemaRegistry::new(),
            &MergeOptions::default(),
        ),
        Err(ReconcileError::UnsafeAddressGroup { .. })
    ));
}

#[test]
fn keeps_structured_conflict_and_mirrors_it_to_backend_diagnostics() {
    let mut high_fields = IndexMap::new();
    high_fields.insert("mtu".into(), Value::U64(9000));
    let mut low_fields = IndexMap::new();
    low_fields.insert("mtu".into(), Value::U64(1400));
    let high = contribution(high_fields);
    let low = Contribution::new(
        "p",
        "p:provider",
        Source::Dhcp { policy: "p".into() },
        "ethernet",
        Match {
            name: Some("eth0".into()),
            ..Match::default()
        },
        low_fields,
        50,
    );
    let plan = reconcile(
        &[high, low],
        &observation(Vec::new(), true),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    assert!(matches!(
        &plan.diagnostics[..],
        [Diagnostic::ValueConflict(conflict)]
            if matches!(conflict.identity, ConflictIdentity::Field { ref path } if path == "mtu")
    ));
    assert_eq!(plan.diff().diagnostics.len(), 1);
    assert_eq!(plan.provenance[0].source.as_deref(), Some("p"));
}

struct DryRunBackend;
impl NetworkBackend for DryRunBackend {
    fn namespace(&self) -> NamespaceId {
        interface().namespace
    }
    fn supported_entities(&self) -> &'static [&'static str] {
        &["ethernet"]
    }
    fn query<'a>(&'a self, _: &'a Match) -> BackendFuture<'a, Result<Observation, BackendError>> {
        Box::pin(async { Ok(observation(Vec::new(), true)) })
    }
    fn apply<'a>(&'a self, _: &'a StateDiff) -> BackendFuture<'a, netfyr_backend::ApplyReport> {
        Box::pin(async { unreachable!() })
    }
    fn dry_run<'a>(&'a self, diff: &'a StateDiff) -> BackendFuture<'a, DryRunReport> {
        Box::pin(async move {
            DryRunReport {
                changes: diff.changes.clone(),
                ..DryRunReport::default()
            }
        })
    }
}

#[tokio::test]
async fn reconciliation_output_passes_directly_to_backend_dry_run() {
    let mut fields = IndexMap::new();
    fields.insert("mtu".into(), Value::U64(9000));
    let plan = reconcile(
        &[contribution(fields)],
        &observation(Vec::new(), true),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    let report = DryRunBackend.dry_run(plan.diff()).await;
    assert_eq!(report.changes, plan.diff().changes);
}
