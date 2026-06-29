# Door spawn names decode to garbage / empty in arena

**Summary:** Some door records in `arena` arrive with non-printable or empty `name` strings (e.g. `"��5�"`, `""`), so those doors can't match a model and fall back to plain boxes.

**Severity:** Low (cosmetic — doors render as fallback boxes; no crash). Possibly a struct-offset/parse bug worth confirming.

**Zone / area:** `arena` (observed); may affect other zones.

## Steps to reproduce
1. Launch client (`--config claude`), zone into `arena`.
2. Grep the log for door warnings:
   `grep "doors: missing model" /tmp/eqoxide-*.log`

## Expected
Each door has a clean ASCII model name (e.g. `POKTELE500`) that maps to a door/object model.

## Actual
```
WARN eqoxide::renderer: doors: missing model "" for door 161 — using fallback box
WARN eqoxide::renderer: doors: missing model "��5�" for door 79 — using fallback box
WARN eqoxide::renderer: doors: missing model "POKTELE500" for door 77 — using fallback box
```
Door 77's name is clean; doors 79 and 161 are garbage/empty. (POKTELE500 is also "missing"
but that's the model-not-baked issue, not a name-decode issue.)

## Diagnosis notes
- The clean name on door 77 vs. garbage on 79/161 suggests either a variable-length record
  misalignment in the door-spawn parser, or genuinely empty/odd names in the server data.
- The garbage-name warning comes from `note_missing_door_models` (renderer.rs:850) using
  `door.name`; the name originates in the OP_SpawnDoor/door-struct parse.
- Not yet confirmed whether this is a parse misalignment (off-by-N in the door struct) or
  legitimate empty door names. Needs a look at the door-struct decode against the RoF2 layout.

## Suspected root cause
(unconfirmed) Door-struct field offset/length mismatch in the door-spawn parser for RoF2,
causing the name field to read past/short for some records. Alternatively, benign empty names
in server data. Verify against the RoF2 door struct before changing parsing.

## Status
Open
