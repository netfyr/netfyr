//! User Story 6 - load administrative overrides (FR-011, SC-002):
//! same-name shadowing selects the whole higher-tier definition before
//! contributions are produced; differently named policies coexist in the
//! loaded set and contribute under SPEC-005's merge rules when they bind
//! to one device.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use netfyr_policy::{DEFAULT_POLICY_PATHS, PolicyError, load_policy_dir, load_policy_tiers};
use netfyr_state::{ApplyOptions, Value};

/// A fresh temporary tier root; the caller removes it when done.
fn tier_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "netfyr-tiers-test-{}-{}-{label}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    fs::create_dir_all(&root).unwrap();
    root
}

/// FR-011: the default ordered search path is a named loader setting,
/// highest precedence first.
#[test]
fn fr011_default_policy_paths_are_named_and_ordered() {
    let expected = [
        "/run/netfyr/policies",
        "/etc/netfyr/policies",
        "/usr/lib/netfyr/policies",
    ];
    assert_eq!(DEFAULT_POLICY_PATHS, &expected[..]);
}

/// US6-1: a policy name present in both tiers selects only the
/// higher-precedence (first) tier's whole definition, and only that
/// definition contributes to the plan.
#[test]
fn us6_1_same_name_in_two_tiers_selects_the_whole_higher_definition() {
    let (etc, usrlib) = (tier_root("etc"), tier_root("usrlib"));
    let (etc_dir, usrlib_dir) = (etc.clone(), usrlib.clone());
    // The higher tier defines eth0 with mtu 9000 and enabled; the vendor
    // tier defines a different eth0 with only mtu 1500.
    fs::write(
        etc.join("eth0.yaml"),
        "match: { name: eth0 }\nmtu: 9000\nenabled: true\n",
    )
    .unwrap();
    fs::write(
        usrlib.join("eth0.yaml"),
        "match: { name: eth0 }\nmtu: 1500\n",
    )
    .unwrap();

    // Only the /etc definition is selected.
    let outcome = load_policy_tiers(&[etc.clone(), usrlib.clone()], &ApplyOptions::default())
        .expect("the tiers are independently valid");
    assert_eq!(outcome.policies.len(), 1);
    let policy = &outcome.policies[0];
    assert_eq!(policy.name, "eth0");
    // The whole higher-tier definition is kept, not a merge.
    let keys: Vec<&str> = policy.fields.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["mtu", "enabled"]);
    assert_eq!(policy.fields["mtu"], Value::U64(9000));
    assert_eq!(policy.fields["enabled"], Value::Bool(true));

    // Reversing the roots reverses the selection: the order, not the
    // path, defines precedence.
    let reversed = load_policy_tiers(&[usrlib, etc], &ApplyOptions::default())
        .expect("the tiers are independently valid");
    assert_eq!(reversed.policies.len(), 1);
    assert_eq!(reversed.policies[0].fields["mtu"], Value::U64(1500));
    assert!(!reversed.policies[0].fields.contains_key("enabled"));

    let _ = fs::remove_dir_all(&etc_dir);
    let _ = fs::remove_dir_all(&usrlib_dir);
}

/// US6-2: differently named policies in different tiers coexist in the
/// loaded set. Same-name shadowing does not apply because the names differ.
#[test]
fn us6_2_distinct_names_coexist_across_tiers() {
    let (etc, usrlib) = (tier_root("etc"), tier_root("usrlib"));
    let (etc_dir, usrlib_dir) = (etc.clone(), usrlib.clone());
    fs::write(etc.join("eth0.yaml"), "match: { name: eth0 }\nmtu: 9000\n").unwrap();
    fs::write(
        usrlib.join("other.yaml"),
        "match: { name: eth0 }\nenabled: true\n",
    )
    .unwrap();

    // Distinct names coexist in the loaded set, regardless of tier.
    let outcome = load_policy_tiers(&[etc, usrlib], &ApplyOptions::default())
        .expect("the tiers are independently valid");
    let names: Vec<&str> = outcome.policies.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["eth0", "other"]);

    let _ = fs::remove_dir_all(&etc_dir);
    let _ = fs::remove_dir_all(&usrlib_dir);
}

/// FR-011: a root that cannot be stat'ed (here: a path below a regular
/// file, ENOTDIR) is a hard Io error rather than a silently skipped
/// tier; a merely absent root is skipped.
#[test]
fn unstattable_root_fails_hard_absent_root_is_skipped() {
    let tier = tier_root("stat");
    let tier_dir = tier.clone();
    fs::write(tier.join("file.yaml"), "not a directory\n").unwrap();

    let error = load_policy_tiers(
        &[tier.join("file.yaml").join("policies")],
        &ApplyOptions::default(),
    )
    .expect_err("a root that cannot be stat'ed must fail the load");
    assert!(
        matches!(&error, PolicyError::Io(_)),
        "expected an Io error, got {error:?}"
    );

    let outcome = load_policy_tiers(&[tier.join("does-not-exist")], &ApplyOptions::default())
        .expect("an absent root is skipped");
    assert!(outcome.policies.is_empty());
    assert!(outcome.warnings.is_empty());

    let _ = fs::remove_dir_all(&tier_dir);
}

/// US6-3: duplicate policy names within one tier return SPEC-003's
/// duplicate-name error at load time, before planning.
#[test]
fn us6_3_duplicate_names_within_one_tier_fail_before_planning() {
    let tier = tier_root("dup");
    let tier_dir = tier.clone();
    fs::write(
        tier.join("a.yaml"),
        "kind: policy\nname: dup\nmatch: { name: eth0 }\nmtu: 9000\n",
    )
    .unwrap();
    fs::write(
        tier.join("b.yaml"),
        "kind: policy\nname: dup\nmatch: { name: eth1 }\nenabled: true\n",
    )
    .unwrap();

    // The duplicate-name error is returned by the tier loader itself.
    let error = load_policy_dir(&tier, ApplyOptions::default())
        .expect_err("a duplicate name within one tier must fail loading");
    assert!(
        matches!(&error, PolicyError::DuplicateName(name) if name == "dup"),
        "expected the duplicate-name error, got {error:?}"
    );

    // And it fails the tiered load as well: no planning ever runs.
    let error = load_policy_tiers(&[tier], &ApplyOptions::default())
        .expect_err("the tiered load must fail before planning");
    assert!(
        matches!(&error, PolicyError::DuplicateName(name) if name == "dup"),
        "expected the duplicate-name error, got {error:?}"
    );

    let _ = fs::remove_dir_all(&tier_dir);
}
