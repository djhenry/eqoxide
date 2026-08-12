#!/usr/bin/env bash
#
# check-no-local-detail.sh — fail the build if local-system detail or proprietary-derived
# content reappears in a TRACKED file. The repo is public; this guard is the durable defense
# against re-introducing the categories that a one-time scrub removed.
#
# It flags (by PATTERN, so it never itself contains a real secret value):
#   - absolute home paths            (a local-system path leak)
#   - the local container-name shape (deployment-specific infrastructure detail)
#   - an inline DB password flag     (the `-u<user> -p<pass>` credential antipattern)
#   - references to a decompiled commercial client and RE tooling
#     (ghidra / capstone / the client exe / a `decompiled/` path / lifted `FUN_xxxxxx` symbols)
#
# Run locally with:  scripts/check-no-local-detail.sh
# Exit 0 = clean, exit 1 = a forbidden pattern was found (prints file:line).
#
# WHAT THIS DOES NOT SEE (eqoxide#980): tracked files, and nothing else. PR bodies, issue bodies
# and comments are public artifacts of this repo that this script structurally cannot reach, and
# they have carried absolute home paths with this check green. `scripts/check-pr-text.sh` covers
# that surface; the two share one pattern list (`scripts/local-detail-patterns.sh`) so they cannot
# drift apart.

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

# One regex per category, defined once and shared with scripts/check-pr-text.sh. The path is
# overridable so `check-pr-text.sh --self-test` can point THIS script at an empty pattern list and
# assert that it refuses — an assertion that runs the real guard instead of re-deriving it.
# shellcheck source=scripts/local-detail-patterns.sh
patterns_file="${LOCAL_DETAIL_PATTERNS_FILE:-$(git rev-parse --show-toplevel)/scripts/local-detail-patterns.sh}"
source "$patterns_file"
# A pattern file that defines nothing at all must reach the refusal below, not crash on an unbound
# variable with an exit code indistinguishable from a real finding.
declare -p LOCAL_DETAIL_PATTERNS >/dev/null 2>&1 || LOCAL_DETAIL_PATTERNS=()
patterns=("${LOCAL_DETAIL_PATTERNS[@]:-}")

# Reach guard. This script reports only EXCEPTIONS, so with an empty pattern list it would print
# OK having looked at nothing — indistinguishable from a clean tree. Refuse that state loudly.
if [ "${#LOCAL_DETAIL_PATTERNS[@]}" -eq 0 ]; then
  echo "::error::check-no-local-detail: pattern list is EMPTY — the scan looked at nothing."
  echo "This is a guard failure, not a clean tree. Patterns were read from: ${patterns_file}"
  exit 1
fi

# Paths excluded from the scan. Every exclusion is a permanent blind spot in the one guard that is
# a PREVENTER, so the list is kept as short as the patterns allow:
#   - this script itself and the shared pattern file (each necessarily contains the patterns, and
#     the pattern file also holds the dirty samples the guards are tested against)
#   - the CI workflow (references the scripts)
# `scripts/check-pr-text.sh` is deliberately NOT excluded: its dirty samples were moved into the
# pattern file above precisely so the 300-line scanner stays inside this scan's reach.
excludes=(
  ":(exclude)scripts/check-no-local-detail.sh"
  ":(exclude)scripts/local-detail-patterns.sh"
  ":(exclude).github/workflows/test.yml"
)

status=0
for re in "${patterns[@]}"; do
  # git grep exits 1 when there are no matches; that is the success case for us.
  if hits=$(git grep -nE -e "$re" -- . "${excludes[@]}"); then
    echo "::error::forbidden pattern /$re/ found in a tracked file:"
    echo "$hits"
    echo
    status=1
  fi
done

if [ "$status" -ne 0 ]; then
  echo "check-no-local-detail: FAILED — see matches above."
  echo "Do not commit local-system detail or proprietary-derived content to this public repo."
  echo "Fixes: use ~/ or a placeholder for paths; read credentials from the environment (no"
  echo "defaults); cite the open-source EQEmu server or the data-file format, not a decompile."
  exit 1
fi

# Report the corpus, not just the (empty) exception list: "0 findings" and "0 files scanned" print
# the same word otherwise. The count is the file set `git grep` above actually searched.
scanned=$(git grep --files-with-matches -I -E -e '' -- . "${excludes[@]}" 2>/dev/null | wc -l || true)
echo "check-no-local-detail: OK — ${#patterns[@]} patterns x ${scanned} tracked text files, no matches."
echo "check-no-local-detail: reach — tracked files only. PR/issue bodies and comments are NOT"
echo "covered here; scripts/check-pr-text.sh is the scanner for that surface (eqoxide#980)."
