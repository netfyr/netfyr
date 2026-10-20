import hashlib
import json
import os
from pathlib import Path
import selectors
import shutil
import stat
import subprocess
import tempfile
import uuid

import pytest

from backend_harness import ip, run

REQUIRED_TOOLS = ("ip", "sysctl", "setpriv", "systemd-detect-virt")
CANDIDATE = pytest.StashKey[dict]()
SKIPPED = pytest.StashKey[list]()


def pytest_addoption(parser):
    parser.addoption("--backend-probe", help="absolute path to the selected backend probe")
    parser.addoption("--disposable-environment", action="store_true")


class SkipCounter:
    def __init__(self):
        self.skipped = []

    def pytest_runtest_logreport(self, report):
        if report.skipped:
            self.skipped.append(report.nodeid)


def pytest_configure(config):
    counter = SkipCounter()
    config.stash[SKIPPED] = counter.skipped
    config.pluginmanager.register(counter, "netfyr-skip-counter")


def pytest_collection_modifyitems(config, items):
    for item in items:
        tiers = [name for name in ("tier1", "tier2") if item.get_closest_marker(name)]
        if len(tiers) != 1:
            raise pytest.UsageError(f"{item.nodeid}: exactly one tier marker is required")


def pytest_sessionfinish(session, exitstatus):
    if session.testscollected == 0 or session.config.stash.get(SKIPPED, []):
        session.exitstatus = pytest.ExitCode.TESTS_FAILED


def pytest_terminal_summary(terminalreporter, config):
    candidate = config.stash.get(CANDIDATE, None)
    if candidate:
        terminalreporter.write_line(
            "backend probe: {original} sha256={sha256} kernel={kernel}".format(**candidate)
        )
    skipped = config.stash.get(SKIPPED, [])
    if skipped:
        terminalreporter.write_line(f"unexpected skips fail this run: {len(skipped)}")


def preflight(config):
    if not config.getoption("disposable_environment"):
        pytest.fail("privileged backend acceptance requires --disposable-environment")
    if os.geteuid() != 0:
        pytest.fail("backend acceptance requires root inside a disposable VM")
    missing = [tool for tool in REQUIRED_TOOLS if not shutil.which(tool)]
    if missing:
        pytest.fail(f"missing prerequisites: {', '.join(missing)}")
    virt = subprocess.run(["systemd-detect-virt", "--vm"], capture_output=True, text=True, timeout=20)
    if virt.returncode != 0 or virt.stdout.strip() in ("", "none"):
        pytest.fail("backend acceptance must run inside a virtual machine (systemd-detect-virt --vm)")


@pytest.fixture(scope="session")
def backend_probe(pytestconfig, record_testsuite_property):
    preflight(pytestconfig)
    option = pytestconfig.getoption("backend_probe")
    if not option or not Path(option).is_absolute():
        pytest.fail("--backend-probe must select an absolute executable path")
    original = Path(option).resolve()
    if not original.is_file() or not os.access(original, os.X_OK):
        pytest.fail(f"selected backend probe is not executable: {original}")
    directory = Path(tempfile.mkdtemp(prefix="netfyr-probe-"))
    try:
        os.chmod(directory, 0o700)
        info = directory.stat()
        if info.st_uid != 0 or stat.S_IMODE(info.st_mode) != 0o700:
            pytest.fail(f"probe copy directory is not root-owned 0700: {directory}")
        copy = directory / "backend-probe"
        shutil.copyfile(original, copy)
        os.chmod(copy, 0o700)
        candidate = {
            "original": str(original),
            "copy": str(copy),
            "sha256": hashlib.sha256(copy.read_bytes()).hexdigest(),
            "kernel": os.uname().release,
        }
        pytestconfig.stash[CANDIDATE] = candidate
        record_testsuite_property("candidate", candidate["original"])
        record_testsuite_property("candidate_sha256", candidate["sha256"])
        record_testsuite_property("kernel", candidate["kernel"])
        yield copy
    finally:
        shutil.rmtree(directory)


class Probe:
    def __init__(self, binary, namespaces, read_only=False):
        command = [str(binary), *[f"/run/netns/{name}" for name in namespaces]]
        if read_only:
            command = ["setpriv", "--bounding-set=-net_admin", *command]
        self.stderr = tempfile.TemporaryFile(mode="w+")
        self.process = subprocess.Popen(
            command,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr,
            text=True, bufsize=1,
        )

    def diagnostics(self):
        self.stderr.seek(0)
        return self.stderr.read()

    def request(self, op, backend=0, **arguments):
        request = dict(op=op, backend=backend, **arguments)
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()
        with selectors.DefaultSelector() as waiter:
            waiter.register(self.process.stdout, selectors.EVENT_READ)
            assert waiter.select(timeout=15), f"probe timed out: {request}"
        response = self.process.stdout.readline()
        assert response, f"probe exited ({self.process.poll()}): {self.diagnostics()}"
        return json.loads(response)

    def close(self):
        try:
            self.process.stdin.close()
            try:
                code = self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
                raise AssertionError("backend probe did not stop after stdin closed")
            assert code == 0, self.diagnostics()
        finally:
            self.stderr.close()


@pytest.fixture
def environment(backend_probe):
    namespaces = []
    probes = []

    def create(count=1, read_only=False):
        selected = []
        for _ in range(count):
            name = "nfb-" + uuid.uuid4().hex[:12]
            run("ip", "netns", "add", name)
            namespaces.append(name)
            selected.append(name)
            ip(name, "link", "add", "name", "a", "type", "veth", "peer", "name", "b")
            ip(name, "link", "add", "name", "u", "type", "veth", "peer", "name", "v")
            for device in ("a", "b", "u", "v"):
                ip(name, "link", "set", "dev", device, "up")
        probe = Probe(backend_probe, selected, read_only=read_only)
        probes.append(probe)
        return selected, probe

    try:
        yield create
    finally:
        errors = []
        for probe in probes:
            try:
                probe.close()
            except Exception as error:
                errors.append(str(error))
        for namespace in reversed(namespaces):
            try:
                run("ip", "netns", "del", namespace)
            except Exception as error:
                errors.append(str(error))
        assert not errors, "cleanup failed: " + "; ".join(errors)
