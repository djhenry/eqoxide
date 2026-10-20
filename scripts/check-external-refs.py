#!/usr/bin/env python3
"""Enforce the external-reference convention in docs/external-references.md.

A citation of another project's source is written `<alias>:<repo-relative path>@<commit>:<line>[-<line>]`,
e.g. `eqemu:zone/corpse.cpp@7ab909ee4:50`. `<commit>` is the last commit that touched that file when
it was read; the alias names the project (see the table in the doc). A citation therefore stays valid
until its own file changes upstream, no matter how many unrelated commits land.

    scripts/check-external-refs.py                         # syntax only
    scripts/check-external-refs.py --checkout ~/git/EQEmu  # also: resolves at its commit, and not stale
    scripts/check-external-refs.py --checkout ~/git/EQEmu --fill   # stamp `eqemu:path:50` -> `eqemu:path@id:50`

`--checkout` is read-only (`git show`, `git diff`); it never touches the clone's working tree. It
proves the cited file and line exist at the cited commit, and reports each citation whose file has
since changed on the alias's upstream ref. It cannot tell whether the line still says what the
comment claims: re-read a stale citation, then update its id.
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
ABBREV = 9

PATH = rf"(?:[\w.+-]+/)+[\w.+-]+\.{EXTS}"
LINES = r"(\d+)(?:[-–](\d+))?"
# alias:path@commit:line   -- the only accepted form
ALIASED = re.compile(rf"(?<![\w/.-])([a-z][a-z0-9_-]*):({PATH})@([0-9a-f]{{7,40}}):{LINES}")
# alias:path:line          -- right alias, no commit (`--fill` stamps these)
NO_ID = re.compile(rf"(?<![\w/.-])([a-z][a-z0-9_-]*):({PATH}):{LINES}")
# path:line anywhere else  -- no alias, or no directory
BARE = re.compile(rf"(?<![\w/.-])((?:[\w.+-]+/)*[\w.+-]+\.{EXTS}):{LINES}")


def aliases():
    """alias -> upstream ref, from the `| `alias` | project | url | `ref` |` rows of the table."""
    out = {}
    for line in TABLE.read_text(encoding="utf-8").splitlines():
        m = re.match(r"\|\s*`([a-z][a-z0-9_-]*)`\s*\|[^|]*\|[^|]*\|\s*`([^`]+)`\s*\|", line)
        if m:
            out[m.group(1)] = m.group(2)
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


class Repo:
    """Read-only view of an upstream clone."""

    def __init__(self, path):
        self.path = path
        self._len = {}

    def git(self, *args):
        return subprocess.run(["git", "-C", self.path, *args], capture_output=True)

    def length(self, commit, path):
        """Line count of `path` at `commit`, or None if the commit or file does not exist."""
        key = (commit, path)
        if key not in self._len:
            r = self.git("show", f"{commit}:{path}")
            self._len[key] = None if r.returncode else r.stdout.count(b"\n") + 1
        return self._len[key]

    def last_touch(self, ref, path):
        r = self.git("log", "-1", f"--abbrev={ABBREV}", "--format=%h", ref, "--", path)
        return r.stdout.decode().strip() or None

    def is_ancestor(self, commit, ref):
        """Whether `commit` is published history of `ref`, not a local or feature-branch commit."""
        return self.git("merge-base", "--is-ancestor", commit, ref).returncode == 0

    def changed_since(self, commit, ref, path):
        return self.git("diff", "--quiet", commit, ref, "--", path).returncode != 0


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--checkout", metavar="CLONE", help="clone of the cited project (the `eqemu` alias)")
    ap.add_argument("--upstream", metavar="REF", help="ref to compare against for staleness (default: the alias's table ref)")
    ap.add_argument("--fill", action="store_true", help="with --checkout: rewrite `alias:path:line` to `alias:path@id:line`")
    args = ap.parse_args()
    if args.fill and not args.checkout:
        ap.error("--fill needs --checkout")

    table = aliases()
    if "eqemu" not in table:
        sys.exit(f"{TABLE.relative_to(ROOT)}: no `eqemu` row in the alias table")
    repo = Repo(args.checkout) if args.checkout else None
    upstream = args.upstream or table["eqemu"]

    problems, cites = [], []
    for name, text in tracked_text_files():
        lines = text.split("\n")
        for lineno, line in enumerate(lines, 1):
            for m in ALIASED.finditer(line):
                if m.group(1) not in table:
                    problems.append(f"{name}:{lineno}: unknown reference alias `{m.group(1)}` (add it to the table)")
                else:
                    cites.append((name, lineno, m.group(1), m.group(2), m.group(3), int(m.group(4)), int(m.group(5) or m.group(4))))
            rest = ALIASED.sub(lambda m: " " * len(m.group(0)), line)
            if args.fill:
                def stamp(m):
                    if m.group(1) != "eqemu":
                        return m.group(0)
                    cid = repo.last_touch(upstream, m.group(2))
                    n = repo.length(cid, m.group(2)) if cid else None
                    if n is None or max(int(m.group(3)), int(m.group(4) or m.group(3))) > n:
                        problems.append(f"{name}:{lineno}: cannot stamp `{m.group(0)}` at {upstream}")
                        return m.group(0)
                    return f"{m.group(1)}:{m.group(2)}@{cid}:{m.group(3)}" + (f"-{m.group(4)}" if m.group(4) else "")
                lines[lineno - 1] = line = NO_ID.sub(stamp, line)
                rest = ALIASED.sub(lambda m: " " * len(m.group(0)), line)
            for m in BARE.finditer(rest):
                problems.append(f"{name}:{lineno}: citation `{m.group(0)}` must be `<alias>:<repo-relative path>@<commit>:<line>`")
        if args.fill and "\n".join(lines) != text:
            (ROOT / name).write_text("\n".join(lines), encoding="utf-8")

    stale = 0
    if repo:
        for name, lineno, alias, path, cid, lo, hi in cites:
            if alias != "eqemu":
                continue
            n = repo.length(cid, path)
            if n is None:
                problems.append(f"{name}:{lineno}: `{path}` does not exist at {cid} (wrong commit or path)")
            elif not repo.is_ancestor(cid, upstream):
                problems.append(f"{name}:{lineno}: {cid} is not in the history of {upstream} (local or feature-branch commit?)")
            elif max(lo, hi) > n:
                problems.append(f"{name}:{lineno}: `{path}:{max(lo, hi)}` is past the end of the file ({n} lines) at {cid}")
            elif repo.changed_since(cid, upstream, path):
                stale += 1
                now = repo.last_touch(upstream, path)
                problems.append(f"{name}:{lineno}: STALE `{path}@{cid}` changed upstream (now @{now}); re-read the cited line, then update the id")

    for p in problems:
        print(p)
    print(f"{len(cites)} aliased citation(s), {len(problems)} problem(s)" + (f" ({stale} stale)" if repo else ""), file=sys.stderr)
    sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main()
