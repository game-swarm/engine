use bevy::prelude::*;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::components::{
    Attributes, BodyPart, BodyPartRegistry, DamageTypeRegistry, Drone, EntityFlags,
    ResistanceRegistry, Structure,
};

pub const DEFAULT_ATTACK_DAMAGE: u32 = 30;
pub const DEFAULT_RANGED_ATTACK_DAMAGE: u32 = 25;
pub const DEFAULT_HEAL_AMOUNT: u32 = 12;
pub const DEFAULT_DAMAGE_MULTIPLIER: u32 = 10_000;
pub const DAMAGE_MULTIPLIER_SCALE: u32 = 10_000;

#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CombatRules {
    /// Fixed-point damage multiplier scaled by 10_000.
    /// 10_000 = 1.0, 15_000 = 1.5, 5_000 = 0.5.
    pub damage_multiplier: u32,
}

impl Default for CombatRules {
    fn default() -> Self {
        Self {
            damage_multiplier: DEFAULT_DAMAGE_MULTIPLIER,
        }
    }
}

impl CombatRules {
    pub fn from_toml_str(contents: &str) -> Result<Self, String> {
        let value = contents
            .parse::<toml::Value>()
            .map_err(|error| format!("failed to parse world.toml: {error}"))?;
        let mut rules = Self::default();
        if let Some(combat) = value.get("combat").and_then(toml::Value::as_table) {
            if let Some(raw) = combat
                .get("damage_multiplier")
                .or_else(|| combat.get("damage"))
            {
                rules.damage_multiplier = parse_fixed_multiplier(raw)?;
            }
        }
        Ok(rules)
    }

    pub fn scale_damage(self, amount: u32) -> u32 {
        scale_fixed(amount, self.damage_multiplier)
    }
}

fn parse_fixed_multiplier(value: &toml::Value) -> Result<u32, String> {
    match value {
        toml::Value::Integer(integer) => {
            if *integer < 0 {
                return Err("combat.damage_multiplier must be non-negative".to_string());
            }
            u32::try_from(*integer).map_err(|_| "combat.damage_multiplier too large".to_string())
        }
        toml::Value::Float(float) => {
            if !float.is_finite() || *float < 0.0 {
                return Err("combat.damage must be a non-negative finite number".to_string());
            }
            let scaled = (*float * DAMAGE_MULTIPLIER_SCALE as f64).round();
            if scaled > u32::MAX as f64 {
                return Err("combat.damage is too large".to_string());
            }
            Ok(scaled as u32)
        }
        _ => Err("combat.damage_multiplier must be an integer fixed-point multiplier".to_string()),
    }
}

fn scale_fixed(amount: u32, multiplier: u32) -> u32 {
    let scaled = (amount as u64 * multiplier as u64) / DAMAGE_MULTIPLIER_SCALE as u64;
    scaled.min(u32::MAX as u64) as u32
}

/// Pending combat events for the current tick.
///
/// Damage is applied **before** healing within the same tick (Phase 2b spec).
/// Using `IndexMap` keyed on Entity ensures deterministic iteration order —
/// `Entity` is `(generation, index)` so sorted iteration is stable across
/// identical world states.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingCombat {
    pub damage: Vec<(u64, u32)>,
    pub typed_damage: Vec<(u64, String, u32)>,
    pub heal: Vec<(u64, u32)>,
}

impl PendingCombat {
    pub fn queue_damage(&mut self, target: Entity, amount: u32) {
        self.damage.push((target.to_bits(), amount));
    }

    pub fn queue_typed_damage(
        &mut self,
        target: Entity,
        damage_type: impl Into<String>,
        amount: u32,
    ) {
        self.typed_damage
            .push((target.to_bits(), damage_type.into(), amount));
    }

    pub fn queue_heal(&mut self, target: Entity, amount: u32) {
        self.heal.push((target.to_bits(), amount));
    }
}

pub fn body_part_damage(
    parts: usize,
    part: BodyPart,
    registry: &BodyPartRegistry,
    rules: CombatRules,
) -> (String, u32) {
    (
        registry.damage_type(part),
        rules.scale_damage(parts as u32 * registry.base_damage(part)),
    )
}

pub fn melee_attack_damage(parts: usize, rules: CombatRules) -> u32 {
    rules.scale_damage(parts as u32 * DEFAULT_ATTACK_DAMAGE)
}

pub fn ranged_attack_damage(parts: usize, rules: CombatRules) -> u32 {
    rules.scale_damage(parts as u32 * DEFAULT_RANGED_ATTACK_DAMAGE)
}

pub fn heal_amount(parts: usize) -> u32 {
    parts as u32 * DEFAULT_HEAL_AMOUNT
}

pub fn final_damage_multiplier(
    body: Option<&[BodyPart]>,
    attrs: Option<&Attributes>,
    flags: Option<&EntityFlags>,
    damage_type: &str,
    _body_registry: &BodyPartRegistry,
    damage_registry: &DamageTypeRegistry,
    resistance_registry: &ResistanceRegistry,
) -> f64 {
    if flags
        .and_then(|flags| flags.0.get(&format!("immune_{damage_type}")))
        .copied()
        .unwrap_or(false)
    {
        return 0.0;
    }
    let component_mult = damage_registry.component_multiplier(damage_type, body)
        * resistance_registry.component_multiplier(damage_type, body);
    let attribute_mult = damage_registry.attribute_multiplier(damage_type, attrs)
        * resistance_registry.attribute_multiplier(damage_type, attrs)
        * fortify_multiplier(attrs);
    component_mult * attribute_mult
}

fn fortify_multiplier(attrs: Option<&Attributes>) -> f64 {
    attrs
        .map(|attrs| {
            if attrs
                .0
                .iter()
                .any(|attr| attr == "Fortified" || attr.starts_with("Fortified:"))
            {
                0.5
            } else {
                1.0
            }
        })
        .unwrap_or(1.0)
}

pub fn combat_system(
    mut combat: ResMut<PendingCombat>,
    body_registry: Res<BodyPartRegistry>,
    damage_registry: Res<DamageTypeRegistry>,
    resistance_registry: Res<ResistanceRegistry>,
    mut drones: Query<(&mut Drone, Option<&Attributes>, Option<&EntityFlags>)>,
    mut structures: Query<
        (&mut Structure, Option<&Attributes>, Option<&EntityFlags>),
        Without<Drone>,
    >,
) {
    // --- Damage phase (first) ---
    // Accumulate total damage per target, then apply in deterministic order.
    let mut damage_by_target: IndexMap<Entity, Vec<(String, u32)>> = IndexMap::new();
    for (entity, amount) in combat.damage.drain(..) {
        damage_by_target
            .entry(Entity::from_bits(entity))
            .or_default()
            .push(("Kinetic".to_string(), amount));
    }
    for (entity, damage_type, amount) in combat.typed_damage.drain(..) {
        damage_by_target
            .entry(Entity::from_bits(entity))
            .or_default()
            .push((damage_type, amount));
    }
    // Sort by Entity bits for determinism.
    damage_by_target.sort_keys();

    for (entity, damages) in &damage_by_target {
        if let Ok((mut drone, attrs, flags)) = drones.get_mut(*entity) {
            let total = damages.iter().fold(0u32, |acc, (dt, amount)| {
                let multiplier = final_damage_multiplier(
                    Some(&drone.body),
                    attrs,
                    flags,
                    dt,
                    &body_registry,
                    &damage_registry,
                    &resistance_registry,
                );
                acc.saturating_add(((*amount as f64) * multiplier).floor() as u32)
            });
            drone.hits = drone.hits.saturating_sub(total);
        } else if let Ok((mut structure, attrs, flags)) = structures.get_mut(*entity) {
            let total = damages.iter().fold(0u32, |acc, (dt, amount)| {
                let multiplier = final_damage_multiplier(
                    None,
                    attrs,
                    flags,
                    dt,
                    &body_registry,
                    &damage_registry,
                    &resistance_registry,
                );
                acc.saturating_add(((*amount as f64) * multiplier).floor() as u32)
            });
            structure.hits = structure.hits.saturating_sub(total);
        }
    }

    // --- Heal phase (second, after damage) ---
    let mut heal_by_target: IndexMap<Entity, u32> = IndexMap::new();
    for (entity, amount) in combat.heal.drain(..) {
        *heal_by_target.entry(Entity::from_bits(entity)).or_default() += amount;
    }
    heal_by_target.sort_keys();

    for (entity, amount) in &heal_by_target {
        if let Ok((mut drone, _, _)) = drones.get_mut(*entity) {
            drone.hits = (drone.hits + amount).min(drone.hits_max);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_integer_and_float_damage_multiplier() {
        let fixed = CombatRules::from_toml_str("[combat]\ndamage_multiplier = 15000\n").unwrap();
        assert_eq!(fixed.damage_multiplier, 15_000);
        assert_eq!(fixed.scale_damage(30), 45);

        let compat_float = CombatRules::from_toml_str("[combat]\ndamage = 0.5\n").unwrap();
        assert_eq!(compat_float.damage_multiplier, 5_000);
        assert_eq!(compat_float.scale_damage(25), 12);
    }
}
