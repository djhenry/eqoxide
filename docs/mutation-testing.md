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
| `UNUSABLE` | the log had neither the sentinel nor a build failure | nothing, in either direction — the harness could not see whether cargo built anything. No spec may declare it, and any occurrence fails the run |

`UNUSABLE` exists because "every row is `INVALID`" is indistinguishable from "the harness is
broken", and the second must not be able to masquerade as the first. Measured cause:
`CARGO_TERM_QUIET=true` makes cargo omit the `Finished` line entirely, so a mutant that built and
was caught would otherwise be scored as one that never built.

`INVALID` is the reason this script exists in the form it does. `cargo test` exits `101` **both**
when the crate fails to compile and when a test fails, so the exit code cannot distinguish them
(measured on a throwaway crate: a failing test exits 101, a type error exits 101). Reporting an
`INVALID` mutant as `RED` is a false claim of evidence — the tooling-layer version of the
agent-honesty invariant that ranks a confident falsehood above a loud crash.

So the verdict is derived from a **compile sentinel** first and the exit code only afterwards:

* the sentinel is the line ``Finished `<profile>` profile`` that cargo prints once the requested
  unit graph has built. The profile name is matched rather than hard-coded, because
  `cargo test --release` prints ``Finished `release` profile`` — measured — so a `[run] command`
  carrying `--release` or `--profile` would otherwise turn every mutant unbuilt while still looking
  like a real table;
* it is searched in output that has first been stripped of ANSI escapes, because cargo decorates
  that line in some environments and the escapes land *inside* the sentinel (measured: colour on
  the CI runner puts a reset between `Finished` and the profile name; a local
  `CARGO_TERM_COLOR=always` run additionally wraps the profile name in an OSC-8 hyperlink, putting
  a whole URL inside it). Unstripped, a mutant that compiled and ran is scored as one that never
  built, which silently downgrades an entire table to "untested" — the exact failure this file is
  about, in the direction that under-claims;
* `RED` and `GREEN` are produced at exactly one place in `scripts/mutate.py`, inside a function
  that cannot be called without a `CompileProof`;
* a `CompileProof` cannot be constructed from a line that does not match the sentinel;
* **a verdict's evidence is produced by the same call that produces the verdict.** An earlier
  version carried the killer test names in a field beside the verdict and substituted them into the
  evidence column whenever they were non-empty, with no verdict guard — which printed rows saying
  `INVALID` (did not build, nothing was tested) next to ``killed by `some::test` `` in the same row.
  A field maintained *alongside* a verdict can always drift from it, so there is no such field.

Where the guards actually are, counted by mutating the file rather than by reading it:

* `find_compile_proof` and the sentinel pattern are the proof-or-None decision itself, and **one
  edit to either is enough to print a never-compiled mutant as `RED`** — forging the `return None`,
  or changing the pattern to something a compile-error log contains. What catches both is
  `--self-test` **case 0**, which asserts that plain cargo output is recognised *and* that a
  compile-error log yields no proof; case 12 applies both one-edit mutants and shows case 0 failing
  on each.
* **Downstream** of that decision, three guards must all be removed before the same thing happens:
  the `proof is None` gate, the type assertion inside `verdict_from_proof`, and that function's read
  of the proof line. Wrapping the gate alone turns an `INVALID` into a loud crash, never a verdict —
  cases 8 and 9.

That downstream count holds for **any** log shape only because `verdict_from_proof` reads the proof
before it branches, and it says so in a comment. It did not always. An independent reviewer mutated
the file and measured a **two**-edit path: with a `[run] command` that is not cargo, a log can carry
libtest-shaped failure lines and no sentinel, and the killers branch used to return before anything
touched the proof — so the gate plus the type assertion were enough to print
``RED | killed by `tests::is_seven` `` and **exit 0** for a run that built nothing. The fix was to
move the read above the branches rather than to qualify the sentence; **case 15** replays that exact
two-edit composition, pins that it now raises, and pins that a third edit is still required to reach
the false `RED`.

If a future cargo changes the wording of that line, `--self-test` fails loudly: its `RED` and
`GREEN` cases would stop matching their declared expectations.

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
  file = "crates/<crate>/src/<module>.rs"   # a placeholder — this example is not runnable as-is
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
command. (`scripts/mutants/` is a convention, not an existing directory — the first spec creates
it.)

## Safety

These rules are enforced by the script, not by reviewer vigilance:

* **An anchor must occur exactly once**, checked in memory before anything is written. An
  ambiguous anchor is refused, never guessed at.
* **A replacement identical to its anchor is refused.** A no-op mutant always scores `GREEN` and
  would read as evidence that a behaviour is uncovered when nothing was perturbed.
* **The restore is verified by sha256.** The original bytes are held in memory, written back, then
  re-read from disk and hashed. A mismatch aborts the whole run, loudly.
* **A spec cannot reach outside `--root`.** An absolute `file`, or one that resolves out of the
  root via `../`, is refused; so is a mutant `id` that could name a path, since the id becomes a
  log filename.
* **The harness never invokes git.** `git checkout`, `git restore` and `git stash` are repo-global:
  in this repo a stash from one worktree has swallowed another worktree's uncommitted work. The one
  place `scripts/mutate.py` spawns a subprocess refuses an argv naming the git executable. That
  catches the accident — a spec whose `command` reaches for git — not a deliberate `sh -c 'git …'`.
  (`scripts/mutate_selftest.py` spawns cargo and python of its own, deliberately, and is not
  covered by that check; it takes no spec input.) If a restore ever does fail, fix the file by
  hand; do not reach for git.

## The self-test

`scripts/mutate.py --self-test` builds a throwaway cargo crate in a temporary directory — never
part of any eqoxide workspace, wherever `TMPDIR` points — and drives the real command-line entry
point against it. It exercises the failure modes, not just a happy path:

| case | scenario | required outcome |
|---|---|---|
| 0 | verbatim decorated `Finished` lines, both measured forms | recognised as proof after stripping, and *not* before |
| 1 | anchor occurs zero times | refuse, exit 2 |
| 2 | anchor occurs twice | refuse, exit 2 |
| 3 | replacement identical to anchor | refuse, exit 2 |
| 4 | mutant that does not compile | `INVALID`, and no row of the table says `RED` |
| 5 | mutant that compiles and is caught | `RED` |
| 6 | mutant that compiles and is not caught | `GREEN` |
| 7 | two edits that each survive alone | `GREEN`, `GREEN`, and `RED` for the pair |
| 8 | the harness's own sentinel gate wrapped away | loud crash, never a verdict |
| 9 | that gate, the type assertion, and the proof read behind them, all wrapped away | only now is a non-compiling mutant printed as `RED` |
| 10 | the harness's restore write corrupted | the sha256 check fires, and the file really is left different |
| 11 | a real run against a cargo told to colourise (`CARGO_TERM_COLOR=always`) | still `RED`, not `INVALID` |
| 12 | the two **one-edit** paths to a false `RED` | both really do produce one, and case 0's assertions fail on both |
| 13 | `CARGO_TERM_QUIET=true`, where cargo omits the `Finished` line | `UNUSABLE`, and the run fails |
| 14 | the self-test's own check accounting | a case whose body stops running is caught by the count, and a case function missing from the table is caught by introspection |
| 15 | the gate and the type assertion wrapped away, against a `[run] command` that builds nothing | the two-edit composition **raises**; a third edit is still required before a false `RED` is printed |
| 16 | `--nocapture --test-threads=1`, where a passing test prints a libtest-shaped decoy | still `RED`, and the evidence names **no** test rather than the innocent one |

Cases 8–10, 12 and 15 mutate a copy of `scripts/mutate.py` using the harness's own edit engine. Case 10
also checks that the alarm is *true* — that the file on disk really does differ — so a restore
check that fired spuriously would not pass as a demonstration. Cases 11 and 13 carry reach
controls: each asserts that the environment really did do the thing under test (colour really
emitted; the sentinel really absent), so neither can pass vacuously. Cases 15 and 16 carry controls
too — that the pristine harness really does score the non-cargo run `UNUSABLE`, and that the decoy
really did land inside libtest's own output line. After each case the self-test re-hashes the
subject file to confirm the tree was restored.

Case 14 is the reach control for all the others. The number of checks is **derived from the checks
themselves** — one list, from which both the total and the failure set are read — and both the
per-case count and the overall total are asserted against a declared constant. Adding or removing a
check therefore fails the self-test until the constant is updated deliberately. This is not
hypothetical bookkeeping: the previous design appended to a separate `failures` list with no total
at all, and wrapping one case body away lost four checks while still printing `SELF-TEST PASSED`
with exit 0.

The case *table* is derived from the case functions for the same reason. A row could otherwise be
dropped in two coordinated edits — the row itself, and the constant — which measurably printed
`SELF-TEST PASSED — 59 checks across 14 cases` with exit 0, the missing case being the one that
proves the two one-edit false-`RED` paths are caught. Every module-level `case_<n>_*` function must
now appear in `CASES` under its own number, so a dropped row leaves its function defined and
undeclared, and that is a failure no one has to remember a number to catch.

It runs in CI alongside the other guards. It is not cheap — it shells out to cargo for every case
that needs a real build — so it lives in the `test` job, next to cargo, rather than in the
dependency-free guard job. No wall-clock figure is quoted anywhere for it: that depends on the
machine and on cargo's cache, and a stale number in tracked text is the defect class this harness
exists to catch.

## Limits — state these rather than over-claiming

* A `GREEN` says the suite does not cover the perturbation. It does not say the code is wrong, and
  it does not say the perturbation is observable at all: a mutant that changes nothing a caller can
  see is `GREEN` and means nothing.
* A `RED` says *some* test failed. Read the killer test names in the table before claiming the test
  you meant is the one that fired — and note the names are **evidence, never the verdict**. They are
  read from libtest's per-test failure line, matched whole with a whitespace-free name. Under
  `--nocapture` a test's own stdout is interleaved into libtest's stream and is indistinguishable
  from it, so a test can print a perfectly-shaped failure line for a test that does not exist; the
  whole-line match rejects the interleaved shape (measured — case 16) but cannot reject a cleanly
  forged one. When no line matches, the row falls back to the build line and names nobody, which is
  the safe direction: a `RED` that names an innocent test is worse than a `RED` that names none.
* The table is only as strong as `[run] command`. A narrowed command makes `GREEN` a weaker claim,
  and the narrowing is part of what you are asserting.
* Every mutant costs one full run of that command.
* The harness reads cargo's output, so cargo's output is part of its trust boundary. Handled and
  covered by the self-test: colour, OSC-8 hyperlinks, and `CARGO_TERM_QUIET`. Under
  `CARGO_TERM_QUIET` libtest also changes shape, so the killer test names are lost as well — which
  is moot, because the run scores `UNUSABLE` and fails.
* Nothing here establishes that a function is *called* in the shipping binary — see the residual
  recorded on #799.
