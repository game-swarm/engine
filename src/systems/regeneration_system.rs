use bevy::prelude::*;

use crate::components::{DeathMark, Drone};
use crate::systems::PendingHeal;

/// Phase 2b regeneration system — recovers drone body hits naturally each tick.
///
/// Each tick, every drone without DeathMark regains 1 hit point, capped at
/// hits_max. Runs after spawn_grace and before damage_application so
/// regeneration happens before combat damage is applied (prevents double-dip
/// with heal).
///
/// Filter: `Without<DeathMark>` — drones marked for death do not regenerate.
pub fn regeneration_system(
    mut pending_heal: ResMut<PendingHeal>,
    drones: Query<(Entity, &Drone), Without<DeathMark>>,
) {
    for (entity, drone) in drones.iter() {
        if drone.hits < drone.hits_max {
            pending_heal.push(entity, 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{DEFAULT_DRONE_LIFESPAN, Position, RoomId};
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
                Position {
                    x: 0,
                    y: 0,
                    room: RoomId(0),
                },
            ))
            .id()
    }

    #[test]
    fn regen_heals_one_per_tick() {
        let mut app = App::new();
        app.insert_resource(PendingHeal::default());
        app.add_systems(Update, regeneration_system);
        let drone = spawn_drone(&mut app, 1, 50, 100);

        app.update();

        let pending = app.world().resource::<PendingHeal>();
        assert_eq!(pending.entries.len(), 1);
        assert_eq!(pending.entries[0].target, drone);
        assert_eq!(pending.entries[0].amount, 1);
    }

    #[test]
    fn regen_capped_at_max_hits() {
        let mut app = App::new();
        app.insert_resource(PendingHeal::default());
        app.add_systems(Update, regeneration_system);
        let drone = spawn_drone(&mut app, 1, 100, 100);

        app.update();

        let d = app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(d.hits, 100, "should not exceed max_hits");
        assert!(app.world().resource::<PendingHeal>().entries.is_empty());
    }

    #[test]
    fn regen_skips_death_marked_drones() {
        let mut app = App::new();
        app.insert_resource(PendingHeal::default());
        app.add_systems(Update, regeneration_system);
        let drone = spawn_drone(&mut app, 1, 50, 100);
        app.world_mut().entity_mut(drone).insert(DeathMark);

        app.update();

        let d = app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(d.hits, 50, "DeathMark drones should not regenerate");
        assert!(app.world().resource::<PendingHeal>().entries.is_empty());
    }
}
