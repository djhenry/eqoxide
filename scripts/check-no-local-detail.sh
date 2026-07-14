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

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

# One regex per category. Kept generic on purpose: we detect the *shape* of a leaked credential,
# never a literal value, so this script is safe to keep in a public repo.
patterns=(
  '/home/[a-z]'                 # absolute home-directory path (use ~/ or a placeholder instead)
  'eqemu_[a-z]+_[0-9]'          # local container name (parameterise it)
  '-u[a-z]+ +-p[A-Za-z0-9]'     # inline DB user+password (read the password from the environment)
  'ghidra'                      # decompilation / RE tooling
  'capstone'                    # disassembly tooling
  'eqgame\.exe'                 # the decompiled commercial client binary
  'decompiled/'                 # a path into decompiled output
  'FUN_[0-9a-fA-F]{6}'          # internal symbol name lifted from the binary
)

# Paths excluded from the scan:
#   - this script itself (it necessarily contains the patterns it searches for)
#   - the CI workflow (references the script)
#
# TEMPORARY exclusions — remove once the concurrent "remove quest oracle" PR lands:
#   tools/quest_finder.py and docs/autonomous-play.md still carry the old detail and are being
#   deleted / scrubbed in that separate PR. They are excluded here only so this guard can be
#   introduced without blocking on that PR's merge. DELETE these two lines once that PR is on main
#   (at which point quest_finder.py is gone and autonomous-play.md is clean).
excludes=(
  ":(exclude)scripts/check-no-local-detail.sh"
  ":(exclude).github/workflows/*"
  ":(exclude)tools/quest_finder.py"
  ":(exclude)docs/autonomous-play.md"
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

echo "check-no-local-detail: OK — no forbidden patterns in tracked files."
