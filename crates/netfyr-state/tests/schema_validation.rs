//! Integration tests for the schema-validation module
//! (story `002-schema-validation`).
//!
//! The tests cover:
//!
//! - unknown fields (`mtt` for `mtu`) rejected with
//!   `UnknownField`, by both `validate` and `validate_writable`;
//! - out-of-range values (mtu 99999 > 65535) rejected with
//!   `OutOfRange`;
//! - read-only fields (`mac`, `carrier`, `driver`, ...) pass
//!   `validate` but fail `validate_writable` with `ReadOnlyField`;
//! - only fragments whose top-level key is present in the
//!   state are applied (`base` always, `ipv4`/`ethernet` on trigger);
//! - pinned per-fragment field sets and writable/read-only
//!   partitions match hardcoded expected lists (drift protection);
//! - every violation is collected and reported together, not just
//!   the first;
//! - `field_info` exposes per-field `writable` metadata.

use indexmap::IndexMap;

use netfyr_state::{
    DecodeMode, Error, FieldInfo, FieldType, SchemaRegistry, Source, State, ValidationError, Value,
    from_yaml,
};

/// A fresh registry over the embedded fragments (base, ipv4, ethernet).
fn registry() -> SchemaRegistry {
    SchemaRegistry::new()
}

/// Build an insertion-ordered field map from `(key, value)` pairs.
fn fields(pairs: &[(&str, Value)]) -> IndexMap<String, Value> {
    let mut map = IndexMap::new();
    for (key, value) in pairs {
        map.insert((*key).to_string(), value.clone());
    }
    map
}

/// A state carrying exactly the given fields (static source, no match).
fn state_with_fields(pairs: &[(&str, Value)]) -> State {
    let mut state = State::new(Source::Static {
        policy: String::new(),
    });
    state.fields = fields(pairs);
    state
}

fn text(value: &str) -> Value {
    Value::String(value.to_string())
}

/// A CIDR like `"10.0.1.50/24"` as [`Value::IpNetwork`].
fn ipnet(value: &str) -> Value {
    let (addr, prefix) = value
        .split_once('/')
        .expect("test fixture: CIDR with a prefix");
    Value::IpNetwork((
        addr.parse().expect("test fixture: a valid IP address"),
        prefix.parse().expect("test fixture: a valid prefix length"),
    ))
}

/// One `ipv4.addresses` entry built from `(key, value)` pairs.
fn address_entry(pairs: &[(&str, Value)]) -> Value {
    Value::Map(fields(pairs))
}

/// An `ipv4` sub-object value wrapping the given address entries.
fn ipv4_object(entries: Vec<Value>) -> Value {
    Value::Map(fields(&[("addresses", Value::List(entries))]))
}

/// An `ethernet` sub-object value built from `(key, value)` pairs.
fn ethernet_object(pairs: &[(&str, Value)]) -> Value {
    Value::Map(fields(pairs))
}

fn info(writable: bool, r#type: FieldType) -> FieldInfo {
    FieldInfo { writable, r#type }
}

/// The fragment names present in the error list, in error order.
fn fragment_names(errors: &[ValidationError]) -> Vec<&'static str> {
    errors
        .iter()
        .map(|e| match e {
            ValidationError::UnknownField { fragment, .. }
            | ValidationError::ReadOnlyField { fragment, .. }
            | ValidationError::OutOfRange { fragment, .. }
            | ValidationError::WrongType { fragment, .. }
            | ValidationError::MissingField { fragment, .. }
            | ValidationError::EnumViolation { fragment, .. }
            | ValidationError::InvalidFormat { fragment, .. } => *fragment,
        })
        .collect()
}

fn has_unknown_field(errors: &[ValidationError], fragment: &str, path: &str) -> bool {
    errors.iter().any(|e| {
        matches!(
            e,
            ValidationError::UnknownField { fragment: f, path: p }
                if *f == fragment && p == path
        )
    })
}

fn has_read_only_field(errors: &[ValidationError], fragment: &str, path: &str) -> bool {
    errors.iter().any(|e| {
        matches!(
            e,
            ValidationError::ReadOnlyField { fragment: f, path: p }
                if *f == fragment && p == path
        )
    })
}

fn has_out_of_range(errors: &[ValidationError], fragment: &str, path: &str) -> bool {
    errors.iter().any(|e| {
        matches!(
            e,
            ValidationError::OutOfRange {
                fragment: f,
                path: p,
                ..
            } if *f == fragment && p == path
        )
    })
}

/// A realistic query result: every read-only base field plus the read-only
/// `ethernet` sub-object, alongside writable `enabled`/`mtu`.
fn query_result_state() -> State {
    let mut state = state_with_fields(&[
        ("name", text("eth0")),
        ("mac", text("aa:bb:cc:dd:ee:ff")),
        ("carrier", Value::Bool(true)),
        ("driver", text("mlx5_core")),
        ("enabled", Value::Bool(true)),
        ("mtu", Value::U64(1500)),
        (
            "ethernet",
            ethernet_object(&[
                ("speed", Value::U64(1000)),
                ("duplex", text("full")),
                ("autoneg", Value::Bool(true)),
            ]),
        ),
    ]);
    state.device_type = "ethernet".to_string();
    state
}

// ---------------------------------------------------------------------------
// Validate state against the schema
// ---------------------------------------------------------------------------

/// A typo'd field (`mtt` for `mtu`) is rejected
/// by `validate_writable` with an `UnknownField` error naming the field and
/// the `base` fragment.
#[test]
fn unknown_field_mtt_rejected_by_validate_writable() {
    let state = state_with_fields(&[("mtt", Value::U64(1500))]);
    let errors = registry().validate_writable(&state);
    assert_eq!(
        errors,
        vec![ValidationError::UnknownField {
            fragment: "base",
            path: "mtt".to_string(),
        }],
    );
}

/// `validate` (query mode) must reject unknown fields too: only the
/// writable/read-only handling differs between the two modes.
#[test]
fn unknown_field_rejected_by_validate_as_well() {
    let state = state_with_fields(&[("mtt", Value::U64(1500))]);
    let errors = registry().validate(&state);
    assert_eq!(
        errors,
        vec![ValidationError::UnknownField {
            fragment: "base",
            path: "mtt".to_string(),
        }],
    );
}

/// mtu 99999 exceeds the 65535 maximum and is
/// rejected with an `OutOfRange` error that carries the legal range.
#[test]
fn mtu_above_maximum_rejected_by_validate() {
    let state = state_with_fields(&[("mtu", Value::U64(99999))]);
    let errors = registry().validate(&state);
    assert_eq!(
        errors,
        vec![ValidationError::OutOfRange {
            fragment: "base",
            path: "mtu".to_string(),
            min: Some(68),
            max: Some(65535),
        }],
    );
}

/// Out-of-range must trigger below the minimum too (68).
#[test]
fn mtu_below_minimum_rejected_with_out_of_range() {
    for mtu in [0u64, 67] {
        let state = state_with_fields(&[("mtu", Value::U64(mtu))]);
        let errors = registry().validate_writable(&state);
        assert_eq!(
            errors,
            vec![ValidationError::OutOfRange {
                fragment: "base",
                path: "mtu".to_string(),
                min: Some(68),
                max: Some(65535),
            }],
            "mtu {mtu} should be rejected as below the 68-byte minimum"
        );
    }
}

/// Boundary values themselves are legal (minimum 68, maximum 65535).
#[test]
fn mtu_at_range_boundaries_accepted() {
    for mtu in [68u64, 65535] {
        let state = state_with_fields(&[("mtu", Value::U64(mtu))]);
        let reg = registry();
        assert!(reg.validate(&state).is_empty(), "mtu {mtu}");
        assert!(reg.validate_writable(&state).is_empty(), "mtu {mtu}");
    }
}

/// Two unknown fields are both reported; the
/// second typo is not hidden behind the first.
#[test]
fn multiple_unknown_fields_all_reported() {
    let state = state_with_fields(&[("mtt", Value::U64(1500)), ("foobar", text("baz"))]);
    let errors = registry().validate_writable(&state);
    assert_eq!(errors.len(), 2);
    assert!(
        has_unknown_field(&errors, "base", "mtt"),
        "missing UnknownField for 'mtt': {errors:?}"
    );
    assert!(
        has_unknown_field(&errors, "base", "foobar"),
        "missing UnknownField for 'foobar': {errors:?}"
    );
}

// ---------------------------------------------------------------------------
// Read-only field constraints
// ---------------------------------------------------------------------------

/// `mac` is a read-only kernel property: query
/// results carrying it pass `validate`, but setting it in a policy fails
/// `validate_writable` with a `ReadOnlyField` error.
#[test]
fn read_only_mac_passes_validate_but_fails_validate_writable() {
    let state = state_with_fields(&[("mac", text("aa:bb:cc:dd:ee:ff"))]);
    let reg = registry();
    assert!(reg.validate(&state).is_empty());
    assert_eq!(
        reg.validate_writable(&state),
        vec![ValidationError::ReadOnlyField {
            fragment: "base",
            path: "mac".to_string(),
        }],
    );
}

#[test]
fn yaml_device_type_passes_query_validation_but_fails_writable_validation() {
    let state = &from_yaml("type: ethernet\nname: eth0\n").unwrap()[0];
    let reg = registry();
    assert!(reg.validate(state).is_empty());
    // `device_type` is structural metadata and is not rejected as
    // read-only; only `name` (a true read-only field) triggers an error.
    assert_eq!(
        reg.validate_writable(state),
        vec![ValidationError::ReadOnlyField {
            fragment: "base",
            path: "name".to_string(),
        }]
    );
}

/// Semantic conversion is selected by the schema path, not by the spelling
/// of a YAML scalar. An address-looking interface name remains a string.
#[test]
fn yaml_plain_string_path_does_not_infer_an_ip_address() {
    for input in ["name: 192.0.2.1\n", "name: \"192.0.2.1\"\n"] {
        let state = &registry().from_yaml(input).unwrap()[0];
        assert_eq!(
            state.fields.get("name"),
            Some(&Value::String("192.0.2.1".to_string()))
        );
        assert!(registry().validate(state).is_empty());
    }
}

#[test]
fn query_codec_validates_and_derives_kernel_match_metadata() {
    let states = registry()
        .decode_yaml(
            "name: 192.0.2.1\ntype: ethernet\ndriver: ixgbe\nmtu: 1500\n",
            DecodeMode::Query,
        )
        .unwrap();
    let state = &states[0];
    assert_eq!(state.match_spec.name.as_deref(), Some("192.0.2.1"));
    assert_eq!(state.match_spec.r#type.as_deref(), Some("ethernet"));
    assert_eq!(state.match_spec.driver.as_deref(), Some("ixgbe"));
    assert_eq!(state.source, Source::Kernel);
    assert_eq!(state.priority, 0);
}

#[test]
fn policy_codec_requires_match_and_collects_schema_errors() {
    assert_eq!(
        registry()
            .decode_yaml("mtu: 1500\n", DecodeMode::Policy)
            .unwrap_err(),
        Error::MissingMatch { state_index: 0 }
    );

    let Error::Validation(errors) = registry()
        .decode_yaml(
            "match: { name: eth0 }\nmtt: 1500\nmtu: 99999\n",
            DecodeMode::Policy,
        )
        .unwrap_err()
    else {
        panic!("expected validation errors");
    };
    assert_eq!(errors.len(), 2);
    assert!(has_unknown_field(&[errors[0].error.clone()], "base", "mtt"));
    assert!(has_out_of_range(&[errors[1].error.clone()], "base", "mtu"));
}

/// A full query result (read-only base fields plus read-only `ethernet`
/// sub-object) is accepted by `validate` in its entirety.
#[test]
fn query_result_with_read_only_fields_passes_validate() {
    assert!(registry().validate(&query_result_state()).is_empty());
}

/// The same read-only fields are all rejected in writable mode. Every
/// violation is collected, not just the first.  `device_type` (`type`) is
/// structural metadata and is intentionally exempt.
#[test]
fn all_read_only_fields_rejected_in_writable_mode() {
    let state = query_result_state();
    let errors = registry().validate_writable(&state);
    assert_eq!(
        errors.len(),
        7,
        "expected 4 base + 3 ethernet violations: {errors:?}"
    );
    for path in ["name", "mac", "carrier", "driver"] {
        assert!(
            has_read_only_field(&errors, "base", path),
            "missing ReadOnlyField for '{path}': {errors:?}"
        );
    }
    for path in ["ethernet.speed", "ethernet.duplex", "ethernet.autoneg"] {
        assert!(
            has_read_only_field(&errors, "ethernet", path),
            "missing ReadOnlyField for '{path}': {errors:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Fragment-based validation
// ---------------------------------------------------------------------------

/// A state carrying an `ipv4` sub-object but no
/// `ethernet` sub-object validates cleanly. `base` and `ipv4` apply, the
/// `ethernet` fragment does not (no error may name it).
#[test]
fn ipv4_only_state_validates_base_and_ipv4_not_ethernet() {
    let state = state_with_fields(&[
        ("mtu", Value::U64(1400)),
        (
            "ipv4",
            ipv4_object(vec![address_entry(&[("ip", ipnet("10.0.1.50/24"))])]),
        ),
    ]);
    let reg = registry();
    let errors = reg.validate(&state);
    assert!(errors.is_empty(), "valid ipv4-only state: {errors:?}");
    let writable_errors = reg.validate_writable(&state);
    assert!(
        writable_errors.is_empty(),
        "valid ipv4-only policy: {writable_errors:?}"
    );
    assert!(
        !fragment_names(&writable_errors).contains(&"ethernet"),
        "ethernet fragment applied without an 'ethernet' key: {writable_errors:?}"
    );
}

/// An unknown field inside the `ipv4` sub-object is reported by the `ipv4`
/// fragment: `base` tolerates the `ipv4` trigger key and the
/// `ethernet` fragment is never applied, so exactly one error comes back.
#[test]
fn unknown_field_in_ipv4_reported_by_ipv4_fragment() {
    let state = state_with_fields(&[("ipv4", Value::Map(fields(&[("bogus", Value::U64(1))])))]);
    let errors = registry().validate_writable(&state);
    assert_eq!(
        errors,
        vec![ValidationError::UnknownField {
            fragment: "ipv4",
            path: "ipv4.bogus".to_string(),
        }],
    );
}

/// The mirror of the ipv4 case: with only an `ethernet` sub-object
/// present, `base` still applies (the `mtt` typo is caught by `base`) and
/// the `ipv4` fragment is not applied.
#[test]
fn ethernet_only_state_validates_base_and_ethernet_not_ipv4() {
    let state = state_with_fields(&[
        ("mtt", Value::U64(1500)),
        ("ethernet", ethernet_object(&[("speed", Value::U64(1000))])),
    ]);
    let errors = registry().validate(&state);
    assert_eq!(
        errors.len(),
        1,
        "only the 'mtt' typo should be flagged: {errors:?}"
    );
    assert!(has_unknown_field(&errors, "base", "mtt"));
    assert!(
        !fragment_names(&errors).contains(&"ipv4"),
        "ipv4 fragment applied without an 'ipv4' key: {errors:?}"
    );
}

/// `base` applies to every state, even one with no other fragment triggers.
#[test]
fn base_fragment_always_applies() {
    // No ipv4/ethernet keys at all: the unknown field is still caught by
    // the always-applied base fragment.
    let state = state_with_fields(&[("mtt", Value::U64(1500))]);
    let errors = registry().validate_writable(&state);
    assert!(has_unknown_field(&errors, "base", "mtt"));
    // And a bare writable field is fully valid with no fragments triggered.
    let enabled_only = state_with_fields(&[("enabled", Value::Bool(true))]);
    let reg = registry();
    assert!(reg.validate(&enabled_only).is_empty());
    assert!(reg.validate_writable(&enabled_only).is_empty());
}

/// The `ipv4` trigger key, when present, must be a sub-object; a scalar
/// there is a type error from the `ipv4` fragment.
#[test]
fn ipv4_trigger_key_must_be_an_object() {
    let state = state_with_fields(&[("ipv4", ipnet("10.0.1.50/24"))]);
    let errors = registry().validate(&state);
    assert_eq!(
        errors,
        vec![ValidationError::WrongType {
            fragment: "ipv4",
            path: "ipv4".to_string(),
            expected: FieldType::Object,
            found: FieldType::IpNetwork,
        }],
    );
}

/// An empty state is valid in both modes (no field trips any fragment).
#[test]
fn empty_state_is_valid_in_both_modes() {
    let state = State::new(Source::Static {
        policy: String::new(),
    });
    let reg = registry();
    assert!(reg.validate(&state).is_empty());
    assert!(reg.validate_writable(&state).is_empty());
}

// ---------------------------------------------------------------------------
// ipv4 fragment behavior
// ---------------------------------------------------------------------------

/// Each `ipv4.addresses` entry requires `ip`; an entry without one
/// yields a `MissingField` error with an `[0]` array-index path.
#[test]
fn ipv4_address_entry_without_ip_is_missing_required_field() {
    let entry = address_entry(&[("valid_lft", Value::U64(120))]);
    let state = state_with_fields(&[("ipv4", ipv4_object(vec![entry]))]);
    let errors = registry().validate_writable(&state);
    assert_eq!(
        errors,
        vec![ValidationError::MissingField {
            fragment: "ipv4",
            path: "ipv4.addresses[0].ip".to_string(),
        }],
    );
}

/// Happy path: a policy setting addresses with lifetimes is fully
/// writable and passes both modes.
#[test]
fn ipv4_valid_addresses_pass_both_validation_modes() {
    let entries = vec![
        address_entry(&[
            ("ip", ipnet("10.0.1.50/24")),
            ("valid_lft", Value::U64(120)),
            ("preferred_lft", Value::U64(60)),
        ]),
        address_entry(&[("ip", ipnet("192.168.7.9/24"))]),
    ];
    let state = state_with_fields(&[("mtu", Value::U64(1400)), ("ipv4", ipv4_object(entries))]);
    let reg = registry();
    assert!(reg.validate(&state).is_empty());
    assert!(reg.validate_writable(&state).is_empty());
}

/// Lifetimes are integer seconds; values above `u32::MAX` remain
/// valid because the schema does not impose an undocumented upper bound.
#[test]
fn ipv4_valid_lft_above_u32_maximum_is_accepted() {
    let entry = address_entry(&[
        ("ip", ipnet("10.0.1.50/24")),
        ("valid_lft", Value::U64(4_294_967_296)),
    ]);
    let state = state_with_fields(&[("ipv4", ipv4_object(vec![entry]))]);
    assert!(registry().validate_writable(&state).is_empty());
}

#[test]
fn ipv4_cidr_format_accepts_valid_network_values_and_strings() {
    let network = state_with_fields(&[(
        "ipv4",
        ipv4_object(vec![address_entry(&[("ip", ipnet("10.0.1.50/24"))])]),
    )]);
    let string = state_with_fields(&[(
        "ipv4",
        ipv4_object(vec![address_entry(&[("ip", text("10.0.1.50/24"))])]),
    )]);
    assert!(registry().validate(&network).is_empty());
    assert!(registry().validate(&string).is_empty());
}

/// A CIDR becomes an IP-network value only at the `ipv4-cidr` schema path.
#[test]
fn yaml_ipv4_cidr_path_decodes_to_ip_network() {
    let state = &registry()
        .from_yaml("ipv4:\n  addresses:\n    - ip: 192.0.2.1/24\n")
        .unwrap()[0];
    let Value::Map(ipv4) = state.fields.get("ipv4").unwrap() else {
        panic!("ipv4 must be a map");
    };
    let Value::List(addresses) = ipv4.get("addresses").unwrap() else {
        panic!("addresses must be a list");
    };
    let Value::Map(address) = &addresses[0] else {
        panic!("address entry must be a map");
    };
    assert_eq!(address.get("ip"), Some(&ipnet("192.0.2.1/24")));
    assert!(registry().validate_writable(state).is_empty());
}

/// Failed format decoding retains the scalar only long enough for schema
/// validation to report the format problem at the right path.
#[test]
fn yaml_invalid_ipv4_cidr_reports_invalid_format_without_ip_inference() {
    let state = &registry()
        .from_yaml("ipv4:\n  addresses:\n    - ip: 192.0.2.1/33\n")
        .unwrap()[0];
    let Value::Map(ipv4) = state.fields.get("ipv4").unwrap() else {
        panic!("ipv4 must be a map");
    };
    let Value::List(addresses) = ipv4.get("addresses").unwrap() else {
        panic!("addresses must be a list");
    };
    let Value::Map(address) = &addresses[0] else {
        panic!("address entry must be a map");
    };
    assert_eq!(address.get("ip"), Some(&text("192.0.2.1/33")));
    assert!(matches!(
        registry().validate_writable(state).as_slice(),
        [ValidationError::InvalidFormat { path, .. }] if path == "ipv4.addresses[0].ip"
    ));
}

#[test]
fn ipv4_cidr_format_rejects_invalid_and_ipv6_networks() {
    let invalid_network = Value::IpNetwork(("10.0.1.50".parse().unwrap(), 33));
    for value in [
        text("not-a-cidr"),
        text("10.0.0.1/33"),
        text("2001:db8::1/64"),
        Value::IpNetwork(("2001:db8::1".parse().unwrap(), 64)),
        invalid_network,
    ] {
        let state =
            state_with_fields(&[("ipv4", ipv4_object(vec![address_entry(&[("ip", value)])]))]);
        assert!(matches!(
            registry().validate(&state).as_slice(),
            [ValidationError::InvalidFormat {
                fragment: "ipv4",
                path,
                expected: "ipv4-cidr",
            }] if path == "ipv4.addresses[0].ip"
        ));
    }
}

/// The address-entry object is closed. An unknown key inside an
/// entry is reported at its `[n]` path by the `ipv4` fragment.
#[test]
fn ipv4_unknown_field_in_address_entry_rejected() {
    let entry = address_entry(&[("ip", ipnet("10.0.1.50/24")), ("bogus", Value::U64(1))]);
    let state = state_with_fields(&[("ipv4", ipv4_object(vec![entry]))]);
    let errors = registry().validate_writable(&state);
    assert_eq!(
        errors,
        vec![ValidationError::UnknownField {
            fragment: "ipv4",
            path: "ipv4.addresses[0].bogus".to_string(),
        }],
    );
}

// ---------------------------------------------------------------------------
// Type and enum checks
// ---------------------------------------------------------------------------

/// A wrong type is reported as `WrongType` with expected/found types.
#[test]
fn mtu_with_string_value_is_wrong_type() {
    let state = state_with_fields(&[("mtu", text("1500"))]);
    let errors = registry().validate(&state);
    assert_eq!(
        errors,
        vec![ValidationError::WrongType {
            fragment: "base",
            path: "mtu".to_string(),
            expected: FieldType::Integer,
            found: FieldType::String,
        }],
    );
}

#[test]
fn writable_read_only_fields_still_report_value_errors() {
    let mac = state_with_fields(&[("mac", Value::Bool(true))]);
    assert_eq!(
        registry().validate_writable(&mac),
        vec![
            ValidationError::ReadOnlyField {
                fragment: "base",
                path: "mac".to_string(),
            },
            ValidationError::WrongType {
                fragment: "base",
                path: "mac".to_string(),
                expected: FieldType::String,
                found: FieldType::Boolean,
            },
        ]
    );

    let ethernet = state_with_fields(&[("ethernet", ethernet_object(&[("duplex", text("quad"))]))]);
    assert_eq!(
        registry().validate_writable(&ethernet),
        vec![
            ValidationError::ReadOnlyField {
                fragment: "ethernet",
                path: "ethernet.duplex".to_string(),
            },
            ValidationError::EnumViolation {
                fragment: "ethernet",
                path: "ethernet.duplex".to_string(),
                allowed: vec![
                    "full".to_string(),
                    "half".to_string(),
                    "unknown".to_string()
                ],
            },
        ]
    );
}

/// `ethernet.duplex` is an enum; a value outside `full`/`half`/
/// `unknown` is rejected with the allowed values listed.
#[test]
fn ethernet_duplex_rejects_value_outside_enum() {
    let state = state_with_fields(&[("ethernet", ethernet_object(&[("duplex", text("quad"))]))]);
    let errors = registry().validate(&state);
    assert_eq!(
        errors,
        vec![ValidationError::EnumViolation {
            fragment: "ethernet",
            path: "ethernet.duplex".to_string(),
            allowed: vec![
                "full".to_string(),
                "half".to_string(),
                "unknown".to_string(),
            ],
        }],
    );
}

/// Legal read-only ethernet values pass query validation.
#[test]
fn ethernet_legal_read_only_values_pass_validate() {
    for duplex in ["full", "half", "unknown"] {
        let state = state_with_fields(&[(
            "ethernet",
            ethernet_object(&[
                ("speed", Value::U64(1000)),
                ("duplex", text(duplex)),
                ("autoneg", Value::Bool(true)),
            ]),
        )]);
        assert!(
            registry().validate(&state).is_empty(),
            "duplex {duplex:?} should be a legal query result"
        );
    }
}

// ---------------------------------------------------------------------------
// Pinned field sets (schema/backend drift protection)
// ---------------------------------------------------------------------------

/// The exact field set, types, and writable/read-only partition of
/// the `base` fragment. If a schema field is added, removed,
/// renamed, or re-partitioned, this test fails. Fields without the
/// `x-netfyr-writable` extension pin as read-only.
#[test]
fn pinned_base_fragment_field_set() {
    assert_eq!(
        registry()
            .fragment_fields("base")
            .expect("'base' fragment must exist"),
        vec![
            ("carrier".to_string(), info(false, FieldType::Boolean)),
            ("driver".to_string(), info(false, FieldType::String)),
            ("enabled".to_string(), info(true, FieldType::Boolean)),
            ("mac".to_string(), info(false, FieldType::String)),
            ("mtu".to_string(), info(true, FieldType::Integer)),
            ("name".to_string(), info(false, FieldType::String)),
            ("type".to_string(), info(false, FieldType::String)),
        ],
    );
}

/// The exact field set, types, and writability of the `ipv4`
/// fragment. Every ipv4 field is writable.
#[test]
fn pinned_ipv4_fragment_field_set() {
    assert_eq!(
        registry()
            .fragment_fields("ipv4")
            .expect("'ipv4' fragment must exist"),
        vec![
            ("addresses".to_string(), info(true, FieldType::Array)),
            ("addresses.ip".to_string(), info(true, FieldType::IpNetwork)),
            (
                "addresses.preferred_lft".to_string(),
                info(true, FieldType::Integer),
            ),
            (
                "addresses.valid_lft".to_string(),
                info(true, FieldType::Integer)
            ),
        ],
    );
}

/// The exact field set, types, and writability of the `ethernet`
/// fragment. Every ethernet field is read-only.
#[test]
fn pinned_ethernet_fragment_field_set() {
    assert_eq!(
        registry()
            .fragment_fields("ethernet")
            .expect("'ethernet' fragment must exist"),
        vec![
            ("autoneg".to_string(), info(false, FieldType::Boolean)),
            ("duplex".to_string(), info(false, FieldType::String)),
            ("speed".to_string(), info(false, FieldType::Integer)),
        ],
    );
}

// ---------------------------------------------------------------------------
// field_info metadata
// ---------------------------------------------------------------------------

/// `field_info` reports `writable: true` and the dialect type for a
/// writable field.
#[test]
fn field_info_reports_mtu_writable_integer() {
    assert_eq!(
        registry().field_info("base", "mtu"),
        Some(info(true, FieldType::Integer)),
    );
}

/// `field_info` reports `writable: false` for a read-only field.
#[test]
fn field_info_reports_mac_read_only_string() {
    assert_eq!(
        registry().field_info("base", "mac"),
        Some(info(false, FieldType::String)),
    );
}

/// Nested fields are addressed by dot path within the fragment.
#[test]
fn field_info_ipv4_nested_address_field() {
    assert_eq!(
        registry().field_info("ipv4", "addresses.ip"),
        Some(info(true, FieldType::IpNetwork)),
    );
}

/// Unknown fields and unknown fragments yield `None`, not a
/// default/incorrect entry.
#[test]
fn field_info_unknown_field_or_fragment_is_none() {
    let reg = registry();
    assert_eq!(
        reg.field_info("base", "mtt"),
        None,
        "'mtt' is not a schema field"
    );
    assert_eq!(
        reg.field_info("nope", "mtu"),
        None,
        "'nope' is not a fragment"
    );
}

// ---------------------------------------------------------------------------
// Error collection across violations and fragments
// ---------------------------------------------------------------------------

/// Within one fragment, an unknown field, an out-of-range value,
/// and a read-only write are all reported together in writable mode.
#[test]
fn writable_mode_collects_unknown_readonly_and_range_together() {
    let state = state_with_fields(&[
        ("mtt", Value::U64(1500)),
        ("mtu", Value::U64(99999)),
        ("mac", text("aa:bb:cc:dd:ee:ff")),
    ]);
    let errors = registry().validate_writable(&state);
    assert_eq!(errors.len(), 3, "all three violations expected: {errors:?}");
    assert!(has_unknown_field(&errors, "base", "mtt"));
    assert!(has_out_of_range(&errors, "base", "mtu"));
    assert!(has_read_only_field(&errors, "base", "mac"));
}

/// Violations are collected across *different* fragments: one
/// unknown field and one range error in `base`, one unknown field in
/// `ipv4`, and one enum violation in `ethernet`, all in a single call.
#[test]
fn errors_collected_across_all_fragments() {
    let state = state_with_fields(&[
        ("mtt", Value::U64(1500)),
        ("mtu", Value::U64(99999)),
        ("ipv4", Value::Map(fields(&[("bogus", Value::U64(1))]))),
        ("ethernet", ethernet_object(&[("duplex", text("quad"))])),
    ]);
    let errors = registry().validate(&state);
    assert_eq!(
        errors.len(),
        4,
        "one violation per seeded fault: {errors:?}"
    );
    assert!(has_unknown_field(&errors, "base", "mtt"));
    assert!(has_out_of_range(&errors, "base", "mtu"));
    assert!(has_unknown_field(&errors, "ipv4", "ipv4.bogus"));
    assert!(
        errors.iter().any(|e| {
            matches!(
                e,
                ValidationError::EnumViolation {
                    fragment: "ethernet",
                    path,
                    ..
                } if path == "ethernet.duplex"
            )
        }),
        "missing EnumViolation for ethernet.duplex: {errors:?}"
    );
    let names = fragment_names(&errors);
    for fragment in ["base", "ipv4", "ethernet"] {
        assert!(
            names.contains(&fragment),
            "no error from '{fragment}': {errors:?}"
        );
    }
}

/// Adding a base property whose name collides with an existing fragment
/// trigger key (e.g. `ipv4`, `ethernet`) must be rejected.
#[test]
fn base_property_colliding_with_fragment_trigger_is_rejected() {
    let mut reg = registry();
    let err = reg
        .add_schema_node("base", "ipv4", r#"{"type":"string"}"#)
        .unwrap_err();
    assert!(
        err.contains("collides with a fragment trigger key"),
        "unexpected error: {err}"
    );
}
