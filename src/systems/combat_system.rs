use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use swarm_engine_api::ids::BodyPart;
use swarm_engine_plugin_sdk::buffers::{PendingDamage, PendingHeal};
use swarm_engine_plugin_sdk::components::{
    BodyPartRegistry, Drone, Owner, Position, SpawningGrace, Structure,
};

use crate::components::{Attributes, DamageTypeRegistry, EntityFlags, ResistanceRegistry};

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
        let value: toml::Value = toml::from_str(contents)
            .map_err(|error| format!("failed to parse world.toml: {error}"))?;
        let mut rules = Self::default();
        if let Some(combat) = value.get("combat").and_then(toml::Value::as_table)
            && let Some(raw) = combat
                .get("damage_multiplier")
                .or_else(|| combat.get("damage"))
        {
            rules.damage_multiplier = parse_fixed_multiplier(raw)?;
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
        toml::Value::Float(_) => {
            Err("combat.damage_multiplier must be an integer fixed-point multiplier".to_string())
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

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Projectile {
    pub source: u64,
    pub target: u64,
    pub damage_type: String,
    pub damage: u32,
    pub speed: u32,
    pub ticks_remaining: u32,
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

pub fn leech_self_heal(actual_damage: u32, heal_bps: u32) -> u32 {
    scale_fixed(actual_damage, heal_bps)
}

fn body_part_count(drone: &Drone, part: BodyPart) -> usize {
    drone
        .body
        .iter()
        .filter(|candidate| **candidate == part)
        .count()
}

/// Check if a drone has ALL specified body parts.
/// Returns `Ok(())` if all parts are present, `Err(body_part)` with the
/// first missing part otherwise. Used by disrupt and combat validation
/// to verify body part targeting (R23 D3/A).
pub fn body_part_match(drone: &Drone, required: &[BodyPart]) -> Result<(), BodyPart> {
    for part in required {
        if !drone.body.contains(part) {
            return Err(*part);
        }
    }
    Ok(())
}

fn same_room_range(source: &Position, target: &Position) -> Option<u32> {
    if source.room != target.room {
        return None;
    }
    Some(source.x.abs_diff(target.x).max(source.y.abs_diff(target.y)))
}

fn projectile_travel_ticks(distance: u32, speed: u32) -> u32 {
    let speed = speed.max(1);
    distance.max(1).div_ceil(speed)
}

pub fn attack_system(
    mut combat: ResMut<PendingCombat>,
    body_registry: Res<BodyPartRegistry>,
    rules: Res<CombatRules>,
    attackers: Query<(Entity, &Position, &Owner, &Drone), Without<SpawningGrace>>,
    targets: Query<(Entity, &Position, Option<&Owner>, &Drone), Without<SpawningGrace>>,
) {
    let mut intents = Vec::new();
    for (attacker_entity, attacker_position, attacker_owner, attacker) in &attackers {
        if attacker.hits == 0 {
            continue;
        }
        let attack_parts = body_part_count(attacker, BodyPart::Attack);
        if attack_parts == 0 {
            continue;
        }
        let (damage_type, damage) = body_part_damage(
            attack_parts,
            BodyPart::Attack,
            body_registry.as_ref(),
            *rules,
        );
        if damage == 0 {
            continue;
        }
        let mut candidates = targets
            .iter()
            .filter(|(target_entity, target_position, target_owner, target)| {
                *target_entity != attacker_entity
                    && target.hits > 0
                    && target_owner.map(|owner| owner.0) != Some(attacker_owner.0)
                    && same_room_range(attacker_position, target_position) == Some(1)
            })
            .map(|(target_entity, _, _, _)| target_entity)
            .collect::<Vec<_>>();
        candidates.sort_by_key(|e| e.to_bits());
        if let Some(target) = candidates.first() {
            intents.push((*target, damage_type.clone(), damage));
        }
    }
    intents.sort_by_key(|(target, _, _)| target.to_bits());
    for (target, damage_type, damage) in intents {
        combat.queue_typed_damage(target, damage_type, damage);
    }
}

pub fn ranged_attack_system(
    mut commands: Commands,
    body_registry: Res<BodyPartRegistry>,
    rules: Res<CombatRules>,
    attackers: Query<(Entity, &Position, &Owner, &Drone), Without<SpawningGrace>>,
    targets: Query<(Entity, &Position, Option<&Owner>, &Drone), Without<SpawningGrace>>,
) {
    let mut launches = Vec::new();
    for (attacker_entity, attacker_position, attacker_owner, attacker) in &attackers {
        if attacker.hits == 0 {
            continue;
        }
        let ranged_parts = body_part_count(attacker, BodyPart::RangedAttack);
        if ranged_parts == 0 {
            continue;
        }
        let (damage_type, damage) = body_part_damage(
            ranged_parts,
            BodyPart::RangedAttack,
            body_registry.as_ref(),
            *rules,
        );
        if damage == 0 {
            continue;
        }
        let range = ranged_parts as u32 * 3;
        let speed = ranged_parts as u32;
        let mut candidates = targets
            .iter()
            .filter_map(|(target_entity, target_position, target_owner, target)| {
                if target_entity == attacker_entity
                    || target.hits == 0
                    || target_owner.map(|owner| owner.0) == Some(attacker_owner.0)
                {
                    return None;
                }
                let distance = same_room_range(attacker_position, target_position)?;
                (distance > 1 && distance <= range).then_some((target_entity, distance))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(target, distance)| (*distance, target.to_bits()));
        if let Some((target, distance)) = candidates.first() {
            launches.push(Projectile {
                source: attacker_entity.to_bits(),
                target: target.to_bits(),
                damage_type: damage_type.clone(),
                damage,
                speed,
                ticks_remaining: projectile_travel_ticks(*distance, speed),
            });
        }
    }
    for projectile in launches {
        commands.spawn(projectile);
    }
}

pub fn projectile_system(
    mut commands: Commands,
    mut combat: ResMut<PendingCombat>,
    mut projectiles: Query<(Entity, &mut Projectile)>,
    targets: Query<&Drone, Without<SpawningGrace>>,
) {
    let mut impacts = Vec::new();
    for (projectile_entity, mut projectile) in &mut projectiles {
        projectile.ticks_remaining = projectile.ticks_remaining.saturating_sub(1);
        if projectile.ticks_remaining > 0 {
            continue;
        }
        let target = Entity::from_bits(projectile.target);
        if targets
            .get(target)
            .map(|drone| drone.hits > 0)
            .unwrap_or(false)
        {
            impacts.push((target, projectile.damage_type.clone(), projectile.damage));
        }
        commands.entity(projectile_entity).despawn();
    }
    impacts.sort_by_key(|(target, _, _)| target.to_bits());
    for (target, damage_type, damage) in impacts {
        combat.queue_typed_damage(target, damage_type, damage);
    }
}

pub fn heal_system(
    mut combat: ResMut<PendingCombat>,
    body_registry: Res<BodyPartRegistry>,
    healers: Query<(Entity, &Position, &Owner, &Drone), Without<SpawningGrace>>,
    targets: Query<(Entity, &Position, &Owner, &Drone), Without<SpawningGrace>>,
) {
    let mut intents = Vec::new();
    for (healer_entity, healer_position, healer_owner, healer) in &healers {
        if healer.hits == 0 {
            continue;
        }
        let heal_parts = body_part_count(healer, BodyPart::Heal);
        if heal_parts == 0 {
            continue;
        }
        let amount = heal_parts as u32 * body_registry.heal_amount(BodyPart::Heal);
        if amount == 0 {
            continue;
        }
        let mut candidates = targets
            .iter()
            .filter(|(target_entity, target_position, target_owner, target)| {
                *target_entity != healer_entity
                    && target.hits > 0
                    && target.hits < target.hits_max
                    && target_owner.0 == healer_owner.0
                    && same_room_range(healer_position, target_position)
                        .map(|distance| distance <= 3)
                        .unwrap_or(false)
            })
            .map(|(target_entity, _, _, _)| target_entity)
            .collect::<Vec<_>>();
        candidates.sort_by_key(|e| e.to_bits());
        if let Some(target) = candidates.first() {
            intents.push((*target, amount));
        }
    }
    intents.sort_by_key(|(target, _)| target.to_bits());
    for (target, amount) in intents {
        combat.queue_heal(target, amount);
    }
}

pub fn final_damage_multiplier_bps(
    body: Option<&[BodyPart]>,
    attrs: Option<&Attributes>,
    flags: Option<&EntityFlags>,
    damage_type: &str,
    _body_registry: &BodyPartRegistry,
    damage_registry: &DamageTypeRegistry,
    resistance_registry: &ResistanceRegistry,
) -> u32 {
    if flags
        .and_then(|flags| flags.0.get(&format!("immune_{damage_type}")))
        .copied()
        .unwrap_or(false)
    {
        return 0;
    }
    [
        damage_registry.component_multiplier_bps(damage_type, body),
        resistance_registry.component_multiplier_bps(damage_type, body),
        damage_registry.attribute_multiplier_bps(damage_type, attrs),
        resistance_registry.attribute_multiplier_bps(damage_type, attrs),
        fortify_multiplier_bps(attrs),
    ]
    .into_iter()
    .fold(DAMAGE_MULTIPLIER_SCALE, scale_fixed)
}

fn fortify_multiplier_bps(attrs: Option<&Attributes>) -> u32 {
    attrs
        .map(|attrs| {
            if attrs
                .0
                .iter()
                .any(|attr| attr == "Fortified" || attr.starts_with("Fortified:"))
            {
                5_000
            } else {
                DAMAGE_MULTIPLIER_SCALE
            }
        })
        .unwrap_or(DAMAGE_MULTIPLIER_SCALE)
}

type CombatDroneItem<'a> = (
    &'a Drone,
    Option<&'a Attributes>,
    Option<&'a EntityFlags>,
    Option<&'a SpawningGrace>,
);
type CombatStructureItem<'a> = (
    &'a Structure,
    Option<&'a Attributes>,
    Option<&'a EntityFlags>,
);
type CombatDroneQuery<'w, 's> = Query<'w, 's, CombatDroneItem<'static>>;
type CombatStructureQuery<'w, 's> = Query<'w, 's, CombatStructureItem<'static>, Without<Drone>>;

#[derive(SystemParam)]
pub struct CombatRegistries<'w> {
    body: Res<'w, BodyPartRegistry>,
    damage: Res<'w, DamageTypeRegistry>,
    resistance: Res<'w, ResistanceRegistry>,
}

#[derive(SystemParam)]
pub struct CombatOutputs<'w> {
    damage: ResMut<'w, PendingDamage>,
    heal: ResMut<'w, PendingHeal>,
}

pub fn combat_system(
    mut combat: ResMut<PendingCombat>,
    registries: CombatRegistries,
    mut outputs: CombatOutputs,
    drones: CombatDroneQuery,
    structures: CombatStructureQuery,
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
        if let Ok((drone, attrs, flags, grace)) = drones.get(*entity) {
            if grace.is_some() {
                continue;
            }
            let total = damages.iter().fold(0u32, |acc, (dt, amount)| {
                let multiplier = final_damage_multiplier_bps(
                    Some(&drone.body),
                    attrs,
                    flags,
                    dt,
                    &registries.body,
                    &registries.damage,
                    &registries.resistance,
                );
                acc.saturating_add(scale_fixed(*amount, multiplier))
            });
            if total > 0 {
                outputs.damage.push(*entity, total, "Kinetic");
            }
        } else if let Ok((_structure, attrs, flags)) = structures.get(*entity) {
            let total = damages.iter().fold(0u32, |acc, (dt, amount)| {
                let multiplier = final_damage_multiplier_bps(
                    None,
                    attrs,
                    flags,
                    dt,
                    &registries.body,
                    &registries.damage,
                    &registries.resistance,
                );
                acc.saturating_add(scale_fixed(*amount, multiplier))
            });
            if total > 0 {
                outputs.damage.push(*entity, total, "Kinetic");
            }
        }
    }

    // --- Heal phase (second, after damage) ---
    let mut heal_by_target: IndexMap<Entity, u32> = IndexMap::new();
    for (entity, amount) in combat.heal.drain(..) {
        *heal_by_target.entry(Entity::from_bits(entity)).or_default() += amount;
    }
    heal_by_target.sort_keys();

    for (entity, amount) in &heal_by_target {
        if drones.get(*entity).is_ok() {
            outputs.heal.push(*entity, *amount);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::DEFAULT_DRONE_LIFESPAN;
    use indexmap::IndexMap;

    #[test]
    fn parses_integer_damage_multiplier() {
        let fixed = CombatRules::from_toml_str("[combat]\ndamage_multiplier = 15000\n").unwrap();
        assert_eq!(fixed.damage_multiplier, 15_000);
        assert_eq!(fixed.scale_damage(30), 45);

        let half = CombatRules::from_toml_str("[combat]\ndamage = 5000\n").unwrap();
        assert_eq!(half.damage_multiplier, 5_000);
        assert_eq!(half.scale_damage(25), 12);
    }

    #[test]
    fn rejects_float_damage_multiplier() {
        let error = CombatRules::from_toml_str("[combat]\ndamage = 0.5\n").unwrap_err();

        assert!(error.contains("integer fixed-point multiplier"));
    }

    #[test]
    fn fixed_point_damage_worked_examples_use_flooring_integer_math() {
        assert_eq!(scale_fixed(15, 10_000), 15);
        assert_eq!(scale_fixed(15, 5_000), 7);
        assert_eq!(scale_fixed(25, 15_000), 37);
        assert_eq!(
            CombatRules {
                damage_multiplier: 15_000
            }
            .scale_damage(30),
            45
        );
    }

    #[test]
    fn leech_heal_worked_example_uses_actual_damage_basis_points() {
        let mitigated_damage = scale_fixed(15, 5_000);
        let scaled_damage = scale_fixed(mitigated_damage, 10_000);
        let target_virtual_remaining_hits = 6;
        let actual_damage = scaled_damage.min(target_virtual_remaining_hits);

        assert_eq!(mitigated_damage, 7);
        assert_eq!(actual_damage, 6);
        assert_eq!(leech_self_heal(actual_damage, 5_000), 3);
    }

    fn test_drone(body: Vec<BodyPart>) -> Drone {
        Drone {
            owner: 1,
            body,
            carry: IndexMap::new(),
            carry_capacity: 0,
            fatigue: 0,
            hits: 100,
            hits_max: 100,
            spawning: false,
            age: 0,
            last_action_tick: u64::MAX,
            lifespan: DEFAULT_DRONE_LIFESPAN,
        }
    }

    #[test]
    fn body_part_match_passes_for_existing_parts() {
        let drone = test_drone(vec![BodyPart::Attack, BodyPart::Move]);
        assert!(body_part_match(&drone, &[BodyPart::Attack]).is_ok());
        assert!(body_part_match(&drone, &[BodyPart::Attack, BodyPart::Move]).is_ok());
    }

    #[test]
    fn body_part_match_fails_for_missing_part() {
        let drone = test_drone(vec![BodyPart::Move]);
        let err = body_part_match(&drone, &[BodyPart::Attack]).unwrap_err();
        assert_eq!(err, BodyPart::Attack);
    }

    #[test]
    fn body_part_match_reports_first_missing_part() {
        let drone = test_drone(vec![BodyPart::Work]);
        let err = body_part_match(&drone, &[BodyPart::Attack, BodyPart::Heal, BodyPart::Work])
            .unwrap_err();
        // Attack is first in the list and missing, so it should be reported first
        assert_eq!(err, BodyPart::Attack);
    }

    #[test]
    fn body_part_match_empty_list_always_passes() {
        let drone = test_drone(vec![]);
        assert!(body_part_match(&drone, &[]).is_ok());
    }
}
