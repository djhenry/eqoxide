#!/usr/bin/env python3
"""check-host-shape.py — a WARNING-level structural heuristic for the one leak class that
`scripts/check-no-local-detail.sh` cannot express as a regex: a deployment HOST NAME dropped into
prose (eqoxide#995).

WHY IT IS NOT A REGEX, AND WHY IT NAMES NOTHING
-----------------------------------------------
A host name has no shape of its own — any word can be one — so a regex covering the class means
writing the names in use into `scripts/local-detail-patterns.sh`, which is tracked in this PUBLIC
repo. That would make the leak-detection guard the only tracked instance of the leak. The owner
rejected that on #995 and chose a STRUCTURAL heuristic instead: flag *hostname-shaped bare tokens
appearing near infrastructure vocabulary*. Nothing in this file, and nothing in any file it reads
from the repo, is a host name, an account, a path or a secret. Its one dirty fixture is an archaic
English word for a cupboard, chosen because no dictionary this script will accept contains it (see
the allowlist section) — it names nothing and refers to nothing here.

WHAT IT IS FIT FOR — read this before quoting a run
---------------------------------------------------
This is a **WARNING, not a gate**, and the reason is measured, not assumed (see #995 for the full
figures). On this repo's tracked corpus the heuristic flags a double-digit number of tokens and
**none of them is a host name** — the class has 0 tracked occurrences since the #989 scrub — so its
measured precision on the real corpus is 0. Every flag it prints today is a false positive.

It is shipped anyway, at warning level, because its *recall* is what #995 is about: it goes RED on
the pre-scrub text that `check-no-local-detail.sh` exited 0 on for months. A reviewer reading a
short list of odd bare words near "server"/"box"/"deploy" is a cheap second pair of eyes. A CI job
failing on that many known-innocent words is not, and suppressing them with an allowlist until the
corpus goes quiet would stop being a detector — which is exactly what #983 measured and refused.

No flag count is written down in this header or in any other tracked file, deliberately: the corpus
changes with every commit, so a pinned figure rots into a false claim. The live count, its
denominator and every bucket are printed by the run itself. The same rule cost this header two other
numbers — see reach note 3 — on a second ground: a figure nobody else can re-derive from the words
next to it is not evidence, whether or not it is stale.

So: `--strict` exists and makes findings fatal, and CI deliberately does not use it.

THE ALLOWLIST IS THE ENGLISH LANGUAGE, NOT A LIST OF THINGS THIS REPO SAYS
--------------------------------------------------------------------------
#983's finding was that "an allowlist is not outside the corpus it allowlists" — a per-token
suppression list grows until the corpus goes quiet and the detector is gone. The vocabulary
allowlist here is a system hunspell dictionary, plus a small suffix/prefix stemmer so inflections
resolve to it. Its CONTENTS are external to this repo and there is NO per-token suppression list
here, deliberately: the flagged tokens are printed and left flagged.

The contents being external is not by itself enough, and the first revision of this file claimed it
was. Review found the hole: the dictionary's contents are external but its SELECTION was
repo-controlled and unvalidated, so a PR could point `HOST_SHAPE_WORDLIST` at a 479k-entry
names-and-inflections superset — which contains short given names, and therefore host names — and
silence the class while every check stayed green. Three things close that, and each is testable:

  1. `HOST_SHAPE_WORDLIST` is IGNORED when `CI` is set. In CI the dictionary is whatever the runner
     installed; a branch cannot redirect it.
  2. The RESOLVED path is validated as hunspell `.dic` FORMAT — a bare decimal count on line 1 and a
     sibling `.aff` — and refused if it is not, wherever it came from. A plain word list has neither.
  3. The self-test's dictionary control uses a fixture token that is a REAL but obscure English word
     (`aumbry`, an archaic word for a small cupboard, with nothing to do with this or any
     deployment). It was drawn at random from the lowercase 4-8 letter words present in the superset
     list and absent from the resolved dictionary. NO SIZE IS GIVEN FOR THAT SET, deliberately: the
     phrase does not determine one. Fold case or not, skip the count line or not, apply the stemmer
     or not, and it moves by tens of thousands — review derived three different values from this
     same sentence, all correct under their own reading. Any figure written here would be false
     under the other readings.

     The size is also not what the control rests on. What it rests on is checkable directly and in
     one line: the fixture IS in the difference set, and `is_english(fixture, en_US)` is False. A
     correct dictionary does not know it, so the dirty fixture flags; a superset list DOES know it,
     so the control goes RED. The previous fixture was an INVENTED word, absent from every
     dictionary including the bad one, so that control could not fail: it passed against the very
     list this header calls a trap, which is why review rejected it.

     What it does NOT cover, stated rather than controlled for: a dictionary crafted to hold exactly
     this repo's leaked token and nothing else passes all three closures above. That is not a gap
     worth a fourth control — building such a list means writing the token down, which republishes
     the leak, and closure 1 refuses the override in CI regardless. A control against it would only
     move the same question one level up.

The dictionary is a hard dependency. If none is found, or the one found is not hunspell format, the
script REFUSES (exit 1) rather than scanning with an empty or wrong allowlist and printing a green
run over a corpus it could not classify — the same refusal shape as the empty-pattern-list guard in
`check-no-local-detail.sh`.

REACH — what this structurally CANNOT see
------------------------------------------
Stated once, honestly, and not compensated for with further controls:

  1. A host name that IS an English word (`atlas`, `phoenix`, `oracle`) is allowlisted and invisible.
     This is the direct cost of the anti-tuning property above.
  2. A host name containing a digit or a hyphen (`db-01`, `web2`) fails the shape filter.
  3. A host name inside code — a config default, a URL, a struct field, a `&str` glued to
     punctuation — fails the bare-prose filter. Only whitespace-delimited prose mentions are seen.
  4. A host name mentioned in more than one tracked file is dropped by the cross-file filter: a name
     that recurs reads as project vocabulary to this heuristic and it cannot tell the difference.
  5. A host name not on a line that also carries infrastructure vocabulary is never a candidate. The
     vocabulary list below is itself an allowlist of words someone already thought of.
  6. Tracked files only, like its sibling — never PR bodies, issue bodies or comments (#980), and
     never the git history.
  7. The detector supplies some of its own candidates: `rsync` is in the infrastructure vocabulary
     AND is hostname-shaped, so it satisfies its own proximity filter. Some of the flags on this
     corpus are the tool seeing its own vocabulary. Harmless, but it inflates the false-positive
     count for a structural reason rather than a corpus one.

A clean run means "no hostname-SHAPED bare token survived five filters", never "no host name is
present". Those differ by all seven items above.

CLASSIFICATION, NOT EXCEPTIONS (the #995 reach requirement)
------------------------------------------------------------
The bug in #995 is that a guard reporting only exceptions cannot tell "nothing wrong" from "nothing
looked at". So this script classifies EVERY candidate token it considered into exactly one bucket,
prints the bucket totals, and prints the corpus itself — files scanned, files skipped, lines, and
the dictionary it resolved with its entry count.

The `classified A/B` line is an IDENTITY BETWEEN TWO INDEPENDENTLY DERIVED NUMBERS: `B` is a count
of tokens the tokenizer produced, incremented per line from its own `findall`, and `A` is the sum of
the six buckets. They are computed by different statements from different data, so a token that
leaves the tokenizer and never reaches a bucket makes them disagree and the run FAILS.

The first revision printed one variable twice. Review measured what that cost: adding a single
`continue` that dropped every 6-character token before bucketing silently discarded about a tenth of
the corpus and lowered the FLAGGED count, while `classified N/N` still claimed full reach and both
the scan and the self-test exited 0 — #995's own defect reproduced inside its fix. Re-run against
this revision, that same mutation makes the scan exit 1 naming the shortfall, and takes the
`buckets == tokens produced` check in `--self-test` RED. See the identity check in `main()`.

USAGE
  scripts/check-host-shape.py               # scan tracked files; warn; exit 0 unless the guard fails
  scripts/check-host-shape.py --strict      # same, but findings are fatal (NOT used in CI)
  scripts/check-host-shape.py --self-test   # offline fixtures, both directions; exit 1 on failure

  HOST_SHAPE_WORDLIST=/path/to/en_US.dic    # override dictionary discovery

EXIT CODES
  0  the scan ran with full reach (findings may have been printed, at warning level)
  1  a GUARD FAILURE — no dictionary, empty corpus, self-test failure, or a classification that did
     not add up. Never used for "found something" unless --strict was passed.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
import tempfile
from collections import Counter, defaultdict

# --- the two vocabularies this heuristic is built from -------------------------------------------

# Infrastructure vocabulary. A candidate token is only ever considered on a line that carries one of
# these. This is a list of GENERIC English/technical words — no name here is specific to any
# deployment, and the list is the same one anybody would write from scratch.
INFRA_WORDS = [
    "host", "hosts", "hostname", "hostnames",
    "server", "servers",
    "box", "boxes",
    "deploy", "deploys", "deployed", "deploying", "deployment", "deployments",
    "container", "containers",
    "ssh", "scp", "rsync",
    "remote", "machine", "machines", "vm", "vms",
    "infra", "infrastructure", "cluster",
    "podman", "docker", "systemd",
    "mariadb", "mysql",
    "nas", "datacenter", "rack",
]
INFRA_RE = re.compile(r"\b(" + "|".join(INFRA_WORDS) + r")\b", re.IGNORECASE)

# Every maximal run of identifier-ish characters is a candidate token. Splitting is deliberately
# NOT done on `_`, `.` or `-`: a token that contains one is a code identifier, a path or an English
# compound, and the shape filter rejects it as a whole rather than mining pieces out of it.
TOKEN_RE = re.compile(r"[A-Za-z0-9_][A-Za-z0-9_.-]*")

# A bare prose mention is whitespace- or sentence-punctuation-delimited on both sides. Anything
# glued to code punctuation (`(`, `<`, `/`, `=`, `:`) is code, not prose. See reach item 3.
PRE_OK = set(" \t(—–\"'`*")
POST_OK = set(" \t,.;:!?)—–\"'`*")

# Hunspell/myspell dictionaries, in discovery order. A plain word list such as
# `/usr/share/dict/words` is deliberately NOT here even though it is more widely installed: on this
# dev box it is a 479,823-entry list that includes given names, and a dictionary large enough to
# contain the very token this heuristic exists to catch would allowlist the leak silently — measured
# by review, the leak drops out of the findings with the headline count unchanged.
#
# THE DISCOVERY LIST IS NOT WHAT ENFORCES THAT, and an earlier revision of this comment claimed it
# was while `HOST_SHAPE_WORDLIST` accepted any file at all. Enforcement is `validate_wordlist_format`
# below, which is applied to the RESOLVED path whatever its provenance, plus the CI override refusal
# in `load_wordlist`, plus the self-test's dictionary control on a real-word fixture. All three are
# testable and all three go red on the trap; the list below is only a search order.
WORDLIST_CANDIDATES = [
    "/usr/share/hunspell/en_US.dic",
    "/usr/share/myspell/en_US.dic",
    "/usr/share/hunspell/en_GB.dic",
    "/usr/share/myspell/en_GB.dic",
]

# A hunspell `en_*` dictionary is on the order of 50k stems. A list several times that size is a
# names-and-inflections superset of the kind that contains short given names, which is the trap.
# This is a coarse backstop BEHIND the format check and the fixture control, not the control itself:
# a hand-built superset in valid hunspell format and under this ceiling still fails the fixture
# check, which is the one that is actually about the class.
MAX_WORDLIST_ENTRIES = 200_000

# English morphology, so `bounces`, `unacknowledged` and `disambiguates` resolve to dictionary
# entries instead of becoming findings. Without this the flag count on this repo is roughly 3x
# higher and consists almost entirely of ordinary inflected English.
SUFFIXES = (
    "s", "es", "ed", "d", "ing", "ly", "er", "ers", "est", "ion", "ions",
    "able", "ible", "ise", "ize", "ised", "ized", "ises", "izes", "ising", "izing",
    "ment", "ments", "ness", "al", "ally", "y", "ies",
)
PREFIXES = ("un", "re", "non", "pre", "mis", "over", "under", "de", "dis", "in", "im")

# The one deliberately-dirty fixture. It is a REAL but obscure English word — an archaic term for a
# small cupboard or recess — and that is the whole point: it is absent from `en_US.dic`, so a correct
# dictionary leaves it flagged, and PRESENT in the 479k names-and-inflections superset, so pointing
# this script at that superset makes the dictionary control FAIL instead of passing.
#
# The previous fixture was an invented string. Nothing knows an invented string, so the control
# passed against every dictionary including the trap — 7/7 on the exact list this file documents as
# unsafe. A control that cannot fail is the defect this whole PR is about.
#
# It has nothing to do with this or any deployment: it was drawn at random from the lowercase 4-8
# letter words that are in the superset and not in `en_US.dic`. That set's SIZE is deliberately not
# written down here or in the header — see reach note 3: the description admits several readings and
# each gives a different count, so any number would be false under the others. The property the
# control needs is membership, not the count: `--self-test` asserts one half directly (the resolved
# dictionary does NOT know this token), and pointing the script at a superset demonstrates the other
# (that dictionary DOES know it, and the control turns red).
#
# This fixture is also the only reason this file is excluded from its own scan (see `self_path()`),
# the same reason `check-no-local-detail.sh` excludes itself and the pattern file.
DIRTY_TOKEN = "aumbry"


# --- vocabulary ----------------------------------------------------------------------------------


def validate_wordlist_format(path: str) -> str | None:
    """Return None if `path` is a hunspell `.dic`, else a reason string.

    Applied to the RESOLVED path regardless of where it came from. Two independent structural
    signals, both of which a plain newline-delimited word list fails: hunspell writes a bare decimal
    approximate-entry-count on line 1, and a `.dic` is meaningless without its affix table, so a
    sibling `.aff` must exist. This is a FORMAT check, not a content check — it is what makes the
    tracked claim "only hunspell dictionaries are accepted" true of the resolved path and not merely
    of the search order.
    """
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as fh:
            first = fh.readline().strip()
    except OSError as exc:
        return f"could not be read ({exc})"
    # hunspell tolerates a BOM and trailing junk on the count line; a bare integer is the signal.
    if not first.lstrip("﻿").split()[0:1] or not first.lstrip("﻿").split()[0].isdigit():
        return ("line 1 is not a bare entry count, so this is not a hunspell .dic — a plain word "
                "list starts with a word")
    if not os.path.isfile(os.path.splitext(path)[0] + ".aff"):
        return "no sibling .aff affix table next to it, which every hunspell .dic has"
    return None


def load_wordlist() -> tuple[set[str], str]:
    """Resolve, validate and read the dictionary. Refuses rather than returning a wrong allowlist."""
    override = os.environ.get("HOST_SHAPE_WORDLIST")
    in_ci = bool(os.environ.get("CI"))
    if override and in_ci:
        # The allowlist's whole claim to being outside the corpus is that a branch cannot choose it.
        # `.github/workflows/test.yml` is a tracked file that runs from the PR's own branch, so an
        # honoured override in CI would let a PR redirect the allowlist at a superset list, silence
        # the class, and stay green. Refuse loudly instead of ignoring it quietly: a run that was
        # asked to use a different dictionary and did not must not look like a normal run.
        sys.stderr.write(
            "::error::check-host-shape: HOST_SHAPE_WORDLIST is set and CI is set. The override is a\n"
            "local-development affordance; honouring it in CI would let a branch choose its own\n"
            "vocabulary allowlist, which is exactly the property this check depends on NOT having.\n"
            f"Refusing. (requested: {override})\n"
        )
        raise SystemExit(1)
    paths = [override] if override else WORDLIST_CANDIDATES
    tried: list[str] = []
    for path in paths:
        if not path or not os.path.isfile(path):
            continue
        why = validate_wordlist_format(path)
        if why is not None:
            if path == override:
                # An explicit request for an unusable file is an error, never a silent fallback to
                # some other dictionary — that would make the run's reach unknowable from its output.
                sys.stderr.write(
                    f"::error::check-host-shape: HOST_SHAPE_WORDLIST={path} is NOT a hunspell "
                    f"dictionary: {why}.\nRefusing. A names-and-inflections word list contains short "
                    "given names, so it would allowlist the very class this checks for — measured, "
                    "the leak drops out of the findings with the headline count unchanged.\n"
                )
                raise SystemExit(1)
            tried.append(f"{path} (rejected: {why})")
            continue
        words: set[str] = set()
        with open(path, "r", encoding="utf-8", errors="replace") as fh:
            next(fh, None)  # the count line
            for line in fh:
                # hunspell `.dic`: `word/AFFIXFLAGS`.
                word = line.split("/", 1)[0].strip().lower()
                if word and word.isascii() and word.isalpha():
                    words.add(word)
        if len(words) > MAX_WORDLIST_ENTRIES:
            sys.stderr.write(
                f"::error::check-host-shape: {path} holds {len(words)} entries, over the "
                f"{MAX_WORDLIST_ENTRIES} ceiling. A list that size is a names-and-inflections\n"
                "superset, not a spelling dictionary, and it would allowlist short given names.\n"
                "Refusing.\n"
            )
            raise SystemExit(1)
        if words:
            return words, path
        tried.append(f"{path} (rejected: parsed to 0 entries)")
    sys.stderr.write(
        "::error::check-host-shape: NO USABLE ENGLISH DICTIONARY — the vocabulary allowlist would be\n"
        "empty, so every ordinary word on an infrastructure line would be reported as a finding and\n"
        "the run would say nothing about host names at all. Refusing, rather than scanning with no\n"
        "allowlist. Install a hunspell en_US dictionary (Debian/Ubuntu: `hunspell-en-us`, Fedora:\n"
        "`hunspell-en-US`) or point HOST_SHAPE_WORDLIST at one.\n"
        "Searched: " + ", ".join(p for p in paths if p) + "\n"
        + ("Rejected: " + "; ".join(tried) + "\n" if tried else "")
    )
    raise SystemExit(1)


def _stem_hits(token: str, words: set[str]) -> bool:
    for suf in SUFFIXES:
        if token.endswith(suf) and len(token) - len(suf) >= 3:
            stem = token[: -len(suf)]
            if stem in words or stem + "e" in words:
                return True
            # `dropped` -> `drop`
            if len(stem) > 1 and stem[-1] == stem[-2] and stem[:-1] in words:
                return True
            # `carries` -> `carry`
            if stem.endswith("i") and stem[:-1] + "y" in words:
                return True
    return False


def is_english(token: str, words: set[str]) -> bool:
    if token in words:
        return True
    if _stem_hits(token, words):
        return True
    for pre in PREFIXES:
        if token.startswith(pre) and len(token) - len(pre) >= 3:
            rest = token[len(pre):]
            if rest in words or _stem_hits(rest, words):
                return True
    return False


# --- shape --------------------------------------------------------------------------------------


def is_hostname_shaped(token: str) -> bool:
    """A bare DNS label as it would be written in prose: all-lowercase letters, 3-24 of them.

    Digits and hyphens are legal in a host name and are rejected anyway — see reach item 2. They are
    what most of this repo's version strings, opcode names and hyphenated English compounds look
    like, and admitting them costs an order of magnitude in false positives for a form that this
    project does not use in prose.
    """
    if "_" in token or "." in token or "-" in token:
        return False
    if not token.islower():
        return False
    if not 3 <= len(token) <= 24:
        return False
    return token.isalpha() and token.isascii()


def is_bare_prose(line: str, start: int, end: int) -> bool:
    before = line[start - 1] if start > 0 else " "
    after = line[end] if end < len(line) else " "
    return before in PRE_OK and after in POST_OK


# --- corpus -------------------------------------------------------------------------------------


def repo_root() -> str:
    return subprocess.run(
        ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True
    ).stdout.strip()


def self_path(root: str) -> str:
    """This script's own path, repo-relative, DERIVED rather than hard-coded.

    The previous revision compared against a literal `scripts/check-host-shape.py`; renaming or
    moving the file would have silently stopped the self-exclusion applying, the dirty fixture in
    this header would have started flagging, and nothing would have said why. `main()` additionally
    refuses when the excluded count is not exactly 1, so a derivation that stops matching is a loud
    failure rather than a confusing finding.
    """
    return os.path.relpath(os.path.abspath(__file__), root)


def read_corpus(root: str) -> tuple[dict[str, str], int, int, int]:
    """Returns (texts, binary-skipped, unreadable-skipped, excluded).

    Binary and unreadable are counted apart: skipping a PNG is expected and skipping a tracked file
    that is missing from the working tree is a reach gap, and one number cannot say which happened.
    """
    out = subprocess.run(
        ["git", "-C", root, "ls-files", "-z"], capture_output=True, text=True, check=True
    ).stdout
    names = [n for n in out.split("\0") if n]
    me = self_path(root)
    texts: dict[str, str] = {}
    skipped_binary = 0
    skipped_unreadable = 0
    excluded = 0
    for name in names:
        if name == me:
            excluded += 1
            continue
        try:
            raw = open(os.path.join(root, name), "rb").read()
        except OSError:
            skipped_unreadable += 1
            continue
        if b"\0" in raw:
            skipped_binary += 1
            continue
        texts[name] = raw.decode("utf-8", errors="replace")
    return texts, skipped_binary, skipped_unreadable, excluded


# --- the scan -----------------------------------------------------------------------------------

# Buckets, in cascade order. Every candidate token lands in exactly one, and the totals are checked
# against an INDEPENDENTLY derived count of what the tokenizer produced — not against a re-sum of
# themselves, which is what the first revision did and which cannot fail. A classification that does
# not add up is the same defect as reporting only exceptions.
BUCKETS = [
    ("not-hostname-shaped", "has _ . - or an uppercase letter or a digit, or is <3 / >24 chars"),
    ("no-infra-context", "its line carries none of the infrastructure vocabulary"),
    ("not-bare-prose", "glued to code punctuation, so it is code and not a prose mention"),
    ("allowlisted-english", "resolves to a dictionary entry (directly or by stem)"),
    ("recurs-across-files", "appears in more than one tracked file"),
    ("FLAGGED", "hostname-shaped, bare, near infrastructure vocabulary, unknown, file-unique"),
]


def scan(texts: dict[str, str], words: set[str]) -> tuple[Counter, dict, int, int, int]:
    """Classify every token in the corpus.

    Returns (bucket counts, findings, files, lines, tokens_produced). `tokens_produced` is the
    control on the classification: it is counted by its own call to the tokenizer, per line, before
    and independently of the cascade below, so any path that consumes a token without bucketing it
    makes the two numbers disagree. Summing the buckets a second time — the first revision's check —
    cannot detect that, because a token that never reaches a bucket is absent from both sums.
    """
    # Pass 1: how many distinct files each hostname-shaped token appears in.
    file_freq: Counter = Counter()
    for text in texts.values():
        seen = {m.group(0) for m in TOKEN_RE.finditer(text) if is_hostname_shaped(m.group(0))}
        for token in seen:
            file_freq[token] += 1

    counts: Counter = Counter()
    findings: dict[str, list[str]] = defaultdict(list)
    n_lines = 0
    tokens_produced = 0
    for name, text in texts.items():
        for lineno, line in enumerate(text.splitlines(), 1):
            n_lines += 1
            tokens_produced += len(TOKEN_RE.findall(line))
            has_infra = INFRA_RE.search(line) is not None
            for m in TOKEN_RE.finditer(line):
                token = m.group(0)
                if not is_hostname_shaped(token):
                    counts["not-hostname-shaped"] += 1
                elif not has_infra:
                    counts["no-infra-context"] += 1
                elif not is_bare_prose(line, m.start(), m.end()):
                    counts["not-bare-prose"] += 1
                elif is_english(token, words):
                    counts["allowlisted-english"] += 1
                elif file_freq[token] > 1:
                    counts["recurs-across-files"] += 1
                else:
                    counts["FLAGGED"] += 1
                    findings[token].append(f"{name}:{lineno}")
    return counts, findings, len(texts), n_lines, tokens_produced


# --- surfacing ------------------------------------------------------------------------------------


def emit_annotations(findings: dict[str, list[str]]) -> None:
    """Put the findings where a human will actually see them.

    The decision to ship this as a warning rests entirely on "a reviewer reads the short list". That
    was not true of the first revision: findings went to plain stdout on a green step, which GitHub
    renders as nothing at all — a reviewer had to expand a passing job to find them. A warning nobody
    sees is not a second pair of eyes, it is a green check with a cost.

    `::warning file=,line=::` attaches each finding to its line in the PR's Files-changed view;
    `$GITHUB_STEP_SUMMARY` puts the whole list on the run summary page.

    Both are gated on the environment that gives them meaning. Outside Actions the `::warning`
    lines are just noise on top of the plain list that was already printed, so they are suppressed;
    set `GITHUB_ACTIONS=true` to see exactly what CI will emit.
    """
    if os.environ.get("GITHUB_ACTIONS"):
        for token in sorted(findings):
            for loc in findings[token]:
                path, _, lineno = loc.rpartition(":")
                print(f"::warning file={path},line={lineno},title=possible host name::"
                      f"'{token}' is a bare lowercase word unknown to the dictionary, near "
                      f"infrastructure vocabulary, and unique to this file. Almost always jargon — "
                      f"but if it names a machine, scrub it (eqoxide#995).")

    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary:
        return
    n_occ = sum(len(v) for v in findings.values())
    try:
        with open(summary, "a", encoding="utf-8") as fh:
            fh.write(f"### check-host-shape: {n_occ} possible host name(s), "
                     f"{len(findings)} distinct token(s)\n\n")
            fh.write("Warning only — this does not fail the build. Every flag measured on this "
                     "corpus to date has been jargon, not a host name (eqoxide#995). Read the list; "
                     "do not add a suppression list to silence it.\n\n")
            fh.write("| token | where |\n| --- | --- |\n")
            for token in sorted(findings):
                fh.write(f"| `{token}` | {', '.join('`' + w + '`' for w in findings[token])} |\n")
    except OSError:
        # Never let a summary-file problem change the verdict of the scan.
        pass


# --- self-test ----------------------------------------------------------------------------------


def self_test(words: set[str], wordlist_path: str) -> int:
    """Fixtures in BOTH directions, offline. A guard that flags everything and a guard that flags
    nothing both print a clean-looking scan; only a two-direction fixture separates them."""
    checks: list[tuple[str, bool]] = []

    # Direction 1 — the dirty fixture MUST flag. This is the #995 shape, rebuilt from a real but
    # obscure word: a bare word the dictionary does not know, in prose, on a line that says "box".
    dirty = {"fixture.md": "the sync ran on every build box and " + DIRTY_TOKEN + "\n"}
    counts, findings, _, _, produced = scan(dirty, words)
    checks.append(("dirty fixture flags", counts["FLAGGED"] == 1 and DIRTY_TOKEN in findings))

    # Direction 2 — ordinary English prose on an infrastructure line must NOT flag. Without this a
    # guard that flags every word would pass direction 1.
    clean = {"fixture.md": "the asset server rejected the request and the container restarted\n"}
    counts_clean, _, _, _, _ = scan(clean, words)
    checks.append(("clean prose does not flag", counts_clean["FLAGGED"] == 0))

    # Reach control — the infrastructure-proximity filter must be load-bearing. Same dirty text with
    # the vocabulary emptied must go quiet; if it still flags, proximity is not doing anything and
    # the FP measurement on #995 does not describe what is running.
    global INFRA_RE
    saved = INFRA_RE
    try:
        INFRA_RE = re.compile(r"(?!x)x")  # matches nothing
        counts_noctx, _, _, _, _ = scan(dirty, words)
    finally:
        INFRA_RE = saved
    checks.append(("proximity filter is load-bearing", counts_noctx["FLAGGED"] == 0))

    # DICTIONARY CONTROL — and this one now discriminates. The fixture is a real English word that
    # `en_US.dic` does not carry and a names-and-inflections superset does, so this check FAILS when
    # the resolved list is the trap. With an invented fixture it passed against every dictionary
    # including the trap (measured by review: 7/7 on the 479k list), which made it no control at all.
    checks.append(("dictionary does not know the fixture", not is_english(DIRTY_TOKEN, words)))
    checks.append(("dictionary knows ordinary English", is_english("machine", words)))
    checks.append(("stemmer resolves an inflection", is_english("disambiguates", words)))

    # Format control — whatever got resolved must pass the same validation the loader applies, and a
    # plain word list must be rejected by it. The second half is what makes this a control rather
    # than a restatement: it fails if `validate_wordlist_format` ever stops discriminating.
    checks.append(("resolved dictionary passes format validation",
                   validate_wordlist_format(wordlist_path) is None))
    # The negative half is built here rather than pointed at a system path, so it runs identically on
    # a runner that has no `/usr/share/dict/words` instead of passing vacuously.
    with tempfile.TemporaryDirectory() as tmp:
        plain = os.path.join(tmp, "words")
        with open(plain, "w", encoding="utf-8") as fh:
            fh.write("aardvark\nabacus\n" + DIRTY_TOKEN + "\n")
        checks.append(("a plain word list is rejected by format validation",
                       validate_wordlist_format(plain) is not None))

    # ACCOUNTING CONTROL — the buckets must equal what the tokenizer produced, and both must equal a
    # count done here, in the test, from the fixture text. Three derivations rather than one variable
    # printed twice: a token consumed without being bucketed breaks the first equality, and a
    # tokenizer change that alters what counts as a token breaks the second.
    bucket_total = sum(counts[b] for b, _ in BUCKETS)
    here = sum(len(TOKEN_RE.findall(ln)) for ln in dirty["fixture.md"].splitlines())
    checks.append(("buckets == tokens produced", bucket_total == produced))
    checks.append(("tokens produced == an independent count of the fixture", produced == here == 9))

    failed = 0
    for label, ok in checks:
        print(f"  [{'ok' if ok else 'FAIL'}] {label}")
        if not ok:
            failed += 1
    print(f"check-host-shape --self-test: {len(checks) - failed}/{len(checks)} checks, "
          f"dictionary {wordlist_path} ({len(words)} entries)")
    if failed:
        print("::error::check-host-shape: SELF-TEST FAILED — the heuristic is not discriminating.")
        return 1
    return 0


# --- main ---------------------------------------------------------------------------------------


def main(argv: list[str]) -> int:
    strict = "--strict" in argv
    words, wordlist_path = load_wordlist()

    if "--self-test" in argv:
        return self_test(words, wordlist_path)

    root = repo_root()
    texts, skipped_binary, skipped_unreadable, excluded = read_corpus(root)
    if not texts:
        print("::error::check-host-shape: CORPUS IS EMPTY — 0 tracked text files were read. That is")
        print("a guard failure, not a clean tree.")
        return 1
    if excluded != 1:
        # The one exclusion is this file. Zero means the derivation stopped matching (renamed? run
        # through a symlink?) and the dirty fixture in this header is about to appear as a finding
        # with nothing explaining it; more than one means something else got excluded silently.
        print(f"::error::check-host-shape: expected exactly 1 excluded file (this script), got "
              f"{excluded}. The self-exclusion is not matching, so the corpus is not what this "
              f"script thinks it is.")
        return 1

    counts, findings, n_files, n_lines, produced = scan(texts, words)
    bucket_total = sum(counts[b] for b, _ in BUCKETS)

    print(f"check-host-shape: corpus — {n_files} tracked text files, {n_lines} lines "
          f"({skipped_binary} binary skipped, {skipped_unreadable} unreadable skipped, {excluded} "
          f"excluded: this script, which necessarily contains its own dirty fixture).")
    print(f"check-host-shape: vocabulary — {len(INFRA_WORDS)} infrastructure words, dictionary "
          f"{wordlist_path} ({len(words)} entries) + a stemmer. No per-token suppression list.")
    print(f"check-host-shape: classified {produced} tokens produced / {bucket_total} bucketed —")
    for bucket, why in BUCKETS:
        print(f"    {counts[bucket]:>8}  {bucket:<22} ({why})")

    # THE identity, between two numbers derived by different statements from different data. Summing
    # the buckets and comparing to a re-sum of the same buckets — the first revision — is unfailable:
    # a token dropped before it reaches a bucket is missing from both sides. Measured by review, that
    # let a one-line mutation discard about a tenth of the corpus and lower the FLAGGED count with
    # the run still reporting full reach and exiting 0 — #995's own defect, so it gets a real check.
    if bucket_total != produced:
        print(f"::error::check-host-shape: {produced} tokens left the tokenizer but {bucket_total} "
              f"were classified — {produced - bucket_total} went unaccounted for. The scan did NOT "
              f"look at everything it reports having looked at.")
        return 1

    if counts["FLAGGED"]:
        print()
        print(f"check-host-shape: {counts['FLAGGED']} flagged occurrence(s), "
              f"{len(findings)} distinct token(s):")
        for token in sorted(findings):
            print(f"    {token}  ->  {', '.join(findings[token])}")
        print()
        print("Each of these is a bare lowercase word, unknown to the dictionary, appearing near")
        print("infrastructure vocabulary and in exactly one tracked file. MOST ARE JARGON, NOT HOST")
        print("NAMES — every flag measured on this corpus to date has been a false positive (#995),")
        print("which is why this is a warning. Read the list; do not add a suppression list.")
        emit_annotations(findings)

    print("check-host-shape: reach — a clean run means 'no hostname-SHAPED bare token survived five")
    print("filters', NOT 'no host name is present'. A host name that is an English word, or carries")
    print("a digit or hyphen, or sits in code rather than prose, or recurs across files, or is not")
    print("on a line with infrastructure vocabulary, is invisible here. See this file's header.")

    if strict and counts["FLAGGED"]:
        print("::error::check-host-shape: --strict and there are findings.")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
