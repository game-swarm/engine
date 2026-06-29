use bevy::prelude::*;
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};

use crate::command::Tick;
use crate::components::{Controller, Drone, Owner, PlayerId};
use crate::resources::PlayerGlobalStorage;
use crate::world::WorldConfig;

/// Tracks which players have received starting resources.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartingResourcesGranted(pub IndexSet<PlayerId>);

/// Tracks each player's first spawn tick for free upkeep timing.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerFirstSpawnTick(pub IndexMap<PlayerId, Tick>);

/// Grants starting resources to players on their first entity spawn,
/// and seeds the first-spawn tick for free upkeep tracking.
pub fn starting_resources_system(
    config: Res<WorldConfig>,
    current_tick: Res<crate::resources::CurrentTick>,
    controllers: Query<&Controller>,
    drones: Query<&Drone, With<Owner>>,
    mut granted: ResMut<StartingResourcesGranted>,
    mut first_spawn: ResMut<PlayerFirstSpawnTick>,
    mut global_storage: ResMut<PlayerGlobalStorage>,
) {
    let tick = current_tick.0;

    // Record first-spawn ticks for new players
    for controller in controllers.iter() {
        if let Some(owner) = controller.owner {
            first_spawn.0.entry(owner).or_insert(tick);
            if !granted.0.contains(&owner)
                && !config.starting_resources.starting_resources.is_empty()
            {
                let storage = global_storage.0.entry(owner).or_default();
                for (resource, amount) in &config.starting_resources.starting_resources {
                    let entry = storage.entry(resource.clone()).or_default();
                    *entry = entry.saturating_add(*amount);
                }
                granted.0.insert(owner);
            }
        }
    }

    // For players with drones but no controller yet
    for drone in drones.iter() {
        let owner = drone.owner;
        if first_spawn.0.contains_key(&owner) {
            continue;
        }
        first_spawn.0.insert(owner, tick);
        if !granted.0.contains(&owner) && !config.starting_resources.starting_resources.is_empty() {
            let storage = global_storage.0.entry(owner).or_default();
            for (resource, amount) in &config.starting_resources.starting_resources {
                let entry = storage.entry(resource.clone()).or_default();
                *entry = entry.saturating_add(*amount);
            }
            granted.0.insert(owner);
        }
    }
}
