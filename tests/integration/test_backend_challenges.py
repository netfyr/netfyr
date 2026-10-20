import errno

import pytest

from backend_harness import (
    add_addresses, address_entry, addresses, apply, assert_rebuilt_group, cidrs, ip, links,
    operation, query, require_success, set_promote_secondaries, stable_state,
)


@pytest.mark.tier2
@pytest.mark.apply
def test_reused_index_name_and_mac_does_not_reuse_identity(environment):
    (namespace,), probe = environment()
    query(probe)
    original = links(namespace)["a"]
    ip(namespace, "link", "del", "dev", "a")
    ip(namespace, "link", "add", "name", "a", "index", str(original["ifindex"]), "address", original["address"], "type", "veth", "peer", "name", "b")
    replacement = links(namespace)["a"]
    assert replacement["ifindex"] == original["ifindex"]
    assert replacement["address"] == original["address"]
    report = apply(probe, operation("mtu", value=9000))
    assert not report["success"], report
    assert report["failed"][0]["error"]["kind"] == "StaleState"
    assert links(namespace)["a"]["mtu"] == 1500


@pytest.mark.tier2
@pytest.mark.apply
@pytest.mark.ipv4
def test_stale_lifetime_attributes_block_removal(environment):
    (namespace,), probe = environment()
    add_addresses(namespace, "10.44.0.1/24")
    query(probe)
    ip(namespace, "addr", "change", "10.44.0.1/24", "dev", "a", "valid_lft", "300", "preferred_lft", "0")
    before = cidrs(namespace)
    report = apply(probe, operation("remove", address="10.44.0.1/24"))
    assert not report["success"], report
    assert report["failed"][0]["error"]["kind"] == "StaleState"
    assert cidrs(namespace) == before == ["10.44.0.1/24"]
    assert addresses(namespace)[0]["preferred_life_time"] == 0


@pytest.mark.tier2
@pytest.mark.apply
@pytest.mark.ipv4
def test_primary_removal_cannot_cascade_to_unplanned_secondary(environment):
    (namespace,), probe = environment()
    set_promote_secondaries(namespace, 0)
    add_addresses(namespace, "10.44.0.1/24", "10.44.0.2/24")
    before = addresses(namespace)
    query(probe)
    report = apply(probe, operation("remove", address="10.44.0.1/24"))
    assert not report["success"], report
    assert addresses(namespace) == before
    assert "UP" in links(namespace)["a"]["flags"]


@pytest.mark.tier2
@pytest.mark.apply
@pytest.mark.ipv4
@pytest.mark.parametrize("promote", [0, 1])
def test_explicit_primary_first_rebuild_accounts_for_kernel_cascade(environment, promote):
    (namespace,), probe = environment()
    set_promote_secondaries(namespace, promote)
    add_addresses(namespace, "10.44.0.1/24", "10.44.0.2/24", "10.55.0.1/24")
    other = address_entry(namespace, "10.55.0.1/24")
    query(probe)
    report = apply(probe,
        operation("remove", address="10.44.0.1/24"),
        operation("remove", address="10.44.0.2/24", dependencies=[0]),
        operation("add", address="10.44.0.2/24", dependencies=[1]),
        operation("add", address="10.44.0.1/24", dependencies=[2]),
    )
    if promote:
        require_success(report, changed=[0, 1, 2, 3], skipped=[])
    else:
        require_success(report, changed=[0, 2, 3], skipped=[1])
        assert report["skipped"][0]["reason"] == "AlreadySatisfied"
    assert_rebuilt_group(namespace, other)


@pytest.mark.tier2
@pytest.mark.apply
@pytest.mark.ipv4
def test_deprecated_finite_address_add_and_exact_idempotence(environment):
    (namespace,), probe = environment()
    query(probe)
    require_success(apply(probe, operation("add", address="10.44.0.1/24", valid_lft=300, preferred_lft=0)), changed=[0])
    added = addresses(namespace)
    assert len(added) == 1 and added[0]["deprecated"]
    assert added[0]["preferred_life_time"] == 0
    assert 0 < added[0]["valid_life_time"] <= 300
    query(probe, name="a")
    for _ in range(3):
        valid = addresses(namespace)[0]["valid_life_time"]
        report = apply(probe, operation("add", address="10.44.0.1/24", valid_lft=valid, preferred_lft=0))
        if addresses(namespace)[0]["valid_life_time"] == valid:
            break
    else:
        pytest.fail("kernel lifetime changed during every re-apply window")
    require_success(report, changed=[], skipped=[0])
    assert report["skipped"][0]["reason"] == "AlreadySatisfied"
    after = addresses(namespace)
    assert len(after) == 1
    assert after[0]["preferred_life_time"] == 0
    assert after[0]["valid_life_time"] == valid


@pytest.mark.tier2
@pytest.mark.apply
@pytest.mark.ipv4
def test_unrelated_peer_and_scope_attributes_survive(environment):
    (namespace,), probe = environment()
    ip(namespace, "addr", "add", "10.77.0.1", "peer", "10.77.0.2", "dev", "a", "scope", "link")
    preserved = addresses(namespace)[0]
    query(probe)
    require_success(apply(probe, operation("add", address="10.44.0.1/24")), changed=[0])
    query(probe)
    require_success(apply(probe, operation("remove", address="10.44.0.1/24")), changed=[0])
    assert addresses(namespace) == [preserved]


@pytest.mark.tier2
@pytest.mark.query
def test_joined_queries_on_two_backends_return_distinct_namespace_results(environment):
    (left, right), probe = environment(2)
    ip(left, "link", "set", "dev", "a", "mtu", "1400")
    ip(right, "link", "set", "dev", "a", "mtu", "1600")
    result = probe.request("query_pair")
    assert "error" not in result, result
    assert len(result["observations"]) == 2
    for observed, expected in zip(result["observations"], [1400, 1600], strict=True):
        devices = {item["name"]: item for item in observed["interfaces"]}
        assert devices["a"]["fields"]["mtu"] == expected


@pytest.mark.tier2
@pytest.mark.apply
def test_permission_error_retains_operation_context_and_blocks_dependency(environment):
    (namespace,), probe = environment(read_only=True)
    query(probe)
    before = stable_state(namespace)
    report = apply(probe,
        operation("mtu", value=9000),
        operation("enabled", value=False, dependencies=[0]),
    )
    assert not report["success"]
    assert not report["succeeded"]
    failed = report["failed"][0]
    assert failed["context"]["index"] == 0
    assert failed["context"]["target"]["name"] == "a"
    assert failed["context"]["field"] == "mtu"
    assert failed["context"]["kind"] == "set_mtu"
    assert failed["error"]["kind"] == "PermissionDenied"
    assert failed["error"]["errno"] == errno.EPERM
    assert report["skipped"][0]["context"]["index"] == 1
    assert report["skipped"][0]["reason"] == "DependencyFailed"
    assert stable_state(namespace) == before


@pytest.mark.tier2
@pytest.mark.apply
@pytest.mark.ipv4
def test_peer_prefix_cascade_cannot_remove_another_local_group(environment):
    (namespace,), probe = environment()
    set_promote_secondaries(namespace, 0)
    ip(namespace, "addr", "add", "10.0.0.1", "peer", "198.51.100.1/24", "dev", "a")
    ip(namespace, "addr", "add", "10.1.0.1", "peer", "198.51.100.2/24", "dev", "a")
    before = addresses(namespace)
    assert not before[0].get("secondary", False)
    assert before[1]["secondary"]
    query(probe)
    report = apply(probe, operation("remove", address="10.0.0.1/24"))
    assert not report["success"], report
    assert addresses(namespace) == before
