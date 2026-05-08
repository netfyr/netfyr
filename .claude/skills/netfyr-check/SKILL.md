---
name: netfyr-check
description: Post-implementation verification checklist for netfyr features. Use after implementing a feature to check if all layers were updated.
disable-model-invocation: true
---

## Changes to review

If the working tree has uncommitted changes, review those; otherwise review the last commit.

```!
bash .claude/skills/netfyr-check/show-changes.sh
```

## Build warnings

```!
bash -c 'cargo build 2>&1 | grep -E "^warning:|generated [0-9]+ warning" || echo "(none)"'
```

## Man pages

```!
ls man/
```

## Integration tests

```!
ls tests/*.sh | head -40
```

## Your task

Analyze the diff above against the netfyr implementation checklist below. For each item, report one of:
- **Updated** — the diff touches this layer appropriately
- **Not needed** — the change doesn't affect this layer (explain briefly why)
- **MISSING** — this layer likely needs updating but wasn't touched

### Checklist

1. **Schema** (`crates/netfyr-state/src/schemas/ip.json`) — were new fields or types added to the JSON schema?
2. **Validation** (`crates/netfyr-state/src/schema.rs`) — if the schema changed, does validation logic need updating?
3. **Value types** (`crates/netfyr-state/src/lib.rs`) — do new Value enum variants or accessor methods need adding?
4. **YAML parsing** (`crates/netfyr-state/src/yaml.rs`) — if Value types changed, does deserialization need updating?
5. **Query layer** (`crates/netfyr-backend/src/netlink/ethernet.rs`) — do new fields need to be read from the kernel via netlink?
6. **Apply layer** (`crates/netfyr-backend/src/netlink/apply.rs`) — do new fields need to be written to the kernel via netlink?
7. **Diff display** (`crates/netfyr-reconcile/src/report.rs`) — do new fields need formatting in diff output?
8. **Man pages** (`man/`) — if user-facing behavior changed, were all relevant man pages updated? Check every man page that documents affected commands or formats. `netfyr.yaml.5` must be updated if the YAML format changed or new fields were added.
9. **Examples** (`man/netfyr-examples.7`) — read the file and check whether the new functionality is covered by existing examples. If the change introduces a new user-facing capability (new field, new address family, new option) that isn't demonstrated by any existing example section, report MISSING.
10. **Integration tests** (`tests/`) — any functional change (new field, new behavior, bug fix) must have a corresponding shell integration test in `tests/`. Check that new tests exist for the new functionality.
11. **RPM spec** (`netfyr.spec`) — if new files, dependencies, or build steps were added, does the spec file need updating?
12. **Install script** (`scripts/install.sh`) — if new installed files or paths were added, does the install script need updating?
13. **Build warnings** — are there any compiler warnings in the build output above?

### Output format

Print a checklist like this:

```
netfyr implementation checklist
  1. Schema              Updated
  2. Validation          Not needed (no new constraints)
  3. Value types          Not needed
  4. YAML parsing         Not needed
  5. Query layer          Updated
  6. Apply layer          Updated
  7. Diff display         MISSING — new route fields not formatted
  8. Man pages            MISSING — netfyr.yaml.5 not updated
  9. Examples             MISSING — no example for the new capability
 10. Integration tests    Updated (2 new tests)
 11. RPM spec            Not needed (no new files or dependencies)
 12. Install script       Not needed (no new installed paths)
 13. Build warnings       (none)
```

After the checklist, briefly explain each MISSING item: what specifically should be done.
