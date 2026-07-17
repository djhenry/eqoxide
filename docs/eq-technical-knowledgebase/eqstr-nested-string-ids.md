# OP_FormattedMessage `%T<n>` — nested eqstr string-id resolution (eqoxide#472)

## Symptom

Merchant NPC speech showed up in the chat log as a raw numeric string —
`<Vaelias> 1148` — instead of the actual sentence, because eqoxide's
`eqstr::substitute()` treated the `%T<n>` format token as plain positional
substitution (printing the literal digits) instead of resolving it.

## Root cause

EQEmu's `eqstr_us.txt` templates can embed a **second string_id as one of
their own `%1..%9` args**, marked with the `T` letter-prefix code: `%T<n>`
means "arg `n` in this message's arg array is itself a decimal string_id —
resolve it (as an eqstr template, reusing the SAME outer arg array for its
own `%1..%9` tokens) and substitute the resolved text here." This differs
from `%B<n>` (bold) and the other letter codes, which are plain positional
literal substitutions.

Confirmed on both sides (via the `eq-client-expert` agent, EQEmu source +
the shipped client string table):
- **Server:** `GENERIC_STRINGID_SAY = 554` (`EQEmu/zone/string_ids.h:200`,
  comment `//%1 says '%T2'`). A merchant window opening with a "handy" item
  in stock rolls a random greeting id and sends it as **arg 2** of string_id
  554 (`EQEmu/zone/client_process.cpp:996-1004`):
  ```cpp
  int greet_id = zone->random.Int(MERCHANT_GREETING /*1144*/, MERCHANT_HANDY_ITEM4 /*1148*/);
  MessageString(Chat::NPCQuestSay, GENERIC_STRINGID_SAY /*554*/,
                 npc->GetCleanName(), std::to_string(greet_id).c_str(),
                 GetName(), handy_item->Name);
  ```
  This is `OP_FormattedMessage` (wire `0x1024`), `string_id=554`,
  `args=[npc_name, "1148", player_name, item_name]`.
- **Client data (ground truth):** the shipped `eqstr_us.txt` has
  `554 %1 says '%T2'` and `1148 Welcome to my shop, %3. You would probably
  find a %4 handy.` — the nested template's own `%3`/`%4` bind to the SAME
  outer args (player name, item name), not a re-indexed sub-array.
- **Second call site confirming the pattern:** `SendTellQueue`
  (`EQEmu/zone/client.cpp:3998-4000`) builds
  `MessageString(Chat::EchoTell, TELL_QUEUED_MESSAGE /*5045*/, who,
  string_id_str, message)` — string_id 5045 is `"You told %1 '%T2, %3'"`,
  same "digit-string-id-in-arg-slot-2 + trailing literal args shared with
  the nested template" shape.

`1148` is a real, in-range eqstr id (`MERCHANT_HANDY_ITEM4`, one of the
`1144..1148` merchant-hail pool) — the bug was purely a missing recursion
step on the client side, not a server anomaly or an unresolvable id.

## Fix

`src/eqstr.rs`: `substitute()` now delegates to a new internal
`substitute_inner(template, args, resolve: Option<&HashMap<u32,String>>, depth)`.
When `resolve` is `Some` (i.e. called from `format_id`, which has the loaded
table) and the letter code is `T`, the arg is parsed as a `u32`, looked up
in the table, and — if found — recursively substituted (reusing the SAME
outer `args`) and spliced in, trimmed. A `MAX_NESTED_DEPTH` (4) guard stops
runaway recursion on a cyclic/malformed table; past the guard, or on any
lookup/parse failure, `%T<n>` falls back to the literal digits (same
behavior as before the fix, and the same as every other letter code).

The pure `substitute()` entry point (no table access, used directly by a
few unit tests and any future caller without table access) keeps its old
literal-substitution behavior for `%T` — only `format_id` (and, in the
recursive case, the templates it resolves) gets the nested lookup, since
that's the only path with a table to resolve against.

## Agent-honesty note

This is the concrete case #472 was filed over: a bare numeric id
(`1148`) rendered as if it were the NPC's own words is a "client hands the
agent garbage instead of truth" bug. The fix resolves the real text; the
existing literal-digit fallback (when the table or nested id is
unavailable) is a pre-existing, lower-severity degradation shared with
every other unresolvable eqstr id in this file, not something this fix
introduces.

## Related
- `eq-chat-wire-format-and-routing.md` — `OP_ChannelMessage` wire format
  (a different opcode/mechanism from this note; the merchant hail travels
  over `OP_FormattedMessage`, not `OP_ChannelMessage` — don't conflate the
  two when debugging similar "raw id in chat" reports).
