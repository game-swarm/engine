use bevy::prelude::*;

use crate::components::Drone;
use crate::resources::PlayerLocalStorage;
use crate::world::WorldConfig;

pub fn memory_upkeep_system(
    config: Res<WorldConfig>,
    drones: Query<&Drone>,
    mut local_storage: ResMut<PlayerLocalStorage>,
) {
    if config.drone.memory_upkeep_cost.is_empty() {
        return;
    }

    for drone in drones.iter() {
        let storage = local_storage.0.entry(drone.owner).or_default();
        for (resource, cost) in &config.drone.memory_upkeep_cost {
            let current = storage.entry(resource.clone()).or_default();
            *current = current.saturating_sub(*cost);
        }
    }
}
