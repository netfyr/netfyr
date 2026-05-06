# Architecture

netfyr is a declarative network configuration tool for Linux. Users describe desired network state in YAML policy files; netfyr diffs that against live kernel state and applies the necessary changes. The project is a Rust workspace with nine crates arranged in layers, where each layer depends only on layers below it.

## Crate dependency graph

```mermaid
graph TD
    state["<b>netfyr-state</b><br/>Core types, selectors, values,<br/>state sets, YAML, schema"]
    policy["<b>netfyr-policy</b><br/>Policy model, static and<br/>dynamic factories"]
    reconcile["<b>netfyr-reconcile</b><br/>Per-field priority merge,<br/>conflict detection, diff"]
    backend["<b>netfyr-backend</b><br/>NetworkBackend trait,<br/>netlink, DHCP factory"]
    journal["<b>netfyr-journal</b><br/>History recording,<br/>querying, reverting"]
    varlink["<b>netfyr-varlink</b><br/>Varlink IPC protocol<br/>types and async client"]
    cli["<b>netfyr-cli</b><br/>User-facing binary,<br/>7 subcommands"]
    daemon["<b>netfyr-daemon</b><br/>Long-running daemon,<br/>Varlink server, DHCP"]
    testutils["<b>netfyr-test-utils</b><br/>Network namespace and<br/>dnsmasq helpers"]

    policy --> state
    reconcile --> state
    backend --> state
    journal --> state
    journal --> reconcile
    journal --> policy
    varlink --> state
    varlink --> policy
    varlink --> backend
    varlink --> reconcile
    cli --> state
    cli --> policy
    cli --> reconcile
    cli --> backend
    cli --> varlink
    cli --> journal
    daemon --> state
    daemon --> policy
    daemon --> reconcile
    daemon --> backend
    daemon --> varlink
    daemon --> journal

    cli -.->|dev| testutils
    daemon -.->|dev| testutils
    backend -.->|dev| testutils
```

## Layers

**Layer 0 — Foundation: `netfyr-state`.**
The types everything else depends on. Defines `State` (a set of typed fields describing one network entity), `Selector` (matches entities by name, type, driver, PCI path, MAC, or labels), `Value` (a rich union type with IP-aware parsing), `FieldValue` (value paired with provenance), and `StateSet` (a collection keyed by entity type and selector). Also owns the `SchemaRegistry` which loads embedded JSON schemas for validation.

**Layer 1 — Domain logic: `netfyr-policy`, `netfyr-reconcile`.**
`netfyr-policy` defines the `Policy` model (name, factory type, priority, state) and the `StateFactory` trait that produces a `StateSet` from a policy. Two factories exist: `StaticFactory` (inline YAML) and `Dhcpv4Factory` (runtime DHCP lease).
`netfyr-reconcile` implements the per-field priority merge algorithm: given multiple policies targeting the same entity, each field is won by the highest-priority policy. When top-priority policies disagree on a field's value, it is reported as a conflict and omitted from the effective state. Also provides `generate_diff` to compute field-level diffs between two `StateSet` values.

**Layer 2 — I/O: `netfyr-backend`, `netfyr-journal`, `netfyr-varlink`.**
`netfyr-backend` defines the `NetworkBackend` async trait (query, apply, dry-run) and provides a netlink-based implementation that talks to the Linux kernel. It also contains the `Dhcpv4Factory` which spawns a DHCP client and publishes leases as `State`.
`netfyr-journal` records every apply operation as a `JournalEntry` in an append-only NDJSON file, supporting history queries and state revert.
`netfyr-varlink` defines the Varlink IPC protocol (request/response types, async client) used for CLI-to-daemon communication over a Unix socket.

**Layer 3 — Binaries: `netfyr-cli`, `netfyr-daemon`.**
`netfyr-cli` is the user-facing `netfyr` binary with subcommands: `apply`, `query`, `history`, `revert`, `diagnose`, `show`, `completions`.
`netfyr-daemon` is a long-lived process that manages policy lifecycle, serves the Varlink API, runs DHCP factories, monitors netlink for external changes, and journals all state mutations.

**Testing: `netfyr-test-utils`.**
Shared helpers for integration tests: `NetnsGuard` (sets up unprivileged user+network namespaces), `DnsmasqGuard` (runs a DHCP server), and veth pair creation. Used as a dev-dependency by `netfyr-cli`, `netfyr-daemon`, and `netfyr-backend`.

## Two-mode architecture

netfyr operates in one of two modes, detected automatically by trying to connect to the daemon socket:

```mermaid
graph LR
    subgraph "Standalone mode"
        CLI1[netfyr CLI] --> Load1[Load policies]
        Load1 --> Merge1[Reconcile / merge]
        Merge1 --> Query1[Query netlink]
        Query1 --> Diff1[Generate diff]
        Diff1 --> Apply1[Apply via netlink]
        Apply1 --> Kernel1[Linux kernel]
    end
```

```mermaid
graph LR
    subgraph "Daemon mode"
        CLI2[netfyr CLI] --> Varlink2[Varlink client]
        Varlink2 --> Daemon2[netfyr-daemon]
        Daemon2 --> Store2[Persist policies]
        Store2 --> Factory2[Sync factories]
        Factory2 --> Merge2[Reconcile / merge]
        Merge2 --> Apply2[Apply via netlink]
        Apply2 --> Kernel2[Linux kernel]
    end
```

In **standalone mode**, the CLI loads static policies directly, reconciles them, and applies via netlink. No daemon is needed. This mode does not support dynamic factories (DHCPv4).

In **daemon mode**, the CLI submits policies over Varlink. The daemon persists them to disk, syncs DHCP factories, runs reconciliation, applies, and journals the result. The daemon also monitors netlink for external changes and records them passively.

## Data model

```mermaid
classDiagram
    class State {
        entity_type: EntityType
        selector: Selector
        fields: IndexMap~String, FieldValue~
        metadata: StateMetadata
        policy_ref: Option~String~
        priority: u32
    }
    class Selector {
        fields: IndexMap~String, Value~
        +key() String
        +matches(other) bool
    }
    class FieldValue {
        value: Value
        provenance: Provenance
    }
    class Value {
        <<enumeration>>
        Bool
        U64
        I64
        String
        IpNetwork
        IpAddr
        MacAddr
        List~Vec~Value~~
        Map~IndexMap~
    }
    class Provenance {
        <<enumeration>>
        UserConfigured
        KernelDefault
        ExternalTool
        Derived
    }
    class StateSet {
        inner: IndexMap~EntityKey, State~
    }
    class Policy {
        name: String
        factory_type: FactoryType
        priority: u32
        selector: Option~Selector~
    }
    class FactoryType {
        <<enumeration>>
        Static
        Dhcpv4
    }

    State --> Selector
    State *-- FieldValue : fields
    FieldValue --> Value
    FieldValue --> Provenance
    StateSet *-- State : inner
    Policy --> FactoryType
    Policy --> Selector
```

## Key concepts

| Concept | Crate | Description |
|---------|-------|-------------|
| **State** | `netfyr-state` | A set of typed fields describing one network entity (e.g. an ethernet interface with MTU, addresses, routes). |
| **Selector** | `netfyr-state` | Matches entities by name, type, driver, PCI path, MAC, or labels. All criteria use AND logic for stable hardware identification across reboots. |
| **Value** | `netfyr-state` | Rich union type supporting booleans, integers, IP addresses/networks, MAC addresses, strings, lists, and maps. Custom YAML deserializer parses strings as IP addresses when appropriate. |
| **FieldValue** | `netfyr-state` | A `Value` paired with `Provenance` — tracks whether the value was user-configured, a kernel default, set by an external tool, or derived. |
| **StateSet** | `netfyr-state` | Collection of `State` values keyed by `(entity_type, selector.key())`. |
| **SchemaRegistry** | `netfyr-state` | Loads embedded JSON schemas (`ethernet.json`, `ip.json`, `link.json`) and validates state fields for type correctness and writability. |
| **Policy** | `netfyr-policy` | Named factory that produces a `StateSet`. Carries a priority, a factory type (static or DHCPv4), and either inline state or a selector for dynamic factories. |
| **StateFactory** | `netfyr-policy` | Trait implemented by `StaticFactory` and `Dhcpv4Factory` to produce `StateSet` from a policy definition. |
| **Reconciliation** | `netfyr-reconcile` | Per-field priority merge across multiple policies. Each field is independently won by the highest-priority policy. |
| **Conflict** | `netfyr-reconcile` | When two policies at the same priority set the same field to different values. Conflicts are reported explicitly and the field is omitted from the effective state. |
| **StateDiff** | `netfyr-reconcile` | Field-level diff between two `StateSet` values, with operations: Add, Remove, Modify (with per-field changes). |
| **NetworkBackend** | `netfyr-backend` | Async trait for querying and applying network state. Currently implemented by `NetlinkBackend`. |
| **JournalEntry** | `netfyr-journal` | Records one apply operation: sequence ID, timestamp, trigger type, active policies, field-level diff, state-after snapshot, and apply outcome. |
| **Trigger** | `netfyr-journal` | What caused a journal entry: `DaemonStartup`, `Apply` (user), `ExternalChange`, or `Revert`. |
| **Varlink** | `netfyr-varlink` | IPC protocol over Unix socket using NUL-terminated JSON messages. 8 RPC methods for policy submission, query, dry-run, status, history, revert, and show. |
