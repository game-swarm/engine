use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Resource, Debug, Clone, Default)]
pub struct PluginRegistry {
    pub enabled: HashSet<String>,
    pub lock: PluginLock,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginLock {
    #[serde(default)]
    pub plugins: HashMap<String, PluginEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEntry {
    pub version: String,
    pub enabled: bool,
    #[serde(default)]
    pub config: HashMap<String, Value>,
}

impl Default for PluginEntry {
    fn default() -> Self {
        Self {
            version: "0.1.0".to_string(),
            enabled: true,
            config: HashMap::new(),
        }
    }
}

impl PluginLock {
    pub fn load_or_default(path: impl AsRef<Path>) -> Self {
        Self::load(path).unwrap_or_else(|_| Self::vanilla())
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let contents = std::fs::read_to_string(path.as_ref())
            .map_err(|error| format!("failed to read {}: {error}", path.as_ref().display()))?;
        toml::from_str(&contents).map_err(|error| format!("failed to parse mods.lock: {error}"))
    }

    pub fn vanilla() -> Self {
        let mut plugins = HashMap::new();
        for name in VANILLA_PLUGIN_NAMES {
            plugins.insert((*name).to_string(), PluginEntry::default());
        }
        Self { plugins }
    }

    pub fn enabled_set(&self) -> HashSet<String> {
        self.plugins
            .iter()
            .filter(|(_, entry)| entry.enabled)
            .map(|(name, _)| name.clone())
            .collect()
    }
}

pub const VANILLA_PLUGIN_NAMES: &[&str] = &[
    "combat-core",
    "depot-storage",
    "empire-upkeep",
    "fog-of-war",
    "pve-spawning",
    "resource-decay",
    "special-attacks",
    "vanilla-boss",
];

pub fn install_plugin_registry(app: &mut App, lock: PluginLock) {
    app.insert_resource(PluginRegistry {
        enabled: lock.enabled_set(),
        lock,
    });
}

pub fn register_mods(app: &mut App, lock: &PluginLock) {
    install_plugin_registry(app, lock.clone());
}

pub fn load_default_plugin_lock() -> PluginLock {
    PluginLock::load_or_default("mods.lock")
}
