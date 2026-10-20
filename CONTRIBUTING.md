# Contributing to eqoxide

eqoxide is a Rust client for EQEmu servers that speaks the RoF2 protocol. This page lists what a
change is expected to do. Setup and running the client are in [README.md](README.md) and
[docs/dev-workflow.md](docs/dev-workflow.md); the code layout is in
[docs/architecture.md](docs/architecture.md).

## Before you open a PR

CI (`.github/workflows/test.yml`) runs these, so run them first:

```sh
python3 scripts/check-external-refs.py                   # citation format, see below
cargo build --workspace --all-targets --locked
cargo test  --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings   # reported, not yet blocking
```

Do not launch the renderer from an agent or CI shell; the harness reaps GUI processes. Use
`./dev-run.sh` in your own terminal (see `docs/dev-workflow.md`).

## Protocol work

This client speaks **RoF2**. Check every struct layout, offset and opcode against the RoF2 sources
(`patch_RoF2.conf`, `rof2_structs.h`, `rof2.cpp`), never the Titanium ones. The rules and the traps
this repo has already hit are in [docs/protocol-notes.md](docs/protocol-notes.md).

## Citing other projects' source

A citation of EQEmu code carries its project and the commit of the cited file:

```
eqemu:zone/corpse.cpp@7ab909ee4:50
```

A bare file-and-line citation with no alias or commit fails the lint. Prefer to name the symbol beside the line number, and do not
put citations in strings the program emits at run time. Syntax, how to stamp a new citation and how
to refresh stale ones: [docs/external-references.md](docs/external-references.md).

## Comments and dead code

- Match the density and idiom of the surrounding code.
- Delete dead code and its commentary. A docstring saying a thing is ignored or unused is not a fix;
  remove the thing.
- Explain *why* (the server behaviour, the issue number, the trap), not what the next line does.

## Behaviour that clients or agents can observe

The HTTP API and its event streams are a contract. If a change alters what a route returns, which
outcomes an action can report, or what an event contains, update
[docs/http-api.md](docs/http-api.md) in the same PR.

## Pull requests

- One concern per PR; reference the issue (`Closes #N`).
- Say what you verified and what you did not. If a change could not be exercised against a live
  server, say so.
- Tests accompany behaviour changes. Packet handlers are pure (`apply_packet(gs, packet)`), so most
  protocol behaviour can be tested without a server.
