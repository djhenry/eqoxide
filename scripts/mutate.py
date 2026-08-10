#!/usr/bin/env python3
"""mutate.py — run a committed set of source mutants and score each one honestly.

WHY THIS EXISTS
---------------
Mutation testing is the standard evidence for a behavioural claim in this repo, because a check
whose text is present in the source is not thereby a check that is ever *reached* — issue #799
collects the measured members of that family. Until now every pull request wrote a throwaway
harness in a scratch directory, pasted a table of verdicts into the PR body, and deleted the
harness — so a reviewer could not reproduce the table without re-implementing it, and the
re-implementations differed. This script is the shared one.

Acceptance (issue #950): a reviewer reproduces a PR's whole mutation table with one command and no
re-implementation, and *a mutant that fails to compile can never be reported as RED.*

THE VERDICTS
------------
  RED      the mutant compiled and at least one test failed. The mutated behaviour is covered.
  GREEN    the mutant compiled and every test passed. The mutated behaviour is NOT covered —
           whatever claim rests on it is unsupported by the suite.
  INVALID  the mutant did not compile. **Nothing was tested.** This is not a RED and must never
           be reported as one: an INVALID mutant leaves its claim UNTESTED, and calling it RED is
           a false claim of evidence.
  TIMEOUT  the test command hit its timeout. Also untested.

The INVALID/RED distinction is the whole reason for the compile sentinel below. `cargo test` exits
101 both when the crate fails to compile and when a test fails, so **the exit code cannot tell the
two apart**. The verdict is therefore derived from the sentinel first and the exit code only
afterwards; see `score_run`, `find_compile_proof` and `verdict_from_proof`. RED and GREEN are
produced at exactly one place in this file, inside a function that cannot be called without a
`CompileProof`, and a `CompileProof` cannot be constructed without a line containing the sentinel.
`--self-test` mutates *this file* to show those guards are reached rather than merely present:
wrapping the sentinel gate away turns an INVALID into a loud crash, and it takes wrapping away the
gate, the type assertion behind it, AND the report's use of the proof object before a mutant that
never compiled can be printed as RED.

SAFETY RULES BAKED IN
---------------------
  * An anchor must occur EXACTLY ONCE in the file, and every anchor in a multi-edit mutant is
    checked in memory BEFORE anything is written to disk. An ambiguous anchor is refused, never
    guessed at.
  * A mutant whose replacement equals its anchor is refused — a no-op mutant scores GREEN and
    would read as evidence of an uncovered behaviour that was never actually perturbed.
  * The original bytes are held in memory and written back verbatim; the restore is then re-read
    from disk and verified by sha256. A failed restore aborts the whole run, loudly.
  * This harness NEVER invokes git. `git checkout` / `git restore` / `git stash` are repo-global
    and would destroy other worktrees' uncommitted work. `run_command` — the only place this tool
    spawns a subprocess — refuses an argv that names the git executable. That stops the accident,
    which is the realistic failure; it inspects argv, so it would not stop a deliberate
    `sh -c 'git …'`, and it does not constrain a future second call site.

WRAP, DON'T DELETE
------------------
Prefer a WRAP mutant to a deletion:

    if false { <the original code> } else { <the perturbed behaviour> }

Two reasons. First, deleting a check often fails to compile (an unused binding, a missing `else`
arm, a moved value), which scores INVALID and proves nothing; a wrap keeps the original text in
the tree — so it still type-checks and still moves the same values — while making the check's
*outcome* different. Second, and worse, deletion gives a FALSE RED against the pins #799 is about:
a test that asserts `source.contains("some_call(")` necessarily fails once you delete the text, so
the deletion mutant scores RED and reads as "this call site is covered" — while the same call
wrapped in `if false { … }` leaves that test green. Deletion measures the text; the wrap measures
the behaviour.

USAGE
-----
    scripts/mutate.py path/to/spec.toml            # run every mutant in the spec
    scripts/mutate.py path/to/spec.toml --only ID  # re-run one row of the table
    scripts/mutate.py --self-test                  # prove the harness still discriminates

Run it from the repository root: file paths in a spec are relative to `--root`, which defaults to
the current directory. (This is deliberately not derived from git — see the safety rules.)

EXIT CODES
----------
    0  every mutant ran, and every declared `expect` matched
    1  a verdict did not match its declared `expect`, or a mutant was INVALID/TIMEOUT with no
       declared expectation — i.e. a row of the table proves nothing
    2  the harness refused to act, or a restore could not be verified

SPEC FORMAT — see docs/mutation-testing.md for the annotated version.
"""

from __future__ import annotations

import argparse
import hashlib
import subprocess
import sys
import tempfile
import tomllib
from dataclasses import dataclass
from pathlib import Path

# The line `cargo test` prints once the whole requested unit graph has been built. Its presence is
# the ONLY reliable evidence that the mutant compiled: a compile failure and a test failure both
# exit 101. Keep this string in sync with cargo; `--self-test` fails loudly if it stops appearing.
COMPILE_SENTINEL = "Finished `test` profile"

RED = "RED"
GREEN = "GREEN"
INVALID = "INVALID"
TIMEOUT = "TIMEOUT"
VERDICTS = (RED, GREEN, INVALID, TIMEOUT)

# Deliberately the same command the `test` CI job gates on, so a mutation table is scored against
# the same suite that guards `main`. Note it is NOT `--all-targets`: that flag adds nothing here
# (the repo has no benches and no examples) and silently drops the doc-test targets — the reasoning
# is spelled out at the `cargo test --workspace` step in .github/workflows/test.yml.
DEFAULT_COMMAND = ["cargo", "test", "--workspace", "--locked"]
DEFAULT_TIMEOUT_SECS = 1800


class Refusal(Exception):
    """The harness declined to act. Always fatal — never downgraded to a verdict."""


# --------------------------------------------------------------------------------------------
# Verdict derivation. This is the honesty-critical part of the file.
# --------------------------------------------------------------------------------------------


@dataclass(frozen=True)
class CompileProof:
    """Evidence that the mutant built: the actual sentinel line from the log.

    Constructing one requires a line containing the sentinel, so a `CompileProof` cannot be
    conjured from a log that lacks it.
    """

    line: str

    def __post_init__(self) -> None:
        if COMPILE_SENTINEL not in self.line:
            raise AssertionError(
                "CompileProof built from a line without the compile sentinel: " + repr(self.line)
            )


def find_compile_proof(output: str) -> CompileProof | None:
    """Return proof that the build finished, or None. The whole log is in memory, so this can
    never read a partially written file — a hazard that has produced wrong counts before."""
    for line in output.splitlines():
        if COMPILE_SENTINEL in line:
            return CompileProof(line)
    return None


def verdict_from_proof(proof: CompileProof, returncode: int) -> str:
    """The ONLY place RED or GREEN is produced. It cannot run without a CompileProof."""
    if not isinstance(proof, CompileProof):
        raise AssertionError("verdict_from_proof called without a CompileProof")
    return GREEN if returncode == 0 else RED


def score_run(output: str, returncode: int, timed_out: bool) -> tuple[str, str]:
    """Score one mutant run. Returns (verdict, one-line evidence)."""
    if timed_out:
        return TIMEOUT, "the test command hit its timeout — nothing was tested"
    proof = find_compile_proof(output)
    if proof is None:
        return INVALID, "no compile sentinel in the log — the mutant did not build, nothing was tested"
    return verdict_from_proof(proof, returncode), proof.line.strip()


def killer_tests(output: str, limit: int = 3) -> list[str]:
    """Names of the tests that failed, as evidence for a RED. Cosmetic, not load-bearing."""
    names = []
    for line in output.splitlines():
        stripped = line.strip()
        if stripped.startswith("test ") and stripped.endswith(" ... FAILED"):
            names.append(stripped[len("test ") : -len(" ... FAILED")])
    return names[:limit]


# --------------------------------------------------------------------------------------------
# Files: mutate in memory, restore from memory, verify by hash.
# --------------------------------------------------------------------------------------------


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class FileGuard:
    """Holds one file's original bytes in memory and puts them back, verifiably."""

    def __init__(self, path: Path) -> None:
        self.path = path
        self.original = path.read_bytes()
        self.digest = sha256_bytes(self.original)

    def write(self, text: str) -> None:
        self.path.write_bytes(text.encode("utf-8"))

    def restore_and_verify(self) -> None:
        self.path.write_bytes(self.original)
        after = self.path.read_bytes()
        actual = sha256_bytes(after)
        if actual != self.digest:
            raise Refusal(
                "RESTORE VERIFICATION FAILED for {}: expected sha256 {}, found {}. "
                "The working tree is NOT back to its pre-mutation state. Fix it by hand — do NOT "
                "use git checkout/restore/stash, they are repo-global.".format(
                    self.path, self.digest, actual
                )
            )


def apply_edit(text: str, anchor: str, replacement: str, where: str) -> str:
    """Apply one edit to in-memory text, refusing anything ambiguous or pointless."""
    if not anchor:
        raise Refusal(f"{where}: empty anchor")
    occurrences = text.count(anchor)
    if occurrences != 1:
        raise Refusal(
            "{}: anchor occurs {} time(s), expected exactly 1. Widen the anchor until it is "
            "unique; the harness will not guess which occurrence you meant.\n--- anchor ---\n{}".format(
                where, occurrences, anchor
            )
        )
    if replacement == anchor:
        raise Refusal(
            f"{where}: replacement is identical to the anchor. A no-op mutant always scores "
            "GREEN and would read as evidence that a behaviour is uncovered when nothing was "
            "actually perturbed."
        )
    return text.replace(anchor, replacement, 1)


# --------------------------------------------------------------------------------------------
# Spec loading
# --------------------------------------------------------------------------------------------


@dataclass(frozen=True)
class Edit:
    file: str
    anchor: str
    replacement: str


@dataclass(frozen=True)
class Mutant:
    id: str
    description: str
    expect: str | None
    edits: tuple[Edit, ...]


@dataclass(frozen=True)
class Spec:
    path: Path
    name: str
    command: tuple[str, ...]
    timeout_secs: int
    mutants: tuple[Mutant, ...]


def _require(table: dict, key: str, where: str) -> object:
    if key not in table:
        raise Refusal(f"{where}: missing required key '{key}'")
    return table[key]


def load_spec(path: Path) -> Spec:
    try:
        raw = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise Refusal(f"cannot read spec {path}: {exc}") from exc

    run = raw.get("run", {})
    command = tuple(run.get("command", DEFAULT_COMMAND))
    if not command:
        raise Refusal(f"{path}: [run] command is empty")
    timeout_secs = int(run.get("timeout_secs", DEFAULT_TIMEOUT_SECS))

    raw_mutants = raw.get("mutant", [])
    if not raw_mutants:
        raise Refusal(f"{path}: no [[mutant]] entries")

    mutants: list[Mutant] = []
    seen: set[str] = set()
    for index, entry in enumerate(raw_mutants):
        where = f"{path}: mutant #{index + 1}"
        ident = str(_require(entry, "id", where))
        if ident in seen:
            raise Refusal(f"{where}: duplicate id '{ident}'")
        seen.add(ident)
        expect = entry.get("expect")
        if expect is not None and expect not in VERDICTS:
            raise Refusal(f"{where}: expect '{expect}' is not one of {', '.join(VERDICTS)}")
        raw_edits = entry.get("edit", [])
        if not raw_edits:
            raise Refusal(f"{where}: no [[mutant.edit]] entries")
        edits = tuple(
            Edit(
                file=str(_require(e, "file", f"{where} edit #{i + 1}")),
                anchor=str(_require(e, "anchor", f"{where} edit #{i + 1}")),
                replacement=str(_require(e, "replacement", f"{where} edit #{i + 1}")),
            )
            for i, e in enumerate(raw_edits)
        )
        mutants.append(
            Mutant(
                id=ident,
                description=str(entry.get("description", "")),
                expect=expect,
                edits=edits,
            )
        )

    return Spec(
        path=path,
        name=str(raw.get("name", path.stem)),
        command=command,
        timeout_secs=timeout_secs,
        mutants=tuple(mutants),
    )


# --------------------------------------------------------------------------------------------
# Running
# --------------------------------------------------------------------------------------------


def run_command(argv: tuple[str, ...], cwd: Path, timeout: int) -> tuple[str, int, bool]:
    """Spawn the test command. The single chokepoint for subprocesses in this harness.

    It refuses to run git. `git checkout` / `git restore` / `git stash` are repo-global: a stash
    from one worktree has swallowed another worktree's uncommitted work in this repo. Restoring
    from the in-memory copy is the only supported undo, so there is no reason for this tool to
    ever invoke git. The check inspects argv, so it catches the accident — a spec whose `command`
    reaches for git — not a deliberate `sh -c 'git …'`.
    """
    for part in argv:
        if part == "git" or part.endswith("/git"):
            raise Refusal(
                "refusing to run git: '{}' in {}. git checkout/restore/stash are repo-global and "
                "destroy other worktrees' work; mutants are undone from the in-memory copy.".format(
                    part, " ".join(argv)
                )
            )
    try:
        completed = subprocess.run(
            list(argv),
            cwd=str(cwd),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        partial = exc.output or b""
        return partial.decode("utf-8", "replace"), -1, True
    return completed.stdout.decode("utf-8", "replace"), completed.returncode, False


@dataclass
class Result:
    mutant: Mutant
    verdict: str
    evidence: str
    killers: tuple[str, ...]
    log_path: Path


def run_mutant(mutant: Mutant, spec: Spec, root: Path, log_dir: Path) -> Result:
    """Apply one mutant, run the suite, restore, verify. Never leaves the tree mutated."""
    guards: dict[Path, FileGuard] = {}
    pending: dict[Path, str] = {}

    # Phase 1: resolve and check EVERY edit in memory. Nothing is written until all anchors have
    # been confirmed unique, so a multi-edit mutant with one bad anchor never half-mutates a file.
    for index, edit in enumerate(mutant.edits):
        target = (root / edit.file).resolve()
        if not target.is_file():
            raise Refusal(f"mutant '{mutant.id}' edit #{index + 1}: no such file: {target}")
        if target not in guards:
            guards[target] = FileGuard(target)
            pending[target] = guards[target].original.decode("utf-8")
        pending[target] = apply_edit(
            pending[target],
            edit.anchor,
            edit.replacement,
            f"mutant '{mutant.id}' edit #{index + 1} ({edit.file})",
        )

    # Phase 2: write, run, and always restore.
    try:
        for target, text in pending.items():
            guards[target].write(text)
        output, returncode, timed_out = run_command(spec.command, root, spec.timeout_secs)
    finally:
        for guard in guards.values():
            guard.restore_and_verify()

    verdict, evidence = score_run(output, returncode, timed_out)
    log_path = log_dir / f"{mutant.id}.log"
    log_path.write_text(output, encoding="utf-8")
    return Result(
        mutant=mutant,
        verdict=verdict,
        evidence=evidence,
        killers=tuple(killer_tests(output)),
        log_path=log_path,
    )


# --------------------------------------------------------------------------------------------
# Reporting
# --------------------------------------------------------------------------------------------


def report(spec: Spec, results: list[Result], log_dir: Path) -> int:
    print()
    print(f"## Mutation table — {spec.name}")
    print()
    print(f"Spec: `{spec.path}`  ")
    print(f"Command: `{' '.join(spec.command)}`  ")
    print(f"Compile sentinel: `` {COMPILE_SENTINEL} `` (a missing sentinel is INVALID, not RED)")
    print()
    print("| mutant | edits | verdict | expected | outcome | evidence |")
    print("|---|---|---|---|---|---|")
    status = 0
    for result in results:
        expect = result.mutant.expect or "—"
        if result.mutant.expect is None:
            outcome = "untested" if result.verdict in (INVALID, TIMEOUT) else "reported"
            if result.verdict in (INVALID, TIMEOUT):
                status = 1
        elif result.mutant.expect == result.verdict:
            outcome = "ok"
        else:
            outcome = "**MISMATCH**"
            status = 1
        evidence = result.evidence
        if result.killers:
            evidence = "killed by " + ", ".join(f"`{k}`" for k in result.killers)
        print(
            f"| `{result.mutant.id}` | {len(result.mutant.edits)} | {result.verdict} | "
            f"{expect} | {outcome} | {evidence} |"
        )
    print()
    for result in results:
        if result.mutant.description:
            print(f"* `{result.mutant.id}` — {result.mutant.description}")
    print()
    print(f"Logs: `{log_dir}`")
    print()
    print(
        "Legend: RED = mutant compiled and a test failed (behaviour is covered). "
        "GREEN = mutant compiled and the suite still passed (behaviour is NOT covered). "
        "INVALID = the mutant did not compile, so **nothing was tested** — it is not a RED. "
        "TIMEOUT = the run did not finish, also untested."
    )
    return status


# --------------------------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------------------------


def run_spec(spec_path: Path, root: Path, only: list[str], log_dir: Path | None) -> int:
    spec = load_spec(spec_path)
    mutants = list(spec.mutants)
    if only:
        wanted = set(only)
        unknown = wanted - {m.id for m in mutants}
        if unknown:
            raise Refusal(f"--only names no such mutant: {', '.join(sorted(unknown))}")
        mutants = [m for m in mutants if m.id in wanted]
    if log_dir is None:
        log_dir = Path(tempfile.mkdtemp(prefix="mutate-logs-"))
    log_dir.mkdir(parents=True, exist_ok=True)

    results: list[Result] = []
    for mutant in mutants:
        print(f"[mutate] {mutant.id}: applying {len(mutant.edits)} edit(s)", flush=True)
        result = run_mutant(mutant, spec, root, log_dir)
        print(f"[mutate] {mutant.id}: {result.verdict} — {result.evidence}", flush=True)
        results.append(result)
    return report(spec, results, log_dir)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description="Run a committed set of source mutants and score each one honestly.",
    )
    parser.add_argument("spec", nargs="?", type=Path, help="path to a mutant spec (.toml)")
    parser.add_argument(
        "--root",
        type=Path,
        default=Path.cwd(),
        help="directory the spec's file paths are relative to, and the cwd for the test command "
        "(default: the current directory)",
    )
    parser.add_argument("--only", action="append", default=[], help="run only this mutant id")
    parser.add_argument("--logs", type=Path, default=None, help="directory to write run logs into")
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="prove the harness still discriminates: refuses ambiguous anchors, scores a "
        "non-compiling mutant INVALID rather than RED, and verifies its own restore",
    )
    args = parser.parse_args(argv)

    try:
        if args.self_test:
            # The self-test lives beside this file so the honesty-critical logic above stays
            # readable. It is only ever imported on this path.
            sys.path.insert(0, str(Path(__file__).resolve().parent))
            from mutate_selftest import self_test

            return self_test()
        if args.spec is None:
            parser.error("a spec path is required unless --self-test is given")
        return run_spec(args.spec, args.root.resolve(), args.only, args.logs)
    except Refusal as exc:
        print(f"\nmutate: REFUSED — {exc}\n", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
