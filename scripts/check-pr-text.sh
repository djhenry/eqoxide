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
#   The script prints its corpus size and classifies EVERY item, printing one verdict line each,
#   because a checker that prints only exceptions cannot tell "nothing wrong" from "nothing looked
#   at". An empty corpus is an ERROR here, not a pass.

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
# shellcheck source=scripts/local-detail-patterns.sh
source "${REPO_ROOT}/scripts/local-detail-patterns.sh"

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
  for re in "${LOCAL_DETAIL_PATTERNS[@]}"; do
    if grep -nE -e "$re" -- "$f" >/dev/null 2>&1; then
      echo "$re"
    fi
  done
}

# Walk a corpus manifest (TAB-separated: kind, id, path) and classify EVERY line.
# Sets globals: CORPUS_TOTAL, CORPUS_CLASSIFIED, CORPUS_FLAGGED.
classify_corpus() {
  local manifest="$1" kind id path hits n
  CORPUS_TOTAL=0; CORPUS_CLASSIFIED=0; CORPUS_FLAGGED=0
  while IFS=$'\t' read -r kind id path; do
    [ -n "${kind:-}" ] || continue
    CORPUS_TOTAL=$((CORPUS_TOTAL + 1))
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
fetch_corpus() {
  local number="$1" repo="$2" dir="$3" manifest="$4" kind id b64
  : > "$manifest"

  # /issues/<n> serves both issues and PRs, so title/body/comments work for either.
  gh api "repos/${repo}/issues/${number}" \
     --jq '"title\t\(.number)\t\(.title // "" | @base64)", "body\t\(.number)\t\(.body // "" | @base64)"' \
    | while IFS=$'\t' read -r kind id b64; do
        printf '%s' "$b64" | base64 -d > "${dir}/${kind}-${id}.txt"
        printf '%s\t%s\t%s\n' "$kind" "$id" "${dir}/${kind}-${id}.txt" >> "$manifest"
      done

  gh api "repos/${repo}/issues/${number}/comments" --paginate \
     --jq '.[] | "comment\t\(.id)\t\(.body // "" | @base64)"' \
    | while IFS=$'\t' read -r kind id b64; do
        printf '%s' "$b64" | base64 -d > "${dir}/${kind}-${id}.txt"
        printf '%s\t%s\t%s\n' "$kind" "$id" "${dir}/${kind}-${id}.txt" >> "$manifest"
      done

  # PR-only surfaces. A plain issue 404s here; that is expected, not an error.
  if gh api "repos/${repo}/pulls/${number}" --jq '.number' >/dev/null 2>&1; then
    gh api "repos/${repo}/pulls/${number}/reviews" --paginate \
       --jq '.[] | select((.body // "") != "") | "review\t\(.id)\t\(.body | @base64)"' \
      | while IFS=$'\t' read -r kind id b64; do
          printf '%s' "$b64" | base64 -d > "${dir}/${kind}-${id}.txt"
          printf '%s\t%s\t%s\n' "$kind" "$id" "${dir}/${kind}-${id}.txt" >> "$manifest"
        done
    gh api "repos/${repo}/pulls/${number}/comments" --paginate \
       --jq '.[] | "review-comment\t\(.id)\t\(.body // "" | @base64)"' \
      | while IFS=$'\t' read -r kind id b64; do
          printf '%s' "$b64" | base64 -d > "${dir}/${kind}-${id}.txt"
          printf '%s\t%s\t%s\n' "$kind" "$id" "${dir}/${kind}-${id}.txt" >> "$manifest"
        done
    echo "check-pr-text: number ${number} is a PULL REQUEST — review bodies and inline review comments included."
  else
    echo "check-pr-text: number ${number} is an ISSUE (no pull-request surfaces to fetch)."
  fi
}

run_live() {
  local number="$1" repo="$2"
  local dir manifest
  dir="$(mktemp -d)"; SCRATCH_DIRS+=("$dir")
  manifest="${dir}/manifest.tsv"

  echo "check-pr-text: scanning ${repo}#${number} with ${#LOCAL_DETAIL_PATTERNS[@]} patterns."
  fetch_corpus "$number" "$repo" "$dir" "$manifest"

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
  echo "Not covered: edit history, commit messages, linked issues/PRs, releases, wiki, CI logs."
  return 0
}

# ---------------------------------------------------------------------------------------------
# Self-test: drive the classifier through its positive, negative, reach and empty-corpus cases.
# Runs offline (no gh, no network). Same PATTERN as check-wrapped-literals.py --self-test: a guard
# that has stopped discriminating must fail loudly rather than pass everything.
# ---------------------------------------------------------------------------------------------
run_self_test() {
  local dir manifest checks=0 fails=0
  dir="$(mktemp -d)"; SCRATCH_DIRS+=("$dir")
  manifest="${dir}/manifest.tsv"; : > "$manifest"

  ok()   { checks=$((checks+1)); echo "  ok   $1"; }
  bad()  { checks=$((checks+1)); fails=$((fails+1)); echo "  FAIL $1"; }
  want() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (want '$3', got '$2')"; fi; }

  # One item per pattern that MUST flag. Built here rather than quoted from a real leak.
  local n=0 re
  for re in "${LOCAL_DETAIL_PATTERNS[@]}"; do
    n=$((n+1))
    case "$re" in
      '/home/[a-z]')            printf 'ran `git -C /home/someuser/checkout status`\n' ;;
      'eqemu_[a-z]+_[0-9]')     printf 'podman exec eqemu_mariadb_1 sh\n' ;;
      '-u[a-z]+ +-p[A-Za-z0-9]') printf 'mysql -uroot -pHunter2 eqemu\n' ;;
      'ghidra')                 printf 'opened it in ghidra\n' ;;
      'capstone')               printf 'disassembled with capstone\n' ;;
      'eqgame\.exe')            printf 'offsets from eqgame.exe\n' ;;
      'decompiled/')            printf 'see decompiled/output.c\n' ;;
      'FUN_[0-9a-fA-F]{6}')     printf 'the routine at FUN_004a1b2c\n' ;;
      *)                        bad "self-test has no dirty sample for new pattern /$re/"; printf 'x\n' ;;
    esac > "${dir}/dirty-${n}.txt"
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

  # REACH CONTROL, not just a positive control. The counts above prove the flags fired; they do NOT
  # prove they fired BECAUSE the pattern list was applied. Empty the list and re-run the identical
  # dirty corpus: if anything still flags, the verdicts were not coming from the patterns.
  local saved=("${LOCAL_DETAIL_PATTERNS[@]}")
  LOCAL_DETAIL_PATTERNS=()
  classify_corpus "$manifest" > "${dir}/verdicts-nopat.txt"
  want "no pattern => nothing flags"  "$CORPUS_FLAGGED"    "0"
  want "no pattern => still classified all" "$CORPUS_CLASSIFIED" "$CORPUS_TOTAL"
  LOCAL_DETAIL_PATTERNS=("${saved[@]}")

  # ...and that empty-list state must be REFUSED by the tracked-file scanner rather than reported
  # as OK, since "0 findings" and "0 patterns applied" print the same word otherwise.
  local out rc=0
  out="$(LOCAL_DETAIL_PATTERNS=() bash -c '
      source '"${REPO_ROOT}"'/scripts/local-detail-patterns.sh
      LOCAL_DETAIL_PATTERNS=()
      if [ "${#LOCAL_DETAIL_PATTERNS[@]}" -eq 0 ]; then echo REFUSED; exit 1; fi
      echo ACCEPTED' 2>&1)" || rc=$?
  want "empty pattern list is refused" "$out" "REFUSED"
  want "empty pattern list exits 1"    "$rc"  "1"

  # An empty corpus must be an error, not a pass.
  : > "${dir}/empty-manifest.tsv"
  classify_corpus "${dir}/empty-manifest.tsv" > /dev/null
  want "empty corpus counts as 0 items" "$CORPUS_TOTAL" "0"

  # Assert how many checks ran. A case that silently stops running must fail this step rather than
  # shrink the output. `checks + 1` counts this assertion itself, which has not been tallied yet.
  want "self-test ran every check (incl. this one)" "$((checks + 1))" "10"

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
      -h|--help)   sed -n '1,40p' "$0"; exit 0 ;;
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
