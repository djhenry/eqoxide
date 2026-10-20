# Citing other projects' source

Docs and code comments here cite EQEmu (and, in principle, other projects) by file and line. Those
projects move, so a bare `zone/client.h:100` is only meaningful relative to the revision it was read
from. This page defines how a citation names its project and revision.

## Pinned revisions

| Alias | Project | Pinned commit |
|---|---|---|
| `eqemu` | [EQEmu/Server](https://github.com/EQEmu/Server) (`v23.10.3-82`, 2026-07-01) | `f9129e5ed366ad3887eeccb16f2a747919831108` |

Every aliased citation means "at the commit in this table". There is exactly one pin per alias.

## Syntax

```
<alias>:<repo-relative path>:<line>            eqemu:zone/corpse.cpp:50
<alias>:<repo-relative path>:<line>-<line>     eqemu:common/database.cpp:848-861
```

- The **alias** is mandatory. A bare `corpse.cpp:50`, or `zone/corpse.cpp:50` with no alias, is a
  lint failure.
- The **path** is the full repo-relative path, never a bare filename: `client.cpp` exists in
  `zone/`, `world/` and `loginserver/`.
- A second line number for the same file may be written bare after the first
  (`eqemu:zone/corpse.cpp:1535, :1548`), and an alternative with `/` (`eqemu:zone/spells.cpp:1803/1824`).
- **Name the symbol next to the line** whenever you can (`SendEndLootErrorPacket`,
  `eqemu:zone/corpse.cpp:50`). The symbol survives a pin bump; the line number does not.
- Prose that merely mentions a file without a line (`common/patches/rof2.cpp`) needs no alias; only
  `path:line` citations are linted.

## Checking

```
scripts/check-external-refs.py                      # syntax; CI runs this
scripts/check-external-refs.py --eqemu ~/git/EQEmu  # also: file exists at the pin, line in range
```

The second form reads `git show <pin>:<path>` from your clone and does not touch its working tree.
It cannot tell whether the line still says what the comment claims.

## Bumping a pin

1. Read every citation of the alias against the new commit (`git grep -n 'eqemu:'`), fixing the ones
   that moved.
2. Run `scripts/check-external-refs.py --eqemu <checkout>` with the new hash in the table.
3. Change the hash and description in the table in the same PR as the citation fixes.

A citation that is stale against the pin is a bug in the citation, not a reason to cite a second
revision.

## Adding another project

Add a row with a short lowercase alias and a full 40-hex commit. Citations then use that alias. The
lint rejects an alias that has no row.
