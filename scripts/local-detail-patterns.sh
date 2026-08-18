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
# NO ENTRY ENCODES A PRIVATE OR LOCAL VALUE, which is what makes this file safe to keep in a public
# repo. That is checked entry by entry, not assumed — and stated carefully, because "matches a shape,
# not a value" is too glib: every regex here has SOME literal in it. What is true of all eight is
# that each literal is either a universal convention (`/home/`, `decompiled/`, the `-u`/`-p` flag
# pair) or the public name of a tool, project or binary (`eqemu`, `ghidra`, `capstone`, `eqgame.exe`,
# and `FUN_`, which is a decompiler's default symbol prefix). Not one of the eight is a host name, an
# account, a machine-specific path, or a secret. Anyone can write down all eight from public sources
# without knowing anything about this deployment.
#
# A CLASS THAT IS DELIBERATELY NOT COVERED *HERE*: deployment HOST NAMES (#995). A host name has no
# shape — any word can be one — so covering the class by pattern means enumerating the literal names
# in use. That was tried in PR #989 and REFUSED, for a reason worth keeping written down: after the
# one tracked occurrence is scrubbed, a literal here would be the ONLY occurrence of the class in
# the tree, in a file whose name announces exactly what it is, at HEAD, indexed by code search. The
# leak-detection guard would become the only tracked leak. Neither of the two arguments for it
# reaches far enough: private-network reachability is a property of the network and can change with
# nobody editing this file, and "already in the history" was a decision about the COST of rewriting,
# not a finding that the value is fine to publish.
#
# The owner's decision on #995 was to cover the class with a STRUCTURAL heuristic that names
# nothing, and it lives in `scripts/check-host-shape.py` rather than in this array — it cannot be
# expressed as a regex, because the discriminator is not the token, it is the token's shape, its
# proximity to infrastructure vocabulary, and its absence from an English dictionary. Nothing was
# added to this file for #995 and nothing should be: the moment a name is written here, the leak has
# moved instead of closing. That script is a WARNING, not a gate, on measured grounds — see its
# header and #995.
#
# THE PATTERN SET IS AN ALLOWLIST OF LEAK SHAPES SOMEONE ALREADY THOUGHT OF. A green run from either
# scanner means "no KNOWN shape matched", never "no local detail is present". #995 is the standing
# proof: a tracked comment named deployment infrastructure, the guard exited 0 on it for months, and
# the comment was scrubbed by #989. The class now has a detector — `scripts/check-host-shape.py` —
# but that does not upgrade this sentence, because that detector is a warning with a narrow, stated
# reach and no literal here matches a host name. When a new class IS coverable by shape, the fix is
# both halves at once — scrub the instance and add the pattern — or it stays uncovered with nothing
# recording that it ever was.
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
)
