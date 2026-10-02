import pytest

from backend_harness import (
    add_addresses, address_entry, addresses, apply, assert_rebuilt_group, cidrs, ip, links,
    operation, query, require_success, stable_state,
)


@pytest.mark.tier1
@pytest.mark.query
@pytest.mark.apply
def test_namespace_binding_and_unmanaged_preservation(environment):
    (left, right), probe = environment(2)
    ip(left, "link", "set", "dev", "a", "mtu", "1400")
    ip(right, "link", "set", "dev", "a", "mtu", "1600")
    assert query(probe, 0, name="a")[0]["fields"]["mtu"] == 1400
    assert query(probe, 1, name="a")[0]["fields"]["mtu"] == 1600
    other_namespace = stable_state(right)
    unmanaged = links(left)["u"]
    require_success(apply(probe, operation("mtu", value=9000)), changed=[0])
    assert links(left)["a"]["mtu"] == 9000
    assert links(left)["u"] == unmanaged
    assert stable_state(right) == other_namespace


@pytest.mark.tier1
@pytest.mark.query
@pytest.mark.apply
def test_second_backend_apply_lands_in_its_namespace(environment):
    (left, right), probe = environment(2)
    query(probe, 1)
    first_namespace = stable_state(left)
    unmanaged = links(right)["u"]
    require_success(apply(probe, operation("mtu", value=9000), backend=1), changed=[0])
    assert links(right)["a"]["mtu"] == 9000
    assert links(right)["u"] == unmanaged
    assert stable_state(left) == first_namespace


@pytest.mark.tier1
@pytest.mark.query
@pytest.mark.ipv4
def test_query_inventory_selectors_and_optional_fields(environment):
    (namespace,), probe = environment()
    add_addresses(namespace, "10.44.0.1/24", "10.44.0.2/24", "169.254.20.30/16")
    ip(namespace, "addr", "add", "10.55.0.1/24", "dev", "a", "valid_lft", "300", "preferred_lft", "200")
    ip(namespace, "-6", "addr", "add", "2001:db8::1/64", "dev", "a")
    item = query(probe, name="a")[0]
    assert item["type"] == "ethernet"
    assert item["source"] == "Kernel"
    assert item["addresses_complete"] is True
    assert query(probe, name="lo")[0]["addresses_complete"] is False
    observed = item["fields"]["ipv4"]["addresses"]
    assert [address["ip"] for address in observed] == cidrs(namespace)
    timed = next(address for address in observed if address["ip"] == "10.55.0.1/24")
    assert 0 < timed["preferred_lft"] <= 200
    assert 0 < timed["valid_lft"] <= 300
    mac = links(namespace)["a"]["address"].upper()
    assert len(query(probe, name="a", mac=mac)) == 1
    assert probe.request("query", selector={"name": "doesnotexist"})["error"]["kind"] == "NotFound"
    veths = {"a", "b", "u", "v"}
    assert {entry["name"] for entry in query(probe, type="ethernet")} == veths
    assert {entry["name"] for entry in query(probe, driver="veth")} == veths
    assert query(probe, driver="driver-does-not-exist") == []
    mismatch = probe.request("query", selector={"name": "a", "mac": "00:00:00:00:00:00"})
    assert "error" not in mismatch and "probe_error" not in mismatch, mismatch
    assert mismatch["interfaces"] == []
    assert query(probe, pci_path="missing") == []
    ethernet = item["fields"]["ethernet"]
    assert ethernet["speed"] == 10000
    assert ethernet["duplex"] == "full"
    assert ethernet["autoneg"] is False


@pytest.mark.tier1
@pytest.mark.query
@pytest.mark.apply
def test_unsupported_links_keep_type_and_reject_mutation(environment):
    (namespace,), probe = environment()
    ip(namespace, "link", "add", "name", "br", "type", "bridge")
    ip(namespace, "link", "add", "name", "dummy", "type", "dummy")
    ip(namespace, "link", "add", "link", "a", "name", "vlan", "type", "vlan", "id", "10")
    add_addresses(namespace, "10.66.0.1/24", device="br")
    inventory = {item["name"]: item for item in query(probe)}
    for name, kind in (("br", "bridge"), ("dummy", "dummy"), ("vlan", "vlan"), ("lo", "loopback")):
        assert inventory[name]["type"] == kind
        assert "ipv4" not in inventory[name]["fields"]
        assert "ethernet" not in inventory[name]["fields"]
    before = stable_state(namespace)
    report = apply(probe, *[operation("mtu", target=name, value=1300) for name in ("br", "dummy", "vlan", "lo")])
    assert not report["success"]
    assert len(report["failed"]) == 4
    assert all(entry["error"]["kind"] == "UnsupportedEntityType" for entry in report["failed"])
    assert stable_state(namespace) == before


@pytest.mark.tier1
@pytest.mark.apply
@pytest.mark.ipv4
def test_apply_idempotence_and_field_preservation(environment):
    (namespace,), probe = environment()
    add_addresses(namespace, "10.55.0.1/24")
    query(probe)
    report = apply(probe, operation("mtu", value=9000), operation("add", address="10.44.0.1/24"))
    require_success(report, changed=[0, 1])
    assert links(namespace)["a"]["mtu"] == 9000
    assert "UP" in links(namespace)["a"]["flags"]
    assert set(cidrs(namespace)) == {"10.55.0.1/24", "10.44.0.1/24"}
    before = stable_state(namespace)
    report = apply(probe, operation("mtu", value=9000), operation("enabled", value=True), operation("add", address="10.44.0.1/24"), operation("remove", address="192.0.2.1/24"))
    require_success(report, changed=[], skipped=[0, 1, 2, 3])
    assert all(entry["reason"] == "AlreadySatisfied" for entry in report["skipped"])
    assert stable_state(namespace) == before


@pytest.mark.tier1
@pytest.mark.apply
@pytest.mark.ipv4
def test_remove_secondary_preserves_device_admin_and_other_prefix(environment):
    (namespace,), probe = environment()
    add_addresses(namespace, "10.44.0.1/24", "10.44.0.2/24", "10.55.0.1/24")
    query(probe)
    before = links(namespace)["a"]
    require_success(apply(probe, operation("remove", address="10.44.0.2/24")), changed=[0])
    assert cidrs(namespace) == ["10.44.0.1/24", "10.55.0.1/24"]
    assert links(namespace)["a"] == before


@pytest.mark.tier2
@pytest.mark.apply
@pytest.mark.ipv4
def test_explicit_same_prefix_rebuild_preserves_other_groups(environment):
    (namespace,), probe = environment()
    add_addresses(namespace, "10.44.0.1/24", "10.44.0.2/24", "10.55.0.1/24")
    other = address_entry(namespace, "10.55.0.1/24")
    query(probe)
    operations = [
        operation("remove", address="10.44.0.2/24"),
        operation("remove", address="10.44.0.1/24", dependencies=[0]),
        operation("add", address="10.44.0.2/24", dependencies=[1]),
        operation("add", address="10.44.0.1/24", dependencies=[2]),
    ]
    require_success(apply(probe, *operations), changed=[0, 1, 2, 3])
    assert_rebuilt_group(namespace, other)


@pytest.mark.tier2
@pytest.mark.apply
@pytest.mark.ipv4
def test_stale_address_group_blocks_dependents_preserves_independent_progress(environment):
    (namespace,), probe = environment()
    add_addresses(namespace, "10.44.0.1/24")
    query(probe)
    add_addresses(namespace, "10.44.0.2/24")
    report = apply(probe, operation("remove", address="10.44.0.1/24"), operation("add", address="10.44.0.3/24", dependencies=[0]), operation("mtu", target="u", value=1400))
    assert not report["success"]
    assert report["failed"][0]["error"]["kind"] == "StaleState"
    assert report["failed"][0]["context"]["index"] == 0
    assert report["skipped"][0]["context"]["index"] == 1
    assert report["skipped"][0]["reason"] == "DependencyFailed"
    assert [entry["index"] for entry in report["succeeded"]] == [2]
    assert cidrs(namespace) == ["10.44.0.1/24", "10.44.0.2/24"]
    assert links(namespace)["u"]["mtu"] == 1400


@pytest.mark.tier2
@pytest.mark.apply
def test_replaced_target_not_mutated_and_independent_work_continues(environment):
    (namespace,), probe = environment()
    query(probe)
    original = links(namespace)["a"]["ifindex"]
    ip(namespace, "link", "del", "dev", "a")
    ip(namespace, "link", "add", "name", "a", "type", "veth", "peer", "name", "b")
    assert original not in {link["ifindex"] for link in links(namespace).values()}
    report = apply(probe, operation("mtu", value=9000), operation("enabled", value=True, dependencies=[0]), operation("mtu", target="u", value=1400))
    assert not report["success"]
    assert report["failed"][0]["error"]["kind"] == "NotFound"
    assert [entry["index"] for entry in report["succeeded"]] == [2]
    assert report["skipped"][0]["reason"] == "DependencyFailed"
    assert links(namespace)["a"]["mtu"] == 1500
    assert "UP" not in links(namespace)["a"]["flags"]
    assert links(namespace)["u"]["mtu"] == 1400


@pytest.mark.tier2
@pytest.mark.apply
@pytest.mark.ipv4
def test_dry_run_exposes_operations_and_never_mutates(environment):
    (namespace,), probe = environment()
    add_addresses(namespace, "10.44.0.1/24")
    query(probe)
    before = stable_state(namespace)
    operations = [operation("mtu", value=9000), operation("remove", address="10.44.0.1/24"), operation("add", address="10.44.0.2/24", dependencies=[1])]
    report = probe.request("dry_run", operations=operations)
    assert report["success"], report
    assert [entry["context"]["index"] for entry in report["operations"]] == [0, 1, 2]
    assert report["changes"], report
    assert stable_state(namespace) == before
    original = links(namespace)["a"]["ifindex"]
    ip(namespace, "link", "del", "dev", "a")
    assert original not in {link["ifindex"] for link in links(namespace).values()}
    invalid = probe.request("dry_run", operations=[operation("mtu", value=9000)])
    assert not invalid["success"]
    assert invalid["failed"][0]["error"]["kind"] == "NotFound"


@pytest.mark.tier2
@pytest.mark.apply
def test_read_only_skip_unknown_writable_failure_and_administrative_state(environment):
    (namespace,), probe = environment()
    query(probe)
    report = apply(probe, operation("readonly", field="carrier", value=False), operation("unknown", field="imaginary-writable", value=1), operation("enabled", value=False))
    assert not report["success"]
    assert report["skipped"][0]["reason"] == "ReadOnly"
    assert report["failed"][0]["error"]["kind"] == "UnsupportedOperation"
    assert [entry["index"] for entry in report["succeeded"]] == [2]
    assert "UP" not in links(namespace)["a"]["flags"]
    require_success(apply(probe, operation("enabled", value=True)), changed=[0])
    assert "UP" in links(namespace)["a"]["flags"]


@pytest.mark.tier2
@pytest.mark.apply
@pytest.mark.ipv4
def test_different_existing_lifetimes_require_explicit_replacement(environment):
    (namespace,), probe = environment()
    add_addresses(namespace, "10.44.0.1/24")
    query(probe)
    report = apply(probe, operation("add", address="10.44.0.1/24", valid_lft=300, preferred_lft=200))
    assert not report["success"]
    assert not report["succeeded"] and not report["skipped"]
    assert len(report["failed"]) == 1
    assert report["failed"][0]["error"]["kind"] == "ApplyFailed"
    assert addresses(namespace)[0]["valid_life_time"] == 4294967295
    report = apply(probe, operation("replace", address="10.44.0.1/24", valid_lft=300, preferred_lft=200))
    require_success(report, changed=[0])
    changed = addresses(namespace)[0]
    assert 0 < changed["valid_life_time"] <= 300
    assert 0 < changed["preferred_life_time"] <= 200
