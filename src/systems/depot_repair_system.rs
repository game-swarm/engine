use bevy::prelude::*;
use indexmap::IndexMap;

use crate::components::{
    Drone, PlayerId, Position, RepairTracker, Structure, StructureType, StructureTypeRegistry,
};

/// Depot repair system — runs after controller_repair_system.
/// Depots within range consume stored energy to reduce drone age.
/// Combined Controller + Depot repair cannot exceed RepairTracker.hard_cap per tick.
pub fn depot_repair_system(
    mut drones: Query<(&mut Drone, &Position)>,
    mut depots: Query<(&mut Structure, &Position)>,
    registry: Res<StructureTypeRegistry>,
    mut repair_tracker: ResMut<RepairTracker>,
) {
    let hard_cap = repair_tracker.hard_cap;

    // Look up Depot type definition for repair parameters
    let depot_def = match registry.structure_types.get(&StructureType::DEPOT) {
        Some(d) => d,
        None => return,
    };

    let repair_range = depot_def.repair_range.unwrap_or(0);
    let repair_aging = depot_def.repair_aging.unwrap_or(0);
    let maintenance_energy = depot_def.maintenance.get("Energy").copied().unwrap_or(0);

    if repair_range == 0 || repair_aging == 0 {
        return;
    }

    // Track remaining repair capacity per depot entity
    // repair_capacity on the def is Some(10), we track locally to avoid mutating the registry
    let per_depot_capacity: u32 = depot_def.repair_capacity.unwrap_or(0);
    let mut depot_repairs_used: IndexMap<PlayerId, u32> = IndexMap::new();

    for (mut drone, drone_pos) in drones.iter_mut() {
        if drone.age == 0 {
            continue;
        }

        let player_id = drone.owner;

        // Check shared hard cap (Controller + Depot combined)
        let total_so_far = *repair_tracker.per_player.get(&player_id).unwrap_or(&0);
        if total_so_far >= hard_cap {
            continue;
        }

        let remaining_cap = hard_cap - total_so_far;

        for (mut structure, depot_pos) in depots.iter_mut() {
            if structure.structure_type != StructureType::DEPOT {
                continue;
            }

            // Only repair player's own drones (if owned) or unowned depots repair anyone
            if structure.owner.is_some() && structure.owner != Some(player_id) {
                continue;
            }

            // Check per-depot capacity
            let used = depot_repairs_used.get(&player_id).unwrap_or(&0);
            if *used >= per_depot_capacity {
                continue;
            }

            // Check range
            let dx = (drone_pos.x - depot_pos.x).unsigned_abs();
            let dy = (drone_pos.y - depot_pos.y).unsigned_abs();
            let distance = dx.max(dy) as u32;
            if distance > repair_range {
                continue;
            }

            // Check maintenance energy
            let energy = structure.energy.unwrap_or(0);
            if energy < maintenance_energy {
                continue;
            }

            // Consume maintenance energy
            structure.energy = Some(energy - maintenance_energy);

            // Apply repair
            let repair_amount = repair_aging.min(drone.age);
            let actual_repair = repair_amount.min(remaining_cap);

            if actual_repair > 0 {
                drone.age = drone.age.saturating_sub(actual_repair);
                *repair_tracker.per_player.entry(player_id).or_default() += actual_repair;
                *depot_repairs_used.entry(player_id).or_default() += actual_repair;
            }
            break; // One repair per tick per drone
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{DEFAULT_DRONE_LIFESPAN, RoomId};
    use crate::world::create_world;
    use bevy::prelude::*;

    #[test]
    fn depot_repairs_drone_in_range() {
        let mut world = create_world();
        world.app.world_mut().insert_resource(RepairTracker {
            per_player: IndexMap::new(),
            hard_cap: 1,
        });
        // Spawn a drone with age
        let drone = world
            .app
            .world_mut()
            .spawn((
                Drone {
                    owner: 1,
                    body: vec![],
                    carry: IndexMap::new(),
                    carry_capacity: 0,
                    fatigue: 0,
                    hits: 100,
                    hits_max: 100,
                    spawning: false,
                    age: 10,
                    last_action_tick: u64::MAX,
                    lifespan: DEFAULT_DRONE_LIFESPAN,
                },
                Position {
                    x: 5,
                    y: 5,
                    room: RoomId(0),
                },
            ))
            .id();

        // Spawn a Depot with energy
        world.app.world_mut().spawn((
            Structure {
                structure_type: StructureType::DEPOT,
                owner: Some(1),
                hits: 3000,
                hits_max: 3000,
                energy: Some(100),
                energy_capacity: Some(500),
                cooldown: 0,
            },
            Position {
                x: 5,
                y: 5,
                room: RoomId(0),
            },
        ));

        world.app.update();

        let drone_after = world.app.world().entity(drone).get::<Drone>().unwrap();
        // Depot repair -1; aging adds +1 (S23) → net unchanged
        assert_eq!(
            drone_after.age, 10,
            "depot repair -1, aging +1; expected 10, got {}",
            drone_after.age
        );
    }

    #[test]
    fn depot_stops_when_energy_exhausted() {
        let mut world = create_world();
        world.app.world_mut().insert_resource(RepairTracker {
            per_player: IndexMap::new(),
            hard_cap: 100,
        });
        // Spawn a drone
        world.app.world_mut().spawn((
            Drone {
                owner: 1,
                body: vec![],
                carry: IndexMap::new(),
                carry_capacity: 0,
                fatigue: 0,
                hits: 100,
                hits_max: 100,
                spawning: false,
                age: 10,
                last_action_tick: u64::MAX,
                lifespan: DEFAULT_DRONE_LIFESPAN,
            },
            Position {
                x: 0,
                y: 0,
                room: RoomId(0),
            },
        ));
        // Depot with 0 energy
        world.app.world_mut().spawn((
            Structure {
                structure_type: StructureType::DEPOT,
                owner: Some(1),
                hits: 3000,
                hits_max: 3000,
                energy: Some(0),
                energy_capacity: Some(500),
                cooldown: 0,
            },
            Position {
                x: 0,
                y: 0,
                room: RoomId(0),
            },
        ));

        world.app.update();

        let drones: Vec<&Drone> = world
            .app
            .world_mut()
            .query::<&Drone>()
            .iter(world.app.world())
            .collect();
        // No repair should happen — energy is 0, aging adds +1 (S23) → 11
        assert_eq!(
            drones[0].age, 11,
            "drone age +1 from aging when depot has no energy; got {}",
            drones[0].age
        );
    }

    #[test]
    fn depot_repairs_player_own_drones_only() {
        let mut world = create_world();
        world.app.world_mut().insert_resource(RepairTracker {
            per_player: IndexMap::new(),
            hard_cap: 100,
        });
        // Player 1 drone
        world.app.world_mut().spawn((
            Drone {
                owner: 1,
                body: vec![],
                carry: IndexMap::new(),
                carry_capacity: 0,
                fatigue: 0,
                hits: 100,
                hits_max: 100,
                spawning: false,
                age: 10,
                last_action_tick: u64::MAX,
                lifespan: DEFAULT_DRONE_LIFESPAN,
            },
            Position {
                x: 0,
                y: 0,
                room: RoomId(0),
            },
        ));
        // Player 2 drone
        world.app.world_mut().spawn((
            Drone {
                owner: 2,
                body: vec![],
                carry: IndexMap::new(),
                carry_capacity: 0,
                fatigue: 0,
                hits: 100,
                hits_max: 100,
                spawning: false,
                age: 10,
                last_action_tick: u64::MAX,
                lifespan: DEFAULT_DRONE_LIFESPAN,
            },
            Position {
                x: 0,
                y: 0,
                room: RoomId(0),
            },
        ));
        // Player 1 Depot
        world.app.world_mut().spawn((
            Structure {
                structure_type: StructureType::DEPOT,
                owner: Some(1),
                hits: 3000,
                hits_max: 3000,
                energy: Some(100),
                energy_capacity: Some(500),
                cooldown: 0,
            },
            Position {
                x: 0,
                y: 0,
                room: RoomId(0),
            },
        ));

        world.app.update();

        let drones: Vec<&Drone> = world
            .app
            .world_mut()
            .query::<&Drone>()
            .iter(world.app.world())
            .collect();
        // Player 1 drone: repair -2, aging +1 (S23) → 9 (<10)
        assert!(drones[0].age < 10, "player 1's drone should be repaired");
        // Player 2 drone NOT repaired, aging +1 (S23) → 11
        assert_eq!(
            drones[1].age, 11,
            "player 2's drone should NOT be repaired; aging +1, got {}",
            drones[1].age
        );
    }
}
