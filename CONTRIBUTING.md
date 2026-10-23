# Contributing to netfyr

## How work is organized

Work is driven by specs in [netfyr/specs](https://github.com/netfyr/specs). Each spec is
an independent deliverable assigned to one developer and lands as one pull request.

Before starting, read your spec's dependencies. Specs reference each other backwards
only, never forwards, so everything your spec depends on is already merged and
implemented. If it is not, say so rather than implementing it yourself: that belongs to
whoever owns the dependency.

A spec creates its own crates under `crates/` and adds them to the workspace `members`
list. The workspace starts empty by design.

## Development workflow

```
cargo build                    # compile the workspace
cargo test                     # unit tests
make test                      # shell integration tests
make test TAGS=ipv4,routing    # integration tests tagged ipv4 OR routing
make test TAGS=ipv4+routing    # integration tests tagged ipv4 AND routing
make fmt                       # format
make clippy                    # lint
make hooks                     # install the git hooks (do this once)
```

`make hooks` needs [pre-commit](https://pre-commit.com) on your `PATH` and installs the
hooks it declares: `cargo fmt --check` on commits that touch Rust files, `cargo clippy`
before pushes that do, a `commit-msg` reference check, and merged-spec resolution
before pushes. Local message checks work offline; publication checks need `curl`.
Run `make hooks` again after pulling changes to the declared hook types. Existing
hooks of the same type are preserved as `<hook>.legacy` and still run.

The `Rust and host checks` workflow runs formatting, a locked build, Rust tests,
host shell checks and Clippy. Its shell lane selects `build,meta` with
`REQUIRE_TEST_SELECTION=1`, so an accidentally empty required selection fails.
Ordinary focused runs can still select nothing successfully. Privileged backend
acceptance belongs in a disposable VM, following `tests/integration/coverage.md`.
CI retains source/tool identity and actual step outcomes; skipped or cancelled
steps remain incomplete. Workflow files do not configure repository required-status rules.

## Code conventions

Rust 2024 edition. Code is formatted with `cargo fmt` and must be clippy-clean.

Crates inherit shared metadata from the workspace rather than restating it:

```toml
[package]
name = "netfyr-example"
edition.workspace = true
version.workspace = true
license.workspace = true
```

### Dependency policy

Only add an external dependency when implementing the functionality yourself would be
unreasonable. `rtnetlink` for netlink, `clap` for CLI parsing, and `tokio` for async are
the kind of thing that qualifies. A date formatter or an argument splitter is not.

Prefer the standard library. When two crates do the same job, take the one with fewer
transitive dependencies. Every dependency costs build time, binary size, audit surface,
and a supply chain you now depend on.

Justify each dependency with a comment in the manifest, saying what it does that the
standard library cannot:

```toml
# socket2: sets SO_BINDTODEVICE, which tokio's UdpSocket does not expose.
socket2 = "0.5"
```

## Tests

Unit tests are `#[cfg(test)]` modules inside the source file they cover. They test
parsing, validation, data structure invariants, and pure functions. They must not depend
on external tools, network namespaces, or built binaries.

Integration tests are shell scripts in `tests/`. They test CLI behavior, system
interaction, and end-to-end workflows, exercising the binary the way a user would. They
are shell rather than Rust so that anyone can read and modify them without knowing Rust.

When in doubt, write a unit test. It fails in a way that points at the cause.

### Writing an integration test

Name the script for what it does: `set-mtu.sh`, `yaml-roundtrip.sh`. Declare its tags in
a comment within the first 10 lines, which is the window the runner scans:

```bash
#!/bin/bash
# tags: ipv4 routing backend
```

Tags are free-form with no central registry. They describe what the test exercises, so
that someone working on routing can run the routing tests without knowing spec numbers.
A test with no tags runs when no filter is given and is excluded when one is.

The runner executes every `tests/*.sh`, so shared helper files must not live directly in
`tests/` or they will be run as tests. Put them in `tests/lib/` and source them.

Locate built binaries through `NETFYR_TARGET_DIR`, which the runner exports, falling back
to the in-tree target directory so the script still works when you run it on its own:

```bash
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
NETFYR_BIN="${NETFYR_TARGET_DIR:-$repo_root/target}/debug/netfyr"
[ -x "$NETFYR_BIN" ] || { echo "FAIL: netfyr not built at $NETFYR_BIN" >&2; exit 1; }
```

Never hardcode `../target/debug`. CI overrides the target directory, and the fallback
only holds for a default build; run through `scripts/run-tests.sh` and the exported value
also accounts for `build.target-dir`.

### No skipping

If a prerequisite is missing, fail. Print `FAIL: ...` to stderr and exit 1:

```bash
command -v unshare >/dev/null || { echo "FAIL: unshare not available" >&2; exit 1; }
```

Never `exit 0` on a missing prerequisite. A skipped test looks identical to a passing one
in CI, so a missing dependency stays invisible until something breaks in production.

### Bug fixes

Reproduce first. Write a test that exercises the buggy code path and fails on the wrong
behavior; a compilation error is never a valid reproduction. Confirm it fails for the
right reason, then fix it, then confirm it passes.

Use an integration test for bugs spanning several components or reported as a sequence of
steps. Reserve unit tests for verifying a single function.

## Submitting changes

One spec per pull request.

Commit messages use the imperative mood, explain why rather than what, and reference the
spec they implement with a `Spec:` trailer naming the spec's path in
[netfyr/specs](https://github.com/netfyr/specs), without the `.md`:

```
Add per-field priority merge to the reconciler

Reconciliation needs to preserve DHCP-assigned addresses when a static
policy touches an unrelated field on the same interface.

Spec: core/201-reconciliation-merge
```

The checker is `scripts/check-spec-ref.sh --msg MESSAGE_FILE` or
`--range BASE..HEAD`. It rejects missing references, mixed-spec branches,
mismatched completion references and completion before the final non-exempt
commit. `VERIFY_SPEC_EXISTS=1` resolves against published requirements; CI pins
the requirements revision before resolving paths. The PR description, independent
of its title, must end with the same trailer. An exempt-only PR still needs that
description reference.

Published root-level top-level requirements use their slug. Their reviewed
classification is listed in `scripts/top-level-specs`; adding an entry requires
confirming that the document is published as top-level. CI also checks its file
exists. Top-level requirements remain approved and cannot carry an `implements`
closer. Feature references retain their numbered path, and only a completing
commit additionally carries `implements netfyr/specs#N`, using the original spec
publication PR number rather than its filename number.

Commit messages describe what changed and why. Report commands and verification
results separately in review evidence rather than in commit messages.

When fixing a bug introduced by an earlier commit, add a
`Fixes: HASH12 ("COMMIT SUBJECT")` trailer with the 12-character hash.

Split work into logical commits: one reviewable increment each. Reviewers read per
commit, so do not bundle a refactor with the feature that motivated it.

Before opening a pull request, make sure `make test` passes, and once the workspace has
crates, that `cargo test` and `make clippy` do too. State what the change does and why,
and update the affected documentation.

Add a `CHANGELOG.md` entry under `## [Unreleased]`, naming the spec in parentheses, only
when a user or packager would notice the change. Scaffolding, test infrastructure, and
contributor tooling stay out of it: the audience for that file is people deciding whether
to upgrade, not people working on netfyr.
