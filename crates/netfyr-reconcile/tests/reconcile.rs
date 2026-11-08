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
    address_with_ip(Value::IpNetwork((IpAddr::V4(ip), 24)), valid, preferred)
}

fn address_with_ip(ip: Value, valid: Option<u64>, preferred: Option<u64>) -> Value {
    let mut map = IndexMap::new();
    map.insert("ip".into(), ip);
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
fn every_changed_writable_leaf_is_planned_as_an_operation() {
    // FR-007 / FR-008: a writable leaf without a typed operation must not
    // appear in the preview as a change that apply never attempts. It is
    // forwarded as a generic assignment for the backend to accept or
    // reject explicitly.
    let mut registry = SchemaRegistry::new();
    registry
        .add_schema_node(
            "base",
            "foo",
            r#"{"type":"integer","x-netfyr-writable":true}"#,
        )
        .unwrap();
    let mut fields = IndexMap::new();
    fields.insert("foo".into(), Value::U64(5));
    fields.insert("mtu".into(), Value::U64(9000));
    let plan = reconcile(
        &[contribution(fields)],
        &observation(Vec::new(), true),
        &registry,
        &MergeOptions::default(),
    )
    .unwrap();
    let changed: Vec<_> = plan
        .diff()
        .changes
        .iter()
        .map(|change| change.field.as_str())
        .collect();
    assert_eq!(changed, ["foo", "mtu"]);
    let kinds: Vec<_> = plan
        .diff()
        .operations
        .iter()
        .map(|operation| &operation.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            &OperationKind::SetField {
                field: "foo".into(),
                value: Value::U64(5),
            },
            &OperationKind::SetMtu(9000),
        ]
    );
    let text = render_text(&plan);
    let lines: Vec<_> = text.lines().collect();
    assert!(lines[0].starts_with("2/eth0 foo: "), "{text}");
    assert_eq!(lines[1], "  #0 set_field 5");
    assert_eq!(
        render_json(&plan)["operations"][0]["arguments"],
        serde_json::json!({ "value": 5 })
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
        Err(ReconcileError::InvalidContributionAddress { .. })
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
            for (contributions, invalid_key) in [(inputs, "p"), (vec![high, low], "p:provider")] {
                let result = reconcile(
                    &contributions,
                    &observation(Vec::new(), true),
                    &SchemaRegistry::new(),
                    &options,
                );
                assert!(
                    matches!(&result, Err(ReconcileError::InvalidContributionAddress { key, .. })
                        if key == invalid_key),
                    "invalid duplicate {invalid:?} must fail in {conflict_policy:?} mode: {result:?}"
                );
            }
        }
    }
}

#[test]
fn invalid_addresses_in_unmatched_contributions_are_rejected() {
    // FR-007 / FR-012: invalid input is an error whether or not its
    // selector binds. A contribution matching no device must not let a
    // sibling's mtu change through as an executable plan; the error names
    // the contribution key, as there is no device to name.
    let ip = Ipv4Addr::new(192, 0, 2, 10);
    let overflow = u64::from(u32::MAX) + 1;
    let mut mtu_fields = IndexMap::new();
    mtu_fields.insert("mtu".into(), Value::U64(9000));
    for invalid in [
        address(ip, Some(0), None),
        address(ip, Some(overflow), None),
        address(ip, Some(3600), Some(3601)),
    ] {
        let unmatched = Contribution::new(
            "q",
            "q",
            Source::Static { policy: "q".into() },
            "ethernet",
            Match {
                name: Some("eth9".into()),
                ..Match::default()
            },
            address_fields(vec![address(ip, None, None), invalid.clone()]),
            100,
        );
        let result = reconcile(
            &[contribution(mtu_fields.clone()), unmatched],
            &observation(Vec::new(), true),
            &SchemaRegistry::new(),
            &MergeOptions::default(),
        );
        assert!(
            matches!(&result, Err(ReconcileError::InvalidContributionAddress { key, message })
                if key == "q" && message.starts_with("ipv4.addresses[1]: ")),
            "invalid {invalid:?} in an unmatched contribution must fail: {result:?}"
        );
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
fn string_cidr_addresses_preserve_host_bits_and_prefix_identity() {
    let addresses = ["192.0.2.10/24", "192.0.2.11/24", "192.0.2.10/25"]
        .into_iter()
        .map(|text| address_with_ip(Value::String(text.into()), None, None))
        .collect();
    let plan = reconcile(
        &[contribution(address_fields(addresses))],
        &observation(Vec::new(), true),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    let mut identities: Vec<_> = plan
        .diff()
        .operations
        .iter()
        .map(|operation| match &operation.kind {
            OperationKind::AddIpv4(address) => (address.ip, address.prefix_len),
            other => panic!("expected an address addition, got {other:?}"),
        })
        .collect();
    identities.sort();
    assert_eq!(
        identities,
        [
            (Ipv4Addr::new(192, 0, 2, 10), 24),
            (Ipv4Addr::new(192, 0, 2, 10), 25),
            (Ipv4Addr::new(192, 0, 2, 11), 24),
        ]
    );
    assert!(plan.diagnostics.is_empty());
}

#[test]
fn string_cidr_duplicates_merge_with_typed_addresses() {
    let ip = Ipv4Addr::new(192, 0, 2, 10);
    let typed = address(ip, Some(3600), Some(1800));
    let text = address_with_ip(
        Value::String("192.0.2.10/24".into()),
        Some(3600),
        Some(1800),
    );
    assert_ne!(typed, text);
    for (winner, other) in [(typed.clone(), text.clone()), (text, typed)] {
        let high = contribution(address_fields(vec![winner.clone()]));
        let low = Contribution::new(
            "p",
            "p:provider",
            Source::Dhcp { policy: "p".into() },
            "ethernet",
            high.match_spec.clone(),
            address_fields(vec![other.clone()]),
            50,
        );
        for contributions in [
            vec![contribution(address_fields(vec![winner, other]))],
            vec![high.clone(), low.clone()],
            vec![low, high],
        ] {
            for conflict_policy in [ConflictPolicy::Resolve, ConflictPolicy::Reject] {
                let plan = reconcile(
                    &contributions,
                    &observation(Vec::new(), true),
                    &SchemaRegistry::new(),
                    &MergeOptions { conflict_policy },
                )
                .unwrap();
                assert!(plan.diagnostics.is_empty());
                assert!(matches!(
                    &plan.diff().operations[..],
                    [operation] if matches!(&operation.kind, OperationKind::AddIpv4(address)
                        if address.ip == ip && address.prefix_len == 24
                            && address.lifetimes() == (3600, 1800))
                ));
                assert_eq!(plan.provenance[0].source.as_deref(), Some("p"));
                assert_eq!(
                    plan.provenance[0].element_sources.as_deref(),
                    Some(&["p".to_string()][..])
                );
            }
        }
    }
}

#[test]
fn string_cidr_conflicts_preserve_winning_values_and_omitted_attributes() {
    let ip = Ipv4Addr::new(192, 0, 2, 10);
    for (winner_lifetime, other_lifetime) in [(None, Some(3600)), (Some(3600), None)] {
        let Value::Map(attributes) =
            address_with_ip(Value::String("192.0.2.10/24".into()), winner_lifetime, None)
        else {
            unreachable!()
        };
        let winner = Value::Map(attributes.into_iter().rev().collect());
        let other = address(ip, other_lifetime, None);
        let plan = reconcile(
            &[contribution(address_fields(vec![
                winner.clone(),
                other.clone(),
            ]))],
            &observation(Vec::new(), true),
            &SchemaRegistry::new(),
            &MergeOptions::default(),
        )
        .unwrap();
        assert!(matches!(
            &plan.diagnostics[..],
            [Diagnostic::ValueConflict(conflict)]
                if conflict.identity == ConflictIdentity::Address { ip: Value::IpNetwork((IpAddr::V4(ip), 24)) }
                    && conflict.winner.value == winner
                    && conflict.winner.source == "p"
                    && conflict.competing.len() == 2
                    && conflict.competing[1].value == other
        ));
        assert!(matches!(
            &plan.diff().operations[..],
            [operation] if matches!(&operation.kind, OperationKind::AddIpv4(address)
                if address.valid_lft == winner_lifetime.map(|value| value as u32))
        ));
        assert_eq!(
            plan.provenance[0].element_sources.as_deref(),
            Some(&["p".to_string()][..])
        );
    }
}

#[test]
fn string_cidr_invalid_values_fail_schema_validation() {
    for text in [
        "192.0.2.10",
        "192.0.2.10/33",
        "192.0.2.10/no",
        "2001:db8::1/64",
    ] {
        for conflict_policy in [ConflictPolicy::Resolve, ConflictPolicy::Reject] {
            let result = reconcile(
                &[contribution(address_fields(vec![address_with_ip(
                    Value::String(text.into()),
                    None,
                    None,
                )]))],
                &observation(Vec::new(), true),
                &SchemaRegistry::new(),
                &MergeOptions { conflict_policy },
            );
            assert!(
                matches!(result, Err(ReconcileError::Validation(_))),
                "{text}: {result:?}"
            );
        }
    }
}

fn assert_duplicate_address_property_order(conflict_policy: ConflictPolicy) {
    let ip = Ipv4Addr::new(192, 0, 2, 10);
    let ordered = address(ip, Some(3600), Some(1800));
    let Value::Map(attributes) = ordered.clone() else {
        unreachable!()
    };
    let reordered = Value::Map(attributes.into_iter().rev().collect());
    assert_ne!(ordered, reordered);
    let high = contribution(address_fields(vec![reordered.clone()]));
    let low = Contribution::new(
        "p",
        "p:provider",
        Source::Dhcp { policy: "p".into() },
        "ethernet",
        high.match_spec.clone(),
        address_fields(vec![ordered.clone()]),
        50,
    );
    let inputs = [
        vec![contribution(address_fields(vec![reordered, ordered]))],
        vec![high.clone(), low.clone()],
        vec![low, high],
    ];
    for contributions in &inputs {
        let result = reconcile(
            contributions,
            &observation(Vec::new(), true),
            &SchemaRegistry::new(),
            &MergeOptions { conflict_policy },
        );
        let plan = result.unwrap_or_else(|error| {
            panic!("property order must not conflict in {conflict_policy:?} mode: {error:?}")
        });
        assert!(
            plan.diagnostics.is_empty(),
            "property order produced diagnostics in {conflict_policy:?} mode: {:?}",
            plan.diagnostics
        );
        assert_eq!(plan.diff().operations.len(), 1);
        assert!(matches!(
            &plan.diff().operations[0].kind,
            OperationKind::AddIpv4(address)
                if address.ip == ip && address.lifetimes() == (3600, 1800)
        ));
        assert_eq!(plan.provenance[0].source.as_deref(), Some("p"));
        assert_eq!(
            plan.provenance[0].element_sources.as_deref(),
            Some(&["p".to_string()][..])
        );
    }
}

#[test]
fn duplicate_address_property_order_is_accepted_in_resolve_mode() {
    assert_duplicate_address_property_order(ConflictPolicy::Resolve);
}

#[test]
fn duplicate_address_property_order_is_accepted_in_reject_mode() {
    assert_duplicate_address_property_order(ConflictPolicy::Reject);
}

#[test]
fn address_attribute_conflict_preserves_the_winning_object() {
    let ip = Ipv4Addr::new(192, 0, 2, 10);
    let Value::Map(attributes) = address(ip, Some(3600), Some(1800)) else {
        unreachable!()
    };
    let winner = Value::Map(attributes.into_iter().rev().collect());
    let high = contribution(address_fields(vec![winner.clone()]));
    let low = Contribution::new(
        "p",
        "p:provider",
        Source::Dhcp { policy: "p".into() },
        "ethernet",
        high.match_spec.clone(),
        address_fields(vec![address(ip, Some(7200), Some(1800))]),
        50,
    );
    let plan = reconcile(
        &[low, high],
        &observation(Vec::new(), true),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    assert!(matches!(
        &plan.diagnostics[..],
        [Diagnostic::ValueConflict(conflict)]
            if matches!(conflict.identity, ConflictIdentity::Address { .. })
                && conflict.winner.source == "p"
                && conflict.winner.value == winner
                && conflict.competing.len() == 2
    ));
    assert_eq!(
        plan.provenance[0].element_sources.as_deref(),
        Some(&["p".to_string()][..])
    );
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
        assert_eq!(render_json(&plan)["changes"][0]["source"], "p:provider");
        assert!(render_text(&plan).contains("(source: p:provider)"));
    }
}

#[test]
fn observed_addresses_alone_do_not_trigger_address_planning() {
    // FR-006: only explicitly configured fields produce operations. An
    // mtu-only policy leaves addresses untouched even when the observed
    // fields carry an address list and the inventory is incomplete or
    // holds a scoped/peer address the planner could not rebuild.
    let mut fields = IndexMap::new();
    fields.insert("mtu".into(), Value::U64(9000));
    let kernel_reported = || {
        let mut ipv4 = IndexMap::new();
        ipv4.insert(
            "addresses".into(),
            Value::List(vec![address(Ipv4Addr::new(192, 0, 2, 10), None, None)]),
        );
        let mut extra = IndexMap::new();
        extra.insert("ipv4".into(), Value::Map(ipv4));
        extra
    };
    let mut incomplete = device_observation("eth0", 7, Vec::new(), false);
    incomplete.state.fields.extend(kernel_reported());
    let plan = reconcile(
        &[contribution(fields.clone())],
        &observation_of(vec![incomplete]),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    assert!(matches!(
        plan.diff().operations.as_slice(),
        [operation] if matches!(operation.kind, OperationKind::SetMtu(9000))
    ));
    assert!(
        !plan
            .diff()
            .changes
            .iter()
            .any(|change| change.field == "ipv4.addresses")
    );

    let mut scoped = device_observation(
        "eth0",
        7,
        vec![AddressObservation {
            address: Ipv4Address {
                ip: Ipv4Addr::new(192, 0, 2, 10),
                prefix_len: 24,
                valid_lft: None,
                preferred_lft: None,
            },
            flags: 0,
            scope: 254,
            peer: Some(Ipv4Addr::new(198, 51, 100, 10)),
        }],
        true,
    );
    scoped.state.fields.extend(kernel_reported());
    let plan = reconcile(
        &[contribution(fields)],
        &observation_of(vec![scoped]),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    assert!(matches!(
        plan.diff().operations.as_slice(),
        [operation] if matches!(operation.kind, OperationKind::SetMtu(9000))
    ));
    assert!(
        !plan
            .diff()
            .changes
            .iter()
            .any(|change| change.field == "ipv4.addresses")
    );
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
fn rebuild_refuses_to_drop_address_flags_it_cannot_re_add() {
    // FR-009: a rebuild re-adds every still-desired member, but AddIpv4
    // cannot carry address flags. Swapping two noprefixroute addresses
    // would bring them back without the flag and let the kernel install
    // the prefix route it suppressed, so planning must fail instead.
    const IFA_F_SECONDARY: u32 = 0x01;
    const IFA_F_PERMANENT: u32 = 0x80;
    const IFA_F_NOPREFIXROUTE: u32 = 0x200;
    let ip = |last| Ipv4Address {
        ip: Ipv4Addr::new(192, 0, 2, last),
        prefix_len: 24,
        valid_lft: None,
        preferred_lft: None,
    };
    let observed = |flags: [u32; 2]| {
        vec![
            AddressObservation {
                address: ip(11),
                flags: flags[0],
                scope: 0,
                peer: None,
            },
            AddressObservation {
                address: ip(10),
                flags: flags[1] | IFA_F_SECONDARY,
                scope: 0,
                peer: None,
            },
        ]
    };
    let plan = |desired: &[u8], flags| {
        reconcile(
            &[contribution(address_fields(
                desired
                    .iter()
                    .map(|last| address(Ipv4Addr::new(192, 0, 2, *last), None, None))
                    .collect(),
            ))],
            &observation(observed(flags), true),
            &SchemaRegistry::new(),
            &MergeOptions::default(),
        )
    };
    let flagged = [IFA_F_NOPREFIXROUTE | IFA_F_PERMANENT; 2];
    let swapped = plan(&[10, 11], flagged);
    assert!(
        matches!(&swapped, Err(ReconcileError::UnsafeAddressGroup { message, .. })
            if message.ends_with("192.0.2.10/24 (0x200), 192.0.2.11/24 (0x200)")),
        "rebuilding flagged addresses must fail: {swapped:?}"
    );
    // Changes that leave the flagged members in place still plan.
    let appended = plan(&[11, 10, 12], flagged).unwrap();
    assert!(matches!(
        appended.diff().operations.as_slice(),
        [operation] if operation.kind == OperationKind::AddIpv4(ip(12))
    ));
    // A rebuild that removes the only flagged member, while the survivor
    // carries nothing beyond what re-adding restores, still plans.
    let primary_removed = plan(&[10], [IFA_F_NOPREFIXROUTE, IFA_F_PERMANENT]).unwrap();
    let kinds: Vec<_> = primary_removed
        .diff()
        .operations
        .iter()
        .map(|operation| &operation.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            &OperationKind::RemoveIpv4(ip(11)),
            &OperationKind::RemoveIpv4(ip(10)),
            &OperationKind::AddIpv4(ip(10)),
        ]
    );
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

#[test]
fn renders_conflict_evidence_and_winner_in_both_outputs() {
    // FR-010 / FR-012: both outputs retain the competing sources and
    // values and the selected winner, and JSON changes carry the winning
    // source.
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
    let json = render_json(&plan);
    let diagnostic = &json["diagnostics"][0];
    assert_eq!(diagnostic["kind"], "value-conflict");
    assert_eq!(diagnostic["identity"]["field"], "mtu");
    assert_eq!(diagnostic["winner"]["source"], "p");
    assert_eq!(diagnostic["winner"]["value"], 9000);
    let competing = diagnostic["competing"].as_array().unwrap();
    assert_eq!(competing.len(), 2);
    assert_eq!(competing[0]["source"], "p");
    assert_eq!(competing[0]["value"], 9000);
    assert_eq!(competing[1]["source"], "p:provider");
    assert_eq!(competing[1]["value"], 1400);
    assert_eq!(json["changes"][0]["source"], "p");
    let text = render_text(&plan);
    assert!(
        text.contains("value conflict on eth0 field mtu: p (9000) wins over p:provider (1400)"),
        "unexpected diagnostics text: {text}"
    );
}

#[test]
fn renders_every_operation_with_its_arguments() {
    // FR-009 / FR-010: the text preview lists each field's operations
    // beneath it, so a same-prefix rebuild shows the addresses
    // disappearing, and JSON operations carry their arguments, so plans
    // differing only in an mtu or address lifetime render differently.
    let observed = [11, 10]
        .into_iter()
        .map(|last| AddressObservation {
            address: Ipv4Address {
                ip: Ipv4Addr::new(192, 0, 2, last),
                prefix_len: 24,
                valid_lft: None,
                preferred_lft: None,
            },
            flags: 0,
            scope: 0,
            peer: None,
        })
        .collect();
    let mut fields = address_fields(vec![
        address(Ipv4Addr::new(192, 0, 2, 10), None, None),
        address(Ipv4Addr::new(192, 0, 2, 11), Some(3600), Some(1800)),
    ]);
    fields.insert("mtu".into(), Value::U64(9000));
    let plan = reconcile(
        &[contribution(fields)],
        &observation(observed, true),
        &SchemaRegistry::new(),
        &MergeOptions::default(),
    )
    .unwrap();
    assert_eq!(
        render_text(&plan),
        "2/eth0 ipv4.addresses: [{ip: 192.0.2.11/24}, {ip: 192.0.2.10/24}] -> \
         [{ip: 192.0.2.10/24}, {ip: 192.0.2.11/24, valid_lft: 3600, preferred_lft: 1800}] \
         (source: p)\n\
         \x20 #1 remove_ipv4 192.0.2.11/24\n\
         \x20 #2 remove_ipv4 192.0.2.10/24 (after #1)\n\
         \x20 #3 add_ipv4 192.0.2.10/24 (after #2)\n\
         \x20 #4 add_ipv4 192.0.2.11/24 valid_lft 3600 preferred_lft 1800 (after #3)\n\
         2/eth0 mtu: 1500 -> 9000 (source: p)\n\
         \x20 #0 set_mtu 9000\n"
    );
    let json = render_json(&plan);
    let operations = json["operations"].as_array().unwrap();
    assert_eq!(operations.len(), 5);
    assert_eq!(operations[0]["index"], 0);
    assert_eq!(operations[0]["kind"], "set_mtu");
    assert_eq!(operations[0]["field"], "mtu");
    assert_eq!(
        operations[0]["arguments"],
        serde_json::json!({ "mtu": 9000 })
    );
    assert_eq!(operations[3]["kind"], "add_ipv4");
    assert_eq!(operations[3]["field"], "ipv4.addresses");
    assert_eq!(operations[3]["depends_on"], serde_json::json!([2]));
    assert_eq!(
        operations[3]["arguments"],
        serde_json::json!({ "ip": "192.0.2.10/24", "valid_lft": null, "preferred_lft": null })
    );
    assert_eq!(
        operations[4]["arguments"],
        serde_json::json!({ "ip": "192.0.2.11/24", "valid_lft": 3600, "preferred_lft": 1800 })
    );
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
