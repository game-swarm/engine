use bevy::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use swarm_engine::components::{
    BodyPart, DamageType, DeathMark, Drone, Owner, PlayerId, Position, Structure,
};

#[derive(Component, Debug, Clone)]
pub struct Tower {
    pub range: u32,
    pub damage: u32,
    pub damage_type: DamageType,
}

#[derive(Resource, Debug, Clone)]
pub struct CombatConfig {
    pub pvp_enabled: bool,
    pub friendly_fire: bool,
    pub damage_multiplier_bp: u32,
}

impl Default for CombatConfig {
    fn default() -> Self {
        Self {
            pvp_enabled: true,
            friendly_fire: false,
            damage_multiplier_bp: 10_000,
        }
    }
}

#[derive(Resource, Debug, Clone, Default)]
pub struct ActionRegistry {
    pub handlers: BTreeSet<&'static str>,
}

#[derive(Resource, Debug, Clone, Default)]
pub struct CombatRegistry {
    pub damage_types: HashSet<DamageType>,
    pub body_parts: HashSet<BodyPart>,
}

#[derive(Resource, Debug, Clone, Default)]
pub struct PendingDamage {
    pub entries: Vec<PendingDamageEntry>,
}

#[derive(Debug, Clone)]
pub struct PendingDamageEntry {
    pub source: Option<Entity>,
    pub target: Entity,
    pub amount: u32,
    pub damage_type: DamageType,
}

#[derive(Resource, Debug, Clone, Default)]
pub struct PendingHeal {
    pub entries: Vec<PendingHealEntry>,
}

#[derive(Debug, Clone)]
pub struct PendingHealEntry {
    pub source: Option<Entity>,
    pub target: Entity,
    pub amount: u32,
}

#[derive(Resource, Debug, Clone, Default)]
pub struct DamageIntentBuffer {
    pub entries: Vec<DamageIntent>,
}

#[derive(Debug, Clone)]
pub struct DamageIntent {
    pub target: Entity,
    pub amount: u32,
    pub damage_type: DamageType,
}

#[derive(Resource, Debug, Clone, Default)]
pub struct HealIntentBuffer {
    pub entries: Vec<HealIntent>,
}

#[derive(Debug, Clone)]
pub struct HealIntent {
    pub target: Entity,
    pub amount: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SpecialAttackKind {
    Fortify = 1,
    Leech = 2,
    Fabricate = 3,
    Disrupt = 4,
    Debilitate = 5,
    Overload = 6,
    Drain = 7,
    Hack = 8,
}

#[derive(Resource, Debug, Clone, Default)]
pub struct StatusIntentBuffer {
    pub entries: Vec<StatusIntent>,
}

#[derive(Debug, Clone)]
pub struct StatusIntent {
    pub kind: SpecialAttackKind,
    pub source: Entity,
    pub target: Entity,
    pub amount: u32,
}

#[derive(Resource, Debug, Clone, Default)]
pub struct ResolvedStatusIntents {
    pub entries: Vec<ResolvedStatusIntent>,
}

#[derive(Debug, Clone)]
pub struct ResolvedStatusIntent {
    pub kind: SpecialAttackKind,
    pub target: Entity,
    pub amount: u32,
}

#[derive(Component, Debug, Clone, Default)]
pub struct StatusState {
    pub effects: BTreeMap<SpecialAttackKind, u32>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CombatCoreModPlugin;

impl Plugin for CombatCoreModPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CombatConfig>()
            .init_resource::<ActionRegistry>()
            .init_resource::<CombatRegistry>()
            .init_resource::<PendingDamage>()
            .init_resource::<PendingHeal>()
            .init_resource::<DamageIntentBuffer>()
            .init_resource::<HealIntentBuffer>()
            .init_resource::<StatusIntentBuffer>()
            .init_resource::<ResolvedStatusIntents>()
            .add_systems(Startup, register_combat_core)
            .add_systems(
                Update,
                (
                    tower_auto_attack_system,
                    attack_system,
                    ranged_attack_system,
                    heal_system,
                    special_attack_reducer,
                    damage_application_system,
                )
                    .chain(),
            );
    }
}

pub fn register_combat_core(
    mut actions: ResMut<ActionRegistry>,
    mut registry: ResMut<CombatRegistry>,
) {
    actions.handlers.extend(["Attack", "RangedAttack", "Heal"]);
    registry.damage_types.extend([
        DamageType::Kinetic,
        DamageType::Thermal,
        DamageType::EMP,
        DamageType::Sonic,
        DamageType::Corrosive,
        DamageType::Psionic,
    ]);
    registry.body_parts.extend([
        BodyPart::Attack,
        BodyPart::RangedAttack,
        BodyPart::Heal,
        BodyPart::Tough,
    ]);
}

pub fn tower_auto_attack_system(
    mut pending: ResMut<PendingDamage>,
    towers: Query<(Entity, &Position, Option<&Owner>, &Tower)>,
    targets: Query<(
        Entity,
        &Position,
        Option<&Owner>,
        Option<&Drone>,
        Option<&Structure>,
    )>,
    config: Res<CombatConfig>,
) {
    let mut queued = Vec::new();
    for (tower_entity, tower_pos, tower_owner, tower) in &towers {
        let mut candidates: Vec<_> = targets
            .iter()
            .filter_map(|(target, pos, owner, drone, structure)| {
                if target == tower_entity || drone.is_none() && structure.is_none() {
                    return None;
                }
                if !can_damage(tower_owner.map(|o| o.0), owner.map(|o| o.0), &config) {
                    return None;
                }
                let distance = range(tower_pos, pos)?;
                (distance <= tower.range).then_some((distance, target))
            })
            .collect();
        candidates.sort_by_key(|(distance, entity)| (*distance, entity.to_bits()));
        if let Some((_, target)) = candidates.first() {
            queued.push(PendingDamageEntry {
                source: Some(tower_entity),
                target: *target,
                amount: tower.damage,
                damage_type: tower.damage_type,
            });
        }
    }
    pending.entries.extend(queued);
}

pub fn attack_system(
    mut pending: ResMut<PendingDamage>,
    mut intents: ResMut<DamageIntentBuffer>,
    config: Res<CombatConfig>,
) {
    let entries = std::mem::take(&mut pending.entries);
    for entry in entries {
        if matches!(entry.damage_type, DamageType::Kinetic) {
            intents.entries.push(DamageIntent {
                target: entry.target,
                amount: scale(entry.amount, config.damage_multiplier_bp),
                damage_type: entry.damage_type,
            });
        } else {
            pending.entries.push(entry);
        }
    }
}

pub fn ranged_attack_system(
    mut pending: ResMut<PendingDamage>,
    mut intents: ResMut<DamageIntentBuffer>,
    config: Res<CombatConfig>,
) {
    let entries = std::mem::take(&mut pending.entries);
    for entry in entries {
        intents.entries.push(DamageIntent {
            target: entry.target,
            amount: scale(entry.amount, config.damage_multiplier_bp),
            damage_type: entry.damage_type,
        });
    }
}

pub fn heal_system(mut pending: ResMut<PendingHeal>, mut intents: ResMut<HealIntentBuffer>) {
    intents
        .entries
        .extend(pending.entries.drain(..).map(|entry| HealIntent {
            target: entry.target,
            amount: entry.amount,
        }));
}

pub fn special_attack_reducer(
    mut input: ResMut<StatusIntentBuffer>,
    mut output: ResMut<ResolvedStatusIntents>,
) {
    let mut raw = std::mem::take(&mut input.entries);
    raw.sort_by(|a, b| {
        b.kind
            .cmp(&a.kind)
            .then_with(|| a.source.to_bits().cmp(&b.source.to_bits()))
            .then_with(|| a.target.to_bits().cmp(&b.target.to_bits()))
    });
    let mut seen = BTreeSet::new();
    output.entries = raw
        .into_iter()
        .filter(|intent| seen.insert(intent.target))
        .map(|intent| ResolvedStatusIntent {
            kind: intent.kind,
            target: intent.target,
            amount: intent.amount,
        })
        .collect();
}

pub fn damage_application_system(
    mut commands: Commands,
    mut damage: ResMut<DamageIntentBuffer>,
    mut heal: ResMut<HealIntentBuffer>,
    mut drones: Query<&mut Drone>,
    mut structures: Query<&mut Structure, Without<Drone>>,
) {
    let mut damage_by_target: HashMap<Entity, u32> = HashMap::new();
    for entry in damage.entries.drain(..) {
        let reduced = match entry.damage_type {
            DamageType::Kinetic => entry.amount,
            _ => entry.amount,
        };
        *damage_by_target.entry(entry.target).or_default() = damage_by_target
            .get(&entry.target)
            .copied()
            .unwrap_or(0)
            .saturating_add(reduced);
    }

    let mut heal_by_target: HashMap<Entity, u32> = HashMap::new();
    for entry in heal.entries.drain(..) {
        *heal_by_target.entry(entry.target).or_default() = heal_by_target
            .get(&entry.target)
            .copied()
            .unwrap_or(0)
            .saturating_add(entry.amount);
    }

    let mut targets: Vec<_> = damage_by_target
        .keys()
        .chain(heal_by_target.keys())
        .copied()
        .collect();
    targets.sort_by_key(|entity| entity.to_bits());
    targets.dedup();
    for target in targets {
        let damage = damage_by_target.get(&target).copied().unwrap_or(0);
        let heal = heal_by_target.get(&target).copied().unwrap_or(0);
        if let Ok(mut drone) = drones.get_mut(target) {
            drone.hits = drone
                .hits
                .saturating_sub(damage)
                .saturating_add(heal)
                .min(drone.hits_max);
            if drone.hits == 0 {
                commands.entity(target).insert(DeathMark);
            }
        } else if let Ok(mut structure) = structures.get_mut(target) {
            structure.hits = structure
                .hits
                .saturating_sub(damage)
                .saturating_add(heal)
                .min(structure.hits_max);
            if structure.hits == 0 {
                commands.entity(target).insert(DeathMark);
            }
        }
    }
}

fn range(a: &Position, b: &Position) -> Option<u32> {
    (a.room == b.room).then(|| a.x.abs_diff(b.x).max(a.y.abs_diff(b.y)))
}

fn can_damage(source: Option<PlayerId>, target: Option<PlayerId>, config: &CombatConfig) -> bool {
    if source.is_some() && target.is_some() && source != target {
        return config.pvp_enabled;
    }
    source != target || config.friendly_fire
}

fn scale(amount: u32, multiplier_bp: u32) -> u32 {
    ((amount as u64 * multiplier_bp as u64) / 10_000).min(u32::MAX as u64) as u32
}
