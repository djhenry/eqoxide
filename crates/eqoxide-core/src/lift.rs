//! Motion of the Kelethin lift platforms, door opentype 59.
//!
//! The native client raises such a door linearly at 0.45 units per 20 ms tick from its spawn z to
//! spawn z plus `door_param`, and lowers it at the same rate. The server sends only open and
//! closed, so the platform height is derived here from the door state and elapsed time.

use std::collections::HashMap;
use std::time::Duration;

use crate::game_state::Door;

/// Door opentype of the lift platforms and of their lever doors.
pub const LIFT_OPENTYPE: u8 = 59;

/// Platform speed in units per second.
pub const LIFT_SPEED: f32 = 9.0;

/// Seconds that a lift door stays away from its rest state after it stops moving, before the
/// native client returns it there without a server packet.
pub const LIFT_REVERT_SECS: f32 = 20.0;

/// Returns how long after a state change a lift door with `door_param` returns to its rest
/// state, which is the travel time to the other end plus [`LIFT_REVERT_SECS`].
pub fn lift_revert_delay(door_param: u32) -> Duration {
    Duration::from_secs_f32(door_param as f32 / LIFT_SPEED + LIFT_REVERT_SECS)
}

/// Returns the rise in units above the spawn z of a lift that is `current` units up, after `dt`
/// seconds of travel toward the open height `door_param` (when `open`) or toward the spawn z.
pub fn lift_rise(current: f32, door_param: u32, open: bool, dt: f32) -> f32 {
    let top = door_param as f32;
    let target = if open { top } else { 0.0 };
    let step = LIFT_SPEED * dt.max(0.0);
    if current < target {
        (current + step).min(target)
    } else {
        (current - step).max(target)
    }
}

/// The rise of every lift door in the current zone, advanced once per frame.
#[derive(Debug, Default, Clone)]
pub struct LiftPoses {
    rise: HashMap<u8, f32>,
}

impl LiftPoses {
    /// Advances every lift door in `doors` by `dt` seconds toward its server state.
    ///
    /// A lift door that has no pose yet starts at its current state, open at `door_param` and
    /// closed at 0, so a lift that spawns open does not travel. Entries for doors that are no
    /// longer present are dropped. Doors of any other opentype are ignored.
    pub fn tick(&mut self, doors: &HashMap<u8, Door>, dt: f32) {
        self.rise.retain(|id, _| doors.get(id).is_some_and(|d| d.opentype == LIFT_OPENTYPE));
        for (&id, d) in doors.iter().filter(|(_, d)| d.opentype == LIFT_OPENTYPE) {
            let seed = if d.is_open { d.door_param as f32 } else { 0.0 };
            let cur = self.rise.entry(id).or_insert(seed);
            *cur = lift_rise(*cur, d.door_param, d.is_open, dt);
        }
    }

    /// Returns the rise in units of lift door `door_id`, or `None` if that door has no pose.
    pub fn rise(&self, door_id: u8) -> Option<f32> {
        self.rise.get(&door_id).copied()
    }

    /// Drops every pose, for a zone change.
    pub fn clear(&mut self) {
        self.rise.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn door(id: u8, opentype: u8, door_param: u32, is_open: bool) -> Door {
        Door {
            door_id: id, name: "FAYLEVATOR".into(), x: 0.0, y: 0.0, z: 0.0, heading: 0.0,
            incline: 0, size: 100, opentype, door_param, invert_state: false, is_open,
        }
    }

    fn doors(list: Vec<Door>) -> HashMap<u8, Door> {
        list.into_iter().map(|d| (d.door_id, d)).collect()
    }

    #[test]
    fn shortest_shaft_takes_7_6_seconds() {
        let mut rise = 0.0;
        let mut frames = 0;
        while rise < 68.0 {
            rise = lift_rise(rise, 68, true, 1.0 / 60.0);
            frames += 1;
        }
        let secs = frames as f32 / 60.0;
        assert!((secs - 7.6).abs() < 0.05, "68 units at 9 u/s took {secs}s");
    }

    #[test]
    fn revert_delay_is_the_travel_time_plus_twenty_seconds() {
        let secs = lift_revert_delay(90).as_secs_f32();
        assert!((secs - 30.0).abs() < 1e-3, "90 units at 9 u/s plus 20 s, got {secs}");
    }

    #[test]
    fn rise_clamps_at_door_param() {
        assert_eq!(lift_rise(67.0, 68, true, 1.0), 68.0);
    }

    #[test]
    fn closing_falls_at_the_same_rate() {
        assert_eq!(lift_rise(30.0, 68, false, 1.0), 21.0);
        assert_eq!(lift_rise(3.0, 68, false, 1.0), 0.0);
    }

    #[test]
    fn new_door_seeds_at_its_state() {
        let mut p = LiftPoses::default();
        p.tick(&doors(vec![door(1, 59, 68, true), door(2, 59, 98, false)]), 0.0);
        assert_eq!(p.rise(1), Some(68.0));
        assert_eq!(p.rise(2), Some(0.0));
    }

    #[test]
    fn non_lift_door_has_no_pose() {
        let mut p = LiftPoses::default();
        p.tick(&doors(vec![door(3, 58, 68, true)]), 1.0);
        assert_eq!(p.rise(3), None);
    }

    #[test]
    fn tick_follows_a_state_change_and_drops_removed_doors() {
        let mut p = LiftPoses::default();
        let mut d = doors(vec![door(1, 59, 68, false)]);
        p.tick(&d, 0.0);
        d.get_mut(&1).unwrap().is_open = true;
        p.tick(&d, 1.0);
        assert_eq!(p.rise(1), Some(9.0));
        p.tick(&HashMap::new(), 1.0);
        assert_eq!(p.rise(1), None);
    }
}
