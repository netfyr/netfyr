# Contributing to netfyr

## How work is organized

Choose work and coordinate with the assigned implementers on the
[core planning page](https://specs.netfyr.io/roadmap?ns=netfyr%2Fspecs).
Read the governing spec, its dependencies and the applicable top-level rules at
the merged revision in [netfyr/specs](https://github.com/netfyr/specs). Check
which dependencies have landed in code and raise missing ones with their owners.
Each feature PR advances one spec. A spec can take several PRs to complete.

Propose new behavior or revise requirements in SpecDoc. A board draft can be
ahead of the published requirements. Use the merged text to assess the
implementation. See [Contributing specifications](https://github.com/netfyr/specs/blob/main/CONTRIBUTING.md)
and the [development guide](https://netfyr.io/development/).

## Development workflow

Use Linux, Git, Make and a Rust toolchain that supports the workspace's
[edition and minimum Rust version](Cargo.toml). Work in the library that owns
the behavior. Add a new crate to the workspace members when the feature needs one.

```sh
cargo build                    # compile the workspace
cargo test                     # Rust unit, integration and documentation tests
make test                      # build and run the host shell checks
make test TAGS=build,meta       # shell checks tagged build OR meta
make test TAGS=meta+policy      # shell checks tagged meta AND policy
make fmt                       # format
make clippy                    # lint
make hooks                     # install the git hooks (do this once)
```

`make hooks` needs [pre-commit](https://pre-commit.com) on your `PATH`.
The [hook configuration](.pre-commit-config.yaml) runs `cargo fmt --check` on
commits that touch Rust and Clippy before pushes that touch Rust. An existing
hook of either type still runs as `<hook>.legacy`.

## Code conventions

Rust 2024 edition. Code is formatted with `cargo fmt` and must be clippy-clean.

Crates inherit shared metadata from the workspace rather than restating it:

```toml
[package]
name = "netfyr-example"
edition.workspace = true
version.workspace = true
license.workspace = true
rust-version.workspace = true
repository.workspace = true
```

### Dependency policy

Only add an external dependency when implementing the functionality yourself would be
unreasonable. `rtnetlink` for netlink, `clap` for CLI parsing, and `tokio` for async are
the kind of thing that qualifies. A date formatter or an argument splitter is not.

Prefer the standard library. When two crates do the same job, take the one with fewer
transitive dependencies. Every dependency costs build time, binary size, audit surface,
and a supply chain you now depend on.

For low-level networking, check the [rust-netlink crates](https://github.com/rust-netlink)
and [Nispor](https://github.com/nispor/nispor) before writing a local adapter.
Document the API gap if you need one. Justify each dependency in a manifest
comment explaining what the standard library lacks.

```toml
# socket2: sets SO_BINDTODEVICE, which tokio's UdpSocket does not expose.
socket2 = "0.5"
```

## Tests

Unit tests are `#[cfg(test)]` modules inside the source file they cover. They test
parsing, validation, data structure invariants, and pure functions. They must not depend
on external tools, network namespaces, or built binaries.

Use Rust integration tests in a crate's `tests/` directory to exercise its public
API. Host shell tests in `tests/*.sh` cover the build, repository conventions
and shell runner.

For backend changes that affect Linux networking, build the backend probe and
run the pytest acceptance suite in a disposable VM. The suite checks kernel
state independently with `ip`. Keep the results outside the guest before
rollback. The [backend acceptance guide](tests/integration/coverage.md) lists
the prerequisites, commands and covered requirements.

### Writing a shell test

Name the script for the behavior it checks, as in `workspace-builds.sh`.
The runner reads tags from a comment within the first ten lines.

```bash
#!/bin/bash
# tags: build meta
```

Choose tags that describe what the test exercises. An unfiltered run includes
untagged scripts. If a filter matches nothing, the runner prints
`no tests matched` and exits successfully. Check the selection before
reporting a focused run.

The runner executes every `tests/*.sh`, so shared helper files must not live directly in
`tests/` or they will be run as tests. Put them in `tests/lib/` and source them.

Locate built binaries through `NETFYR_TARGET_DIR`. The
[shell runner](scripts/run-tests.sh) exports Cargo's resolved target directory,
including any `CARGO_TARGET_DIR` or `build.target-dir` override. For a direct
run against a default build, a test can fall back to the repository's `target/`.

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

Use imperative subjects. Split work into commits that a reviewer can assess
individually. Explain the problem, what changes and any compatibility limits
in the commit messages and PR description.

Put exactly one `Spec:` trailer in the final paragraph of each ordinary commit.
End the PR description with a trailer block naming the same spec, even if the
PR contains only exempt commits. Merge, revert, `fixup!`, `squash!` and
`amend!` commits are exempt. Use the published path without `.md`. For
repository-wide work, cite the applicable `meta/` spec.

```
Accept CIDR strings in reconciliation

Library callers can supply an IPv4 address as a CIDR string. Match it
to the equivalent typed address when planning changes.

Spec: core/005-reconciliation
```

When a PR completes a feature, add `implements netfyr/specs#N` to the last
non-exempt commit. Use the original spec publication PR number. For example,
`Spec: core/004-network-backend` pairs with `implements netfyr/specs#17`.
Preserve both lines when merging. Partial work uses the governing `Spec:`
reference without a completion line. Top-level specs remain approved. See the
[merged spec-reference rules](https://github.com/netfyr/specs/blob/main/workspace/009-spec-references-in-commits-and-pull-requests.md).

Put the same qualified `implements` line in the completing PR description so
SpecDoc can find it for [implementation feedback](https://specdoc.josie.cloud/spec-lifecycle/#learning-from-implementation-review).
A follow-up PR for an already implemented feature can link its description
the same way. For a regression with a known introducing commit, add
`Fixes: HASH12 ("COMMIT SUBJECT")` to the fix commit, using the 12-character
hash and subject of the commit that introduced the bug.

Before opening a PR, run `cargo fmt --check`, `cargo test`, `make test` and
`make clippy`. Keep verification checklists, commands, results and gaps in
chat or separate review evidence. Update the affected documentation.

Add a `CHANGELOG.md` entry under `## [Unreleased]` for behavior or
compatibility changes that users or packagers would notice. Name the spec in
parentheses so readers can find the requirements behind the entry.
