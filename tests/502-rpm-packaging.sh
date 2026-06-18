#!/bin/bash
# 502-rpm-packaging.sh
# Verify that RPM packaging files for netfyr exist and are correctly structured.
#
# Tests acceptance criteria from SPEC-502 that can be verified without a live
# RPM build environment: file existence, static content, metadata correctness,
# and packaging conventions.
#
# Usage: bash tests/502-rpm-packaging.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"

PROJECT_ROOT="$SCRIPT_DIR/.."
SPEC="$PROJECT_ROOT/netfyr.spec"
BUILD_SCRIPT="$PROJECT_ROOT/scripts/build-rpm.sh"
SERVICE_UNIT="$PROJECT_ROOT/dist/systemd/netfyr.service"
SOCKET_UNIT="$PROJECT_ROOT/dist/systemd/netfyr.socket"
GITIGNORE="$PROJECT_ROOT/.gitignore"

failed=0

# ---------------------------------------------------------------------------
# Spec file exists (prerequisite for all other checks)
# ---------------------------------------------------------------------------

if [[ ! -f "$SPEC" ]]; then
    echo "FAIL: 502-rpm-packaging: netfyr.spec does not exist at workspace root" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Scenario: RPM spec file is valid — metadata fields
# ---------------------------------------------------------------------------

spec_name=$(grep '^Name:' "$SPEC" | awk '{print $2}')
if [[ "$spec_name" != "netfyr" ]]; then
    echo "FAIL: 502-rpm-packaging: spec Name is '$spec_name', expected 'netfyr'" >&2
    failed=1
fi

if ! grep -qE '^Version:' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec is missing a Version field" >&2
    failed=1
fi

if ! grep -qE '^License:.*MIT' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec License field is missing or not MIT" >&2
    failed=1
fi

if ! grep -qE '^URL:' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec URL field is missing" >&2
    failed=1
fi

if ! grep -qE '^Source0:.*\.tar\.gz' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec missing Source0 (source tarball)" >&2
    failed=1
fi

if ! grep -qE '^Source1:.*vendor.*\.tar\.gz' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec missing Source1 (vendor tarball)" >&2
    failed=1
fi

if ! grep -qE '^ExclusiveArch:.*%\{rust_arches\}' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec missing ExclusiveArch with %{rust_arches}" >&2
    failed=1
fi

# ---------------------------------------------------------------------------
# Spec file: required BuildRequires for the Rust + systemd toolchain
# ---------------------------------------------------------------------------

if ! grep -qE '^BuildRequires:.*cargo' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec missing BuildRequires for cargo" >&2
    failed=1
fi

if ! grep -qE '^BuildRequires:.*rust-packaging' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec missing BuildRequires for rust-packaging" >&2
    failed=1
fi

if ! grep -qE '^BuildRequires:.*systemd-rpm-macros' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec missing BuildRequires for systemd-rpm-macros" >&2
    failed=1
fi

# ---------------------------------------------------------------------------
# Spec file: %prep uses %cargo_prep (required by rust-packaging)
# ---------------------------------------------------------------------------

if ! grep -q '%cargo_prep' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec %prep section missing %cargo_prep" >&2
    failed=1
fi

# ---------------------------------------------------------------------------
# Spec file: %build uses %cargo_build
# ---------------------------------------------------------------------------

if ! grep -q '%cargo_build' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec %build section missing %cargo_build" >&2
    failed=1
fi

# ---------------------------------------------------------------------------
# Spec file: man pages generated via 'cargo run -p xtask', not 'cargo xtask'
# The cargo xtask alias may not exist after %cargo_prep rewrites .cargo/config.toml
# ---------------------------------------------------------------------------

if ! grep -q 'cargo run -p xtask' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec does not generate man pages via 'cargo run -p xtask -- man'" >&2
    failed=1
fi

# Active (non-comment) lines must not use the 'cargo xtask' alias directly
if grep -vE '^\s*#' "$SPEC" | grep -qE '\bcargo xtask\b'; then
    echo "FAIL: 502-rpm-packaging: spec has active (non-comment) 'cargo xtask' invocation — use 'cargo run -p xtask -- man'" >&2
    failed=1
fi

# ---------------------------------------------------------------------------
# Spec file: %check section smoke-tests the built binaries
# ---------------------------------------------------------------------------

if ! grep -q '^%check' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec missing %check section" >&2
    failed=1
fi

if ! grep -q 'netfyr --help' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec %check does not smoke-test 'netfyr --help'" >&2
    failed=1
fi

if ! grep -q 'netfyr-daemon --help' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec %check does not smoke-test 'netfyr-daemon --help'" >&2
    failed=1
fi

# ---------------------------------------------------------------------------
# Spec file: %files section for the main (CLI) package
# ---------------------------------------------------------------------------

# Extract lines between '%files' (exact) and the next '%files ' (subpackage)
main_files=$(awk '/^%files$/{found=1; next} found && /^%files /{found=0} found{print}' "$SPEC")

if ! echo "$main_files" | grep -q '%{_bindir}/netfyr'; then
    echo "FAIL: 502-rpm-packaging: spec %files missing CLI binary %{_bindir}/netfyr" >&2
    failed=1
fi

if ! echo "$main_files" | grep -q '%{_mandir}/man1/netfyr.1'; then
    echo "FAIL: 502-rpm-packaging: spec %files missing netfyr.1 man page" >&2
    failed=1
fi

if ! echo "$main_files" | grep -q '%{_mandir}/man1/netfyr-apply.1'; then
    echo "FAIL: 502-rpm-packaging: spec %files missing netfyr-apply.1 man page" >&2
    failed=1
fi

if ! echo "$main_files" | grep -q '%{_mandir}/man1/netfyr-query.1'; then
    echo "FAIL: 502-rpm-packaging: spec %files missing netfyr-query.1 man page" >&2
    failed=1
fi

if ! echo "$main_files" | grep -q '%{_mandir}/man1/netfyr-history.1'; then
    echo "FAIL: 502-rpm-packaging: spec %files missing netfyr-history.1 man page" >&2
    failed=1
fi

if ! echo "$main_files" | grep -q '%{_mandir}/man1/netfyr-revert.1'; then
    echo "FAIL: 502-rpm-packaging: spec %files missing netfyr-revert.1 man page" >&2
    failed=1
fi

if ! echo "$main_files" | grep -q '%{_mandir}/man5/netfyr.yaml.5'; then
    echo "FAIL: 502-rpm-packaging: spec %files missing netfyr.yaml.5 man page" >&2
    failed=1
fi

if ! echo "$main_files" | grep -q '%{_mandir}/man7/netfyr-examples.7'; then
    echo "FAIL: 502-rpm-packaging: spec %files missing netfyr-examples.7 man page" >&2
    failed=1
fi

if ! echo "$main_files" | grep -q 'bash-completion'; then
    echo "FAIL: 502-rpm-packaging: spec %files missing bash completion path" >&2
    failed=1
fi

# Config directory must use %dir to avoid removing it on uninstall
if ! echo "$main_files" | grep -qE '%dir.*%\{_sysconfdir\}/netfyr'; then
    echo "FAIL: 502-rpm-packaging: spec %files config directories must use %dir prefix" >&2
    failed=1
fi

if ! echo "$main_files" | grep -q '%license LICENSE'; then
    echo "FAIL: 502-rpm-packaging: spec %files missing %license LICENSE" >&2
    failed=1
fi

# LICENSE must not be manually installed (conflicts with %license)
if grep -qE '^install.*LICENSE' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec manually installs LICENSE (conflicts with the %license directive)" >&2
    failed=1
fi

# ---------------------------------------------------------------------------
# Spec file: %files daemon section
# ---------------------------------------------------------------------------

daemon_files=$(awk '/^%files daemon/{found=1; next} found && /^%files/{found=0} found && /^%changelog/{found=0} found{print}' "$SPEC")

if ! echo "$daemon_files" | grep -q '%{_bindir}/netfyr-daemon'; then
    echo "FAIL: 502-rpm-packaging: spec %files daemon missing %{_bindir}/netfyr-daemon" >&2
    failed=1
fi

if ! echo "$daemon_files" | grep -q '%{_mandir}/man8/netfyr-daemon.8'; then
    echo "FAIL: 502-rpm-packaging: spec %files daemon missing netfyr-daemon.8 man page" >&2
    failed=1
fi

if ! echo "$daemon_files" | grep -q '%{_unitdir}/netfyr.service'; then
    echo "FAIL: 502-rpm-packaging: spec %files daemon missing %{_unitdir}/netfyr.service" >&2
    failed=1
fi

if ! echo "$daemon_files" | grep -q '%{_unitdir}/netfyr.socket'; then
    echo "FAIL: 502-rpm-packaging: spec %files daemon missing %{_unitdir}/netfyr.socket" >&2
    failed=1
fi

if ! echo "$daemon_files" | grep -q '%license LICENSE'; then
    echo "FAIL: 502-rpm-packaging: spec %files daemon missing %license LICENSE" >&2
    failed=1
fi

# ---------------------------------------------------------------------------
# Scenario: Daemon RPM requires CLI package and systemd
# ---------------------------------------------------------------------------

daemon_pkg=$(awk '/^%package daemon/{found=1; next} found && /^%description/{found=0} found{print}' "$SPEC")

if ! echo "$daemon_pkg" | grep -q 'Requires:.*%{name}'; then
    echo "FAIL: 502-rpm-packaging: daemon subpackage missing Requires for CLI package (%{name})" >&2
    failed=1
fi

if ! echo "$daemon_pkg" | grep -q 'Requires:.*systemd'; then
    echo "FAIL: 502-rpm-packaging: daemon subpackage missing Requires for systemd" >&2
    failed=1
fi

# ---------------------------------------------------------------------------
# Spec file: systemd scriptlets for daemon package lifecycle
# ---------------------------------------------------------------------------

if ! grep -q '%systemd_post' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec missing %systemd_post scriptlet in %post daemon" >&2
    failed=1
fi

if ! grep -q '%systemd_preun' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec missing %systemd_preun scriptlet in %preun daemon" >&2
    failed=1
fi

if ! grep -q '%systemd_postun_with_restart' "$SPEC"; then
    echo "FAIL: 502-rpm-packaging: spec missing %systemd_postun_with_restart scriptlet in %postun daemon" >&2
    failed=1
fi

# ---------------------------------------------------------------------------
# Scenario: Build script exists and is executable
# ---------------------------------------------------------------------------

if [[ ! -f "$BUILD_SCRIPT" ]]; then
    echo "FAIL: 502-rpm-packaging: scripts/build-rpm.sh does not exist" >&2
    failed=1
else
    if [[ ! -x "$BUILD_SCRIPT" ]]; then
        echo "FAIL: 502-rpm-packaging: scripts/build-rpm.sh is not executable" >&2
        failed=1
    fi

    # Build script must pass a bash syntax check
    if ! bash -n "$BUILD_SCRIPT" 2>/dev/null; then
        echo "FAIL: 502-rpm-packaging: scripts/build-rpm.sh has bash syntax errors" >&2
        failed=1
    fi

    # Build script must reference the spec file to extract Name and Version
    if ! grep -qE '\bSPEC\b|netfyr\.spec' "$BUILD_SCRIPT"; then
        echo "FAIL: 502-rpm-packaging: build script does not reference netfyr.spec" >&2
        failed=1
    fi

    if ! grep -qE '\b(NAME|VERSION)\b' "$BUILD_SCRIPT"; then
        echo "FAIL: 502-rpm-packaging: build script does not extract Name/Version variables from spec" >&2
        failed=1
    fi

    # Build script must create a source tarball via git archive
    if ! grep -q 'git archive' "$BUILD_SCRIPT"; then
        echo "FAIL: 502-rpm-packaging: build script does not use 'git archive' for source tarball" >&2
        failed=1
    fi

    # Build script must create a vendor tarball
    if ! grep -q 'cargo vendor' "$BUILD_SCRIPT"; then
        echo "FAIL: 502-rpm-packaging: build script does not call 'cargo vendor' for vendor tarball" >&2
        failed=1
    fi

    # Build script must invoke rpmbuild
    if ! grep -q 'rpmbuild' "$BUILD_SCRIPT"; then
        echo "FAIL: 502-rpm-packaging: build script does not invoke rpmbuild" >&2
        failed=1
    fi
fi

# ---------------------------------------------------------------------------
# Scenario: Daemon RPM installs systemd units — service unit content
# ---------------------------------------------------------------------------

if [[ ! -f "$SERVICE_UNIT" ]]; then
    echo "FAIL: 502-rpm-packaging: dist/systemd/netfyr.service does not exist" >&2
    failed=1
else
    if ! grep -q 'Type=notify' "$SERVICE_UNIT"; then
        echo "FAIL: 502-rpm-packaging: netfyr.service is missing Type=notify" >&2
        failed=1
    fi

    if ! grep -q 'ExecStart=/usr/bin/netfyr-daemon' "$SERVICE_UNIT"; then
        echo "FAIL: 502-rpm-packaging: netfyr.service ExecStart must be /usr/bin/netfyr-daemon" >&2
        failed=1
    fi

    if ! grep -q 'RuntimeDirectory=netfyr' "$SERVICE_UNIT"; then
        echo "FAIL: 502-rpm-packaging: netfyr.service missing RuntimeDirectory=netfyr" >&2
        failed=1
    fi

    if ! grep -q 'StateDirectory=netfyr' "$SERVICE_UNIT"; then
        echo "FAIL: 502-rpm-packaging: netfyr.service missing StateDirectory=netfyr" >&2
        failed=1
    fi

    if ! grep -q 'WantedBy=multi-user.target' "$SERVICE_UNIT"; then
        echo "FAIL: 502-rpm-packaging: netfyr.service missing WantedBy=multi-user.target" >&2
        failed=1
    fi
fi

# ---------------------------------------------------------------------------
# Scenario: Daemon RPM installs socket unit — socket unit content
# ---------------------------------------------------------------------------

if [[ ! -f "$SOCKET_UNIT" ]]; then
    echo "FAIL: 502-rpm-packaging: dist/systemd/netfyr.socket does not exist" >&2
    failed=1
else
    if ! grep -q 'ListenStream=/run/netfyr/netfyr.sock' "$SOCKET_UNIT"; then
        echo "FAIL: 502-rpm-packaging: netfyr.socket ListenStream must be /run/netfyr/netfyr.sock" >&2
        failed=1
    fi

    if ! grep -q 'WantedBy=sockets.target' "$SOCKET_UNIT"; then
        echo "FAIL: 502-rpm-packaging: netfyr.socket missing WantedBy=sockets.target" >&2
        failed=1
    fi
fi

# ---------------------------------------------------------------------------
# vendor/ directory must be in .gitignore (never committed)
# ---------------------------------------------------------------------------

if [[ ! -f "$GITIGNORE" ]]; then
    echo "FAIL: 502-rpm-packaging: .gitignore does not exist" >&2
    failed=1
elif ! grep -qE '^/?vendor(/|$)' "$GITIGNORE"; then
    echo "FAIL: 502-rpm-packaging: vendor/ directory is not listed in .gitignore" >&2
    failed=1
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

if [[ "$failed" -eq 1 ]]; then
    exit 1
fi

echo "PASS: 502-rpm-packaging"
