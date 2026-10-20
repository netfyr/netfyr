import json
import subprocess


def run(*args):
    result = subprocess.run(args, capture_output=True, text=True, timeout=20)
    assert result.returncode == 0, f"{args}: {result.stderr}"
    return result.stdout


def ip(namespace, *args):
    return run("ip", "-n", namespace, *args)


def links(namespace):
    return {link["ifname"]: link for link in json.loads(ip(namespace, "-j", "-d", "link"))}


def addresses(namespace, device="a"):
    return json.loads(ip(namespace, "-j", "-4", "addr", "show", "dev", device))[0]["addr_info"]


def cidrs(namespace, device="a"):
    return [f"{a['local']}/{a['prefixlen']}" for a in addresses(namespace, device)]


def address_entry(namespace, cidr, device="a"):
    return next(a for a in addresses(namespace, device) if f"{a['local']}/{a['prefixlen']}" == cidr)


def add_addresses(namespace, *cidr_list, device="a"):
    for cidr in cidr_list:
        ip(namespace, "addr", "add", cidr, "dev", device)


def set_promote_secondaries(namespace, value, device="a"):
    run("ip", "netns", "exec", namespace, "sysctl", "-q", "-w", f"net.ipv4.conf.{device}.promote_secondaries={value}")


def stable_state(namespace):
    return {
        "links": {
            name: {key: item[key] for key in ("ifindex", "mtu", "flags", "address") if key in item}
            for name, item in links(namespace).items()
        },
        "addresses": json.loads(ip(namespace, "-j", "-4", "addr")),
        "routes": json.loads(ip(namespace, "-j", "-4", "route", "show", "table", "all")),
    }


def assert_rebuilt_group(namespace, other_before):
    group = [a for a in addresses(namespace) if a["local"].startswith("10.44.")]
    assert [a["local"] for a in group] == ["10.44.0.2", "10.44.0.1"]
    assert not group[0].get("secondary", False)
    assert group[1]["secondary"]
    assert address_entry(namespace, "10.55.0.1/24") == other_before


def query(probe, backend=0, **selector):
    result = probe.request("query", backend, selector=selector)
    assert "error" not in result and "probe_error" not in result, result
    return result["interfaces"]


def apply(probe, *operations, backend=0):
    report = probe.request("apply", backend, operations=list(operations))
    assert "probe_error" not in report, report
    return report


def operation(kind, target="a", **arguments):
    return dict(kind=kind, target=target, **arguments)


def require_success(report, changed=None, skipped=None):
    assert report["success"], report
    assert report["failed"] == [], report
    if changed is not None:
        assert [entry["index"] for entry in report["succeeded"]] == changed, report
    if skipped is not None:
        assert [entry["context"]["index"] for entry in report["skipped"]] == skipped, report
