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
from the repo, is a host name, an account, a path or a secret. Its one dirty fixture (`vrolkam`) is
invented here and is in no dictionary.

WHAT IT IS FIT FOR — read this before quoting a run
---------------------------------------------------
This is a **WARNING, not a gate**, and the reason is measured, not assumed (see #995 for the full
figures). On this repo's tracked corpus the heuristic flags a double-digit number of tokens and
**none of them is a host name** — the class has 0 tracked occurrences since the #989 scrub — so its
measured precision on the real corpus is 0. Every flag it prints today is a false positive.

It is shipped anyway, at warning level, because its *recall* is what #995 is about: it goes RED on
the pre-scrub text that `check-no-local-detail.sh` exited 0 on for months. A reviewer reading a
short list of odd bare words near "server"/"box"/"deploy" is a cheap second pair of eyes. A CI job
failing on 30 known-innocent words is not, and suppressing them with an allowlist until the corpus
goes quiet would stop being a detector — which is exactly what #983 measured and refused.

So: `--strict` exists and makes findings fatal, and CI deliberately does not use it.

THE ALLOWLIST IS THE ENGLISH LANGUAGE, NOT A LIST OF THINGS THIS REPO SAYS
--------------------------------------------------------------------------
#983's finding was that "an allowlist is not outside the corpus it allowlists" — a per-token
suppression list grows until the corpus goes quiet and the detector is gone. The vocabulary
allowlist here is a system hunspell dictionary, plus a small suffix/prefix stemmer so inflections
resolve to it. It is external to this repo, it is not editable from a PR, and adding a host name to
it would mean adding a word to an English dictionary. There is NO per-token suppression list in this
repo, deliberately: the flagged tokens are printed and left flagged.

The dictionary is a hard dependency. If none is found the script REFUSES (exit 1) rather than
scanning with an empty allowlist and printing a green run over a corpus it could not classify —
the same refusal shape as the empty-pattern-list guard in `check-no-local-detail.sh`.

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

A clean run means "no hostname-SHAPED bare token survived five filters", never "no host name is
present". Those differ by all six items above.

CLASSIFICATION, NOT EXCEPTIONS (the #995 reach requirement)
------------------------------------------------------------
The bug in #995 is that a guard reporting only exceptions cannot tell "nothing wrong" from "nothing
looked at". So this script classifies EVERY candidate token it considered into exactly one bucket,
prints the bucket totals, and prints the corpus itself — files scanned, files skipped, lines, and
the dictionary it resolved with its entry count. The bucket totals sum to the token total; that
identity is asserted, not hoped for.

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

# Hunspell/myspell dictionaries, in discovery order. ONLY hunspell-format `.dic` files are accepted.
# `/usr/share/dict/words` is deliberately NOT in this list even though it is more widely installed:
# on this dev box it is a 479,823-entry list that includes given names, and a dictionary large
# enough to contain the very token this heuristic exists to catch would allowlist the leak. The
# self-test's dictionary control is what actually enforces that, on whatever list gets resolved.
WORDLIST_CANDIDATES = [
    "/usr/share/hunspell/en_US.dic",
    "/usr/share/myspell/en_US.dic",
    "/usr/share/hunspell/en_GB.dic",
    "/usr/share/myspell/en_GB.dic",
]

# English morphology, so `bounces`, `unacknowledged` and `disambiguates` resolve to dictionary
# entries instead of becoming findings. Without this the flag count on this repo is roughly 3x
# higher and consists almost entirely of ordinary inflected English.
SUFFIXES = (
    "s", "es", "ed", "d", "ing", "ly", "er", "ers", "est", "ion", "ions",
    "able", "ible", "ise", "ize", "ised", "ized", "ises", "izes", "ising", "izing",
    "ment", "ments", "ness", "al", "ally", "y", "ies",
)
PREFIXES = ("un", "re", "non", "pre", "mis", "over", "under", "de", "dis", "in", "im")

# The one deliberately-dirty fixture, invented here. It is hostname-SHAPED and in no dictionary, and
# it is the only reason this file is excluded from its own scan (see SELF_PATH below) — the same
# reason `check-no-local-detail.sh` excludes itself and the pattern file.
DIRTY_TOKEN = "vrolkam"

SELF_PATH = "scripts/check-host-shape.py"


# --- vocabulary ----------------------------------------------------------------------------------


def load_wordlist() -> tuple[set[str], str]:
    """Resolve and read the dictionary. Refuses rather than returning an empty allowlist."""
    override = os.environ.get("HOST_SHAPE_WORDLIST")
    paths = [override] if override else WORDLIST_CANDIDATES
    for path in paths:
        if not path or not os.path.isfile(path):
            continue
        words: set[str] = set()
        with open(path, "r", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                # hunspell `.dic`: `word/AFFIXFLAGS`, with a bare entry count on line 1.
                word = line.split("/", 1)[0].strip().lower()
                if word and word.isascii() and word.isalpha():
                    words.add(word)
        if words:
            return words, path
    sys.stderr.write(
        "::error::check-host-shape: NO ENGLISH DICTIONARY FOUND — the vocabulary allowlist would be\n"
        "empty, so every ordinary word on an infrastructure line would be reported as a finding and\n"
        "the run would say nothing about host names at all. Refusing, rather than scanning with no\n"
        "allowlist. Install a hunspell en_US dictionary (Debian/Ubuntu: `hunspell-en-us`, Fedora:\n"
        "`hunspell-en-US`) or point HOST_SHAPE_WORDLIST at one.\n"
        "Searched: " + ", ".join(p for p in paths if p) + "\n"
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


def read_corpus(root: str) -> tuple[dict[str, str], int, int]:
    out = subprocess.run(
        ["git", "-C", root, "ls-files", "-z"], capture_output=True, text=True, check=True
    ).stdout
    names = [n for n in out.split("\0") if n]
    texts: dict[str, str] = {}
    skipped_binary = 0
    excluded = 0
    for name in names:
        if name == SELF_PATH:
            excluded += 1
            continue
        try:
            raw = open(os.path.join(root, name), "rb").read()
        except OSError:
            skipped_binary += 1
            continue
        if b"\0" in raw:
            skipped_binary += 1
            continue
        texts[name] = raw.decode("utf-8", errors="replace")
    return texts, skipped_binary, excluded


# --- the scan -----------------------------------------------------------------------------------

# Buckets, in cascade order. Every candidate token lands in exactly one; the totals are asserted to
# sum to the number of tokens seen, because a classification that does not add up is the same defect
# as reporting only exceptions.
BUCKETS = [
    ("not-hostname-shaped", "has _ . - or an uppercase letter or a digit, or is <3 / >24 chars"),
    ("no-infra-context", "its line carries none of the infrastructure vocabulary"),
    ("not-bare-prose", "glued to code punctuation, so it is code and not a prose mention"),
    ("allowlisted-english", "resolves to a dictionary entry (directly or by stem)"),
    ("recurs-across-files", "appears in more than one tracked file"),
    ("FLAGGED", "hostname-shaped, bare, near infrastructure vocabulary, unknown, file-unique"),
]


def scan(texts: dict[str, str], words: set[str]) -> tuple[Counter, dict, int, int]:
    """Classify every token in the corpus. Returns (bucket counts, findings, files, lines)."""
    # Pass 1: how many distinct files each hostname-shaped token appears in.
    file_freq: Counter = Counter()
    for text in texts.values():
        seen = {m.group(0) for m in TOKEN_RE.finditer(text) if is_hostname_shaped(m.group(0))}
        for token in seen:
            file_freq[token] += 1

    counts: Counter = Counter()
    findings: dict[str, list[str]] = defaultdict(list)
    n_lines = 0
    for name, text in texts.items():
        for lineno, line in enumerate(text.splitlines(), 1):
            n_lines += 1
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
    return counts, findings, len(texts), n_lines


# --- self-test ----------------------------------------------------------------------------------


def self_test(words: set[str], wordlist_path: str) -> int:
    """Fixtures in BOTH directions, offline. A guard that flags everything and a guard that flags
    nothing both print a clean-looking scan; only a two-direction fixture separates them."""
    checks: list[tuple[str, bool]] = []

    # Direction 1 — the dirty fixture MUST flag. This is the #995 shape, rebuilt from an invented
    # token: a bare unknown word in prose on a line that also says "box".
    dirty = {"fixture.md": "the sync ran on every build box and " + DIRTY_TOKEN + "\n"}
    counts, findings, _, _ = scan(dirty, words)
    checks.append(("dirty fixture flags", counts["FLAGGED"] == 1 and DIRTY_TOKEN in findings))

    # Direction 2 — ordinary English prose on an infrastructure line must NOT flag. Without this a
    # guard that flags every word would pass direction 1.
    clean = {"fixture.md": "the asset server rejected the request and the container restarted\n"}
    counts_clean, _, _, _ = scan(clean, words)
    checks.append(("clean prose does not flag", counts_clean["FLAGGED"] == 0))

    # Reach control — the infrastructure-proximity filter must be load-bearing. Same dirty text with
    # the vocabulary emptied must go quiet; if it still flags, proximity is not doing anything and
    # the FP measurement on #995 does not describe what is running.
    global INFRA_RE
    saved = INFRA_RE
    try:
        INFRA_RE = re.compile(r"(?!x)x")  # matches nothing
        counts_noctx, _, _, _ = scan(dirty, words)
    finally:
        INFRA_RE = saved
    checks.append(("proximity filter is load-bearing", counts_noctx["FLAGGED"] == 0))

    # Dictionary control — on WHATEVER list got resolved, the fixture token must be unknown and a
    # common word must be known. A dictionary big enough to contain a short name (the 479k-entry
    # /usr/share/dict/words on this dev box is) would silently allowlist the class.
    checks.append(("dictionary does not know the fixture", not is_english(DIRTY_TOKEN, words)))
    checks.append(("dictionary knows ordinary English", is_english("machine", words)))
    checks.append(("stemmer resolves an inflection", is_english("disambiguates", words)))

    # Classification identity — the buckets must account for every token, on a fixture whose token
    # count is known by hand: 9 tokens on the dirty line.
    total = sum(counts[b] for b, _ in BUCKETS)
    checks.append(("buckets sum to tokens seen", total == 9))

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
    texts, skipped_binary, excluded = read_corpus(root)
    if not texts:
        print("::error::check-host-shape: CORPUS IS EMPTY — 0 tracked text files were read. That is")
        print("a guard failure, not a clean tree.")
        return 1

    counts, findings, n_files, n_lines = scan(texts, words)
    seen = sum(counts[b] for b, _ in BUCKETS)

    print(f"check-host-shape: corpus — {n_files} tracked text files, {n_lines} lines "
          f"({skipped_binary} binary/unreadable skipped, {excluded} excluded: this script, which "
          f"necessarily contains its own dirty fixture).")
    print(f"check-host-shape: vocabulary — {len(INFRA_WORDS)} infrastructure words, dictionary "
          f"{wordlist_path} ({len(words)} entries) + a stemmer. No per-token suppression list.")
    print(f"check-host-shape: classified {seen}/{seen} candidate tokens —")
    for bucket, why in BUCKETS:
        print(f"    {counts[bucket]:>8}  {bucket:<22} ({why})")

    # A classification that does not add up is the defect this script exists to avoid, one level up.
    if seen != sum(counts.values()):
        print("::error::check-host-shape: bucket totals do not account for every token.")
        return 1

    if counts["FLAGGED"]:
        print()
        print(f"check-host-shape: {counts['FLAGGED']} flagged occurrence(s), "
              f"{len(findings)} distinct token(s):")
        for token in sorted(findings):
            print(f"    {token}  ->  {', '.join(findings[token])}")
        print()
        print("Each of these is a bare lowercase word, unknown to the dictionary, appearing near")
        print("infrastructure vocabulary and in exactly one tracked file. MOST OF THEM ARE JARGON,")
        print("NOT HOST NAMES — measured precision on this corpus is 0 (#995), which is why this is")
        print("a warning. Read the list; do not add a suppression list to silence it.")

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
