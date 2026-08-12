#!/usr/bin/env bash
#
# check-pr-text.sh — scan the text a PR or issue publishes (title, body, and every comment) for the
# same local-system / proprietary-derived patterns that `check-no-local-detail.sh` keeps out of
# tracked files.
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
#           scripts/check-pr-text.sh --self-test
#
#   exit 0 = every item in the corpus was classified and none matched
#   exit 1 = a pattern matched, or the corpus could not be established (see REACH below)
#
# REACH — what this run did and did not look at. Read this before quoting a green result.
#   COVERED: the CURRENT text of, for one number: the title, the body, every issue-style comment,
#            and (for a PR) every review body and every inline review comment.
#   NOT COVERED: the edit history of any of those (the first version is the one that leaked);
#            commit messages and branch names; the bodies of OTHER issues/PRs this one links to;
#            releases, wiki, gists, and the Actions logs; and anything published after this run.
#   NOT COVERED BY CI, specifically: the workflow that calls this script is triggered by
#            `pull_request`, so in CI it only ever reads the text as it stood at the last PUSH.
#            A comment posted after the final push — including a review comment, and including the
#            comment that reports this scan — is never scanned by any automated run. Run this
#            script by hand against the number to cover them.
#   The script prints its corpus size and classifies EVERY item, printing one verdict line each,
#   because a checker that prints only exceptions cannot tell "nothing wrong" from "nothing looked
#   at". An empty corpus is an ERROR here, not a pass; so is an empty pattern list.
#
#   THE PATTERN SET IS AN ALLOWLIST. Even a fully-classified, fully-covered corpus is only checked
#   against shapes someone already thought of, so a green run means "no KNOWN shape matched", never
#   "no local detail is present". eqoxide#995 is the standing proof, and it is an OPEN gap rather
#   than a closed one: a tracked comment naming deployment infrastructure sat green under the
#   sibling scanner for months. That comment is scrubbed; the class itself is still not covered by
#   either scanner, because a host name has no shape and enumerating literals would put the value
#   in a tracked file. See #995.
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

  local is_pr
  is_pr="$(jq -r 'if .pull_request then "yes" else "no" end' < "$issue_json")"
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

    echo "check-pr-text: number ${number} is a PULL REQUEST (issues/${number} carries .pull_request)"
    echo "               — review bodies and inline review comments included."
  else
    echo "check-pr-text: number ${number} is an ISSUE (issues/${number} has no .pull_request);"
    echo "               there are no pull-request surfaces to fetch."
  fi
}

run_live() {
  local number="$1" repo="$2"
  require_patterns "a live scan of ${repo}#${number}" || return 1

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
  if [ "$CORPUS_CLASSIFIED" -ne "$CORPUS_TOTAL" ]; then
    echo "::error::check-pr-text: ${CORPUS_CLASSIFIED} of ${CORPUS_TOTAL} items were classified — the rest were skipped."
    return 1
  fi

  if [ "$CORPUS_FLAGGED" -gt 0 ]; then
    echo "::error::check-pr-text: ${CORPUS_FLAGGED} of ${CORPUS_TOTAL} items carry local-system or proprietary-derived detail."
    echo
    print_convention
    echo
    echo "DETECTOR, NOT PREVENTER: this text is already published and GitHub keeps the edit"
    echo "history. Editing it now limits further exposure; it does not un-publish it."
    return 1
  fi

  echo "check-pr-text: OK — all ${CORPUS_TOTAL} items clean AS CURRENTLY WRITTEN."
  echo "check-pr-text: this is a DETECTOR, not a preventer. It reads the CURRENT text only, so a"
  echo "green run means 'nothing is exposed now', NOT 'nothing was ever exposed' — an earlier"
  echo "revision of any of these items may have carried detail that is still in the edit history."
  echo "Not covered: edit history, commit messages, linked issues/PRs, releases, wiki, CI logs —"
  echo "and, when run from the pull_request workflow, any comment posted after the last push,"
  echo "which includes every review comment on the final revision."
  echo "The pattern set is an ALLOWLIST of shapes someone already thought of, so this reads as"
  echo "'no KNOWN shape matched', not 'no local detail is present' (eqoxide#995)."
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

  # Assert how many checks ran. A case that silently stops running must fail this step rather than
  # shrink the output. `checks + 1` counts this assertion itself, which has not been tallied yet.
  want "self-test ran every check (incl. this one)" "$((checks + 1))" "15"

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
      --repo)      repo="$2"; shift 2 ;;
      -h|--help)   sed -n '1,45p' "$0"; exit 0 ;;
      *)           number="$1"; shift ;;
    esac
  done
  if [ -z "$number" ]; then
    echo "usage: scripts/check-pr-text.sh <pr-or-issue-number> [--repo <owner>/<name>]" >&2
    echo "       scripts/check-pr-text.sh --self-test" >&2
    exit 2
  fi
  if [ -z "$repo" ]; then
    repo="$(gh repo view --json nameWithOwner --jq .nameWithOwner)"
  fi
  run_live "$number" "$repo"
}

main "$@"
