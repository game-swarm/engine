pub mod components;
pub mod systems;
pub mod world;

pub use components::*;
pub use world::{SwarmWorld, create_world};

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use crate::{components::*, create_world, systems::*};

    fn drone_count(world: &mut crate::SwarmWorld) -> usize {
        world
            .app
            .world_mut()
            .query::<&Drone>()
            .iter(world.app.world())
            .count()
    }

    fn drone_hits(world: &mut crate::SwarmWorld) -> u32 {
        *world
            .app
            .world_mut()
            .query::<&Drone>()
            .iter(world.app.world())
            .map(|drone| &drone.hits)
            .next()
            .expect("expected one drone")
    }

    fn default_structure(cooldown: u32) -> Structure {
        Structure {
            structure_type: StructureType::Spawn,
            owner: Some(1),
            hits: 5_000,
            hits_max: 5_000,
            energy: Some(0),
            energy_capacity: Some(300),
            cooldown,
        }
    }

    #[test]
    fn default_world_has_plain_terrain_and_source() {
        let mut world = create_world();

        assert_eq!(world.get_terrain(RoomId(0), 0, 0), Some(TerrainType::Plain));
        assert_eq!(
            world.get_terrain(RoomId(0), 49, 49),
            Some(TerrainType::Plain)
        );
        assert_eq!(world.get_terrain(RoomId(0), 50, 50), None);
        assert!(world.is_passable(RoomId(0), 25, 25));

        let sources = world
            .app
            .world_mut()
            .query::<(&Position, &Source)>()
            .iter(world.app.world())
            .map(|(position, source)| (*position, source.clone()))
            .collect::<Vec<_>>();

        assert_eq!(sources.len(), 1);
        assert_eq!(
            sources[0].0,
            Position {
                x: 25,
                y: 25,
                room: RoomId(0)
            }
        );
        assert_eq!(sources[0].1.amount, 3_000);
        assert_eq!(sources[0].1.capacity, 3_000);
        assert_eq!(sources[0].1.produces.get("Energy"), Some(&1));
    }

    #[test]
    fn terrain_set_get_and_passable_track_each_other() {
        let mut world = create_world();

        assert!(world.set_terrain(RoomId(0), 3, 4, TerrainType::Swamp));
        assert_eq!(world.get_terrain(RoomId(0), 3, 4), Some(TerrainType::Swamp));
        assert!(world.is_passable(RoomId(0), 3, 4));

        assert!(world.set_terrain(RoomId(0), 3, 4, TerrainType::Wall));
        assert_eq!(world.get_terrain(RoomId(0), 3, 4), Some(TerrainType::Wall));
        assert!(!world.is_passable(RoomId(0), 3, 4));
        assert!(!world.set_terrain(RoomId(0), 50, 0, TerrainType::Plain));
        assert_eq!(world.get_terrain(RoomId(0), 50, 0), None);
    }

    #[test]
    fn queue_spawn_spawns_on_run_tick() {
        let mut world = create_world();

        world.queue_spawn(7, 10, 10, vec![BodyPart::Move, BodyPart::Work]);

        assert_eq!(drone_count(&mut world), 0);
        assert_eq!(world.app.world().resource::<PendingSpawnQueue>().0.len(), 1);

        world.run_tick();

        assert_eq!(drone_count(&mut world), 1);
        assert_eq!(world.app.world().resource::<PendingSpawnQueue>().0.len(), 0);
        assert_eq!(
            world
                .app
                .world()
                .resource::<RoomDroneCounts>()
                .0
                .get(&(RoomId(0), 7)),
            Some(&1)
        );
    }

    #[test]
    fn spawn_system_rejects_wall_tile_spawns() {
        let mut world = create_world();

        assert!(world.set_terrain(RoomId(0), 12, 12, TerrainType::Wall));
        world.queue_spawn(7, 12, 12, vec![BodyPart::Move]);
        world.run_tick();

        assert_eq!(drone_count(&mut world), 0);
        assert_eq!(
            world
                .app
                .world()
                .resource::<RoomDroneCounts>()
                .0
                .get(&(RoomId(0), 7))
                .copied()
                .unwrap_or_default(),
            0
        );
    }

    #[test]
    fn phase2b_death_mark_frees_count_before_spawn_then_cleanup_removes_dead() {
        let mut world = create_world();
        let old_drone = world.spawn_drone(9, 13, 13, vec![BodyPart::Move]);
        world
            .app
            .world_mut()
            .entity_mut(old_drone)
            .get_mut::<Drone>()
            .unwrap()
            .hits = 0;

        world.queue_spawn(9, 14, 14, vec![BodyPart::Work]);
        world.run_tick();

        assert!(world.app.world().get_entity(old_drone).is_err());
        assert_eq!(drone_count(&mut world), 1);
        assert_eq!(
            world
                .app
                .world()
                .resource::<RoomDroneCounts>()
                .0
                .get(&(RoomId(0), 9)),
            Some(&1)
        );
    }

    #[test]
    fn combat_applies_damage_before_heal() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);

        {
            let mut combat = world.app.world_mut().resource_mut::<PendingCombat>();
            combat.damage.push((drone, 60));
            combat.heal.push((drone, 30));
        }

        world.run_tick();

        assert_eq!(drone_hits(&mut world), 70);
    }

    #[test]
    fn decay_reduces_fatigue_and_cooldown_and_increments_age() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let structure = world
            .app
            .world_mut()
            .spawn((
                Position {
                    x: 11,
                    y: 10,
                    room: RoomId(0),
                },
                default_structure(3),
            ))
            .id();

        world
            .app
            .world_mut()
            .entity_mut(drone)
            .get_mut::<Drone>()
            .unwrap()
            .fatigue = 2;
        world
            .app
            .world_mut()
            .entity_mut(drone)
            .get_mut::<Drone>()
            .unwrap()
            .age = 7;

        world.run_tick();

        let drone_ref = world.app.world().entity(drone).get::<Drone>().unwrap();
        let structure_ref = world
            .app
            .world()
            .entity(structure)
            .get::<Structure>()
            .unwrap();
        assert_eq!(drone_ref.fatigue, 1);
        assert_eq!(drone_ref.age, 8);
        assert_eq!(structure_ref.cooldown, 2);
    }

    #[test]
    fn death_cleanup_removes_dead_drone_and_decrements_room_count() {
        let mut world = create_world();
        let drone = world.spawn_drone(42, 10, 10, vec![BodyPart::Move]);
        world
            .app
            .world_mut()
            .entity_mut(drone)
            .get_mut::<Drone>()
            .unwrap()
            .hits = 0;

        world.run_tick();

        assert_eq!(drone_count(&mut world), 0);
        assert_eq!(
            world
                .app
                .world()
                .resource::<RoomDroneCounts>()
                .0
                .get(&(RoomId(0), 42)),
            Some(&0)
        );
    }

    #[test]
    fn state_checksum_is_deterministic_and_changes_with_state() {
        let mut first = create_world();
        let mut second = create_world();

        let first_checksum = first.state_checksum();
        assert_eq!(first_checksum, first.state_checksum());
        assert_eq!(first_checksum, second.state_checksum());

        first.spawn_drone(
            1,
            10,
            10,
            vec![BodyPart::Move, BodyPart::Work, BodyPart::Carry],
        );
        assert_ne!(first_checksum, first.state_checksum());

        let terrain_checksum = second.state_checksum();
        second.set_terrain(RoomId(0), 0, 0, TerrainType::Wall);
        assert_ne!(terrain_checksum, second.state_checksum());

        let structure_checksum = second.state_checksum();
        second.app.world_mut().spawn((
            Position {
                x: 1,
                y: 1,
                room: RoomId(0),
            },
            default_structure(0),
        ));
        assert_ne!(structure_checksum, second.state_checksum());

        let controller_checksum = second.state_checksum();
        second.app.world_mut().spawn((
            Position {
                x: 2,
                y: 2,
                room: RoomId(0),
            },
            Controller {
                owner: Some(1),
                level: 2,
                progress: 10,
                progress_total: 200,
                downgrade_timer: 1_000,
                safe_mode: 0,
                safe_mode_available: 1,
                safe_mode_cooldown: 0,
            },
        ));
        assert_ne!(controller_checksum, second.state_checksum());

        let resource_checksum = second.state_checksum();
        let mut amounts = IndexMap::new();
        amounts.insert("Energy".to_string(), 42);
        second.app.world_mut().spawn((
            Position {
                x: 3,
                y: 3,
                room: RoomId(0),
            },
            Resource { amounts },
        ));
        assert_ne!(resource_checksum, second.state_checksum());
    }
}
