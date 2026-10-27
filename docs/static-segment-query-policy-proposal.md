# Proposal: bounded static collision segment inspection

Status: **native-fidelity-first direction; revised design for owner review; no segment operation is implemented by this change**.
Tracked by [issue #1165](https://github.com/djhenry/eqoxide/issues/1165), following the
[owned package reader](static-scene-package-reader.md) and the asset server's
[reviewed inspection design](https://github.com/djhenry/eqoxide_asset_server/blob/ccf56db60230113606fdafbe8dabd3910c720b38/docs/shared-static-collision-design.md).

The first operation inspects the declared static triangle dataset in an explicitly
selected immutable package. EQG and older sources use the same baked contract and
query implementation. A hit or miss describes those triangles; it does not certify
movement, walkable support, navigation, volumes, dynamic actors or world completeness.

## Owner direction and native compatibility

The owner prioritizes results close to the native client on native game assets and permits adaptive arithmetic if it meets that goal. This is conditional permission, not approval of exact-crossing semantics or evidence of native equivalence. The earlier two choices varied numerical precision while sharing the same crossing rules; native compatibility also requires different hit rules.

Source investigation, independently checked against the native RoF2 client, establishes these differences for the audited general terrain segment path:

| Behavior | Native terrain path | Earlier exact-crossing proposal |
| --- | --- | --- |
| Triangle acceptance | Closest segment/triangle squared distance at most `2^-23`, approximately `0.000345267` geometry units of separation | An actual surface crossing inside the triangle |
| Default front gate | Direction dot precomputed oriented normal is at most zero | Exact geometric normal dot direction is strictly negative |
| Coplanar segment | Can qualify through proximity, subject to candidate and mode gates | Miss |
| Equal returned parameter | Rounded parameter at most the previous value replaces the prior result; traversal matters | Exact unrounded parameter, then lexicographically smallest reference |
| Very small triangles | Native preparation can mark triangles invalid; source admission needs its own verified policy | Every nonzero triangle remains eligible |

For a concrete kernel distinction, consider the oriented triangle `(0,0,0)`, `(4,0,0)`, `(0,4,0)` and the segment from `(1,3+2^-12,1)` to `(1,3+2^-12,-1)`. Its plane crossing lies outside the face, so exact crossing misses. Its closest squared distance to the hypotenuse is `2^-25`, within the audited native terrain threshold; its direction is front-facing. All values are exactly representable binary numbers. This is an independently checked mathematical/kernel example, not execution of a complete native scene query.

**Recommend a separately named, source-independent segment-proximity inspection policy before selecting an arithmetic backend.** Baking can resolve coordinates, winding and source eligibility. It cannot remove query-dependent proximity, direction, rounding or nearest-hit decisions from the client. The earlier crossing policy IDs must not silently acquire proximity meanings.

Adaptive arithmetic could robustly evaluate a proximity policy, but exact distance decisions are not native rounded arithmetic. Its use remains conditional on demonstrated fidelity. A filter-only exact-crossing kernel also fails to reproduce the native proximity rule. Do not select either earlier alternative solely because it is more precise or simpler.

The audit does not establish one universal native query policy. The terrain threshold is in its world-geometry frame. One audited simple actor path inverse-transforms both endpoints into its model-local frame and applies the same squared threshold without scale compensation. For an ideal positive uniform scale `s`, this corresponds to world squared tolerance `s*s*2^-23`. This is a bounded source finding; native inversion/rounding, other actor paths and activation are not covered. Local small-face admission also occurs before that actor scaling.

Simple model helpers have different equal-parameter replacement behavior, and the native traversal order is not preserved by the current common component. A public scene wrapper also applies a small positional backoff, which must be distinguished from the geometric closest point. Native normal preparation, precision and caller modes need explicit scope. None of these observations certifies movement or full-world membership.

Before implementation, freeze the targeted native-style operation, endpoint conversion, proximity metric and units, source invalid-triangle admission, front/tangent classification, nearest/tie approximation, reported point meaning, policy IDs and work limits. Keep all runtime rules generic; if bake-resolved per-instance parameters are required, extend the common contract rather than dispatching on EQG/WLD. Do not introduce a tolerance field until its meaning and need are verified.

Acceptance must compare the baked dataset and proposed operation against independently established native behavior on representative terrain and placed-actor inputs. Include edge/endpoint/coplanar cases, tolerance boundaries, small triangles, uniform scales and near-ties. Separate modeled native arithmetic from actual native execution, and report discrepancies and untested callers. No corpus agreement rate, bitwise compatibility or performance result has been established. Required backend/domain/license/workspace review and independent producer-to-consumer testing remain gates before operation advertisement.

This adds complexity beyond the original arithmetic choice: segment-to-triangle distance, explicit tolerance and result semantics, native reference testing, and possibly common-contract additions. Source assembly and package identity work can continue independently. Neither the proximity direction nor the alternative below enables gameplay collision.

## Selected metric representation

The current geometry-only contract permits different local vertices and matrices to describe the same world surface. That is insufficient to recover a native query-unit metric. Matrix scale alone cannot reveal whether source scale was folded into vertices or whether a terrain matrix is just an export representation.

A synthetic example makes the distinction concrete. Use world triangle `(0,0,0)`, `(8,0,0)`, `(0,8,0)` and segment `(2,6+2^-10,1)` to `(2,6+2^-10,-1)`. Closest world squared distance is `2^-21`. Representing the same surface with local vertices divided by four and an instance scale of four gives local squared distance `2^-25`. Comparing with the illustrative squared tolerance `2^-23` therefore accepts the local-metric case and refuses the world-metric case. Folding that scale into vertices does not change the surface, but loses the metric if no other contract preserves it. This exact-geometric example illustrates scale and tolerance semantics; its chosen tolerance is not asserted as a native constant, and it is not a native scene or runtime query test.

On 2026-10-14 the owner selected preserving query frames first. The clarified name **preserving supplied source query frames first** describes that same semantic strategy, not a new owner decision. The alternatives below record that choice and its cost:

| Strategy | Baker obligation | Consequence |
| --- | --- | --- |
| **Preserve supplied source query frames first (selected)** | Keep supplied decoded model-local query units and instance poses; retain terrain in its decoded world query-unit frame. A new proximity profile declares that mesh-local units control tolerance. Reject unit-changing output reparameterization and scale baking, or restore the supplied source query frame before export. | Reuses geometry/matrices without adding a metric field, and preserves the supplied source frame for later arithmetic comparison. Constrains collision optimization and authoring transforms; it is a new semantic restriction, not an assumption about existing packages. |
| Bake-resolved generic metric semantics | Resolve source metric context before export and retain sufficient generic query-unit/tolerance data to survive local-unit changes and pose baking. Validate that information through the producer, decoder and query handler. | Permits more geometry optimization but adds common-contract data and tests. A world squared-tolerance scalar reproduces the ideal uniform-scale cutoff; it does not reproduce native local rounding by itself. Exact fields and numerical interpretation need a separate design. |

The selected strategy keeps one common GLB-based runtime standard and preserves supplied source query frames for a future fidelity milestone. This producer assertion does not authenticate the supplied source/game, establish universal EQ/RoF2 units or verify native behavior. Runtime never selects behavior by EQG/WLD, names or raw source flags. It does not promise bit-identical native results. Generic metric data remains a possible later enhancement, outside the first profile. Existing reader/capability requirements must reject unsupported frame declarations; no older package's policy meaning changes silently.


## First implementation unit: explicit frame declaration

Add optional required capability `eqoxide-source-query-frames-preserved-v1` alongside mandatory `static-collision-triangles-v1` at reader contract 2. It carries eqoxide's producer assertion that the supplied source collision query frames are preserved: terrain uses decoded world query units, while models retain decoded model-local query units with scale in instance transforms. Decoding a source's vertex quantization is required; it is not permission to fold an actor's instance scale into its vertex pool. Only the existing exact axis permutations/sign changes occur at the common geometry boundary.

The writer must accept this declaration explicitly, preserve the supplied local pools and matrices, and leave its existing entry point unspecified by default. The decoder and immutable package reader validate exact allowed capability sets and matching component/descriptor/manifest requirements. Report `unspecified` or `declared-source-query-frames-preserved`; do not call it source-verified. Preservation of supplied source frames cannot be proved from bare coordinates. Bakes must establish it, include that policy in their bake revision, and refuse an unresolved required frame.

This unit adds no per-instance metric fields or new query policy meanings. Existing front/both policy strings remain unchanged, required operations remain empty, and every inspection probe continues to report segment queries unimplemented. Future separately named proximity policies must require the source-frame preservation declaration and their own accepted kernel, with reference-engine fidelity established separately. Old frame-unaware clients explicitly refuse the extra required capability; updated clients continue to decode old packages as unspecified. The old explicit `static-collision-query-unit-frames-v1` capability is refused rather than aliased or defaulted to unspecified. Source importer routing and native-asset frame acceptance are separate gates from authored producer/consumer declaration conformance.

## Earlier exact-crossing alternative, retained for comparison

The rest of this document describes the earlier exact-crossing proposal. It is not the selected native-fidelity policy, an implementation authorization or a promise of native results. Its request, result, bounds and rollout ideas may be reused only after the proximity contract is frozen and their compatibility is checked.

The earlier recommendation was **certified f64 filtering with adaptive exact fallback** for intersection
classification and nearest-hit ordering. This retains inclusive triangle boundaries
and segment endpoints without a global area, distance or parallelism epsilon. Exact
decisions concern the represented inputs: the decoder's already rounded world
vertices and the supplied endpoints. They are not a claim of exact physical geometry,
native-client equivalence or bitwise reproduction of another client.

This strengthens the earlier producer design's widened-arithmetic recommendation,
which cautions against mathematical-exactness claims. The owner must explicitly
approve exact decisions over represented inputs before implementation; then update
the producer query-policy prose consistently before advertising the operation.
Existing geometry/policy identifiers are reserved for inspection, not evidence that
this stronger numerical promise has already been implemented or accepted.

The smaller alternative is certified f64 filtering that returns a distinct
`numerical-uncertainty` refusal when the filter cannot prove an answer. It must never
guess a miss or publish a best-so-far hit when an uncertain candidate could win.
This avoids an exact backend, but some valid grazing, shared-edge, endpoint or nearly
equal-distance queries would produce no hit/miss answer. Ordinary f64 arithmetic
with a fixed epsilon is not proposed as an equivalent substitute.

Adaptive exact fallback adds a reviewed arithmetic backend, precision/workspace
limits, independent mathematical test oracles and more adversarial testing. No such
backend has been selected or added. Choosing between these approaches is insufficient
to satisfy the owner's native-result priority. This alternative does not authorize a
dependency by name. Both approaches also need an explicit work-budget refusal.

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
The current validator's ordinary widened cross product is not an exact
nondegeneracy proof: rounding in differences/products can admit an exactly collinear
triangle. An exact degeneracy discovered by the query is therefore an
`unqueryable-geometry` refusal, not proof of corrupt package data. Check it before
the parallel/miss rule and refuse the whole query instead of silently dropping the
triangle. Nonfinite world points or invalid indices are separate invariant failures.
Any later change to these geometric rules requires a new policy ID; internal
optimizations that preserve the rules may retain the policy and change the kernel
implementation version.

## Result, identity and refusal semantics

Check finite endpoints and distinctness first, then query-limit validity, expected
set/zone/revision, operation/policy support, and component availability, before
geometry traversal.
Use typed outcomes, with that precedence when several conditions fail. Checked
counter overflow is a work-budget refusal. Nonfinite transformed geometry is an
invariant failure. No refusal is converted to a miss, and no partial scan becomes
a hit or miss.

| Outcome | Meaning |
| --- | --- |
| `hit` | Full required scan completed; one nearest accepted triangle selected. |
| `miss` | Full required scan completed with no accepted crossing. |
| `invalid-input` | Nonfinite endpoints, zero-length segment, invalid limits or malformed expected identity. |
| `identity-mismatch` | The selected verified package does not match the expected identity. |
| `unsupported-operation` / `unsupported-policy` | The selected implementation cannot supply the requested contract. |
| `unavailable` | Collision is `not-extracted` or `unsupported`; this is not known-empty. |
| `work-budget` | A checked traversal, arithmetic or workspace limit would be exceeded. |
| `unqueryable-geometry` | A triangle admitted by widened validation is exactly degenerate for this operation. |
| `geometry-invariant` | Traversal contradicts validated finite points or index bounds. |
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

Both approaches must return successful positions and parameters rounded to nearest,
ties-to-even f64 from their exact values. The filter-only alternative may succeed
only when it certifies those rounded outputs as well as classification and ordering;
otherwise it returns `numerical-uncertainty`. The exact backend resolves those
uncertainties subject to its budgets. Ordering and ties use unrounded
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

Use unsigned u64 limit/counter values; any serialized input must use unsigned
integer tokens, not floats, strings or negative numbers. Initially these defaults
are also hard maxima. Zero is legal: it allows work requiring none of that resource
and otherwise produces `work-budget`. Validate every supplied limit against its hard
maximum before availability handling, including for known-empty data. Thus an empty
dataset can miss under all-zero budgets, but cannot bypass an invalid limit request.

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

The collision GLB header retains its geometry capability and any explicit source-frame
preservation requirement. A query operation belongs to descriptor/package requirements, not that geometry
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
collapse, plus exactly collinear triangles that widened validation admits. One
discriminating local example is `a=(2^-52,3*2^-52,0)`, `b=(1,3,0)`, `c=(2,6,0)`:
all inputs are representable f32 values and exactly collinear, while ordinary f64
differences/cross products can produce a nonzero result. This is an arithmetic
counterexample, not evidence from a compiled query implementation. Compare closest
order and exact ties under translated/scaled placements,
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
