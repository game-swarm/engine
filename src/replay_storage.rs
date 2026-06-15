use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::command::Tick;
use crate::components::PlayerId;
use crate::rule_module::RhaiRuleModules;
use crate::tick::{ReplayError, TickTrace, WorldSnapshot, replay_tick};
use crate::world::{SwarmWorld, WorldConfig};

pub const DEFAULT_KEYFRAME_INTERVAL: Tick = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayStorageConfig {
    pub keyframe_interval: Tick,
}

impl Default for ReplayStorageConfig {
    fn default() -> Self {
        Self {
            keyframe_interval: DEFAULT_KEYFRAME_INTERVAL,
        }
    }
}

impl ReplayStorageConfig {
    pub fn is_keyframe_tick(&self, tick: Tick) -> bool {
        let interval = self.keyframe_interval.max(1);
        tick == 0 || tick % interval == 0
    }

    pub fn nearest_keyframe_tick(&self, tick: Tick) -> Tick {
        let interval = self.keyframe_interval.max(1);
        tick - (tick % interval)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModVersionHash {
    pub module_id: String,
    pub version_hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModsLock {
    pub modules: Vec<ModVersionHash>,
}

impl ModsLock {
    pub fn from_world(world: &bevy::prelude::World) -> Self {
        let Some(modules) = world.get_resource::<RhaiRuleModules>() else {
            return Self::default();
        };

        let entries = modules
            .module_version_hashes()
            .into_iter()
            .map(|(module_id, version_hash)| ModVersionHash {
                module_id,
                version_hash,
            })
            .collect();
        Self { modules: entries }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldConfigSnapshot {
    pub config: WorldConfig,
    pub fingerprint: [u8; 32],
}

impl WorldConfigSnapshot {
    pub fn new(config: WorldConfig) -> Self {
        let bytes = serde_json::to_vec(&config).expect("world config must serialize");
        Self {
            config,
            fingerprint: *blake3::hash(&bytes).as_bytes(),
        }
    }

    pub fn from_world(world: &bevy::prelude::World) -> Self {
        Self::new(world.resource::<WorldConfig>().clone())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayEnvironmentSnapshot {
    pub mods_lock: ModsLock,
    pub world_config: WorldConfigSnapshot,
}

impl ReplayEnvironmentSnapshot {
    pub fn from_world(world: &bevy::prelude::World) -> Self {
        Self {
            mods_lock: ModsLock::from_world(world),
            world_config: WorldConfigSnapshot::from_world(world),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityDelta {
    pub entity: u64,
    pub before: Option<crate::tick::EntitySnapshot>,
    pub after: Option<crate::tick::EntitySnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldDelta {
    pub from_tick: Tick,
    pub to_tick: Tick,
    pub entity_changes: Vec<EntityDelta>,
    pub commands: Vec<crate::command::RawCommand>,
}

impl WorldDelta {
    pub fn between(
        before: &WorldSnapshot,
        after: &WorldSnapshot,
        from_tick: Tick,
        to_tick: Tick,
        commands: Vec<crate::command::RawCommand>,
    ) -> Self {
        let mut entity_ids = BTreeSet::new();
        entity_ids.extend(before.entities().keys().copied());
        entity_ids.extend(after.entities().keys().copied());

        let entity_changes = entity_ids
            .into_iter()
            .filter_map(|entity| {
                let before_snapshot = before.entities().get(&entity).cloned();
                let after_snapshot = after.entities().get(&entity).cloned();
                (before_snapshot != after_snapshot).then_some(EntityDelta {
                    entity: entity.0,
                    before: before_snapshot,
                    after: after_snapshot,
                })
            })
            .collect();

        Self {
            from_tick,
            to_tick,
            entity_changes,
            commands,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayKeyframe {
    pub tick: Tick,
    pub state: WorldSnapshot,
    pub environment: ReplayEnvironmentSnapshot,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayDeltaRecord {
    pub tick: Tick,
    pub delta: WorldDelta,
    pub trace: TickTrace,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReplayStorageRecord {
    Keyframe(ReplayKeyframe),
    Delta(ReplayDeltaRecord),
}

pub fn replay_record_for_trace(
    previous_state: &WorldSnapshot,
    trace: TickTrace,
    environment: ReplayEnvironmentSnapshot,
    config: &ReplayStorageConfig,
) -> ReplayStorageRecord {
    if config.is_keyframe_tick(trace.tick) {
        ReplayStorageRecord::Keyframe(ReplayKeyframe {
            tick: trace.tick,
            state: trace.state.clone(),
            environment,
        })
    } else {
        ReplayStorageRecord::Delta(ReplayDeltaRecord {
            tick: trace.tick,
            delta: WorldDelta::between(
                previous_state,
                &trace.state,
                trace.tick.saturating_sub(1),
                trace.tick,
                trace.commands.clone(),
            ),
            trace,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InMemoryReplayStore {
    pub keyframes: BTreeMap<Tick, ReplayKeyframe>,
    pub deltas: BTreeMap<Tick, ReplayDeltaRecord>,
}

impl InMemoryReplayStore {
    pub fn put(&mut self, record: ReplayStorageRecord) {
        match record {
            ReplayStorageRecord::Keyframe(keyframe) => {
                self.keyframes.insert(keyframe.tick, keyframe);
            }
            ReplayStorageRecord::Delta(delta) => {
                self.deltas.insert(delta.tick, delta);
            }
        }
    }

    pub fn replay_to(&self, target_tick: Tick) -> Result<WorldSnapshot, ReplayError> {
        let (keyframe_tick, keyframe) = self
            .keyframes
            .range(..=target_tick)
            .next_back()
            .ok_or(ReplayError::MissingKeyframe { tick: target_tick })?;
        let mut state = keyframe.state.clone();
        for tick in (keyframe_tick + 1)..=target_tick {
            let delta = self
                .deltas
                .get(&tick)
                .ok_or(ReplayError::MissingDelta { tick })?;
            state = replay_tick(&state, &delta.trace)?;
        }
        Ok(state)
    }

    pub fn environment_at(&self, tick: Tick) -> Option<&ReplayEnvironmentSnapshot> {
        self.keyframes
            .range(..=tick)
            .next_back()
            .map(|(_, keyframe)| &keyframe.environment)
    }
}

pub fn replay_environment(world: &SwarmWorld) -> ReplayEnvironmentSnapshot {
    ReplayEnvironmentSnapshot::from_world(world.app.world())
}

#[allow(dead_code)]
fn _player_id_type_guard(_: PlayerId) {}
