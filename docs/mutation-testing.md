# Mutation testing

`scripts/mutate.py` runs a committed set of source mutants and scores each one. It exists so that
a mutation table in a pull request body is **reproducible by the reviewer with one command**,
instead of being a figure the reviewer has to take on trust or re-derive with a harness of their
own.

```sh
scripts/mutate.py scripts/mutants/<issue>.toml   # reproduce a PR's whole table
scripts/mutate.py scripts/mutants/<issue>.toml --only some-mutant-id
scripts/mutate.py --self-test                    # prove the harness still discriminates
```

Run it from the repository root. Paths inside a spec are relative to `--root`, which defaults to
the current directory.

## Why a shared harness

A test can assert that a call site is *written* without establishing that it is ever *reached*.
Issue #799 collects the measured members of that family — a trailing comment, a shadowing binding,
an `if false { … }` wrap, `#[cfg(any())]`, an argument-discarding `macro_rules!`, a stray `}` in a
comment that truncated a brace-depth scan. Each is a way to leave a pin green while the pinned
behaviour is gone. (#799 also records the converse direction — the call runs, but not in the
spelling the scan matches — for which the same remedy applies: perturb the behaviour and see
whether anything reds.) Mutation testing is therefore the standard evidence here for any claim of
the form "this check is load-bearing".

Before this script, every PR that used mutation testing wrote a throwaway harness in a scratch
directory, pasted its verdicts, and deleted the harness. That costs each reviewer a full
re-implementation, and the re-implementations differ.

## Verdicts, and what each one licenses you to claim

| verdict | meaning | what you may claim |
|---|---|---|
| `RED` | the mutant compiled, and at least one test failed | the perturbed behaviour is covered — read the killer test names to confirm the test you *meant* is the one that fired |
| `GREEN` | the mutant compiled, and every test passed | the perturbed behaviour is **not** covered by this command's suite |
| `INVALID` | the mutant did not compile | **nothing.** The claim is untested. This is not a RED |
| `TIMEOUT` | the run did not finish | nothing. Also untested |

`INVALID` is the reason this script exists in the form it does. `cargo test` exits `101` **both**
when the crate fails to compile and when a test fails, so the exit code cannot distinguish them
(measured on a throwaway crate: a failing test exits 101, a type error exits 101). Reporting an
`INVALID` mutant as `RED` is a false claim of evidence — the tooling-layer version of the
agent-honesty invariant that ranks a confident falsehood above a loud crash.

So the verdict is derived from a **compile sentinel** first and the exit code only afterwards:

* the sentinel is the line ``Finished `test` profile`` that cargo prints once the requested unit
  graph has built;
* `RED` and `GREEN` are produced at exactly one place in `scripts/mutate.py`, inside a function
  that cannot be called without a `CompileProof`;
* a `CompileProof` cannot be constructed from a line that does not contain the sentinel.

`--self-test` case 8 wraps that gate away and shows the harness crashes loudly rather than
emitting a verdict; case 9 has to wrap away the gate, the type assertion behind it, **and** the
report's use of the proof object before a mutant that never compiled can be printed as `RED`.

If a future cargo changes the wording of that line, `--self-test` fails loudly: its `RED` and
`GREEN` cases would score `INVALID` and mismatch their declared expectations.

## Prefer WRAP mutants to deletion

```rust
// original
if self.armed && zone_changed { self.reset(); }

// WRAP mutant — the check still type-checks and still moves the same values,
// but its outcome is different
if false { if self.armed && zone_changed { self.reset(); } } else { }
```

Deletion is the weaker mutation for two separate reasons:

1. Deleting a check often fails to compile — an unused binding, a missing `else` arm, a moved
   value — which scores `INVALID` and proves nothing.
2. Against the pins #799 is about, deletion gives a **false RED**. A test that asserts
   `source.contains("some_call(")` necessarily fails once the text is deleted, so the deletion
   mutant is scored `RED` and reads as "this call site is covered" — while the same call wrapped
   in `if false { … }` leaves that test green. Deletion measures the text; the wrap measures the
   behaviour.

## Multi-edit mutants

A mutant may carry several edits, applied together. That is how you show a guarantee does not rest
on an unrelated accident: if the check and the code path it falls back to each survive alone, but
the pair is caught, the test was resting on the two agreeing rather than on either one. It is also
how you mutate a check *and* the sweep that is supposed to reach it. All of a mutant's anchors are
resolved in memory before anything is written, so a mutant with one bad anchor never half-applies.

## Spec format

TOML, so anchors and replacements can be written as multi-line literal strings with no escaping.
In a `'''…'''` string a newline immediately after the opening delimiter is dropped, and the
newline before the closing delimiter is kept — so the value below is exactly the three lines shown.

```toml
name = "issue 950 — example (illustrative; adapt the paths and anchors)"

# Optional. Defaults to `cargo test --workspace --locked` — deliberately the same command the
# `test` CI job gates on, so the table is scored against the suite that guards `main`.
# Every mutant in the spec runs this ONE command, so the table is internally comparable.
# Narrowing it narrows what a GREEN means, and the narrowing is then part of your claim.
[run]
command = ["cargo", "test", "--workspace", "--locked"]
timeout_secs = 1800

[[mutant]]
id = "gate-always-open"
description = "WRAP the arm check so it never fires"
expect = "RED"                # optional; a mismatch fails the run

  [[mutant.edit]]
  file = "crates/eqoxide-nav/src/example.rs"
  anchor = '''
        if self.armed {
            self.reset();
        }
'''
  replacement = '''
        if false {
            if self.armed {
                self.reset();
            }
        }
'''
```

Commit the spec you actually ran as `scripts/mutants/<issue-number>.toml` and reference it from the
PR body, or paste it into the body verbatim. Either way the reviewer reproduces the table with one
command.

## Safety

These rules are enforced by the script, not by reviewer vigilance:

* **An anchor must occur exactly once**, checked in memory before anything is written. An
  ambiguous anchor is refused, never guessed at.
* **A replacement identical to its anchor is refused.** A no-op mutant always scores `GREEN` and
  would read as evidence that a behaviour is uncovered when nothing was perturbed.
* **The restore is verified by sha256.** The original bytes are held in memory, written back, then
  re-read from disk and hashed. A mismatch aborts the whole run, loudly.
* **The harness never invokes git.** `git checkout`, `git restore` and `git stash` are repo-global:
  in this repo a stash from one worktree has swallowed another worktree's uncommitted work. The one
  place the script spawns a subprocess refuses an argv naming the git executable. That catches the
  accident — a spec whose `command` reaches for git — not a deliberate `sh -c 'git …'`. If a
  restore ever does fail, fix the file by hand; do not reach for git.

## The self-test

`scripts/mutate.py --self-test` builds a throwaway cargo crate in a temporary directory — never
inside this repo, never part of the workspace — and drives the real command-line entry point
against it. It exercises the failure modes, not just a happy path:

| case | scenario | required outcome |
|---|---|---|
| 1 | anchor occurs zero times | refuse, exit 2 |
| 2 | anchor occurs twice | refuse, exit 2 |
| 3 | replacement identical to anchor | refuse, exit 2 |
| 4 | mutant that does not compile | `INVALID`, and no row of the table says `RED` |
| 5 | mutant that compiles and is caught | `RED` |
| 6 | mutant that compiles and is not caught | `GREEN` |
| 7 | two edits that each survive alone | `GREEN`, `GREEN`, and `RED` for the pair |
| 8 | the harness's own sentinel gate wrapped away | loud crash, never a verdict |
| 9 | that gate, the type assertion, and the proof's use, all wrapped away | only now is a non-compiling mutant printed as `RED` |
| 10 | the harness's restore write corrupted | the sha256 check fires, and the file really is left different |

Cases 8–10 mutate a copy of `scripts/mutate.py` using the harness's own edit engine. Case 10 also
checks that the alarm is *true* — that the file on disk really does differ — so a restore check
that fired spuriously would not pass as a demonstration. After each case the self-test re-hashes
the subject file to confirm the tree was restored.

It runs in CI alongside the other guards.

## Limits — state these rather than over-claiming

* A `GREEN` says the suite does not cover the perturbation. It does not say the code is wrong, and
  it does not say the perturbation is observable at all: a mutant that changes nothing a caller can
  see is `GREEN` and means nothing.
* A `RED` says *some* test failed. Read the killer test names in the table before claiming the test
  you meant is the one that fired.
* The table is only as strong as `[run] command`. A narrowed command makes `GREEN` a weaker claim,
  and the narrowing is part of what you are asserting.
* Every mutant costs one full run of that command.
* Nothing here establishes that a function is *called* in the shipping binary — see the residual
  recorded on #799.
