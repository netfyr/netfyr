#!/bin/bash
# Discover and run the shell integration tests in tests/.
#
#   run-tests.sh                    all tests
#   run-tests.sh TAGS=ipv4,routing  tests tagged ipv4 OR routing
#   run-tests.sh TAGS=ipv4+routing  tests tagged ipv4 AND routing
#
# Env: TAGS is read when no argument gives one, SKIP_BUILD=1 skips the cargo build,
# TESTS_DIR overrides the test directory; REQUIRE_TEST_SELECTION=1 rejects an empty selection.

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tests_dir="${TESTS_DIR:-$repo_root/tests}"

filter="${TAGS:-}"
skip_build="${SKIP_BUILD:-}"
require_selection="${REQUIRE_TEST_SELECTION:-0}"
case "$require_selection" in
    0|1) ;;
    *) echo "FAIL: REQUIRE_TEST_SELECTION must be 0 or 1" >&2; exit 1 ;;
esac
for arg in "$@"; do
    case "$arg" in
        TAGS=*) filter="${arg#TAGS=}" ;;
        *) echo "FAIL: unknown argument '$arg'" >&2; exit 1 ;;
    esac
done

# These are the runner's inputs, read once above. A test that runs the runner
# would otherwise inherit them and narrow, or skip the build of, a run it meant
# to control itself. NETFYR_TARGET_DIR below is the only thing a test inherits.
unset TAGS SKIP_BUILD TESTS_DIR REQUIRE_TEST_SELECTION

command -v cargo >/dev/null || { echo "FAIL: cargo not found" >&2; exit 1; }

# Honors CARGO_TARGET_DIR and build.target-dir. Stderr is kept off stdout so a
# cargo warning cannot end up being parsed as JSON.
if ! metadata="$(cd "$repo_root" && cargo metadata --format-version 1 --no-deps 2>/dev/null)"; then
    echo "FAIL: cargo metadata rejected $repo_root/Cargo.toml:" >&2
    (cd "$repo_root" && cargo metadata --format-version 1 --no-deps >/dev/null) 2>&1 \
        | sed 's/^/    /' >&2
    exit 1
fi

case "$metadata" in
    *'"target_directory":"'*) ;;
    *) echo "FAIL: cargo metadata reported no target_directory" >&2; exit 1 ;;
esac
target_dir="${metadata#*\"target_directory\":\"}"
export NETFYR_TARGET_DIR="${target_dir%%\"*}"

if [ "$skip_build" != "1" ]; then
    (cd "$repo_root" && cargo build) || { echo "FAIL: cargo build failed" >&2; exit 1; }
fi

read_tags() {
    sed -n '1,10s/^#[[:space:]]*tags:[[:space:]]*//p' "$1" | head -1
}

# Splitting is done with read -ra throughout: a bare `for x in ${var}` would
# also pathname-expand, making a tag like 'rout*' match or not depending on what
# happens to sit in the working directory.
matches_filter() {
    local -a tags terms conjunct
    local term tag

    [ -z "$filter" ] && return 0
    read -ra tags <<<"$1"

    read -ra terms <<<"${filter//,/ }"
    for term in "${terms[@]}"; do
        read -ra conjunct <<<"${term//+/ }"
        [ ${#conjunct[@]} -eq 0 ] && continue
        for tag in "${conjunct[@]}"; do
            [[ " ${tags[*]} " == *" $tag "* ]] || continue 2
        done
        return 0
    done
    return 1
}

shopt -s nullglob
scripts=("$tests_dir"/*.sh)
shopt -u nullglob

# A filter selecting nothing is a normal answer; having nothing to select from is
# not, whether or not a filter is set.
if [ ${#scripts[@]} -eq 0 ]; then
    echo "FAIL: no test scripts found in $tests_dir" >&2
    exit 1
fi

passed=0
total=0

for script in "${scripts[@]}"; do
    matches_filter "$(read_tags "$script")" || continue

    name="${script##*/}"
    total=$((total + 1))
    if output="$(bash "$script" 2>&1)"; then
        echo "PASS $name"
        passed=$((passed + 1))
    else
        echo "FAIL $name"
        # shellcheck disable=SC2001  # indenting every line, not a single substitution
        [ -n "$output" ] && sed 's/^/    /' <<<"$output"
    fi
done

if [ "$total" -eq 0 ]; then
    if [ "$require_selection" = 1 ]; then
        echo "FAIL: required selection matched no tests: TAGS=$filter" >&2
        exit 1
    fi
    echo "no tests matched TAGS=$filter"
    exit 0
fi

echo "$passed/$total passed"
[ "$passed" -eq "$total" ]
