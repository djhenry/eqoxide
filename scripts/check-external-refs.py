#!/usr/bin/env python3
"""Enforce the external-reference convention in docs/external-references.md.

A citation of another project's source is written `<alias>:<repo-relative path>:<line>[-<line>]`,
e.g. `eqemu:zone/corpse.cpp:50`. The alias resolves to a pinned commit in the table in
docs/external-references.md. This script fails on a C/C++-style `path.cpp:NNN` citation that
carries no alias (or no directory), so a reference can never silently float against upstream's HEAD.

    scripts/check-external-refs.py                      # syntax only (what CI runs)
    scripts/check-external-refs.py --eqemu ~/git/EQEmu  # also check each cite resolves at the pin

`--eqemu` is read-only (`git show <pin>:<path>`); it does not touch the checkout's working tree. It
proves the file exists at the pin and the line is in range, not that the line still says what the
comment claims: re-read the cited line when you bump the pin.
"""
import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TABLE = ROOT / "docs" / "external-references.md"
EXTS = r"(?:cpp|cc|h|hpp|conf)"
# Skipped: the doc that defines the syntax (its examples are deliberately bare) and this script.
SKIP = {"docs/external-references.md", "scripts/check-external-refs.py"}
BINARY_SUFFIXES = {".lock", ".png", ".jpg", ".bin", ".wld", ".s3d", ".eqg", ".zip", ".gif", ".ttf"}

CITE = re.compile(rf"(?<![\w/.-])((?:[\w.+-]+/)*[\w.+-]+\.{EXTS}):(\d+)(?:[-–](\d+))?")
ALIASED = re.compile(rf"(?<![\w/.-])([a-z][a-z0-9_-]*):((?:[\w.+-]+/)+[\w.+-]+\.{EXTS}):(\d+)(?:[-–](\d+))?")


def pins():
    """alias -> (project, commit), from the `| alias | project | commit |` rows of the table."""
    out = {}
    for line in TABLE.read_text(encoding="utf-8").splitlines():
        m = re.match(r"\|\s*`([a-z][a-z0-9_-]*)`\s*\|\s*([^|]+?)\s*\|\s*`([0-9a-f]{40})`\s*\|", line)
        if m:
            out[m.group(1)] = (m.group(2), m.group(3))
    return out


def tracked_text_files():
    names = subprocess.run(["git", "ls-files"], cwd=ROOT, capture_output=True, text=True, check=True).stdout.split("\n")
    for name in names:
        if not name or name in SKIP or Path(name).suffix in BINARY_SUFFIXES:
            continue
        try:
            yield name, (ROOT / name).read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--eqemu", metavar="CHECKOUT", help="path to an EQEmu/Server clone; verify citations resolve at the pin")
    args = ap.parse_args()

    table = pins()
    if "eqemu" not in table:
        sys.exit(f"{TABLE.relative_to(ROOT)}: no `eqemu` row with a 40-hex commit")

    problems, aliased = [], []
    for name, text in tracked_text_files():
        for lineno, line in enumerate(text.split("\n"), 1):
            # Blank out aliased cites first so the bare-cite pass cannot re-match their tail.
            for m in ALIASED.finditer(line):
                if m.group(1) not in table:
                    problems.append(f"{name}:{lineno}: unknown reference alias `{m.group(1)}` (add it to the table)")
                else:
                    aliased.append((name, lineno, m.group(1), m.group(2), int(m.group(3)), int(m.group(4) or m.group(3))))
            rest = ALIASED.sub(lambda m: " " * len(m.group(0)), line)
            for m in CITE.finditer(rest):
                problems.append(f"{name}:{lineno}: bare citation `{m.group(0)}`; write `eqemu:<repo-relative path>:<line>`")

    if args.eqemu:
        sha = table["eqemu"][1]
        lengths = {}
        for name, lineno, alias, path, lo, hi in aliased:
            if alias != "eqemu":
                continue
            if path not in lengths:
                r = subprocess.run(["git", "-C", args.eqemu, "show", f"{sha}:{path}"], capture_output=True)
                lengths[path] = None if r.returncode else r.stdout.count(b"\n") + 1
            n = lengths[path]
            if n is None:
                problems.append(f"{name}:{lineno}: `{path}` does not exist at eqemu@{sha[:9]}")
            elif max(lo, hi) > n:
                problems.append(f"{name}:{lineno}: `{path}:{max(lo, hi)}` is past the end of the file ({n} lines) at eqemu@{sha[:9]}")

    for p in problems:
        print(p)
    print(f"{len(aliased)} aliased citation(s), {len(problems)} problem(s)", file=sys.stderr)
    sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main()
