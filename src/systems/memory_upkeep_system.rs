use bevy::prelude::*;

use indexmap::{IndexMap, IndexSet};
use swarm_engine_api::ids::PlayerId;
use swarm_engine_plugin_sdk::components::{Drone, Position};

use crate::resource_ledger::{ResourceLedger, ResourceOperation};
use crate::resources::PlayerGlobalStorage;
use crate::systems::starting_resources_system::PlayerFirstSpawnTick;
use crate::world::WorldConfig;

#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct EmpireUpkeepDeficits(pub IndexMap<PlayerId, u32>);

pub fn memory_upkeep_system(
    config: Res<WorldConfig>,
    current_tick: Res<crate::resources::CurrentTick>,
    first_spawn: Res<PlayerFirstSpawnTick>,
    mut drones: Query<(&mut Drone, &Position)>,
    mut global_storage: ResMut<PlayerGlobalStorage>,
    mut ledger: ResMut<ResourceLedger>,
    mut deficits: Local<EmpireUpkeepDeficits>,
) {
    if !config.empire_upkeep.enabled {
        return;
    }

    let tick = current_tick.0;

    let mut rooms_by_player: IndexMap<PlayerId, IndexSet<u32>> = IndexMap::new();
    for (drone, position) in drones.iter() {
        rooms_by_player
            .entry(drone.owner)
            .or_default()
            .insert(position.room.0);
    }

    for (player_id, rooms) in rooms_by_player {
        let total_rooms = rooms.len() as u32;

        // Free upkeep exemption: first N controllers (rooms) are free for free_upkeep_ticks
        let effective_rooms = if let Some(spawn_tick) = first_spawn.0.get(&player_id) {
            let elapsed = tick.saturating_sub(*spawn_tick);
            if elapsed < config.starting_resources.free_upkeep_ticks {
                total_rooms.saturating_sub(config.starting_resources.free_upkeep_controllers)
            } else {
                total_rooms
            }
        } else {
            total_rooms
        };

        let cost = config.empire_upkeep.upkeep_cost(effective_rooms);
        if cost == 0 {
            deficits.0.shift_remove(&player_id);
            continue;
        }

        let storage = global_storage.0.entry(player_id).or_default();
        let available = storage
            .entry(config.empire_upkeep.resource.clone())
            .or_default();
        if *available >= cost {
            *available -= cost;
            ledger.record(
                tick,
                Some(player_id),
                None,
                &config.empire_upkeep.resource,
                i64::from(cost),
                ResourceOperation::UpkeepDeduction,
            );
            deficits.0.shift_remove(&player_id);
            continue;
        }

        let deducted = *available;
        *available = 0;
        if deducted > 0 {
            ledger.record(
                tick,
                Some(player_id),
                None,
                &config.empire_upkeep.resource,
                i64::from(deducted),
                ResourceOperation::UpkeepDeduction,
            );
        }
        let deficit_ticks = deficits
            .0
            .entry(player_id)
            .and_modify(|ticks| *ticks = ticks.saturating_add(1))
            .or_insert(1);

        if *deficit_ticks >= 10 {
            for (mut drone, _) in drones
                .iter_mut()
                .filter(|(drone, _)| drone.owner == player_id)
            {
                drone.age = drone.age.saturating_add(9);
            }
        } else if *deficit_ticks >= 3 {
            for (mut drone, _) in drones
                .iter_mut()
                .filter(|(drone, _)| drone.owner == player_id)
            {
                drone.fatigue = drone.fatigue.saturating_add(50);
            }
        }
    }
}
