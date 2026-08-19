#!/usr/bin/env bash
#
# check-pr-text.sh — scan the text a PR or issue publishes (title, body, every comment, and a pull
# request's commit messages) on TWO independent passes:
#
#   1. the local-system / proprietary-derived patterns that `check-no-local-detail.sh` keeps out of
#      tracked files but structurally cannot see on this surface (eqoxide#980);
#   2. a NEGATED CLOSING KEYWORD — "does not close" and friends immediately before an issue number,
#      which GitHub links and closes anyway because it does not read the negation (eqoxide#1041).
#
# Both passes always run and both classify every item. The exit code is NOT their plain union: the
# local-detail pass gates on any finding, while the negated-close pass gates only on the surfaces
# GitHub actually links from (a pull request's body, a commit message). A finding on a title, a
# comment or an issue body is reported, counted and printed as a WARNING and does not change the
# exit code, because it cannot close anything from where it sits. See negclose_surface_gates, where
# the policy and the measurements behind it live.
#
# WHY THIS EXISTS (eqoxide#980). `check-no-local-detail.sh` scans TRACKED FILES ONLY. PR bodies,
# issue bodies and comments are public artifacts of this public repo that it structurally cannot
# see — and two PRs in the #975 wave shipped absolute home paths in their bodies with the
# `no-local-detail` check GREEN the whole time. The cause is not carelessness: every fleet agent is
# asked to prove it did not edit the shared checkout, and the natural way to write that proof is to
# paste the command it ran, path and all. The more rigorous the provenance, the likelier the leak.
#
# THIS IS A DETECTOR, NOT A PREVENTER, and the script says so in its own output on every run. By
# the time any check can read a PR body, GitHub has already published it — to the web, to the API,
# to every watcher's email. Editing the text afterwards limits further exposure; it does not
# un-publish it, and GitHub keeps the edit history. A green run here means "the current text is
# clean", never "nothing was ever exposed".
#
#   usage:  scripts/check-pr-text.sh <pr-or-issue-number> [--repo <owner>/<name>]
#           scripts/check-pr-text.sh --commits <rev-range>
#           scripts/check-pr-text.sh --self-test
#
#   exit 0 = every item in the corpus was classified, and nothing matched that this run GATES on:
#            no local-detail pattern anywhere, and no negated closing keyword on a surface GitHub
#            links from. It does NOT mean nothing matched — a negated closing keyword on a title, a
#            comment or an issue body warns and still exits 0. READ THE SUMMARY LINE, not the exit
#            code: it prints `flagged N (M on a surface that can actually close)`, and the final
#            line says CLEAN or NOT CLEAN accordingly.
#   exit 1 = a local-detail pattern matched, or a negated closing keyword matched on a linking
#            surface, or the corpus could not be established (see REACH below)
#
# REACH — what this run did and did not look at. Read this before quoting a green result.
#   COVERED: the CURRENT text of, for one number: the title, the body, every issue-style comment,
#            and (for a PR) every review body, every inline review comment, and the message of
#            every commit in the pull request. The commit surface was added for eqoxide#1041: issue
#            314 was false-closed by a commit message and by nothing else, so a check that read only
#            bodies and comments watched that one go past. It comes from the API, not from git,
#            because every `actions/checkout` in this repo's workflow runs at the default
#            fetch-depth of 1 — the runner does not have the branch history, and a git-based scan
#            there would read one merge commit and report a clean corpus.
#   NOT COVERED: the edit history of any of those (the first version is the one that leaked);
#            branch names; the bodies of OTHER issues/PRs this one links to; releases, wiki, gists,
#            and the Actions logs; and anything published after this run. Commit messages are
#            covered for a PULL REQUEST number only — an issue number has no commits, and a push
#            straight to main never reaches this script at all (use `--commits <rev-range>`).
#   NOT COVERED BY ANYTHING, and worth saying out loud: the squash-merge commit message GitHub
#            composes in the merge dialog. It does not exist until the merge button is pressed, so
#            no pre-merge check of any kind can read it. Whatever is typed there is published
#            unscanned, and a closing keyword typed there closes just as hard.
#   NOT COVERED BY CI, specifically: the workflow that calls this script is triggered by
#            `pull_request`, so in CI it only ever reads the text as it stood at the last PUSH.
#            A comment posted after the final push — including a review comment, and including the
#            comment that reports this scan — is never scanned by any automated run. The same limit
#            hits eqoxide#1041 harder than eqoxide#980 did: a scope note ("this PR deliberately
#            does not close X") is most often ADDED TO THE BODY DURING REVIEW, after the last push,
#            which is precisely the edit no automated run here will ever see. This job is therefore
#            NOT a seal on that surface. Run this script by hand against the number before merging,
#            and check `gh pr view <n> --json closingIssuesReferences` against what you intend.
#   The script prints its corpus size and classifies EVERY item, printing one verdict line each,
#   because a checker that prints only exceptions cannot tell "nothing wrong" from "nothing looked
#   at". An empty corpus is an ERROR here, not a pass; so is an empty pattern list.
#
#   THE PATTERN SET IS AN ALLOWLIST. Even a fully-classified, fully-covered corpus is only checked
#   against shapes someone already thought of, so a green run means "no KNOWN shape matched", never
#   "no local detail is present". eqoxide#995 is the standing proof: a tracked comment naming
#   deployment infrastructure sat green under the sibling scanner for months. That comment is
#   scrubbed, and the class is now covered by a THIRD tool — `scripts/check-host-shape.py`, a
#   structural heuristic that flags hostname-shaped bare tokens near infrastructure vocabulary
#   without naming anything. It does NOT run on this surface: it needs a whole tracked corpus to
#   compute its cross-file frequency filter, and a PR body is a handful of paragraphs. So for PR
#   and issue text the host-name class remains uncovered, and this scanner's green run still means
#   only "no KNOWN shape matched". See #995.
#
#   Which of those reach signals actually carry information, stated plainly so none of them is read
#   as more than it is:
#     - `flagged N` — the finding count. Load-bearing.
#     - pattern count / the empty-list refusal — load-bearing, and mutation-tested by --self-test.
#     - `corpus = N items` and the empty-corpus refusal — load-bearing; every issue and PR has at
#       least a title, so 0 means the fetch failed.
#     - `classified N/N` — load-bearing only because CORPUS_TOTAL is read from the manifest while
#       CORPUS_CLASSIFIED is incremented per item actually read. An earlier version incremented both
#       in the same iteration, which made this line true by construction and its check unfailable.
#     - `negclose corpus = N items; classified N/N; flagged N (M on a surface that can actually
#       close)` — the same three signals for the second pass, with the same construction and the
#       same empty-list refusal. Load-bearing. The two numbers differ on purpose: N is every
#       finding, M is the subset on a surface GitHub links from, and only M decides the exit code.
#     - `[negclose-warn]` — a finding on a surface MEASURED not to link (a title, a comment, an
#       issue body). It is a real finding and the rewrite is the same; it does not gate because it
#       cannot close anything. Read it as "fix this before it gets pasted into a body", never as
#       noise. A run whose only findings are warnings exits 0 and says so in the summary line —
#       that is a deliberate policy, recorded with its measurements at negclose_surface_gates, not
#       an accident of the exit code. In `--commits` mode every item is a commit message, so
#       flagged and gating must be EQUAL there; the script errors out if they are not.
#     - `closingIssuesReferences` — printed verbatim on any negclose finding for a PR. This is the
#       only observable that reports what GitHub actually PARSED; what a body appears to say is not
#       evidence of what was linked. Load-bearing, and it is the check to run before every merge.

set -euo pipefail

# Temp dirs are collected and removed by one EXIT trap. A per-function RETURN trap leaks into the
# caller's scope under `set -u` and fires with `dir` unset, which turned a clean run into an
# "unbound variable" failure after it had already printed OK.
SCRATCH_DIRS=()
# Ends in `:` on purpose: a trap whose last command is falsy can leak its status into the script's
# exit code, which turned a passing self-test into a silent exit 1.
cleanup_scratch() { local d; for d in "${SCRATCH_DIRS[@]:-}"; do [ -n "$d" ] && rm -rf "$d"; done; : ; }
trap cleanup_scratch EXIT

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || echo .)"
# The path is overridable so the self-test can point the REAL scanners — this one and
# check-no-local-detail.sh — at an empty pattern list and assert that they refuse to run.
PATTERNS_FILE="${LOCAL_DETAIL_PATTERNS_FILE:-${REPO_ROOT}/scripts/local-detail-patterns.sh}"
# shellcheck source=scripts/local-detail-patterns.sh
source "$PATTERNS_FILE"
# A pattern file that defines nothing at all must land in the same refusal path as one that defines
# an empty list, not in an `unbound variable` crash whose exit code is indistinguishable from a hit.
declare -p LOCAL_DETAIL_PATTERNS     >/dev/null 2>&1 || LOCAL_DETAIL_PATTERNS=()
declare -p LOCAL_DETAIL_DIRTY_SAMPLES >/dev/null 2>&1 || LOCAL_DETAIL_DIRTY_SAMPLES=()

# Reach guard, and the first thing every mode does — before any network call, so an empty list can
# never be mistaken for a clean fetch. This script reports only EXCEPTIONS, so with no patterns it
# would classify every item against nothing and print OK, which is indistinguishable from a clean
# corpus. Refuse loudly instead.
require_patterns() {
  local ctx="$1"
  if [ "${#LOCAL_DETAIL_PATTERNS[@]}" -eq 0 ]; then
    echo "::error::check-pr-text: pattern list is EMPTY — ${ctx} would compare every item against"
    echo "nothing and report OK. That is a guard failure, not a clean result."
    echo "Patterns were read from: ${PATTERNS_FILE}"
    return 1
  fi
  return 0
}

# The convention half of eqoxide#980. Printed on every failure because the fix is almost never
# "delete the sentence" — it is "make the same claim without the path".
print_convention() {
  cat <<'EOF'
How to keep the proof and drop the leak — name the tree, do not paste its path:
  instead of:  `git -C <absolute path to the shared checkout> status --porcelain` is empty
  write:       `git status --porcelain` in the shared checkout is empty — it was never touched
  instead of:  gated in <absolute path to a worktree>
  write:       gated in the `<branch-name>` worktree
The proof value is in the command and its result. The absolute path adds nothing to the claim and
is the entire leak. Same for host names, container names, and inline credentials.
EOF
}

# ---------------------------------------------------------------------------------------------
# eqoxide#1041 — NEGATED CLOSING KEYWORD.
#
# GitHub links `close|closes|closed|fix|fixes|fixed|resolve|resolves|resolved` — optionally with a
# colon after it — to the `#<n>` that follows and does NOT read a negation in front of it. "Does
# not close" + a number closes that issue. The sentence written specifically to record that an
# issue is OUT of scope is the sentence that marks it resolved.
#
# HOW MANY TIMES THIS HAS HAPPENED HERE IS A LOWER BOUND, NOT A COUNT. As of 2026-08-19: AT LEAST
# TEN confirmed false closes, across two surfaces and eight work threads, the longest standing 49
# days (issue 41); all ten are open again and enumerated in eqoxide#1041. Three of them (issues 41,
# 378 and 871) were invisible to the sweep that first reported "six", and 871 exposed the colon
# variant of the defect.
#
# ONE OF THE TEN WAS CAUGHT BY A HUMAN, IN 1h 40m, and it is worth more than the other nine put
# together. Issue 194 was false-closed by PR 752's body and someone noticed the same evening. It was
# then left out of two successive counts as "the one that got noticed" — which is not an instance
# criterion, and on a check whose whole thesis is that these go unnoticed it inverts the meaning of
# the number. Detection latency does not decide membership. If a sentence closed an issue it did not
# mean to close, it is an instance, and the fast catch is evidence the class is real rather than
# evidence that one did not count.
#
# The floor moves for two reasons that no sweep can remove:
#
#   - it reads each body as it stands TODAY, so a disclaimer later edited out leaves no trace; and
#   - it can only match the phrasings on the cue list below, which issue 871 already proved
#     incomplete once.
#
# So the only honest form of the claim is "at least N, found by THESE cues". The cue list is
# NEGCLOSE_NEGATIONS below; every run of this script prints its size next to the corpus size, so
# the cues are named at the point the number is used. Never quote a bare count from this file as
# if the set were closed. It is not closed, and it was wrong the first time.
#
# TWO THINGS ARE CHECKED HERE AND THEY ARE DELIBERATELY NOT THE SAME CHECK:
#
#   1. NEGCLOSE (a text lint, offline, gates). "A closing keyword sits next to a number you appear
#      to be disclaiming." No attempt is made to infer intent from wider context, because the
#      remedy is the same rewrite either way: `Refs <NUM>` / `does not address <NUM>` / `<NUM>
#      stays open`. This fires on the TEXT and says nothing about whether anything was closed.
#
#   2. ATTRIBUTION (an audit verdict). "Did THIS pull request or commit range actually cause that
#      issue's close?" A different question with a different answer, and issue 302 is the standing
#      proof that it has to be asked separately: PR #653's body disclaims 302, GitHub linked 302 to
#      that PR anyway (it is in its `closingIssuesReferences` to this day) — and 302's close was
#      still legitimate. That close fired at 2026-07-11T20:18:54Z, ONE SECOND after PR #306 merged
#      (20:18:53Z); PR #653 was not opened until 2026-07-22T14:35:40Z, 10d 18h 16m LATER. PR #306
#      closed it. Matching the text pattern is not a false close.
#
#   So: the text lint flags PR #653's sentence — correctly, because it created a real link and the
#   remedy is the same rewrite — while the attribution classifier returns NOT-ATTRIBUTED for 302. A
#   guard that collapsed the two into one would have to choose between missing 982 and libelling
#   PR #653.
#
# REACH OF THE NEGCLOSE LINT — stated because a scanner that reports only exceptions cannot tell
# "nothing wrong" from "nothing looked at":
#   COVERED: every item in the corpus this script assembles, which in live mode now INCLUDES the
#            commit messages of the pull request's own commits — surface 2 of the defect, where
#            issue 314 died and nowhere else. `--commits <range>` scans local git commit messages
#            instead, for use before a PR exists.
#   NOT COVERED: the squash-merge message typed into GitHub's merge dialog, which does not exist
#            anywhere until the merge button is pressed and is therefore unreachable by ANY
#            pre-merge check; text edited after the last push (see the `pull_request` note above —
#            a scope note added to a body during review is exactly the edit this misses);
#            `GH-123` and full-URL issue references, which GitHub also links but this regex does
#            not read; a negation separated from the keyword by `.`, `;`, `!`, `?` or an em dash,
#            which the window deliberately will not cross (see NEGCLOSE_GAP); a disclaimer SPLIT
#            ACROSS TWO LINES, with the negation ending one line and the keyword opening the next,
#            because the scan is line-oriented `grep` — plausible in wrapped Markdown, and swept
#            for across this repo's full published history on 2026-08-19 with zero instances
#            found, so it is a real hole with no known occupant; and, in live mode, a pull request
#            with more than 250 commits, which is where `/pulls/<n>/commits` stops paginating
#            (measured maximum in this repo: 14, so it cannot bite today).
#   BACKTICKS ARE REAL SUPPRESSION AND YOU STILL MUST NOT USE THEM AS SUCH. Measured 4/4 in this
#            repo (PRs #670, #841, #996, #1037; zero counterexamples): a closing keyword inside a
#            code span is NOT parsed and creates NO link. Every control that DID link — #791,
#            #821, #824, #835, #840 — carries a second, un-backticked copy of the keyword.
#            This lint flags backticked occurrences anyway, on purpose, because the exemption is
#            not a mechanism to rely on: bodies get edited, backticks come off, and the exemption
#            is undocumented behaviour observed in one repo rather than a promise.
#            THE OTHER HALF MATTERS MORE AND IS EASY TO MISS: do not put your REAL `Closes #N`
#            inside backticks either, or it will silently not close. That is not hypothetical —
#            PR #1037 wrote its real target as `Closes #1022` in backticks, so its
#            `closingIssuesReferences` came back as exactly [939, 1010], the two issues it
#            DISCLAIMED and not the one it fixed. Issue 1022 was closed only because that PR's
#            squash commit `895d36c` happened to carry a plain `Closes #1022` on line 38 of its
#            MESSAGE — the other surface, by luck. One PR, both failure modes, one body.
#   CONSEQUENCE, stated so nobody is surprised: a PR whose body QUOTES this pattern — including the
#            PR that added this check — is flagged, and that is correct behaviour, not a false
#            positive. Quoting it in a body is not safe; GitHub links quoted text too.
#
# `\b` is a GNU grep extension. Both CI (ubuntu-latest) and the dev boxes run GNU grep; --self-test
# exercises every boundary case, so a grep without `\b` fails the self-test loudly rather than
# silently matching "Notes:" and "another".

# The keyword half is GitHub's documented set, verbatim, as an ERE alternation. Kept as a plain
# string (not an array) so that emptying it via NEGCLOSE_PATTERNS_FILE is a one-line mutation.
NEGCLOSE_KEYWORDS='close[sd]?|fix(es|ed)?|resolve[sd]?'

# The negation half. Each entry carries its own word-boundary anchoring because they do not all
# anchor the same way: `not` MUST be a whole word or `Notes:`, `another` and `annotation` all match
# it, while `n't` is a suffix with no left boundary at all inside `doesn't`. The typographic
# apostrophe is a separate entry rather than a bracket expression, because a multi-byte character
# inside `[...]` is not portable across grep locales while an alternation is.
NEGCLOSE_NEGATIONS=(
  '\bnot\b'
  '\bcannot\b'
  "n't\\b"
  'n’t\b'
  '\bnever\b'
  '\bno longer\b'
  '\bwithout\b'
  '\brather than\b'
  '\binstead of\b'
)

# How far the negation may sit in front of the keyword. eqoxide#1041 specified ~40 characters. The
# gap class excludes `#` so the window cannot leap over an intervening issue reference, and excludes
# `.;!?` so a negation in the PREVIOUS sentence cannot trip the next sentence's legitimate close
# ("This does not touch the renderer. Fixes #500." must stay green).
#
# The em dash is in the stop set for the same reason, and it was added on measured evidence rather
# than taste: this repo's conventional subject line is `<what changed, often with a negation> —
# Fixes #N`, e.g. "auto-loot into a free slot + stack, never overwrite occupied slots — Fixes #56".
# Swept over the full published history on 2026-08-19 (4598 items: every PR title and body, every
# issue title and body, every issue/PR comment, every commit message on main), adding `—` takes the
# flag count 49 -> 41. All 8 dropped items are false positives — three PR titles, their three
# commit-message twins, and two comments — and ZERO measured true instances are lost: every fixture
# in NC_RED_FIXTURES still fires. The set is also byte-identical under LC_ALL=C and LC_ALL=
# en_US.UTF-8, checked, so the multi-byte character inside a negated bracket expression is not a
# locale hazard here in the way it would be for a positive match.
#
# COVERAGE OF THESE TWO CONSTANTS, stated narrowly and measured, because an overstated coverage
# claim is the exact defect this script exists to catch. An earlier revision of this comment said
# "all of these exclusions ... are exercised by --self-test in both directions". That was FALSE:
# when it was written only `.` and `—` were pinned, and dropping any of `#`, `;`, `!` or `?` left
# --self-test at a full-green 67. Re-measured against THIS revision, one character at a time:
#
#   stop set   dropping any ONE of `#` `.` `;` `!` `?` `—` turns --self-test RED (1 check each);
#              emptying the set entirely turns 19 red. Each is now held by a named green fixture,
#              `#` included — it is the one with its own written rationale above, and it was the
#              least guarded of the six.
#   NEGCLOSE_GAP  pinned on the RECALL side only. The widest real instance in NC_RED_FIXTURES needs
#              16 characters, so 15 and below turn --self-test RED and 16 turns it green.
#
# WHAT IS STILL NOT PINNED, said plainly rather than left inside a broad claim: nothing requires the
# window to STOP at 40. 41, 100 and 400 are all full green, so a widened window is invisible to this
# suite. Pinning that side needs a green fixture asserting that a negation ~41 characters ahead of a
# close is unrelated, and this repository has no measured text supporting that boundary — inventing
# one would encode 40 as correct on no evidence, which is the same failure in the other direction.
# It is recorded as a known gap instead. Narrowing the window is NOT invisible and never was
# harmless: at NEGCLOSE_GAP=3 the real bodies of pull requests 835 and 66 flip from flagged to
# clean and the exit code from 1 to 0 — measured, which is why the recall side is pinned first.
NEGCLOSE_GAP=40
NEGCLOSE_GAP_STOPS='#.;!?—'

# Same override mechanism as LOCAL_DETAIL_PATTERNS_FILE, and for the same reason: it is how the
# self-test points the REAL script at an empty list and asserts that it refuses to run, instead of
# re-deriving the refusal inline where the assertion could not fail.
if [ -n "${NEGCLOSE_PATTERNS_FILE:-}" ]; then
  # shellcheck disable=SC1090
  source "$NEGCLOSE_PATTERNS_FILE"
fi
declare -p NEGCLOSE_NEGATIONS >/dev/null 2>&1 || NEGCLOSE_NEGATIONS=()

# Refuse to run rather than report a clean corpus, for the same reason require_patterns does: with
# no negations (or no keywords) every item is compared against nothing and prints "no findings",
# which is the same word an actually-clean corpus prints.
require_negclose_patterns() {
  local ctx="$1"
  if [ "${#NEGCLOSE_NEGATIONS[@]}" -eq 0 ] || [ -z "${NEGCLOSE_KEYWORDS:-}" ]; then
    echo "::error::check-pr-text: the negated-close negation or keyword list is EMPTY — ${ctx}"
    echo "would compare every item against nothing and report no findings. That is a guard failure,"
    echo "not a clean result. See eqoxide#1041."
    return 1
  fi
  return 0
}

# Build the ERE. Returns 1 (printing nothing) when either half is empty, so the reach control in
# --self-test can empty a list and observe that NOTHING flags.
negclose_regex() {
  [ "${#NEGCLOSE_NEGATIONS[@]}" -gt 0 ] || return 1
  [ -n "${NEGCLOSE_KEYWORDS:-}" ] || return 1
  local joined="" n
  for n in "${NEGCLOSE_NEGATIONS[@]}"; do
    [ -n "$n" ] || continue
    joined="${joined:+${joined}|}${n}"
  done
  [ -n "$joined" ] || return 1
  printf '%s' "(${joined})[^${NEGCLOSE_GAP_STOPS}]{0,${NEGCLOSE_GAP}}\\b(${NEGCLOSE_KEYWORDS})[[:space:]:]*#[0-9]+"
}

# Classify one text file for negated closing keywords. Prints `LINE:matched text`, one per hit;
# prints nothing when clean. Never exits non-zero on "no match" — the caller decides what a match
# means, same contract as scan_text_file above.
scan_negated_close() {
  local f="$1" re
  re="$(negclose_regex)" || return 0
  grep -noEi -e "$re" -- "$f" 2>/dev/null || true
}

# The convention half of eqoxide#1041, printed on every finding because the fix is never "delete
# the sentence" — the scope note is worth keeping, it just must not use a linking keyword.
print_negclose_convention() {
  cat <<'EOF'
A closing keyword immediately before an issue number LINKS that issue — with or without a colon
after the keyword — and GitHub does not read the negation in front of it. The sentence you wrote to
keep the issue OUT of scope is the sentence that closes it. Rewrite, keeping the scope note:
  instead of:  does not close <NUM>            write:  Refs <NUM> — not addressed here
  instead of:  deliberately NOT Closes <NUM>   write:  <NUM> stays open — narrowed, not closed
  instead of:  it does NOT fix <NUM>           write:  does not address <NUM>
  instead of:  ## Filed, not fixed: <NUM>      write:  ## Filed, not addressed: <NUM>

ABOUT BACKTICKS — this is flagged even inside a code span, and the reason is not that backticks
fail to suppress the link. They DO suppress it: measured 4/4 in this repo (PRs 670, 841, 996, 1037,
no counterexamples), a closing keyword inside a code span creates no link at all. It is flagged
anyway because that exemption is undocumented behaviour to lean a guard on, and one edit that drops
the backticks turns the sentence live again with nothing to notice it.
The consequence you are far more likely to be bitten by runs the OTHER way: do not put your REAL
`Closes <NUM>` inside backticks either, or it will silently never close. PR 1037 did exactly that
and GitHub reported its links as the two issues it DISCLAIMED and not the one it fixed.

Then, before merging, check `gh pr view <n> --json closingIssuesReferences` against what this pull
request actually intends to close. That command reports what GitHub PARSED; what you meant to write
is not observable from the body. See eqoxide#1041.
EOF
}

# Walk the same corpus manifest and classify EVERY line for a negated closing keyword.
# Sets globals: NEGCLOSE_TOTAL, NEGCLOSE_CLASSIFIED, NEGCLOSE_FLAGGED, NEGCLOSE_GATING,
# NEGCLOSE_NUMBERS. FLAGGED counts every item with a hit; GATING counts only the subset on a
# surface GitHub actually links from, which is the number that decides the exit code.
# NEGCLOSE_TOTAL is read from the manifest and NOT incremented alongside NEGCLOSE_CLASSIFIED, for
# the same reason classify_corpus does it that way: counting both in one loop iteration makes
# `classified N/N` an identity that can never disagree.

# Which surfaces GitHub actually LINKS from. Measured in this repo, not assumed:
#   body of a PULL REQUEST -> links (nine of the ten confirmed false closes of eqoxide#1041);
#   commit message         -> links once the commit reaches the default branch (issue 314, and
#                             issue 1022 via the squash commit 895d36c);
#   PR title               -> does NOT link. 5/5 measured: PRs 236, 437, 461, 465 and 466 each
#                             carry a closing keyword in the title for a number the body never
#                             names WITH A CLOSING KEYWORD, and every one reports
#                             `closingIssuesReferences: []`. Note the qualifier: three of the five
#                             (236, 465, 466) DO name the number in prose. Bare mentions are not
#                             the control — a keyword-free mention cannot link either — so the
#                             control is "no keyword in the body", which holds 5/5.
#   comments               -> do NOT link, and the measurement is built to be falsifiable: a
#                             comment can only be shown not to link where the BODY does not
#                             already explain the link. Swept 2026-08-19 over 500 pull requests
#                             and all 833 issue-style comments on them: 134 comment-borne
#                             keyword+number pairs across 61 pull requests, 80 distinct
#                             (pull request, number) references. 41 of those numbers ARE in the
#                             parent's `closingIssuesReferences` and every one of the 41 is
#                             explained by a closing keyword in the pull request's own BODY; the
#                             other 39, across 26 pull requests, are not linked at all. Links
#                             attributable to a comment: ZERO out of 80.
#                             (An earlier revision of this file said "33/33 ... a closing target
#                             its body never names". That sentence had the same defect as the
#                             title one above — "names" where it meant "names with a closing
#                             keyword" — so it was replaced rather than restated.)
#   issue body/title       -> nothing to link FROM; an issue is not a pull request.
#
# THE GRAMMAR BETWEEN THE KEYWORD AND THE NUMBER was measured too, because this repository's own
# commit-subject convention sits right on it. `fix(<NUM>)` — the conventional-commit scope form used
# by nearly every commit here — does NOT link: 2/2 clean cases in the last 40 merged pull requests
# (PR 1019's body carries that form for issue 1015 and linked only 995; PR 947 carries it for issue
# 884 and linked only 901), with a third case confounded because the same number was also named
# plainly elsewhere in the body.
# A parenthesis is therefore not a link, which is why the gap this scanner allows between the
# keyword and the `#` is whitespace and a colon and nothing else. Two consequences worth stating:
# a `fix(<NUM>)` subject line does not close that issue, so a commit that MEANS to close must say so
# in the plain form; and the scanner allows a zero-width gap, which is marginally wider than the
# measured grammar — it can only cost a false positive, never a miss.
#
# THIS IS WHY ONLY TWO SURFACES GATE. Every item is still scanned, still classified, still printed
# — the text is copied from one surface into another and the rewrite is identical — but only a
# finding that can ACTUALLY cause a false close returns non-zero: a pull request's BODY, or a
# COMMIT MESSAGE. A finding on a title, a comment or an issue body is reported as a WARNING.
#
# The alternative was measured and rejected on this very pull request. Gating on all surfaces made
# the review comment that QUOTES the defect fail the check, which would block every PR whose review
# discusses this class — the shape that gets a guard switched off. And it would have been a false
# statement as well as an obstruction: the same comment linked nothing at all.
negclose_surface_note() {
  local kind="$1" num="$2"
  case "$kind" in
    commit)
      echo "                GitHub closes issue ${num} when this commit reaches the default branch." ;;
    body)
      if [ "${CORPUS_IS_PR:-no}" = "yes" ]; then
        echo "                GitHub reads that as a link and will CLOSE issue ${num} on merge."
      else
        echo "                An issue body does not link, so issue ${num} does not close from here"
        echo "                — but this sentence closes it the moment it is pasted into a PR body."
      fi ;;
    title)
      echo "                A title does not link (measured 5/5 here), so issue ${num} does not"
      echo "                close from here — rewrite it anyway; titles get copied into bodies." ;;
    *)
      echo "                A comment does not link (0 of 80 measured here), so issue ${num} does"
      echo "                not close from here — rewrite it anyway; comments get pasted into bodies." ;;
  esac
}

# Does a finding on this surface gate the run, or only warn? Exactly the surfaces measured above to
# create a real link: a PULL REQUEST's body, and any commit message. Kept as its own function so
# --self-test can assert the policy directly rather than inferring it from an exit code.
negclose_surface_gates() {
  case "$1" in
    commit) return 0 ;;
    body)   [ "${CORPUS_IS_PR:-no}" = "yes" ] && return 0 || return 1 ;;
    *)      return 1 ;;
  esac
}

classify_negclose() {
  local manifest="$1" kind id path hits hit ln txt num
  NEGCLOSE_CLASSIFIED=0; NEGCLOSE_FLAGGED=0; NEGCLOSE_GATING=0; NEGCLOSE_NUMBERS=""
  NEGCLOSE_TOTAL=$(grep -c '[^[:space:]]' "$manifest" || true)
  while IFS=$'\t' read -r kind id path; do
    [ -n "${kind:-}" ] || continue
    if [ ! -r "${path:-/nonexistent}" ]; then
      echo "  [UNREAD]      ${kind}#${id} — ${path:-<no path>} unreadable; NOT classified"
      continue
    fi
    hits="$(scan_negated_close "$path")"
    if [ -n "$hits" ]; then
      NEGCLOSE_FLAGGED=$((NEGCLOSE_FLAGGED + 1))
      if negclose_surface_gates "$kind"; then
        NEGCLOSE_GATING=$((NEGCLOSE_GATING + 1))
        echo "  [NEGCLOSE]    ${kind}#${id}"
      else
        echo "  [negclose-warn] ${kind}#${id} — this surface does not link, so it does not gate"
      fi
      while IFS= read -r hit; do
        [ -n "$hit" ] || continue
        ln="${hit%%:*}"; txt="${hit#*:}"
        num="$(printf '%s' "$txt" | sed -E 's/.*#([0-9]+).*/\1/')"
        echo "                line ${ln}: \"${txt}\""
        negclose_surface_note "$kind" "$num"
        NEGCLOSE_NUMBERS="${NEGCLOSE_NUMBERS} ${num}"
      done <<< "$hits"
    else
      echo "  [no-negclose] ${kind}#${id}"
    fi
    NEGCLOSE_CLASSIFIED=$((NEGCLOSE_CLASSIFIED + 1))
  done < "$manifest"
}

# ---------------------------------------------------------------------------------------------
# ATTRIBUTION — a different question from the text lint. Given an issue's close event and the pull
# request or commit range under examination, did THIS change cause that close?
#
# The rules are derived from measured close events of eqoxide#1041, not invented:
#   - a linked-issue close carries `commit_id: null` and fires 1-2 seconds after the PR's merge, so
#     a close that PRE-DATES the merge cannot have come from it (this is the whole of issue 302);
#   - a commit-message close carries the closing commit's sha, so membership in the range is the
#     test (this is issue 314, closed by 10619d99 with no PR link at all).
# Prints exactly one of: ATTRIBUTED-LINK, ATTRIBUTED-COMMIT, NOT-ATTRIBUTED, STILL-OPEN.
# ISO-8601 UTC timestamps compare correctly as strings; every value here arrives from the API in
# that form, so no date parsing is involved and there is nothing for a locale to break.
negclose_attribution() {
  local close_at="$1" close_commit="$2" pr_merged_at="$3" range_commits="$4"
  if [ -z "$close_at" ] || [ "$close_at" = "null" ]; then
    echo "STILL-OPEN"; return 0
  fi
  if [ -n "$close_commit" ] && [ "$close_commit" != "null" ]; then
    case " ${range_commits} " in
      *" ${close_commit} "*) echo "ATTRIBUTED-COMMIT" ;;
      *)                     echo "NOT-ATTRIBUTED" ;;
    esac
    return 0
  fi
  if [ -z "$pr_merged_at" ] || [ "$pr_merged_at" = "null" ]; then
    echo "NOT-ATTRIBUTED"; return 0
  fi
  if [[ "$close_at" < "$pr_merged_at" ]]; then
    echo "NOT-ATTRIBUTED"
  else
    echo "ATTRIBUTED-LINK"
  fi
}

# Classify one text file. Prints one line per matching pattern; prints nothing when clean.
# Never exits non-zero on "no match" — the caller decides what a match means.
scan_text_file() {
  local f="$1" re
  for re in "${LOCAL_DETAIL_PATTERNS[@]:-}"; do
    [ -n "$re" ] || continue
    if grep -nE -e "$re" -- "$f" >/dev/null 2>&1; then
      echo "$re"
    fi
  done
}

# Walk a corpus manifest (TAB-separated: kind, id, path) and classify EVERY line.
# Sets globals: CORPUS_TOTAL, CORPUS_CLASSIFIED, CORPUS_FLAGGED.
classify_corpus() {
  local manifest="$1" kind id path hits n
  CORPUS_CLASSIFIED=0; CORPUS_FLAGGED=0
  # CORPUS_TOTAL is read from the manifest, NOT incremented alongside CORPUS_CLASSIFIED in the loop
  # below. Incrementing both in the same iteration makes `classified N/N` an identity that can
  # never disagree, and an assertion that cannot fail is not evidence of anything.
  CORPUS_TOTAL=$(grep -c '[^[:space:]]' "$manifest" || true)
  while IFS=$'\t' read -r kind id path; do
    [ -n "${kind:-}" ] || continue
    if [ ! -r "${path:-/nonexistent}" ]; then
      # Counted in TOTAL (it came from the manifest) but never in CLASSIFIED. This is the case the
      # `classified N/N` line exists to catch: an item that was listed and then not looked at.
      echo "  [UNREAD]  ${kind}#${id} — ${path:-<no path>} unreadable; NOT classified"
      continue
    fi
    hits="$(scan_text_file "$path")"
    n=$(wc -c < "$path")
    if [ -n "$hits" ]; then
      CORPUS_FLAGGED=$((CORPUS_FLAGGED + 1))
      echo "  [FLAGGED] ${kind}#${id} (${n} chars)"
      while read -r re; do
        [ -n "$re" ] || continue
        echo "            pattern /${re}/ matched:"
        grep -nE -e "$re" -- "$path" | sed 's/^/              /'
      done <<< "$hits"
    else
      echo "  [clean]   ${kind}#${id} (${n} chars)"
    fi
    CORPUS_CLASSIFIED=$((CORPUS_CLASSIFIED + 1))
  done < "$manifest"
}

# ---------------------------------------------------------------------------------------------
# Live mode: build the corpus from the GitHub API.
# ---------------------------------------------------------------------------------------------

# Decode a `kind<TAB>id<TAB>base64(body)` stream into one file per item plus a manifest line.
# Bodies are carried base64-encoded because they are multi-line and the stream is line-oriented.
emit_items() {
  local dir="$1" manifest="$2" kind id b64
  while IFS=$'\t' read -r kind id b64; do
    [ -n "${kind:-}" ] || continue
    printf '%s' "$b64" | base64 -d > "${dir}/${kind}-${id}.txt"
    printf '%s\t%s\t%s\n' "$kind" "$id" "${dir}/${kind}-${id}.txt" >> "$manifest"
  done
}

# Every fetch is checked. A swallowed failure here does not produce a smaller corpus that looks
# suspicious — it produces a corpus that is silently missing a surface while the final line still
# says which surfaces were covered. That would be exactly the false reach statement this script
# was written to eliminate.
fetch_or_die() {
  local out="$1" what="$2"; shift 2
  if ! gh api "$@" > "$out" 2>"${out}.err"; then
    echo "::error::check-pr-text: fetching ${what} FAILED — the corpus is incomplete, so no verdict"
    echo "can be given. This is a fetch failure, not a clean result."
    sed 's/^/  /' "${out}.err" || true
    return 1
  fi
  return 0
}

fetch_corpus() {
  local number="$1" repo="$2" dir="$3" manifest="$4"
  : > "$manifest"
  CORPUS_IS_PR="no"; CORPUS_PR_COMMITS=""

  # /issues/<n> serves both issues and PRs, so title/body/comments work for either — and the SAME
  # payload carries `.pull_request` when the number is a PR. Probing /pulls/<n> separately used to
  # decide that, which meant a transient failure of the probe was indistinguishable from "this is
  # an issue": the script would skip the review surfaces and still print a reach line claiming it
  # had covered everything an issue has. One fetch, one answer, no way for them to disagree.
  local issue_json="${dir}/issue.json"
  fetch_or_die "$issue_json" "repos/${repo}/issues/${number}" "repos/${repo}/issues/${number}" || return 1

  jq -r '"title\t\(.number)\t\(.title // "" | @base64)", "body\t\(.number)\t\(.body // "" | @base64)"' \
    < "$issue_json" | emit_items "$dir" "$manifest"

  local comments="${dir}/comments.tsv"
  fetch_or_die "$comments" "repos/${repo}/issues/${number}/comments" \
    "repos/${repo}/issues/${number}/comments" --paginate \
    --jq '.[] | "comment\t\(.id)\t\(.body // "" | @base64)"' || return 1
  emit_items "$dir" "$manifest" < "$comments"

  CORPUS_IS_PR="$(jq -r 'if .pull_request then "yes" else "no" end' < "$issue_json")"
  local is_pr="$CORPUS_IS_PR"
  if [ "$is_pr" = "yes" ]; then
    local reviews="${dir}/reviews.tsv" rcomments="${dir}/review-comments.tsv"
    fetch_or_die "$reviews" "repos/${repo}/pulls/${number}/reviews" \
      "repos/${repo}/pulls/${number}/reviews" --paginate \
      --jq '.[] | select((.body // "") != "") | "review\t\(.id)\t\(.body | @base64)"' || return 1
    emit_items "$dir" "$manifest" < "$reviews"

    fetch_or_die "$rcomments" "repos/${repo}/pulls/${number}/comments" \
      "repos/${repo}/pulls/${number}/comments" --paginate \
      --jq '.[] | "review-comment\t\(.id)\t\(.body // "" | @base64)"' || return 1
    emit_items "$dir" "$manifest" < "$rcomments"

    # COMMIT MESSAGES (eqoxide#1041, surface 2). Issue 314 was false-closed by a commit message and
    # by nothing else — no PR body anywhere carried the text — so a check that reads only bodies and
    # comments would have watched that one go past. These come from the API rather than from git on
    # purpose: every `actions/checkout` in this repo's workflow runs at the default fetch-depth of
    # 1, so the runner does NOT have the branch history, and a git-based scan there would silently
    # examine one merge commit and report a clean corpus. Use `--commits <range>` for the git-based
    # scan on a machine that actually has the commits.
    local prcommits="${dir}/pr-commits.tsv"
    fetch_or_die "$prcommits" "repos/${repo}/pulls/${number}/commits" \
      "repos/${repo}/pulls/${number}/commits" --paginate \
      --jq '.[] | "commit\t\(.sha)\t\(.commit.message // "" | @base64)"' || return 1
    emit_items "$dir" "$manifest" < "$prcommits"
    CORPUS_PR_COMMITS="$(cut -f2 "$prcommits" | tr '\n' ' ')"

    echo "check-pr-text: number ${number} is a PULL REQUEST (issues/${number} carries .pull_request)"
    echo "               — review bodies, inline review comments and commit messages included."
  else
    echo "check-pr-text: number ${number} is an ISSUE (issues/${number} has no .pull_request);"
    echo "               there are no pull-request surfaces to fetch."
  fi
}

# The pre-merge control eqoxide#1041 asks for by name: ask GITHUB what it parsed, rather than
# trusting what the body appears to say. `closingIssuesReferences` is the only observable that
# reports the actual links; it is what proves PR #1037 linked the two issues it disclaimed and did
# NOT link the one it meant to close.
# Sets NEGCLOSE_LINKED (space-padded list of linked issue numbers). Returns 1 on fetch failure.
negclose_fetch_links() {
  local number="$1" repo="$2" dir="$3"
  local owner="${repo%%/*}" name="${repo##*/}" out="${dir}/closing-refs.json"
  fetch_or_die "$out" "closingIssuesReferences for ${repo}#${number}" graphql \
    -f query='query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){merged mergedAt closingIssuesReferences(first:100){nodes{number state}}}}}' \
    -F owner="$owner" -F name="$name" -F number="$number" || return 1
  NEGCLOSE_LINKED=" $(jq -r '.data.repository.pullRequest.closingIssuesReferences.nodes[].number' < "$out" | tr '\n' ' ')"
  NEGCLOSE_PR_MERGED_AT="$(jq -r '.data.repository.pullRequest.mergedAt // ""' < "$out")"
  return 0
}

# For one disclaimed issue number, say whether GitHub currently links it to this PR and — when the
# PR is already merged — whether that issue's close is actually attributable to it.
negclose_report_number() {
  local num="$1" repo="$2" dir="$3"
  case "$NEGCLOSE_LINKED" in
    *" ${num} "*)
      echo "    issue ${num}: LINKED by GitHub to this pull request — it WILL close on merge." ;;
    *)
      echo "    issue ${num}: not currently in this pull request's closingIssuesReferences."
      echo "                  Still rewrite the sentence: the link set is recomputed from the body"
      echo "                  on every edit, so 'not linked right now' is not a durable property."
      return 0 ;;
  esac
  [ -n "${NEGCLOSE_PR_MERGED_AT:-}" ] || return 0
  local tl="${dir}/timeline-${num}.json" close_at close_commit verdict
  fetch_or_die "$tl" "close events for ${repo}#${num}" \
    "repos/${repo}/issues/${num}/timeline?per_page=100" --paginate \
    --jq '[.[] | select(.event=="closed")] | last | "\(.created_at // "")\t\(.commit_id // "null")"' || return 1
  close_at="$(cut -f1 < "$tl")"; close_commit="$(cut -f2 < "$tl")"
  verdict="$(negclose_attribution "$close_at" "$close_commit" "$NEGCLOSE_PR_MERGED_AT" "${CORPUS_PR_COMMITS:-}")"
  echo "                  close attribution: ${verdict} (close_at=${close_at:-none} commit_id=${close_commit:-null}"
  echo "                  merged_at=${NEGCLOSE_PR_MERGED_AT}). NOT-ATTRIBUTED means something else"
  echo "                  closed it first, as with issue 302 — the text is still wrong, the close is not."
  return 0
}

run_live() {
  local number="$1" repo="$2"
  require_patterns "a live scan of ${repo}#${number}" || return 1
  require_negclose_patterns "a live scan of ${repo}#${number}" || return 1

  local dir manifest
  dir="$(mktemp -d)"; SCRATCH_DIRS+=("$dir")
  manifest="${dir}/manifest.tsv"

  echo "check-pr-text: scanning ${repo}#${number} with ${#LOCAL_DETAIL_PATTERNS[@]} patterns."
  fetch_corpus "$number" "$repo" "$dir" "$manifest" || return 1

  # The corpus is itself an item to be checked. A number that yields nothing has not been proven
  # clean — it has not been read. Every issue and PR has at least a title, so 0 means the fetch
  # failed (bad token, bad number, rate limit) and MUST NOT report OK.
  if [ ! -s "$manifest" ]; then
    echo "::error::check-pr-text: corpus is EMPTY for ${repo}#${number} — nothing was examined."
    echo "This is a fetch failure, not a clean result. Check GH_TOKEN scope and the number."
    return 1
  fi

  classify_corpus "$manifest"

  echo "check-pr-text: corpus = ${CORPUS_TOTAL} items; classified ${CORPUS_CLASSIFIED}/${CORPUS_TOTAL}; flagged ${CORPUS_FLAGGED}."
  # Both passes always RUN. An early `return 1` from the local-detail half used to be the only exit
  # here; leaving it that way would mean a body with a home path in it never got scanned for a
  # negated closing keyword at all, and the run would report one finding while silently declining to
  # look for the other. Accumulate instead, and report every pass.
  local rc=0
  if [ "$CORPUS_CLASSIFIED" -ne "$CORPUS_TOTAL" ]; then
    echo "::error::check-pr-text: ${CORPUS_CLASSIFIED} of ${CORPUS_TOTAL} items were classified — the rest were skipped."
    rc=1
  fi

  if [ "$CORPUS_FLAGGED" -gt 0 ]; then
    echo "::error::check-pr-text: ${CORPUS_FLAGGED} of ${CORPUS_TOTAL} items carry local-system or proprietary-derived detail."
    echo
    print_convention
    echo
    echo "DETECTOR, NOT PREVENTER: this text is already published and GitHub keeps the edit"
    echo "history. Editing it now limits further exposure; it does not un-publish it."
    rc=1
  fi

  # ---- eqoxide#1041: the negated-closing-keyword pass, over the SAME corpus. ----
  echo
  echo "check-pr-text: negated-closing-keyword pass (eqoxide#1041) over the same ${CORPUS_TOTAL} items,"
  echo "               ${#NEGCLOSE_NEGATIONS[@]} negation cues against GitHub's closing-keyword set."
  classify_negclose "$manifest"
  echo "check-pr-text: negclose corpus = ${NEGCLOSE_TOTAL} items; classified ${NEGCLOSE_CLASSIFIED}/${NEGCLOSE_TOTAL}; flagged ${NEGCLOSE_FLAGGED} (${NEGCLOSE_GATING} on a surface that can actually close)."
  if [ "$NEGCLOSE_CLASSIFIED" -ne "$NEGCLOSE_TOTAL" ]; then
    echo "::error::check-pr-text: negclose pass classified ${NEGCLOSE_CLASSIFIED} of ${NEGCLOSE_TOTAL} items — the rest were skipped."
    rc=1
  fi

  if [ "$NEGCLOSE_FLAGGED" -gt 0 ]; then
    if [ "$NEGCLOSE_GATING" -gt 0 ]; then
      echo "::error::check-pr-text: ${NEGCLOSE_GATING} of ${NEGCLOSE_TOTAL} items put a closing keyword next to an issue number they appear to disclaim, on a surface GitHub links from."
    else
      echo "::warning::check-pr-text: ${NEGCLOSE_FLAGGED} of ${NEGCLOSE_TOTAL} items put a closing keyword next to an issue number they appear to disclaim — all of them on a surface GitHub does NOT link from (title/comment/issue body), so this WARNS and does not gate. Rewrite them anyway: that text gets pasted into a body."
    fi
    local nums num
    nums="$(printf '%s' "$NEGCLOSE_NUMBERS" | tr ' ' '\n' | grep -E '^[0-9]+$' | sort -un | tr '\n' ' ')"
    echo "  numbers at risk: ${nums}"
    if [ "${CORPUS_IS_PR:-no}" = "yes" ]; then
      if negclose_fetch_links "$number" "$repo" "$dir"; then
        echo "  GitHub's own answer — closingIssuesReferences for this pull request:${NEGCLOSE_LINKED:- <none>}"
        for num in $nums; do
          negclose_report_number "$num" "$repo" "$dir" || rc=1
        done
      else
        rc=1
      fi
    else
      echo "  (This number is an ISSUE, not a pull request: closingIssuesReferences does not apply,"
      echo "   so only the text is checked here. The same sentence in a PR body WOULD link.)"
    fi
    echo
    print_negclose_convention
    # Only a finding on a surface GitHub actually links from can cause the defect, so only that
    # gates. See negclose_surface_gates: measured, not a preference.
    if [ "$NEGCLOSE_GATING" -gt 0 ]; then
      rc=1
    fi
  fi

  if [ "$rc" -ne 0 ]; then
    return "$rc"
  fi

  # A warn-only negclose finding does not gate, so this line is reachable with NEGCLOSE_FLAGGED > 0.
  # It said "all N items clean ... on both passes" over a flagged item, on the honesty surface, in
  # the one check whose entire subject is a sentence that states the opposite of what happened.
  # Whatever it says now has to be true of BOTH passes, so it is derived from the counts.
  if [ "${NEGCLOSE_FLAGGED:-0}" -gt 0 ]; then
    echo "check-pr-text: NOT CLEAN, but not gating — ${CORPUS_TOTAL} items, ${NEGCLOSE_FLAGGED} with"
    echo "a negated closing keyword, ${NEGCLOSE_GATING} of them on a surface that can actually close."
    echo "Exit 0 here means NOT GATING, it does not mean clean. Rewrite the flagged text anyway: it"
    echo "closes the issue the moment anyone pastes it into a pull request body."
  else
    echo "check-pr-text: OK — all ${CORPUS_TOTAL} items clean AS CURRENTLY WRITTEN, on both passes."
  fi
  echo "check-pr-text: this is a DETECTOR, not a preventer. It reads the CURRENT text only, so a"
  echo "green run means 'nothing is exposed now', NOT 'nothing was ever exposed' — an earlier"
  echo "revision of any of these items may have carried detail that is still in the edit history."
  echo "Not covered: edit history, linked issues/PRs, releases, wiki, CI logs —"
  echo "and, when run from the pull_request workflow, any comment posted after the last push,"
  echo "which includes every review comment on the final revision."
  echo "Commit messages ARE covered for a pull request (the PR's own commits, via the API), and are"
  echo "NOT covered for an issue number or for a push straight to main. The squash-merge message"
  echo "typed into GitHub's merge dialog is covered by nothing: it does not exist until merge."
  echo "The pattern set is an ALLOWLIST of shapes someone already thought of, so this reads as"
  echo "'no KNOWN shape matched', not 'no local detail is present' (eqoxide#995)."
  return 0
}

# ---------------------------------------------------------------------------------------------
# Commit mode: scan LOCAL git commit messages in a revision range for a negated closing keyword.
#
# This is the pre-push half of eqoxide#1041's surface 2. Live mode covers a pull request's commits
# from the API; this covers them before the pull request exists, and covers a range that is going
# straight to main (which no `pull_request`-triggered job ever sees). It scans ONLY for the negated
# closing keyword — the local-detail patterns are the tracked-file scanner's job on this surface.
#
# Deliberately NOT wired into the `test` CI job: every `actions/checkout` in this repo's workflow
# runs at the default fetch-depth of 1, so the runner has no branch history to walk and this mode
# there would scan an empty or one-commit range and call it clean. Changing that is a checkout
# change to the heaviest job in the workflow, not a one-liner, and the API path already covers the
# same commits for a pull request.
# ---------------------------------------------------------------------------------------------
run_commits() {
  local range="$1"
  require_negclose_patterns "a commit-message scan of ${range}" || return 1

  local dir manifest sha n=0
  dir="$(mktemp -d)"; SCRATCH_DIRS+=("$dir")
  manifest="${dir}/manifest.tsv"; : > "$manifest"

  if ! git rev-list "$range" > "${dir}/revs.txt" 2>"${dir}/revs.err"; then
    echo "::error::check-pr-text: 'git rev-list ${range}' FAILED — no commits were examined, so no"
    echo "verdict can be given. This is a range failure, not a clean result."
    sed 's/^/  /' "${dir}/revs.err" || true
    return 1
  fi

  while read -r sha; do
    [ -n "$sha" ] || continue
    n=$((n+1))
    git log -1 --format=%B "$sha" > "${dir}/commit-${sha}.txt"
    printf 'commit\t%s\t%s\n' "$sha" "${dir}/commit-${sha}.txt" >> "$manifest"
  done < "${dir}/revs.txt"

  # An empty range is an ERROR, not a pass, for the same reason an empty corpus is one in live mode:
  # "0 commits carried the pattern" and "0 commits were looked at" print the same word otherwise,
  # and the range is the single easiest thing to get wrong (a stale `origin/main`, a two-dot range
  # after main moved). If the range really is empty, that is worth knowing before it is called green.
  if [ "$n" -eq 0 ]; then
    echo "::error::check-pr-text: 0 commits in range '${range}' — nothing was examined."
    echo "This is a reach failure, not a clean result. Check the range (and whether origin/main is"
    echo "up to date in this checkout: a stale ref silently empties it)."
    return 1
  fi

  echo "check-pr-text --commits ${range}: ${n} commit message(s), ${#NEGCLOSE_NEGATIONS[@]} negation cues."
  classify_negclose "$manifest"
  echo "check-pr-text: negclose corpus = ${NEGCLOSE_TOTAL} commits; classified ${NEGCLOSE_CLASSIFIED}/${NEGCLOSE_TOTAL}; flagged ${NEGCLOSE_FLAGGED}."
  if [ "$NEGCLOSE_CLASSIFIED" -ne "$NEGCLOSE_TOTAL" ]; then
    echo "::error::check-pr-text: classified ${NEGCLOSE_CLASSIFIED} of ${NEGCLOSE_TOTAL} commits — the rest were skipped."
    return 1
  fi
  # Every item in this mode is a commit message, which is a surface GitHub links from, so nothing
  # here can land in the warn-only bucket. Asserted rather than assumed: if these ever disagree the
  # gating policy has drifted and a real finding would be downgraded to a warning in silence.
  if [ "$NEGCLOSE_FLAGGED" -ne "$NEGCLOSE_GATING" ]; then
    echo "::error::check-pr-text: ${NEGCLOSE_FLAGGED} flagged but only ${NEGCLOSE_GATING} gating in --commits mode, where every item is a commit message. That is a guard bug, not a result."
    return 1
  fi
  if [ "$NEGCLOSE_FLAGGED" -gt 0 ]; then
    echo "::error::check-pr-text: ${NEGCLOSE_FLAGGED} of ${NEGCLOSE_TOTAL} commit messages put a closing keyword next to an issue number they appear to disclaim."
    echo
    print_negclose_convention
    return 1
  fi
  echo "check-pr-text: OK — ${NEGCLOSE_TOTAL} commit messages, none with a negated closing keyword."
  echo "NOT covered by this mode: the squash-merge message GitHub composes at merge time (it does"
  echo "not exist yet), the pull request's body and comments (use the number mode for those), and"
  echo "the local-detail pattern set (that is check-no-local-detail.sh, on tracked files)."
  return 0
}

# ---------------------------------------------------------------------------------------------
# Self-test: drive the classifier through its positive, negative, reach and empty-corpus cases,
# and drive BOTH real scanners through their empty-pattern-list refusal by running them.
# Runs offline (no gh, no network). Same PATTERN as check-wrapped-literals.py --self-test: a guard
# that has stopped discriminating must fail loudly rather than pass everything.
# ---------------------------------------------------------------------------------------------
run_self_test() {
  require_patterns "the self-test" || return 1
  require_negclose_patterns "the self-test" || return 1

  local dir manifest checks=0 fails=0
  dir="$(mktemp -d)"; SCRATCH_DIRS+=("$dir")
  manifest="${dir}/manifest.tsv"; : > "$manifest"

  ok()   { checks=$((checks+1)); echo "  ok   $1"; }
  bad()  { checks=$((checks+1)); fails=$((fails+1)); echo "  FAIL $1"; }
  want() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (want '$3', got '$2')"; fi; }

  # One item per pattern that MUST flag. The samples live beside the patterns they exercise, in
  # scripts/local-detail-patterns.sh, so that adding a pattern without a sample is a length
  # mismatch here rather than a silent gap — and so this script itself stays inside the reach of
  # the tracked-file scanner instead of having to be excluded from it for containing them.
  want "one dirty sample per pattern" \
    "${#LOCAL_DETAIL_DIRTY_SAMPLES[@]}" "${#LOCAL_DETAIL_PATTERNS[@]}"

  local n=0 sample
  for sample in "${LOCAL_DETAIL_DIRTY_SAMPLES[@]}"; do
    n=$((n+1))
    printf '%s\n' "$sample" > "${dir}/dirty-${n}.txt"
    printf 'dirty\t%s\t%s\n' "$n" "${dir}/dirty-${n}.txt" >> "$manifest"
  done
  local dirty_count=$n

  # Clean items that MUST NOT flag, including near-misses that describe the shape without being it.
  printf '`git status --porcelain` in the shared checkout is empty — it was never touched\n' > "${dir}/clean-1.txt"
  printf 'gated in the fix-ci-honesty worktree; read the format spec, not a decompile\n'      > "${dir}/clean-2.txt"
  printf ''                                                                                   > "${dir}/clean-3.txt"
  printf 'clean\t1\t%s\nclean\t2\t%s\nclean\t3\t%s\n' \
    "${dir}/clean-1.txt" "${dir}/clean-2.txt" "${dir}/clean-3.txt" >> "$manifest"

  echo "check-pr-text --self-test: ${dirty_count} dirty + 3 clean items"
  classify_corpus "$manifest" > "${dir}/verdicts.txt"
  want "every item classified"        "$CORPUS_CLASSIFIED" "$CORPUS_TOTAL"
  want "corpus size"                  "$CORPUS_TOTAL"      "$((dirty_count + 3))"
  want "every dirty item flagged"     "$CORPUS_FLAGGED"    "$dirty_count"
  want "clean items not flagged"      "$(grep -c '^  \[clean\]' "${dir}/verdicts.txt")" "3"

  # The classified/total comparison must be able to FAIL. Feed the classifier a manifest naming a
  # file that does not exist: it is in the corpus (it came from the manifest) and cannot be looked
  # at, so total must exceed classified. If these two were counted in the same loop iteration this
  # check could not go red no matter what the classifier did.
  printf 'ghost\t1\t%s\n' "${dir}/does-not-exist.txt" > "${dir}/ghost-manifest.tsv"
  classify_corpus "${dir}/ghost-manifest.tsv" > /dev/null
  want "unreadable item counted, not classified" "${CORPUS_TOTAL}/${CORPUS_CLASSIFIED}" "1/0"

  # REACH CONTROL, not just a positive control. The counts above prove the flags fired; they do NOT
  # prove they fired BECAUSE the pattern list was applied. Empty the list and re-run the identical
  # dirty corpus: if anything still flags, the verdicts were not coming from the patterns.
  local saved=("${LOCAL_DETAIL_PATTERNS[@]}")
  LOCAL_DETAIL_PATTERNS=()
  classify_corpus "$manifest" > "${dir}/verdicts-nopat.txt"
  want "no pattern => nothing flags"  "$CORPUS_FLAGGED"    "0"
  want "no pattern => still classified all" "$CORPUS_CLASSIFIED" "$CORPUS_TOTAL"
  LOCAL_DETAIL_PATTERNS=("${saved[@]}")

  # ...and that empty-list state must be REFUSED by the real scanners rather than reported as OK,
  # since "0 findings" and "0 patterns applied" print the same word otherwise.
  #
  # These checks RUN THE SCRIPTS. An earlier version re-derived the refusal condition in an inline
  # `bash -c` that never invoked either scanner, so deleting a reach guard left this self-test at
  # full green — the assertion tested a copy of the guard, not the guard. Pointing the real
  # scanners at an empty pattern file via LOCAL_DETAIL_PATTERNS_FILE is what makes deleting either
  # guard turn this red.
  local empty_patterns="${dir}/empty-patterns.sh"
  printf 'LOCAL_DETAIL_PATTERNS=()\nLOCAL_DETAIL_DIRTY_SAMPLES=()\n' > "$empty_patterns"

  local out rc

  rc=0
  out="$(LOCAL_DETAIL_PATTERNS_FILE="$empty_patterns" \
         bash "${REPO_ROOT}/scripts/check-no-local-detail.sh" 2>&1)" || rc=$?
  want "check-no-local-detail refuses an empty pattern list" \
       "$(printf '%s' "$out" | grep -c 'pattern list is EMPTY' || true)" "1"
  want "check-no-local-detail exits 1 on an empty pattern list" "$rc" "1"

  # Live mode, not --self-test: the refusal has to fire before the first network call, so this runs
  # with a number and repo and must still fail offline without ever reaching `gh`.
  rc=0
  out="$(LOCAL_DETAIL_PATTERNS_FILE="$empty_patterns" \
         bash "${REPO_ROOT}/scripts/check-pr-text.sh" 1 --repo owner/name 2>&1)" || rc=$?
  want "check-pr-text refuses an empty pattern list" \
       "$(printf '%s' "$out" | grep -c 'pattern list is EMPTY' || true)" "1"
  want "check-pr-text exits 1 on an empty pattern list" "$rc" "1"
  want "check-pr-text refuses BEFORE any fetch" \
       "$(printf '%s' "$out" | grep -c 'scanning owner/name#1' || true)" "0"

  # An empty corpus must be an error, not a pass.
  : > "${dir}/empty-manifest.tsv"
  classify_corpus "${dir}/empty-manifest.tsv" > /dev/null
  want "empty corpus counts as 0 items" "$CORPUS_TOTAL" "0"

  # -------------------------------------------------------------------------------------------
  # eqoxide#1041 — NEGATED CLOSING KEYWORD.
  #
  # EVERY POSITIVE FIXTURE BELOW IS A MEASURED INSTANCE, QUOTED VERBATIM, not invented — with the
  # single, labelled exception noted on the last row. An invented fixture for this class would be
  # written by the same hand that failed to find three of these on the first sweep. Each RED line is
  # the exact text from the pull-request body or commit message that actually closed the issue named
  # beside it; each was verified to go red on this regex and green on the pre-#1041 script, which
  # had no such regex at all.
  #
  # Three of these are here BECAUSE the first sweep missed them, which is the point:
  #   - PR49 / issue 41 and PR421 / issue 378 were worded outside the negation cues that sweep used;
  #   - PR868 / issue 871 is the COLON VARIANT — `## Filed, not fixed: #871` — proof that the
  #     keyword may be followed by `:` and still link, and that "filed, not fixed" is exactly how an
  #     agent writes a negative scope decision.
  # They are the standing evidence that the cue list is a floor and not a closed set.
  # -------------------------------------------------------------------------------------------
  local nc_dir="${dir}/negclose"; mkdir -p "$nc_dir"
  local nc_red="${nc_dir}/red.tsv"; : > "$nc_red"
  local nc_green="${nc_dir}/green.tsv"; : > "$nc_green"
  local row label expect text

  # label | issue the sentence disclaims and GitHub closed anyway | the text
  local NC_RED_FIXTURES=(
    'PR49 body (issue 41, closed 49 days — MISSED by the first sweep)|41|## Caveat — why this doesn'"'"'t close #41'
    'PR54 body (issue 35, closed 48 days)|35|Refs #35, #2, #49 — partial (routing half of the multi-tier fix); does not close #35.'
    'PR421 body (issue 378, closed 34 days — MISSED by the first sweep)|378|This is **Phase 1** (partial — does **not** close #378). It collapses the height-axis half of the four mutually-blind nav predicates into one `Traversability` authority.'
    'PR868 body heading (issue 871, closed 12 days — COLON VARIANT, MISSED by the first sweep)|871|## Filed, not fixed: #871'
    'the same colon variant as running prose, the form the heading is short for|871|Filed, not fixed: #871 — the zone-in ordering bug is real but out of scope here.'
    'PR864 body (issue 854)|854|    capability drops 2.5 → 2.0; a 2.40 curb becomes impassable — and it still does not close #854,'
    'PR994 body (issue 982)|982|Does not close #982 (the `_cited` list is still a compile-only existence check with no reverse'
    'PR1037 body, quoted note (issue 939)|939|> **#939** ... "Landing this issue does not close #939; it narrows the window #939 has to'
    'PR1037 body, scope line (issue 939)|939|- **deliberately NOT Closes #939** — narrowed, not closed. Both zone-entry paths now publish, so'
    'PR1037 body, scope line (issue 1010)|1010|- **deliberately NOT Closes #1010** — untouched. Separate PR.'
    'commit 10619d99 message (issue 314, closed 37 days, COMMIT surface)|314|Scope note: this is the edge-SLIP class. It does NOT fix #314 (the North Qeynos'
    'PR752 body line 145 (issue 194, the one a HUMAN caught — in 1h 40m — and the one two
     successive counts then dropped for exactly that reason)|194|**Deliberately does not close #194.** Two gaps stay open, and the full re-verification of all three'
    'PR653 body (issue 302 — see the NEGATIVE note below)|302|- This does not close #302 or #254. It removes one documented contributor.'
    # THE WIDEST GAP IN THE SET, and the only fixture that exercises NEGCLOSE_GAP above 3.
    # Measured instance, quoted verbatim from comment 4849028657 on 49: the negation `n't` sits
    # 16 characters in front of `close #41` (' claim to fully '). Before this row the suite pinned
    # only `gap >= 3`, so the 40 that eqoxide#1041 specified could be narrowed to 3 — or widened to
    # 400 — with --self-test still fully green, while narrowing MEASURABLY disarmed the gating
    # surface on real repository text. This is a genuine disclaimer on a warn-only surface: it did
    # NOT cause a false close (issue 41 was closed by PR 49's BODY, which is a separate row above),
    # and it is here for its SHAPE, which is the wording the 40-character window was sized for.
    'comment 4849028657 on 49 (issue 41, gap 16 — WIDEST in the set; pins NEGCLOSE_GAP)|41|The PR is honest about its scope (doesn'"'"'t claim to fully close #41'
  )
  local n_red=0
  for row in "${NC_RED_FIXTURES[@]}"; do
    n_red=$((n_red+1))
    text="${row#*|*|}"
    printf '%s\n' "$text" > "${nc_dir}/red-${n_red}.txt"
    printf 'red\t%s\t%s\n' "$n_red" "${nc_dir}/red-${n_red}.txt" >> "$nc_red"
  done

  # MUST NOT FLAG. Four are real text from the same pull requests — including PR1037's real target,
  # which it wrote inside backticks and GitHub did NOT link, and PR653's and PR994's legitimate
  # closes. The last three are this repo's conventional subject line, verbatim from PRs 62, 86 and
  # 89: a clause containing a negation, an em dash, then a perfectly legitimate `Fixes #N`. They are
  # measured false positives of the pre-em-dash gap class and they are the assertion that keeps `—`
  # in NEGCLOSE_GAP_STOPS — delete it from the stop set and these three go red.
  # The rest are the boundary cases the regex has to survive: `Notes:`, `Another` and
  # `annotation` all contain the letters "not", `unfixed` contains "fix", and a negation in the
  # PREVIOUS sentence must not poison the next sentence's legitimate close.
  local NC_GREEN_FIXTURES=(
    '- `Closes #1022`'
    'Fixes #641. Rebased on `8129376`. Second revision — all four blocking findings from the independent review addressed.'
    'Closes #990'
    'Refs #35, #2, #49 — partial (routing half of the multi-tier fix)'
    'Refs #939 — narrowed, not closed. Both zone-entry paths now publish.'
    'Notes: this closes #7'
    'Another change here. Fixes #501.'
    'This PR does not touch the renderer. Fixes #500.'
    'annotation cleanup, fixes #9'
    'The prefix handling is unfixed; resolves #12'
    'fix(inventory): auto-loot into a free slot + stack, never overwrite occupied slots — Fixes #56'
    'fix(convert): W=0 quaternion frames are a 180° rotation, not identity (wolf rear inverted) — Fixes #40'
    'fix(render): hold sit/kneel poses instead of looping the transition — Fixes #83'
    # ONE PER REMAINING GAP STOP, so that every character in NEGCLOSE_GAP_STOPS is pinned by a
    # fixture rather than by its rationale alone. Each goes RED if that one character is dropped
    # from the set. `#` is the one the comment above singles out with its own reasoning — the
    # window must not leap over an intervening issue reference and attribute a disclaimer about
    # one number to the next number in the sentence — and it was the least guarded of the six.
    'does not affect #12 and closes #34'
    'This is not in scope; Closes #77'
    'Not this one! Closes #88'
    'Why not this one? Closes #99'
  )
  local n_green=0
  for text in "${NC_GREEN_FIXTURES[@]}"; do
    n_green=$((n_green+1))
    printf '%s\n' "$text" > "${nc_dir}/green-${n_green}.txt"
    printf 'green\t%s\t%s\n' "$n_green" "${nc_dir}/green-${n_green}.txt" >> "$nc_green"
  done

  echo "check-pr-text --self-test: eqoxide#1041 — ${n_red} measured-instance items + ${n_green} must-stay-green items"
  classify_negclose "$nc_red" > "${nc_dir}/red-verdicts.txt"
  want "every measured instance flagged"        "$NEGCLOSE_FLAGGED"    "$n_red"
  want "every measured instance classified"     "$NEGCLOSE_CLASSIFIED" "$NEGCLOSE_TOTAL"
  # The finding is only useful if it names the RIGHT number — a regex that flagged the line but
  # reported the wrong issue would still pass a count-only assertion.
  want "the disclaimed numbers are extracted" \
    "$(printf '%s' "$NEGCLOSE_NUMBERS" | tr ' ' '\n' | grep -E '^[0-9]+$' | sort -un | tr '\n' ' ')" \
    "35 41 194 302 314 378 854 871 939 982 1010 "

  # The gating policy as a unit, independent of any corpus: measured behaviour is that only a pull
  # request's body and a commit message create a link, so only those may decide an exit code.
  CORPUS_IS_PR=yes
  negclose_surface_gates commit  && want "policy: a commit message gates"            "yes" "yes" \
                                 || want "policy: a commit message gates"            "no"  "yes"
  negclose_surface_gates body    && want "policy: a PULL REQUEST body gates"         "yes" "yes" \
                                 || want "policy: a PULL REQUEST body gates"         "no"  "yes"
  negclose_surface_gates comment && want "policy: a comment does NOT gate"           "yes" "no"  \
                                 || want "policy: a comment does NOT gate"           "no"  "no"
  CORPUS_IS_PR=no
  negclose_surface_gates body    && want "policy: an ISSUE body does NOT gate"       "yes" "no"  \
                                 || want "policy: an ISSUE body does NOT gate"       "no"  "no"
  CORPUS_IS_PR=no

  classify_negclose "$nc_green" > "${nc_dir}/green-verdicts.txt"
  want "no must-stay-green item flagged"        "$NEGCLOSE_FLAGGED"    "0"
  want "every must-stay-green item classified"  "$NEGCLOSE_CLASSIFIED" "$n_green"

  # REACH CONTROL, twice, one per half of the pattern. The counts above prove the flags fired; they
  # do NOT prove they fired BECAUSE the negation list and the keyword list were applied. Empty each
  # in turn and re-run the identical red corpus: if anything still flags, the verdicts were not
  # coming from the patterns. Every item must still be CLASSIFIED both times — "nothing flagged"
  # and "nothing looked at" have to stay distinguishable.
  local nc_saved_neg=("${NEGCLOSE_NEGATIONS[@]}") nc_saved_kw="$NEGCLOSE_KEYWORDS"
  NEGCLOSE_NEGATIONS=()
  classify_negclose "$nc_red" > /dev/null
  want "no negation cue => nothing flags"       "$NEGCLOSE_FLAGGED"    "0"
  want "no negation cue => still classified all" "$NEGCLOSE_CLASSIFIED" "$NEGCLOSE_TOTAL"
  NEGCLOSE_NEGATIONS=("${nc_saved_neg[@]}")
  NEGCLOSE_KEYWORDS=""
  classify_negclose "$nc_red" > /dev/null
  want "no closing keyword => nothing flags"    "$NEGCLOSE_FLAGGED"    "0"
  NEGCLOSE_KEYWORDS="$nc_saved_kw"

  # ...and that empty-list state must be REFUSED by the REAL script rather than reported as no
  # findings. Same indirection as the LOCAL_DETAIL_PATTERNS_FILE checks above and for the same
  # reason: this must run the script, not a copy of its condition.
  local empty_neg="${nc_dir}/empty-negclose.sh"
  printf 'NEGCLOSE_NEGATIONS=()\nNEGCLOSE_KEYWORDS=""\n' > "$empty_neg"
  rc=0
  out="$(NEGCLOSE_PATTERNS_FILE="$empty_neg" \
         bash "${REPO_ROOT}/scripts/check-pr-text.sh" 1 --repo owner/name 2>&1)" || rc=$?
  want "check-pr-text refuses an empty negation list" \
       "$(printf '%s' "$out" | grep -c 'negation or keyword list is EMPTY' || true)" "1"
  want "check-pr-text exits 1 on an empty negation list" "$rc" "1"
  want "the negclose refusal fires BEFORE any fetch" \
       "$(printf '%s' "$out" | grep -c 'scanning owner/name#1' || true)" "0"

  # -------------------------------------------------------------------------------------------
  # ATTRIBUTION — the second, separate question, and the home of the #302 NEGATIVE FIXTURE.
  #
  # Every row is measured: the close-event timestamps and commit ids come from each issue's
  # timeline and each pull request's `merged_at`, read from the API during the eqoxide#1041 sweep.
  # Every confirmed false close lands 1-2 seconds after its PR merged. Issue 302's close landed at
  # 2026-07-11T20:18:54Z, ONE SECOND after PR #306 merged and 10d 18h 16m BEFORE PR #653 was even
  # opened (2026-07-22T14:35:40Z) — PR #306 closed it, legitimately — so the
  # classifier must return NOT-ATTRIBUTED there even though PR #653's text matched the lint above
  # and even though GitHub does list 302 in that PR's closingIssuesReferences. That is the whole
  # point of keeping the two checks apart: matching the pattern is not a false close.
  # -------------------------------------------------------------------------------------------
  local a_label a_close a_commit a_merged a_range a_expect
  local NC_ATTRIBUTION=(
    'issue 35 <- PR 54|2026-07-01T00:43:38Z|null|2026-07-01T00:43:36Z||ATTRIBUTED-LINK'
    'issue 854 <- PR 864|2026-08-06T04:25:57Z|null|2026-08-06T04:25:56Z||ATTRIBUTED-LINK'
    'issue 982 <- PR 994|2026-08-12T21:51:34Z|null|2026-08-12T21:51:33Z||ATTRIBUTED-LINK'
    'issue 939 <- PR 1037|2026-08-18T23:36:58Z|null|2026-08-18T23:36:57Z||ATTRIBUTED-LINK'
    'issue 1010 <- PR 1037|2026-08-18T23:36:59Z|null|2026-08-18T23:36:57Z||ATTRIBUTED-LINK'
    'issue 314 <- its closing commit|2026-07-12T18:18:53Z|10619d99dab87aa6fe57503fcec17d5c416254a8||10619d99dab87aa6fe57503fcec17d5c416254a8|ATTRIBUTED-COMMIT'
    'issue 314 vs a range without that commit|2026-07-12T18:18:53Z|10619d99dab87aa6fe57503fcec17d5c416254a8||bb15748000000000000000000000000000000000|NOT-ATTRIBUTED'
    # The range test is EXACT-TOKEN membership, and the space padding around both sides of the
    # `case` glob is what makes it exact. Without this row the padding is unpinned: dropping it
    # (`*" ${close_commit} "*` -> `*"${close_commit}"*`) was MEASURED to leave --self-test at full
    # green, because every other row here compares a full 40-char sha against a range of full
    # 40-char shas, where substring and exact membership cannot disagree. An ABBREVIATED sha is
    # where they disagree, so that is what this row feeds. NOT-ATTRIBUTED is the correct verdict
    # and the safe direction: both real inputs are full shas (the API's `commit_id`, and
    # `git log --format=%H`), so an abbreviated one is out-of-contract input that must fail to
    # match rather than silently match a commit it merely prefixes.
    'an ABBREVIATED sha must not match a range by substring (pins the space padding)|2026-07-12T18:18:53Z|10619d99||10619d99dab87aa6fe57503fcec17d5c416254a8|NOT-ATTRIBUTED'
    'issue 302 <- PR 653 (NEGATIVE FIXTURE)|2026-07-11T20:18:54Z|null|2026-07-22T22:31:39Z||NOT-ATTRIBUTED'
    'a disclaimed issue that is still open|||2026-07-22T22:31:39Z||STILL-OPEN'
    'a disclaimed issue closed while the PR is unmerged|2026-08-01T00:00:00Z|null|||NOT-ATTRIBUTED'
  )
  for row in "${NC_ATTRIBUTION[@]}"; do
    IFS='|' read -r a_label a_close a_commit a_merged a_range a_expect <<< "$row"
    want "attribution: ${a_label}" \
      "$(negclose_attribution "$a_close" "$a_commit" "$a_merged" "$a_range")" "$a_expect"
  done

  # -------------------------------------------------------------------------------------------
  # run_live, END TO END, OFFLINE. Everything above drives classify_negclose directly, which does
  # not prove run_live CALLS it — and that gap was measured on this branch, not assumed: deleting
  # the `classify_negclose "$manifest"` line from run_live left --self-test at full green, exactly
  # the failure mode this file's header already records for the empty-pattern checks ("the
  # assertion tested a copy of the guard, not the guard").
  #
  # The fix is the same one that worked there: RUN THE SCRIPT. A stub `gh` is placed first on PATH
  # and serves recorded payloads for PR #1037 — the pull request whose body, whose comment and whose
  # commit message each carried the defect, and whose closingIssuesReferences GitHub really did
  # report as exactly the two issues that body disclaimed. The stub emits already-filtered output
  # because `gh` applies `--jq` itself. STUB_MODE=clean serves the corrected text through the same
  # code path, so the run can be shown to go GREEN as well as RED — a check that only ever goes one
  # way is not evidence.
  #
  # One caveat about reading these checks, so nobody leans on the wrong one. Under the mutation
  # above, `run_live exits 1 on a negated closing keyword` still passes — but for the wrong reason:
  # with classify_negclose gone, the child dies on an unbound NEGCLOSE_TOTAL under `set -u`, and a
  # dead process also exits non-zero. It is the PAIRED clean-corpus check below it — `run_live exits
  # 0 once the same text is rewritten` — that actually catches the deletion, because a crash cannot
  # produce exit 0. An exit-code assertion is only evidence when the same corpus is run BOTH ways.
  # -------------------------------------------------------------------------------------------
  local stubdir="${nc_dir}/stub"; mkdir -p "$stubdir"
  cat > "${stubdir}/gh" <<'GHSTUB'
#!/usr/bin/env bash
# Offline stand-in for `gh api`, driven by check-pr-text.sh --self-test. Recorded from
# djhenry/eqoxide#1037. Not a general gh: it answers exactly the calls run_live makes.
set -euo pipefail
if [ "${STUB_MODE:-dirty}" = "warnonly" ]; then
  # The case B2 was hiding in: the ONLY finding is on a surface GitHub does not link from, so the
  # run does not gate and reaches the final summary line with a flagged item in hand.
  BODY='- Closes #1022
- #939 stays open — narrowed, not closed. Both zone-entry paths now publish.
- #1010 is untouched. Separate PR.'
  COMMENT='Landing this issue does not close #939; it narrows the window #939 has to fire.'
  CMSG='fix(#1022): publish the door roster from the LOGIN handshake own drain too

Scope: #939 stays open (narrowed, not closed) and #1010 is untouched.'
elif [ "${STUB_MODE:-dirty}" = "clean" ]; then
  BODY='- Closes #1022
- #939 stays open — narrowed, not closed. Both zone-entry paths now publish.
- #1010 is untouched. Separate PR.'
  COMMENT='Landing this issue narrows the window #939 has to fire; #939 stays open.'
  CMSG='fix(#1022): publish the door roster from the LOGIN handshake own drain too

Scope: #939 stays open (narrowed, not closed) and #1010 is untouched.'
else
  BODY='- `Closes #1022`
- **deliberately NOT Closes #939** — narrowed, not closed. Both zone-entry paths now publish, so
- **deliberately NOT Closes #1010** — untouched. Separate PR.'
  COMMENT='Landing this issue does not close #939; it narrows the window #939 has to fire.'
  CMSG='fix(#1022): publish the door roster from the LOGIN handshake own drain too

- **deliberately NOT Closes #1010** — untouched. Separate PR.'
fi
b64() { printf '%s' "$1" | base64 | tr -d '\n'; }
args="$*"
# `gh api "$@"` puts the subcommand first, so every call arrives as `api <endpoint-or-graphql> ...`.
case "$args" in
  "api graphql"*)
    printf '%s\n' '{"data":{"repository":{"pullRequest":{"merged":true,"mergedAt":"2026-08-18T23:36:57Z","closingIssuesReferences":{"nodes":[{"number":939,"state":"CLOSED"},{"number":1010,"state":"CLOSED"}]}}}}}' ;;
  *"/issues/939/timeline"*)  printf '2026-08-18T23:36:58Z\tnull\n' ;;
  *"/issues/1010/timeline"*) printf '2026-08-18T23:36:59Z\tnull\n' ;;
  *"/issues/1037/comments"*) printf 'comment\t5335451007\t%s\n' "$(b64 "$COMMENT")" ;;
  *"/pulls/1037/reviews"*)   : ;;
  *"/pulls/1037/comments"*)  : ;;
  *"/pulls/1037/commits"*)   printf 'commit\t15b6630057fd99b75b813f254aa347f6c4d9f594\t%s\n' "$(b64 "$CMSG")" ;;
  *"/issues/1037")           jq -n --arg b "$BODY" '{number:1037,title:"fix(#1022): publish the door roster",body:$b,pull_request:{}}' ;;
  *) echo "stub gh: unhandled call: $args" >&2; exit 4 ;;
esac
GHSTUB
  chmod +x "${stubdir}/gh"

  rc=0
  out="$(PATH="${stubdir}:${PATH}" STUB_MODE=dirty \
         bash "${REPO_ROOT}/scripts/check-pr-text.sh" 1037 --repo owner/name 2>&1)" || rc=$?
  want "run_live exits 1 on a negated closing keyword" "$rc" "1"
  want "run_live flags it on ALL THREE surfaces (body, comment, commit message)" \
       "$(printf '%s' "$out" | grep -cE '^  \[(NEGCLOSE\]|negclose-warn\])' || true)" "3"
  # The gating policy, asserted on a real run rather than inferred from the exit code: the two
  # surfaces GitHub links from gate, the one it does not link from warns. Getting this backwards
  # would either block every review thread that discusses this defect class or let a real false
  # close through as a warning.
  want "only the two LINKING surfaces gate" \
       "$(printf '%s' "$out" | grep -c '^  \[NEGCLOSE\]' || true)" "2"
  want "the non-linking surface warns instead of gating" \
       "$(printf '%s' "$out" | grep -c '^  \[negclose-warn\]' || true)" "1"
  # Three assertions, not one, because the per-item verdict has to be TRUE per surface and not just
  # loud. GitHub links from a pull request's body and from commit messages; it does not link from a
  # comment. A guard that told an operator their comment was about to close an issue would be making
  # the same class of false statement this whole check exists to catch.
  want "run_live says a PR BODY will close it, twice" \
       "$(printf '%s' "$out" | grep -cE 'will CLOSE issue (939|1010) on merge' || true)" "2"
  want "run_live does NOT claim a COMMENT closes anything" \
       "$(printf '%s' "$out" | grep -c 'A comment does not link (0 of 80 measured here), so issue 939 does' || true)" "1"
  want "run_live says a COMMIT closes on reaching the default branch" \
       "$(printf '%s' "$out" | grep -c 'GitHub closes issue 1010 when this commit reaches the default branch' || true)" "1"
  want "run_live prints what GitHub actually parsed" \
       "$(printf '%s' "$out" | grep -c 'closingIssuesReferences for this pull request: 939 1010' || true)" "1"
  want "run_live reports the close attribution" \
       "$(printf '%s' "$out" | grep -c 'close attribution: ATTRIBUTED-LINK' || true)" "2"
  want "run_live scanned the commit surface at all" \
       "$(printf '%s' "$out" | grep -c '^  \[NEGCLOSE\]    commit#15b6630057fd99b75b813f254aa347f6c4d9f594$' || true)" "1"
  # The local-detail half must still be running in the same invocation — one pass must not be able
  # to short-circuit the other.
  want "run_live still ran the local-detail pass too" \
       "$(printf '%s' "$out" | grep -c 'check-pr-text: corpus = 4 items; classified 4/4' || true)" "1"

  rc=0
  out="$(PATH="${stubdir}:${PATH}" STUB_MODE=clean \
         bash "${REPO_ROOT}/scripts/check-pr-text.sh" 1037 --repo owner/name 2>&1)" || rc=$?
  want "run_live exits 0 once the same text is rewritten" "$rc" "0"
  want "run_live reports the rewritten corpus clean on both passes" \
       "$(printf '%s' "$out" | grep -c 'clean AS CURRENTLY WRITTEN, on both passes' || true)" "1"
  want "the rewritten text really went through the negclose pass" \
       "$(printf '%s' "$out" | grep -c 'negclose corpus = 4 items; classified 4/4; flagged 0' || true)" "1"

  # -------------------------------------------------------------------------------------------
  # The WARN-ONLY run: a finding on a surface GitHub does not link from. It does not gate, so the
  # run exits 0 and reaches the final summary — which is exactly where this script spent a revision
  # printing "OK — all N items clean ... on both passes" over a flagged item. Nothing in the 60
  # checks the suite THEN CONTAINED could see that (the number is that revision's, not this one's),
  # because both other modes either gate or have nothing to report. The
  # summary text is asserted directly here for that reason: the exit code was never the bug.
  # -------------------------------------------------------------------------------------------
  rc=0
  out="$(PATH="${stubdir}:${PATH}" STUB_MODE=warnonly \
         bash "${REPO_ROOT}/scripts/check-pr-text.sh" 1037 --repo owner/name 2>&1)" || rc=$?
  want "a warn-only finding does not gate" "$rc" "0"
  want "warn-only run flags exactly one item, on the non-linking surface" \
       "$(printf '%s' "$out" | grep -c '^  \[negclose-warn\]' || true)" "1"
  want "warn-only run gates on nothing" \
       "$(printf '%s' "$out" | grep -c '^  \[NEGCLOSE\]' || true)" "0"
  want "warn-only run counts it as flagged-but-not-gating" \
       "$(printf '%s' "$out" | grep -c 'flagged 1 (0 on a surface that can actually close)' || true)" "1"
  want "warn-only run does NOT call the corpus clean" \
       "$(printf '%s' "$out" | grep -c 'clean AS CURRENTLY WRITTEN, on both passes' || true)" "0"
  want "warn-only run says what actually happened" \
       "$(printf '%s' "$out" | grep -c 'NOT CLEAN, but not gating' || true)" "1"

  # -------------------------------------------------------------------------------------------
  # --commits mode, end to end, on a throwaway repository built here. This is the surface issue 314
  # died on, and running the classifier alone would not prove the mode WIRES it up — the range
  # walk, the per-commit message extraction and the exit code are all only exercised by running it.
  # The dirty commit carries the measured text; the repo is created in a temp dir and removed by the
  # EXIT trap, so nothing is ever pushed anywhere.
  # -------------------------------------------------------------------------------------------
  local trepo="${nc_dir}/gitrepo"
  mkdir -p "$trepo"
  git -c init.defaultBranch=main init -q "$trepo"
  git -C "$trepo" -c user.email=selftest@invalid -c user.name=selftest \
      commit -q --allow-empty -m 'fix(nav): a clean scope note — Refs #310, not addressed here'
  git -C "$trepo" -c user.email=selftest@invalid -c user.name=selftest \
      commit -q --allow-empty -m 'Scope note: this is the edge-SLIP class. It does NOT fix #314 (the North Qeynos'
  git -C "$trepo" -c user.email=selftest@invalid -c user.name=selftest \
      commit -q --allow-empty -m 'docs: restate the scope note without a linking keyword — #314 stays open'

  rc=0
  out="$(cd "$trepo" && LOCAL_DETAIL_PATTERNS_FILE="$PATTERNS_FILE" \
         bash "${REPO_ROOT}/scripts/check-pr-text.sh" --commits HEAD~2..HEAD~1 2>&1)" || rc=$?
  want "--commits flags the measured commit-message instance" \
       "$(printf '%s' "$out" | grep -c 'GitHub closes issue 314 when this commit reaches the default branch' || true)" "1"
  want "--commits exits 1 on a finding" "$rc" "1"

  rc=0
  out="$(cd "$trepo" && LOCAL_DETAIL_PATTERNS_FILE="$PATTERNS_FILE" \
         bash "${REPO_ROOT}/scripts/check-pr-text.sh" --commits HEAD~1..HEAD 2>&1)" || rc=$?
  want "--commits passes a clean commit range" "$rc" "0"
  want "--commits classified the clean commit" \
       "$(printf '%s' "$out" | grep -c 'classified 1/1' || true)" "1"

  # An empty range must be an ERROR, not a pass: "0 commits carried the pattern" and "0 commits were
  # looked at" print the same word otherwise, and a stale `origin/main` in a worktree is the easiest
  # way in this fleet to hand it an empty range without noticing.
  rc=0
  out="$(cd "$trepo" && LOCAL_DETAIL_PATTERNS_FILE="$PATTERNS_FILE" \
         bash "${REPO_ROOT}/scripts/check-pr-text.sh" --commits HEAD..HEAD 2>&1)" || rc=$?
  want "--commits refuses an empty range" \
       "$(printf '%s' "$out" | grep -c '0 commits in range' || true)" "1"
  want "--commits exits 1 on an empty range" "$rc" "1"

  # Assert how many checks ran. A case that silently stops running must fail this step rather than
  # shrink the output. `checks + 1` counts this assertion itself, which has not been tallied yet.
  want "self-test ran every check (incl. this one)" "$((checks + 1))" "67"

  if [ "$fails" -ne 0 ]; then
    echo "check-pr-text --self-test: FAILED ${fails} of ${checks} checks."
    return 1
  fi
  echo "check-pr-text --self-test: OK — ${checks} checks, ${#LOCAL_DETAIL_PATTERNS[@]} patterns exercised."
  return 0
}

# ---------------------------------------------------------------------------------------------
main() {
  local number="" repo="${GH_REPO:-}"
  while [ $# -gt 0 ]; do
    case "$1" in
      --self-test) run_self_test; exit $? ;;
      --commits)   run_commits "$2"; exit $? ;;
      --repo)      repo="$2"; shift 2 ;;
      # Print the leading comment block, however long it grows. A fixed line range here rots into
      # a truncated help text the first time the header is edited.
      -h|--help)   awk 'NR>1 && !/^#/{exit} {print}' "$0"; exit 0 ;;
      *)           number="$1"; shift ;;
    esac
  done
  if [ -z "$number" ]; then
    echo "usage: scripts/check-pr-text.sh <pr-or-issue-number> [--repo <owner>/<name>]" >&2
    echo "       scripts/check-pr-text.sh --commits <rev-range>" >&2
    echo "       scripts/check-pr-text.sh --self-test" >&2
    exit 2
  fi
  if [ -z "$repo" ]; then
    repo="$(gh repo view --json nameWithOwner --jq .nameWithOwner)"
  fi
  run_live "$number" "$repo"
}

main "$@"
