# Citing other projects' source

Docs and code comments here cite EQEmu (and, in principle, other projects) by file and line. Those
projects move, so a bare `zone/client.h:100` is only meaningful relative to the revision it was read
from. A citation here therefore names its project **and the commit of the cited file**.

## Syntax

```
<alias>:<repo-relative path>@<commit>:<line>            eqemu:zone/corpse.cpp@7ab909ee4:50
<alias>:<repo-relative path>@<commit>:<line>-<line>     eqemu:common/database.cpp@3f2a9c1d0:848-861
```

- **`<alias>`** names the project (table below). Mandatory.
- **`<path>`** is the full repo-relative path, never a bare filename: `client.cpp` exists in `zone/`,
  `world/` and `loginserver/`.
- **`<commit>`** is the last commit that touched *that file* when you read it, abbreviated to 9
  characters:
  `git log -1 --abbrev=9 --format=%h <ref> -- <path>`. It is not the project's HEAD, so unrelated
  upstream commits never invalidate a citation. It also makes the citation a permalink:
  `<url>/blob/<commit>/<path>#L<line>`.
- A second line number for the same file may follow bare (`eqemu:zone/corpse.cpp@7ab909ee4:1535, :1548`),
  as may an alternative with `/` (`...:1803/1824`).
- **Name the symbol next to the line** whenever you can (`SendEndLootErrorPacket`,
  `eqemu:zone/corpse.cpp@7ab909ee4:50`). The symbol survives edits; the line number does not.
- Prose that mentions a file without a line (`common/patches/rof2.cpp`) needs no alias; only
  `path:line` citations are linted.
- Do not put a citation in a string the program emits at run time (error messages, log lines). Put
  it in a comment beside the string.

## Aliases

| Alias | Project | URL | Compared against |
|---|---|---|---|
| `eqemu` | EQEmu/Server | https://github.com/EQEmu/Server | `origin/master` |

The last column is the ref used to decide whether a cited file has changed since it was read. There
is deliberately no single pinned commit: each citation carries its own.

## Checking

```
scripts/check-external-refs.py                         # syntax only
scripts/check-external-refs.py --checkout ~/git/EQEmu  # also: file and line exist at the cited commit; not stale
```

`--checkout` reads your clone with `git show` / `git diff` and does not touch its working tree. Update
the clone first (`git fetch`). It prints one line per citation whose file has changed upstream since
the cited commit, with the new id. It cannot tell whether the cited line still says what the comment
claims; that is what you are there to re-read.

## Keeping citations current

Run the check with `--checkout` after fetching upstream. Only citations into files that actually
changed are listed. For each:

1. Look at the cited code at the new commit. Fix the line number, or the comment, if it moved.
2. Replace the id with the one the check printed.

A citation whose file did not change needs nothing, and an old id on it is still correct.

## Writing a new citation

Write `eqemu:zone/corpse.cpp:50` with no id, then run
`scripts/check-external-refs.py --checkout ~/git/EQEmu --fill`. It stamps the last commit that touched
each such file on the upstream ref and checks the line is in range. Read the line at that commit
first: the id asserts that is what you read.

## Adding another project

Add a row with a short lowercase alias, its URL and the ref to compare against. The lint rejects an
alias that has no row. (`--checkout` currently verifies only `eqemu`; extend `main()` to take a clone
per alias when a second project is cited.)
