use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::Path,
};

use bevy::prelude::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use swarm_engine_api::ids::{BodyPart, RoomId};
use swarm_engine_plugin_sdk::buffers::SpecialAttackKind;
use swarm_engine_plugin_sdk::components::Position;

use crate::components::WorldMode;
use crate::world::{PlayerViewMode, WorldConfig};

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

impl PluginEntry {
    pub fn config_bool(&self, key: &str) -> Option<bool> {
        self.config.get(key).and_then(Value::as_bool)
    }

    pub fn config_u64(&self, key: &str) -> Option<u64> {
        self.config.get(key).and_then(Value::as_u64)
    }

    pub fn config_u32(&self, key: &str) -> Option<u32> {
        self.config_u64(key).and_then(|value| value.try_into().ok())
    }
}

impl PluginLock {
    pub fn load_or_default(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(contents) => Self::parse_lock(&contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::vanilla()),
            Err(error) => Err(format!("failed to read {}: {error}", path.display())),
        }
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let contents = std::fs::read_to_string(path.as_ref())
            .map_err(|error| format!("failed to read {}: {error}", path.as_ref().display()))?;
        Self::parse_lock(&contents)
    }

    fn parse_lock(contents: &str) -> Result<Self, String> {
        toml::from_str(contents).map_err(|error| format!("failed to parse mods.lock: {error}"))
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

    pub fn enabled_vanilla_plugins_in_dependency_order(&self) -> Result<Vec<&'static str>, String> {
        self.validate_known_plugins()?;
        self.validate_dependencies()?;
        Ok(VANILLA_PLUGIN_NAMES
            .iter()
            .copied()
            .filter(|name| self.enabled(name))
            .collect())
    }

    pub fn validate_enabled_features(&self) -> Result<(), String> {
        for name in self.enabled_vanilla_plugins_in_dependency_order()? {
            if !compiled_feature_enabled(name) {
                return Err(format!(
                    "mods.lock enables '{name}' but the engine binary was not compiled with feature '{}'",
                    feature_name(name)
                ));
            }
        }
        Ok(())
    }

    pub fn runtime_config(&self) -> Result<VanillaRuntimeConfig, String> {
        self.validate_known_plugins()?;
        self.validate_dependencies()?;
        Ok(VanillaRuntimeConfig {
            combat_core: self.decode_enabled("combat-core")?,
            depot_storage: self.decode_enabled("depot-storage")?,
            empire_upkeep: self.decode_enabled("empire-upkeep")?,
            fog_of_war: self.decode_enabled("fog-of-war")?,
            pve_spawning: self.decode_enabled("pve-spawning")?,
            resource_decay: self.decode_enabled("resource-decay")?,
            special_attacks: self.decode_enabled("special-attacks")?,
            vanilla_boss: self.decode_enabled("vanilla-boss")?,
        })
    }

    fn enabled(&self, name: &str) -> bool {
        self.plugins
            .get(name)
            .map(|entry| entry.enabled)
            .unwrap_or(true)
    }

    fn validate_known_plugins(&self) -> Result<(), String> {
        for name in self.plugins.keys() {
            if !VANILLA_PLUGIN_NAMES.contains(&name.as_str()) {
                return Err(format!("mods.lock contains unknown plugin '{name}'"));
            }
        }
        Ok(())
    }

    fn validate_dependencies(&self) -> Result<(), String> {
        for (plugin, dependencies) in VANILLA_PLUGIN_DEPENDENCIES {
            if !self.enabled(plugin) {
                continue;
            }
            for dependency in *dependencies {
                if !self.enabled(dependency) {
                    return Err(format!(
                        "mods.lock enables '{plugin}' but dependency '{dependency}' is disabled"
                    ));
                }
            }
        }
        Ok(())
    }

    fn decode_enabled<T>(&self, name: &str) -> Result<Option<T>, String>
    where
        T: DeserializeOwned + Default + ValidateRuntimeConfig,
    {
        if !self.enabled(name) {
            return Ok(None);
        }
        let entry = self.plugins.get(name).cloned().unwrap_or_default();
        let config = decode_plugin_config::<T>(name, &entry)?;
        config.validate(name)?;
        Ok(Some(config))
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

pub const VANILLA_PLUGIN_DEPENDENCIES: &[(&str, &[&str])] = &[
    ("combat-core", &[]),
    ("depot-storage", &[]),
    ("empire-upkeep", &[]),
    ("fog-of-war", &[]),
    ("pve-spawning", &[]),
    ("resource-decay", &[]),
    ("special-attacks", &["combat-core"]),
    ("vanilla-boss", &["pve-spawning", "combat-core"]),
];

pub const CANONICAL_PLUGIN_CONFIG_KEYS: &[(&str, &[&str])] = &[
    ("combat-core", &["damage_multiplier"]),
    (
        "depot-storage",
        &[
            "depot_capacity",
            "depot_hits",
            "repair_range",
            "repair_capacity",
        ],
    ),
    (
        "empire-upkeep",
        &[
            "base_upkeep",
            "room_soft_cap",
            "controller_passive_income",
            "controller_passive_income_rcl_bonus",
            "resource",
            "repair_cap",
            "distance_decay_bp",
            "recycle_refund_base",
            "recycle_refund_min",
            "tutorial_recycle_refund_full_ticks",
        ],
    ),
    ("fog-of-war", &["fog_of_war", "player_view"]),
    (
        "pve-spawning",
        &[
            "spawn_interval",
            "max_npcs_per_room",
            "npc_drone_body",
            "npc_drop_table",
        ],
    ),
    (
        "resource-decay",
        &["decay_rate_ppm", "per_resource_decay_rate_ppm"],
    ),
    (
        "special-attacks",
        &[
            "special_attacks_enabled",
            "enabled",
            "tutorial_enabled",
            "novice_enabled",
            "damage_multiplier",
        ],
    ),
    (
        "vanilla-boss",
        &[
            "arena_bosses_enabled",
            "world_bosses_enabled",
            "boss_spawn_interval",
            "boss_templates",
        ],
    ),
];

pub const CANONICAL_PLUGIN_CONFIG_TYPES: &[(&str, &[(&str, &str)])] = &[
    ("combat-core", &[("damage_multiplier", "fixed_bp")]),
    (
        "depot-storage",
        &[
            ("depot_capacity", "u32"),
            ("depot_hits", "u32"),
            ("repair_range", "u32"),
            ("repair_capacity", "u32"),
        ],
    ),
    (
        "empire-upkeep",
        &[
            ("base_upkeep", "u32"),
            ("room_soft_cap", "u32"),
            ("controller_passive_income", "u32"),
            ("controller_passive_income_rcl_bonus", "u32"),
            ("resource", "string"),
            ("repair_cap", "basis_points"),
            ("distance_decay_bp", "basis_points"),
            ("recycle_refund_base", "basis_points"),
            ("recycle_refund_min", "basis_points"),
            ("tutorial_recycle_refund_full_ticks", "u64"),
        ],
    ),
    (
        "fog-of-war",
        &[("fog_of_war", "bool"), ("player_view", "enum")],
    ),
    (
        "pve-spawning",
        &[
            ("spawn_interval", "u32"),
            ("max_npcs_per_room", "u32"),
            ("npc_drone_body", "array<BodyPart>"),
            ("npc_drop_table", "map<Resource,u32>"),
        ],
    ),
    (
        "resource-decay",
        &[
            ("decay_rate_ppm", "ppm"),
            ("per_resource_decay_rate_ppm", "map<Resource,ppm>"),
        ],
    ),
    (
        "special-attacks",
        &[
            ("special_attacks_enabled", "bool"),
            ("enabled", "array<SpecialAttack>"),
            ("tutorial_enabled", "array<SpecialAttack>"),
            ("novice_enabled", "array<SpecialAttack>"),
            ("damage_multiplier", "fixed_bp"),
        ],
    ),
    (
        "vanilla-boss",
        &[
            ("arena_bosses_enabled", "bool"),
            ("world_bosses_enabled", "bool"),
            ("boss_spawn_interval", "u64"),
            ("boss_templates", "array<BossTemplate>"),
        ],
    ),
];

#[derive(Debug, Clone, PartialEq)]
pub struct VanillaRuntimeConfig {
    pub combat_core: Option<CombatCoreRuntimeConfig>,
    pub depot_storage: Option<DepotStorageRuntimeConfig>,
    pub empire_upkeep: Option<EmpireUpkeepRuntimeConfig>,
    pub fog_of_war: Option<FogOfWarRuntimeConfig>,
    pub pve_spawning: Option<PveSpawningRuntimeConfig>,
    pub resource_decay: Option<ResourceDecayRuntimeConfig>,
    pub special_attacks: Option<SpecialAttacksRuntimeConfig>,
    pub vanilla_boss: Option<VanillaBossRuntimeConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CombatCoreRuntimeConfig {
    pub damage_multiplier: u32,
}

impl Default for CombatCoreRuntimeConfig {
    fn default() -> Self {
        Self {
            damage_multiplier: 10_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DepotStorageRuntimeConfig {
    pub depot_capacity: u32,
    pub depot_hits: u32,
    pub repair_range: u32,
    pub repair_capacity: u32,
}

impl Default for DepotStorageRuntimeConfig {
    fn default() -> Self {
        Self {
            depot_capacity: 10_000,
            depot_hits: 5_000,
            repair_range: 1,
            repair_capacity: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmpireUpkeepRuntimeConfig {
    pub base_upkeep: u32,
    pub room_soft_cap: u32,
    pub controller_passive_income: u32,
    pub controller_passive_income_rcl_bonus: u32,
    pub resource: String,
    pub repair_cap: u32,
    pub distance_decay_bp: u32,
    pub recycle_refund_base: u32,
    pub recycle_refund_min: u32,
    pub tutorial_recycle_refund_full_ticks: u64,
}

impl Default for EmpireUpkeepRuntimeConfig {
    fn default() -> Self {
        let defaults = crate::world::EmpireUpkeepConfig::default();
        Self {
            base_upkeep: defaults.base_upkeep,
            room_soft_cap: defaults.room_soft_cap,
            controller_passive_income: defaults.controller_passive_income,
            controller_passive_income_rcl_bonus: defaults.controller_passive_income_rcl_bonus,
            resource: defaults.resource,
            repair_cap: defaults.repair_cap,
            distance_decay_bp: defaults.distance_decay_bp,
            recycle_refund_base: defaults.recycle_refund_base,
            recycle_refund_min: defaults.recycle_refund_min,
            tutorial_recycle_refund_full_ticks: defaults.tutorial_recycle_refund_full_ticks,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FogOfWarRuntimeConfig {
    pub fog_of_war: bool,
    pub player_view: PlayerViewMode,
}

impl Default for FogOfWarRuntimeConfig {
    fn default() -> Self {
        let defaults = crate::world::VisibilityConfig::default();
        Self {
            fog_of_war: defaults.fog_of_war,
            player_view: defaults.player_view,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PveSpawningRuntimeConfig {
    pub spawn_interval: u32,
    pub max_npcs_per_room: u32,
    pub npc_drone_body: Vec<BodyPart>,
    pub npc_drop_table: BTreeMap<String, u32>,
}

impl Default for PveSpawningRuntimeConfig {
    fn default() -> Self {
        Self {
            spawn_interval: 300,
            max_npcs_per_room: 50,
            npc_drone_body: vec![BodyPart::Attack, BodyPart::Move, BodyPart::Move],
            npc_drop_table: BTreeMap::from([("Energy".to_string(), 50)]),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResourceDecayRuntimeConfig {
    pub decay_rate_ppm: u32,
    pub per_resource_decay_rate_ppm: BTreeMap<String, u32>,
}

impl Default for ResourceDecayRuntimeConfig {
    fn default() -> Self {
        Self {
            decay_rate_ppm: 1_000,
            per_resource_decay_rate_ppm: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SpecialAttackName {
    Hack,
    Drain,
    Overload,
    Debilitate,
    Disrupt,
    Fortify,
    Leech,
    Fabricate,
}

impl SpecialAttackName {
    pub fn runtime_kind(self) -> SpecialAttackKind {
        match self {
            Self::Hack => SpecialAttackKind::Hack,
            Self::Drain => SpecialAttackKind::Drain,
            Self::Overload => SpecialAttackKind::Overload,
            Self::Debilitate => SpecialAttackKind::Debilitate,
            Self::Disrupt => SpecialAttackKind::Disrupt,
            Self::Fortify => SpecialAttackKind::Fortify,
            Self::Leech => SpecialAttackKind::Leech,
            Self::Fabricate => SpecialAttackKind::Fabricate,
        }
    }

    pub fn action_name(self) -> &'static str {
        match self {
            Self::Hack => "Hack",
            Self::Drain => "Drain",
            Self::Overload => "Overload",
            Self::Debilitate => "Debilitate",
            Self::Disrupt => "Disrupt",
            Self::Fortify => "Fortify",
            Self::Leech => "Leech",
            Self::Fabricate => "Fabricate",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpecialAttacksRuntimeConfig {
    pub special_attacks_enabled: bool,
    pub enabled: BTreeSet<SpecialAttackName>,
    pub tutorial_enabled: BTreeSet<SpecialAttackName>,
    pub novice_enabled: BTreeSet<SpecialAttackName>,
    pub damage_multiplier: u32,
}

impl Default for SpecialAttacksRuntimeConfig {
    fn default() -> Self {
        Self {
            special_attacks_enabled: true,
            enabled: all_special_attack_names(),
            tutorial_enabled: [
                SpecialAttackName::Hack,
                SpecialAttackName::Drain,
                SpecialAttackName::Fortify,
            ]
            .into_iter()
            .collect(),
            novice_enabled: [
                SpecialAttackName::Hack,
                SpecialAttackName::Drain,
                SpecialAttackName::Overload,
                SpecialAttackName::Fortify,
            ]
            .into_iter()
            .collect(),
            damage_multiplier: 10_000,
        }
    }
}

impl SpecialAttacksRuntimeConfig {
    pub fn runtime_kinds_for_mode(&self, mode: WorldMode) -> BTreeSet<SpecialAttackKind> {
        if !self.special_attacks_enabled {
            return BTreeSet::new();
        }
        let names = match mode {
            WorldMode::Tutorial => &self.tutorial_enabled,
            WorldMode::Novice => &self.novice_enabled,
            WorldMode::Default | WorldMode::Arena => &self.enabled,
        };
        names.iter().map(|name| name.runtime_kind()).collect()
    }

    fn action_names_for_mode(&self, mode: WorldMode) -> BTreeSet<&'static str> {
        if !self.special_attacks_enabled {
            return BTreeSet::new();
        }
        let names = match mode {
            WorldMode::Tutorial => &self.tutorial_enabled,
            WorldMode::Novice => &self.novice_enabled,
            WorldMode::Default | WorldMode::Arena => &self.enabled,
        };
        names.iter().map(|name| name.action_name()).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BossModeConfig {
    World,
    Arena,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BossTemplateConfig {
    pub name: String,
    pub mode: BossModeConfig,
    pub hits: u32,
    pub phases: Vec<u32>,
    pub drops: BTreeMap<String, u32>,
    pub spawn_position: Position,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VanillaBossRuntimeConfig {
    pub arena_bosses_enabled: bool,
    pub world_bosses_enabled: bool,
    pub boss_spawn_interval: u64,
    pub boss_templates: Vec<BossTemplateConfig>,
}

impl Default for VanillaBossRuntimeConfig {
    fn default() -> Self {
        Self {
            arena_bosses_enabled: true,
            world_bosses_enabled: true,
            boss_spawn_interval: 5_000,
            boss_templates: vec![
                BossTemplateConfig {
                    name: "world-alpha".to_string(),
                    mode: BossModeConfig::World,
                    hits: 100_000,
                    phases: vec![75, 50, 25],
                    drops: BTreeMap::from([
                        ("Energy".to_string(), 5_000),
                        ("Mineral".to_string(), 100),
                    ]),
                    spawn_position: Position {
                        x: 25,
                        y: 25,
                        room: RoomId(0),
                    },
                },
                BossTemplateConfig {
                    name: "arena-champion".to_string(),
                    mode: BossModeConfig::Arena,
                    hits: 50_000,
                    phases: vec![50, 20],
                    drops: BTreeMap::from([("ArenaToken".to_string(), 1)]),
                    spawn_position: Position {
                        x: 25,
                        y: 25,
                        room: RoomId(1),
                    },
                },
            ],
        }
    }
}

pub trait ValidateRuntimeConfig {
    fn validate(&self, plugin: &str) -> Result<(), String>;
}

impl ValidateRuntimeConfig for CombatCoreRuntimeConfig {
    fn validate(&self, plugin: &str) -> Result<(), String> {
        validate_fixed_bp(plugin, "damage_multiplier", self.damage_multiplier)
    }
}

impl ValidateRuntimeConfig for DepotStorageRuntimeConfig {
    fn validate(&self, plugin: &str) -> Result<(), String> {
        validate_positive(plugin, "depot_capacity", self.depot_capacity)?;
        validate_positive(plugin, "depot_hits", self.depot_hits)?;
        validate_positive(plugin, "repair_capacity", self.repair_capacity)?;
        validate_positive(plugin, "repair_range", self.repair_range)
    }
}

impl ValidateRuntimeConfig for EmpireUpkeepRuntimeConfig {
    fn validate(&self, plugin: &str) -> Result<(), String> {
        validate_positive(plugin, "room_soft_cap", self.room_soft_cap)?;
        validate_bp(plugin, "repair_cap", self.repair_cap)?;
        validate_bp(plugin, "distance_decay_bp", self.distance_decay_bp)?;
        validate_bp(plugin, "recycle_refund_base", self.recycle_refund_base)?;
        validate_bp(plugin, "recycle_refund_min", self.recycle_refund_min)?;
        if self.recycle_refund_min > self.recycle_refund_base {
            return Err(format!(
                "{plugin}.recycle_refund_min must be <= recycle_refund_base"
            ));
        }
        if self.resource.trim().is_empty() {
            return Err(format!("{plugin}.resource must not be empty"));
        }
        Ok(())
    }
}

impl ValidateRuntimeConfig for FogOfWarRuntimeConfig {
    fn validate(&self, _plugin: &str) -> Result<(), String> {
        Ok(())
    }
}

impl ValidateRuntimeConfig for PveSpawningRuntimeConfig {
    fn validate(&self, plugin: &str) -> Result<(), String> {
        validate_positive(plugin, "spawn_interval", self.spawn_interval)?;
        validate_positive(plugin, "max_npcs_per_room", self.max_npcs_per_room)?;
        if self.npc_drone_body.is_empty() {
            return Err(format!("{plugin}.npc_drone_body must not be empty"));
        }
        Ok(())
    }
}

impl ValidateRuntimeConfig for ResourceDecayRuntimeConfig {
    fn validate(&self, plugin: &str) -> Result<(), String> {
        validate_ppm(plugin, "decay_rate_ppm", self.decay_rate_ppm)?;
        for (resource, ppm) in &self.per_resource_decay_rate_ppm {
            if resource.trim().is_empty() {
                return Err(format!(
                    "{plugin}.per_resource_decay_rate_ppm contains an empty resource name"
                ));
            }
            validate_ppm(plugin, resource, *ppm)?;
        }
        Ok(())
    }
}

impl ValidateRuntimeConfig for SpecialAttacksRuntimeConfig {
    fn validate(&self, plugin: &str) -> Result<(), String> {
        validate_fixed_bp(plugin, "damage_multiplier", self.damage_multiplier)
    }
}

impl ValidateRuntimeConfig for VanillaBossRuntimeConfig {
    fn validate(&self, plugin: &str) -> Result<(), String> {
        validate_positive_u64(plugin, "boss_spawn_interval", self.boss_spawn_interval)?;
        if self.boss_templates.is_empty() {
            return Err(format!("{plugin}.boss_templates must not be empty"));
        }
        for template in &self.boss_templates {
            validate_positive(plugin, "boss_templates[].hits", template.hits)?;
            if template.name.trim().is_empty() {
                return Err(format!("{plugin}.boss_templates[].name must not be empty"));
            }
            if template.phases.is_empty() {
                return Err(format!(
                    "{plugin}.boss_templates[].phases must not be empty"
                ));
            }
        }
        Ok(())
    }
}

pub fn apply_lock_to_world_config(
    lock: &PluginLock,
    config: &mut WorldConfig,
    mode: WorldMode,
) -> Result<VanillaRuntimeConfig, String> {
    let runtime = lock.runtime_config()?;
    if let Some(combat) = &runtime.combat_core {
        if !config.explicit_fields.contains("combat.damage_multiplier") {
            config.combat.damage_multiplier = f64::from(combat.damage_multiplier) / 10_000.0;
        }
    }
    if let Some(upkeep) = &runtime.empire_upkeep {
        apply_empire_upkeep_to_world_config(upkeep, config);
    }
    if let Some(fog) = &runtime.fog_of_war {
        if !config.explicit_fields.contains("visibility.fog_of_war") {
            config.visibility.fog_of_war = fog.fog_of_war;
        }
        if !config.explicit_fields.contains("visibility.player_view") {
            config.visibility.player_view = fog.player_view;
        }
    }
    if let Some(special) = &runtime.special_attacks {
        let allowed = special.action_names_for_mode(mode);
        config.custom_actions.retain(|action| {
            special_action_name(action.name.as_str()).is_none_or(|name| allowed.contains(name))
        });
    }
    Ok(runtime)
}

pub fn install_plugin_registry(app: &mut App, lock: PluginLock) {
    app.insert_resource(PluginRegistry {
        enabled: lock.enabled_set(),
        lock,
    });
}

pub fn register_mods(app: &mut App, lock: &PluginLock) {
    install_plugin_registry(app, lock.clone());
}

pub fn load_default_plugin_lock() -> Result<PluginLock, String> {
    PluginLock::load_or_default("mods.lock")
}

fn apply_empire_upkeep_to_world_config(
    upkeep: &EmpireUpkeepRuntimeConfig,
    config: &mut WorldConfig,
) {
    let explicit = &config.explicit_fields;
    if !explicit.contains("empire_upkeep.base_upkeep") {
        config.empire_upkeep.base_upkeep = upkeep.base_upkeep;
    }
    if !explicit.contains("empire_upkeep.room_soft_cap") {
        config.empire_upkeep.room_soft_cap = upkeep.room_soft_cap;
    }
    if !explicit.contains("empire_upkeep.controller_passive_income") {
        config.empire_upkeep.controller_passive_income = upkeep.controller_passive_income;
    }
    if !explicit.contains("empire_upkeep.controller_passive_income_rcl_bonus") {
        config.empire_upkeep.controller_passive_income_rcl_bonus =
            upkeep.controller_passive_income_rcl_bonus;
    }
    if !explicit.contains("empire_upkeep.resource") {
        config.empire_upkeep.resource = upkeep.resource.clone();
    }
    if !explicit.contains("empire_upkeep.repair_cap") {
        config.empire_upkeep.repair_cap = upkeep.repair_cap;
    }
    if !explicit.contains("empire_upkeep.distance_decay_bp") {
        config.empire_upkeep.distance_decay_bp = upkeep.distance_decay_bp;
    }
    if !explicit.contains("empire_upkeep.recycle_refund_base") {
        config.empire_upkeep.recycle_refund_base = upkeep.recycle_refund_base;
    }
    if !explicit.contains("empire_upkeep.recycle_refund_min") {
        config.empire_upkeep.recycle_refund_min = upkeep.recycle_refund_min;
    }
    if !explicit.contains("empire_upkeep.tutorial_recycle_refund_full_ticks") {
        config.empire_upkeep.tutorial_recycle_refund_full_ticks =
            upkeep.tutorial_recycle_refund_full_ticks;
    }
}

fn decode_plugin_config<T>(plugin: &str, entry: &PluginEntry) -> Result<T, String>
where
    T: DeserializeOwned + Default,
{
    let mut object = serde_json::Map::new();
    for (key, value) in &entry.config {
        object.insert(key.clone(), value.clone());
    }
    serde_json::from_value(Value::Object(object))
        .map_err(|error| format!("invalid {plugin} runtime config: {error}"))
}

fn validate_positive(plugin: &str, key: &str, value: u32) -> Result<(), String> {
    if value == 0 {
        return Err(format!("{plugin}.{key} must be greater than zero"));
    }
    Ok(())
}

fn validate_positive_u64(plugin: &str, key: &str, value: u64) -> Result<(), String> {
    if value == 0 {
        return Err(format!("{plugin}.{key} must be greater than zero"));
    }
    Ok(())
}

fn validate_bp(plugin: &str, key: &str, value: u32) -> Result<(), String> {
    if value > 10_000 {
        return Err(format!("{plugin}.{key} must be <= 10000 basis points"));
    }
    Ok(())
}

fn validate_fixed_bp(plugin: &str, key: &str, value: u32) -> Result<(), String> {
    validate_positive(plugin, key, value)?;
    if value > 1_000_000 {
        return Err(format!(
            "{plugin}.{key} must be <= 1000000 fixed basis points"
        ));
    }
    Ok(())
}

fn validate_ppm(plugin: &str, key: &str, value: u32) -> Result<(), String> {
    if value > 1_000_000 {
        return Err(format!("{plugin}.{key} must be <= 1000000 ppm"));
    }
    Ok(())
}

fn all_special_attack_names() -> BTreeSet<SpecialAttackName> {
    [
        SpecialAttackName::Hack,
        SpecialAttackName::Drain,
        SpecialAttackName::Overload,
        SpecialAttackName::Debilitate,
        SpecialAttackName::Disrupt,
        SpecialAttackName::Fortify,
        SpecialAttackName::Leech,
        SpecialAttackName::Fabricate,
    ]
    .into_iter()
    .collect()
}

fn special_action_name(name: &str) -> Option<&'static str> {
    match name {
        "Hack" => Some("Hack"),
        "Drain" => Some("Drain"),
        "Overload" => Some("Overload"),
        "Debilitate" => Some("Debilitate"),
        "Disrupt" => Some("Disrupt"),
        "Fortify" => Some("Fortify"),
        "Leech" => Some("Leech"),
        "Fabricate" => Some("Fabricate"),
        _ => None,
    }
}

fn compiled_feature_enabled(plugin: &str) -> bool {
    match plugin {
        "combat-core" => cfg!(feature = "mod_combat_core"),
        "depot-storage" => cfg!(feature = "mod_depot_storage"),
        "empire-upkeep" => cfg!(feature = "mod_empire_upkeep"),
        "fog-of-war" => cfg!(feature = "mod_fog_of_war"),
        "pve-spawning" => cfg!(feature = "mod_pve_spawning"),
        "resource-decay" => cfg!(feature = "mod_resource_decay"),
        "special-attacks" => cfg!(feature = "mod_special_attacks"),
        "vanilla-boss" => cfg!(feature = "mod_vanilla_boss"),
        _ => false,
    }
}

fn feature_name(plugin: &str) -> &'static str {
    match plugin {
        "combat-core" => "mod_combat_core",
        "depot-storage" => "mod_depot_storage",
        "empire-upkeep" => "mod_empire_upkeep",
        "fog-of-war" => "mod_fog_of_war",
        "pve-spawning" => "mod_pve_spawning",
        "resource-decay" => "mod_resource_decay",
        "special-attacks" => "mod_special_attacks",
        "vanilla-boss" => "mod_vanilla_boss",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plugin_entry_reads_typed_config_values() {
        let entry = PluginEntry {
            version: "0.1.0".to_string(),
            enabled: true,
            config: HashMap::from([
                ("enabled".to_string(), json!(false)),
                ("damage_multiplier".to_string(), json!(12500)),
                ("boss_spawn_interval".to_string(), json!(9000)),
            ]),
        };

        assert_eq!(entry.config_bool("enabled"), Some(false));
        assert_eq!(entry.config_u32("damage_multiplier"), Some(12_500));
        assert_eq!(entry.config_u64("boss_spawn_interval"), Some(9_000));
        assert_eq!(entry.config_u32("missing"), None);
    }

    #[test]
    fn strict_decode_rejects_unknown_obsolete_keys() {
        let lock = lock_with_config(
            "pve-spawning",
            [("spawn_rate", json!(5)), ("npc_types", json!("basic"))],
        );
        let error = lock.runtime_config().unwrap_err();
        assert!(error.contains("unknown field"), "{error}");
    }

    #[test]
    fn strict_decode_rejects_wrong_types_and_ranges() {
        let wrong_type = lock_with_config("combat-core", [("damage_multiplier", json!("fast"))]);
        assert!(
            wrong_type
                .runtime_config()
                .unwrap_err()
                .contains("invalid combat-core")
        );

        let invalid_range =
            lock_with_config("resource-decay", [("decay_rate_ppm", json!(1_000_001))]);
        assert!(
            invalid_range
                .runtime_config()
                .unwrap_err()
                .contains("<= 1000000 ppm")
        );
    }

    #[test]
    fn dependency_order_is_deterministic_and_validated() {
        let mut lock = PluginLock::vanilla();
        lock.plugins.get_mut("combat-core").unwrap().enabled = false;
        let error = lock
            .enabled_vanilla_plugins_in_dependency_order()
            .unwrap_err();
        assert!(error.contains("special-attacks"));

        let lock = PluginLock::vanilla();
        assert_eq!(
            lock.enabled_vanilla_plugins_in_dependency_order().unwrap(),
            VANILLA_PLUGIN_NAMES
        );
    }

    #[test]
    fn lock_defaults_apply_but_explicit_world_fields_win() {
        let lock = lock_with_config(
            "empire-upkeep",
            [("base_upkeep", json!(99)), ("room_soft_cap", json!(3))],
        );
        let mut config = WorldConfig::from_toml_str("[empire_upkeep]\nbase_upkeep = 77\n").unwrap();

        apply_lock_to_world_config(&lock, &mut config, WorldMode::Default).unwrap();

        assert_eq!(config.empire_upkeep.base_upkeep, 77);
        assert_eq!(config.empire_upkeep.room_soft_cap, 3);
    }

    #[test]
    fn special_attack_allowlists_filter_actions_without_overriding_vanilla_defs() {
        let lock = lock_with_config(
            "special-attacks",
            [
                ("enabled", json!(["Leech"])),
                ("tutorial_enabled", json!(["Hack"])),
                ("novice_enabled", json!(["Overload"])),
            ],
        );
        let expected = [
            (
                WorldMode::Tutorial,
                ["Attack", "RangedAttack", "Heal", "Hack"],
            ),
            (
                WorldMode::Novice,
                ["Attack", "RangedAttack", "Heal", "Overload"],
            ),
            (
                WorldMode::Default,
                ["Attack", "RangedAttack", "Heal", "Leech"],
            ),
            (
                WorldMode::Arena,
                ["Attack", "RangedAttack", "Heal", "Leech"],
            ),
        ];

        for (mode, expected_names) in expected {
            let mut config = WorldConfig::default();
            apply_lock_to_world_config(&lock, &mut config, mode).unwrap();
            let names = config
                .custom_actions
                .iter()
                .map(|action| action.name.as_str())
                .collect::<BTreeSet<_>>();

            assert_eq!(
                names,
                expected_names.into_iter().collect::<BTreeSet<_>>(),
                "{mode:?} custom action allowlist differed"
            );
        }
    }

    #[test]
    fn special_attack_runtime_kinds_use_mode_specific_allowlists() {
        let config = SpecialAttacksRuntimeConfig {
            enabled: [SpecialAttackName::Fabricate].into_iter().collect(),
            tutorial_enabled: [SpecialAttackName::Hack].into_iter().collect(),
            novice_enabled: [SpecialAttackName::Overload].into_iter().collect(),
            ..Default::default()
        };

        assert_eq!(
            config.runtime_kinds_for_mode(WorldMode::Tutorial),
            [SpecialAttackKind::Hack].into_iter().collect()
        );
        assert_eq!(
            config.runtime_kinds_for_mode(WorldMode::Novice),
            [SpecialAttackKind::Overload].into_iter().collect()
        );
        assert_eq!(
            config.runtime_kinds_for_mode(WorldMode::Default),
            [SpecialAttackKind::Fabricate].into_iter().collect()
        );
        assert_eq!(
            config.runtime_kinds_for_mode(WorldMode::Arena),
            [SpecialAttackKind::Fabricate].into_iter().collect()
        );
    }

    #[test]
    fn public_spectate_and_custom_actions_are_rejected_by_strict_contracts() {
        let lock = lock_with_config("fog-of-war", [("public_spectate", json!(true))]);
        assert!(lock.runtime_config().unwrap_err().contains("unknown field"));

        let lock = lock_with_config("special-attacks", [("custom_actions", json!("[]"))]);
        assert!(lock.runtime_config().unwrap_err().contains("unknown field"));
    }

    #[test]
    fn enabled_plugins_require_compiled_features() {
        let lock = PluginLock::vanilla();

        let result = lock.validate_enabled_features();
        if cfg!(all(
            feature = "mod_combat_core",
            feature = "mod_depot_storage",
            feature = "mod_empire_upkeep",
            feature = "mod_fog_of_war",
            feature = "mod_pve_spawning",
            feature = "mod_resource_decay",
            feature = "mod_special_attacks",
            feature = "mod_vanilla_boss"
        )) {
            assert!(result.is_ok());
        } else {
            assert!(
                result
                    .unwrap_err()
                    .contains("was not compiled with feature")
            );
        }
    }

    #[test]
    fn boss_templates_decode_as_typed_runtime_config() {
        let lock = lock_with_config(
            "vanilla-boss",
            [(
                "boss_templates",
                json!([{ "name": "omega", "mode": "world", "hits": 42, "phases": [75], "drops": {"Energy": 1}, "spawn_position": {"x": 1, "y": 2, "room": 0} }]),
            )],
        );
        let config = lock.runtime_config().unwrap().vanilla_boss.unwrap();

        assert_eq!(config.boss_templates[0].name, "omega");
        assert_eq!(config.boss_templates[0].mode, BossModeConfig::World);
    }

    #[test]
    fn manifests_match_canonical_runtime_schema_keys() {
        for (plugin, keys) in CANONICAL_PLUGIN_CONFIG_KEYS {
            let path = format!("mods/{plugin}/mod.toml");
            let manifest = std::fs::read_to_string(&path).unwrap();
            let manifest: toml::Value = toml::from_str(&manifest).unwrap();
            let config = manifest
                .get("config")
                .and_then(toml::Value::as_table)
                .unwrap_or_else(|| panic!("missing [config] in {path}"));
            let actual = config.keys().map(String::as_str).collect::<BTreeSet<_>>();
            let expected = keys.iter().copied().collect::<BTreeSet<_>>();
            assert_eq!(actual, expected, "{plugin} manifest keys differ");
        }
    }

    #[test]
    fn manifests_match_canonical_runtime_schema_types() {
        for (plugin, types) in CANONICAL_PLUGIN_CONFIG_TYPES {
            let path = format!("mods/{plugin}/mod.toml");
            let manifest = std::fs::read_to_string(&path).unwrap();
            let manifest: toml::Value = toml::from_str(&manifest).unwrap();
            let config = manifest
                .get("config")
                .and_then(toml::Value::as_table)
                .unwrap_or_else(|| panic!("missing [config] in {path}"));
            for (key, expected_type) in *types {
                let actual_type = config
                    .get(*key)
                    .and_then(toml::Value::as_table)
                    .and_then(|metadata| metadata.get("type"))
                    .and_then(toml::Value::as_str)
                    .unwrap_or_else(|| panic!("missing type for {plugin}.{key}"));
                assert_eq!(actual_type, *expected_type, "{plugin}.{key} type differs");
            }
        }
    }

    #[test]
    fn manifest_defaults_decode_as_typed_runtime_configs() {
        let lock = PluginLock {
            plugins: VANILLA_PLUGIN_NAMES
                .iter()
                .map(|plugin| {
                    (
                        (*plugin).to_string(),
                        PluginEntry {
                            version: "0.1.0".to_string(),
                            enabled: true,
                            config: manifest_default_config(plugin),
                        },
                    )
                })
                .collect(),
        };

        let runtime = lock.runtime_config().unwrap();
        assert_eq!(
            runtime.combat_core,
            Some(CombatCoreRuntimeConfig::default())
        );
        assert_eq!(
            runtime.depot_storage,
            Some(DepotStorageRuntimeConfig::default())
        );
        assert_eq!(
            runtime.empire_upkeep,
            Some(EmpireUpkeepRuntimeConfig::default())
        );
        assert_eq!(runtime.fog_of_war, Some(FogOfWarRuntimeConfig::default()));
        assert_eq!(
            runtime.pve_spawning,
            Some(PveSpawningRuntimeConfig::default())
        );
        assert_eq!(
            runtime.resource_decay,
            Some(ResourceDecayRuntimeConfig::default())
        );
        assert_eq!(
            runtime.special_attacks,
            Some(SpecialAttacksRuntimeConfig::default())
        );
        assert_eq!(
            runtime.vanilla_boss,
            Some(VanillaBossRuntimeConfig::default())
        );
    }

    #[test]
    fn load_or_default_only_falls_back_when_lock_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing-mods.lock");
        assert_eq!(
            PluginLock::load_or_default(&missing)
                .unwrap()
                .enabled_vanilla_plugins_in_dependency_order()
                .unwrap(),
            VANILLA_PLUGIN_NAMES
        );

        let malformed = directory.path().join("mods.lock");
        std::fs::write(&malformed, "[plugins.combat-core\n").unwrap();
        assert!(
            PluginLock::load_or_default(&malformed)
                .unwrap_err()
                .contains("failed to parse mods.lock")
        );

        let unsafe_lock = directory.path().join("unsafe-mods.lock");
        std::fs::write(
            &unsafe_lock,
            "[plugins.fog-of-war]\nversion = \"0.1.0\"\nenabled = true\nconfig = { public_spectate = true }\n",
        )
        .unwrap();
        assert!(
            PluginLock::load_or_default(&unsafe_lock)
                .unwrap()
                .runtime_config()
                .unwrap_err()
                .contains("unknown field")
        );
    }

    fn lock_with_config<I>(plugin: &str, config: I) -> PluginLock
    where
        I: IntoIterator<Item = (&'static str, Value)>,
    {
        let mut lock = PluginLock::vanilla();
        lock.plugins.get_mut(plugin).unwrap().config = config
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();
        lock
    }

    fn manifest_default_config(plugin: &str) -> HashMap<String, Value> {
        let path = format!("mods/{plugin}/mod.toml");
        let manifest = std::fs::read_to_string(&path).unwrap();
        let manifest: toml::Value = toml::from_str(&manifest).unwrap();
        manifest
            .get("config")
            .and_then(toml::Value::as_table)
            .unwrap_or_else(|| panic!("missing [config] in {path}"))
            .iter()
            .map(|(key, metadata)| {
                let default = metadata
                    .get("default")
                    .unwrap_or_else(|| panic!("missing default for {plugin}.{key}"));
                (key.clone(), serde_json::to_value(default).unwrap())
            })
            .collect()
    }
}
