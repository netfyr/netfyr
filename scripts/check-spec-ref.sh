#!/bin/bash
# Check a governing Spec: reference or an explained No-Spec: maintenance declaration.
#
#   check-spec-ref.sh --msg FILE     one commit message (used by the commit-msg hook)
#   check-spec-ref.sh --description FILE   the pull request's declaration
#   check-spec-ref.sh --range A..B   one governing spec, with maintenance allowed
#   check-spec-ref.sh --push         individual pushed messages, against merged specs only
#
# The commit that finishes a spec also carries an 'implements <specs-repo>#N' line, which
# is what moves the spec board's card. That number and the Spec: path come from different
# places, so when the network is available they are checked against each other.
#
# Env: VERIFY_SPEC_EXISTS=1 resolves refs against the specs repo (needs network),
# NETFYR_SPECS_RAW and NETFYR_SPECS_API override the URLs resolution goes through,
# NETFYR_SPECS_REPO overrides the owner/repo the implements line must name,
# GITHUB_TOKEN is used for the API call when set.

set -uo pipefail

specs_repo="${NETFYR_SPECS_REPO:-netfyr/specs}"
specs_raw="${NETFYR_SPECS_RAW:-https://raw.githubusercontent.com/$specs_repo/main}"
specs_api="${NETFYR_SPECS_API:-https://api.github.com/repos/$specs_repo}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
top_level_specs="${NETFYR_TOP_LEVEL_SPECS_FILE:-$script_dir/top-level-specs}"

# Mirrors the specs repo layout: NNN-slug.md at the root or under an area directory.
ref_re='^([a-z0-9][a-z0-9-]*/)?[0-9]{3}-[a-z0-9-]+$'
top_level_re='^[a-z][a-z0-9-]*$'
# SpecDoc scans the whole original message with this grammar.
completion_re='(^|[^A-Za-z0-9_])implements ([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)?#[0-9]+'

status=0
last_ref=""
last_impl=""
last_kind=""

fail() { echo "FAIL: $*" >&2; status=1; }

short() { git log --no-walk --format=%h "$@" | tr '\n' ' ' | sed 's/ $//'; }

need_curl() {
    command -v curl >/dev/null || { echo "FAIL: curl not found, cannot verify spec refs" >&2; exit 1; }
}

spec_exists() {
    need_curl
    curl -sfL --max-time 20 -o /dev/null "$specs_raw/$1.md"
}

# The files a spec's pull request touched, one per line.
pr_files() {
    need_curl
    local auth=()
    [ -n "${GITHUB_TOKEN:-}" ] && auth=(-H "Authorization: Bearer $GITHUB_TOKEN")
    curl -sfL --max-time 20 -H "Accept: application/vnd.github+json" "${auth[@]}" \
        "$specs_api/pulls/$1/files" \
        | sed -n 's/.*"filename"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p'
}

# Only the commit that completes a spec carries this, so its absence is not an error.
check_implements() {
    local label="$1" ref="$2" line="$3" repo num files

    if [[ ! "$line" =~ ^implements\ ([^[:space:]#]*)#([0-9]+)$ ]]; then
        fail "$label: '$line' is not 'implements $specs_repo#<number>'"
        return
    fi
    repo="${BASH_REMATCH[1]}"
    num="${BASH_REMATCH[2]}"

    # A bare '#N' resolves against whichever repo the board is scanning, which
    # outside the specs repo silently points at an unrelated issue.
    if [ -z "$repo" ]; then
        fail "$label: 'implements #$num' needs the repository: implements $specs_repo#$num"
        return
    fi
    if [ "$repo" != "$specs_repo" ]; then
        fail "$label: implements names $repo, specs live in $specs_repo"
        return
    fi
    last_impl="$repo#$num"

    [ "${VERIFY_SPEC_EXISTS:-}" = "1" ] || return

    files="$(pr_files "$num")"
    if [ -z "$files" ]; then
        fail "$label: $specs_repo#$num has no files, so it is not a spec pull request"
        return
    fi
    grep -qxF "$ref.md" <<<"$files" \
        || fail "$label: implements $specs_repo#$num, which did not touch $ref.md"
}

# A commit that carries someone else's message, or that will be squashed into a
# commit that has already been checked, has no reference of its own to give.
exempt_subject() {
    case "$1" in
        Merge\ *|Revert\ *|fixup!\ *|squash!\ *|amend!\ *) return 0 ;;
        *) return 1 ;;
    esac
}

check_message() {
    local label="$1" raw="$2" mode="${3:-commit}" msg subject block refs count ref impl
    local maintenance_count reason board_matches top_level=0
    msg="$(git stripspace --strip-comments <<<"$raw")"
    last_ref=""
    last_impl=""
    last_kind=""

    subject="$(head -1 <<<"$msg")"
    [ -n "$subject" ] || { fail "$label: empty commit message"; return; }

    # The final paragraph, read directly rather than through git interpret-trailers:
    # the implements line carries no 'Key: value' separator, and one such line makes
    # git disown the whole block.
    block="$(awk 'BEGIN{RS=""} END{print}' <<<"$msg")"
    count="$(grep -c '^Spec:' <<<"$block")"
    maintenance_count="$(grep -c '^No-Spec:' <<<"$block")"
    impl="$(grep -iE '^implements[[:space:]]' <<<"$block")"
    board_matches="$(LC_ALL=C grep -oiE "$completion_re" <<<"$raw")"
    if [ -n "$board_matches" ] && [ "$board_matches" != "$impl" ]; then
        fail "$label: completion references must appear only as one final-paragraph implements trailer"
        return
    fi

    if [ "$maintenance_count" -gt 0 ]; then
        if [ "$maintenance_count" -ne 1 ]; then
            fail "$label: more than one No-Spec: declaration"
            return
        fi
        if [ "$count" -gt 0 ]; then
            fail "$label: use either Spec: or No-Spec:, not both"
            return
        fi
        reason="$(sed -n 's/^No-Spec:[[:space:]]*//p' <<<"$block")"
        if [[ ! "$reason" =~ [^[:space:]] ]]; then
            fail "$label: No-Spec: needs a maintenance reason"
            return
        fi
        if [[ -n "$board_matches" || -n "$impl" ]]; then
            fail "$label: No-Spec: maintenance cannot carry implements references"
            return
        fi
        last_kind=maintenance
        return
    fi

    if [ -z "$impl" ] && { [ "$mode" = merge ] || { [ "$mode" = commit ] && exempt_subject "$subject"; }; }; then return; fi

    refs="$(sed -n 's/^Spec:[[:space:]]*//p' <<<"$block")"

    if [ "$count" -eq 0 ]; then
        fail "$label: the final paragraph needs Spec: <path> or No-Spec: <maintenance reason>"
        return
    fi
    if [ "$count" -gt 1 ]; then
        fail "$label: $count Spec: trailers, a commit implements exactly one spec"
        return
    fi

    ref="$refs"
    if [[ ! "$ref" =~ $ref_re ]]; then
        if [[ "$ref" =~ $top_level_re ]] && [ -r "$top_level_specs" ] \
            && grep -qxF -- "$ref" "$top_level_specs"; then
            top_level=1
        else
            fail "$label: 'Spec: $ref' is not a feature path or a registered published top-level slug"
            return
        fi
    fi
    if [ "${VERIFY_SPEC_EXISTS:-}" = "1" ] && ! spec_exists "$ref"; then
        fail "$label: $ref.md does not exist in the specs repo"
        return
    fi

    if [ -n "$impl" ] && [ "$(wc -l <<<"$impl")" -gt 1 ]; then
        fail "$label: more than one implements trailer"
        return
    fi
    if [ -n "$impl" ] && [ "$top_level" = 1 ]; then
        fail "$label: top-level specs stay approved and cannot carry implements"
        return
    fi
    [ -n "$impl" ] && check_implements "$label" "$ref" "$impl"

    last_ref="$ref"
    last_kind=spec
}

usage() { echo "usage: ${0##*/} --msg FILE | --description FILE | --range BASE..HEAD | --push" >&2; exit 2; }

# Publication checks use the merged requirements repository; local message checks
# remain available offline.
push_only=0
if [ "${1:-}" = "--push" ]; then
    push_only=1
    export VERIFY_SPEC_EXISTS=1
    to="${PRE_COMMIT_TO_REF:-HEAD}"
    from="${PRE_COMMIT_FROM_REF:-}"
    # Direct callers may omit the previous tip; pre-commit normally supplies one.
    if [ -z "$from" ] || [[ "$from" =~ ^0+$ ]]; then
        from="$(git rev-parse --verify -q origin/HEAD || git rev-parse --verify -q origin/main)" \
            || from=""
    fi
    if [ -n "$from" ]; then set -- --range "$from..$to"; else set -- --range "$to"; fi
fi

[ $# -eq 2 ] || usage

case "$1" in
    --msg)
        [ -r "$2" ] || { echo "FAIL: cannot read commit message file '$2'" >&2; exit 1; }
        comment="$(git config --get core.commentString || git config --get core.commentChar || printf '#')"
        [ "$comment" != auto ] || comment='#'
        message="$(awk -v marker="$comment ------------------------ >8 ------------------------" '$0 == marker {exit} {print}' "$2")"
        check_message "commit message" "$message"
        ;;
    --description)
        [ -r "$2" ] || { echo "FAIL: cannot read pull request description '$2'" >&2; exit 1; }
        check_message "pull request description" "$(cat "$2")" description
        ;;
    --range)
        commits="$(git rev-list --reverse "$2")" \
            || { echo "FAIL: '$2' is not a revision range" >&2; exit 1; }
        # An empty range means nothing was checked, which must not read as approval.
        [ -n "$commits" ] || { echo "FAIL: no commits in $2" >&2; exit 1; }

        refs=()
        closers=()
        maintenance=0
        # Only spec-bearing work advances the spec's final contributing commit.
        tip=""
        while read -r sha; do
            parents="$(git show -s --format=%P "$sha")"
            mode=commit
            if [[ "$parents" == *' '* ]]; then
                mode=merge
            fi
            check_message "$(git log -1 --format='%h ("%s")' "$sha")" "$(git log -1 --format=%B "$sha")" "$mode"
            [ -n "$last_ref" ] && { refs+=("$last_ref"); tip="$sha"; }
            [ -n "$last_impl" ] && closers+=("$sha")
            [ "$last_kind" = maintenance ] && maintenance=1
        done <<<"$commits"

        # A push range can include commits inherited from newer main. PR scope
        # belongs to the base/head range supplied by CI, not the old remote tip.
        [ "$push_only" -eq 0 ] || exit "$status"

        # A spec is finished once. Two closing trailers means two commits each
        # claim the work is done, and the board acts on whichever it sees first.
        if [ ${#closers[@]} -gt 1 ]; then
            fail "${#closers[@]} commits carry an implements trailer: $(short "${closers[@]}")"
        elif [ ${#closers[@]} -eq 1 ] && [ "${closers[0]}" != "$tip" ]; then
            fail "$(short "${closers[0]}") completes the spec but is not the last commit, $(short "$tip") is"
        fi

        # One spec per pull request: reviewers read a branch against a single spec.
        if [ ${#refs[@]} -gt 1 ]; then
            distinct="$(printf '%s\n' "${refs[@]}" | sort -u)"
            [ "$(grep -c . <<<"$distinct")" -eq 1 ] \
                || fail "commits reference more than one spec: ${distinct//$'\n'/ }"
        fi
        if [ ${#refs[@]} -gt 0 ]; then
            last_ref="${refs[0]}"
            last_kind=spec
        elif [ "$maintenance" -eq 1 ]; then
            last_ref=""
            last_kind=maintenance
        else
            last_ref=""
            last_kind=""
        fi
        ;;
    *) usage ;;
esac

# Return the real path, No-Spec for maintenance, or nothing for mechanical exemptions.
if [ "$status" -eq 0 ]; then
    case "$last_kind" in
        spec) printf '%s\n' "$last_ref" ;;
        maintenance) printf 'No-Spec\n' ;;
    esac
fi

exit "$status"
