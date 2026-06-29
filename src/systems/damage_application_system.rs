use bevy::prelude::*;

use indexmap::IndexMap;

use crate::components::{DeathMark, Drone, MarkedForDeath, SpawningGrace, Structure};
use crate::systems::special_attack_reducer::{PendingDamage, PendingHeal};

/// S15: Damage Application System
///
/// Applies pending damage from special attacks to entities.
/// Accounts for SpawningGrace (newborn invincibility) and marks
/// entities for death when hits reach zero.
///
/// Filter: `Without<SpawningGrace>` per manifest §S15.
pub fn damage_application_system(
    mut damage: ResMut<PendingDamage>,
    mut heal: ResMut<PendingHeal>,
    mut drones: Query<&mut Drone, Without<SpawningGrace>>,
    mut structures: Query<&mut Structure, Without<SpawningGrace>>,
    mut commands: Commands,
) {
    if damage.entries.is_empty() && heal.entries.is_empty() {
        return;
    }

    let mut damage_by_target: IndexMap<Entity, u32> = IndexMap::new();
    for entry in std::mem::take(&mut damage.entries) {
        *damage_by_target.entry(entry.target).or_default() = damage_by_target
            .get(&entry.target)
            .copied()
            .unwrap_or_default()
            .saturating_add(entry.amount);
    }

    let mut heal_by_target: IndexMap<Entity, u32> = IndexMap::new();
    for entry in std::mem::take(&mut heal.entries) {
        *heal_by_target.entry(entry.target).or_default() = heal_by_target
            .get(&entry.target)
            .copied()
            .unwrap_or_default()
            .saturating_add(entry.amount);
    }

    let mut targets = damage_by_target.keys().copied().collect::<Vec<_>>();
    for target in heal_by_target.keys().copied() {
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    targets.sort_by_key(|entity| entity.to_bits());

    for target in targets {
        let amount = damage_by_target
            .get(&target)
            .copied()
            .unwrap_or_default()
            .saturating_sub(heal_by_target.get(&target).copied().unwrap_or_default());
        if let Ok(mut drone) = drones.get_mut(target) {
            let heal_amount = heal_by_target
                .get(&target)
                .copied()
                .unwrap_or_default()
                .saturating_sub(damage_by_target.get(&target).copied().unwrap_or_default());
            if heal_amount > 0 {
                drone.hits = drone.hits.saturating_add(heal_amount).min(drone.hits_max);
            } else {
                drone.hits = drone.hits.saturating_sub(amount);
            }
            if drone.hits == 0 {
                commands.entity(target).insert(DeathMark);
            }
            continue;
        }

        if let Ok(mut structure) = structures.get_mut(target) {
            structure.hits = structure.hits.saturating_sub(amount);
            if structure.hits == 0 {
                commands.entity(target).insert(DeathMark);
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
            entries: vec![(drone, 30, "Kinetic".into()).into()],
        });
        app.insert_resource(PendingHeal::default());

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
            entries: vec![(drone, 10, "Kinetic".into()).into()],
        });
        app.insert_resource(PendingHeal::default());

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
            entries: vec![(drone, 30, "Kinetic".into()).into()],
        });
        app.insert_resource(PendingHeal::default());

        app.update();

        let d = app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(d.hits, 100, "SpawningGrace protects from damage");
    }
}
