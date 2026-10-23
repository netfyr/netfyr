#!/bin/bash
# tags: meta policy
# The Spec: trailer check: format, placement, exemptions, and the one-spec-per-branch
# rule that the range mode enforces.

set -uo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
checker="$repo_root/scripts/check-spec-ref.sh"
[ -x "$checker" ] || { echo "FAIL: $checker not found or not executable" >&2; exit 1; }
command -v curl >/dev/null || { echo "FAIL: curl not available" >&2; exit 1; }

tmp="$(mktemp -d)" || { echo "FAIL: mktemp -d failed" >&2; exit 1; }
trap 'rm -rf "$tmp"' EXIT

status=0

msg_check() {
    local want="$1" desc="$2" msg="$3"
    printf '%s\n' "$msg" >"$tmp/msg"
    if bash "$checker" --msg "$tmp/msg" >/dev/null 2>&1; then
        [ "$want" = accept ] || { echo "FAIL: $desc: accepted, expected reject" >&2; status=1; }
    else
        [ "$want" = reject ] || { echo "FAIL: $desc: rejected, expected accept" >&2; status=1; }
    fi
}

msg_check accept "an area-qualified ref" \
    "$(printf 'Do a thing\n\nBecause of a reason.\n\nSpec: core/002-schema-validation')"
msg_check accept "a spec at the repo root" "$(printf 'Do a thing\n\nSpec: 000-project-setup')"
msg_check accept "other trailers alongside" \
    "$(printf 'Do a thing\n\nSpec: core/003-policies\nSigned-off-by: A <a@b.c>')"
msg_check accept "a comment block appended by git" \
    "$(printf 'Do a thing\n\nSpec: core/003-policies\n# Please enter the commit message\n# On branch main')"

msg_check reject "no trailer at all" "$(printf 'Do a thing\n\nBecause of a reason.')"
msg_check reject "an empty message" ""
msg_check reject "a trailer stranded above the last paragraph" \
    "$(printf 'Do a thing\n\nSpec: core/003-policies\n\nOne more thought.')"
msg_check reject "two specs in one commit" \
    "$(printf 'Do a thing\n\nSpec: core/003-policies\nSpec: core/002-schema-validation')"
msg_check reject "a bare number" "$(printf 'Do a thing\n\nSpec: 003')"
msg_check reject "a bare slug" "$(printf 'Do a thing\n\nSpec: policies')"
msg_check reject "a two-digit number" "$(printf 'Do a thing\n\nSpec: core/03-policies')"
msg_check reject "the .md suffix" "$(printf 'Do a thing\n\nSpec: core/003-policies.md')"
msg_check reject "an uppercase ref" "$(printf 'Do a thing\n\nSpec: Core/003-Policies')"
msg_check reject "a URL instead of a path" \
    "$(printf 'Do a thing\n\nSpec: https://github.com/netfyr/specs/blob/main/core/003-policies.md')"
msg_check reject "the legacy Story: trailer" "$(printf 'Do a thing\n\nStory: 003-policies')"
msg_check accept "a registered top-level spec" "$(printf 'Clarify a principle\n\nSpec: netfyr-philosophy')"
msg_check reject "an unregistered top-level slug" "$(printf 'Do a thing\n\nSpec: unrelated-principle')"
msg_check reject "completion on top-level requirements" \
    "$(printf 'Clarify a principle\n\nSpec: netfyr-philosophy\nimplements netfyr/specs#12')"

msg_check accept "a merge commit" "$(printf 'Merge branch feature into main')"
msg_check accept "a revert" "$(printf 'Revert "Do a thing"\n\nThis reverts commit abc123.')"
msg_check accept "a fixup awaiting squash" "$(printf 'fixup! Do a thing')"

printf 'Merge a maintenance change\n' >"$tmp/description"
if bash "$checker" --description "$tmp/description" >/dev/null 2>&1; then
    echo "FAIL: an exempt-looking description bypassed its required reference" >&2
    status=1
fi
printf 'Merge a maintenance change\n\nSpec: core/003-policies\n' >"$tmp/description"
bash "$checker" --description "$tmp/description" >/dev/null 2>&1 \
    || { echo "FAIL: rejected an exempt-looking description with its reference" >&2; status=1; }

msg_check accept "an implements line on the completing commit" \
    "$(printf 'Do a thing\n\nSpec: core/003-policies\nimplements netfyr/specs#12')"
msg_check reject "an implements line without the repository" \
    "$(printf 'Do a thing\n\nSpec: core/003-policies\nimplements #12')"
msg_check reject "an implements line naming another repository" \
    "$(printf 'Do a thing\n\nSpec: core/003-policies\nimplements netfyr/netfyr#12')"
msg_check reject "an implements line with no number" \
    "$(printf 'Do a thing\n\nSpec: core/003-policies\nimplements netfyr/specs')"
msg_check reject "a capitalised implements line the board would ignore" \
    "$(printf 'Do a thing\n\nSpec: core/003-policies\nImplements netfyr/specs#12')"
msg_check reject "two completion lines in one message" \
    "$(printf 'Do a thing\n\nSpec: core/003-policies\nimplements netfyr/specs#12\nimplements netfyr/specs#44')"
msg_check accept "implements prose above the trailer block" \
    "$(printf 'Do a thing\n\nimplements whatever it feels like\n\nSpec: core/003-policies')"

specs="$tmp/specs"
mkdir -p "$specs/core"
: >"$specs/core/003-policies.md"
: >"$specs/core/002-schema-validation.md"
: >"$specs/netfyr-philosophy.md"
export NETFYR_SPECS_RAW="file://$specs"
VERIFY_SPEC_EXISTS=1 bash "$checker" --msg <(printf 'Do a thing\n\nSpec: core/003-policies\n') >/dev/null 2>&1 \
    || { echo "FAIL: rejected a ref that exists in the specs repo" >&2; status=1; }
if VERIFY_SPEC_EXISTS=1 bash "$checker" --msg <(printf 'Do a thing\n\nSpec: core/404-ghost\n') >/dev/null 2>&1; then
    echo "FAIL: accepted a ref with no spec behind it" >&2
    status=1
fi
VERIFY_SPEC_EXISTS=1 bash "$checker" --msg <(printf 'Clarify a principle\n\nSpec: netfyr-philosophy\n') >/dev/null 2>&1 \
    || { echo "FAIL: rejected a registered published top-level spec" >&2; status=1; }
if VERIFY_SPEC_EXISTS=1 bash "$checker" --msg <(printf 'Clarify a principle\n\nSpec: netfyr-identity\n') >/dev/null 2>&1; then
    echo "FAIL: accepted a registered top-level spec whose published file is missing" >&2
    status=1
fi

# The pull request a spec arrived in has to be the one the implements line names,
# so a stale or copied number is caught rather than closing the wrong card.
api="$tmp/api"
mkdir -p "$api/pulls/12" "$api/pulls/44" "$api/pulls/77"
printf '[{"sha": "abc", "filename": "core/003-policies.md", "status": "added"}]\n' >"$api/pulls/12/files"
printf '[{"filename": "core/002-schema-validation.md"}]\n' >"$api/pulls/44/files"
printf '[]\n' >"$api/pulls/77/files"
export NETFYR_SPECS_API="file://$api"

verified() {
    { VERIFY_SPEC_EXISTS=1 bash "$checker" --msg <(printf '%s\n' "$1") >/dev/null; } 2>&1
}

out="$(verified "$(printf 'Do a thing\n\nSpec: core/003-policies\nimplements netfyr/specs#12')")"
[ -z "$out" ] || { echo "FAIL: rejected a matching pull request number: $out" >&2; status=1; }

out="$(verified "$(printf 'Do a thing\n\nSpec: core/003-policies\nimplements netfyr/specs#44')")"
grep -q 'did not touch core/003-policies.md' <<<"$out" \
    || { echo "FAIL: accepted a number belonging to another spec: $out" >&2; status=1; }

out="$(verified "$(printf 'Do a thing\n\nSpec: core/003-policies\nimplements netfyr/specs#77')")"
grep -q 'not a spec pull request' <<<"$out" \
    || { echo "FAIL: accepted a pull request that touched no files: $out" >&2; status=1; }

out="$(verified "$(printf 'Do a thing\n\nSpec: core/003-policies\nimplements netfyr/specs#999')")"
[ -n "$out" ] || { echo "FAIL: accepted a pull request number that does not resolve" >&2; status=1; }

# Without the network flag the number is taken on trust, so committing works offline.
bash "$checker" --msg <(printf 'Do a thing\n\nSpec: core/003-policies\nimplements netfyr/specs#44\n') \
    >/dev/null 2>&1 \
    || { echo "FAIL: the offline check resolved a pull request number" >&2; status=1; }

unset NETFYR_SPECS_API NETFYR_SPECS_RAW

work="$tmp/repo"
git init -q -b main "$work" || { echo "FAIL: git init failed" >&2; exit 1; }
commit() {
    : >"$work/$RANDOM$RANDOM"
    git -C "$work" add -A
    git -C "$work" -c user.name=t -c user.email=t@t -c commit.gpgsign=false \
        commit -q --no-verify -m "$1"
}

commit "$(printf 'Base\n\nSpec: core/003-policies')"
base="$(git -C "$work" rev-parse HEAD)"
commit "$(printf 'Second\n\nSpec: core/003-policies')"
(cd "$work" && bash "$checker" --range "$base..HEAD") >/dev/null 2>&1 \
    || { echo "FAIL: rejected a range where every commit names the same spec" >&2; status=1; }

# A branch may finish its spec or not, so zero closing trailers is fine and one is
# fine; two commits both claiming the spec is done is not.
closer="$(git -C "$work" rev-parse HEAD)"
commit "$(printf 'Close it\n\nSpec: core/003-policies\nimplements netfyr/specs#12')"
(cd "$work" && bash "$checker" --range "$closer..HEAD") >/dev/null 2>&1 \
    || { echo "FAIL: rejected a range whose last commit completes the spec" >&2; status=1; }

range_out() { { (cd "$work" && bash "$checker" --range "$1") >/dev/null; } 2>&1; }

commit "$(printf 'Close it again\n\nSpec: core/003-policies\nimplements netfyr/specs#12')"
out="$(range_out "$closer..HEAD")"
grep -q 'carry an implements trailer' <<<"$out" \
    || { echo "FAIL: accepted two commits completing one spec: $out" >&2; status=1; }

# Work continuing past the closer means the spec was not finished where it said.
git -C "$work" reset -q --hard HEAD~1
commit "$(printf 'More work\n\nSpec: core/003-policies')"
out="$(range_out "$closer..HEAD")"
grep -q 'is not the last commit' <<<"$out" \
    || { echo "FAIL: accepted a closer with a later commit after it: $out" >&2; status=1; }

# An exempt commit after the closer is not work, so it does not reopen the spec.
git -C "$work" reset -q --hard HEAD~1
commit "$(printf 'fixup! Close it')"
out="$(range_out "$closer..HEAD")"
[ -z "$out" ] || { echo "FAIL: a trailing fixup unseated the closer: $out" >&2; status=1; }
git -C "$work" reset -q --hard HEAD~1

commit "$(printf 'Third\n\nSpec: core/002-schema-validation')"
if (cd "$work" && bash "$checker" --range "$base..HEAD") >/dev/null 2>&1; then
    echo "FAIL: accepted a range spanning two specs" >&2
    status=1
fi

commit "$(printf 'Fourth\n\nno trailer here')"
if (cd "$work" && bash "$checker" --range "HEAD~1..HEAD") >/dev/null 2>&1; then
    echo "FAIL: accepted a range containing an unreferenced commit" >&2
    status=1
fi

if (cd "$work" && bash "$checker" --range "not-a-range") >/dev/null 2>&1; then
    echo "FAIL: accepted a bogus revision range" >&2
    status=1
fi

if (cd "$work" && bash "$checker" --range "HEAD..HEAD") >/dev/null 2>&1; then
    echo "FAIL: an empty range passed as though it had been checked" >&2
    status=1
fi

# CI compares the branch's reference against the one in the pull request description.
out="$(bash "$checker" --msg <(printf 'Do a thing\n\nSpec: core/003-policies\n') 2>/dev/null)"
[ "$out" = "core/003-policies" ] \
    || { echo "FAIL: --msg printed '$out', expected the accepted reference" >&2; status=1; }

out="$(cd "$work" && bash "$checker" --range "$base..$closer" 2>/dev/null)"
[ "$out" = "core/003-policies" ] \
    || { echo "FAIL: --range printed '$out', expected one reference" >&2; status=1; }

out="$(bash "$checker" --msg <(printf 'Merge branch feature into main\n') 2>/dev/null)"
[ -z "$out" ] || { echo "FAIL: an exempt commit printed a reference: '$out'" >&2; status=1; }

# Pushing is where an unapproved spec has to be caught: --push resolves every
# reference against the specs repo whether or not the caller asked it to.
export NETFYR_SPECS_RAW="file://$specs" NETFYR_SPECS_API="file://$api"
push_out() {
    { (cd "$work" && PRE_COMMIT_FROM_REF="$1" PRE_COMMIT_TO_REF="$2" bash "$checker" --push) \
        >/dev/null; } 2>&1
}

approved="$(git -C "$work" rev-parse HEAD)"
commit "$(printf 'Work on a merged spec\n\nSpec: core/003-policies')"
out="$(push_out "$approved" HEAD)"
[ -z "$out" ] || { echo "FAIL: blocked a push for an approved spec: $out" >&2; status=1; }

unapproved="$(git -C "$work" rev-parse HEAD)"
commit "$(printf 'Work on a draft spec\n\nSpec: core/404-ghost')"
out="$(push_out "$unapproved" HEAD)"
grep -q 'does not exist in the specs repo' <<<"$out" \
    || { echo "FAIL: let through a spec that was never merged: $out" >&2; status=1; }

# A new branch arrives with an all-zero from-ref and no remote to fall back on,
# so everything on it gets checked rather than nothing.
out="$(push_out 0000000000000000000000000000000000000000 HEAD)"
grep -q 'does not exist in the specs repo' <<<"$out" \
    || { echo "FAIL: an unseen branch skipped the approval check: $out" >&2; status=1; }

unset NETFYR_SPECS_RAW NETFYR_SPECS_API

exit "$status"
