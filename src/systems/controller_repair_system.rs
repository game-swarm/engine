use bevy::prelude::*;

use crate::components::{Controller, Drone, Position, RepairTracker};
use crate::resources::{PlayerGlobalStorage, ResourceRegistry};
use crate::tick::{TickTraceEvent, TickTraceEventLog};
use crate::world::WorldConfig;

/// Drone body hits repair system — handles Controller repair only.
/// Depot repair is handled separately in depot_repair_system.
/// Both systems share RepairTracker to enforce the combined hard cap.
/// Runs after command execution, before decay.
pub fn controller_repair_system(
    mut drones: Query<(Entity, &mut Drone, &Position)>,
    controllers: Query<(&Controller, &Position)>,
    registry: Res<ResourceRegistry>,
    config: Res<WorldConfig>,
    mut global_storage: ResMut<PlayerGlobalStorage>,
    mut repair_tracker: ResMut<RepairTracker>,
    mut trace_events: ResMut<TickTraceEventLog>,
) {
    let hard_cap = repair_tracker.hard_cap;
    let mut body_repair_this_tick = 0;

    // Collect all repair sources: Controllers with repair capacity
    let repair_sources: Vec<(&Controller, &Position)> = controllers
        .iter()
        .filter(|(c, _)| c.owner.is_some() && c.repair_capacity > 0)
        .collect();

    if repair_sources.is_empty() {
        return;
    }

    for (entity, mut drone, drone_pos) in drones.iter_mut() {
        if drone.hits >= drone.hits_max {
            continue;
        }

        if body_repair_this_tick >= hard_cap {
            continue;
        }

        let player_id = drone.owner;

        // Check hard cap — shared across Controller + Depot
        let total_so_far = *repair_tracker.per_player.get(&player_id).unwrap_or(&0);
        if total_so_far >= hard_cap {
            continue;
        }

        // Check each repair source
        for (controller, ctrl_pos) in &repair_sources {
            if controller.owner != Some(player_id) {
                continue; // Only repair own drones
            }

            // Check range
            let dx = (drone_pos.x - ctrl_pos.x).unsigned_abs();
            let dy = (drone_pos.y - ctrl_pos.y).unsigned_abs();
            let distance = dx.max(dy);
            if distance > controller.repair_range {
                continue;
            }

            let repair_amount = controller
                .repair_per_drone
                .min(drone.hits_max.saturating_sub(drone.hits))
                .min(hard_cap.saturating_sub(total_so_far))
                .min(hard_cap.saturating_sub(body_repair_this_tick));
            if repair_amount == 0 {
                break;
            }

            let body_cost = registry.body_energy_cost(&drone.body);
            let full_repair_cost = config.empire_upkeep.repair_cost(body_cost, distance);
            let repair_cost = (u64::from(full_repair_cost).saturating_mul(u64::from(repair_amount))
                / u64::from(drone.hits_max.max(1))) as u32;
            let storage = global_storage.0.entry(player_id).or_default();
            let energy = storage
                .entry(config.empire_upkeep.resource.clone())
                .or_default();
            if *energy < repair_cost {
                break;
            }

            *energy -= repair_cost;
            drone.hits = drone.hits.saturating_add(repair_amount).min(drone.hits_max);
            body_repair_this_tick += repair_amount;
            *repair_tracker.per_player.entry(player_id).or_default() += repair_amount;
            trace_events.events.push(TickTraceEvent {
                system: "controller_repair_system".to_string(),
                entity: entity.to_bits(),
                event: "controller_age_repair".to_string(),
                amount: repair_cost,
                resource: Some(config.empire_upkeep.resource.clone()),
            });
            break; // One repair per tick per drone
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{BodyPart, DEFAULT_DRONE_LIFESPAN, RoomId};
    use crate::world::create_world;
    use indexmap::IndexMap;

    #[test]
    fn controller_repairs_body_hits_in_range() {
        let mut world = create_world();
        world.app.world_mut().insert_resource(RepairTracker {
            per_player: IndexMap::new(),
            hard_cap: 10,
        });
        world
            .app
            .world_mut()
            .resource_mut::<PlayerGlobalStorage>()
            .0
            .entry(1)
            .or_default()
            .insert("Energy".to_string(), 1_000);
        let drone = world
            .app
            .world_mut()
            .spawn((
                Drone {
                    owner: 1,
                    body: vec![BodyPart::Move, BodyPart::Work],
                    carry: IndexMap::new(),
                    carry_capacity: 0,
                    fatigue: 0,
                    hits: 90,
                    hits_max: 100,
                    spawning: false,
                    age: 0,
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
                x: 6,
                y: 5,
                room: RoomId(0),
            },
        ));

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
                repair_per_drone: 5,
            },
            Position {
                x: 5,
                y: 5,
                room: RoomId(0),
            },
        ));

        world.app.update();

        let drone_after = world.app.world().entity(drone).get::<Drone>().unwrap();
        // Controller repair -5, regeneration +1 → net 96
        assert_eq!(drone_after.hits, 96);
    }

    #[test]
    fn controller_repair_cost_increases_with_distance() {
        let world = create_world();
        let registry = world.app.world().resource::<ResourceRegistry>();
        let body_cost = registry.body_energy_cost(&[BodyPart::Move, BodyPart::Work]);
        let config = &world.app.world().resource::<WorldConfig>().empire_upkeep;

        assert!(config.repair_cost(body_cost, 3) > config.repair_cost(body_cost, 0));
    }
}
