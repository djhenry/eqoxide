#!/usr/bin/env bash
#
# local-detail-patterns.sh — the ONE definition of the "local-system detail / proprietary-derived
# content" pattern set, and of the deliberately-dirty sample per pattern that the scanners' guards
# are tested against. Sourced (never executed) by both scanners:
#
#   scripts/check-no-local-detail.sh   scans TRACKED FILES     (a preventer: runs before merge)
#   scripts/check-pr-text.sh           scans a PR's BODY+COMMENTS (a detector: text already public)
#
# It exists so the two cannot drift. eqoxide#980 is exactly the failure of having one scanner and
# two surfaces; having two scanners with two hand-copied pattern lists would be the next version of
# the same bug.
#
# THIS FILE IS THE ONLY ONE EXCLUDED FROM THE TRACKED-FILE SCAN. The dirty samples live here rather
# than in `check-pr-text.sh` for exactly that reason: an excluded file is a permanent blind spot in
# the one guard that is a preventer, so the blind spot is kept to this one file instead of also
# covering a 300-line script. Both scanners are scanned in full.
#
# Kept generic where a shape exists: most entries match the *shape* of a leak rather than a value.
# NO CREDENTIAL OR SECRET IS EVER WRITTEN HERE. One class is an exception to the shape rule, and it
# is worth being precise about rather than glossing: a deployment HOST NAME has no shape — any word
# can be a host name — so covering that class means enumerating the names this project actually
# uses. That is a literal in a public file, accepted for two reasons: the name is on a private
# network with no external exposure, and the pre-scrub occurrence it was added for (#995) is already
# permanent in this repo's history, which the standing decision is deliberately NOT to rewrite. So
# the enumeration adds no exposure that does not already exist, and buys forward coverage.
#
# THE PATTERN SET IS AN ALLOWLIST OF LEAK SHAPES SOMEONE ALREADY THOUGHT OF. A green run from either
# scanner means "no KNOWN shape matched", never "no local detail is present". #995 is the proof: a
# tracked comment named deployment infrastructure and the guard exited 0 for months, because host
# names were simply not in this list. When a new class is found, the fix is BOTH halves — scrub the
# instance and add the pattern — in one change, or the class stays uncovered with nothing recording
# that it ever was.
#
# Both scanners read this path from `${LOCAL_DETAIL_PATTERNS_FILE:-<repo>/scripts/local-detail-
# patterns.sh}`. That override is not a convenience: it is how `check-pr-text.sh --self-test`
# points the REAL scanners at an empty pattern list and asserts that they refuse to run, rather
# than re-deriving the refusal inline where the assertion could not fail.

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
  '\b[Jj]imbo\b'                # deployment host name (#995) -- see the allowlist note above
)

# One deliberately-dirty sample per pattern, in the SAME ORDER, invented here rather than quoted
# from any real leak. `check-pr-text.sh --self-test` requires this array to be the same length as
# the one above and requires every entry to be flagged, so adding a pattern without a sample fails
# the guard loudly instead of silently shrinking its coverage.
# shellcheck disable=SC2034  # consumed by the sourcing script, not by this file
LOCAL_DETAIL_DIRTY_SAMPLES=(
  'ran `git -C /home/someuser/checkout status`'
  'podman exec eqemu_mariadb_1 sh'
  'mysql -uroot -pHunter2 eqemu'
  'opened it in ghidra'
  'disassembled with capstone'
  'offsets read from eqgame.exe'
  'see decompiled/output.c'
  'the routine at FUN_004a1b2c'
  'restarted the zone launcher on jimbo'
)
