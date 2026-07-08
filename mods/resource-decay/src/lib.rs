use bevy::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use swarm_engine::components::{DeathMark, Resource, Structure, StructureType};

#[derive(Resource, Debug, Clone)]
pub struct ResourceDecayConfig {
    pub decay_rate_ppm: u32,
    pub per_resource_decay_rate_ppm: BTreeMap<String, u32>,
}

impl Default for ResourceDecayConfig {
    fn default() -> Self {
        Self {
            decay_rate_ppm: 1_000,
            per_resource_decay_rate_ppm: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceDecayModPlugin;

impl Plugin for ResourceDecayModPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ResourceDecayConfig>()
            .add_systems(Update, resource_decay_system);
    }
}

pub fn resource_decay_system(
    mut commands: Commands,
    config: Res<ResourceDecayConfig>,
    mut resources: Query<(Entity, &mut Resource, Option<&Structure>)>,
) {
    for (entity, mut resource, structure) in &mut resources {
        if structure.is_some_and(is_storage_structure) {
            continue;
        }
        let keys: Vec<_> = resource.amounts.keys().cloned().collect();
        for key in keys {
            let amount = resource.amounts.get(&key).copied().unwrap_or(0);
            let ppm = config
                .per_resource_decay_rate_ppm
                .get(&key)
                .copied()
                .unwrap_or(config.decay_rate_ppm)
                .min(1_000_000);
            let decayed = ((amount as u64 * (1_000_000 - ppm) as u64) / 1_000_000) as u32;
            if decayed == 0 {
                resource.amounts.shift_remove(&key);
            } else {
                resource.amounts.insert(key, decayed);
            }
        }
        let nonzero: BTreeSet<_> = resource
            .amounts
            .iter()
            .filter(|(_, amount)| **amount > 0)
            .map(|(name, _)| name.clone())
            .collect();
        if nonzero.is_empty() {
            commands.entity(entity).insert(DeathMark);
        }
    }
}

fn is_storage_structure(structure: &Structure) -> bool {
    matches!(
        structure.structure_type,
        StructureType::STORAGE | StructureType::EXTENSION | StructureType::TERMINAL
    )
}
