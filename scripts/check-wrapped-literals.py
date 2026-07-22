#!/usr/bin/env python3
"""check-wrapped-literals — fail the build on a CORRUPTED line-continuation in a Rust string.

The defect (#641 review, findings 6 and N2):

    Rust joins a wrapped string literal with a trailing backslash, which eats the newline AND the
    next line's indentation:

        "a long message that continues \\
         on the next line"        ->   "a long message that continues on the next line"

    If the backslash is lost — most often when the code is authored through a shell heredoc, or any
    other layer that itself treats backslash-newline as a continuation — the newline disappears but
    the indentation does not, and the literal silently becomes:

        "a long message that continues         on the next line"

    It compiles. Tests pass. It surfaces only when a human reads the WARN line or the assertion
    message, i.e. exactly when they are already debugging something else.

This regressed three times on one pull request, twice after a manual sweep was reported clean.
That is the argument for a mechanical check instead of a fourth sweep.

Why a parser and not a grep: the obvious `grep` for "6+ spaces near a quote" is swamped by
deliberate padding that is NOT inside a literal — aligned tuple tables (`("ELF", "elf",   "Elf")`),
aligned map inserts, and `///` doc comments containing quoted text. Those are all correct. This
walks each line tracking whether it is inside a string, so it only ever reports a space run that is
genuinely part of the string's VALUE.

TUNING — and this is a HEURISTIC tuned to the observed mechanism, not a proof:

  * THRESHOLD = 12 spaces. The corruption bakes in a source INDENTATION, which for a wrapped
    message nested inside an `assert!`/`warn!` is deep: the three real cases measured 18, 18 and 14.
    Deliberate column alignment is shallower — the widest in this tree is 10. A run of 12+ spaces in
    the middle of a string is therefore a strong signal, and the gap between 10 and 14 is what makes
    the threshold defensible rather than arbitrary.
  * LEADING runs are ignored. Indenting the start of a literal is a normal idiom for CLI help text
    (`eprintln!("            creature, bear, rat, ...")`) and is not this defect, which by
    construction appears MID-literal where the newline used to be.

Both rules exist because the naive "6+ spaces near a quote" grep is swamped by correct code:
aligned tuple tables, aligned map inserts, `///` doc comments containing quoted text, and CLI usage
blocks. A guard that cries wolf gets disabled, so it is tuned to catch the mechanism that actually
bit us and to stay silent otherwise. Run `--self-test` to confirm it still discriminates.

KNOWN FALSE NEGATIVES — stated so nobody over-trusts a green run:

  * A corruption at SHALLOW nesting slips under the threshold, e.g.
    `warn!("lost the backslash        and continues")` with an 8-space indent. Threshold 12 buys a
    quiet guard at the cost of the shallow cases; the three that actually bit us were 14/18/18.
  * A continuation lost IMMEDIATELY after an embedded `\\n` escape, because indentation there is
    exempt (see the `\\n` handling below).
  * Anything spanning lines: this is a per-line check.

OPT-OUT: `check-wrapped-literals: allow` in a comment on the same line. Prefer a format spec
(`{:>8}`) over the opt-out if the padding really is deliberate. The tree currently has ZERO
opt-outs, and `MAX_OPTOUTS` below keeps it that way — a blanket sprinkling of the marker would
otherwise disable this check silently, one line at a time.

Exit 0 = clean, 1 = a suspect literal was found (prints file:line:content).
"""
import subprocess
import sys

THRESHOLD = 12
ALLOW = "check-wrapped-literals: allow"
# Raising this is a reviewable act. Each opt-out disables the check for one line, so an unbudgeted
# marker is how a guard like this dies quietly (#641 review N2-b).
MAX_OPTOUTS = 0


def suspect_runs(line: str) -> bool:
    """True if `line` has a run of >= THRESHOLD spaces MID-literal in a (non-raw) string."""
    i, n = 0, len(line)
    seen_text = False
    in_str = False
    run = 0
    while i < n:
        c = line[i]
        if not in_str:
            # Comments end the code portion of the line; quoted text inside them is prose.
            if c == "/" and i + 1 < n and line[i + 1] == "/":
                return False
            # A CHAR literal containing a quote — `'"'` — used to flip string parity for the rest of
            # the line, in BOTH directions: a false positive on padding that was outside any string,
            # and (the dangerous one) a false negative that hid a real corruption sharing the line.
            # Skip `'X'` and `'\X'`; a lifetime (`&'a str`) has no closing quote, so fall through and
            # advance one char. (#641 review N2-b defect 1)
            if c == "'":
                for width in (3, 4):  # 'X'  and  '\X'
                    if i + width <= n and line[i + width - 1] == "'":
                        i += width
                        break
                else:
                    i += 1
                continue
            # Raw strings (r"..", r#"..."#): skip exactly the literal, by counting its hashes and
            # finding the matching terminator. Previously the whole LINE was skipped, which silently
            # disabled the check for any real corruption sharing it. (#641 review N2-b defect 3)
            if c == "r" and i + 1 < n and line[i + 1] in ('"', "#"):
                j = i + 1
                hashes = 0
                while j < n and line[j] == "#":
                    hashes += 1
                    j += 1
                if j < n and line[j] == '"':
                    close = line.find('"' + "#" * hashes, j + 1)
                    if close == -1:
                        return False  # unterminated on this line — nothing more to say about it
                    i = close + 1 + hashes
                    continue
                i += 1
                continue
            if c == '"':
                in_str, run, seen_text = True, 0, False
            i += 1
            continue
        # Inside a string.
        if c == "\\":
            # An embedded `\n` starts a fresh visual line inside the literal, and indenting after it
            # is the same normal idiom the `seen_text` gate already exempts at the literal's START
            # (`eprintln!("Usage:\n            eqoxide --config NAME")`). Treat it the same way.
            # Cost, stated rather than hidden: a continuation lost IMMEDIATELY after a `\n` escape
            # is now invisible to this check. (#641 review N2-b defect 2 — this also retired the
            # tree's only opt-out.)
            escaped = line[i + 1] if i + 1 < n else ""
            i += 2  # an escape consumes the next char
            run = 0
            seen_text = escaped not in ("n", "r")
            continue
        if c == '"':
            in_str = False
            i += 1
            continue
        if c == " ":
            run += 1
            # `seen_text` gates out a LEADING indent, which is a normal CLI-help idiom and not this
            # defect — the corruption always lands where a newline used to be, i.e. mid-literal.
            if run >= THRESHOLD and seen_text:
                return True
        else:
            run = 0
            seen_text = True
        i += 1
    return False


def self_test() -> int:
    """Confirm the detector still discriminates — a guard nobody has tested is not a guard."""
    corrupt = [
        # The three shapes that actually regressed on #641, verbatim in structure.
        'assert!(x, "still queued for retry —                  these are NOT sent (#641)");',
        'warn!("besides its definition —              expected 3 mentions (definition + two)");',
        'assert_ne!(k, W, "fails with EMSGSIZE from the real sendto — if this is                  WouldBlock");',
        # N2-b defect 1, the SILENT direction: a char literal holding a quote used to flip string
        # parity for the rest of the line, hiding a real corruption after it.
        'let q = \'"\'; warn!("lost the backslash              and then continues");',
        # N2-b defect 3: a raw string earlier on the line used to disable the check for the WHOLE
        # line, hiding a real corruption sharing it.
        'let re = r"x"; warn!("lost the backslash              and then continues");',
    ]
    fine = [
        '("ELF", "elf",       "Elf"),',                      # aligned tuple table (padding outside)
        '"guild_id":       g.guild_id,',                     # aligned map insert (padding outside)
        'eprintln!("            creature, bear, rat, bat");',  # leading indent in help text
        '("Katie          (-138.5,-17.5)", -138.5f32),',     # deliberate label padding, under threshold
        'let e = src.find("\\n        }\\n").expect("x");',   # a code-shape pattern, under threshold
        '/// 199 "Insufficient Mana"                 (spells.cpp:490)',  # doc comment, not code
        'let s = "a normal message with single spaces";',
        'let s = "wrapped correctly \\',                     # a real continuation: no run at all
        # N2-b defect 1, the NOISY direction: the padding here is between literals, not inside one.
        'let q = \'"\'; let t = [("a",              "b")];',
        # N2-b defect 2: indenting after an embedded \n is the same idiom as indenting at the start.
        r'eprintln!("Usage:\n            eqoxide --config NAME");',
        # A lifetime is not a char literal and must not eat the following quote.
        'fn f<\'a>(s: &\'a str) { warn!("plain message"); }',
        # A raw string must be skipped precisely, not swallow the rest of the line.
        'let re = r#"a"b"#; let t = [("a",              "b")];',
    ]
    bad = [s for s in corrupt if not suspect_runs(s)]
    noisy = [s for s in fine if suspect_runs(s)]
    for s in bad:
        print(f"SELF-TEST FAIL (missed a corrupted literal): {s}", file=sys.stderr)
    for s in noisy:
        print(f"SELF-TEST FAIL (false positive on correct code): {s}", file=sys.stderr)
    if bad or noisy:
        return 1
    print(f"check-wrapped-literals --self-test: OK "
          f"({len(corrupt)} corrupted detected, {len(fine)} correct-code samples ignored).")
    return 0


def main() -> int:
    if "--self-test" in sys.argv:
        return self_test()
    root = subprocess.run(["git", "rev-parse", "--show-toplevel"],
                          capture_output=True, text=True, check=True).stdout.strip()
    files = subprocess.run(["git", "-C", root, "ls-files", "*.rs"],
                           capture_output=True, text=True, check=True).stdout.split()
    hits = []
    optouts = []
    for f in files:
        try:
            with open(f"{root}/{f}", encoding="utf-8") as fh:
                for lineno, line in enumerate(fh, 1):
                    line = line.rstrip("\n")
                    if ALLOW in line:
                        optouts.append(f"{f}:{lineno}")
                        continue
                    if suspect_runs(line):
                        hits.append(f"{f}:{lineno}: {line.strip()[:160]}")
        except (OSError, UnicodeDecodeError):
            continue

    if len(optouts) > MAX_OPTOUTS:
        print(f"check-wrapped-literals: FAILED — {len(optouts)} opt-out marker(s), budget is "
              f"{MAX_OPTOUTS}:", file=sys.stderr)
        print("\n".join(f"  {o}" for o in optouts), file=sys.stderr)
        print("Each marker disables this check for one line. If the new one is genuinely deliberate "
              "padding,\nraise MAX_OPTOUTS in this script so the decision is reviewed rather than "
              "absorbed silently.", file=sys.stderr)
        return 1

    if hits:
        print("\n".join(hits))
        print(
            "\ncheck-wrapped-literals: FAILED — the lines above contain a long run of spaces "
            "INSIDE a string literal.\nThat is the signature of a line-continuation backslash lost "
            "while the code was written, leaving\nthe indentation baked into the message. Re-wrap "
            'the literal with a trailing "\\" and re-read the\nrendered text. If the padding is '
            "deliberate, prefer a format spec ({:>8}), or add\n"
            f"`{ALLOW}` in a comment on that line.",
            file=sys.stderr,
        )
        return 1
    print(f"check-wrapped-literals: OK — no run of {THRESHOLD}+ spaces inside a string literal "
          f"in {len(files)} tracked .rs files.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
