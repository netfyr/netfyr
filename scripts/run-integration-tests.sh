#!/usr/bin/env bash
set -euo pipefail

spec="${1:-}"

if [ -n "$spec" ]; then
    scripts=$(ls tests/${spec}-*.sh 2>/dev/null) || true
else
    scripts=$(ls tests/[0-9]*.sh 2>/dev/null) || true
fi

if [ -z "$scripts" ]; then
    echo "No integration test scripts found in tests/[0-9]*.sh"
    exit 0
fi

jobs=${JOBS:-$(( $(nproc 2>/dev/null || echo 4) * 2 ))}

cleanup() {
    trap - INT TERM EXIT
    printf '\r\033[K' 2>/dev/null || true
    kill $(jobs -p) 2>/dev/null || true
    wait 2>/dev/null || true
    rm -rf "$logdir" 2>/dev/null || true
    exit 130
}
trap cleanup INT TERM

logdir=$(mktemp -d)
statusdir="$logdir/status"
mkdir -p "$statusdir"

total=0
for s in $scripts; do total=$((total + 1)); done

echo "Running $total tests ($jobs parallel)..."
echo

use_status=false
green=""
red=""
reset=""
if [ -t 1 ]; then
    use_status=true
    green=$'\033[32m'
    red=$'\033[31m'
    reset=$'\033[0m'
fi

if $use_status; then
    (
        declare -A printed
        cols=$(tput cols 2>/dev/null || echo 80)

        print_results() {
            for f in "$statusdir"/pass.* "$statusdir"/fail.*; do
                [ -f "$f" ] || continue
                bname="${f##*/}"
                [ "${printed[$bname]:-}" ] && continue
                printed[$bname]=1
                printf '\r\033[K'
                test_name="${bname#pass.}"
                test_name="${test_name#fail.}"
                case "$bname" in
                    pass.*) echo "${green}PASS${reset}: tests/$test_name" ;;
                    fail.*) echo "${red}FAIL${reset}: tests/$test_name"
                            sed 's/^/  │ /' "$logdir/$test_name.log" 2>/dev/null || true ;;
                esac
            done
        }

        while [ ! -f "$statusdir/.done" ]; do
            print_results

            p=$(find "$statusdir" -maxdepth 1 -name 'pass.*' 2>/dev/null | wc -l)
            f=$(find "$statusdir" -maxdepth 1 -name 'fail.*' 2>/dev/null | wc -l)
            r_names=$(find "$statusdir" -maxdepth 1 -name 'running.*' 2>/dev/null \
                | sed 's/.*running\.//; s/\.sh$//' | sort | paste -sd ', ' -)
            r_count=$(find "$statusdir" -maxdepth 1 -name 'running.*' 2>/dev/null | wc -l)
            done_count=$((p + f))

            if [ "$r_count" -gt 0 ]; then
                status="[$done_count/$total] $p passed, $f failed | running: $r_names"
                if [ ${#status} -gt "$cols" ]; then
                    status="[$done_count/$total] $p passed, $f failed | $r_count running"
                fi
                printf '\r\033[K%s' "$status"
            fi

            sleep 0.3
        done

        print_results
        printf '\r\033[K'
    ) &
    monitor_pid=$!
fi

running=0
test_pids=""
for script in $scripts; do
    name=$(basename "$script")
    (
        set +e
        touch "$statusdir/running.$name"
        bash "$script" > "$logdir/$name.log" 2>&1
        rc=$?
        rm -f "$statusdir/running.$name"
        if [ "$rc" -eq 0 ]; then
            touch "$statusdir/pass.$name"
        else
            touch "$statusdir/fail.$name"
        fi
    ) &
    test_pids="$test_pids $!"
    running=$((running + 1))
    if [ "$running" -ge "$jobs" ]; then
        wait -n 2>/dev/null || true
        running=$((running - 1))
    fi
done

for pid in $test_pids; do
    wait "$pid" 2>/dev/null || true
done

if $use_status; then
    touch "$statusdir/.done"
    wait "$monitor_pid" 2>/dev/null || true
fi

if ! $use_status; then
    for f in "$statusdir"/pass.* "$statusdir"/fail.*; do
        [ -f "$f" ] || continue
        bname="${f##*/}"
        test_name="${bname#pass.}"
        test_name="${test_name#fail.}"
        case "$bname" in
            pass.*) echo "PASS: tests/$test_name" ;;
            fail.*) echo "FAIL: tests/$test_name"
                    sed 's/^/  │ /' "$logdir/$test_name.log" 2>/dev/null || true ;;
        esac
    done
fi

echo
failed=$(find "$statusdir" -maxdepth 1 -name 'fail.*' 2>/dev/null | wc -l)
rm -rf "$logdir"

if [ "$failed" -gt 0 ]; then
    echo "One or more integration tests failed."
    exit 1
else
    echo "All $total integration tests passed."
fi
