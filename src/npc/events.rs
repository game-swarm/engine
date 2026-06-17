use bevy::prelude::*;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::command::Tick;
use crate::components::{
    BodyPart, BodyPartRegistry, Drone, Owner, Position, RoomId, RoomTerrains, Source, SpawningGrace,
};
use crate::resources::CurrentTick;
use crate::world::WorldConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorldEventKind {
    SwarmInvasion,
    ResourceBoom,
    RuinAwakening,
    MerchantArrival,
}

impl WorldEventKind {
    fn seed_tag(self) -> u8 {
        match self {
            Self::SwarmInvasion => 1,
            Self::ResourceBoom => 2,
            Self::RuinAwakening => 3,
            Self::MerchantArrival => 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EventRuleConfig {
    pub probability: f64,
    pub interval: Tick,
    pub event_cooldown: Tick,
    pub duration: Tick,
}

impl Default for EventRuleConfig {
    fn default() -> Self {
        Self {
            probability: 0.0,
            interval: 1,
            event_cooldown: 0,
            duration: 0,
        }
    }
}

#[derive(Resource, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EventConfig {
    pub enabled: bool,
    pub swarm_invasion: EventRuleConfig,
    pub resource_boom: EventRuleConfig,
    pub ruin_awakening: EventRuleConfig,
    pub merchant_arrival: EventRuleConfig,
    pub resource_boom_multiplier: u32,
    pub swarm_invasion_count: u32,
    pub ruin_guardian_count: u32,
    pub ruin_creep_count: u32,
    pub merchant_count: u32,
}

impl Default for EventConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            swarm_invasion: EventRuleConfig {
                probability: 0.10,
                interval: 1_000,
                event_cooldown: 1_000,
                duration: 200,
            },
            resource_boom: EventRuleConfig {
                probability: 0.15,
                interval: 500,
                event_cooldown: 500,
                duration: 100,
            },
            ruin_awakening: EventRuleConfig {
                probability: 1.0,
                interval: 1,
                event_cooldown: 0,
                duration: 0,
            },
            merchant_arrival: EventRuleConfig {
                probability: 1.0,
                interval: 2_000,
                event_cooldown: 2_000,
                duration: 100,
            },
            resource_boom_multiplier: 2,
            swarm_invasion_count: 30,
            ruin_guardian_count: 3,
            ruin_creep_count: 10,
            merchant_count: 1,
        }
    }
}

impl EventConfig {
    fn rule(&self, kind: WorldEventKind) -> &EventRuleConfig {
        match kind {
            WorldEventKind::SwarmInvasion => &self.swarm_invasion,
            WorldEventKind::ResourceBoom => &self.resource_boom,
            WorldEventKind::RuinAwakening => &self.ruin_awakening,
            WorldEventKind::MerchantArrival => &self.merchant_arrival,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldEvent {
    pub id: u64,
    pub kind: WorldEventKind,
    pub start_tick: Tick,
    pub duration: Tick,
    pub origin: Position,
    pub applied: bool,
    pub spawned_entities: Vec<Entity>,
}

impl WorldEvent {
    pub fn expires_at(&self) -> Option<Tick> {
        (self.duration > 0).then(|| self.start_tick.saturating_add(self.duration))
    }
}

#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct EventState {
    pub active: Vec<WorldEvent>,
    pub last_triggered: IndexMap<WorldEventKind, Tick>,
    next_id: u64,
}

impl EventState {
    pub fn trigger(&mut self, kind: WorldEventKind, tick: Tick, duration: Tick, origin: Position) {
        self.next_id = self.next_id.saturating_add(1);
        self.last_triggered.insert(kind, tick);
        self.active.push(WorldEvent {
            id: self.next_id,
            kind,
            start_tick: tick,
            duration,
            origin,
            applied: false,
            spawned_entities: Vec::new(),
        });
    }

    fn can_trigger(&self, kind: WorldEventKind, tick: Tick, rule: &EventRuleConfig) -> bool {
        self.last_triggered
            .get(&kind)
            .is_none_or(|last| tick.saturating_sub(*last) >= rule.event_cooldown)
    }
}

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldNpc {
    pub kind: WorldNpcKind,
    pub event_id: u64,
    pub hits: u32,
    pub damage: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorldNpcKind {
    Swarmling,
    Creep,
    Guardian,
    Merchant,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AncientRuin {
    pub active: bool,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ResourceBoomed {
    pub original_produces: IndexMap<String, u32>,
}

pub fn deterministic_event_seed(world_seed: u64, tick: Tick, kind: WorldEventKind) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&world_seed.to_le_bytes());
    hasher.update(&tick.to_le_bytes());
    hasher.update(&[kind.seed_tag()]);
    *hasher.finalize().as_bytes()
}

pub fn deterministic_event_triggers(
    world_seed: u64,
    tick: Tick,
    kind: WorldEventKind,
    probability: f64,
) -> bool {
    if probability <= 0.0 || !probability.is_finite() {
        return false;
    }
    if probability >= 1.0 {
        return true;
    }
    let threshold = (probability.clamp(0.0, 1.0) * 256.0).floor() as u8;
    deterministic_event_seed(world_seed, tick, kind)[0] < threshold
}

pub fn world_event_system(
    world_config: Res<WorldConfig>,
    config: Res<EventConfig>,
    current_tick: Res<CurrentTick>,
    mut state: ResMut<EventState>,
    drones: Query<&Position, With<Drone>>,
    ruins: Query<(&Position, &AncientRuin)>,
    terrains: Res<RoomTerrains>,
) {
    if !config.enabled {
        return;
    }
    let tick = current_tick.0;
    let world_seed = world_config.world.world_seed;

    for kind in [
        WorldEventKind::SwarmInvasion,
        WorldEventKind::ResourceBoom,
        WorldEventKind::MerchantArrival,
    ] {
        let origin = choose_event_origin(kind, tick, world_seed, &drones, &terrains);
        maybe_trigger_random_event(kind, tick, world_seed, &config, &mut state, origin);
    }

    let rule = config.rule(WorldEventKind::RuinAwakening);
    if !state.can_trigger(WorldEventKind::RuinAwakening, tick, rule) {
        return;
    }
    for (ruin_position, ruin) in ruins.iter() {
        if ruin.active {
            continue;
        }
        let has_drone = drones
            .iter()
            .any(|drone_position| drone_position.room == ruin_position.room);
        if has_drone
            && deterministic_event_triggers(
                world_seed,
                tick,
                WorldEventKind::RuinAwakening,
                rule.probability,
            )
        {
            state.trigger(
                WorldEventKind::RuinAwakening,
                tick,
                rule.duration,
                *ruin_position,
            );
            break;
        }
    }
}

fn maybe_trigger_random_event(
    kind: WorldEventKind,
    tick: Tick,
    world_seed: u64,
    config: &EventConfig,
    state: &mut EventState,
    origin: Position,
) {
    let rule = config.rule(kind);
    if rule.interval > 0 && tick % rule.interval != 0 {
        return;
    }
    if !state.can_trigger(kind, tick, rule) {
        return;
    }
    if deterministic_event_triggers(world_seed, tick, kind, rule.probability) {
        state.trigger(kind, tick, rule.duration, origin);
    }
}

fn choose_event_origin(
    kind: WorldEventKind,
    tick: Tick,
    world_seed: u64,
    drones: &Query<&Position, With<Drone>>,
    terrains: &RoomTerrains,
) -> Position {
    match kind {
        WorldEventKind::SwarmInvasion => highest_density_position(drones),
        WorldEventKind::ResourceBoom => Position {
            room: RoomId(0),
            x: 25,
            y: 25,
        },
        WorldEventKind::RuinAwakening | WorldEventKind::MerchantArrival => {
            let mut rooms = terrains.0.keys().copied().collect::<Vec<_>>();
            rooms.sort_unstable();
            let room = if rooms.is_empty() {
                RoomId(0)
            } else {
                let seed = deterministic_event_seed(world_seed, tick, kind);
                rooms[usize::from(seed[1]) % rooms.len()]
            };
            Position { room, x: 25, y: 25 }
        }
    }
}

fn highest_density_position(drones: &Query<&Position, With<Drone>>) -> Position {
    let mut counts: IndexMap<RoomId, (u32, i32, i32)> = IndexMap::new();
    for position in drones.iter() {
        let entry = counts
            .entry(position.room)
            .or_insert((0, position.x, position.y));
        entry.0 += 1;
        entry.1 += position.x;
        entry.2 += position.y;
    }
    counts
        .into_iter()
        .max_by_key(|(room, (count, _, _))| (*count, std::cmp::Reverse(*room)))
        .map(|(room, (count, total_x, total_y))| Position {
            room,
            x: total_x / count as i32,
            y: total_y / count as i32,
        })
        .unwrap_or(Position {
            room: RoomId(0),
            x: 25,
            y: 25,
        })
}

pub fn event_effect_system(
    mut commands: Commands,
    config: Res<EventConfig>,
    current_tick: Res<CurrentTick>,
    body_registry: Res<BodyPartRegistry>,
    mut state: ResMut<EventState>,
    mut sources: Query<(Entity, &mut Source, Option<&ResourceBoomed>)>,
    mut ruins: Query<(&Position, &mut AncientRuin)>,
) {
    let tick = current_tick.0;
    let mut expired = Vec::new();
    for (index, event) in state.active.iter().enumerate() {
        if event
            .expires_at()
            .is_some_and(|expires_at| tick >= expires_at)
        {
            expired.push(index);
        }
    }

    for index in expired.into_iter().rev() {
        let event = state.active.remove(index);
        cleanup_event(&mut commands, &mut sources, event);
    }

    for event in &mut state.active {
        if event.applied {
            continue;
        }
        match event.kind {
            WorldEventKind::SwarmInvasion => spawn_npcs(
                &mut commands,
                event,
                WorldNpcKind::Swarmling,
                config.swarm_invasion_count,
                &body_registry,
            ),
            WorldEventKind::ResourceBoom => apply_resource_boom(
                &mut commands,
                &mut sources,
                config.resource_boom_multiplier.max(1),
            ),
            WorldEventKind::RuinAwakening => {
                for (position, mut ruin) in ruins.iter_mut() {
                    if *position == event.origin {
                        ruin.active = true;
                    }
                }
                spawn_npcs(
                    &mut commands,
                    event,
                    WorldNpcKind::Guardian,
                    config.ruin_guardian_count,
                    &body_registry,
                );
                spawn_npcs(
                    &mut commands,
                    event,
                    WorldNpcKind::Creep,
                    config.ruin_creep_count,
                    &body_registry,
                );
            }
            WorldEventKind::MerchantArrival => spawn_npcs(
                &mut commands,
                event,
                WorldNpcKind::Merchant,
                config.merchant_count,
                &body_registry,
            ),
        }
        event.applied = true;
    }
}

fn cleanup_event(
    commands: &mut Commands,
    sources: &mut Query<(Entity, &mut Source, Option<&ResourceBoomed>)>,
    event: WorldEvent,
) {
    for entity in event.spawned_entities {
        commands.entity(entity).despawn();
    }
    if event.kind == WorldEventKind::ResourceBoom {
        for (entity, mut source, boomed) in sources.iter_mut() {
            if let Some(boomed) = boomed {
                source.produces = boomed.original_produces.clone();
                commands.entity(entity).remove::<ResourceBoomed>();
            }
        }
    }
}

fn apply_resource_boom(
    commands: &mut Commands,
    sources: &mut Query<(Entity, &mut Source, Option<&ResourceBoomed>)>,
    multiplier: u32,
) {
    for (entity, mut source, boomed) in sources.iter_mut() {
        if boomed.is_some() {
            continue;
        }
        let original = source.produces.clone();
        for amount in source.produces.values_mut() {
            *amount = amount.saturating_mul(multiplier);
        }
        commands.entity(entity).insert(ResourceBoomed {
            original_produces: original,
        });
    }
}

fn spawn_npcs(
    commands: &mut Commands,
    event: &mut WorldEvent,
    kind: WorldNpcKind,
    count: u32,
    body_registry: &BodyPartRegistry,
) {
    for index in 0..count {
        let position = offset_position(event.origin, index);
        let npc = WorldNpc {
            kind,
            event_id: event.id,
            hits: npc_hits(kind),
            damage: npc_damage(kind),
        };
        let entity = if kind == WorldNpcKind::Merchant {
            commands.spawn((position, npc)).id()
        } else {
            commands
                .spawn((
                    position,
                    npc,
                    Owner(0),
                    Drone::new(0, npc_body(kind), body_registry),
                    SpawningGrace { remaining: 1 },
                ))
                .id()
        };
        event.spawned_entities.push(entity);
    }
}

fn npc_hits(kind: WorldNpcKind) -> u32 {
    match kind {
        WorldNpcKind::Swarmling => 20,
        WorldNpcKind::Creep => 50,
        WorldNpcKind::Guardian => 300,
        WorldNpcKind::Merchant => 200,
    }
}

fn npc_damage(kind: WorldNpcKind) -> u32 {
    match kind {
        WorldNpcKind::Swarmling => 5,
        WorldNpcKind::Creep => 10,
        WorldNpcKind::Guardian => 30,
        WorldNpcKind::Merchant => 0,
    }
}

fn npc_body(kind: WorldNpcKind) -> Vec<BodyPart> {
    match kind {
        WorldNpcKind::Swarmling | WorldNpcKind::Creep => vec![BodyPart::Move, BodyPart::Attack],
        WorldNpcKind::Guardian => vec![BodyPart::Move, BodyPart::Attack, BodyPart::Tough],
        WorldNpcKind::Merchant => Vec::new(),
    }
}

fn offset_position(origin: Position, index: u32) -> Position {
    let radius = 3_i32;
    Position {
        room: origin.room,
        x: (origin.x + (index as i32 % radius) - 1).clamp(0, 49),
        y: (origin.y + (index as i32 / radius) - 1).clamp(0, 49),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::WorldMode;
    use crate::world::{WorldConfig, WorldSectionConfig, create_world_with_mode_and_config};

    fn always(duration: Tick) -> EventRuleConfig {
        EventRuleConfig {
            probability: 1.0,
            interval: 1,
            event_cooldown: 0,
            duration,
        }
    }

    fn config_with_events(events: EventConfig) -> WorldConfig {
        WorldConfig {
            world: WorldSectionConfig {
                world_seed: 7,
                ..Default::default()
            },
            events,
            ..Default::default()
        }
    }

    #[test]
    fn trigger_probability_is_deterministic() {
        assert!(deterministic_event_triggers(
            1,
            1,
            WorldEventKind::SwarmInvasion,
            1.0
        ));
        assert!(!deterministic_event_triggers(
            1,
            1,
            WorldEventKind::SwarmInvasion,
            0.0
        ));
        assert_eq!(
            deterministic_event_seed(42, 1_000, WorldEventKind::ResourceBoom),
            deterministic_event_seed(42, 1_000, WorldEventKind::ResourceBoom)
        );
        assert_ne!(
            deterministic_event_seed(42, 1_000, WorldEventKind::ResourceBoom),
            deterministic_event_seed(43, 1_000, WorldEventKind::ResourceBoom)
        );
    }

    #[test]
    fn resource_boom_applies_multiplier_and_cleans_up() {
        let mut events = EventConfig {
            enabled: true,
            resource_boom: always(1),
            ..Default::default()
        };
        events.swarm_invasion.probability = 0.0;
        events.merchant_arrival.probability = 0.0;
        let mut world =
            create_world_with_mode_and_config(WorldMode::Default, config_with_events(events));

        world.run_tick();
        let produces_after_boom = world
            .app
            .world_mut()
            .query::<&Source>()
            .iter(world.app.world())
            .next()
            .unwrap()
            .produces
            .get("Energy")
            .copied();
        assert_eq!(produces_after_boom, Some(2));

        world.run_tick();
        let produces_after_cleanup = world
            .app
            .world_mut()
            .query::<&Source>()
            .iter(world.app.world())
            .next()
            .unwrap()
            .produces
            .get("Energy")
            .copied();
        assert_eq!(produces_after_cleanup, Some(1));
    }

    #[test]
    fn cooldown_blocks_repeat_triggers() {
        let mut events = EventConfig {
            enabled: true,
            swarm_invasion: EventRuleConfig {
                probability: 1.0,
                interval: 1,
                event_cooldown: 3,
                duration: 10,
            },
            ..Default::default()
        };
        events.resource_boom.probability = 0.0;
        events.merchant_arrival.probability = 0.0;
        let mut world =
            create_world_with_mode_and_config(WorldMode::Default, config_with_events(events));

        for _ in 0..3 {
            world.run_tick();
        }
        let state = world.app.world().resource::<EventState>();
        assert_eq!(
            state
                .active
                .iter()
                .filter(|event| event.kind == WorldEventKind::SwarmInvasion)
                .count(),
            1
        );
        assert_eq!(
            state.last_triggered.get(&WorldEventKind::SwarmInvasion),
            Some(&0)
        );

        world.run_tick();
        let state = world.app.world().resource::<EventState>();
        assert_eq!(
            state
                .active
                .iter()
                .filter(|event| event.kind == WorldEventKind::SwarmInvasion)
                .count(),
            2
        );
        assert_eq!(
            state.last_triggered.get(&WorldEventKind::SwarmInvasion),
            Some(&3)
        );
    }
}
