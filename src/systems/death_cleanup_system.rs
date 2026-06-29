use bevy::prelude::*;

use crate::components::{DeathMark, Drone, MarkedForDeath, Position, Source};

/// Death cleanup system (S25) — deterministic despawn of all MarkedForDeath
/// entities, ordered by entity index descending (stable ordering).
/// When a Drone dies carrying resources, the carry is dropped into any Source
/// at the same position (capped at the Source's capacity).
pub fn death_cleanup_system(
    mut commands: Commands,
    marked: Query<(Entity, Option<&Drone>, Option<&Position>), With<MarkedForDeath>>,
    mut sources: Query<(&Position, &mut Source)>,
) {
    // Collect all marked entities with their entity index for deterministic ordering
    let mut marked_entities: Vec<(Entity, Option<&Drone>, Option<&Position>)> =
        marked.iter().map(|(e, d, p)| (e, d, p)).collect();
    // Sort by entity index descending (deterministic despawn order)
    marked_entities.sort_by_key(|(entity, _, _)| std::cmp::Reverse(entity.index()));

    for (entity, drone, position) in marked_entities {
        // Drop carried resources to a Source at the same position (if any)
        if let (Some(drone), Some(position)) = (drone, position) {
            if !drone.carry.is_empty() {
                for (pos, mut source) in sources.iter_mut() {
                    if pos == position {
                        let mut total_dropped: u32 = 0;
                        for amount in drone.carry.values() {
                            total_dropped = total_dropped.saturating_add(*amount);
                        }
                        // Carry drop accelerates source regeneration
                        let free_capacity = source.capacity.saturating_sub(
                            source.ticks_to_regeneration
                        );
                        let effective = total_dropped.min(free_capacity);
                        source.ticks_to_regeneration =
                            source.ticks_to_regeneration.saturating_sub(effective);
                        break;
                    }
                }
            }
        }
        commands.entity(entity).despawn_recursive();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::BodyPartRegistry;
    use crate::world::create_world;
    use indexmap::IndexMap;

    fn spawn_dead_drone(world: &mut crate::SwarmWorld, owner: u32, x: i32, y: i32) -> Entity {
        world
            .app
            .world_mut()
            .spawn((
                Drone {
                    owner,
                    body: vec![],
                    carry: IndexMap::new(),
                    carry_capacity: 0,
                    fatigue: 0,
                    hits: 0,
                    hits_max: 100,
                    spawning: false,
                    age: 0,
                    last_action_tick: u64::MAX,
                    lifespan: 1500,
                },
                Position {
                    x,
                    y,
                    room: crate::components::RoomId(0),
                },
                DeathMark,
            ))
            .id()
    }

    #[test]
    fn despawns_marked_drones_in_deterministic_order() {
        let mut world = create_world();
        let a = spawn_dead_drone(&mut world, 1, 10, 10);
        let b = spawn_dead_drone(&mut world, 1, 11, 10);
        let c = spawn_dead_drone(&mut world, 1, 12, 10);

        // Verify all exist before system runs
        assert!(world.app.world().get_entity(a).is_ok());
        assert!(world.app.world().get_entity(b).is_ok());
        assert!(world.app.world().get_entity(c).is_ok());

        world.app.update();

        // All marked entities should be despawned
        assert!(world.app.world().get_entity(a).is_err());
        assert!(world.app.world().get_entity(b).is_err());
        assert!(world.app.world().get_entity(c).is_err());
    }

    #[test]
    fn leaves_unmarked_drones_alone() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![]);

        world.app.update();

        // Unmarked drone should survive
        assert!(world.app.world().get_entity(drone).is_ok());
    }

    #[test]
    fn drops_carry_to_source_at_same_position() {
        let mut world = create_world();
        // Spawn a source at position (10,10) with capacity 500
        world.app.world_mut().spawn((
            Position {
                x: 10,
                y: 10,
                room: crate::components::RoomId(0),
            },
            Source {
                produces: IndexMap::new(),
                capacity: 500,
                ticks_to_regeneration: 100,
            },
        ));
        // Spawn a dead drone at the same position carrying resources
        let mut carry = IndexMap::new();
        carry.insert("Energy".to_string(), 50);
        world.app.world_mut().spawn((
            Drone {
                owner: 1,
                body: vec![],
                carry,
                carry_capacity: 50,
                fatigue: 0,
                hits: 0,
                hits_max: 100,
                spawning: false,
                age: 0,
                last_action_tick: u64::MAX,
                lifespan: 1500,
            },
            Position {
                x: 10,
                y: 10,
                room: crate::components::RoomId(0),
            },
            DeathMark,
        ));

        world.app.update();

        // Source regeneration timer should be reduced by the dropped resources
        let ticks = {
            let source = world
                .app
                .world_mut()
                .query::<(&Position, &Source)>()
                .iter(world.app.world())
                .find(|(pos, _)| pos.x == 10 && pos.y == 10)
                .unwrap()
                .1
                .ticks_to_regeneration;
            source
        };
        assert_eq!(
            ticks, 50,
            "source regen should decrease by dropped carry amount (50)"
        );
    }
}
