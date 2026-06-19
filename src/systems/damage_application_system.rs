use bevy::prelude::*;

use crate::components::{Drone, MarkedForDeath, SpawningGrace, Structure};
use crate::systems::special_attack_reducer::PendingDamage;

/// S15: Damage Application System
///
/// Applies pending damage from special attacks to entities.
/// Accounts for SpawningGrace (newborn invincibility) and marks
/// entities for death when hits reach zero.
///
/// Filter: `Without<SpawningGrace>` per manifest §S15.
pub fn damage_application_system(
    mut damage: ResMut<PendingDamage>,
    mut drones: Query<&mut Drone, Without<SpawningGrace>>,
    mut structures: Query<&mut Structure, Without<SpawningGrace>>,
    mut commands: Commands,
) {
    if damage.entries.is_empty() {
        return;
    }

    let entries = std::mem::take(&mut damage.entries);

    for (target, amount, _damage_type) in entries {
        // Try to apply damage to drone
        if let Ok(mut drone) = drones.get_mut(target) {
            drone.hits = drone.hits.saturating_sub(amount);
            if drone.hits == 0 {
                commands.entity(target).insert(MarkedForDeath);
            }
            continue;
        }

        // Try to apply damage to structure
        if let Ok(mut structure) = structures.get_mut(target) {
            structure.hits = structure.hits.saturating_sub(amount);
            if structure.hits == 0 {
                commands.entity(target).insert(MarkedForDeath);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{Position, RoomId, DEFAULT_DRONE_LIFESPAN};
    use indexmap::IndexMap;

    fn spawn_test_drone(app: &mut App, owner: u32, hits: u32, hits_max: u32) -> Entity {
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
    fn damage_reduces_hits() {
        let mut app = App::new();
        app.add_systems(Update, damage_application_system);
        let drone = spawn_test_drone(&mut app, 1, 100, 100);
        app.insert_resource(PendingDamage {
            entries: vec![(drone, 30, "Kinetic".into())],
        });

        app.update();

        let d = app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(d.hits, 70);
    }

    #[test]
    fn damage_at_zero_marks_for_death() {
        let mut app = App::new();
        app.add_systems(Update, damage_application_system);
        let drone = spawn_test_drone(&mut app, 1, 10, 100);
        app.insert_resource(PendingDamage {
            entries: vec![(drone, 10, "Kinetic".into())],
        });

        app.update();

        let d = app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(d.hits, 0);
        assert!(
            app.world().entity(drone).contains::<MarkedForDeath>(),
            "should be marked for death"
        );
    }

    #[test]
    fn damage_skips_spawning_grace() {
        let mut app = App::new();
        app.add_systems(Update, damage_application_system);
        let drone = spawn_test_drone(&mut app, 1, 100, 100);
        app.world_mut().entity_mut(drone).insert(SpawningGrace { remaining: 1 });
        app.insert_resource(PendingDamage {
            entries: vec![(drone, 30, "Kinetic".into())],
        });

        app.update();

        let d = app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(d.hits, 100, "SpawningGrace protects from damage");
    }
}
