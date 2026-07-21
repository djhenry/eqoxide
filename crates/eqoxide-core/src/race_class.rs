//! EQ id → name/code lookup tables shared across the render, net, and http layers.
//!
//! Pure, dependency-free code→name / code→archetype tables peeled out of `eq_net`
//! (#544 Step 2h) so the http layer (and its future crate) can resolve class/race names
//! without up-referencing the not-yet-extracted `eq_net` module. These are re-exported from
//! their original locations (`eq_net::packet_handler::class_name`,
//! `eq_net::protocol::{eq_race_to_code, is_boat_race}`) so existing call sites are unchanged.
//!
//! Value-identical to the pre-move definitions — the `/observe` JSON exposes these class/race
//! names, so any change here is an observable API-output change.

// ── Class ID → name (EQEmu common/classes.h) ───────────────────────────────

/// EQ class id (1..=16) → name. From EQEmu common/classes.h.
pub fn class_name(id: u32) -> &'static str {
    match id {
        1 => "Warrior", 2 => "Cleric", 3 => "Paladin", 4 => "Ranger",
        5 => "Shadow Knight", 6 => "Druid", 7 => "Monk", 8 => "Bard",
        9 => "Rogue", 10 => "Shaman", 11 => "Necromancer", 12 => "Wizard",
        13 => "Magician", 14 => "Enchanter", 15 => "Beastlord", 16 => "Berserker",
        _ => "",
    }
}

// ── Race ID → renderer code mapping (EQEmu common/races.h) ─────────────────

/// True for boat/ship spawn races (EQEmu common/races.h): Ship=72, Launch=73, GhostShip=114,
/// Boat=141, DiscordShip=404, Rowboat=502, Boat2=533, MerchantShip=550, PirateShip=551,
/// GhostShip2=552. These are `GravityBehavior::Floating` server-side — they ride the water surface
/// and must be exempt from the client's floor-snap so they don't sink (the server's `Mob::FixZ`
/// skips them too, zone/waypoints.cpp). #194.
pub fn is_boat_race(race_id: u32) -> bool {
    matches!(race_id, 72 | 73 | 114 | 141 | 404 | 502 | 533 | 550 | 551 | 552)
}

pub fn eq_race_to_code(race_id: u32) -> &'static str {
    // Boats/ships render as the "boat" archetype (a real ship model), not a HUM placeholder (#194).
    if is_boat_race(race_id) {
        return "SHP";
    }
    match race_id {
        // Playable races
        1 => "HUM", 2 => "BAR", 3 => "ERU", 4 => "ELF", 5 => "HIE", 6 => "DKE",
        7 => "HEF", 8 => "DWF", 9 => "TRL", 10 => "OGR", 11 => "HFL", 12 => "GNM",
        128 => "IKS", 130 => "VAH", 330 => "FRG", 522 => "DRK",
        // NPC races 13..=127 — best-fit to an available archetype model
        // (humanoid/elf/dwarf/gnoll/skeleton/zombie/creature/bear/wolf/rat/snake/
        // frog/bat/bird/wasp/worm/fish). Names from EQEmu common/races.h.
        13 => "BRD",  // Aviak
        14 => "WOL",  // Werewolf
        15 => "HUM",  // Brownie
        16 => "HUM",  // Centaur
        17 => "HUM",  // Golem
        18 => "HUM",  // Giant
        19 => "SNA",  // Trakanon (dragon)
        20 => "SKE",  // Venril Sathir (lich)
        21 => "SPI",  // Evil Eye
        22 => "SPI",  // Beetle
        23 => "HUM",  // Kerran (cat-folk)
        24 => "FIS",  // Fish
        25 => "HUM",  // Fairy
        26 => "FRG",  // Froglok
        27 => "FRG",  // Froglok Ghoul
        28 => "HUM",  // Fungusman
        29 => "HUM",  // Gargoyle
        30 => "SPI",  // Gasbag
        31 => "SPI",  // Gelatinous Cube
        32 => "HUM",  // Ghost
        33 => "ZOM",  // Ghoul
        34 => "BAT",  // Giant Bat
        35 => "SNA",  // Giant Eel
        36 => "RAT",  // Giant Rat
        37 => "SNA",  // Giant Snake
        38 => "SPI",  // Giant Spider
        39 => "GNL",  // Gnoll
        40 => "GNL",  // Goblin
        41 => "BEA",  // Gorilla
        42 => "WOL",  // Wolf
        43 => "BEA",  // Bear
        44 => "HUM",  // Freeport Guard
        45 => "SKE",  // Demi Lich
        46 => "HUM",  // Imp
        47 => "BRD",  // Griffin
        48 => "GNL",  // Kobold
        49 => "SNA",  // Lava Dragon
        50 => "WOL",  // Lion
        51 => "HUM",  // Lizard Man
        52 => "SPI",  // Mimic
        53 => "HUM",  // Minotaur
        54 => "GNL",  // Orc
        55 => "HUM",  // Human Beggar
        56 => "HUM",  // Pixie
        57 => "SPI",  // Drachnid
        58 => "HUM",  // Solusek Ro
        59 => "HUM",  // Bloodgill
        60 => "SKE",  // Skeleton
        61 => "FIS",  // Shark
        62 => "HUM",  // Tunare
        63 => "WOL",  // Tiger
        64 => "HUM",  // Treant
        65 => "HUM",  // Vampire
        66 => "HUM",  // Statue of Rallos Zek
        67 => "HUM",  // Highpass Citizen
        68 => "SNA",  // Tentacle Terror
        69 => "SPI",  // Wisp
        70 => "ZOM",  // Zombie
        71 => "HUM",  // Qeynos Citizen
        72 => "HUM",  // Ship
        73 => "HUM",  // Launch
        74 => "FIS",  // Piranha
        75 => "HUM",  // Elemental
        76 => "WOL",  // Puma
        77 => "ELF",  // Neriak Citizen (dark elf)
        78 => "HUM",  // Erudite Citizen
        79 => "WSP",  // Bixie
        80 => "SPI",  // Reanimated Hand
        81 => "HUM",  // Rivervale Citizen
        82 => "HUM",  // Scarecrow
        83 => "RAT",  // Skunk
        84 => "SNA",  // Snake Elemental
        85 => "SKE",  // Spectre
        86 => "BEA",  // Sphinx
        87 => "RAT",  // Armadillo
        88 => "HUM",  // Clockwork Gnome
        89 => "SNA",  // Drake
        90 => "HUM",  // Halas Citizen
        91 => "SNA",  // Alligator
        92 => "HUM",  // Grobb Citizen (troll)
        93 => "HUM",  // Oggok Citizen (ogre)
        94 => "DWF",  // Kaladim Citizen (dwarf)
        95 => "HUM",  // Cazic Thule
        96 => "BRD",  // Cockatrice
        97 => "HUM",  // Daisy Man
        98 => "ELF",  // Elf Vampire
        99 => "HUM",  // Denizen
        100 => "HUM", // Dervish
        101 => "HUM", // Efreeti
        102 => "FRG", // Froglok Tadpole
        103 => "HUM", // Phinigel Autropos
        104 => "WRM", // Leech
        105 => "FIS", // Swordfish
        106 => "HUM", // Felguard
        107 => "BEA", // Mammoth
        108 => "SPI", // Eye of Zomm
        109 => "WSP", // Wasp
        110 => "HUM", // Mermaid
        111 => "BRD", // Harpy
        112 => "ELF", // Fayguard (elf)
        113 => "WSP", // Drixie
        114 => "HUM", // Ghost Ship
        115 => "FIS", // Clam
        116 => "FIS", // Sea Horse
        117 => "DWF", // Dwarf Ghost
        118 => "HUM", // Erudite Ghost
        119 => "WOL", // Sabertooth
        120 => "WOL", // Wolf Elemental
        121 => "SNA", // Gorgon
        122 => "SKE", // Dragon Skeleton
        123 => "HUM", // Innoruuk
        124 => "WOL", // Unicorn
        125 => "BRD", // Pegasus
        126 => "HUM", // Djinn
        127 => "HUM", // Invisible Man
        // Post-Titanium "new model" NPC race IDs. PEQ uses these heavily even in
        // classic-era zones — e.g. restless/decaying skeletons in qeytoqrg are race
        // 367 (Skeleton2), not 60 — so without these they all render as human males.
        // Best-fit to an available archetype model, as above; names from races.h.
        131 => "IKS", // Sarnak (lizardkin)
        137 => "GNL", // Kunark Goblin
        141 => "HUM", // Boat
        145 => "SPI", // Goo
        161 => "SKE", // Undead Iksar
        188 => "HUM", // Frost Giant
        202 => "GNL", // Grimling
        215 => "HUM", // Tegi
        217 => "SNA", // Shissar
        224 => "HUM", // Shade
        240 => "HUM", // Teleport Man
        350 => "FRG", // Undead Froglok
        359 => "ZOM", // Undead Vampire
        360 => "HUM", // Vampire (Luclin)
        361 => "GNL", // Rujarkian Orc
        364 => "ELF", // Sand Elf
        367 => "SKE", // Skeleton (new model)
        368 => "ZOM", // Mummy
        369 => "GNL", // Goblin (new model)
        372 => "HUM", // Dervish (new model)
        373 => "HUM", // Shade (new model)
        374 => "HUM", // Golem (new model)
        394 => "SNA", // Ikaav (snake-woman)
        396 => "HUM", // Kyv
        397 => "HUM", // Noc
        402 => "HUM", // Mastruq
        413 => "HUM", // Dragorn
        415 => "RAT", // Rat (new model)
        432 => "SNA", // Drake (new model)
        433 => "GNL", // Goblin2
        439 => "WOL", // Puma (new model)
        440 => "SPI", // Spider (new model)
        442 => "HUM", // Animated Statue
        456 => "HUM", // Sporali
        457 => "HUM", // Gnomework
        458 => "GNL", // Orc (new model)
        461 => "SPI", // Drachnid (new model)
        467 => "HUM", // Shiliskin
        468 => "SNA", // Snake (new model)
        // Unknown — default to humanoid
        _ => "HUM",
    }
}
