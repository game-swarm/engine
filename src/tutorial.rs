use bevy::prelude::*;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use swarm_engine_api::ids::{BodyPart, PlayerId, RoomId};
use swarm_engine_plugin_sdk::components::{Drone, Owner, Position, Structure, StructureType};

use crate::command::{ObjectId, Tick};
use crate::components::Source;
use crate::onboarding::OnboardingEvent;
use crate::resources::ResourceCost;
use crate::systems::PendingSpawnQueue;

pub const TUTORIAL_PLAYER_ID: PlayerId = 1;
pub const TUTORIAL_ROOM_ID: RoomId = RoomId(0);
pub const TUTORIAL_SPAWN_POSITION: Position = Position {
    x: 24,
    y: 25,
    room: TUTORIAL_ROOM_ID,
};
pub const TUTORIAL_SOURCE_POSITION: Position = Position {
    x: 25,
    y: 25,
    room: TUTORIAL_ROOM_ID,
};
pub const TUTORIAL_TOWER_POSITION: Position = Position {
    x: 23,
    y: 25,
    room: TUTORIAL_ROOM_ID,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TutorialStep {
    SpawnDrone,
    Collect,
    BuildTower,
    Deploy,
}

impl TutorialStep {
    pub fn stable_id(self) -> &'static str {
        match self {
            Self::SpawnDrone => "spawn_drone",
            Self::Collect => "collect",
            Self::BuildTower => "build_tower",
            Self::Deploy => "deploy",
        }
    }
}

#[derive(Message, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TutorialStepEvent {
    pub step: TutorialStep,
    pub tick: Tick,
    pub message: String,
}

#[derive(Resource, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TutorialConfig {
    pub enabled: bool,
    pub bot_enabled: bool,
    pub player_id: PlayerId,
    pub room_id: RoomId,
    pub tick_interval_ms: u64,
    pub predeployed_module: TutorialModule,
}

impl TutorialConfig {
    pub fn disabled(tick_interval_ms: u64) -> Self {
        Self {
            enabled: false,
            bot_enabled: false,
            player_id: TUTORIAL_PLAYER_ID,
            room_id: TUTORIAL_ROOM_ID,
            tick_interval_ms,
            predeployed_module: TutorialModule::basic_harvester(),
        }
    }

    pub fn enabled(tick_interval_ms: u64) -> Self {
        Self {
            enabled: true,
            bot_enabled: true,
            player_id: TUTORIAL_PLAYER_ID,
            room_id: TUTORIAL_ROOM_ID,
            tick_interval_ms,
            predeployed_module: TutorialModule::basic_harvester(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TutorialModule {
    pub module_id: String,
    pub language: String,
    pub version_tag: String,
    pub wasm_hash: String,
    pub source: String,
}

impl TutorialModule {
    pub fn basic_harvester() -> Self {
        Self {
            module_id: "tutorial-basic-harvester".to_string(),
            language: "wasm".to_string(),
            version_tag: "tutorial-v1".to_string(),
            wasm_hash: "predeployed:tutorial-basic-harvester".to_string(),
            source: "spawn_drone -> collect -> build_tower -> deploy".to_string(),
        }
    }
}

#[derive(Resource, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TutorialState {
    pub module_deployed: bool,
    pub current_step: TutorialStep,
    pub completed_steps: Vec<TutorialStep>,
    pub spawn_id: Option<ObjectId>,
    pub drone_id: Option<ObjectId>,
    pub source_id: Option<ObjectId>,
    pub tower_id: Option<ObjectId>,
    pub last_tick: Tick,
}

impl TutorialState {
    pub fn new(spawn_id: ObjectId, source_id: ObjectId) -> Self {
        Self {
            module_deployed: true,
            current_step: TutorialStep::SpawnDrone,
            completed_steps: Vec::new(),
            spawn_id: Some(spawn_id),
            drone_id: None,
            source_id: Some(source_id),
            tower_id: None,
            last_tick: 0,
        }
    }

    fn mark_completed(&mut self, step: TutorialStep, next: TutorialStep) {
        if !self.completed_steps.contains(&step) {
            self.completed_steps.push(step);
        }
        self.current_step = next;
    }

    pub fn has_completed(&self, step: TutorialStep) -> bool {
        self.completed_steps.contains(&step)
    }
}

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TutorialBot;

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TutorialSpawn;

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TutorialSource;

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TutorialTower;

pub fn tutorial_spawn_energy() -> Option<u32> {
    Some(1_000)
}

pub fn free_tutorial_tower_cost() -> ResourceCost {
    ResourceCost::new()
}

pub fn tutorial_bot_system(
    mut commands: Commands,
    config: Res<TutorialConfig>,
    mut state: ResMut<TutorialState>,
    mut queue: ResMut<PendingSpawnQueue>,
    mut events: ParamSet<(
        MessageWriter<OnboardingEvent>,
        MessageWriter<TutorialStepEvent>,
    )>,
    mut sources: Query<(Entity, &Position, &mut Source), With<TutorialSource>>,
    drones: Query<(Entity, &Position, &Owner), With<Drone>>,
) {
    if !config.enabled || !config.bot_enabled || !state.module_deployed {
        return;
    }

    state.last_tick = state.last_tick.saturating_add(1);
    match state.current_step {
        TutorialStep::SpawnDrone => {
            queue.0.push(crate::systems::PendingSpawn {
                owner: config.player_id,
                body: vec![BodyPart::Move, BodyPart::Work, BodyPart::Carry],
                position: Position {
                    x: TUTORIAL_SPAWN_POSITION.x + 1,
                    y: TUTORIAL_SPAWN_POSITION.y,
                    room: config.room_id,
                },
            });
            state.mark_completed(TutorialStep::SpawnDrone, TutorialStep::Collect);
            events.p1().write(TutorialStepEvent {
                step: TutorialStep::SpawnDrone,
                tick: state.last_tick,
                message: "教程 bot 已从预部署 WASM 模块创建第一架 drone。".to_string(),
            });
        }
        TutorialStep::Collect => {
            let Some((drone_entity, _, _)) = drones.iter().find(|(_, position, owner)| {
                position.room == config.room_id && owner.0 == config.player_id
            }) else {
                return;
            };
            let Some((source_entity, _, mut source)) = sources
                .iter_mut()
                .find(|(_, position, _)| position.room == config.room_id)
            else {
                return;
            };
            source.capacity = source.capacity.saturating_sub(20);
            state.drone_id = Some(drone_entity.to_bits());
            state.source_id = Some(source_entity.to_bits());
            state.mark_completed(TutorialStep::Collect, TutorialStep::BuildTower);
            events.p0().write(OnboardingEvent::ResourceCollected);
            events.p1().write(TutorialStepEvent {
                step: TutorialStep::Collect,
                tick: state.last_tick,
                message: "教程 bot 已让 drone 采集附近 source。".to_string(),
            });
        }
        TutorialStep::BuildTower => {
            commands.spawn((
                TUTORIAL_TOWER_POSITION,
                TutorialTower,
                Structure {
                    structure_type: StructureType::TOWER,
                    owner: Some(config.player_id),
                    hits: 1,
                    hits_max: 5_000,
                    energy: Some(0),
                    energy_capacity: Some(1_000),
                    cooldown: 0,
                },
            ));
            state.mark_completed(TutorialStep::BuildTower, TutorialStep::Deploy);
            events.p0().write(OnboardingEvent::StructureBuilt);
            events.p1().write(TutorialStepEvent {
                step: TutorialStep::BuildTower,
                tick: state.last_tick,
                message: "教程 bot 已在提示坐标建造 Tower。".to_string(),
            });
        }
        TutorialStep::Deploy => {
            state.mark_completed(TutorialStep::Deploy, TutorialStep::Deploy);
            events.p1().write(TutorialStepEvent {
                step: TutorialStep::Deploy,
                tick: state.last_tick,
                message: "教程完成：预部署 WASM 模块已验证，玩家可部署到 World 或 Arena。"
                    .to_string(),
            });
        }
    }
}

pub fn sync_tutorial_state_system(
    mut commands: Commands,
    config: Res<TutorialConfig>,
    mut state: ResMut<TutorialState>,
    drones: Query<(Entity, &Position, &Owner), With<Drone>>,
    towers: Query<(Entity, &Position), With<Structure>>,
) {
    if !config.enabled {
        return;
    }

    if state.drone_id.is_none() {
        for (entity, position, owner) in &drones {
            if position.room == config.room_id && owner.0 == config.player_id {
                state.drone_id = Some(entity.to_bits());
                break;
            }
        }
    }

    if state.tower_id.is_none() {
        for (entity, position) in &towers {
            if *position == TUTORIAL_TOWER_POSITION {
                state.tower_id = Some(entity.to_bits());
                commands.entity(entity).insert(TutorialTower);
                break;
            }
        }
    }
}

pub fn tutorial_source_amounts() -> IndexMap<String, u32> {
    IndexMap::from([("Energy".to_string(), 20)])
}
