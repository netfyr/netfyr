# Backend acceptance coverage

The backend suite exercises SPEC-004 through `NetworkBackend`, driven by the
`backend-probe` binary in `probe/`. The probe keeps queried identities and
observations so tests can change the kernel before executing a stale plan.

Build explicitly, then copy the probe and this suite into a disposable VM:

```sh
cargo build -p netfyr-backend-probe
python3 -m pytest tests/integration \
  --backend-probe=/absolute/path/to/backend-probe \
  --disposable-environment --junitxml=/absolute/path/to/results.xml
```

Prerequisites: a virtual machine (`systemd-detect-virt --vm` must report one);
Linux network namespaces, veth, bridge, dummy and VLAN kernel support; root
with namespace and network administration capabilities; Python, pytest,
iproute2 (`ip`), `sysctl`, `setpriv` and `systemd-detect-virt`. Missing
prerequisites fail. Each case declares exactly one tier (`tier1` or `tier2`),
and unexpected skips or empty collection fail the run.

The selected probe is resolved, copied once per session into a root-owned 0700
temporary directory, and only that copy is executed. The resolved original
path, the copy's SHA-256 and the kernel release are printed in the terminal
summary and, with `--junitxml`, recorded as test suite properties.

Fixtures create veth pairs directly inside unique namespaces. Setup and cleanup
use `ip`; the candidate never restores the fixture. Keep an outer VM snapshot
so an interrupted run can discard the environment independently. Never run
these privileged tests on a workstation's ordinary filesystem.

Assertions about kernel state come from `ip -j` observations taken by the
harness, never from probe output. Probe responses are checked only for
report structure, error kinds, errno values and counts.

## Requirements and executable cases

Names below omit the common `test_` prefix. Both `test_backend.py` and
`test_backend_challenges.py` are required for a complete run; shared helpers
live in `backend_harness.py` and fixtures in `conftest.py`.

| SPEC-004 requirement | Cases |
|---|---|
| FR-001 namespace binding | `namespace_binding_and_unmanaged_preservation`, `second_backend_apply_lands_in_its_namespace`, `joined_queries_on_two_backends_return_distinct_namespace_results` |
| FR-002, FR-006, FR-007 device types | `unsupported_links_keep_type_and_reject_mutation`, `query_inventory_selectors_and_optional_fields` |
| FR-003, FR-005 error/report context | `stale_address_group_blocks_dependents_preserves_independent_progress`, `permission_error_retains_operation_context_and_blocks_dependency` |
| FR-004, FR-018 dry run | `dry_run_exposes_operations_and_never_mutates` |
| FR-008 selectors | `query_inventory_selectors_and_optional_fields` (name, MAC, `type=ethernet`, `driver=veth`, non-matching driver, MAC and PCI path) |
| FR-009 address inventory | `query_inventory_selectors_and_optional_fields` (address order, lifetimes, `addresses_complete` true for veth and false for `lo`), `deprecated_finite_address_add_and_exact_idempotence` |
| FR-010 | Not exercised by this harness. |
| FR-011 target/group freshness | `replaced_target_not_mutated_and_independent_work_continues` (`NotFound`), `reused_index_name_and_mac_does_not_reuse_identity` (`StaleState`), `stale_address_group_blocks_dependents_preserves_independent_progress`, `stale_lifetime_attributes_block_removal` |
| FR-012 explicit execution order | `explicit_same_prefix_rebuild_preserves_other_groups`, `explicit_primary_first_rebuild_accounts_for_kernel_cascade` |
| FR-013 idempotence and replacement | `apply_idempotence_and_field_preservation`, `different_existing_lifetimes_require_explicit_replacement`, `deprecated_finite_address_add_and_exact_idempotence` |
| FR-014 independent progress | `stale_address_group_blocks_dependents_preserves_independent_progress`, `replaced_target_not_mutated_and_independent_work_continues` |
| FR-015 narrow removal | `remove_secondary_preserves_device_admin_and_other_prefix`, `primary_removal_cannot_cascade_to_unplanned_secondary`, `peer_prefix_cascade_cannot_remove_another_local_group`, `unrelated_peer_and_scope_attributes_survive` |
| FR-016 defensive fields | `read_only_skip_unknown_writable_failure_and_administrative_state` |
| FR-017 same-prefix semantics | `explicit_same_prefix_rebuild_preserves_other_groups` (secondary removed first, so `promote_secondaries` does not apply) and `explicit_primary_first_rebuild_accounts_for_kernel_cascade[0]` / `[1]` (the only cases parametrized over `promote_secondaries`); `primary_removal_cannot_cascade_to_unplanned_secondary` and `peer_prefix_cascade_cannot_remove_another_local_group` pin `promote_secondaries=0` |

The suite collects 22 cases: 6 `tier1` and 16 `tier2`.

These live-kernel checks complement Rust tests for malformed inventory,
unsupported optional enrichment, invalid dependencies, transport failure and
cancellation. Unconfirmed writes (`OutcomeUnknown`) are not exercised here.
Those failure modes
require controlled injection; an ordinary successful kernel dump is not
evidence that its malformed counterpart is safe.

The harness does not make the guest filesystem immutable against a privileged
candidate. Results are authoritative only when retained outside the VM.
