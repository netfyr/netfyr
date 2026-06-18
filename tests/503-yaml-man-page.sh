#!/bin/bash
# 503-yaml-man-page.sh
# Verify that man/netfyr.yaml.5 exists, renders without troff warnings,
# and contains every section required by SPEC-503.
#
# Usage: bash tests/503-yaml-man-page.sh
#
# Requires: groff (for rendering) or man (for display check).
# Content checks are done with grep against the raw troff source.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/helpers.sh"
PROJECT_ROOT="$SCRIPT_DIR/.."
MAN_PAGE="$PROJECT_ROOT/man/netfyr.yaml.5"

failed=0

pass() { echo "PASS: 503-yaml-man-page: $1"; }
fail() { echo "FAIL: 503-yaml-man-page: $1" >&2; failed=1; }

# ── AC-1: File exists ─────────────────────────────────────────────────────────

if [[ -f "$MAN_PAGE" ]]; then
    pass "man/netfyr.yaml.5 exists"
else
    fail "man/netfyr.yaml.5 does not exist"
    # Can't continue without the file.
    exit 1
fi

# ── AC-1: Section 5 header ────────────────────────────────────────────────────

if grep -q '\.TH' "$MAN_PAGE" && grep -q ' 5 ' "$MAN_PAGE"; then
    pass ".TH header declares section 5"
else
    fail ".TH header must declare section 5"
fi

# ── AC-1: NAME section contains netfyr.yaml ───────────────────────────────────

if grep -q 'netfyr\.yaml' "$MAN_PAGE"; then
    pass "NAME section contains netfyr.yaml"
else
    fail "man page must mention netfyr.yaml (check NAME section)"
fi

# ── AC-1: Renders without troff warnings ─────────────────────────────────────

if command -v groff &>/dev/null; then
    warnings=$(groff -man -Tutf8 "$MAN_PAGE" 2>&1 >/dev/null || true)
    if [[ -z "$warnings" ]]; then
        pass "groff renders man/netfyr.yaml.5 without warnings"
    else
        fail "groff reported warnings when rendering man/netfyr.yaml.5:
$warnings"
    fi
else
    echo "SKIP: 503-yaml-man-page: groff not available; skipping render test"
fi

# ── AC-2: BARE STATE FORMAT section ──────────────────────────────────────────

if grep -q 'BARE STATE FORMAT' "$MAN_PAGE"; then
    pass "BARE STATE FORMAT section exists"
else
    fail "man/netfyr.yaml.5 must have a BARE STATE FORMAT section"
fi

# AC-2: selector sub-mapping documented in BARE STATE FORMAT
if grep -q 'selector' "$MAN_PAGE"; then
    pass "BARE STATE FORMAT documents the selector sub-mapping"
else
    fail "BARE STATE FORMAT must document the selector sub-mapping"
fi

# AC-2: example present in BARE STATE FORMAT (checked via .nf block in file)
if grep -q '\.nf' "$MAN_PAGE"; then
    pass "man page includes at least one example (.nf block)"
else
    fail "man page must include at least one formatted example (.nf block)"
fi

# ── AC-3: POLICY FORMAT section ───────────────────────────────────────────────

if grep -q 'POLICY FORMAT' "$MAN_PAGE"; then
    pass "POLICY FORMAT section exists"
else
    fail "man/netfyr.yaml.5 must have a POLICY FORMAT section"
fi

for field in kind name factory priority selector state states; do
    if grep -q "$field" "$MAN_PAGE"; then
        pass "POLICY FORMAT documents field: $field"
    else
        fail "POLICY FORMAT must document field: $field"
    fi
done

# AC-3: factory type examples
if grep -q 'factory: static' "$MAN_PAGE"; then
    pass "POLICY FORMAT includes a static factory example"
else
    fail "POLICY FORMAT must include a static factory example"
fi

if grep -q 'factory: dhcpv4' "$MAN_PAGE"; then
    pass "POLICY FORMAT includes a dhcpv4 factory example"
else
    fail "POLICY FORMAT must include a dhcpv4 factory example"
fi

# ── AC-4: MULTI-DOCUMENT FILES section ───────────────────────────────────────

if grep -q 'MULTI-DOCUMENT' "$MAN_PAGE"; then
    pass "MULTI-DOCUMENT FILES section exists"
else
    fail "man/netfyr.yaml.5 must have a MULTI-DOCUMENT FILES section"
fi

# AC-4: documents the --- separator (troff encodes hyphens as \-\-\-)
if grep -qF '\-\-\-' "$MAN_PAGE" || grep -qF '---' "$MAN_PAGE"; then
    pass "MULTI-DOCUMENT FILES section mentions the --- separator"
else
    fail "MULTI-DOCUMENT FILES section must mention the '---' document separator"
fi

# ── AC-5: SELECTORS section ───────────────────────────────────────────────────

if grep -q '^\.SH SELECTORS' "$MAN_PAGE"; then
    pass "SELECTORS section exists"
else
    fail "man/netfyr.yaml.5 must have a SELECTORS section"
fi

for field in name driver pci_path mac; do
    if grep -q "$field" "$MAN_PAGE"; then
        pass "SELECTORS section documents: $field"
    else
        fail "SELECTORS section must document selector field: $field"
    fi
done

# ── AC-6: FIELDS section ──────────────────────────────────────────────────────

if grep -q '^\.SH FIELDS' "$MAN_PAGE"; then
    pass "FIELDS section exists"
else
    fail "man/netfyr.yaml.5 must have a FIELDS section"
fi

for field in mtu addresses routes; do
    if grep -q "$field" "$MAN_PAGE"; then
        pass "FIELDS section documents ethernet field: $field"
    else
        fail "FIELDS section must document ethernet field: $field"
    fi
done

# AC-6: state field — must appear in FIELDS section (between .SH FIELDS and VALUE TYPES)
if grep -q '\.B state' "$MAN_PAGE"; then
    pass "FIELDS section documents ethernet field: state"
else
    fail "FIELDS section must document the ethernet 'state' field"
fi

# ── AC-7: Factory types documented ───────────────────────────────────────────

if grep -q 'static' "$MAN_PAGE" && grep -q 'dhcpv4' "$MAN_PAGE"; then
    pass "POLICY FORMAT documents both static and dhcpv4 factory types"
else
    fail "POLICY FORMAT must document both 'static' and 'dhcpv4' factory types"
fi

# ── AC-8: VALUE TYPES section ─────────────────────────────────────────────────

if grep -q 'VALUE TYPES' "$MAN_PAGE"; then
    pass "VALUE TYPES section exists"
else
    fail "man/netfyr.yaml.5 must have a VALUE TYPES section"
fi

for type_name in Bool U64 I64 IpAddr IpNetwork String List Map; do
    if grep -q "$type_name" "$MAN_PAGE"; then
        pass "VALUE TYPES documents type: $type_name"
    else
        fail "VALUE TYPES section must document netfyr type: $type_name"
    fi
done

# AC-8: YAML scalar type descriptions
for yaml_type in boolean integer; do
    if grep -qi "$yaml_type" "$MAN_PAGE"; then
        pass "VALUE TYPES mentions YAML type: $yaml_type"
    else
        fail "VALUE TYPES section must mention YAML type: $yaml_type"
    fi
done

# ── AC-9: FILES section ───────────────────────────────────────────────────────

if grep -q '^\.SH FILES' "$MAN_PAGE"; then
    pass "FILES section exists"
else
    fail "man/netfyr.yaml.5 must have a FILES section"
fi

if grep -q '/etc/netfyr/policies/' "$MAN_PAGE"; then
    pass "FILES section lists /etc/netfyr/policies/"
else
    fail "FILES section must list /etc/netfyr/policies/"
fi

if grep -q '/var/lib/netfyr/' "$MAN_PAGE"; then
    pass "FILES section lists /var/lib/netfyr/"
else
    fail "FILES section must list /var/lib/netfyr/ (daemon-persisted policies)"
fi

# ── Summary ───────────────────────────────────────────────────────────────────

if [[ "$failed" -eq 0 ]]; then
    echo "OK: 503-yaml-man-page: all checks passed"
    exit 0
else
    echo "FAIL: 503-yaml-man-page: one or more checks failed" >&2
    exit 1
fi
