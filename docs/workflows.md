# Workflows

Sequence diagrams for the main operations in netfyr. Each operation auto-detects whether the daemon is running by trying to connect to the Varlink socket; if the connection fails, the operation runs in standalone mode.

## Apply (standalone mode)

The CLI loads policies, reconciles them, queries live kernel state, computes a diff, and applies directly via netlink. Non-static policies (e.g. DHCPv4) are rejected in this mode.

```mermaid
sequenceDiagram
    participant User
    participant CLI as netfyr CLI
    participant Policy as netfyr-policy
    participant Schema as SchemaRegistry
    participant Reconcile as netfyr-reconcile
    participant Backend as NetlinkBackend
    participant Journal as netfyr-journal
    participant Kernel as Linux Kernel

    User->>CLI: netfyr apply policy.yaml
    CLI->>CLI: Try connect to daemon socket
    Note over CLI: Connection fails → standalone mode

    CLI->>Policy: load_policy_file / load_policy_dir
    Policy-->>CLI: PolicySet
    CLI->>Schema: validate_policies(PolicySet)

    CLI->>CLI: policies_to_inputs(PolicySet)
    CLI->>Reconcile: merge(PolicyInputs)
    Reconcile-->>CLI: ReconciliationResult<br/>(effective_state, conflicts)

    CLI->>Backend: query_all()
    Backend->>Kernel: RTM_GETLINK + RTM_GETADDR + RTM_GETROUTE
    Kernel-->>Backend: raw netlink data
    Backend-->>CLI: actual StateSet

    CLI->>CLI: normalize_route_defaults(effective_state)
    CLI->>Reconcile: generate_diff(desired, actual)
    Reconcile-->>CLI: ReconcileDiff (rich, for display)
    CLI->>CLI: compute_state_diff(actual, desired)
    Note over CLI: state_diff (lightweight, for apply)

    CLI->>Backend: apply(state_diff)
    Backend->>Kernel: RTM_SETLINK / RTM_NEWADDR / RTM_NEWROUTE
    Kernel-->>Backend: result
    Backend-->>CLI: ApplyReport

    CLI->>Journal: journal.append(entry)
    Note over Journal: Non-fatal on failure

    CLI->>User: Display results + exit code
```

## Apply (daemon mode)

The CLI submits policies over Varlink. The daemon persists them, syncs DHCP factories, runs reconciliation, applies, and journals.

```mermaid
sequenceDiagram
    participant User
    participant CLI as netfyr CLI
    participant Varlink as VarlinkClient
    participant Daemon as netfyr-daemon
    participant Store as PolicyStore
    participant Factory as FactoryManager
    participant Reconciler
    participant Backend as NetlinkBackend
    participant Journal as netfyr-journal
    participant Kernel as Linux Kernel

    User->>CLI: netfyr apply policy.yaml
    CLI->>Varlink: connect(socket_path)
    Note over CLI: Connection succeeds → daemon mode

    CLI->>CLI: Convert PolicySet to VarlinkPolicy
    CLI->>Varlink: submit_policies(Vec<VarlinkPolicy>)
    Varlink->>Daemon: SubmitPolicies request

    Daemon->>Store: Persist policies to disk
    Daemon->>Factory: sync(policies)
    Note over Factory: Start/stop DHCP factories as needed

    Daemon->>Reconciler: reconcile_and_apply()
    Reconciler->>Backend: query_all()
    Backend->>Kernel: netlink query
    Kernel-->>Backend: current state
    Reconciler->>Reconciler: merge + generate_diff
    Reconciler->>Backend: apply(diff)
    Backend->>Kernel: netlink operations
    Kernel-->>Backend: result
    Reconciler->>Journal: append(entry)

    Daemon-->>Varlink: VarlinkApplyReport
    Varlink-->>CLI: report
    CLI->>User: Display results
```

## Query

Queries live kernel state, optionally filtered by selector. In daemon mode the query goes through Varlink; in standalone mode it queries netlink directly.

```mermaid
sequenceDiagram
    participant User
    participant CLI as netfyr CLI
    participant Backend as NetlinkBackend
    participant Kernel as Linux Kernel

    User->>CLI: netfyr query [-s type=ethernet] [-o json]
    CLI->>CLI: Parse selector filters

    alt Daemon running
        CLI->>CLI: VarlinkClient.query(selector)
        Note over CLI: Daemon queries netlink on behalf of CLI
    else Standalone mode
        CLI->>Backend: query_all()
        Backend->>Kernel: RTM_GETLINK + RTM_GETADDR + RTM_GETROUTE
        Kernel-->>Backend: raw netlink data
        Backend-->>CLI: StateSet
    end

    CLI->>CLI: Filter by selector (name, type, driver, mac, pci_path)
    CLI->>User: Output as YAML or JSON
```

## Revert

Restores system state to match a journal entry's `state_after` snapshot. Computes a diff between current kernel state and the target snapshot, then applies.

```mermaid
sequenceDiagram
    participant User
    participant CLI as netfyr CLI
    participant Journal as netfyr-journal
    participant Reconcile as netfyr-reconcile
    participant Backend as NetlinkBackend
    participant Kernel as Linux Kernel

    User->>CLI: netfyr revert <seq-id>

    alt Daemon running
        CLI->>CLI: VarlinkClient.revert(seq, dry_run)
        Note over CLI: Daemon handles revert internally
    else Standalone mode
        CLI->>Journal: journal.read_entry(seq)
        Journal-->>CLI: JournalEntry

        CLI->>CLI: entry.state_after.to_state_set()
        Note over CLI: Reconstruct target StateSet from snapshot

        CLI->>Backend: query_all()
        Backend->>Kernel: netlink query
        Kernel-->>Backend: current StateSet

        CLI->>Reconcile: generate_diff(target, current)
        Reconcile-->>CLI: ReconcileDiff
        CLI->>CLI: compute_state_diff(current, target)

        CLI->>Backend: apply(state_diff)
        Backend->>Kernel: netlink operations
        Kernel-->>Backend: ApplyReport

        CLI->>Journal: journal.append(revert entry)
        CLI->>User: Display results
    end
```

## Daemon startup

The daemon initializes logging, loads persisted policies, starts DHCP factories, runs an initial reconciliation, notifies systemd, and enters the Varlink event loop. Failures at each step are logged but do not prevent the daemon from starting.

```mermaid
sequenceDiagram
    participant Systemd
    participant Daemon as netfyr-daemon
    participant Store as PolicyStore
    participant Factory as FactoryManager
    participant Reconciler
    participant Backend as NetlinkBackend
    participant Monitor as NetlinkMonitor
    participant Varlink as Varlink Server
    participant Kernel as Linux Kernel

    Systemd->>Daemon: ExecStart

    Note over Daemon: 1. Init structured logging (stderr)
    Note over Daemon: 2. Create socket directory

    Daemon->>Store: 3. Load from /var/lib/netfyr/policies/
    Store-->>Daemon: PolicyStore (or empty on failure)

    Daemon->>Factory: 4. sync(policies)
    Note over Factory: Start Dhcpv4Factory for each DHCPv4 policy

    Daemon->>Reconciler: 5. reconcile_and_apply(DaemonStartup)
    Reconciler->>Backend: query + merge + diff + apply
    Backend->>Kernel: netlink operations
    Note over Daemon: Log and continue on failure

    Daemon->>Systemd: 6. sd_notify(READY=1)

    Daemon->>Varlink: 7. serve_varlink()

    par Varlink event loop
        Varlink->>Varlink: Accept connections
        Note over Varlink: Handle SubmitPolicies, Query,<br/>DryRun, GetStatus, GetHistory,<br/>GetJournalEntry, Revert, GetShowInfo
    and Netlink monitoring
        Monitor->>Kernel: Subscribe RTNLGRP_LINK,<br/>IPV4_IFADDR, IPV4_ROUTE
        Kernel-->>Monitor: netlink events
        Note over Monitor: Debounce 500ms sliding window
        Monitor->>Reconciler: Record external changes
        Note over Reconciler: Journal with trigger=ExternalChange
    and DHCP factory events
        Factory-->>Daemon: LeaseAcquired / LeaseRenewed / LeaseExpired
        Daemon->>Reconciler: reconcile_and_apply()
    end

    Note over Daemon: 8. On SIGTERM/SIGINT:<br/>release DHCP leases, exit cleanly<br/>(leave applied config in place)
```

## DHCP lease lifecycle

When a DHCPv4 policy is submitted, the daemon starts a DHCP factory that spawns a client. Lease events trigger reconciliation, incorporating DHCP-acquired state into the priority merge alongside static policies.

```mermaid
sequenceDiagram
    participant Daemon as netfyr-daemon
    participant Factory as Dhcpv4Factory
    participant Client as DHCP Client
    participant Server as DHCP Server
    participant Reconciler

    Daemon->>Factory: start(interface, selector)
    Factory->>Client: Spawn tokio task

    Client->>Server: DHCPDISCOVER
    Server-->>Client: DHCPOFFER
    Client->>Server: DHCPREQUEST
    Server-->>Client: DHCPACK

    Client->>Factory: LeaseAcquired(lease)
    Factory->>Factory: lease_to_state(lease)
    Note over Factory: Produces State with address,<br/>routes, DNS from lease

    Factory-->>Daemon: FactoryEvent::LeaseAcquired
    Daemon->>Reconciler: reconcile_and_apply()
    Note over Reconciler: DHCP state participates in<br/>per-field priority merge with<br/>static policies

    loop Lease renewal
        Client->>Server: DHCPREQUEST (at T1)
        Server-->>Client: DHCPACK
        Client->>Factory: LeaseRenewed(lease)
        Factory-->>Daemon: FactoryEvent::LeaseRenewed
        Daemon->>Reconciler: reconcile_and_apply()
    end

    alt Shutdown
        Daemon->>Factory: stop()
        Factory->>Client: Cancel task
        Client->>Server: DHCPRELEASE
    end
```
