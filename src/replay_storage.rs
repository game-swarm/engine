use serde::{Deserialize, Serialize};

use crate::command::Tick;
use crate::world::WorldConfig;

pub const DEFAULT_KEYFRAME_INTERVAL: Tick = 100;

// ── Replay Storage Config ────────────────────────────────────────────

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

// ── Mods Lock ────────────────────────────────────────────────────────

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
    pub fn from_rhai_modules(modules: &crate::rule_module::RhaiRuleModules) -> Self {
        let modules = modules
            .module_version_hashes()
            .iter()
            .map(|(id, hash)| ModVersionHash {
                module_id: id.clone(),
                version_hash: hash.clone(),
            })
            .collect();
        Self { modules }
    }
}

// ── World Config Snapshot ────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldConfigSnapshot {
    pub tick: Tick,
    pub config_toml: String,
}

impl WorldConfigSnapshot {
    pub fn from_config(tick: Tick, config: &WorldConfig) -> Self {
        Self {
            tick,
            config_toml: toml::to_string_pretty(config).unwrap_or_default(),
        }
    }
}

// ── Keyframe Data ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyframeData {
    pub tick: Tick,
    pub world_snapshot: Vec<u8>,
    pub mods_lock: ModsLock,
    pub world_config: WorldConfigSnapshot,
}

// ── Tick Delta ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TickDelta {
    pub tick: Tick,
    pub commands_json: String,
    pub entity_changes: Vec<EntityChange>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EntityChange {
    Created {
        entity_id: u64,
        component_data: Vec<u8>,
    },
    Modified {
        entity_id: u64,
        component_data: Vec<u8>,
    },
    Removed {
        entity_id: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldDelta {
    pub from_tick: Tick,
    pub to_tick: Tick,
    pub entity_changes: Vec<EntityChange>,
    pub commands: Vec<crate::command::RawCommand>,
}

impl WorldDelta {
    pub fn between(
        _before: &crate::tick::WorldSnapshot,
        _after: &crate::tick::WorldSnapshot,
        from_tick: Tick,
        to_tick: Tick,
        commands: Vec<crate::command::RawCommand>,
    ) -> Self {
        Self {
            from_tick,
            to_tick,
            entity_changes: Vec::new(),
            commands,
        }
    }
}

// ── Replay Error ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    NoKeyframeFound,
    TickOutOfRange,
    StorageUnavailable,
    ConfigMismatch,
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoKeyframeFound => write!(f, "no keyframe found for requested tick"),
            Self::TickOutOfRange => write!(f, "tick out of replay range"),
            Self::StorageUnavailable => write!(f, "replay storage unavailable"),
            Self::ConfigMismatch => write!(f, "world config mismatch in replay"),
        }
    }
}
