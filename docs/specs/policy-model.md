# Spec: Policy Model

## Problem

The multi-policy-per-interface model was designed for composability:
multiple policies can target the same interface and are merged per-field
by priority. This creates complexity that makes GUI integration
difficult:

- A GUI client cannot simply "edit eth0's config" — it must understand
  which policies target eth0, how they merge, and which one to modify.
- Priority conflicts between policies are confusing for non-expert users.
- The policy YAML format has redundant fields (interface name appears in
  both the policy selector and the inline state selector).
- Each policy has a single factory type, so "DHCP for IPv4 + static MTU"
  requires two separate policies for the same interface.

## Goals

1. One policy per interface — no multi-policy merging, no priority
   conflicts.
2. A single policy bundles link-level settings, IPv4 configuration, and
   IPv6 configuration.
3. DHCPv4 and static addresses can coexist in the same policy (e.g.,
   DHCPv4 + static IPv6 addresses).
4. DHCPv4 can have its own parameters (e.g., DHCP options).
5. The YAML format is flat and does not repeat the interface name.
6. A profile management API allows GUIs to save, list, activate, and
   deactivate profiles per interface.

## Policy YAML format

### Complete schema

```yaml
# Required. Unique name for this policy/profile.
name: <string>

# Required. The network interface this policy applies to.
# Replaces the current selector.name + state.selector.name.
interface: <string>

# ── Link-level settings ──────────────────────────────────────────────

# Administrative state. When true, the interface is brought up (IFF_UP).
# Optional. Default: true. Type: bool.
enabled: <bool>

# Maximum Transmission Unit in bytes.
# Optional. No default (kernel default preserved). Type: integer, 68-65535.
mtu: <integer>

# ── IPv4 configuration ───────────────────────────────────────────────

# Optional. Omit the entire section to leave IPv4 unconfigured.
ipv4:

  # DHCPv4 client.
  # Optional. Set to true for default parameters, or an object for
  # custom parameters. Omit or set to false to disable.
  dhcp: <bool | object>

  # When dhcp is an object, the following fields are accepted:
  #
  #   send-hostname: <bool>
  #     Include the system hostname in DHCP requests (Option 12).
  #     Default: false.
  #
  #   client-id: <string>
  #     Client identifier sent in DHCP requests (Option 61).
  #     Default: derived from MAC address.
  #
  #   request-options: <list of string>
  #     Additional DHCP options to request beyond the default set.
  #     The default set is always requested: subnet-mask, router,
  #     dns-server, domain-name, lease-time, server-id, renewal-time,
  #     rebinding-time.
  #     Recognized additional options: ntp, hostname, domain-search,
  #     interface-mtu, static-routes, classless-static-routes.
  #     Default: [].
  #
  #   route-metric: <integer>
  #     Metric for routes obtained via DHCP.
  #     Default: 100.
  #
  # Example:
  #   dhcp:
  #     send-hostname: true
  #     request-options: [ntp, classless-static-routes]
  #     route-metric: 200

  # Static IPv4 addresses.
  # Optional. Type: list of address entries.
  # Each entry is either a CIDR string or an object with fields:
  #   address: <string>    CIDR notation, e.g. "10.0.0.5/24". Required.
  #   valid-lft: <integer> Valid lifetime in seconds. Optional.
  #   preferred-lft: <integer> Preferred lifetime in seconds. Optional.
  addresses:
    - <string | object>

  # Static IPv4 routes.
  # Optional. Type: list of route objects.
  routes:
    - destination: <string>    # CIDR notation. Required.
      gateway: <string>        # IPv4 address. Optional.
      metric: <integer>        # Route metric, lower is preferred. Optional.
      mtu: <integer>           # Route-specific MTU. Optional.
      table: <integer>         # Routing table ID (0-4294967295). Optional.
                               # Default: 254 (main).
      tos: <integer>           # Type of Service filter (0-255). Optional.

  # Static DNS servers for this interface.
  # Optional. Type: list of IPv4 address strings.
  dns:
    - <string>

# ── IPv6 configuration ───────────────────────────────────────────────

# Optional. Omit the entire section to leave IPv6 unconfigured.
# Only static addresses are supported at this time (no SLAAC or
# DHCPv6).
ipv6:

  # Static IPv6 addresses.
  # Same format as ipv4.addresses.
  addresses:
    - <string | object>

  # Static IPv6 routes.
  # Same format as ipv4.routes, but with IPv6 addresses.
  routes:
    - destination: <string>
      gateway: <string>
      metric: <integer>
      mtu: <integer>
      table: <integer>

  # Static DNS servers for this interface (IPv6 addresses).
  dns:
    - <string>
```

### Examples

**Desktop workstation — DHCP for IPv4:**
```yaml
name: eth0-auto
interface: eth0
ipv4:
  dhcp: true
```

**Dual-stack server — DHCPv4 + static IPv6:**
```yaml
name: eth0-prod
interface: eth0
mtu: 9000
ipv4:
  dhcp: true
ipv6:
  addresses:
    - "fd00::50/64"
  routes:
    - destination: "::/0"
      gateway: "fd00::1"
  dns:
    - "fd00::2"
    - "fd00::3"
```

**DHCP with custom options and static fallback address:**
```yaml
name: eth0-office
interface: eth0
ipv4:
  dhcp:
    send-hostname: true
    request-options: [ntp, classless-static-routes]
    route-metric: 200
  addresses:
    - "192.168.1.100/24"
```

**Minimal — just bring the interface up:**
```yaml
name: eth0-up
interface: eth0
enabled: true
```

### Validation rules

- `name` must be non-empty and unique across all profiles.
- `interface` must be non-empty.
- At most one active policy per interface (enforced by the daemon).
- `mtu` must be in range 68-65535.
- Addresses must be valid CIDR notation.
- Route destinations must be valid CIDR notation.
- Gateways and DNS entries must be valid IP addresses.
- `table` must be in range 0-4294967295.
- `tos` must be in range 0-255.
- `route-metric` must be non-negative.
- `valid-lft` and `preferred-lft` must be non-negative.

## One-policy-per-interface constraint

The daemon enforces that at most one policy is active for any given
interface. Attempting to activate a second policy for the same interface
deactivates the current one.

This eliminates:
- Priority-based per-field merging in the reconciler
- Conflict detection and reporting
- The `priority` field in policies (no longer meaningful)

The reconciler simplifies to: for each active policy, compute the diff
between desired state and actual system state, then apply.

## Profile management

### Concepts

A **profile** is a policy that may or may not be active. The daemon
stores all profiles (active and inactive) and is the single source of
truth. GUIs are stateless — they query the daemon for the profile list.

Each profile has:
- A unique `name`
- An `interface` it targets
- An `active` flag (at most one profile per interface is active)
- The configuration fields (link settings, ipv4, ipv6)

### Varlink API

```varlink
method ListProfiles() -> (profiles: []Profile)

method GetProfile(name: string) -> (profile: Profile)

method SaveProfile(profile: Profile) -> ()

method DeleteProfile(name: string) -> ()

method ActivateProfile(name: string) -> (report: ApplyReport)

method DeactivateProfile(interface: string) -> (report: ApplyReport)

type Profile (
    name: string,
    interface: string,
    active: bool,
    enabled: ?bool,
    mtu: ?int,
    ipv4: ?Ipv4Config,
    ipv6: ?Ipv6Config
)

type Ipv4Config (
    dhcp: ?object,
    addresses: ?[]object,
    routes: ?[]Route,
    dns: ?[]string
)

type Ipv6Config (
    addresses: ?[]object,
    routes: ?[]Route,
    dns: ?[]string
)

type Route (
    destination: string,
    gateway: ?string,
    metric: ?int,
    mtu: ?int,
    table: ?int,
    tos: ?int
)

error NotFound (reason: string)
error AlreadyExists (reason: string)
```

### Method semantics

**ListProfiles()**
- Returns all stored profiles.
- Each profile includes `active: true/false`.
- Read-only, any UID.
- Filtering parameters (e.g., by interface) can be added later as
  optional fields without breaking existing clients.

**GetProfile(name)**
- Returns a single profile by name.
- Returns `NotFound` if the name does not exist.
- Read-only, any UID.

**SaveProfile(profile)**
- Creates a new profile or updates an existing one (matched by name).
- If the profile is currently active, the new configuration is applied
  immediately (reconcile + apply).
- If the profile changes the `interface` field and was active, it is
  deactivated on the old interface first.
- Root only.

**DeleteProfile(name)**
- Deletes a profile by name.
- If the profile is currently active, it is deactivated first (interface
  state is cleaned up).
- Returns `NotFound` if the name does not exist.
- Root only.

**ActivateProfile(name)**
- Marks the named profile as active.
- If another profile is active on the same interface, that profile is
  deactivated first (set to `active: false`).
- Starts any factories declared in the profile (DHCPv4, etc.).
- Runs reconciliation: computes desired state from profile, diffs
  against actual system state, applies changes.
- Returns an `ApplyReport` with the results.
- Returns `NotFound` if the name does not exist.
- Root only.

**DeactivateProfile(interface)**
- Deactivates the active profile on the named interface.
- Stops any running factories for that interface.
- Removes the configuration applied by the profile (addresses, routes).
- Does not delete the profile — it remains in the library as inactive.
- Returns `NotFound` if no profile is active on that interface.
- Root only.

### Storage

Profiles are stored in `/var/lib/netfyr/profiles/` (overridable via
`NETFYR_PROFILE_DIR`). Each profile is a YAML file named
`{sanitized-name}.yaml`. An `active.json` file in the same directory
tracks which profile is active per interface:

```json
{
  "eth0": "eth0-office",
  "wlan0": "home-wifi"
}
```

On daemon startup:
1. Load all profile YAML files from the directory.
2. Load `active.json` to determine which profiles are active.
3. For each active profile, start factories and reconcile.

### Interaction with the CLI

- `netfyr apply <file>` loads a profile YAML, calls `SaveProfile` +
  `ActivateProfile`.
- `netfyr apply --deactivate <interface>` calls `DeactivateProfile`.
- `netfyr query`, `netfyr show`, `netfyr history`, `netfyr revert`
  continue to work as before — they operate on system state, not
  profiles.

## Factory management changes

Currently, `FactoryManager` tracks factories keyed by policy name.
With the new model:

- Each active profile can spawn a DHCPv4 factory if `ipv4.dhcp` is set.
- Factories are keyed by `(interface, factory_type)` instead of policy
  name.
- `FactoryManager::sync()` takes the set of active profiles and starts/
  stops factories to match.
- Factory-produced state is merged with the profile's static fields to
  produce the effective desired state for the interface.

### Factory types

| Config field | Factory type | Produces |
|-------------|--------------|----------|
| `ipv4.dhcp` | Dhcpv4 | addresses, routes, dns |

### Reconciliation changes

With one-policy-per-interface, reconciliation simplifies:

1. For each active profile, build the desired state:
   a. Start with static fields from the profile (enabled, mtu,
      ipv4/ipv6 addresses, routes, dns).
   b. Merge DHCPv4-produced state (addresses, routes, dns) if
      `ipv4.dhcp` is active. Factory state is additive — it adds to
      static addresses, not replaces them.
2. Query actual system state for the interface.
3. Compute diff (desired vs. actual).
4. Apply diff via the netlink backend.

No priority merging. No conflict detection. No multi-policy input
construction.

## Files to modify

| File / Area | Changes |
|------------|---------|
| `crates/netfyr-policy/` | New profile parser. New `Profile` type replacing `Policy`. |
| `crates/netfyr-state/src/schemas/` | May need updates for new field names if link/ip schemas change. |
| `crates/netfyr-reconcile/` | Remove priority merging and conflict detection. Simplify to single-profile-per-entity reconciliation. |
| `crates/netfyr-backend/src/dhcp/` | DHCP client gains configurable options (send-hostname, client-id, request-options, route-metric). |
| `crates/netfyr-daemon/src/server.rs` | New Varlink handlers for profile CRUD and activation. Replace SubmitPolicies with profile operations. |
| `crates/netfyr-daemon/src/factory_manager.rs` | Key by (interface, factory_type). |
| `crates/netfyr-daemon/src/policy_store.rs` | Replace with profile store (load/save profiles + active.json). |
| `crates/netfyr-varlink/src/io.netfyr.varlink` | New interface definition with profile methods and types. |
| `crates/netfyr-varlink/src/types.rs` | New wire types for Profile, Ipv4Config, Ipv6Config, Route. |
| `crates/netfyr-varlink/src/client.rs` | New client methods for profile operations. |
| `crates/netfyr-cli/` | Update CLI commands to use profile API. |
