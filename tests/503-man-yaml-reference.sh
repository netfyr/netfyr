#!/bin/bash
# 503-man-yaml-reference.sh
# Verify that man/netfyr.yaml.5 exists, renders without troff warnings, and
# documents all formats and fields specified in SPEC-503.
#
# Usage: bash tests/503-man-yaml-reference.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
MAN_FILE="$REPO_ROOT/man/netfyr.yaml.5"

failed=0

fail() {
    echo "FAIL: 503-man-yaml-reference: $1" >&2
    failed=1
}

# Scenario: Man page exists
if [[ ! -f "$MAN_FILE" ]]; then
    echo "FAIL: 503-man-yaml-reference: man/netfyr.yaml.5 does not exist" >&2
    exit 1
fi

# Scenario: Man page renders without troff warnings
# Try groff first (more widely available in CI), fall back to man --warnings.
if command -v groff >/dev/null 2>&1; then
    warnings=$(groff -man -Tascii "$MAN_FILE" 2>&1 1>/dev/null || true)
    if [[ -n "$warnings" ]]; then
        fail "groff reported warnings: $warnings"
    fi
elif command -v man >/dev/null 2>&1; then
    # man --warnings sends troff diagnostics to stderr; suppress normal output.
    warnings=$(MANROFFOPT="-ww" man "$MAN_FILE" 2>&1 1>/dev/null || true)
    if [[ -n "$warnings" ]]; then
        fail "man reported warnings: $warnings"
    fi
fi

# Scenario: NAME section contains "netfyr.yaml"
name_section=$(sed -n '/^\.SH NAME/,/^\.SH/p' "$MAN_FILE")
if ! echo "$name_section" | grep -q "netfyr\.yaml"; then
    fail "NAME section does not contain 'netfyr.yaml'"
fi

# Scenario: BARE STATE FORMAT section is documented with selector: sub-mapping
# and at least one example.
if ! grep -q 'SH.*BARE STATE FORMAT' "$MAN_FILE"; then
    fail "BARE STATE FORMAT section is missing"
fi

bare_section=$(sed -n '/SH.*BARE STATE FORMAT/,/^\.SH/p' "$MAN_FILE")

if ! echo "$bare_section" | grep -q "selector"; then
    fail "BARE STATE FORMAT does not describe the selector: sub-mapping"
fi

if ! echo "$bare_section" | grep -q "^\.nf"; then
    fail "BARE STATE FORMAT section has no inline example (.nf block)"
fi

# Scenario: POLICY FORMAT section documents kind, name, factory, priority,
# selector, state, and states, with examples for static and dhcpv4.
if ! grep -q 'SH.*POLICY FORMAT' "$MAN_FILE"; then
    fail "POLICY FORMAT section is missing"
fi

policy_section=$(sed -n '/SH.*POLICY FORMAT/,/^\.SH/p' "$MAN_FILE")

for field in kind name factory priority selector state states; do
    if ! echo "$policy_section" | grep -q "\\b${field}\\b"; then
        fail "POLICY FORMAT does not document field '${field}'"
    fi
done

if ! echo "$policy_section" | grep -q "static"; then
    fail "POLICY FORMAT does not include a static factory example"
fi

if ! echo "$policy_section" | grep -q "dhcpv4"; then
    fail "POLICY FORMAT does not include a dhcpv4 factory example"
fi

# Scenario: MULTI-DOCUMENT FILES section explains the "---" separator
# and includes at least one example.
if ! grep -q 'SH.*MULTI-DOCUMENT FILES' "$MAN_FILE"; then
    fail "MULTI-DOCUMENT FILES section is missing"
fi

multi_section=$(sed -n '/SH.*MULTI-DOCUMENT FILES/,/^\.SH/p' "$MAN_FILE")

# Troff encodes literal "--" as "\-\-"; "---" appears as "\-\-\-" in source.
if ! echo "$multi_section" | grep -qE '(---|\\-\\-\\-)'; then
    fail "MULTI-DOCUMENT FILES does not document the '---' document separator"
fi

if ! echo "$multi_section" | grep -q "^\.nf"; then
    fail "MULTI-DOCUMENT FILES section has no inline example (.nf block)"
fi

# Scenario: All selector fields are documented (name, driver, pci_path, mac).
if ! grep -q '^\.SH SELECTORS' "$MAN_FILE"; then
    fail "SELECTORS section is missing"
fi

selectors_section=$(sed -n '/^\.SH SELECTORS/,/^\.SH/p' "$MAN_FILE")

for field in name driver pci_path mac; do
    if ! echo "$selectors_section" | grep -q "\\b${field}\\b"; then
        fail "SELECTORS section does not document '${field}'"
    fi
done

# Scenario: All ethernet fields are documented (mtu, enabled, ipv4, ipv6).
if ! grep -q '^\.SH FIELDS' "$MAN_FILE"; then
    fail "FIELDS section is missing"
fi

fields_section=$(sed -n '/^\.SH FIELDS/,/^\.SH/p' "$MAN_FILE")

for field in mtu enabled ipv4 ipv6; do
    if ! echo "$fields_section" | grep -q "\\b${field}\\b"; then
        fail "FIELDS section does not document '${field}'"
    fi
done

# Scenario: Factory types documented (checked via POLICY FORMAT).
for factory in static dhcpv4 ipv6auto; do
    if ! echo "$policy_section" | grep -q "\\b${factory}\\b"; then
        fail "POLICY FORMAT does not document factory type '${factory}'"
    fi
done

# Scenario: ipv6auto factory documents SLAAC and DHCPv6.
if ! echo "$policy_section" | grep -qi "slaac\|router advertisement\|dhcpv6"; then
    fail "POLICY FORMAT does not document SLAAC or DHCPv6 for ipv6auto factory"
fi

# Scenario: VALUE TYPES section shows the YAML-to-netfyr type mapping.
if ! grep -q 'SH.*VALUE TYPES' "$MAN_FILE"; then
    fail "VALUE TYPES section is missing"
fi

value_section=$(sed -n '/SH.*VALUE TYPES/,/^\.SH/p' "$MAN_FILE")

for type in Bool U64 I64 IpAddr IpNetwork String List Map; do
    if ! echo "$value_section" | grep -q "\\b${type}\\b"; then
        fail "VALUE TYPES section does not document netfyr type '${type}'"
    fi
done

# Scenario: FILES section lists /etc/netfyr/policies/ and /var/lib/netfyr/policies/.
if ! grep -q '^\.SH FILES' "$MAN_FILE"; then
    fail "FILES section is missing"
fi

files_section=$(sed -n '/^\.SH FILES/,/^\.SH/p' "$MAN_FILE")

if ! echo "$files_section" | grep -q "/etc/netfyr/policies/"; then
    fail "FILES section does not list /etc/netfyr/policies/"
fi

if ! echo "$files_section" | grep -q "/var/lib/netfyr/policies/"; then
    fail "FILES section does not list /var/lib/netfyr/policies/"
fi

if [[ "$failed" -eq 1 ]]; then
    exit 1
fi

echo "PASS: 503-man-yaml-reference"
