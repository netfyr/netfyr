#!/usr/bin/env bash
# Integration tests for SPEC-504: Architecture Documentation
# Validates that docs/architecture.md and docs/workflows.md exist and contain
# the required Mermaid diagrams and content described in the acceptance criteria.
#
# Does NOT require a network namespace — these are pure file-content checks.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=helpers.sh
source "${SCRIPT_DIR}/helpers.sh"

REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

PASS=0
FAIL=0

pass() { echo "PASS: $*"; ((PASS++)) || true; }
fail() { echo "FAIL: $*" >&2; ((FAIL++)) || true; }

check_file_exists() {
    local file="$1"
    if [[ -f "${REPO_ROOT}/${file}" ]]; then
        pass "${file} exists"
    else
        fail "${file} does not exist"
    fi
}

check_contains() {
    local file="$1"
    local pattern="$2"
    local description="$3"
    local path="${REPO_ROOT}/${file}"
    if [[ ! -f "${path}" ]]; then
        fail "${file} missing — cannot check: ${description}"
        return
    fi
    if grep -qE -- "${pattern}" "${path}"; then
        pass "${file}: ${description}"
    else
        fail "${file}: ${description} (pattern not found: '${pattern}')"
    fi
}

count_matches() {
    local file="$1"
    local pattern="$2"
    local path="${REPO_ROOT}/${file}"
    grep -cE "${pattern}" "${path}" 2>/dev/null || echo 0
}

echo "=== Scenario: Architecture document exists ==="

check_file_exists "docs/architecture.md"
check_contains "docs/architecture.md" '```mermaid' \
    "contains a Mermaid block"
check_contains "docs/architecture.md" 'graph TD' \
    "contains a graph TD crate dependency diagram"
# Four-layer architecture
check_contains "docs/architecture.md" 'Layer 0|Foundation' \
    "describes Layer 0 (Foundation)"
check_contains "docs/architecture.md" 'Layer 1|Domain' \
    "describes Layer 1 (Domain logic)"
check_contains "docs/architecture.md" 'Layer 2|I/O' \
    "describes Layer 2 (I/O)"
check_contains "docs/architecture.md" 'Layer 3|Binar' \
    "describes Layer 3 (Binaries)"
# Mode diagrams
check_contains "docs/architecture.md" '[Ss]tandalone' \
    "describes standalone mode"
check_contains "docs/architecture.md" '[Dd]aemon' \
    "describes daemon mode"
check_contains "docs/architecture.md" 'graph LR' \
    "contains graph LR mode diagrams"
# Data model
check_contains "docs/architecture.md" 'classDiagram' \
    "contains a Mermaid classDiagram (data model)"
check_contains "docs/architecture.md" 'State' \
    "data model references State"
check_contains "docs/architecture.md" 'Selector' \
    "data model references Selector"
check_contains "docs/architecture.md" 'Value' \
    "data model references Value"
check_contains "docs/architecture.md" 'Provenance' \
    "data model references Provenance"
# Key concepts table
check_contains "docs/architecture.md" '\| ' \
    "contains a key concepts reference table (Markdown table)"
check_contains "docs/architecture.md" '\|---|\| ---' \
    "key concepts table has a header separator row"

echo ""
echo "=== Scenario: Workflow document exists ==="

check_file_exists "docs/workflows.md"
check_contains "docs/workflows.md" 'sequenceDiagram' \
    "contains Mermaid sequenceDiagram blocks"
check_contains "docs/workflows.md" '[Ss]tandalone.*[Aa]pply|[Aa]pply.*[Ss]tandalone' \
    "contains sequence diagram for standalone apply"
check_contains "docs/workflows.md" '[Dd]aemon.*[Aa]pply|[Aa]pply.*[Dd]aemon' \
    "contains sequence diagram for daemon-mode apply"
check_contains "docs/workflows.md" '[Qq]uery' \
    "contains sequence diagram for query"
check_contains "docs/workflows.md" '[Rr]evert' \
    "contains sequence diagram for revert"
check_contains "docs/workflows.md" '[Ss]tartup|[Dd]aemon.*[Ss]tart' \
    "contains sequence diagram for daemon startup"
check_contains "docs/workflows.md" 'DHCP|dhcp' \
    "contains sequence diagram for DHCP lease lifecycle"
check_contains "docs/workflows.md" '[Ll]ease' \
    "DHCP section describes lease lifecycle"
check_contains "docs/workflows.md" '[Ii]pv6.*[Aa]uto|[Ii]pv6[Aa]uto|ipv6auto' \
    "contains sequence diagram for ipv6auto lifecycle"
check_contains "docs/workflows.md" '[Dd][Hh][Cc][Pp][Vv]6|dhcpv6' \
    "contains sequence diagram for DHCPv6 client lifecycle"

# Count sequence diagrams
SEQ_COUNT=$(count_matches "docs/workflows.md" 'sequenceDiagram')
if [[ "${SEQ_COUNT}" -ge 8 ]]; then
    pass "docs/workflows.md: has at least 8 sequenceDiagram blocks (found ${SEQ_COUNT})"
else
    fail "docs/workflows.md: expected at least 8 sequenceDiagram blocks, found ${SEQ_COUNT}"
fi

echo ""
echo "=== Scenario: Diagrams reference all workspace crates ==="

for crate in \
    netfyr-state \
    netfyr-policy \
    netfyr-reconcile \
    netfyr-backend \
    netfyr-journal \
    netfyr-varlink \
    netfyr-cli \
    netfyr-daemon \
    netfyr-test-utils
do
    check_contains "docs/architecture.md" "${crate}" \
        "dependency graph includes node for ${crate}"
done

check_contains "docs/architecture.md" "-.->"\
    "netfyr-test-utils shown as dev-dependency (dashed arrow -.->)"

echo ""
echo "=== Results: ${PASS} passed, ${FAIL} failed ==="

if [[ "${FAIL}" -gt 0 ]]; then
    exit 1
fi
