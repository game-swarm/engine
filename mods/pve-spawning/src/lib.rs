use bevy::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use swarm_engine::components::{
    BodyPart, BodyPartRegistry, Drone, PlayerId, Position, Resource, RoomId,
};

pub const NPC_OWNER: PlayerId = 0;

#[derive(Component, Debug, Clone)]
pub struct RoomConfig {
    pub room: RoomId,
    pub spawn_origin: Position,
    pub pve_enabled: bool,
}

#[derive(Resource, Debug, Clone)]
pub struct WorldConfig {
    pub pve_only: bool,
    pub pvp_enabled: bool,
}

impl Default for WorldConfig {
    fn default() -> Self {
        Self {
            pve_only: true,
            pvp_enabled: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NpcKind {
    Neutral,
    ResourceWave,
    Elite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NpcState {
    Guard,
    Patrol,
    Chase,
    Return,
}

#[derive(Component, Debug, Clone)]
pub struct NpcAI {
    pub kind: NpcKind,
    pub state: NpcState,
    pub home: Position,
    pub target: Option<Entity>,
    pub drop_table: BTreeMap<String, u32>,
}

#[derive(Resource, Debug, Clone)]
pub struct PveSpawningConfig {
    pub spawn_interval: u32,
    pub max_npcs_per_room: u32,
    pub npc_drone_body: Vec<BodyPart>,
    pub npc_drop_table: BTreeMap<String, u32>,
}

impl Default for PveSpawningConfig {
    fn default() -> Self {
        Self {
            spawn_interval: 300,
            max_npcs_per_room: 50,
            npc_drone_body: vec![BodyPart::Attack, BodyPart::Move, BodyPart::Move],
            npc_drop_table: BTreeMap::from([("Energy".to_string(), 50)]),
        }
    }
}

#[derive(Resource, Debug, Clone, Default)]
pub struct PveSpawnClock {
    pub tick: u32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PveSpawningModPlugin;

impl Plugin for PveSpawningModPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WorldConfig>()
            .init_resource::<PveSpawningConfig>()
            .init_resource::<PveSpawnClock>()
            .add_systems(
                Update,
                (pve_spawn_system, npc_ai_system, pve_drop_system).chain(),
            );
    }
}

pub fn pve_spawn_system(
    mut commands: Commands,
    mut clock: ResMut<PveSpawnClock>,
    config: Res<PveSpawningConfig>,
    world: Res<WorldConfig>,
    rooms: Query<&RoomConfig>,
    npcs: Query<(&NpcAI, &Position)>,
) {
    clock.tick = clock.tick.saturating_add(1);
    if clock.tick % config.spawn_interval != 0 || !world.pve_only && world.pvp_enabled {
        return;
    }
    let mut counts: BTreeMap<RoomId, u32> = BTreeMap::new();
    for (_, pos) in &npcs {
        *counts.entry(pos.room).or_default() += 1;
    }
    for room in &rooms {
        if !room.pve_enabled
            || counts.get(&room.room).copied().unwrap_or(0) >= config.max_npcs_per_room
        {
            continue;
        }
        let kind = if clock.tick % (config.spawn_interval * 10) == 0 {
            NpcKind::Elite
        } else if clock.tick % (config.spawn_interval * 3) == 0 {
            NpcKind::ResourceWave
        } else {
            NpcKind::Neutral
        };
        commands.spawn((
            npc_drone(
                &config.npc_drone_body,
                if kind == NpcKind::Elite { 500 } else { 100 },
            ),
            room.spawn_origin,
            NpcAI {
                kind,
                state: NpcState::Guard,
                home: room.spawn_origin,
                target: None,
                drop_table: config.npc_drop_table.clone(),
            },
        ));
    }
}

pub fn npc_ai_system(
    mut npcs: Query<(&mut NpcAI, &mut Position)>,
    drones: Query<(Entity, &Drone, &Position)>,
) {
    let player_positions: Vec<_> = drones
        .iter()
        .filter(|(_, drone, _)| drone.owner != NPC_OWNER)
        .map(|(entity, _, position)| (entity, *position))
        .collect();
    for (mut ai, mut pos) in &mut npcs {
        let target = player_positions
            .iter()
            .filter(|(_, target_pos)| target_pos.room == pos.room)
            .min_by_key(|(_, target_pos)| {
                pos.x.abs_diff(target_pos.x) + pos.y.abs_diff(target_pos.y)
            })
            .copied();
        match (ai.state, target) {
            (_, Some((entity, target_pos))) => {
                ai.state = NpcState::Chase;
                ai.target = Some(entity);
                step_toward(&mut pos, target_pos);
            }
            (NpcState::Chase, None) => ai.state = NpcState::Return,
            (NpcState::Return, None) if pos.x != ai.home.x || pos.y != ai.home.y => {
                step_toward(&mut pos, ai.home)
            }
            (NpcState::Return, None) => ai.state = NpcState::Guard,
            (NpcState::Guard, None) => ai.state = NpcState::Patrol,
            (NpcState::Patrol, None) => pos.x = pos.x.saturating_add(1),
        }
    }
}

pub fn pve_drop_system(
    mut commands: Commands,
    dead_npcs: Query<(Entity, &NpcAI, &Position, &Drone)>,
) {
    let mut spawned = BTreeSet::new();
    for (entity, ai, position, drone) in &dead_npcs {
        if drone.owner == NPC_OWNER && drone.hits == 0 && spawned.insert(entity) {
            commands.spawn((
                Resource {
                    amounts: ai.drop_table.clone().into_iter().collect(),
                },
                *position,
            ));
            commands.entity(entity).despawn();
        }
    }
}

fn npc_drone(body: &[BodyPart], hits: u32) -> Drone {
    let registry = BodyPartRegistry::default();
    let mut drone = Drone::new(NPC_OWNER, body.to_vec(), &registry);
    drone.hits = hits;
    drone.hits_max = hits;
    drone
}

fn step_toward(pos: &mut Position, target: Position) {
    pos.x += (target.x - pos.x).signum();
    pos.y += (target.y - pos.y).signum();
}
