use bevy::prelude::*;
use std::collections::BTreeMap;
use swarm_engine::components::{Drone, PlayerId, Position, Structure};

#[derive(Component, Debug, Clone)]
pub struct ForwardDepot {
    pub owner: Option<PlayerId>,
    pub storage: BTreeMap<String, u32>,
    pub capacity: u32,
    pub repair_capacity: u32,
    pub repair_range: u32,
    pub repair_cost_energy: u32,
    pub repair_age_per_energy: u32,
}

impl Default for ForwardDepot {
    fn default() -> Self {
        Self {
            owner: None,
            storage: BTreeMap::new(),
            capacity: 10_000,
            repair_capacity: 5,
            repair_range: 1,
            repair_cost_energy: 1,
            repair_age_per_energy: 1,
        }
    }
}

#[derive(Resource, Debug, Clone)]
pub struct DepotStorageConfig {
    pub repair_range: u32,
    pub repair_capacity: u32,
    pub depot_hits: u32,
    pub depot_capacity: u32,
}

impl Default for DepotStorageConfig {
    fn default() -> Self {
        Self {
            repair_range: 1,
            repair_capacity: 5,
            depot_hits: 5_000,
            depot_capacity: 10_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DepotStorageModPlugin;

impl Plugin for DepotStorageModPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DepotStorageConfig>().add_systems(
            Update,
            (initialize_forward_depots, depot_repair_system).chain(),
        );
    }
}

pub fn initialize_forward_depots(
    config: Res<DepotStorageConfig>,
    mut depots: Query<(&mut ForwardDepot, Option<&mut Structure>), Added<ForwardDepot>>,
) {
    for (mut depot, structure) in &mut depots {
        depot.repair_range = config.repair_range;
        depot.repair_capacity = config.repair_capacity;
        depot.capacity = config.depot_capacity;
        if let Some(mut structure) = structure {
            structure.hits = structure.hits.max(config.depot_hits);
            structure.hits_max = structure.hits_max.max(config.depot_hits);
        }
    }
}

pub fn depot_repair_system(
    mut drones: Query<(Entity, &mut Drone, &Position)>,
    mut depots: Query<(Entity, &mut ForwardDepot, &Position)>,
) {
    let mut drone_rows: Vec<_> = drones
        .iter_mut()
        .filter(|(_, drone, _)| drone.age > 0)
        .map(|(entity, drone, position)| (entity, drone, *position))
        .collect();
    drone_rows.sort_by_key(|(entity, drone, position)| {
        (
            position.room,
            position.x,
            position.y,
            drone.owner,
            entity.to_bits(),
        )
    });

    let mut depot_rows: Vec<_> = depots
        .iter_mut()
        .map(|(entity, depot, position)| (entity, depot, *position))
        .collect();
    depot_rows.sort_by_key(|(entity, _, position)| {
        (position.room, position.x, position.y, entity.to_bits())
    });

    let mut used_by_depot: BTreeMap<Entity, u32> = BTreeMap::new();
    for (_, mut drone, drone_pos) in drone_rows {
        for (depot_entity, depot, depot_pos) in &mut depot_rows {
            if depot.owner.is_some() && depot.owner != Some(drone.owner) {
                continue;
            }
            if distance(&drone_pos, depot_pos).is_none_or(|value| value > depot.repair_range) {
                continue;
            }
            let used = used_by_depot.get(depot_entity).copied().unwrap_or(0);
            if used >= depot.repair_capacity {
                continue;
            }
            let available = depot.storage.get("Energy").copied().unwrap_or(0);
            if available < depot.repair_cost_energy {
                break;
            }
            let repaired = depot.repair_age_per_energy.min(drone.age);
            if repaired == 0 {
                break;
            }
            let repair_cost = depot.repair_cost_energy;
            depot
                .storage
                .insert("Energy".to_string(), available - repair_cost);
            drone.age = drone.age.saturating_sub(repaired);
            used_by_depot.insert(*depot_entity, used + 1);
            break;
        }
    }
}

fn distance(a: &Position, b: &Position) -> Option<u32> {
    (a.room == b.room).then(|| a.x.abs_diff(b.x).max(a.y.abs_diff(b.y)))
}
