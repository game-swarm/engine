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
    // Allocate an exact proportional share using largest remainders. Resource
    // names break equal-remainder ties so insertion order cannot change results.
    let total_cargo = resources.values().map(|amount| *amount as u64).sum::<u64>();
    if total_cargo == 0 {
        return None;
    }
    let steal_amount = (available_carry as u64).min(total_cargo) as u32;
    let mut allocations = resources
        .iter()
        .map(|(resource, amount)| {
            let numerator = *amount as u64 * steal_amount as u64;
            (
                resource.clone(),
                (numerator / total_cargo) as u32,
                numerator % total_cargo,
            )
        })
        .collect::<Vec<_>>();
    let allocated = allocations
        .iter()
        .map(|(_, amount, _)| *amount)
        .sum::<u32>();
    allocations.sort_by(|left, right| right.2.cmp(&left.2).then_with(|| left.0.cmp(&right.0)));
    for (_, amount, _) in allocations
        .iter_mut()
        .take(steal_amount.saturating_sub(allocated) as usize)
    {
        *amount += 1;
    }
    for (resource, taken, _) in allocations {
        let amount = resources
            .get_mut(&resource)
            .expect("allocation keys come from the resource map");
        *amount -= taken;
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

    #[test]
    fn interception_removes_exact_capacity_without_proportional_oversteal() {
        let mut resources = IndexMap::from([
            ("Zynthium".to_string(), 1),
            ("Energy".to_string(), 1),
            ("Oxygen".to_string(), 1),
        ]);
        let thief = Drone {
            owner: 2,
            body: Vec::new(),
            carry: IndexMap::new(),
            carry_capacity: 2,
            fatigue: 0,
            hits: 100,
            hits_max: 100,
            spawning: false,
            age: 0,
            last_action_tick: 0,
            lifespan: 1_500,
        };

        assert_eq!(intercept_cargo(&mut resources, &thief), Some(2));
        assert_eq!(resources.values().sum::<u32>(), 1);
        assert_eq!(resources["Energy"], 0);
        assert_eq!(resources["Oxygen"], 0);
        assert_eq!(resources["Zynthium"], 1);
    }

    #[test]
    fn interception_accounts_for_used_capacity_exactly() {
        let mut resources = IndexMap::from([("Energy".to_string(), 5), ("Oxygen".to_string(), 3)]);
        let thief = Drone {
            owner: 2,
            body: Vec::new(),
            carry: IndexMap::from([("Energy".to_string(), 2)]),
            carry_capacity: 5,
            fatigue: 0,
            hits: 100,
            hits_max: 100,
            spawning: false,
            age: 0,
            last_action_tick: 0,
            lifespan: 1_500,
        };

        assert_eq!(intercept_cargo(&mut resources, &thief), Some(3));
        assert_eq!(resources.values().sum::<u32>(), 5);
        assert_eq!(resources["Energy"], 3);
        assert_eq!(resources["Oxygen"], 2);
    }
}
