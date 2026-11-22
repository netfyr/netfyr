#!/bin/bash
# tags: meta
# The runner's tag filtering: OR across comma-separated terms, AND within a term,
# everything when unfiltered, and untagged tests excluded once a filter is given.

set -uo pipefail

runner="$(cd "$(dirname "$0")/.." && pwd)/scripts/run-tests.sh"
[ -x "$runner" ] || { echo "FAIL: $runner not found or not executable" >&2; exit 1; }

tmp="$(mktemp -d)" || { echo "FAIL: mktemp -d failed" >&2; exit 1; }
trap 'rm -rf "$tmp"' EXIT

make_test() {
    printf '#!/bin/bash\n# tags: %s\nexit 0\n' "$2" >"$tmp/$1.sh"
}

make_test alpha   "ipv4 routing"
make_test beta    "ipv4"
make_test gamma   "schema"
printf '#!/bin/bash\nexit 0\n' >"$tmp/untagged.sh"

# Matches FAIL as well as PASS: a test that ran and failed must not look filtered out.
# Stderr is left alone so that a runner that died says why, rather than looking like
# a filter that matched nothing.
ran() {
    TESTS_DIR="$tmp" SKIP_BUILD=1 bash "$runner" "$@" \
        | sed -n 's/^\(PASS\|FAIL\) \(.*\)\.sh$/\2/p' | sort | paste -sd' '
}

status=0
fail() { echo "FAIL: $*" >&2; status=1; }

# Status only, for the cases that assert how the runner exited rather than what ran.
runs() { TESTS_DIR="$1" SKIP_BUILD=1 bash "$runner" "${@:2}" >/dev/null 2>&1; }

check() {
    local expected="$1" desc="$2"; shift 2
    local actual
    actual="$(ran "$@")"
    [ "$actual" = "$expected" ] || fail "$desc: expected '$expected', got '$actual'"
    # Empty output is also what a runner that never started produces, so an
    # expectation of "nothing ran" has to confirm the run itself succeeded.
    [ -n "$expected" ] || runs "$tmp" "$@" || fail "$desc: the runner exited non-zero"
}

check "alpha beta gamma untagged" "no filter runs everything including untagged"
check "alpha beta gamma" "OR matches either tag, untagged excluded" TAGS=ipv4,schema
check "alpha" "AND requires every tag in the term" TAGS=ipv4+routing
check "alpha beta" "single tag matches every test carrying it" TAGS=ipv4
check "alpha gamma" "AND term and OR term combine" TAGS=ipv4+routing,schema
check "" "an unmatched tag runs nothing" TAGS=nosuchtag
check "" "a tag that is only a prefix of a real one matches nothing" TAGS=ipv

# A lone separator selects no tags, which must not degrade into "match all".
check "" "a bare + matches nothing" TAGS=+
check "" "a bare comma matches nothing" TAGS=,

REQUIRE_TEST_SELECTION=1 runs "$tmp" TAGS=nosuchtag \
    && fail "a required empty selection reported success"
REQUIRE_TEST_SELECTION=1 runs "$tmp" TAGS=ipv4 \
    || fail "a required nonempty selection failed"
REQUIRE_TEST_SELECTION=invalid runs "$tmp" TAGS=ipv4 \
    && fail "an invalid required-selection policy was accepted"

# Tags are literal. Globbing would make results depend on the working directory,
# so this runs from a directory holding a file named after a real tag.
touch "$tmp/ipv4"
glob_ran="$(cd "$tmp" && ran 'TAGS=ipv*')"
[ -z "$glob_ran" ] || fail "glob metachar expanded against the cwd, ran '$glob_ran'"
rm -f "$tmp/ipv4"

actual="$(TAGS=schema ran)"
[ "$actual" = "gamma" ] || fail "TAGS env var ignored, got '$actual'"

runs /nonexistent && fail "runner exited 0 with no test scripts at all"

# Nothing to select from is a broken run whether or not a filter narrowed it.
runs /nonexistent TAGS=meta && fail "a filter turned an empty tests directory into a pass"

runs "$tmp" --bogus && fail "runner accepted an unknown argument"

# The runner is what tells a test where the binaries are; FR-011 rests on it.
# shellcheck disable=SC2016  # the probe expands the variable when it runs, not here
printf '#!/bin/bash\n# tags: probe\nprintf %%s "${NETFYR_TARGET_DIR:-unset}" >"%s/seen"\n' "$tmp" \
    >"$tmp/probe.sh"
runs "$tmp" TAGS=probe
seen="$(cat "$tmp/seen" 2>/dev/null)"
# Read back independently of the runner's own extraction, so this is an oracle
# rather than the same code agreeing with itself.
expected_target="$(cd "$(dirname "$runner")/.." && cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
[ -n "$expected_target" ] && [ "$seen" = "$expected_target" ] \
    || fail "runner exported NETFYR_TARGET_DIR='$seen', expected '$expected_target'"
rm -f "$tmp/probe.sh" "$tmp/seen"

printf '#!/bin/bash\n# tags: broken\nexit 1\n' >"$tmp/broken.sh"
check "broken" "a failing test still counts as having run" TAGS=broken
runs "$tmp" TAGS=broken && fail "runner exited 0 despite a failing test"

exit "$status"
