#!/usr/bin/env python3
"""Quest-giver discovery tool for the EQEmu server this client connects to.

Future agent-bot-players: use this to FIND quests. It cross-references the server's Lua quest
scripts (which NPCs actually have quests) with where those NPCs are spawned (DB), and summarizes
what each quest giver wants (turn-in item ids + counts) and rewards (XP/items), plus their hail
text. EQEmu quests are NOT in the DB — they're Lua in the server's quests/ dir — so you can't find
them with SQL alone; that's why this tool exists.

Usage:
    python3 tools/quest_finder.py <zone>            # all quest givers spawned in <zone>
    python3 tools/quest_finder.py <zone> --turnins  # only turn-in quests (have check_turn_in)
    python3 tools/quest_finder.py --npc <Name>      # detail (full script) for one NPC
    python3 tools/quest_finder.py --beginner        # curated beginner Qeynos turn-in quests

Zones of interest near the Qeynos newbie hub: qeynos (South Qeynos), qeynos2 (North Qeynos),
qcat (aqueduct/sewers), qrg (Surefall Glade — Ranger/Druid guild), qeytoqrg / qeynos hills.

Requires: `podman` access to the eqemu containers (quests dir + mariadb). Adjust QUESTS_DIR /
container names below if your setup differs.
"""
import re
import subprocess
import sys

MARIADB = ["podman", "exec", "eqemu_mariadb_1", "mariadb", "-uroot", "-prootpass", "peq", "-N", "-e"]
EQEMU = ["podman", "exec", "eqemu_eqemu_1", "sh", "-lc"]
QUESTS_DIR = "/opt/eqemu/data/quests"


def sql(query):
    out = subprocess.run(MARIADB + [query], capture_output=True, text=True).stdout
    return [line.split("\t") for line in out.strip().splitlines() if line and "ERROR" not in line]


def sh(cmd):
    return subprocess.run(EQEMU + [cmd], capture_output=True, text=True).stdout


def spawned_npcs(zone):
    """{name: (id, x, y, z, heading)} for every NPC actually spawned in `zone`."""
    rows = sql(
        "SELECT n.id, n.name, ROUND(sp.x), ROUND(sp.y), ROUND(sp.z), ROUND(sp.heading) "
        "FROM npc_types n JOIN spawnentry se ON se.npcID=n.id "
        "JOIN spawngroup sg ON sg.id=se.spawngroupID "
        "JOIN spawn2 sp ON sp.spawngroupID=sg.id "
        f"WHERE sp.zone='{zone}';"
    )
    out = {}
    by_id = {}
    for r in rows:
        if len(r) < 6:
            continue
        nid, name = r[0], r[1]
        rec = (nid, r[2], r[3], r[4], r[5])
        out[name] = rec
        by_id[nid] = (name, rec)
    return out, by_id


def quest_scripts(zone):
    """List quest script basenames for a zone (Name.lua / <npcid>.lua / #Name.lua)."""
    return [f.strip() for f in sh(f"ls {QUESTS_DIR}/{zone}/ 2>/dev/null").splitlines() if f.strip().endswith(".lua")]


def item_names(ids):
    if not ids:
        return {}
    idlist = ",".join(str(i) for i in ids)
    rows = sql(f"SELECT id, name FROM items WHERE id IN ({idlist});")
    return {int(r[0]): r[1] for r in rows if len(r) >= 2}  # int keys to match parsed item ids


def summarize(zone, fname):
    """Parse a quest .lua → (turn_in_sets, exp_rewards, hail_text, has_trade)."""
    txt = sh(f"cat {QUESTS_DIR}/{zone}/{fname} 2>/dev/null")
    # turn-in item sets, e.g. check_turn_in(e.trade, {item1 = 13915, item2 = 13915})
    sets = []
    for m in re.finditer(r"check_turn_in\([^,]+,\s*\{([^}]*)\}", txt):
        ids = [int(x) for x in re.findall(r"item\d+\s*=\s*(\d+)", m.group(1))]
        if ids:
            sets.append(ids)
    exp = [int(x) for x in re.findall(r"AddEXP\((\d+)\)", txt)]
    hail = ""
    sm = re.search(r'findi\("hail"\).*?Say\("([^"]{0,160})', txt, re.S)
    if sm:
        hail = sm.group(1)
    return sets, exp, hail, ("event_trade" in txt)


def npc_for_script(fname, spawned, by_id):
    base = fname[:-4]  # strip .lua
    if base.startswith("#"):
        base = base[1:]
    if base.isdigit():
        hit = by_id.get(base)
        return (hit[0], hit[1]) if hit else (None, None)
    rec = spawned.get(base)
    return (base.replace("_", " "), rec) if rec else (base.replace("_", " "), None)


def list_zone(zone, only_turnins=False):
    spawned, by_id = spawned_npcs(zone)
    scripts = quest_scripts(zone)
    if not scripts:
        print(f"No quest scripts found for zone '{zone}' (check the zone short_name / QUESTS_DIR).")
        return
    # gather all item ids first for one name lookup
    rows = []
    all_ids = set()
    for f in scripts:
        name, rec = npc_for_script(f, spawned, by_id)
        sets, exp, hail, has_trade = summarize(zone, f)
        if only_turnins and not sets:
            continue
        for s in sets:
            all_ids.update(s)
        rows.append((name, rec, sets, exp, hail, has_trade, f))
    names = item_names(all_ids)

    print(f"\n=== Quest givers in '{zone}' ({len(rows)} with scripts"
          f"{', turn-ins only' if only_turnins else ''}) ===")
    # spawned ones first, sorted by name
    rows.sort(key=lambda r: (r[1] is None, r[0] or ""))
    for name, rec, sets, exp, hail, has_trade, f in rows:
        loc = f"@ ({rec[1]},{rec[2]},{rec[3]}) hdg {rec[4]}" if rec else "(NOT spawned in this zone)"
        print(f"\n• {name or f}  [{f}]  {loc}")
        if hail:
            print(f"    hail: \"{hail.strip()}\"")
        if sets:
            best = max(sets, key=len)
            from collections import Counter
            c = Counter(best)
            want = ", ".join(f"{names.get(i, '?')} (item {i}) x{n}" for i, n in c.items())
            print(f"    turn in: {want}")
            if exp:
                print(f"    reward XP (per turn-in tier): {sorted(set(exp))}")
        elif has_trade:
            print("    has a turn-in (event_trade) — items not auto-parsed; use --npc for the script")
        else:
            print("    dialogue/hail quest (no turn-in) — hail + say keywords")


def detail(name):
    safe = name.replace(" ", "_")
    # find which zone(s) have a script for this npc
    for zone in sh(f"ls {QUESTS_DIR}").split():
        for cand in (f"{safe}.lua", f"#{safe}.lua"):
            txt = sh(f"cat {QUESTS_DIR}/{zone}/{cand} 2>/dev/null")
            if txt.strip():
                print(f"=== {zone}/{cand} ===\n{txt}")
                return
    print(f"No quest script found for '{name}'.")


BEGINNER = [
    ("qeynos",  "Captain_Tillin",     "Gnoll Fangs — kill gnolls (Blackburrow / gnoll pups in Qeynos Hills), turn in Gnoll Fang (13915) x1-4. Big XP."),
    ("qeynos2", "Priestess_Caulria",  "Rabid Pelts — kill rabid wolves/grizzlies in Qeynos Hills, turn in pelts. XP + Cure Disease."),
]


def beginner():
    print("=== Curated beginner Qeynos turn-in quests ===")
    for zone, npc, desc in BEGINNER:
        spawned, by_id = spawned_npcs(zone)
        rec = spawned.get(npc)
        loc = f"({rec[1]},{rec[2]},{rec[3]})" if rec else "NOT SPAWNED"
        print(f"\n• {npc} in {zone} @ {loc}\n    {desc}")
    print("\nRun `python3 tools/quest_finder.py qeynos --turnins` (and qeynos2) for the full list.")


def export(zones, path="../eqoxide_asset_server/content/quests.json"):
    """Write quests.json — the canonical quest-giver data. It is delivered to the client through the
    asset server's `gamedata` set (default path = the asset server's content/quests.json; a server
    can instead drop an override in the bake raw_dir). The client marks quest givers (golden '!') and
    serves GET /quests from it. Keyed by zone -> clean NPC name (spaces, matching clean_entity_name)."""
    import json
    import os
    from collections import Counter
    data = {}
    for zone in zones:
        spawned, by_id = spawned_npcs(zone)
        givers = {}
        for f in quest_scripts(zone):
            name, rec = npc_for_script(f, spawned, by_id)
            if not rec:  # only givers actually spawned in this zone
                continue
            sets, exp, hail, has_trade = summarize(zone, f)
            ids = sorted({i for s in sets for i in s})
            names = item_names(ids)
            best = max(sets, key=len) if sets else []
            c = Counter(best)
            clean = (name or f[:-4]).replace("_", " ")
            givers[clean] = {
                "npc_id": rec[0],
                "x": float(rec[1]), "y": float(rec[2]), "z": float(rec[3]),
                "wanted": [[i, names.get(i, "?"), c[i]] for i in c],
                "reward_xp": sorted(set(exp)),
                "hail": hail.strip()[:200],
                "turn_in": bool(sets) or has_trade,
            }
        data[zone] = givers
    os.makedirs(os.path.dirname(path), exist_ok=True)
    json.dump(data, open(path, "w"), indent=1)
    print(f"wrote {path}: " + ", ".join(f"{z}={len(g)} givers" for z, g in data.items()))


if __name__ == "__main__":
    args = sys.argv[1:]
    if not args:
        print(__doc__)
    elif args[0] == "--beginner":
        beginner()
    elif args[0] == "--export":
        export(args[1:] or ["qeynos", "qeynos2", "qcat", "qrg", "qeytoqrg"])
    elif args[0] == "--npc" and len(args) > 1:
        detail(args[1])
    else:
        list_zone(args[0], only_turnins=("--turnins" in args))
