# NPC rat movement jerky despite lerp

**Summary:** NPC rats (and likely other roaming NPCs) move in a visibly jerky /
stuttering way even though client-side position interpolation (lerp) is applied —
the lerp smooths it somewhat but does not eliminate the stutter.

**Severity:** Low (visual only; does not affect gameplay/correctness).

**Zone / area:** Observed with `a_rodent` NPCs in `neriakc` (Neriak Third Gate);
likely applies to roaming NPC movement in general.

## Steps to reproduce
1. Log in a character in a zone with wandering rats (e.g. Mordeth in `neriakc`,
   which has many `a_rodent###` spawns).
2. Watch a rat wander in the GUI for several seconds.

## Expected
The rat glides smoothly between positions at a steady pace.

## Actual
The rat's motion is jerky/stuttery — it appears to lurch or snap between points.
The existing lerp reduces but does not remove the choppiness.

## Diagnosis notes
- Reported from direct GUI observation (the position/HTTP API doesn't expose the
  per-frame smoothing, so this is a visual-only observation for now).
- Client already interpolates NPC positions (lerp), so the artifact is in the
  *input* to the lerp (update cadence/spacing) or the lerp parameters, not a total
  absence of smoothing.

## Suspected root cause
(unconfirmed) NPC position updates arrive sparsely or are partially dropped, so
the lerp interpolates between far-apart, infrequent samples — smooth within a
segment but snapping at each new sample. This may tie into the unreliable-app-
packet handling for NPC movement (movement updates can arrive as raw unreliable
app packets that the client has historically dropped). Possible fixes: ensure all
NPC movement updates are consumed, and/or lerp toward a predicted position over
the expected update interval rather than snapping to each received point.

## Status
Migrated to GitHub issue https://github.com/djhenry/eqoxide/issues/1 (in-repo tracker deprecated).
