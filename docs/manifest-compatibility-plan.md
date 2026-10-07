# Manifest reader compatibility implementation plan

> **For agentic workers:** Use the agent-fleet implementation and independent review process. Steps use checkboxes for tracking.

**Goal:** Extend the existing manifest so incompatible assets are rejected before download, cache acceptance, or decoding.

**Architecture:** Keep content/chunk addressing; add a strict manifest envelope with reader requirements and a separate immutable revision. Update both the producer and consumers together. This is the compatibility milestone of the unified asset project, not its geometry migration.

**Tech stack:** Rust, serde/serde_json, blake3, axum, reqwest, ureq.

**Spec:** [Unified baked assets](unified-baked-assets-design.md).

**Tracking:** eqoxide #1142; eqoxide_asset_server #57; parent eqoxide #1126.

## Global constraints

- The owner authorizes a coordinated pre-release breaking cutover; do not maintain dual-format publishing. Preserve old data for rollback, not a second active exporter.
- Use isolated branches; no merge without human review. Leave the held older review findings alone.
- No source-format-dependent client behavior or geometry changes in this milestone.
- One local compilation at a time, low-priority, one job, debug information and incremental compilation disabled. No remote builds.
- Preserve content/chunk hashes and the existing content-digest function.
- Initial reader support is version `1`, capability `legacy-assets-v1`. This describes existing asset consumption, not the future unified geometry contract.

## Shared wire contract

The existing manifest gains required fields (no missing-field compatibility defaults):

```rust
pub struct ReaderRequirements {
    pub reader_version: u32,
    pub capabilities: Vec<String>,
}
// Existing set, digest, files remain unchanged.
// New Manifest fields:
// schema_version: u32 (exactly 1 for this envelope)
// revision: String (lowercase blake3 hex)
// requirements: ReaderRequirements
```

`digest` remains the existing content hash. `revision` hashes the domain bytes `eqoxide-manifest-v1\0`, then compact JSON encoding of this ordered tuple:

```text
(schema_version, set, digest, requirements.reader_version,
 sorted_unique_capability_strings,
 files_sorted_by_path_as_(path,size,blake3,chunks)_tuples)
```

Rust tuples serialize as JSON arrays; use only these ordered arrays/scalars in the revision input. Preserve each file's chunk sequence. Reordering files or capabilities does not alter identity, but a changed requirement, size, chunk layout, or file content does. Reject duplicate file paths and invalid identity/reference encodings. Do not hash the revision into itself.

Request headers:

```text
X-Eqoxide-Asset-Readers: 1
X-Eqoxide-Asset-Capabilities: legacy-assets-v1
```

Reader values are an explicit comma-separated supported-version set, not a claim that a numerically newer reader supports all old contracts. Capabilities are comma-separated tokens matching `[a-z0-9][a-z0-9._-]*`. Header values are limited to 4,096 bytes. The sorted, deduplicated, comma-joined required capability list must also fit that limit, so every valid requirement can be advertised. Reject malformed lists and test the exact 4,096/4,097-byte boundary. For this milestone constants advertise only the implemented version and capability. All bake entry points pass explicit `ReaderRequirements::legacy()`; no implicit universal-compatible default.

On unsupported/missing advertisement, the manifest endpoint returns HTTP 409 and JSON with `reason: "asset_reader_incompatible"` and the required reader version/capabilities. Malformed advertisements return an explicit 400 error. Authentication is still checked first; no-auth mode bypasses authentication only, never compatibility. Gate compatibility before evaluating `If-None-Match`. ETag contains `revision`, not content `digest`.

Client independently verifies schema, reader requirements, requested set, content digest, and revision before requesting chunks or changing assembled assets. Report incompatibility clearly. No invented minimum application release while the application version remains unchanged across development builds.

## Review focus

- Matching ETag from an incompatible client must never yield 304 (server handler tests).
- A cached record from another reader support set must not authorize reuse (client cache tests).
- Metadata-only or re-chunking changes must change revision without changing content digest (both implementations, shared fixture).
- An old digest-shaped storage pointer must not bypass migration/schema validation (server migration tests).
- Conditional response/body must describe one manifest snapshot even during publication (server loads exact pointer once).

## Task 1: Producer envelope, storage migration, and HTTP gate

**Owner/files:** asset-server worker; `src/manifest.rs`, new `src/compatibility.rs`, `src/lib.rs`, `src/build.rs`, `src/server.rs`, `src/sync_client.rs`, `src/main.rs`, and focused tests/fixtures.

**Consumes:** shared wire contract above.
**Produces:** `ReaderRequirements::legacy()`, strict manifest identity validation, explicit-requirement publication, migrated storage, and compatible manifest endpoint.

- [x] Add failing tests for canonical revision identity and compatibility-only changes. For unchanged files assert `old.digest == new.digest` and `old.revision != new.revision` when required reader changes.
- [x] Implement the strict envelope and canonical revision helper. Validate publication input before writing chunks; update every bake call site with explicit legacy requirements.
- [x] Use revision-keyed manifests and atomically replace the latest pointer after a complete manifest write. Read one latest pointer and load that exact revision for a response. Validate stored identity before either 200 or 304.
- [x] Add explicit migration DTOs for numeric-keyed and old digest-keyed manifests. Extend the existing migration command: verify identities and referenced chunk/file data, preserve bytes/chunk IDs, write new revision, then repoint latest. Current valid manifests are idempotent; future/unknown schema is refused, never downgraded. Preserve old manifest files.
- [x] Add handler tests: no header + matching ETag =>409; unsupported version/capability =>409; malformed header =>400; compatible matching revision =>304; changed requirements =>200 or incompatibility; malformed/dangling stored manifest cannot return304. Keep authentication and shrink-prevention tests.
- [x] Update the reusable `SyncClient` to send headers and validate response requirements/identity/requested set before any chunk operation. Test incompatible response causes zero chunk requests; keep existing cold/warm/delta tests.
- [x] Run server test suite; mutate the compatibility gate ordering or remove it and verify regression RED, restore and rerun focused tests. Commit tested changes only.

## Task 2: Client verification, cache boundary, and diagnostics

**Owner/files:** client worker; `src/asset_sync.rs`, new `src/asset_compatibility.rs`, `src/lib.rs`, focused tests/fixtures. Do not change render/collision loading behavior.

**Consumes:** identical shared wire contract and reference fixture.
**Produces:** advertised support and fail-closed manifest/cache checks in existing sync flow.

- [x] Write tests for strict schema and requirement parsing, unknown required capabilities, mismatched requested set, wrong content digest, and wrong revision. Assert zero chunk requests and unchanged synced/assembled state on failure.
- [x] Implement isolated compatibility/revision helpers matching the producer. Keep existing public content-digest computation; use revision for conditional identity.
- [x] Add the support headers to `AssetSync::get_manifest`. Decode structured409 errors into actionable failures without turning other status codes into success.
- [x] Store sufficient manifest envelope/requirements in cached synced records to revalidate against current reader support before conditional requests and304 acceptance. Treat old records lacking the envelope as a cold manifest cache; retain reusable CAS chunks. Do not infer cached compatibility from a revision string.
- [x] Preserve warm-cache repair and fresh-manifest recovery. Revalidate fresh recovery manifests through the same path, never a bypass. Pin zero extra chunk downloads for valid warm-cache migration and rejection of an unsolicited304 without a validated cache record.
- [x] Update existing fixture builders to create valid versioned manifests. Use the producer's shared golden JSON fixture and assert the same content digest/revision; also change one capability and assert failure/revision change.
- [x] Run focused sync tests then the full workspace serially. Independently mutate requirement enforcement and cached compatibility acceptance to demonstrate RED, restore, and rerun affected tests. Commit tested changes only.

## Task 3: Cross-repository acceptance and rollout documentation

**Owner:** orchestrator plus independent reviewer. No shared implementation ownership.

- [x] Run the actual updated server on a reserved loopback port with a synthetic store. Check missing/unsupported header failures and compatible 200/304 behavior with a real HTTP client.
- [x] Exercise the real eqoxide sync transport against that server: compatible cold/warm sync, requirement-only update rejection, and unchanged local assets on rejection. Repeat with the server-side SyncClient. Use a small fixture and no game login for this transport-only feature.
- [x] Reviewer independently checks both diffs, runs suites and at least one meaningful mutation, then repeats the actual HTTP/consumer boundary. Runtime renderer testing is not evidence for this transport boundary and is unnecessary for this milestone.
- [x] Document coordinated binary upgrade, explicit manifest migration, preserved rollback store, reader1 meaning, and that unified GLB geometry remains a later milestone. Do not deploy or alter the running service as part of acceptance.
- [ ] Publish linked PRs with concrete validation and limitations; wait for human review. Do not merge.

## Follow-on milestones

After the compatibility boundary, implement the common geometry/material writer and both source adapters, then explicit static collision/semantic data, then the generic client loader and independent zone acceptance. Each receives its own implementation plan against the approved design; this plan deliberately does not claim those behaviors are delivered by metadata alone.

## Acceptance results

Independent acceptance passed on client `47c5afc6` and server `5ba4ee0`: client workspace 2,261 passed with 51 ignored; server suite 86 passed with 27 ignored. Removing cached compatibility validation and removing the server gate before conditional responses each caused their regression test to fail; both passed after restoration.

A separate loopback server/client run verified cold sync (two chunks, 12 bytes), zero-download warm sync, rejection of a requirement-only update with the entire client cache unchanged, restored compatible reuse, and rejection of corrupt metadata before 304. The server SyncClient cold/warm/delta tests also exercised real TCP. No game login, deployment, or production store modification was needed for this transport milestone.
