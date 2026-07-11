# RoF2 Book/Note Reading — OP_ReadBook

Single opcode, single struct name (`BookRequest_Struct`), used for **both**
directions on the RoF2 wire. Confirmed in `EQEmu/common/patches/rof2.cpp` +
`EQEmu/common/patches/rof2_structs.h`.

## 1. Opcode

`OP_ReadBook = 0x72df` — `EQEmu/utils/patches/patch_RoF2.conf:300`.

Same opcode both ways (client request and server reply both go out as
`OP_ReadBook`). Confirmed: `ENCODE(OP_ReadBook)` at `rof2.cpp:3347` (server→client)
and `DECODE(OP_ReadBook)` at `rof2.cpp:6031` (client→server) both key off the
same opcode name; the strategy dispatcher maps both to `0x72df`.

After the reply, RoF2 (and every SoF+ client) also gets a zero-payload
`OP_FinishWindow` (`opcode 0x7349`, `patch_RoF2.conf:457`) — sent unconditionally
after `ReadBook()` runs, gated on `ClientVersion() >= SoF`
(`EQEmu/zone/client_packet.cpp:13205-13208`). eqoxide should expect/tolerate
this trailing empty packet after a book reply.

## 2. Wire struct — `RoF2::structs::BookRequest_Struct` (8216 bytes, FIXED size, both directions)

`EQEmu/common/patches/rof2_structs.h:2899-2908`:

```
/*0000*/ uint32 window;                          // 0xFFFFFFFF = open new window; else target window id
/*0004*/ TypelessInventorySlot_Struct invslot;    // 8 bytes: Slot/SubIndex/AugIndex/Unknown01 (int16 each)
/*0012*/ uint32 type;                             // 0=Scroll, 1=Book, 2=ItemInfo (BookType namespace, emu_constants.h:786-790)
/*0016*/ uint32 target_id;                        // client's current target
/*0020*/ uint8  can_cast;                         // reply-only; ignored by server on request
/*0021*/ uint8  can_scribe;                       // reply-only; ignored by server on request
/*0022*/ char   txtfile[8194];                    // NUL-terminated; request=filename key, reply=book text
/*8216*/ (total struct size)
```

`TypelessInventorySlot_Struct` (`rof2_structs.h:70-77`, 8 bytes):
`int16 Slot; int16 SubIndex; int16 AugIndex; int16 Unknown01;` — no `Type` field
(request is always implicitly `typePossessions`; bank/shared-bank items cannot
be read via this opcode — see `ServerToRoF2TypelessSlot`/`RoF2ToServerTypelessSlot`
only handle `EQ::invtype::typePossessions`, `rof2.cpp:7051-7078`).

**Both the request AND the reply are exactly 8216 bytes on the wire.** This is
not a length-prefixed/variable packet on RoF2 — proven by the macros:
- `DECODE(OP_ReadBook)` calls `DECODE_LENGTH_EXACT(structs::BookRequest_Struct)`
  (`rof2.cpp:6033`) — rejects any inbound `OP_ReadBook` whose size != 8216.
- `ENCODE(OP_ReadBook)` calls `SETUP_DIRECT_ENCODE(BookText_Struct, structs::BookRequest_Struct)`
  which expands to `ALLOC_VAR_ENCODE(eq_struct, sizeof(eq_struct))`
  (`ss_define.h:41-43,54-58`) — **always** allocates a fresh
  `sizeof(structs::BookRequest_Struct)` = 8216-byte buffer and `memset`s it to 0
  first, regardless of how long the actual book text is. The server-internal
  variable-length packet (see below) is copied in via `strn0cpy(eq->txtfile,
  emu->booktext, sizeof(eq->txtfile))` (`rof2.cpp:3361`), which NUL-terminates
  and zero-pads the remainder (already zero from the memset).

So: eqoxide must send exactly 8216 bytes for the request, and must expect
exactly 8216 bytes for the reply. Books/notes longer than 8193 characters get
silently truncated by `strn0cpy` server-side.

## 3. What the client must send in the request

`Handle_OP_ReadBook` (`EQEmu/zone/client_packet.cpp:13195-13209`) validates
`app->size == sizeof(BookRequest_Struct)` then calls `Client::ReadBook(b)`
(`EQEmu/zone/client.cpp:2674-2738`):

```cpp
const std::string& text_file = book->txtfile;   // <-- client-supplied filename, NOT server-derived
if (text_file.empty()) return;
auto b = content_db.GetBook(text_file);          // DB lookup keyed on client's txtfile
...
auto inst = const_cast<EQ::ItemInstance*>(m_inv[book->invslot]);  // invslot used only for type/can_scribe re-check
```

**Ground truth: the server does NOT re-derive the filename from the inventory
slot.** It trusts whatever string the client puts in `txtfile` and looks it up
directly against the `books` table's `name` column
(`EQEmu/common/shareddb.cpp:1292-1314`, `BooksRepository::GetWhere(... "`name` = '{}'" ...)`,
table columns `id/name/txtfile/language` — `common/repositories/base/base_books_repository.h:38-43`).
`invslot` is used only afterward to re-fetch the `ItemInstance` so the server
can override `type` with `inst->GetItem()->Book` and set `can_scribe` from a
tradeskill-recipe lookup — **and only when `invslot <= EQ::invbag::GENERAL_BAGS_END`**
(`client.cpp:2697`), i.e. main inventory/bags only; bank/cursor/etc. skip that
override and the server just echoes back whatever `type` the client sent.

So the real RoF2 client must have cached the item's `Filename` string (parsed
out of the item blob when it received `OP_CharInventory`/`OP_ItemPacket` for
that slot — see §5) and echoes it back verbatim in the `OP_ReadBook` request's
`txtfile` field, alongside `invslot`, `type`, `target_id`, `window`.

Server-side truncation gotcha: the emu-internal `common::BookRequest_Struct`
(`EQEmu/common/eq_packet_structs.h:2798-2804`) that the wire struct gets
decoded into has `char txtfile[20]` — `DECODE(OP_ReadBook)` does
`strn0cpy(emu->txtfile, eq->txtfile, sizeof(emu->txtfile))` (`rof2.cpp:6040`),
i.e. only the **first 19 characters + NUL** of whatever the client sends
actually reach the DB lookup, even though the wire buffer holds up to 8193
usable bytes. Item `Filename` is `char[33]` server-side
(`common/item_data.h:503`) so a book whose filename is 20+ chars will fail to
resolve against a live EQEmu RoF2 server today — not an eqoxide bug, just
something to be aware of if testing against stock EQEmu.

## 4. Reply layout (server → client)

Same `BookRequest_Struct` wire layout as §2. Concretely, on the reply:
- `window` = echo of request (`0xFFFFFFFF` if request `window==0xFF`, i.e. "open a fresh book window") — `rof2.cpp:3352-3355`.
- `invslot` = echo of request's slot (re-encoded).
- `type` = `0/1/2` (Scroll/Book/ItemInfo), possibly overridden from the item's `Book` field as above.
- `target_id` = echo.
- `can_cast` = currently always `0` — server has a `// todo: implement` at `client.cpp:2694`.
- `can_scribe` = `1` iff a `tradeskill_recipes` row has `learned_by_item_id = <item id>` (`client.cpp:2699-2708`), else `0`.
- `txtfile` = the book text itself (from `books.txtfile` column), NUL-terminated, zero-padded to 8194 bytes. Uses `` ` `` (backtick) as the in-text newline character per the struct comment (`eq_packet_structs.h:2784`) — eqoxide should replace backtick with `\n` when rendering.
- Text is garbled per-character if the player's `languages[book.language]` skill is below `Language::MaxValue - book.language`... actually: `GarbleMessage(t->booktext, (Language::MaxValue - m_pp.languages[b.language]))` when `b.language` is a real language (`Language::CommonTongue..Unknown27`) — `client.cpp:2714-2718`. eqoxide's server (EQEmu) already performs this server-side; the client just renders whatever bytes arrive (no client-side un-garbling — same behavior real EQ has always had for foreign-language books).

`can_cast`/`can_scribe` control whether the client's book-reading UI shows a
"Cast" or "Scribe" button (relevant for spell scrolls found as loot — not
applicable to plain notes/books, but worth wiring the fields through even if
eqoxide's UI doesn't yet implement those buttons).

## 5. Item blob — `Item.Book` / `Item.Filename` fields (how the client learns the filename in the first place)

Confirmed in the existing `item-serialization.md` note (§8-9) and re-verified here:

- `RoF2::structs::ItemSecondaryBodyStruct` (`rof2_structs.h:4872-4896`, written at `rof2.cpp:6663-6690`) is a **74-byte fixed struct** whose last two bytes are:
  - `uint8 book;`     (offset within struct: byte 72) — `item->Book` (0 = not a book, 1 = book/note/scroll) — `rof2.cpp:6687`.
  - `uint8 booktype;` (byte 73) — `item->BookType` — `rof2.cpp:6688`.
- Immediately following `ItemSecondaryBodyStruct` on the wire is a **variable-length NUL-terminated C-string**: `item->Filename` (or just `"\0"` if empty) — `rof2.cpp:6692-6694`. This is explicitly documented in the struct comment: `//int32 filename; filename is either 0xffffffff/0x00000000 or the null term string ex: CREWizardNote\0` (`rof2_structs.h:4895`).
- Server-side field: `Item_Struct.Book` (`uint8`, "0=Not book, 1=Book") and `Item_Struct.Filename[33]` — `EQEmu/common/item_data.h:501,503`.

So: a "note" is any item with `Book != 0` and a non-empty `Filename`. eqoxide's
item-blob parser (wherever it parses `SerializeItem` output for
`OP_CharInventory`/`OP_ItemPacket`/`OP_ItemLinkResponse`) must read `book` and
`booktype` as the last two bytes of the 74-byte `ItemSecondaryBodyStruct`, then
read the following NUL-terminated string as `Filename` — this is the exact
value to echo back in `txtfile` on `OP_ReadBook`.

## 6. saylink / item-link book path

Not separately verified server-side — there is only ONE server handler,
`Client::Handle_OP_ReadBook` → `Client::ReadBook`
(`EQEmu/zone/client_packet.cpp:13195`, `EQEmu/zone/client.cpp:2674`); grep across
`zone/*.cpp` shows no other code path that triggers a book-text reply from a
saylink click. **Inferred** (not directly provable from server source, since
saylink-click handling of "open item info / read book" is client-side UI
logic): clicking "read" inside an item's saylink info window still results in
the exact same `OP_ReadBook` request (the client already has the full item
blob — including `Book`/`Filename` — from the `OP_ItemLinkResponse` /
`OP_ItemPacket` that populated the link, so it has everything needed to build
the request without a separate protocol path). There is a distinct
**quest-triggered** path, `Client::QuestReadBook` (`client.cpp:2740-2753`,
exposed to quest scripts as `Perl_Client_ReadBook`/`quest::readbook` via
`ReadBookByName`, `client.cpp:11401`) which sends a reply directly (bypassing
`books` table lookup — text is passed in from the quest script) with
`window=0xFF`, `invslot=0`, no client-driven request at all. eqoxide does not
need to implement anything client-side for that path beyond generic
`OP_ReadBook`-reply handling — it's indistinguishable on the wire from a normal
reply except `invslot` will be `0` and there was no preceding request.

## Recommendation for eqoxide

**Request (send on right-click "read" of a book/note item):**
- Build an 8216-byte `OP_ReadBook` (`0x72df`) packet, all-zero except:
  - `window` (u32 LE) = `0xFFFFFFFF` for a fresh window (matches client behavior for right-click; only reuse a non-FF window id if re-invoking an already-open book window).
  - `invslot` (8 bytes: `Slot,SubIndex,AugIndex,Unknown01` as i16 LE) = the item's RoF2 typeless possessions slot (Type is implicit/omitted; do NOT use this opcode for bank/shared-bank items — server can't resolve them here).
  - `type` (u32 LE) = the item's `BookType`/kind if known, else `1` (Book) is a safe default; server may override it anyway from the item's `Book` field for general-inventory slots.
  - `target_id` (u32 LE) = current target id (0 if none).
  - `txtfile` = the item's parsed `Filename` string (NUL-terminated, ASCII), copied starting at byte 22. Keep it short — anything past 19 chars gets truncated by the reference server before the DB lookup, so match real item data (typically short, e.g. `CREWizardNote`).
- Leave `can_cast`/`can_scribe` zeroed on the request; the server ignores them on decode.

**Reply (parse):**
- Expect exactly 8216 bytes on `OP_ReadBook` inbound. Read `window`(u32@0), `invslot`(8B@4), `type`(u32@12), `target_id`(u32@16), `can_cast`(u8@20), `can_scribe`(u8@21), then `txtfile` as a NUL-terminated string starting at offset 22 (stop at the first `\0`; ignore the zero-padded tail).
- Replace backtick (`` ` ``) characters in the text with newlines before rendering.
- After the reply, also expect an immediate zero-length `OP_FinishWindow` (`0x7349`) — safe to no-op on it, or use it as a "book window is ready to display" signal.
- `window==0xFFFFFFFF` means "open a brand-new reading window"; any other value is an existing window id the client should already be tracking (RoF2 supports multiple simultaneously-open book/note windows, keyed by that id — not yet confirmed how the client allocates window ids for a fresh request; if unclear, always send `0xFF`/treat every reply as a new window until this is exercised further).

**Item parsing (prerequisite, feeds the above):**
- When parsing an item blob (`SerializeItem` output), after the `ItemBodyStruct` (255B) and the `CharmFile` NUL-terminated string, read the 74-byte `ItemSecondaryBodyStruct`; its last two bytes are `book`(u8) and `booktype`(u8). Immediately after that struct, read a NUL-terminated string = `Filename`. `book != 0` && non-empty `Filename` ⇒ item is readable; wire up "read" on right-click only when both hold.

**Verification status:** opcode value, both struct layouts, decode/encode
truncation behavior, and the item-blob Book/Filename position are all
**confirmed** by direct source reads (see citations above; struct sizes were
also cross-checked against the pre-existing `item-serialization.md` note,
which independently derived the same 74-byte `ItemSecondaryBodyStruct` size).
The saylink-vs-right-click equivalence (§6) is **inferred**, not directly
provable from server source — cheapest way to fully confirm would be a packet
capture of a real RoF2 client reading a book via each path, if this ever
becomes load-bearing.
