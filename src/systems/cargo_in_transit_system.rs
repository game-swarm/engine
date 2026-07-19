use bevy::prelude::*;
use indexmap::IndexMap;
use swarm_engine_api::ids::{PlayerId, RoomId};
use swarm_engine_plugin_sdk::components::{Drone, Position};

/// Cargo in transit between global and local storage.
/// Represents resources being transported, vulnerable to enemy interception.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CargoInTransit {
    pub resources: IndexMap<String, u32>,
    pub owner: PlayerId,
    pub origin_room: RoomId,
    pub destination_room: RoomId,
    pub remaining_ticks: u32,
}

/// System that processes cargo in transit — ticks down timers and handles interception.
pub fn cargo_in_transit_system(
    mut commands: Commands,
    cargo_entities: Query<(Entity, &CargoInTransit, &Position)>,
    enemy_drones: Query<(&Drone, &Position)>,
) {
    let mut to_deliver: Vec<(Entity, IndexMap<String, u32>, PlayerId)> = Vec::new();

    for (entity, cargo, cargo_pos) in cargo_entities.iter() {
        let mut remaining = cargo.remaining_ticks;
        let mut resources = cargo.resources.clone();
        let mut intercepted = false;

        if remaining > 0 {
            // Check for enemy interception on same tile
            for (drone, drone_pos) in enemy_drones.iter() {
                if drone.owner == cargo.owner {
                    continue; // Same player's drones don't intercept
                }
                if drone_pos.x == cargo_pos.x
                    && drone_pos.y == cargo_pos.y
                    && drone_pos.room == cargo_pos.room
                {
                    // Interception: enemy steals resources proportional to CARRY capacity
                    if let Some(stolen_amount) = intercept_cargo(&mut resources, drone) {
                        intercepted = true;
                        // Intercepted: deliver partial to thief
                        // (The thief's resources are added via a command queue in full impl)
                        if stolen_amount > 0 {
                            // For now: mark cargo as intercepted, return partial
                            break;
                        }
                    }
                }
            }

            remaining = remaining.saturating_sub(1);
        }

        if remaining == 0 || intercepted {
            to_deliver.push((entity, resources, cargo.owner));
        } else {
            // Update remaining ticks
            commands.entity(entity).insert(CargoInTransit {
                remaining_ticks: remaining,
                ..cargo.clone()
            });
        }
    }

    // Despawn delivered/intercepted cargo entities
    for (entity, _resources, _owner) in to_deliver {
        commands.entity(entity).despawn();
    }
}

/// Attempt to intercept cargo. Returns the amount stolen.
fn intercept_cargo(resources: &mut IndexMap<String, u32>, thief: &Drone) -> Option<u32> {
    let carry_capacity = thief.carry_capacity;
    let used_carry: u32 = thief.carry.values().sum();
    let available_carry = carry_capacity.saturating_sub(used_carry);
    if available_carry == 0 {
        return None;
    }
    // Steal up to available carry capacity, proportional across resources
    let total_cargo: u32 = resources.values().sum();
    if total_cargo == 0 {
        return None;
    }
    let steal_amount = available_carry.min(total_cargo);
    let ratio = steal_amount as f64 / total_cargo as f64;
    for amount in resources.values_mut() {
        let taken = (*amount as f64 * ratio).ceil() as u32;
        *amount = amount.saturating_sub(taken);
    }
    Some(steal_amount)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_in_transit_has_correct_defaults() {
        let mut resources = IndexMap::new();
        resources.insert("Energy".to_string(), 100);
        let cargo = CargoInTransit {
            resources,
            owner: 1,
            origin_room: RoomId(0),
            destination_room: RoomId(1),
            remaining_ticks: 10,
        };
        assert_eq!(cargo.remaining_ticks, 10);
        assert_eq!(cargo.owner, 1);
    }
}
