#!/usr/bin/env bash
#
# update-pins.sh - repin the git dependencies in Cargo.toml.
#
# Resolves the latest commit for dioxuslabs/blitz and dioxuslabs/anyrender and
# rewrites every `rev = "..."` that belongs to those repositories, including the
# commented-out lines in [patch.crates-io] so they stay usable.
#
#   ./scripts/update-pins.sh                    # both repos, default branch
#   ./scripts/update-pins.sh --check            # exit 1 if out of date, change nothing
#   ./scripts/update-pins.sh --dry-run          # show what would change
#   ./scripts/update-pins.sh --branch main      # resolve a branch other than HEAD
#   ./scripts/update-pins.sh --blitz a1b2c3d    # pin one repo to an exact rev
#   ./scripts/update-pins.sh --anyrender-branch next
#   ./scripts/update-pins.sh --no-verify        # skip the cargo fetch check
#
# Uses `git ls-remote`, not the GitHub API: no token, no rate limit.

set -euo pipefail

BLITZ_REPO="dioxuslabs/blitz"
ANYRENDER_REPO="dioxuslabs/anyrender"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST="${SCRIPT_DIR}/../Cargo.toml"

blitz_ref=""
anyrender_ref=""
blitz_pin=""
anyrender_pin=""
dry_run=0
check_only=0
verify=1

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
note() { printf '%s\n' "$*" >&2; }

usage() {
    sed -n '3,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --branch)           blitz_ref="${2:?--branch needs a value}"
                            anyrender_ref="$2"; shift 2 ;;
        --blitz-branch)     blitz_ref="${2:?--blitz-branch needs a value}"; shift 2 ;;
        --anyrender-branch) anyrender_ref="${2:?--anyrender-branch needs a value}"; shift 2 ;;
        --blitz)            blitz_pin="${2:?--blitz needs a value}"; shift 2 ;;
        --anyrender)        anyrender_pin="${2:?--anyrender needs a value}"; shift 2 ;;
        --dry-run|-n)       dry_run=1; shift ;;
        --check)            check_only=1; shift ;;
        --no-verify)        verify=0; shift ;;
        -h|--help)          usage 0 ;;
        *)                  note "unknown option: $1"; usage 2 ;;
    esac
done

command -v git >/dev/null 2>&1 || die "git is required"
[[ -f "$MANIFEST" ]] || die "no Cargo.toml at $MANIFEST"

# Ask the remote for a ref's commit. Empty ref means the default branch.
resolve_ref() {
    local repo="$1" ref="${2:-HEAD}" out sha
    out="$(git ls-remote "https://github.com/${repo}" "$ref" 2>/dev/null)" \
        || die "could not reach https://github.com/${repo}"
    # ls-remote prints "<sha>\t<refname>"; HEAD is the first line when it matches.
    sha="$(printf '%s\n' "$out" | awk 'NR==1 {print $1}')"
    [[ -n "$sha" ]] || die "ref '${ref}' not found in ${repo}"
    printf '%s\n' "$sha"
}

# The rev currently pinned for a repo, read back from the manifest.
current_rev() {
    local repo="$1"
    grep -oE "github\.com/${repo}\", rev = \"[0-9a-fA-F]+\"" "$MANIFEST" \
        | head -n1 | grep -oE '[0-9a-fA-F]{7,40}"$' | tr -d '"'
}

# Rewrite every rev belonging to one repo. Matching on the repo URL plus its
# closing quote keeps `dioxuslabs/blitz` from also matching `dioxuslabs/blitzy`,
# and leaves the [package] repository field alone since it has no rev.
repin() {
    local repo="$1" sha="$2" file="$3" tmp
    tmp="$(mktemp)"
    sed -E "\%github\.com/${repo}\"%{s/rev = \"[0-9a-fA-F]+\"/rev = \"${sha}\"/}" \
        "$file" > "$tmp"
    mv "$tmp" "$file"
}

count_pins() {
    grep -cE "github\.com/$1\", rev = \"[0-9a-fA-F]+\"" "$MANIFEST" || true
}

short() { printf '%s' "${1:0:7}"; }

# --- resolve -----------------------------------------------------------------

if [[ -n "$blitz_pin" ]]; then
    blitz_sha="$blitz_pin"
else
    note "resolving ${BLITZ_REPO} ${blitz_ref:-HEAD}..."
    blitz_sha="$(resolve_ref "$BLITZ_REPO" "$blitz_ref")"
fi

if [[ -n "$anyrender_pin" ]]; then
    anyrender_sha="$anyrender_pin"
else
    note "resolving ${ANYRENDER_REPO} ${anyrender_ref:-HEAD}..."
    anyrender_sha="$(resolve_ref "$ANYRENDER_REPO" "$anyrender_ref")"
fi

blitz_now="$(current_rev "$BLITZ_REPO" || true)"
anyrender_now="$(current_rev "$ANYRENDER_REPO" || true)"

changed=0
report() {
    local name="$1" now="$2" next="$3"
    if [[ "$now" == "$next" ]]; then
        printf '  %-10s %s (unchanged)\n' "$name" "$(short "$now")"
    else
        printf '  %-10s %s -> %s\n' "$name" "$(short "${now:-none}")" "$(short "$next")"
        changed=1
    fi
}

printf 'pins:\n'
report blitz     "$blitz_now"     "$blitz_sha"
report anyrender "$anyrender_now" "$anyrender_sha"

if [[ $changed -eq 0 ]]; then
    printf 'already up to date\n'
    exit 0
fi

if [[ $check_only -eq 1 ]]; then
    printf 'out of date (run without --check to update)\n'
    exit 1
fi

if [[ $dry_run -eq 1 ]]; then
    printf 'dry run, nothing written\n'
    exit 0
fi

# --- write -------------------------------------------------------------------

backup="${MANIFEST}.bak"
cp "$MANIFEST" "$backup"

restore() {
    if [[ -f "$backup" ]]; then
        mv "$backup" "$MANIFEST"
        note "restored $MANIFEST"
    fi
}

repin "$BLITZ_REPO"     "$blitz_sha"     "$MANIFEST"
repin "$ANYRENDER_REPO" "$anyrender_sha" "$MANIFEST"

# A silent no-op here almost always means the manifest was reformatted and the
# patterns stopped matching, which would otherwise look like a successful update.
if [[ "$(count_pins "$BLITZ_REPO")" -eq 0 ]]; then
    restore
    die "no blitz revs were rewritten — has Cargo.toml been reformatted?"
fi
if [[ "$(current_rev "$BLITZ_REPO")" != "$blitz_sha" ]]; then
    restore
    die "blitz rev did not take effect"
fi

printf 'updated %s\n' "$MANIFEST"

# --- verify ------------------------------------------------------------------

if [[ $verify -eq 1 ]] && command -v cargo >/dev/null 2>&1; then
    printf 'fetching to confirm the revs resolve...\n'
    if cargo fetch --manifest-path "$MANIFEST" >/dev/null; then
        printf 'ok — Cargo.lock updated\n'
    else
        note ""
        note "cargo fetch failed with the new pins."
        note "if this is a version mismatch on the [patch.crates-io] entries,"
        note "bump the anyrender/peniko requirements in [dependencies] to match"
        note "what the new rev declares, then re-run cargo fetch."
        restore
        exit 1
    fi
else
    [[ $verify -eq 1 ]] && note "cargo not found, skipping verification"
fi

rm -f "$backup"

printf '\nnext: make clean && make\n'
