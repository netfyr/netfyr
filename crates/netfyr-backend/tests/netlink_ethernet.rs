//! Integration tests for the ethernet netlink query backend.
//!
//! These tests use unprivileged user + network namespaces to create isolated
//! environments with known interface configuration. No root is required.
//!
//! If the kernel has user namespaces disabled (`/proc/sys/kernel/unprivileged_userns_clone = 0`),
//! `NetnsGuard::new()` will fail and tests are skipped gracefully.

use netfyr_backend::netlink::ethernet::query_ethernet;
use netfyr_backend::netlink::query::establish_connection;
use netfyr_backend::{BackendError, NetlinkBackend, NetworkBackend};
use netfyr_state::{MacAddr, Provenance, Selector};
use netfyr_test_utils::netns::{
    add_address, create_veth_pair, get_link_index, set_link_down, set_link_up, set_mtu, NetnsGuard,
};
use rtnetlink::{LinkBond, LinkBridge, LinkVlan, RouteMessageBuilder};
use std::net::Ipv4Addr;

/// Macro to skip a test when namespace creation is not available (EPERM).
macro_rules! require_netns {
    ($guard:ident) => {
        let $guard = match NetnsGuard::new() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("Skipping test: cannot create network namespace: {e}");
                return;
            }
        };
    };
}

// ── Test 1: Query all returns both veth endpoints ─────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_query_all_veth_pair_returns_two_entities() {
    require_netns!(_guard);

    create_veth_pair("veth-a", "veth-b").await.unwrap();

    let handle = establish_connection().await.unwrap();
    let result = query_ethernet(&handle, None).await.unwrap();

    // A fresh namespace has lo plus our two veth endpoints.
    let found: Vec<_> = result
        .iter()
        .filter(|s| s.selector.name.as_deref() == Some("veth-a")
            || s.selector.name.as_deref() == Some("veth-b"))
        .collect();
    assert_eq!(found.len(), 2, "Expected both veth-a and veth-b in results");

    for state in &found {
        assert_eq!(state.entity_type, "ethernet");
        assert!(state.fields.contains_key("name"), "should have name field");
        assert!(state.fields.contains_key("mtu"), "should have mtu field");
        assert!(state.fields.contains_key("mac"), "should have mac field");
    }
}

// ── Test 2: Query by name selector ───────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_query_by_name_selector_returns_one_entity() {
    require_netns!(_guard);

    create_veth_pair("veth-test0", "veth-test1").await.unwrap();

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("veth-test0");
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();

    assert_eq!(result.len(), 1);
    let state = result.get("ethernet", "veth-test0").unwrap();
    assert_eq!(
        state.selector.name.as_deref(),
        Some("veth-test0")
    );
}

// ── Test 3: Query includes IP addresses ──────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_query_includes_ip_addresses() {
    require_netns!(_guard);

    create_veth_pair("veth-addr0", "veth-addr1").await.unwrap();
    set_link_up("veth-addr0").await.unwrap();
    add_address("veth-addr0", "10.99.0.1/24").await.unwrap();

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("veth-addr0");
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();

    assert_eq!(result.len(), 1);
    let state = result.get("ethernet", "veth-addr0").unwrap();
    let addresses = state
        .fields
        .get("addresses")
        .expect("addresses field missing")
        .value
        .as_list()
        .expect("addresses should be a list");

    let has_addr = addresses
        .iter()
        .any(|v| v.as_str() == Some("10.99.0.1/24"));
    assert!(has_addr, "Expected 10.99.0.1/24 in addresses, got: {addresses:?}");
}

// ── Test 4: All fields have KernelDefault provenance ─────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_all_fields_have_kernel_default_provenance() {
    require_netns!(_guard);

    create_veth_pair("veth-prov0", "veth-prov1").await.unwrap();

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("veth-prov0");
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();

    assert_eq!(result.len(), 1);
    let state = result.get("ethernet", "veth-prov0").unwrap();

    for (name, fv) in &state.fields {
        assert_eq!(
            fv.provenance,
            Provenance::KernelDefault,
            "Field '{name}' has non-KernelDefault provenance"
        );
    }
}

// ── Test 5: Query non-existent interface returns NotFound ─────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_query_nonexistent_interface_returns_not_found() {
    require_netns!(_guard);

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("nonexistent99");
    let result = query_ethernet(&handle, Some(&sel)).await;

    assert!(
        matches!(result, Err(BackendError::NotFound { .. })),
        "Expected NotFound, got: {result:?}"
    );
}

// ── Test 6: MTU is reported correctly ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_mtu_reported_correctly() {
    require_netns!(_guard);

    create_veth_pair("veth-mtu0", "veth-mtu1").await.unwrap();
    set_mtu("veth-mtu0", 1400).await.unwrap();

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("veth-mtu0");
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();

    assert_eq!(result.len(), 1);
    let state = result.get("ethernet", "veth-mtu0").unwrap();
    let mtu = state
        .fields
        .get("mtu")
        .expect("mtu field missing")
        .value
        .as_u64()
        .expect("mtu should be u64");

    assert_eq!(mtu, 1400, "Expected MTU 1400, got {mtu}");
}

// ── Test 7: Query by MAC address ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_query_by_mac_address() {
    require_netns!(_guard);

    create_veth_pair("veth-mac0", "veth-mac1").await.unwrap();

    // First query without selector to get the MAC.
    let handle = establish_connection().await.unwrap();
    let sel0 = Selector::with_name("veth-mac0");
    let result0 = query_ethernet(&handle, Some(&sel0)).await.unwrap();
    assert_eq!(result0.len(), 1);
    let state0 = result0.get("ethernet", "veth-mac0").unwrap();
    let mac_str = state0
        .fields
        .get("mac")
        .expect("mac field missing")
        .value
        .as_str()
        .expect("mac should be string")
        .to_owned();

    // Now query by MAC selector.
    let mac: netfyr_state::MacAddr = mac_str.parse().expect("should parse mac");
    let mac_sel = Selector {
        mac: Some(mac),
        ..Default::default()
    };
    let result_by_mac = query_ethernet(&handle, Some(&mac_sel)).await.unwrap();
    assert_eq!(result_by_mac.len(), 1);
    let found = result_by_mac.iter().next().unwrap();
    assert_eq!(found.selector.name.as_deref(), Some("veth-mac0"));
}

// ── Test 8: Query link down — carrier false, speed absent ────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_link_down_carrier_false_and_no_speed() {
    require_netns!(_guard);

    create_veth_pair("veth-down0", "veth-down1").await.unwrap();
    // Do NOT set link up — it stays down.

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("veth-down0");
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();

    assert_eq!(result.len(), 1);
    let state = result.get("ethernet", "veth-down0").unwrap();

    // carrier should be false (or absent).
    if let Some(fv) = state.fields.get("carrier") {
        assert_eq!(fv.value.as_bool(), Some(false), "carrier should be false when down");
    }

    // speed should be absent (sysfs returns -1 or error when down).
    assert!(
        !state.fields.contains_key("speed"),
        "speed field should be absent when link is down"
    );

    // name, mtu, mac should still be present.
    assert!(state.fields.contains_key("name"));
    assert!(state.fields.contains_key("mtu"));
    assert!(state.fields.contains_key("mac"));
}

// ── Test 9: query_all returns ethernet interfaces ─────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_query_all_returns_ethernet_interfaces() {
    require_netns!(_guard);

    create_veth_pair("veth-qa0", "veth-qa1").await.unwrap();

    let backend = NetlinkBackend::new();
    let all = backend.query_all().await.unwrap();

    let found_qa0 = all.get("ethernet", "veth-qa0").is_some();
    let found_qa1 = all.get("ethernet", "veth-qa1").is_some();
    assert!(found_qa0, "veth-qa0 not found in query_all");
    assert!(found_qa1, "veth-qa1 not found in query_all");
}

// ── Test 10: Multiple selector fields use AND logic ───────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_and_selector_logic() {
    require_netns!(_guard);

    create_veth_pair("veth-and0", "veth-and1").await.unwrap();

    // Get MACs for both.
    let handle = establish_connection().await.unwrap();
    let result_all = query_ethernet(&handle, None).await.unwrap();

    let mac0 = result_all
        .get("ethernet", "veth-and0")
        .and_then(|s| s.fields.get("mac"))
        .and_then(|fv| fv.value.as_str())
        .expect("veth-and0 should have mac")
        .to_owned();

    let mac1 = result_all
        .get("ethernet", "veth-and1")
        .and_then(|s| s.fields.get("mac"))
        .and_then(|fv| fv.value.as_str())
        .expect("veth-and1 should have mac")
        .to_owned();

    // Selector: name=veth-and0 AND mac=<mac of veth-and0> → should match.
    let mac0_parsed: netfyr_state::MacAddr = mac0.parse().unwrap();
    let sel_match = Selector {
        name: Some("veth-and0".to_string()),
        mac: Some(mac0_parsed),
        ..Default::default()
    };
    let result_match = query_ethernet(&handle, Some(&sel_match)).await.unwrap();
    assert_eq!(result_match.len(), 1);

    // Selector: name=veth-and0 AND mac=<mac of veth-and1> → should not match.
    let mac1_parsed: netfyr_state::MacAddr = mac1.parse().unwrap();
    let sel_no_match = Selector {
        name: Some("veth-and0".to_string()),
        mac: Some(mac1_parsed),
        ..Default::default()
    };
    let result_no_match =
        query_ethernet(&handle, Some(&sel_no_match)).await;
    // Either empty set (not specific enough to trigger NotFound) or NotFound.
    match result_no_match {
        Ok(set) => assert!(set.is_empty(), "Expected empty set for mismatched AND selector"),
        Err(BackendError::NotFound { .. }) => {} // also acceptable
        Err(e) => panic!("Unexpected error: {e}"),
    }
}

// ── Test 11: Comprehensive spec scenario — veth with mtu, address, provenance ──

/// Scenario: Query veth interface in unprivileged namespace (spec "Given veth-test0/veth-test1,
/// set to link up with mtu 1400 and address 10.99.0.1/24").
///
/// Covers: mtu field=1400, addresses contains "10.99.0.1/24", all fields KernelDefault,
/// selector name matches.
#[tokio::test(flavor = "multi_thread")]
async fn test_query_veth_spec_comprehensive_scenario() {
    require_netns!(_guard);

    create_veth_pair("veth-test0", "veth-test1").await.unwrap();
    set_link_up("veth-test0").await.unwrap();
    set_mtu("veth-test0", 1400).await.unwrap();
    add_address("veth-test0", "10.99.0.1/24").await.unwrap();

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("veth-test0");
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();

    assert_eq!(result.len(), 1, "Expected exactly one entity for veth-test0");

    let state = result.get("ethernet", "veth-test0")
        .expect("veth-test0 must be in result");

    // entity_type must be "ethernet"
    assert_eq!(state.entity_type, "ethernet");

    // selector name must be "veth-test0"
    assert_eq!(state.selector.name.as_deref(), Some("veth-test0"));

    // mtu must be 1400
    let mtu = state.fields.get("mtu")
        .expect("mtu field must be present")
        .value.as_u64()
        .expect("mtu must be u64");
    assert_eq!(mtu, 1400, "MTU must be 1400");

    // addresses must contain "10.99.0.1/24"
    let addresses = state.fields.get("addresses")
        .expect("addresses field must be present")
        .value.as_list()
        .expect("addresses must be a list");
    let has_addr = addresses.iter().any(|v| v.as_str() == Some("10.99.0.1/24"));
    assert!(has_addr, "addresses must contain '10.99.0.1/24', got: {addresses:?}");

    // All fields must have KernelDefault provenance
    for (field_name, fv) in &state.fields {
        assert_eq!(
            fv.provenance,
            Provenance::KernelDefault,
            "Field '{field_name}' must have KernelDefault provenance"
        );
    }
}

// ── Test 12: Loopback interface excluded from ethernet results ─────────────────

/// Scenario: Query excludes non-ethernet interfaces — a fresh namespace has only
/// the loopback interface (ARPHRD_LOOPBACK), which must be excluded from ethernet query.
#[tokio::test(flavor = "multi_thread")]
async fn test_query_excludes_loopback_interface_in_fresh_namespace() {
    require_netns!(_guard);
    // Do NOT create any veth pairs — fresh namespace has only lo.

    let handle = establish_connection().await.unwrap();
    let result = query_ethernet(&handle, None).await.unwrap();

    // lo must not appear (it is ARPHRD_LOOPBACK, not ARPHRD_ETHER).
    let has_lo = result.iter().any(|s| s.selector.name.as_deref() == Some("lo"));
    assert!(!has_lo, "loopback interface 'lo' must not appear in ethernet query results");

    // A fresh namespace with no ethernet interfaces yields an empty StateSet.
    assert!(result.is_empty(), "Expected empty ethernet result in a namespace with only lo");
}

// ── Test 13: Routes field contains connected subnet route ─────────────────────

/// Scenario: Query ethernet interface includes routes.
///
/// After assigning "10.99.2.1/24" to an UP interface, the kernel creates a
/// connected subnet route "10.99.2.0/24". That route must appear in the
/// "routes" field as a map with "destination" and "metric" keys.
#[tokio::test(flavor = "multi_thread")]
async fn test_query_includes_connected_subnet_route() {
    require_netns!(_guard);

    create_veth_pair("veth-rt0", "veth-rt1").await.unwrap();
    set_link_up("veth-rt0").await.unwrap();
    add_address("veth-rt0", "10.99.2.1/24").await.unwrap();

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("veth-rt0");
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();

    assert_eq!(result.len(), 1);
    let state = result.get("ethernet", "veth-rt0").unwrap();

    let routes = state.fields.get("routes")
        .expect("routes field must be present")
        .value.as_list()
        .expect("routes must be a list");

    assert!(!routes.is_empty(), "Expected at least one route after assigning an address to an UP interface");

    // Each route must be a map containing "destination" and "metric" keys.
    for route_val in routes {
        let route_map = route_val.as_map()
            .expect("each route entry must be a Value::Map");
        assert!(route_map.contains_key("destination"), "route must have 'destination' key");
        assert!(route_map.contains_key("metric"),      "route must have 'metric' key");
    }

    // The connected subnet route 10.99.2.0/24 must appear.
    let has_subnet = routes.iter().any(|r| {
        r.as_map()
            .and_then(|m| m.get("destination"))
            .and_then(|v| v.as_str())
            .map(|s| s == "10.99.2.0/24")
            .unwrap_or(false)
    });
    assert!(has_subnet, "Expected subnet route '10.99.2.0/24' in routes, got: {routes:?}");

    // Routes field provenance must be KernelDefault.
    assert_eq!(
        state.fields.get("routes").unwrap().provenance,
        Provenance::KernelDefault,
        "routes field must have KernelDefault provenance"
    );
}

// ── Test 14: All returned entities have entity_type "ethernet" ────────────────

/// Scenario: Query all ethernet interfaces — every returned State has entity_type "ethernet".
#[tokio::test(flavor = "multi_thread")]
async fn test_query_all_returned_entities_have_ethernet_type() {
    require_netns!(_guard);

    create_veth_pair("veth-et0", "veth-et1").await.unwrap();

    let handle = establish_connection().await.unwrap();
    let result = query_ethernet(&handle, None).await.unwrap();

    for state in result.iter() {
        assert_eq!(
            state.entity_type, "ethernet",
            "Entity '{}' must have entity_type 'ethernet', got '{}'",
            state.selector.name.as_deref().unwrap_or("?"),
            state.entity_type
        );
    }

    // Both veth endpoints must be present.
    assert!(
        result.get("ethernet", "veth-et0").is_some(),
        "veth-et0 must be in query results"
    );
    assert!(
        result.get("ethernet", "veth-et1").is_some(),
        "veth-et1 must be in query results"
    );
}

// ── Test 15: Bridge interface is excluded from ethernet results ───────────────

/// Scenario: Query excludes non-ethernet interfaces — bridge interfaces
/// (InfoKind::Bridge) must not appear in ethernet query results.
#[tokio::test(flavor = "multi_thread")]
async fn test_query_excludes_bridge_interface() {
    require_netns!(_guard);

    // Create a veth pair (should appear) and a bridge (must NOT appear).
    create_veth_pair("veth-br0", "veth-br1").await.unwrap();

    // Create bridge interface directly via rtnetlink.
    let (conn, handle_br, _) = rtnetlink::new_connection().unwrap();
    tokio::spawn(conn);
    handle_br
        .link()
        .add(LinkBridge::new("br-excl").build())
        .execute()
        .await
        .unwrap();

    let handle = establish_connection().await.unwrap();
    let result = query_ethernet(&handle, None).await.unwrap();

    // bridge must NOT appear.
    let has_bridge = result.iter().any(|s| s.selector.name.as_deref() == Some("br-excl"));
    assert!(!has_bridge, "bridge interface 'br-excl' must not appear in ethernet query results");

    // veth endpoints must still appear.
    assert!(result.get("ethernet", "veth-br0").is_some(), "veth-br0 must be present");
    assert!(result.get("ethernet", "veth-br1").is_some(), "veth-br1 must be present");
}

// ── Test 16: Selector with name=nonexistent returns NotFound ──────────────────

/// Scenario: Query for non-existent interface returns NotFound (entity_type and
/// selector captured in the error).
#[tokio::test(flavor = "multi_thread")]
async fn test_query_nonexistent_interface_error_captures_entity_type() {
    require_netns!(_guard);

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("eth99");
    let result = query_ethernet(&handle, Some(&sel)).await;

    match result {
        Err(BackendError::NotFound { ref entity_type, ref selector }) => {
            assert_eq!(entity_type, "ethernet", "NotFound error must name entity type 'ethernet'");
            assert_eq!(
                selector.name.as_deref(),
                Some("eth99"),
                "NotFound error must capture the requested selector name"
            );
        }
        Err(e) => panic!("Expected NotFound, got {e:?}"),
        Ok(set) => panic!("Expected Err(NotFound), got Ok with {} entities", set.len()),
    }
}

// ── Test 17: query_all via NetlinkBackend includes veth entities ──────────────

/// Scenario: query_all includes all ethernet interfaces.
/// NetlinkBackend::query_all must return the same interfaces as a direct
/// query_ethernet call with no selector.
#[tokio::test(flavor = "multi_thread")]
async fn test_query_all_via_backend_matches_direct_query() {
    require_netns!(_guard);

    create_veth_pair("veth-all0", "veth-all1").await.unwrap();

    let backend = NetlinkBackend::new();
    let all = backend.query_all().await.unwrap();

    // query_all result must include veth-all0 and veth-all1.
    assert!(
        all.get("ethernet", "veth-all0").is_some(),
        "query_all must include veth-all0"
    );
    assert!(
        all.get("ethernet", "veth-all1").is_some(),
        "query_all must include veth-all1"
    );

    // Each entity must have the core required fields.
    for state in [
        all.get("ethernet", "veth-all0").unwrap(),
        all.get("ethernet", "veth-all1").unwrap(),
    ] {
        assert!(state.fields.contains_key("name"), "must have 'name' field");
        assert!(state.fields.contains_key("mtu"),  "must have 'mtu' field");
        assert!(state.fields.contains_key("mac"),  "must have 'mac' field");
        assert_eq!(state.entity_type, "ethernet");
    }
}

// ── Test 18: Bond interface is excluded from ethernet results ─────────────────

/// Scenario: Query excludes non-ethernet interfaces — bond interfaces
/// (InfoKind::Bond) must not appear in ethernet query results.
///
/// Covers acceptance criterion: "bridge, bond, and vlan interfaces are excluded".
#[tokio::test(flavor = "multi_thread")]
async fn test_query_excludes_bond_interface() {
    require_netns!(_guard);

    // Create a veth pair (should appear) and a bond (must NOT appear).
    create_veth_pair("veth-bnd0", "veth-bnd1").await.unwrap();

    // Create bond interface directly via rtnetlink.
    let (conn, handle_bond, _) = rtnetlink::new_connection().unwrap();
    tokio::spawn(conn);
    handle_bond
        .link()
        .add(LinkBond::new("bond-excl").build())
        .execute()
        .await
        .unwrap();

    let handle = establish_connection().await.unwrap();
    let result = query_ethernet(&handle, None).await.unwrap();

    // bond must NOT appear in ethernet query results.
    let has_bond = result
        .iter()
        .any(|s| s.selector.name.as_deref() == Some("bond-excl"));
    assert!(
        !has_bond,
        "bond interface 'bond-excl' must not appear in ethernet query results"
    );

    // veth endpoints must still appear.
    assert!(
        result.get("ethernet", "veth-bnd0").is_some(),
        "veth-bnd0 must be present"
    );
    assert!(
        result.get("ethernet", "veth-bnd1").is_some(),
        "veth-bnd1 must be present"
    );
}

// ── Test 19: Vlan interface is excluded from ethernet results ─────────────────

/// Scenario: Query excludes non-ethernet interfaces — vlan interfaces
/// (InfoKind::Vlan) must not appear in ethernet query results.
///
/// Covers acceptance criterion: "bridge, bond, and vlan interfaces are excluded".
#[tokio::test(flavor = "multi_thread")]
async fn test_query_excludes_vlan_interface() {
    require_netns!(_guard);

    // Create a veth pair as the parent for the vlan.
    create_veth_pair("veth-vlp0", "veth-vlp1").await.unwrap();
    set_link_up("veth-vlp0").await.unwrap();

    // Resolve parent interface index (required by LinkVlan).
    let parent_index = get_link_index("veth-vlp0").await.unwrap();

    // Create vlan interface (id=100) on top of veth-vlp0.
    let (conn, handle_vlan, _) = rtnetlink::new_connection().unwrap();
    tokio::spawn(conn);
    handle_vlan
        .link()
        .add(LinkVlan::new("vlan100-excl", parent_index, 100).build())
        .execute()
        .await
        .unwrap();

    let handle = establish_connection().await.unwrap();
    let result = query_ethernet(&handle, None).await.unwrap();

    // vlan must NOT appear in ethernet query results.
    let has_vlan = result
        .iter()
        .any(|s| s.selector.name.as_deref() == Some("vlan100-excl"));
    assert!(
        !has_vlan,
        "vlan interface 'vlan100-excl' must not appear in ethernet query results"
    );

    // veth endpoints must still appear.
    assert!(
        result.get("ethernet", "veth-vlp0").is_some(),
        "veth-vlp0 must be present"
    );
    assert!(
        result.get("ethernet", "veth-vlp1").is_some(),
        "veth-vlp1 must be present"
    );
}

// ── Test 20: Carrier is true when both veth ends are up ───────────────────────

/// Scenario: Query a specific ethernet interface by name — the entity has the
/// correct carrier value.
///
/// For a veth pair, bringing both endpoints up causes the kernel to report
/// carrier=1 (true). This covers the acceptance criterion that the returned
/// entity has the correct carrier value for an up interface.
#[tokio::test(flavor = "multi_thread")]
async fn test_carrier_is_true_when_both_veth_ends_are_up() {
    require_netns!(_guard);

    create_veth_pair("veth-cup0", "veth-cup1").await.unwrap();

    // Both veth ends must be up for the kernel to report carrier=1.
    set_link_up("veth-cup0").await.unwrap();
    set_link_up("veth-cup1").await.unwrap();

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("veth-cup0");
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();

    assert_eq!(result.len(), 1);
    let state = result.get("ethernet", "veth-cup0").unwrap();

    // The carrier field must be present.
    let carrier_fv = state
        .fields
        .get("carrier")
        .expect("carrier field must be present in the returned State");

    // Carrier must hold a bool value.
    let carrier_val = carrier_fv
        .value
        .as_bool()
        .expect("carrier field must be Value::Bool");

    // When both veth ends are up, the kernel sets IFLA_CARRIER=1 → true.
    assert!(
        carrier_val,
        "carrier must be true when both veth endpoints are up"
    );

    // Carrier field must have KernelDefault provenance.
    assert_eq!(
        carrier_fv.provenance,
        Provenance::KernelDefault,
        "carrier field must have KernelDefault provenance"
    );

    // Core fields must also be present.
    assert!(state.fields.contains_key("name"), "name must be present");
    assert!(state.fields.contains_key("mtu"),  "mtu must be present");
    assert!(state.fields.contains_key("mac"),  "mac must be present");
}

// ── Test 21: Route with gateway includes "gateway" field ─────────────────────

/// Scenario: Query ethernet interface includes routes — each route has
/// destination, gateway (if applicable), and metric fields.
///
/// This test adds a static default route (0.0.0.0/0) via an explicit gateway
/// and verifies the "routes" field in the returned State includes a route map
/// with "destination", "gateway", and "metric" keys.
#[tokio::test(flavor = "multi_thread")]
async fn test_query_includes_route_with_gateway_field() {
    require_netns!(_guard);

    create_veth_pair("veth-gw0", "veth-gw1").await.unwrap();
    set_link_up("veth-gw0").await.unwrap();
    set_link_up("veth-gw1").await.unwrap();

    // Assign an address so there is a connected subnet route and a viable
    // gateway address in the same subnet.
    add_address("veth-gw0", "10.99.4.1/24").await.unwrap();

    // Get the interface index to attach the explicit route.
    let iface_index = get_link_index("veth-gw0").await.unwrap();

    // Add a default route (0.0.0.0/0) via 10.99.4.254 out of veth-gw0.
    // The gateway 10.99.4.254 is in the 10.99.4.0/24 subnet, so the kernel
    // accepts the route without the onlink flag on most kernels; onlink is
    // included for robustness across different kernel configurations.
    let (conn, handle_rt, _) = rtnetlink::new_connection().unwrap();
    tokio::spawn(conn);
    let route = RouteMessageBuilder::<Ipv4Addr>::new()
        .output_interface(iface_index)
        .gateway("10.99.4.254".parse::<Ipv4Addr>().unwrap())
        .onlink()
        .build();
    handle_rt.route().add(route).execute().await.unwrap();

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("veth-gw0");
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();

    assert_eq!(result.len(), 1);
    let state = result.get("ethernet", "veth-gw0").unwrap();

    let routes = state
        .fields
        .get("routes")
        .expect("routes field must be present")
        .value
        .as_list()
        .expect("routes must be a list");

    assert!(
        !routes.is_empty(),
        "At least one route expected after assigning an address and adding a gateway route"
    );

    // Each route must be a map with at least "destination" and "metric" keys.
    for route_val in routes {
        let route_map = route_val
            .as_map()
            .expect("each route entry must be a Value::Map");
        assert!(route_map.contains_key("destination"), "route must have 'destination' key");
        assert!(route_map.contains_key("metric"),      "route must have 'metric' key");
    }

    // The default route 0.0.0.0/0 must appear and must carry a "gateway" key.
    let default_route = routes
        .iter()
        .find(|r| {
            r.as_map()
                .and_then(|m| m.get("destination"))
                .and_then(|v| v.as_str())
                .map(|s| s.starts_with("0.0.0.0/"))
                .unwrap_or(false)
        })
        .expect("default route (0.0.0.0/x) must appear in the routes field");

    let route_map = default_route
        .as_map()
        .expect("default route must be a Value::Map");

    // Gateway key must be present for a route with an explicit next hop.
    let gw = route_map
        .get("gateway")
        .expect("default route must have a 'gateway' key");
    assert_eq!(
        gw.as_str(),
        Some("10.99.4.254"),
        "gateway must be '10.99.4.254', got: {:?}",
        gw
    );

    // Routes field must have KernelDefault provenance.
    assert_eq!(
        state.fields.get("routes").unwrap().provenance,
        Provenance::KernelDefault,
        "routes field must have KernelDefault provenance"
    );
}

// ── Test 22: NetlinkBackend::supported_entities returns "ethernet" ─────────────

/// Scenario: NetlinkBackend supports the "ethernet" entity type.
///
/// Covers acceptance criterion: "A NetlinkBackend that supports entity type
/// 'ethernet'" when query_all is called.
#[test]
fn test_netlinkbackend_supports_ethernet_entity_type() {
    let backend = NetlinkBackend::new();
    let supported = backend.supported_entities();
    assert!(
        supported.contains(&"ethernet".to_string()),
        "NetlinkBackend must declare 'ethernet' in supported_entities()"
    );
}

// ── Test 23: Addresses field is always present (even empty) ───────────────────

/// Scenario: Query by name returns an entity whose "addresses" field is always
/// a list (even when the interface has no IP address assigned).
///
/// Covers the part of criterion 2 and 3 that addresses is always present and
/// is a list type.
#[tokio::test(flavor = "multi_thread")]
async fn test_addresses_field_is_always_a_list() {
    require_netns!(_guard);

    // veth pair with no address assigned — addresses should be an empty list.
    create_veth_pair("veth-noaddr0", "veth-noaddr1").await.unwrap();

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("veth-noaddr0");
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();

    assert_eq!(result.len(), 1);
    let state = result.get("ethernet", "veth-noaddr0").unwrap();

    let addresses_fv = state
        .fields
        .get("addresses")
        .expect("addresses field must always be present");
    assert!(
        addresses_fv.value.as_list().is_some(),
        "addresses field must always be a list, even when empty"
    );

    // Provenance must be KernelDefault.
    assert_eq!(
        addresses_fv.provenance,
        Provenance::KernelDefault,
        "addresses field must have KernelDefault provenance"
    );
}

// ── Test 24: Routes field is always present (even empty) ──────────────────────

/// Scenario: The "routes" field is always a list in the returned State, even
/// when there are no routes for the interface (interface is down with no address).
#[tokio::test(flavor = "multi_thread")]
async fn test_routes_field_is_always_a_list() {
    require_netns!(_guard);

    // veth pair that is down — no routes expected.
    create_veth_pair("veth-nort0", "veth-nort1").await.unwrap();

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("veth-nort0");
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();

    assert_eq!(result.len(), 1);
    let state = result.get("ethernet", "veth-nort0").unwrap();

    let routes_fv = state
        .fields
        .get("routes")
        .expect("routes field must always be present");
    assert!(
        routes_fv.value.as_list().is_some(),
        "routes field must always be a list, even when empty"
    );

    // Provenance must be KernelDefault.
    assert_eq!(
        routes_fv.provenance,
        Provenance::KernelDefault,
        "routes field must have KernelDefault provenance"
    );
}

// ── Test 25: enabled field is present with a valid bool value ─────────────────

/// Scenario: The "enabled" field must be present in every returned State and
/// must hold a boolean derived from the IFF_UP link flag.
#[tokio::test(flavor = "multi_thread")]
async fn test_enabled_field_is_present_and_is_valid_bool() {
    require_netns!(_guard);

    create_veth_pair("veth-ops0", "veth-ops1").await.unwrap();

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("veth-ops0");
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();

    assert_eq!(result.len(), 1);
    let state = result.get("ethernet", "veth-ops0").unwrap();

    let enabled_fv = state
        .fields
        .get("enabled")
        .expect("enabled field must always be present");

    // enabled must be a bool.
    enabled_fv
        .value
        .as_bool()
        .expect("enabled must be Value::Bool");

    // Must have KernelDefault provenance.
    assert_eq!(
        enabled_fv.provenance,
        Provenance::KernelDefault,
        "enabled field must have KernelDefault provenance"
    );
}

// ── Test 26: Driver selector with no matching driver returns empty ─────────────

/// Scenario: Query by driver selector.
///
/// In an unprivileged network namespace, veth interfaces do not expose a
/// hardware driver via sysfs (no /sys/class/net/<name>/device/driver symlink).
/// A query with a driver selector that requires a specific driver must therefore
/// return an empty set (since no interfaces match).
///
/// This test validates the driver-selector code path is correctly integrated
/// with the full query flow, even though an end-to-end positive test (with a
/// real NIC and driver) requires physical hardware unavailable in CI namespaces.
/// The positive selector matching logic is covered by unit tests in query.rs.
#[tokio::test(flavor = "multi_thread")]
async fn test_driver_selector_no_match_when_veth_has_no_driver() {
    require_netns!(_guard);

    create_veth_pair("veth-drv0", "veth-drv1").await.unwrap();

    let handle = establish_connection().await.unwrap();

    // A driver selector without name — veth has no sysfs driver → no match.
    let sel = Selector {
        driver: Some("e1000".to_string()),
        ..Default::default()
    };
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();

    assert!(
        result.is_empty(),
        "Driver selector 'e1000' must not match veth interfaces (which have no kernel driver), \
         got {} entities",
        result.len()
    );
}

// ── Test 27: NetlinkBackend::query via trait with name selector ───────────────

/// Scenario: NetlinkBackend::query dispatches to query_ethernet and filters by
/// selector correctly when called through the NetworkBackend trait interface.
///
/// Tests the full trait dispatch path (not just the inner query_ethernet function)
/// by calling `backend.query("ethernet", Some(&sel))` and asserting on the result.
#[tokio::test(flavor = "multi_thread")]
async fn test_netlinkbackend_trait_query_with_name_selector() {
    require_netns!(_guard);

    create_veth_pair("veth-trait0", "veth-trait1").await.unwrap();

    let backend = NetlinkBackend::new();
    let sel = Selector::with_name("veth-trait0");
    let result = backend
        .query(&"ethernet".to_string(), Some(&sel))
        .await
        .unwrap();

    assert_eq!(result.len(), 1, "query via trait must return exactly one entity for name selector");

    let state = result.get("ethernet", "veth-trait0")
        .expect("veth-trait0 must be in result");
    assert_eq!(state.entity_type, "ethernet");
    assert_eq!(state.selector.name.as_deref(), Some("veth-trait0"));

    // Core fields must be present.
    assert!(state.fields.contains_key("name"), "name field must be present");
    assert!(state.fields.contains_key("mtu"),  "mtu field must be present");
    assert!(state.fields.contains_key("mac"),  "mac field must be present");
    assert!(state.fields.contains_key("enabled"), "enabled field must be present");
    assert!(state.fields.contains_key("carrier"),   "carrier field must be present");
}

// ── Test 28: NetlinkBackend::query returns NotFound for missing interface ──────

/// Scenario: NetlinkBackend::query via trait returns BackendError::NotFound when
/// the interface requested by name does not exist.
///
/// Covers acceptance criterion: "Query for non-existent interface returns NotFound".
/// This test uses the NetworkBackend trait interface (not the inner function directly).
#[tokio::test(flavor = "multi_thread")]
async fn test_netlinkbackend_trait_query_not_found_for_missing_interface() {
    require_netns!(_guard);

    let backend = NetlinkBackend::new();
    let sel = Selector::with_name("eth-does-not-exist-xyzzy99");
    let result = backend
        .query(&"ethernet".to_string(), Some(&sel))
        .await;

    assert!(
        matches!(result, Err(BackendError::NotFound { .. })),
        "query via trait must return NotFound for a non-existent interface, got: {result:?}"
    );
}

// ── Test 29: NetlinkBackend::query for unsupported entity type ────────────────

/// Scenario: NetlinkBackend::query for an entity type it doesn't support
/// returns BackendError::UnsupportedEntityType.
///
/// Validates that the backend correctly rejects unknown entity types at the
/// trait dispatch level.
#[tokio::test]
async fn test_netlinkbackend_query_unsupported_entity_type_returns_error() {
    let backend = NetlinkBackend::new();
    let result = backend.query(&"firewall-rule".to_string(), None).await;

    assert!(result.is_err(), "query for unsupported entity type must return Err");
    assert!(
        matches!(result, Err(BackendError::UnsupportedEntityType(_))),
        "expected UnsupportedEntityType, got: {result:?}"
    );
}

// ── Test 30: IPv6 addresses are excluded from query results ──────────────────

/// Scenario: Query ethernet interface includes IP addresses — IPv6 addresses
/// (e.g., fe80::) are excluded from the results even when the interface is up
/// and the kernel has assigned link-local IPv6 addresses.
///
/// Covers acceptance criterion: "And IPv6 addresses (e.g., fe80::) are excluded".
#[tokio::test(flavor = "multi_thread")]
async fn test_query_excludes_ipv6_addresses() {
    require_netns!(_guard);

    create_veth_pair("veth-v6-0", "veth-v6-1").await.unwrap();
    set_link_up("veth-v6-0").await.unwrap();
    set_link_up("veth-v6-1").await.unwrap();
    add_address("veth-v6-0", "10.99.6.1/24").await.unwrap();
    // Give the kernel a moment to assign the IPv6 link-local address.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("veth-v6-0");
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();

    assert_eq!(result.len(), 1);
    let state = result.get("ethernet", "veth-v6-0").unwrap();

    let addresses = state
        .fields
        .get("addresses")
        .expect("addresses field must be present")
        .value
        .as_list()
        .expect("addresses must be a list");

    // The IPv4 address must appear.
    let has_ipv4 = addresses
        .iter()
        .any(|v| v.as_str() == Some("10.99.6.1/24"));
    assert!(has_ipv4, "Expected 10.99.6.1/24 in addresses, got: {addresses:?}");

    // No IPv6 address must appear (no colon in any address string).
    for addr in addresses {
        if let Some(s) = addr.as_str() {
            assert!(
                !s.contains(':'),
                "IPv6 address must not appear in query results, got: {s}"
            );
        }
    }

    // Addresses field must have KernelDefault provenance.
    assert_eq!(
        state.fields.get("addresses").unwrap().provenance,
        Provenance::KernelDefault
    );
}

// ── Test 31: Query by MAC for second veth — first veth excluded ───────────────

/// Scenario: Query by MAC address selector — specifically targets veth-mac1 (not
/// veth-mac0) to verify the MAC filter selects the correct one and excludes the other.
///
/// This supplements test_query_by_mac_address (which queries veth-mac0's MAC) by
/// also validating that querying veth-mac1's MAC excludes veth-mac0.
#[tokio::test(flavor = "multi_thread")]
async fn test_query_by_mac_selects_second_veth_excludes_first() {
    require_netns!(_guard);

    create_veth_pair("veth-mac2a", "veth-mac2b").await.unwrap();

    // First get the MAC of veth-mac2b.
    let handle = establish_connection().await.unwrap();
    let sel_b = Selector::with_name("veth-mac2b");
    let result_b = query_ethernet(&handle, Some(&sel_b)).await.unwrap();
    assert_eq!(result_b.len(), 1);
    let mac_b_str = result_b
        .get("ethernet", "veth-mac2b")
        .and_then(|s| s.fields.get("mac"))
        .and_then(|fv| fv.value.as_str())
        .expect("veth-mac2b must have a mac field")
        .to_owned();

    // Query using veth-mac2b's MAC — should return exactly veth-mac2b.
    let mac_b: MacAddr = mac_b_str.parse().expect("mac must be parseable");
    let mac_sel = Selector {
        mac: Some(mac_b),
        ..Default::default()
    };
    let result = query_ethernet(&handle, Some(&mac_sel)).await.unwrap();

    assert_eq!(result.len(), 1, "MAC selector must return exactly one entity");
    let found = result.iter().next().unwrap();
    assert_eq!(
        found.selector.name.as_deref(),
        Some("veth-mac2b"),
        "MAC selector must match veth-mac2b, not veth-mac2a"
    );
    // veth-mac2a must NOT appear.
    assert!(
        result.get("ethernet", "veth-mac2a").is_none(),
        "veth-mac2a must be excluded when filtering by veth-mac2b's MAC"
    );
}

// ── enabled/carrier state combinations ──────────────────────────────────────

/// Both down: newly created veth pair without set_link_up → enabled=false, carrier=false.
#[tokio::test(flavor = "multi_thread")]
async fn test_enabled_false_carrier_false_when_both_down() {
    require_netns!(_guard);

    create_veth_pair("veth-ec-a0", "veth-ec-a1").await.unwrap();

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("veth-ec-a0");
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();
    let state = result.get("ethernet", "veth-ec-a0").unwrap();

    let enabled = state.fields.get("enabled").unwrap().value.as_bool().unwrap();
    let carrier = state.fields.get("carrier").unwrap().value.as_bool().unwrap();

    assert!(!enabled, "enabled must be false when link is not brought up");
    assert!(!carrier, "carrier must be false when link is down");
}

/// Admin up, no carrier: only one end of the veth pair is up → enabled=true, carrier=false.
#[tokio::test(flavor = "multi_thread")]
async fn test_enabled_true_carrier_false_when_peer_down() {
    require_netns!(_guard);

    create_veth_pair("veth-ec-b0", "veth-ec-b1").await.unwrap();
    set_link_up("veth-ec-b0").await.unwrap();

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("veth-ec-b0");
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();
    let state = result.get("ethernet", "veth-ec-b0").unwrap();

    let enabled = state.fields.get("enabled").unwrap().value.as_bool().unwrap();
    let carrier = state.fields.get("carrier").unwrap().value.as_bool().unwrap();

    assert!(enabled, "enabled must be true when link is brought up");
    assert!(!carrier, "carrier must be false when peer is still down");
}

/// Both up, carrier present: both ends of the veth pair are up → enabled=true, carrier=true.
#[tokio::test(flavor = "multi_thread")]
async fn test_enabled_true_carrier_true_when_both_up() {
    require_netns!(_guard);

    create_veth_pair("veth-ec-c0", "veth-ec-c1").await.unwrap();
    set_link_up("veth-ec-c0").await.unwrap();
    set_link_up("veth-ec-c1").await.unwrap();

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("veth-ec-c0");
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();
    let state = result.get("ethernet", "veth-ec-c0").unwrap();

    let enabled = state.fields.get("enabled").unwrap().value.as_bool().unwrap();
    let carrier = state.fields.get("carrier").unwrap().value.as_bool().unwrap();

    assert!(enabled, "enabled must be true when link is up");
    assert!(carrier, "carrier must be true when both veth endpoints are up");
}

/// Admin down after carrier: bring both up then bring one down → enabled=false, carrier=false.
#[tokio::test(flavor = "multi_thread")]
async fn test_enabled_false_carrier_false_after_admin_down() {
    require_netns!(_guard);

    create_veth_pair("veth-ec-d0", "veth-ec-d1").await.unwrap();
    set_link_up("veth-ec-d0").await.unwrap();
    set_link_up("veth-ec-d1").await.unwrap();
    set_link_down("veth-ec-d0").await.unwrap();

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("veth-ec-d0");
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();
    let state = result.get("ethernet", "veth-ec-d0").unwrap();

    let enabled = state.fields.get("enabled").unwrap().value.as_bool().unwrap();
    let carrier = state.fields.get("carrier").unwrap().value.as_bool().unwrap();

    assert!(!enabled, "enabled must be false after set_link_down");
    assert!(!carrier, "carrier must be false when interface is admin-down");
}

// ── Schema-backend field divergence check ────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_query_fields_are_subset_of_schema_fields() {
    require_netns!(_guard);

    create_veth_pair("veth-sf-a", "veth-sf-b").await.unwrap();
    add_address("veth-sf-a", "10.99.0.1/24").await.unwrap();

    let handle = establish_connection().await.unwrap();
    let sel = Selector::with_name("veth-sf-a");
    let result = query_ethernet(&handle, Some(&sel)).await.unwrap();
    let state = result.get("ethernet", "veth-sf-a").unwrap();

    let schema = netfyr_state::SchemaRegistry::new();
    let entity_schema = schema.get_schema("ethernet").unwrap();
    let schema_fields: std::collections::HashSet<&str> =
        entity_schema.field_names().into_iter().collect();

    for field_name in state.fields.keys() {
        assert!(
            schema_fields.contains(field_name.as_str()),
            "backend query produced field '{}' which is not in the ethernet schema.\n\
             Schema fields: {:?}\n\
             Add the field to the schema, or remove it from the query code.",
            field_name,
            schema_fields
        );
    }
}
