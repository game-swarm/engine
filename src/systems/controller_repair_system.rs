use bevy::prelude::*;

use crate::components::{Controller, Drone, PlayerId, Position, RepairTracker};

/// Drone age repair system — handles Controller repair only.
/// Depot repair is handled separately in depot_repair_system.
/// Both systems share RepairTracker to enforce the combined hard cap.
/// Runs after command execution, before decay.
pub fn controller_repair_system(
    mut drones: Query<(&mut Drone, &Position)>,
    controllers: Query<(&Controller, &Position)>,
    mut repair_tracker: ResMut<RepairTracker>,
) {
    let hard_cap = repair_tracker.hard_cap;

    // Collect all repair sources: Controllers with repair capacity
    let repair_sources: Vec<(&Controller, &Position)> = controllers
        .iter()
        .filter(|(c, _)| c.owner.is_some() && c.repair_capacity > 0)
        .collect();

    if repair_sources.is_empty() {
        return;
    }

    for (mut drone, drone_pos) in drones.iter_mut() {
        if drone.age == 0 {
            continue;
        }

        let player_id = drone.owner;

        // Check hard cap — shared across Controller + Depot
        let total_so_far = *repair_tracker.per_player.get(&player_id).unwrap_or(&0);
        if total_so_far >= hard_cap {
            continue;
        }

        let remaining_cap = hard_cap - total_so_far;

        // Check each repair source
        for (controller, ctrl_pos) in &repair_sources {
            if controller.owner != Some(player_id) {
                continue; // Only repair own drones
            }

            // Check range
            let dx = (drone_pos.x - ctrl_pos.x).unsigned_abs();
            let dy = (drone_pos.y - ctrl_pos.y).unsigned_abs();
            let distance = dx.max(dy) as u32;
            if distance > controller.repair_range {
                continue;
            }

            // Apply repair
            let repair_amount = controller.repair_per_drone.min(drone.age);
            let actual_repair = repair_amount.min(remaining_cap);

            if actual_repair > 0 {
                drone.age = drone.age.saturating_sub(actual_repair);
                *repair_tracker.per_player.entry(player_id).or_default() += actual_repair;
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
    use indexmap::IndexMap;

    #[test]
    fn controller_repairs_drone_in_range() {
        let mut world = create_world();
        // Insert RepairTracker
        world.app.world_mut().insert_resource(RepairTracker {
            per_player: IndexMap::new(),
            hard_cap: 1,
        });
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
                    aging_remainder: 0,
                    lifespan: DEFAULT_DRONE_LIFESPAN,
                    executed_command_this_tick: false,
                },
                Position {
                    x: 5,
                    y: 5,
                    room: RoomId(0),
                },
            ))
            .id();

        world.app.world_mut().spawn((
            Controller {
                owner: Some(1),
                level: 3,
                progress: 100,
                progress_total: 400,
                downgrade_timer: 5000,
                safe_mode: 0,
                safe_mode_available: 0,
                safe_mode_cooldown: 0,
                repair_capacity: 20,
                repair_range: 2,
                repair_per_drone: 2,
            },
            Position {
                x: 5,
                y: 5,
                room: RoomId(0),
            },
        ));

        world.app.update();

        let drone_after = world.app.world().entity(drone).get::<Drone>().unwrap();
        assert!(drone_after.age <= 10, "repair should offset natural decay");
    }
}
