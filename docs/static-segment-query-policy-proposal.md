# Proposal: bounded static collision segment inspection

Status: **design for owner review; no segment operation is implemented by this change**.
Tracked by [issue #1165](https://github.com/djhenry/eqoxide/issues/1165), following the
[owned package reader](static-scene-package-reader.md) and the asset server's
[reviewed inspection design](https://github.com/djhenry/eqoxide_asset_server/blob/ccf56db60230113606fdafbe8dabd3910c720b38/docs/shared-static-collision-design.md).

The first operation inspects the declared static triangle dataset in an explicitly
selected immutable package. EQG and older sources use the same baked contract and
query implementation. A hit or miss describes those triangles; it does not certify
movement, walkable support, navigation, volumes, dynamic actors or world completeness.

## Decision requested

Recommend **certified f64 filtering with adaptive exact fallback** for intersection
classification and nearest-hit ordering. This retains inclusive triangle boundaries
and segment endpoints without a global area, distance or parallelism epsilon. Exact
decisions concern the represented inputs: the decoder's already rounded world
vertices and the supplied endpoints. They are not a claim of exact physical geometry,
native-client equivalence or bitwise reproduction of another client.

The smaller alternative is certified f64 filtering that returns a distinct
`numerical-uncertainty` refusal when the filter cannot prove an answer. It must never
guess a miss or publish a best-so-far hit when an uncertain candidate could win.
This avoids an exact backend, but some valid grazing, shared-edge, endpoint or nearly
equal-distance queries would produce no hit/miss answer. Ordinary f64 arithmetic
with a fixed epsilon is not proposed as an equivalent substitute.

Adaptive exact fallback adds a reviewed arithmetic backend, precision/workspace
limits, independent mathematical test oracles and more adversarial testing. No such
backend has been selected or added. Implementation should follow the owner's choice
of these two numerical approaches; this document does not authorize a dependency by
name. Both approaches also need an explicit work-budget refusal.

Roundoff near a zero determinant can reverse an orientation decision; adaptive
predicates provide an established way to resolve such decisions.
[Shewchuk's predicate research](https://www.cs.cmu.edu/~quake/robust.html) supports
that approach, not a ready-made package-specific segment or nearest-order API.
[Woop, Benthin and Wald's watertight ray/triangle algorithm](https://jcgt.org/published/0002/01/05/)
addresses shared boundaries with conservative tests and higher-precision fallback.
It is useful implementation research, but does not itself supply the exact ordering,
immutable identity or refusal contract proposed here.

## Proposed request and geometry semantics

The package-owned CPU API receives `&StaticScenePackage`, an expected set, zone and
manifest revision, two finite f64 server-axis endpoints, and explicit query limits.
It does not accept a caller-forged mutable collision DTO as a verified package.
Endpoint values are interpreted as their represented binary numbers; input is not
silently narrowed to f32. Equal endpoints are invalid, including equality of signed
zeros. Subtraction or intermediate overflow in the f64 fast path must fall back or
refuse explicitly, even when both input endpoints are finite.

The collision instance matrix and position pool already use server axes. Evaluate
indexed world points using the decoder's existing f32 `Mat4::transform_point3` path,
then widen the resulting finite f32 points. Do not perform a second axis conversion,
evaluate an ideal f64 matrix instead, or intersect an inverse-transformed segment
against a different rounded surface. A narrow shared world-point helper may prevent
these two code paths from drifting.

For each oriented triangle, let `n = (b-a) cross (c-a)` and `d = end-start`, considered
with exact arithmetic over those represented values for classification:

| Rule | Proposed meaning |
| --- | --- |
| Triangle membership | All three edges and vertices are included; no barycentric padding. |
| Segment membership | Parameter `t` is in the closed interval `[0,1]`. |
| Front policy | `inspection-segment-front-v1` accepts crossings with `n dot d < 0`. |
| Both policy | `inspection-segment-both-v1` accepts either nonzero crossing sign. |
| Parallel/coplanar | `n dot d = 0` yields no crossing, including a segment lying in the triangle plane. |
| Tiny nonzero triangles | Remain eligible; no new minimum area or segment length. |
| Nearest result | Smallest exact accepted segment parameter, before rounding output. |
| Equal parameter | Lexicographically smallest `(instance_index, primitive_index, triangle_index)`. |

An endpoint on the surface can hit at `t=0` or `t=1` when the direction satisfies
the selected face policy. A coplanar segment remains a miss even if it traverses
the face. Shared edges may yield several equal-parameter candidates; the tie rule
selects one deterministically within the identified component. It does not promise
stable array indices across different bakes. Triangle index means the zero-based
triplet within that primitive, not an index-buffer byte offset. Mesh index and the
opaque owner ID accompany the winning reference.

The current geometry validator permits very small nonzero triangles and uses no
query epsilon. Its matrix admission tolerance is not a surface/contact tolerance.
If implementation discovers an exactly degenerate transformed triangle despite
package validation, report a geometry-invariant failure rather than skipping it.
Any later change to these geometric rules requires a new policy ID; internal
optimizations that preserve the rules may retain the policy and change the kernel
implementation version.

## Result, identity and refusal semantics

Check finite endpoints and distinctness first, then the expected set/zone/revision,
operation/policy support, and component availability, before geometry traversal.
Use typed outcomes, with that precedence when several conditions fail. Checked
counter overflow is a work-budget refusal. Nonfinite transformed geometry is an
invariant failure. No refusal is converted to a miss, and no partial scan becomes
a hit or miss.

| Outcome | Meaning |
| --- | --- |
| `hit` | Full required scan completed; one nearest accepted triangle selected. |
| `miss` | Full required scan completed with no accepted crossing. |
| `invalid-input` | Nonfinite endpoints, zero-length segment or malformed expected identity. |
| `identity-mismatch` | The selected verified package does not match the expected identity. |
| `unsupported-operation` / `unsupported-policy` | The selected implementation cannot supply the requested contract. |
| `unavailable` | Collision is `not-extracted` or `unsupported`; this is not known-empty. |
| `work-budget` | A checked traversal, arithmetic or workspace limit would be exceeded. |
| `geometry-invariant` | Traversal contradicts validated finite, nondegenerate geometry. |
| `numerical-uncertainty` | Alternative filter-only mode could not certify an answer. |

Once the operation is implemented, `available-empty` returns a miss after the same
request, identity and support checks. Its collision digest still identifies a real
known-empty component. The query cannot infer emptiness from an absent component or
from the visual role. The component's declared policy is authoritative; the initial
API does not offer a request override that changes front/both behavior.

Hit and miss reports echo set, zone, manifest revision, bake revision, descriptor
digest, collision digest, declared dataset scope, policy ID, operation ID and kernel
implementation version. Hits also contain the component-local reference, owner ID,
front/back classification, geometric normal, position and segment parameter. Reports
include traversal and exact-fallback counters so an inspection tool can describe
cost without inventing latency or memory measurements.

With the recommended exact backend, return the position and parameter rounded to
nearest, ties-to-even f64 from their exact values. Ordering and ties use unrounded
values; two distinct hits can print the same rounded parameter. The normal follows
the triangle's orientation even for a back-face hit, rather than flipping toward the
segment. Normalize a scaled f64 approximation of the exact cross product, retaining
its orientation; this normal is approximate and is not a contact-response guarantee.
The backend review must establish that conversions remain finite and do not alter
the selected reference. JSON output must not emit NaN or infinity.

An older revision may be inspected if explicitly requested. These results contain no
active-world/session-generation lease; they must not be used to move an actor or
publish current-world clearance. Offline immutable identity is the inspection scope.

## Work and memory policy

Start with a streaming linear scan of indexed, instanced triangles. Do not retain an
expanded triangle soup or build a grid/BVH in this first operation. This keeps extra
geometry storage small, but each query is O(expanded triangles); repeated queries on
large zones may be slow. No measured latency or total process-memory estimate exists
for this proposed implementation.

Proposed initial defaults, adjustable downward by the caller:

| Query limit | Proposed default |
| --- | ---: |
| Expanded triangles examined | 5,000,000 |
| Server world-point evaluations | 15,000,000 |
| Exact-fallback kernel calls, including ordering/conversion | 100,000 |
| Peak additional arithmetic workspace | 1 MiB |

Charge each triangle and its three world-point evaluations before computing it.
Charge every fallback, including final output conversion, before invoking it. The
exact backend must enforce bounded workspace and have a documented worst-case work
bound for each kernel call over the entire finite f64 endpoint domain. A count cap
alone is insufficient if one call can allocate or refine without bound. The proposed
workspace default requires validation against the chosen backend; insufficient space
returns `work-budget`, not a reduced-precision answer. Reject requests above the
implementation's hard limits rather than silently clamping them.

Known counts exceeding a traversal limit can refuse before any scan. A nearer hit
found before a limit or uncertain predicate cannot be returned as the final answer.
Budget refusal on a valid dataset is expected and does not mean malformed assets.
The decoder's separate limits and its already retained arrays remain outside these
additional query budgets. Accelerator construction, caching and live-query rate
limits are later units; any cache must include collision digest, policy and kernel
version in its key.

## Operation enablement and rollout

Geometry decoding, optional local operations and package minimum requirements are
distinct. An implemented offline inspector may query a verified geometry-only package
whose `requiredOperations` is empty, if its collision data and declared policy are
supported. Empty means no operation required for consuming the package; it is not a
prohibition on optional inspection. Until a real handler passes acceptance, neither
the CLI nor any manifest/client capability claims `static-segment-query-v1` support.

A separate producer/consumer extension may allow exactly
`requiredOperations: ["static-segment-query-v1"]`, retaining empty as valid. Its
descriptor/root manifest capability union must include that operation plus the
existing package and available-role requirements. Required query operation with
unavailable collision refuses; available-empty remains legitimate. Unknown or
duplicate required operations refuse. Retain the typed operations in the sealed
package instead of discarding them after validation.

The collision GLB header continues to require only its geometry capability. A
query operation belongs to descriptor/package requirements, not that geometry
header. Reader contract 2 can remain for this additive, explicitly required capability
if its existing meanings stay unchanged. Older implementations will refuse the new
required operation. Online advertisements remain reader 1/legacy until their own
integration is accepted; this unit does not change the game asset path.

## Implementation sequence and acceptance

1. Obtain the numerical choice for this proposal and freeze the request/result and
   budget contract. Review the arithmetic backend/domain and license separately;
   do not import a predicate library and assume it solves closest-order comparison.
2. Implement the package-owned CPU operation with the shared f32 world-point path,
   explicit outcomes and checked budgets. Keep gameplay ray code unchanged.
3. Add a separate offline `static_segment_query_probe` example using explicit
   assembled files and bounded ingestion. Keep the existing decode-only probe's
   `segment_queries: false` accurate for what that tool does.
4. Independently accept the operation using real producer packages, adversarial
   fixtures and an independent exact oracle. Only then add optional/required
   operation reporting and the paired producer descriptor extension.

Acceptance must distinguish interior/front/back crossings, parallel and coplanar
misses, both endpoints, shared edges/vertices, reversed winding, tiny triangles,
near-parallel cases, extreme finite endpoints, placement rounding and f32 world
collapse. Compare closest order and exact ties under translated/scaled placements,
including hits whose rounded parameters coincide. Exercise unavailable versus
known-empty, wrong immutable identity, unknown policy/operation, every work counter
and workspace refusal, and no best-so-far success on incomplete work. Mutation tests
must detect swapped axes, bypassed f32 placement, reversed face signs, omitted
boundaries, approximate tie ordering and removed budget charges. Repeat against the
actual producer's asymmetric, instanced and known-empty packages.

This proposal changes documentation only, so validation here is contract/source
consistency and independent design review. It has no runtime or live acceptance
claim. The deferred standalone preview refactor
[issue #1164](https://github.com/djhenry/eqoxide/issues/1164) remains a separate unit.
