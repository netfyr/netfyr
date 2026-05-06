# netfyr

netfyr is a declarative network configuration tool for Linux. You describe the desired state of your network interfaces in YAML policy files, and netfyr diffs that against the live kernel state (queried via netlink) and applies the necessary changes.

Multiple policies can target the same interface. When they overlap, netfyr merges them per-field using numeric priority: the highest-priority policy wins each field independently. If two policies set the same field at the same priority to different values, that's a conflict — netfyr reports it explicitly rather than silently picking a winner.

There are two modes of operation, detected automatically. In standalone mode, the CLI loads static policies, reconciles them, and applies the result directly via netlink — no daemon required. In daemon mode, `netfyr-daemon` runs as a long-lived process that manages policy lifecycle, serves a Varlink IPC API, and supports dynamic factories. The main dynamic factory today is DHCPv4: the daemon runs a DHCP client, and the resulting lease is published as network state that participates in the same reconciliation and priority merge as static policies.

All apply operations are journaled. The journal records the before/after state, the diff, and which policies contributed, so you can inspect history and revert to a prior state.

## Usage

### Apply a policy

```yaml
# ethernet-static.yaml
kind: policy
name: eth0-static
priority: 100
factory: static
state:
  type: ethernet
  name: eth0
  mtu: 1500
  addresses:
    - "192.168.1.10/24"
  routes:
    - destination: "0.0.0.0/0"
      gateway: "192.168.1.1"
```

```bash
netfyr apply ethernet-static.yaml
# or apply an entire directory
netfyr apply /etc/netfyr/policies/
# or apply with no arguments (defaults to /etc/netfyr/policies/)
netfyr apply
```

### Apply a DHCPv4 policy

DHCPv4 policies carry no inline `state` — the address, routes, and other lease parameters are acquired from the DHCP server at runtime. They require the daemon.

```yaml
# ethernet-dhcp.yaml
kind: policy
name: eth0-dhcp
factory: dhcpv4
priority: 50
selector:
  name: eth0
```

```bash
netfyr apply ethernet-dhcp.yaml
```

### Preview changes without applying

```bash
netfyr apply --dry-run ethernet-static.yaml
```

### Query network state

```bash
# Query all current network state
netfyr query

# Query filtered by selector
netfyr query -s type=ethernet -s name=eth0

# Output as JSON
netfyr query --output json
```

### View history

```bash
# Show journal history of state changes
netfyr history

# Filter by count or time range
netfyr history --count 10
netfyr history --since 1h
```

Example output:

```
SEQ  TIMESTAMP   TRIGGER                ENTITIES  CHANGES
4    42 sec ago  revert (2)             enp1s0    -172.20.1.1/24, mtu 1450→1480
3    1 min ago   external               enp1s0    +172.20.1.1/24, mtu 1480→1450
2    1 min ago   apply (enp1s0-static)  enp1s0    (none)
1    1 min ago   daemon-startup         enp1s0    (none)
```

Use `--show` to inspect a single entry in detail:

```bash
netfyr history --show 3
```

```
Entry #3 at 2026-05-06 07:55:31 UTC
Trigger: external (enp1s0)
Active policies:
  - enp1s0-static (static, priority 100)
Diff:
  ~ ethernet enp1s0
      -mtu: 1480
      +mtu: 1450
      addresses:
        +172.20.1.1/24
Outcome: observed
```

### Revert to a previous state

```bash
# Preview what a revert would change
netfyr revert --dry-run <sequence-id>

# Revert system state to match a journal snapshot
netfyr revert <sequence-id>
```

### Daemon mode

`netfyr-daemon` reads policies from a directory, listens on a Varlink socket, and manages dynamic factory lifecycle (e.g., DHCPv4 clients). When the daemon is running, `netfyr apply` and `netfyr query` automatically communicate via Varlink rather than operating directly on the kernel.

```bash
netfyr-daemon
```

By default the daemon reads policies from `/var/lib/netfyr/policies` and listens on `/run/netfyr/netfyr.sock`. Override with `NETFYR_POLICY_DIR` and `NETFYR_SOCKET_PATH`.

The daemon subscribes to netlink events and journals out-of-band changes to managed interfaces — for example, someone running `ip link set dev eth0 mtu 9000` while the daemon is active. Events are debounced with a 500ms sliding window and filtered against the daemon's own applies, so only genuine external mutations produce journal entries. These entries appear in `netfyr history` with trigger type `external_change` and carry the same field-level diff and state snapshot as any other journal entry, making them available for inspection and revert. The daemon does not re-apply desired state after detecting drift; it records the change passively.

The daemon signals systemd readiness via `sd_notify(READY=1)`. A minimal systemd unit:

```ini
[Service]
ExecStart=/usr/bin/netfyr-daemon
Type=notify
```

## Building

```bash
# Build all crates
cargo build

# Build a single crate
cargo build -p netfyr-state
```

## Testing

```bash
# Run Rust unit and integration tests
cargo test

# Run all shell integration tests (builds first)
make integration-test

# Run tests for a specific story/spec number
make integration-test SPEC=401
```

Integration tests are shell scripts in `tests/` named `NNN-description.sh`. They use `unshare --user --net` to run inside an unprivileged network namespace — no root access required. Tests follow a strict no-skip policy: if a prerequisite is missing (binary not built, `unshare` unavailable, `dnsmasq` not installed), the test prints `FAIL:` to stderr and exits 1. It never exits 0 on a missing prerequisite.

## Architecture

The project is a Rust workspace with nine crates arranged in layers:

| Crate | Role |
|---|---|
| `netfyr-state` | Core state types, selectors, values, state sets, YAML parsing, schema validation |
| `netfyr-policy` | Policy types, static and dynamic factories, YAML policy loading |
| `netfyr-reconcile` | Multi-policy reconciliation with per-field priority, conflict detection, diff generation |
| `netfyr-backend` | Backend trait and netlink implementation for querying and applying network state |
| `netfyr-varlink` | Varlink IPC protocol types and client for daemon communication |
| `netfyr-journal` | History journal for recording, querying, and reverting state changes |
| `netfyr-cli` | User-facing CLI binary (`netfyr`) with subcommands: `apply`, `query`, `history`, `revert`, `diagnose`, `show`, `completions` |
| `netfyr-daemon` | Long-running daemon for dynamic factories (DHCPv4), Varlink server, systemd integration |
| `netfyr-test-utils` | Shared test utilities (network namespace setup, dnsmasq helpers) |

Dependency flow: `netfyr-state` is the foundation. `netfyr-policy` and `netfyr-reconcile` depend on it. `netfyr-backend` depends on `netfyr-state`. `netfyr-journal` depends on `netfyr-state`. `netfyr-varlink` depends on all library crates. `netfyr-cli` and `netfyr-daemon` are the top-level binaries that wire everything together.

## License

See the [LICENSE](LICENSE) file for details.
