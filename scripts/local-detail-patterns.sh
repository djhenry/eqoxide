#!/usr/bin/env bash
#
# local-detail-patterns.sh — the ONE definition of the "local-system detail / proprietary-derived
# content" pattern set. Sourced (never executed) by both scanners:
#
#   scripts/check-no-local-detail.sh   scans TRACKED FILES     (a preventer: runs before merge)
#   scripts/check-pr-text.sh           scans a PR's BODY+COMMENTS (a detector: text already public)
#
# It exists so the two cannot drift. eqoxide#980 is exactly the failure of having one scanner and
# two surfaces; having two scanners with two hand-copied pattern lists would be the next version of
# the same bug. Both scanners are excluded from the tracked-file scan for the same reason this file
# is: they necessarily contain the patterns they search for.
#
# Kept generic on purpose: every entry matches the *shape* of a leak, never a literal secret value,
# so this file is safe to keep in a public repo.

# shellcheck disable=SC2034  # consumed by the sourcing script, not by this file
LOCAL_DETAIL_PATTERNS=(
  '/home/[a-z]'                 # absolute home-directory path (use ~/ or a placeholder instead)
  'eqemu_[a-z]+_[0-9]'          # local container name (parameterise it)
  '-u[a-z]+ +-p[A-Za-z0-9]'     # inline DB user+password (read the password from the environment)
  'ghidra'                      # decompilation / RE tooling
  'capstone'                    # disassembly tooling
  'eqgame\.exe'                 # the decompiled commercial client binary
  'decompiled/'                 # a path into decompiled output
  'FUN_[0-9a-fA-F]{6}'          # internal symbol name lifted from the binary
)
