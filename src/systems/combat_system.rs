use bevy::prelude::*;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::components::{Drone, Structure};

/// Pending combat events for the current tick.
///
/// Damage is applied **before** healing within the same tick (Phase 2b spec).
/// Using `IndexMap` keyed on Entity ensures deterministic iteration order —
/// `Entity` is `(generation, index)` so sorted iteration is stable across
/// identical world states.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingCombat {
    pub damage: Vec<(u64, u32)>,
    pub heal: Vec<(u64, u32)>,
}

pub fn combat_system(
    mut combat: ResMut<PendingCombat>,
    mut drones: Query<&mut Drone>,
    mut structures: Query<&mut Structure>,
) {
    // --- Damage phase (first) ---
    // Accumulate total damage per target, then apply in deterministic order.
    let mut damage_by_target: IndexMap<Entity, u32> = IndexMap::new();
    for (entity, amount) in combat.damage.drain(..) {
        *damage_by_target
            .entry(Entity::from_bits(entity))
            .or_default() += amount;
    }
    // Sort by Entity bits for determinism.
    damage_by_target.sort_keys();

    for (entity, amount) in &damage_by_target {
        if let Ok(mut drone) = drones.get_mut(*entity) {
            drone.hits = drone.hits.saturating_sub(*amount);
        } else if let Ok(mut structure) = structures.get_mut(*entity) {
            structure.hits = structure.hits.saturating_sub(*amount);
        }
    }

    // --- Heal phase (second, after damage) ---
    let mut heal_by_target: IndexMap<Entity, u32> = IndexMap::new();
    for (entity, amount) in combat.heal.drain(..) {
        *heal_by_target.entry(Entity::from_bits(entity)).or_default() += amount;
    }
    heal_by_target.sort_keys();

    for (entity, amount) in &heal_by_target {
        if let Ok(mut drone) = drones.get_mut(*entity) {
            drone.hits = (drone.hits + amount).min(drone.hits_max);
        }
    }
}
