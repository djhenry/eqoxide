//! `SceneState` — the render thread's per-frame snapshot of what to draw (entity billboards, player
//! pose/position, recent messages, target info, …). Copied from the network-owned `GameState` once
//! per frame so the render loop never blocks on or shares locks with the EQ network thread.

use crate::game_state::GameState;

/// How long a one-shot combat swing (OP_Animation) plays before reverting to idle/walk. ~one swing.
pub const COMBAT_SWING_WINDOW: std::time::Duration = std::time::Duration::from_millis(600);

/// Billboard for one entity in the scene.
#[derive(Debug, Clone)]
pub struct Billboard {
    pub id:        u32,
    pub pos:       [f32; 3],
    pub level:     u32,
    pub hp_pct:    f32,
    pub is_target: bool,
    pub dead:      bool,
    pub name:      String,
    pub race:      String,
    pub action:    String,
    pub heading:   f32,
    pub equipment: [u32; 9],
    pub equipment_tint: [[u8; 3]; 9],
    pub gender:    u8,
    /// Face variant (0-indexed from Spawn_Struct `face`).
    pub face:      u8,
    /// Hair style from Spawn_Struct `hairstyle`. 0 = bald.
    pub hairstyle: u8,
    /// Hair color index — runtime-tints synthetic hair shells only (eqoxide#98).
    pub haircolor: u8,
    /// Helm material + show-helm flag, for hiding hair shells under a worn helm.
    pub helm:      u32,
    pub showhelm:  u8,
    /// Boat/ship: floats on the water surface, exempt from the render floor-snap (#194).
    pub floating:  bool,
}

/// A door to render this frame. Positions are in client convention [east=x, north=y, up=z].
/// `heading` is EQ 0..512; `open_frac` is 0=closed..1=open, eased render-side by `App` (see
/// `ease_door_frac` in app.rs) since `GameState::Door` only carries the authoritative `is_open`.
#[derive(Debug, Clone)]
pub struct DoorRender {
    pub door_id:   u8,
    pub name:      String,
    pub pos:       [f32; 3],
    pub heading:   f32,
    pub incline:   i32,
    pub size:      u16,
    pub opentype:  u8,
    pub open_frac: f32,
}

/// A single entry in the message log.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub kind: String,
    pub text: String,
    pub timestamp: std::time::Instant,
}

/// All data the renderer needs for one frame.
#[derive(Debug, Default, Clone)]
pub struct SceneState {
    pub zone: String,
    pub zone_changed: bool,
    pub player_pos: [f32; 3],
    pub player_heading: f32,
    pub player_hp_pct: f32,
    pub player_mana_pct: f32,
    pub player_xp_pct: f32,
    pub player_name: String,
    pub player_level: u32,
    pub player_race: String,
    pub player_class: String,
    pub player_gender: u8,
    /// Player face variant (0-indexed from PlayerProfile, offset 00898).
    pub player_face: u8,
    /// Player hair style (from PlayerProfile, offset 00896). 0 = bald.
    pub player_hairstyle: u8,
    /// Player hair color — runtime-tints the player's synthetic hair shells only (eqoxide#98).
    pub player_haircolor: u8,
    pub coin: [u32; 4],
    pub stats: [u32; 7],
    pub player_action: String,
    pub target_name: Option<String>,
    pub target_hp_pct: Option<f32>,
    pub target_con: Option<[u8; 3]>,
    pub strategy: String,
    /// True when no server packet has arrived for a while — drives the HUD "connection lost"
    /// banner so a frozen/dead server is visible to a human player, not silently idle (#8).
    pub disconnected: bool,
    pub billboards: Vec<Billboard>,
    /// Doors to render this frame, copied from `GameState::doors`.
    pub doors: Vec<DoorRender>,
    pub messages: Vec<LogEntry>,
    /// Clickable NPC-dialogue choices (saylinks) from the most recent NPC message, for the HUD's
    /// clickable dialogue (#120).
    pub dialogue_choices: Vec<crate::game_state::DialogueChoice>,
    /// Active native Task-system tasks (from OP_TaskDescription/OP_TaskActivity), sorted by the
    /// server's journal display order, for the Task Window (#144).
    pub tasks: Vec<crate::game_state::ActiveTask>,
    /// Completed-task history (from OP_CompletedTasks), for the Task Window's history tab (#144).
    pub completed_tasks: Vec<crate::game_state::CompletedTaskEntry>,
    /// Item material IDs for each equipment slot (0..9), from the player profile.
    pub player_equipment: [u32; 9],
    /// RGB tint for each equipment slot (0..9), from the player profile.
    pub player_equipment_tint: [[u8; 3]; 9],
    /// Player inventory + equipment items (for the inventory UI window).
    pub inventory: Vec<crate::game_state::InvItem>,
    /// Equipped weapon held-model ids (IDFile, e.g. "IT10649"), for rendering weapons in hand.
    /// Empty = nothing equipped in that slot. Primary = worn slot 13, secondary = slot 14.
    pub primary_weapon_idfile: String,
    pub secondary_weapon_idfile: String,
    /// Memorized spell gem IDs (9 slots); 0xFFFF_FFFF = empty slot.
    pub mem_spells: [u32; 9],
    /// Active cast in progress (Some) or idle (None).
    pub casting: Option<crate::game_state::CastState>,
    /// True when the player is sitting.
    pub sitting: bool,
    /// True when auto-attack is enabled.
    pub auto_attack: bool,
    /// The spawn_id of the current target, if any.
    pub target_id: Option<u32>,
    /// `Some(merchant_entity_id)` while a merchant window is open; drives the HUD merchant window.
    pub merchant_open: Option<u32>,
    /// Items the open merchant offers (for the merchant window's buy list).
    pub merchant_items: Vec<crate::game_state::MerchantItem>,
    /// Current group roster (empty = not grouped), for the always-on roster panel.
    pub group_members: Vec<crate::game_state::GroupMember>,
    /// Current group leader's name ("" if unknown/not grouped).
    pub group_leader: String,
    // ── UI-overhaul additions (#162) ──
    /// Absolute HP/mana values for the player window ("123 / 456" readouts).
    pub cur_hp: i32,
    pub max_hp: i32,
    pub cur_mana: i32,
    pub max_mana: i32,
    /// Player skill values indexed by skill id (see `crate::skills`), from the profile.
    pub player_skills: Vec<u32>,
    /// `Some(trainer_entity_id)` while a GM-trainer session is open.
    pub trainer_open: Option<u32>,
    /// Per-skill caps the open trainer offers (same indexing as `player_skills`).
    pub trainer_skills: Vec<u32>,
    /// The player's pet spawn id, if one is up.
    pub pet_id: Option<u32>,
    /// Pending group invite from this player name (accept/decline dialog).
    pub pending_invite: Option<String>,
    /// Tasks offered by an open task-select window.
    pub task_offers: Vec<crate::game_state::TaskOffer>,
    /// True while the auto-loot session is working a corpse (loot window).
    pub loot_active: bool,
    pub player_dead: bool,
    /// Who last killed the player — shown on the HUD death overlay (#284).
    pub killed_by: String,
    pub zone_id: u16,
}

impl SceneState {
    /// Populate billboards with one entry per loaded archetype for the test zone.
    /// Each model is placed side-by-side along the east axis so every archetype
    /// can be visually inspected.
    pub fn inject_test_billboards(&mut self) {
        use crate::models::race_to_archetype;
        use std::collections::HashSet;

        // EQ race codes that map to distinct archetypes. Each entry is
        // (race_code, archetype_key, name_label).
        // Archetypes without converted GLB models are skipped at render time.
        let archetypes: Vec<(&str, &str, &str)> = vec![
            ("HUM", "humanoid",  "Humanoid"),
            ("ELF", "elf",       "Elf"),
            ("DWF", "dwarf",     "Dwarf"),
            ("GNL", "gnoll",     "Gnoll"),
            ("FRG", "frog",      "Frog"),
            ("SKE", "skeleton",  "Skeleton"),
            ("ZOM", "zombie",    "Zombie"),
            ("BEA", "bear",      "Bear"),
            ("WOL", "wolf",      "Wolf"),
            ("RAT", "rat",       "Rat"),
            ("SNA", "snake",     "Snake"),
            ("BAT", "bat",       "Bat"),
            ("BRD", "bird",      "Bird"),
            ("WSP", "wasp",      "Wasp"),
            ("WRM", "worm",      "Worm"),
            ("FIS", "fish",      "Fish"),
        ];

        // Deduplicate archetypes (e.g. WOL/LIO/CAT all map to "wolf").
        let mut seen = HashSet::new();
        let mut unique: Vec<(&str, &str, &str)> = Vec::new();
        for entry in &archetypes {
            let arch = race_to_archetype(entry.0);
            if seen.insert(arch) {
                unique.push(*entry);
            }
        }

        // Debug hook: `EQ_TESTZONE_ONLY=<archetype>` shows just that model, centered — handy for
        // eyeballing a single archetype's orientation/scale (e.g. the fish, #149).
        if let Ok(only) = std::env::var("EQ_TESTZONE_ONLY") {
            unique.retain(|(_, arch, _)| *arch == only);
        }

        let spacing = 20.0_f32; // east spacing between models
        let start_east = -((unique.len() as f32) * spacing * 0.5); // center around origin

        for (i, &(race, _arch, label)) in unique.iter().enumerate() {
            let east = start_east + i as f32 * spacing;
            self.billboards.push(crate::scene::Billboard {
                id:        1000 + i as u32,
                pos:       [east, 0.0, 0.0], // [east, north, height]
                level:     50,
                hp_pct:    100.0,
                is_target: false,
                dead:      false,
                name:      format!("Test_{}", label),
                race:      race.to_string(),
                action:    "idle".to_string(),
                heading:   0.0,
                equipment:      [0; 9],
                equipment_tint: [[0; 3]; 9],
                gender:    0,
                face:      0,
                hairstyle: 0,
                haircolor: 0,
                helm:      0,
                showhelm:  0,
                floating:  false,
            });
        }

        tracing::info!("testzone: injected {} billboards for character model inspection",
                  self.billboards.len());
    }

    /// Build SceneState from a live GameState snapshot.
    pub fn from_game_state(gs: &GameState, door_frac: &std::collections::HashMap<u8, f32>) -> Self {
        let billboards = gs.entities.values().map(|e| {
            // Map EQ Animation:: values to action strings for clip resolution.
            // Animation constants from eq_constants.h: Standing=100, Freeze=102,
            // Looting=105, Sitting=110, Crouching=111, Lying=115.
            // Dead entities always use the "dead" clip — no combat swing can override.
            // (apply_death sets e.animation=115, but guard here in case the animation
            // field is stale from a race or a future code path that forgets to update it.)
            let action: String = if e.dead {
                "dead".to_string()
            } else {
                // A transient combat swing (OP_Animation) overrides the looping animation for a
                // short window: action "C0{code}" resolves to the matching combat clip (C05 = 1H
                // weapon, …).
                match gs.combat_anims.get(&e.spawn_id) {
                    Some((code, start)) if start.elapsed() < COMBAT_SWING_WINDOW => format!("C{:02}", code),
                    _ => match e.animation {
                        100 => "idle",       // Animation::Standing
                        102 => "idle",       // Animation::Freeze
                        110 => "sitting",    // Animation::Sitting
                        111 => "crouching",  // Animation::Crouching
                        105 => "idle",       // Animation::Looting (treat as idle)
                        115 => "dead",       // Animation::Lying
                        _   => "idle",       // default / standing / safe default
                    }.to_string(),
                }
            };
            Billboard {
                id:        e.spawn_id,
                pos:       [e.x, e.y, e.z],
                level:     e.level,
                hp_pct:    e.hp_pct,
                is_target: gs.target_id == Some(e.spawn_id),
                dead:      e.dead,
                name:      e.name.clone(),
                race:      e.race.clone(),
                action:    action,
                heading:   e.heading,
                equipment:      e.equipment,
                equipment_tint: e.equipment_tint,
                gender:    e.gender,
                face:      e.face,
                hairstyle: e.hairstyle,
                haircolor: e.haircolor,
                helm:      e.helm as u32,
                showhelm:  e.showhelm,
                floating:  e.floating,
            }
        }).collect();

        let doors = gs.doors.values().map(|d| DoorRender {
            door_id: d.door_id,
            name:    d.name.clone(),
            // Client convention [east=x, north=y, up=z] — same as entities/player.
            pos:     [d.x, d.y, d.z],
            heading: d.heading,
            incline: d.incline,
            size:    d.size,
            opentype: d.opentype,
            open_frac: door_frac.get(&d.door_id).copied().unwrap_or(0.0),
        }).collect();

        let messages = gs.messages.iter().map(|m| LogEntry {
            kind: m.kind.clone(),
            text: m.text.clone(),
            timestamp: m.timestamp,
        }).collect();

        SceneState {
            zone: gs.zone_name.clone(),
            zone_changed: gs.zone_changed,
            // World space is EQ native: [east=server_x, north=server_y, up=server_z].
            // Zone geometry, entities and the player all share this one frame.
            player_pos: [gs.player_x, gs.player_y, gs.player_z],
            player_heading: gs.player_heading,
            player_hp_pct: gs.hp_pct,
            player_mana_pct: gs.mana_pct,
            player_xp_pct: gs.xp_pct,
            player_name: gs.player_name.clone(),
            player_level: gs.player_level,
            player_race: gs.player_race.clone(),
            player_class: gs.player_class.clone(),
            player_gender: gs.player_gender,
            player_face: gs.player_face,
            player_hairstyle: gs.player_hairstyle,
            player_haircolor: gs.player_haircolor,
            coin: gs.coin,
            stats: gs.stats,
            player_action: gs.player_action.clone(),
            target_name: gs.target_name.clone(),
            target_hp_pct: gs.target_hp_pct,
            target_con: gs.target_con,
            strategy: gs.strategy.clone(),
            disconnected: false, // set per-frame in app.rs from last_inbound (#8)
            billboards,
            doors,
            messages,
            dialogue_choices: gs.dialogue_choices.clone(),
            tasks: {
                let mut t: Vec<_> = gs.tasks.values()
                    .filter(|t| t.status == crate::game_state::TaskStatus::Active)
                    .cloned().collect();
                t.sort_by_key(|t| t.sequence_number);
                t
            },
            completed_tasks: gs.completed_task_history.clone(),
            player_equipment: gs.player_equipment,
            player_equipment_tint: gs.player_equipment_tint,
            inventory: gs.inventory.clone(),
            primary_weapon_idfile: gs.inventory.iter().find(|i| i.slot == 13)
                .map(|i| i.idfile.clone()).unwrap_or_default(),
            secondary_weapon_idfile: gs.inventory.iter().find(|i| i.slot == 14)
                .map(|i| i.idfile.clone()).unwrap_or_default(),
            mem_spells: gs.mem_spells,
            // Drop a stale cast bar: if the cast time has elapsed plus a grace window and the
            // server never sent a terminal packet (OP_MemorizeSpell scribing=3 / OP_InterruptCast),
            // stop showing "Casting …" forever. (Spec Risks: cast_ms + grace fallback.)
            casting: gs.casting.clone().filter(|c| {
                c.started.elapsed().as_millis() < c.cast_ms as u128 + 1500
            }),
            sitting: gs.sitting,
            auto_attack: gs.auto_attack,
            target_id: gs.target_id,
            merchant_open: gs.merchant_open,
            merchant_items: gs.merchant_items.clone(),
            // Override the OP_GroupUpdateB placeholder level (70/65) with the real level resolved
            // from the profile / entity list, so the HUD roster shows true levels. (eqoxide#104)
            group_members: gs.group_members.iter().map(|m| crate::game_state::GroupMember {
                level: gs.group_member_level(&m.name), ..m.clone()
            }).collect(),
            group_leader: gs.group_leader.clone(),
            cur_hp: gs.cur_hp,
            max_hp: gs.max_hp,
            cur_mana: gs.cur_mana,
            max_mana: gs.max_mana,
            player_skills: gs.player_skills.clone(),
            trainer_open: gs.trainer_open,
            trainer_skills: gs.trainer_skills.clone(),
            pet_id: gs.pet_id,
            pending_invite: gs.pending_invite.clone(),
            task_offers: gs.task_offers.clone(),
            // Gate on the mirrored loot SESSION only (OP_UI_LOOT_STATE from the gameplay loop).
            // The render-side pending_loot fills from corpse packets but is drained only by the
            // gameplay loop's copy, so including it here held the Loot window open forever (#4).
            loot_active: gs.loot_session_active,
            player_dead: gs.player_dead,
            killed_by: gs.killed_by.clone(),
            zone_id: gs.zone_id,
        }
    }
}

impl Default for LogEntry {
    fn default() -> Self {
        LogEntry {
            kind: String::new(),
            text: String::new(),
            timestamp: std::time::Instant::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SceneState;
    use crate::game_state::{Entity, GameState};

    fn sample_state() -> GameState {
        let mut gs = GameState::new();
        gs.zone_name = "qeynoshills".into();
        gs.zone_changed = false;
        gs.player_name = "Aethas".into();
        gs.player_level = 5;
        gs.hp_pct = 87.3;
        gs.mana_pct = 75.0;
        gs.xp_pct = 12.5;
        gs.player_x = 1.0;
        gs.player_y = 2.0;
        gs.player_z = 3.0;
        gs.player_heading = 192.0;
        gs.player_race = "HUM".into();
        gs.player_action = "walking".into();
        gs.target_id = Some(42);
        gs.target_name = Some("a gnoll".into());
        gs.target_hp_pct = Some(61.0);
        gs.strategy = "Attacking".into();

        gs.upsert_entity(Entity {
            spawn_id: 42,
            name: "a gnoll".into(),
            level: 4,
            is_npc: true,
            x: 10.0, y: 20.0, z: 3.0,
            hp_pct: 61.0,
            cur_hp: 61,
            max_hp: 100,
            race: "GNL".into(),
            heading: 0.0,
            dead: false,
            equipment: [0; 9], equipment_tint: [[0; 3]; 9], gender: 0, helm: 0, showhelm: 0, floating: false,
            face: 0, hairstyle: 0, haircolor: 0,
            animation: 0,
        });

        gs
    }

    #[test]
    fn from_game_state_sets_player_fields() {
        let scene = SceneState::from_game_state(&sample_state(), &std::collections::HashMap::new());
        assert_eq!(scene.player_name, "Aethas");
        assert_eq!(scene.player_pos, [1.0, 2.0, 3.0]); // EQ native [server_x, server_y, server_z]
        assert_eq!(scene.player_heading, 192.0);
    }

    #[test]
    fn from_game_state_marks_target_billboard() {
        let scene = SceneState::from_game_state(&sample_state(), &std::collections::HashMap::new());
        assert_eq!(scene.billboards.len(), 1);
        assert!(scene.billboards[0].is_target);
    }

    #[test]
    fn from_game_state_no_target_no_is_target() {
        let mut gs = sample_state();
        gs.target_id = None;
        let scene = SceneState::from_game_state(&gs, &std::collections::HashMap::new());
        assert!(!scene.billboards[0].is_target);
    }

    #[test]
    fn from_game_state_billboard_race_propagated() {
        let gs = sample_state();
        let scene = SceneState::from_game_state(&gs, &std::collections::HashMap::new());
        assert_eq!(scene.billboards[0].race, "GNL");
    }

    #[test]
    fn from_game_state_billboard_id_propagated() {
        let scene = SceneState::from_game_state(&sample_state(), &std::collections::HashMap::new());
        assert_eq!(scene.billboards[0].id, 42);
    }

    #[test]
    fn from_game_state_zone_name() {
        let scene = SceneState::from_game_state(&sample_state(), &std::collections::HashMap::new());
        assert_eq!(scene.zone, "qeynoshills");
    }

    // --- Coordinate mapping: player_pos ---

    #[test]
    fn player_pos_coordinate_mapping() {
        let mut gs = GameState::new();
        gs.player_x = 100.0;
        gs.player_y = 200.0;
        gs.player_z = 50.0;
        let scene = SceneState::from_game_state(&gs, &std::collections::HashMap::new());
        assert_eq!(
            scene.player_pos,
            [100.0, 200.0, 50.0],
            "player_pos is EQ native [server_x=east, server_y=north, server_z=up]"
        );
    }

    // --- Coordinate mapping: entity billboard pos ---

    #[test]
    fn billboard_pos_coordinate_mapping() {
        let mut gs = GameState::new();
        gs.upsert_entity(Entity {
            spawn_id: 1,
            name: "test".into(),
            level: 1,
            is_npc: true,
            x: 10.0,
            y: 20.0,
            z: 5.0,
            hp_pct: 100.0,
            cur_hp: 100,
            max_hp: 100,
            race: String::new(),
            heading: 0.0,
            dead: false,
            equipment: [0; 9], equipment_tint: [[0; 3]; 9], gender: 0, helm: 0, showhelm: 0, floating: false,
            face: 0, hairstyle: 0, haircolor: 0,
            animation: 0,
        });
        let scene = SceneState::from_game_state(&gs, &std::collections::HashMap::new());
        assert_eq!(scene.billboards.len(), 1);
        let b = &scene.billboards[0];
        assert_eq!(b.pos[0], 10.0, "pos[0] should be server_x (east)");
        assert_eq!(b.pos[1], 20.0, "pos[1] should be server_y (north)");
        assert_eq!(b.pos[2], 5.0,  "pos[2] should be server_z (height)");
    }

    // --- is_target flag ---

    #[test]
    fn target_entity_has_is_target_true() {
        let gs = sample_state(); // target_id = Some(42)
        let scene = SceneState::from_game_state(&gs, &std::collections::HashMap::new());
        let targeted: Vec<_> = scene.billboards.iter().filter(|b| b.is_target).collect();
        assert_eq!(targeted.len(), 1);
        assert_eq!(targeted[0].id, 42);
    }

    #[test]
    fn non_target_entities_have_is_target_false() {
        let mut gs = sample_state();
        // Add a second entity that is NOT the target
        gs.upsert_entity(Entity {
            spawn_id: 99,
            name: "bystander".into(),
            level: 2,
            is_npc: true,
            x: 5.0, y: 5.0, z: 0.0,
            hp_pct: 100.0,
            cur_hp: 100,
            max_hp: 100,
            race: String::new(),
            heading: 0.0,
            dead: false,
            equipment: [0; 9], equipment_tint: [[0; 3]; 9], gender: 0, helm: 0, showhelm: 0, floating: false,
            face: 0, hairstyle: 0, haircolor: 0,
            animation: 0,
        });
        gs.target_id = Some(42);
        let scene = SceneState::from_game_state(&gs, &std::collections::HashMap::new());
        for b in &scene.billboards {
            if b.id == 42 {
                assert!(b.is_target, "id=42 should be targeted");
            } else {
                assert!(!b.is_target, "id={} should not be targeted", b.id);
            }
        }
    }

    #[test]
    fn from_game_state_propagates_equipment() {
        let mut gs = GameState::new();
        let mut e = Entity {
            spawn_id: 5, name: "x".into(), level: 1, is_npc: true,
            x: 0.0, y: 0.0, z: 0.0, hp_pct: 100.0, cur_hp: 1, max_hp: 1,
            race: "HUM".into(), heading: 0.0, dead: false,
            equipment: [0; 9], equipment_tint: [[0; 3]; 9], gender: 0, helm: 0, showhelm: 0, floating: false,
            face: 0, hairstyle: 0, haircolor: 0,
            animation: 0,
        };
        e.equipment[1] = 17;
        e.equipment_tint[1] = [9, 8, 7];
        gs.upsert_entity(e);
        let scene = SceneState::from_game_state(&gs, &std::collections::HashMap::new());
        assert_eq!(scene.billboards[0].equipment[1], 17);
        assert_eq!(scene.billboards[0].equipment_tint[1], [9, 8, 7]);
    }

    #[test]
    fn from_game_state_propagates_gender() {
        let mut gs = GameState::new();
        gs.player_gender = 1; // female player
        let e = Entity {
            spawn_id: 5, name: "x".into(), level: 1, is_npc: true,
            x: 0.0, y: 0.0, z: 0.0, hp_pct: 100.0, cur_hp: 1, max_hp: 1,
            race: "HUM".into(), heading: 0.0, dead: false,
            equipment: [0; 9], equipment_tint: [[0; 3]; 9], gender: 1, helm: 0, showhelm: 0, floating: false,
            face: 0, hairstyle: 0, haircolor: 0,
            animation: 0,
        };
        gs.upsert_entity(e);
        let scene = SceneState::from_game_state(&gs, &std::collections::HashMap::new());
        assert_eq!(scene.billboards[0].gender, 1, "entity gender propagates to billboard");
        assert_eq!(scene.player_gender, 1, "player gender propagates to scene");
    }

    // --- Message count ---

    #[test]
    fn message_count_matches() {
        let mut gs = GameState::new();
        gs.log_msg("say", "hello");
        gs.log_msg("tell", "world");
        gs.log_msg("ooc", "third");
        let scene = SceneState::from_game_state(&gs, &std::collections::HashMap::new());
        assert_eq!(scene.messages.len(), 3);
        assert_eq!(scene.messages[0].text, "hello");
        assert_eq!(scene.messages[2].text, "third");
    }

    #[test]
    fn from_game_state_copies_group_roster() {
        use crate::game_state::GroupMember;
        let mut gs = sample_state();
        gs.player_name = "Aldric".into();
        gs.group_leader = "Aldric".into();
        gs.group_members = vec![
            GroupMember { name: "Aldric".into(), is_leader: true, level: 10, ..Default::default() },
            GroupMember { name: "Sariel".into(), level: 8, ..Default::default() },
        ];
        let scene = SceneState::from_game_state(&gs, &std::collections::HashMap::new());
        assert_eq!(scene.group_leader, "Aldric");
        assert_eq!(scene.group_members.len(), 2);
        assert_eq!(scene.group_members[1].name, "Sariel");
    }
}
