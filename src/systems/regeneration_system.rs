use bevy::prelude::*;

use crate::components::{Drone, MarkedForDeath};

/// Phase 2b regeneration system — recovers drone body hits naturally each tick.
///
/// Each tick, every drone without MarkedForDeath regains 1 hit point, capped at
/// hits_max. Runs after spawn_grace and before damage_application so
/// regeneration happens before combat damage is applied (prevents double-dip
/// with heal).
///
/// Filter: `Without<MarkedForDeath>` — drones marked for death do not regenerate.
pub fn regeneration_system(mut drones: Query<&mut Drone, Without<MarkedForDeath>>) {
    for mut drone in drones.iter_mut() {
        if drone.hits < drone.hits_max {
            drone.hits = drone.hits.saturating_add(1).min(drone.hits_max);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{Position, RoomId, DEFAULT_DRONE_LIFESPAN};
    use indexmap::IndexMap;

    fn spawn_drone(app: &mut App, owner: u32, hits: u32, hits_max: u32) -> Entity {
        app.world_mut()
            .spawn((
                Drone {
                    owner,
                    body: vec![],
                    carry: IndexMap::new(),
                    carry_capacity: 0,
                    fatigue: 0,
                    hits,
                    hits_max,
                    spawning: false,
                    age: 0,
                    last_action_tick: u64::MAX,
                    lifespan: DEFAULT_DRONE_LIFESPAN,
                },
                Position { x: 0, y: 0, room: RoomId(0) },
            ))
            .id()
    }

    #[test]
    fn regen_heals_one_per_tick() {
        let mut app = App::new();
        app.add_systems(Update, regeneration_system);
        let drone = spawn_drone(&mut app, 1, 50, 100);

        app.update();

        let d = app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(d.hits, 51, "should gain 1 hit per tick");
    }

    #[test]
    fn regen_capped_at_max_hits() {
        let mut app = App::new();
        app.add_systems(Update, regeneration_system);
        let drone = spawn_drone(&mut app, 1, 100, 100);

        app.update();

        let d = app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(d.hits, 100, "should not exceed max_hits");
    }

    #[test]
    fn regen_skips_death_marked_drones() {
        let mut app = App::new();
        app.add_systems(Update, regeneration_system);
        let drone = spawn_drone(&mut app, 1, 50, 100);
        app.world_mut().entity_mut(drone).insert(MarkedForDeath);

        app.update();

        let d = app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(d.hits, 50, "MarkedForDeath drones should not regenerate");
    }
}
