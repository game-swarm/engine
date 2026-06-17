use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::command::{
    CommandIntent, CommandResult, CommandSource, RawCommand, Tick, apply_command, source_gate,
    validate_command,
};
use crate::components::*;
use crate::dragonfly::DragonflyCache;
use crate::fdb::FoundationDbStore;
use crate::npc::events::{EventConfig, EventState, event_effect_system, world_event_system};
use crate::npc::loot::{BlueprintRegistry, NpcLootTables};
use crate::onboarding::{
    OnboardingConfig, OnboardingEvent, OnboardingProgress, OnboardingSwarmEvent, onboarding_system,
    send_onboarding_event,
};
use crate::pve::{
    DifficultyZone, WorldPveConfig, ZoneDefinition, zone_definition_for_room, zone_for_room,
};
use crate::ranking::{LeaderboardEntry, MatchOutcome, RankingState};
use crate::replay_storage::ReplayStore;
use crate::resources::{
    CurrentTick, GlobalStorageConfig, MarketOrders, PendingGlobalTransfers, PlayerGlobalStorage,
    PlayerLocalStorage, PveOutputTracker, ResourceDef, ResourceRegistry, SourceDef,
};
use crate::rule_module::{
    RhaiRuleModules, rhai_rule_module_tick_end_system, rhai_rule_module_tick_start_system,
    run_init_scripts,
};
use crate::systems::*;

#[path = "shard.rs"]
pub mod shard;
pub use shard::*;

static NEXT_TUTORIAL_WORLD_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Resource, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorldConfig {
    pub world: WorldSectionConfig,
    pub spawn: SpawnConfig,
    pub code: CodeConfig,
    pub drone: DroneConfig,
    pub visibility: VisibilityConfig,
    pub events: EventConfig,
    pub pve: WorldPveConfig,
    pub resources: WorldResourceConfig,
    pub combat: WorldCombatConfig,
    pub damage_types: Vec<crate::components::DamageTypeDef>,
    pub body_part_types: Vec<crate::components::BodyPartTypeDef>,
    pub structure_types: Vec<crate::components::StructureTypeDef>,
    pub resource_types: Vec<ResourceDef>,
    pub source_types: Vec<SourceDef>,
    #[serde(default = "default_special_effects")]
    pub special_effects: Vec<crate::components::SpecialEffectDef>,
    #[serde(default = "default_custom_actions")]
    pub custom_actions: Vec<crate::components::CustomActionDef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorldSectionConfig {
    pub name: String,
    pub mode: String,
    pub tick_interval_ms: u64,
    pub seed_rotation_interval: u64,
    pub world_seed: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpawnPolicy {
    RandomRoom,
    ManualSelect,
    FixedSpawn,
    Inherit,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RespawnPolicy {
    NewRoom,
    OriginalRoom,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpawnConfig {
    pub policy: SpawnPolicy,
    pub cooldown: Tick,
    pub safe_mode_duration: Tick,
    #[serde(alias = "respawn")]
    pub respawn_policy: RespawnPolicy,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodeUpdateWindow {
    pub every: Tick,
    pub duration: Tick,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CodePropagationSource {
    Spawn,
    Controller,
    #[serde(alias = "Global")]
    AnyDrone,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodeConfig {
    pub update_cost: crate::resources::ResourceCost,
    pub update_cooldown: Tick,
    pub update_window: CodeUpdateWindow,
    pub propagation_speed: u32,
    pub propagation_source: CodePropagationSource,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DroneConfig {
    pub env_vars: bool,
    pub memory_size: u32,
    pub memory_spawn_cost: crate::resources::ResourceCost,
    pub memory_upkeep_cost: crate::resources::ResourceCost,
    pub lifespan: u32,
    pub min_lifespan: u32,
    pub max_body_parts: usize,
    pub max_drones_per_player: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlayerViewMode {
    Drone,
    Full,
    Allied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplayPrivacy {
    Private,
    Allies,
    World,
    Public,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VisibilityConfig {
    pub fog_of_war: bool,
    pub player_view: PlayerViewMode,
    pub public_spectate: bool,
    pub spectate_delay: Tick,
    pub replay_privacy: ReplayPrivacy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorldResourceConfig {
    pub source_regeneration_rate: u32,
    pub build_cost_multiplier: u32,
    pub drone_decay_rate: u32,
    pub max_pve_output_per_tick: u32,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorldCombatConfig {
    pub pvp_enabled: bool,
    pub friendly_fire: bool,
    pub damage_multiplier: f64,
}
impl Default for WorldConfig {
    fn default() -> Self {
        Self {
            world: WorldSectionConfig::default(),
            spawn: SpawnConfig::default(),
            code: CodeConfig::default(),
            drone: DroneConfig::default(),
            visibility: VisibilityConfig::default(),
            events: EventConfig::default(),
            pve: WorldPveConfig::default(),
            resources: WorldResourceConfig::default(),
            combat: WorldCombatConfig::default(),
            damage_types: default_damage_types(),
            body_part_types: Vec::new(),
            structure_types: Vec::new(),
            resource_types: Vec::new(),
            source_types: Vec::new(),
            special_effects: default_special_effects(),
            custom_actions: default_custom_actions(),
        }
    }
}

fn damage_type_def(
    damage_type: DamageType,
    component_multipliers: &[(&str, f64)],
    attribute_multipliers: &[(&str, f64)],
) -> crate::components::DamageTypeDef {
    let mut def = crate::components::DamageTypeDef {
        name: damage_type.to_string(),
        ..Default::default()
    };
    for (component, multiplier) in component_multipliers {
        def.component_multipliers
            .insert((*component).to_string(), *multiplier);
    }
    for (attribute, multiplier) in attribute_multipliers {
        def.attribute_multipliers
            .insert((*attribute).to_string(), *multiplier);
    }
    def
}

fn default_damage_types() -> Vec<crate::components::DamageTypeDef> {
    vec![
        damage_type_def(
            DamageType::Kinetic,
            &[
                ("Move", 1.0),
                ("Work", 1.0),
                ("Carry", 1.0),
                ("Attack", 1.0),
                ("RangedAttack", 1.0),
                ("Heal", 1.0),
                ("Claim", 1.0),
                ("Tough", 0.5),
            ],
            &[("Shielded", 0.7)],
        ),
        damage_type_def(
            DamageType::Thermal,
            &[
                ("Move", 1.2),
                ("Work", 1.0),
                ("Carry", 1.0),
                ("Attack", 1.0),
                ("RangedAttack", 1.0),
                ("Heal", 1.0),
                ("Claim", 1.0),
                ("Tough", 0.8),
            ],
            &[],
        ),
        damage_type_def(
            DamageType::EMP,
            &[
                ("Move", 1.0),
                ("Work", 1.0),
                ("Carry", 1.0),
                ("Attack", 1.0),
                ("RangedAttack", 1.0),
                ("Heal", 1.0),
                ("Claim", 1.3),
                ("Tough", 1.0),
            ],
            &[],
        ),
        damage_type_def(
            DamageType::Corrosive,
            &[
                ("Move", 1.0),
                ("Work", 1.2),
                ("Carry", 1.2),
                ("Attack", 1.0),
                ("RangedAttack", 1.0),
                ("Heal", 1.0),
                ("Claim", 1.0),
                ("Tough", 0.7),
            ],
            &[],
        ),
        damage_type_def(
            DamageType::Psionic,
            &[
                ("Move", 1.0),
                ("Work", 1.0),
                ("Carry", 1.0),
                ("Attack", 1.0),
                ("RangedAttack", 1.0),
                ("Heal", 1.0),
                ("Claim", 1.4),
                ("Tough", 1.0),
            ],
            &[],
        ),
    ]
}

fn special_effect_def(
    name: &str,
    description: &str,
    handler: &str,
    target: &str,
    duration: u32,
    resistance: Option<&str>,
) -> crate::components::SpecialEffectDef {
    crate::components::SpecialEffectDef {
        name: name.to_string(),
        description: description.to_string(),
        handler: handler.to_string(),
        target: target.to_string(),
        duration,
        resistance: resistance.map(str::to_string),
    }
}

fn default_special_effects() -> Vec<crate::components::SpecialEffectDef> {
    vec![
        special_effect_def(
            "hack",
            "Take control of a target drone after a control lock",
            "hack",
            "enemy_drone",
            5,
            Some("Psionic"),
        ),
        special_effect_def(
            "drain",
            "Steal resources from a target structure or storage",
            "drain",
            "enemy_structure",
            0,
            Some("EMP"),
        ),
        special_effect_def(
            "overload",
            "Reduce the target player's fuel budget",
            "overload",
            "enemy_player",
            0,
            Some("EMP"),
        ),
        special_effect_def(
            "debilitate",
            "Apply vulnerability to a target damage type",
            "debilitate",
            "enemy_any",
            50,
            Some("Corrosive"),
        ),
        special_effect_def(
            "disrupt",
            "Interrupt a target's ongoing special action",
            "disrupt",
            "enemy_drone",
            0,
            Some("Sonic"),
        ),
        special_effect_def(
            "fortify",
            "Shield and cleanse self or an ally",
            "fortify",
            "self_or_ally",
            3,
            None,
        ),
        special_effect_def(
            "leech",
            "Heal the attacker for a portion of dealt damage",
            "leech",
            "enemy_any",
            0,
            Some("Corrosive"),
        ),
        special_effect_def(
            "fabricate",
            "Convert an enemy drone into an owned structure",
            "fabricate",
            "enemy_drone",
            0,
            Some("Psionic"),
        ),
        special_effect_def(
            "heal_self",
            "Heal the attacker for a configured portion of damage",
            "heal_self",
            "enemy_any",
            0,
            None,
        ),
        special_effect_def(
            "scramble_commands",
            "Randomize the target's next command order",
            "scramble_commands",
            "enemy_drone",
            0,
            None,
        ),
        special_effect_def(
            "convert_to_structure",
            "Convert a target drone into an owned structure",
            "convert_to_structure",
            "enemy_drone",
            0,
            Some("Psionic"),
        ),
    ]
}

fn custom_action_def(
    name: &str,
    description: &str,
    special_effect: &str,
    cooldown: Option<u32>,
    cost: &[(&str, u32)],
) -> crate::components::CustomActionDef {
    let mut action = crate::components::CustomActionDef {
        name: name.to_string(),
        description: description.to_string(),
        special_effect: Some(special_effect.to_string()),
        cooldown,
        ..Default::default()
    };
    for (resource, amount) in cost {
        action.cost.insert((*resource).to_string(), *amount);
    }
    action
}

fn default_custom_actions() -> Vec<crate::components::CustomActionDef> {
    let mut debilitate = custom_action_def(
        "Debilitate",
        "Apply vulnerability to a target damage type for 50 ticks",
        "debilitate",
        Some(150),
        &[("Energy", 200)],
    );
    debilitate.damage_type = Some("Corrosive".to_string());
    debilitate.special_param = Some(2.0);

    let mut fortify = custom_action_def(
        "Fortify",
        "Shield and cleanse self or an ally",
        "fortify",
        Some(300),
        &[("Energy", 400)],
    );
    fortify.special_param = Some(0.5);

    let mut hack = custom_action_def(
        "Hack",
        "Take control of a drone after a control lock",
        "hack",
        Some(200),
        &[("Energy", 1000)],
    );
    hack.damage_type = Some("Psionic".to_string());
    hack.range = 1;

    let mut drain = custom_action_def(
        "Drain",
        "Steal resources from a target structure",
        "drain",
        Some(50),
        &[("Energy", 200)],
    );
    drain.damage_type = Some("EMP".to_string());
    drain.range = 1;

    let mut overload = custom_action_def(
        "Overload",
        "Reduce the target player's fuel budget",
        "overload",
        Some(200),
        &[("Energy", 300)],
    );
    overload.damage_type = Some("EMP".to_string());
    overload.range = 1;
    overload.special_param = Some(500_000.0);

    let mut disrupt = custom_action_def(
        "Disrupt",
        "Interrupt a target's ongoing special action",
        "disrupt",
        Some(50),
        &[("Energy", 100)],
    );
    disrupt.damage_type = Some("Sonic".to_string());
    disrupt.range = 1;

    let mut leech = custom_action_def(
        "Leech",
        "Corrosive attack that heals the attacker for 50% of dealt damage",
        "leech",
        None,
        &[("Energy", 300)],
    );
    leech.damage_type = Some("Corrosive".to_string());
    leech.base_damage = Some(15);
    leech.range = 1;
    leech.special_param = Some(0.5);

    let mut fabricate = custom_action_def(
        "Fabricate",
        "Convert an enemy drone into an owned structure",
        "fabricate",
        Some(500),
        &[("Energy", 2000), ("Mineral", 500)],
    );
    fabricate.range = 1;

    vec![
        hack, drain, overload, debilitate, disrupt, fortify, leech, fabricate,
    ]
}

impl Default for WorldSectionConfig {
    fn default() -> Self {
        Self {
            name: "World of Swarm".to_string(),
            mode: "persistent".to_string(),
            tick_interval_ms: crate::components::DEFAULT_TICK_INTERVAL_MS,
            seed_rotation_interval: 0,
            world_seed: 0,
        }
    }
}
impl Default for SpawnPolicy {
    fn default() -> Self {
        Self::RandomRoom
    }
}
impl Default for RespawnPolicy {
    fn default() -> Self {
        Self::NewRoom
    }
}
impl Default for SpawnConfig {
    fn default() -> Self {
        Self {
            policy: SpawnPolicy::RandomRoom,
            cooldown: 0,
            safe_mode_duration: 500,
            respawn_policy: RespawnPolicy::NewRoom,
        }
    }
}
impl Default for CodeUpdateWindow {
    fn default() -> Self {
        Self {
            every: 0,
            duration: 0,
        }
    }
}
impl Default for CodePropagationSource {
    fn default() -> Self {
        Self::Spawn
    }
}
impl Default for CodeConfig {
    fn default() -> Self {
        Self {
            update_cost: crate::resources::ResourceCost::new(),
            update_cooldown: 5,
            update_window: CodeUpdateWindow::default(),
            propagation_speed: 0,
            propagation_source: CodePropagationSource::Spawn,
        }
    }
}
impl Default for DroneConfig {
    fn default() -> Self {
        Self {
            env_vars: true,
            memory_size: 1024,
            memory_spawn_cost: crate::resources::ResourceCost::new(),
            memory_upkeep_cost: crate::resources::ResourceCost::new(),
            lifespan: DEFAULT_DRONE_LIFESPAN,
            min_lifespan: MIN_LIFESPAN,
            max_body_parts: 50,
            max_drones_per_player: 500,
        }
    }
}
impl Default for PlayerViewMode {
    fn default() -> Self {
        Self::Drone
    }
}

impl Default for ReplayPrivacy {
    fn default() -> Self {
        Self::Private
    }
}

impl Default for VisibilityConfig {
    fn default() -> Self {
        Self {
            fog_of_war: true,
            player_view: PlayerViewMode::Drone,
            public_spectate: false,
            spectate_delay: 0,
            replay_privacy: ReplayPrivacy::Private,
        }
    }
}

impl Default for WorldResourceConfig {
    fn default() -> Self {
        Self {
            source_regeneration_rate: 10_000,
            build_cost_multiplier: 10_000,
            drone_decay_rate: 10_000,
            max_pve_output_per_tick: crate::resources::DEFAULT_MAX_PVE_OUTPUT_PER_TICK,
        }
    }
}
impl Default for WorldCombatConfig {
    fn default() -> Self {
        Self {
            pvp_enabled: true,
            friendly_fire: false,
            damage_multiplier: 1.0,
        }
    }
}
impl WorldConfig {
    pub fn from_toml_str(contents: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(contents)
    }
    pub fn from_world_toml(
        path: impl AsRef<std::path::Path>,
    ) -> Result<Self, WorldConfigLoadError> {
        let path = path.as_ref();
        let contents =
            std::fs::read_to_string(path).map_err(|source| WorldConfigLoadError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        Self::from_toml_str(&contents).map_err(WorldConfigLoadError::Parse)
    }
    pub fn load_or_default(path: impl AsRef<std::path::Path>) -> Self {
        Self::from_world_toml(path).unwrap_or_default()
    }
    pub fn propagation_system_enabled(&self) -> bool {
        self.code.propagation_speed > 0
    }
    fn combat_damage_multiplier_fixed(&self) -> u32 {
        let scaled = (self.combat.damage_multiplier * 10_000.0).round();
        if !scaled.is_finite() || scaled <= 0.0 {
            0
        } else {
            scaled.min(u32::MAX as f64) as u32
        }
    }
    fn install_resources(&self, app: &mut App) {
        app.insert_resource(self.clone());
        app.insert_resource(self.events.clone());
        app.insert_resource(CombatRules {
            damage_multiplier: self.combat_damage_multiplier_fixed(),
        });
        let damage_registry = DamageTypeRegistry::from_defs(self.damage_types.clone());
        let body_registry = BodyPartRegistry::from_defs(self.body_part_types.clone());
        app.insert_resource(ResistanceRegistry::from_registries(
            &body_registry,
            &damage_registry,
        ));
        app.insert_resource(damage_registry);
        app.insert_resource(body_registry);
        app.insert_resource(StructureTypeRegistry::from_defs(
            self.structure_types.clone(),
        ));
        app.insert_resource(ResourceRegistry::from_defs(
            self.resource_types.clone(),
            self.source_types.clone(),
        ));
        app.insert_resource(SpecialEffectRegistry::from_defs(
            self.special_effects.clone(),
        ));
        app.insert_resource(CustomActionRegistry::from_defs(self.custom_actions.clone()));
        app.insert_resource(NpcLootTables::default());
        app.insert_resource(BlueprintRegistry::default());
        app.insert_resource(PveOutputTracker::new(
            self.resources.max_pve_output_per_tick,
        ));
        app.insert_resource(LatestCodeVersions::default());
        app.insert_resource(RepairTracker {
            per_player: Default::default(),
            hard_cap: 1,
        });
        app.insert_resource(DroneEnvVars::default());
        app.insert_resource(ReplayStore::default());
    }
    fn register_systems(&self, app: &mut App) {
        if self.propagation_system_enabled() {
            app.add_systems(Update, code_propagation_system.before(spawn_system));
        }
        app.add_systems(
            Update,
            spawning_grace_system
                .after(spawn_system)
                .before(combat_system),
        );
        app.add_systems(
            Update,
            spawning_grace_expiry_system
                .after(combat_system)
                .before(decay_system),
        );
        app.add_systems(
            Update,
            (world_event_system, event_effect_system)
                .chain()
                .after(spawn_system)
                .before(regeneration_system),
        );
        app.add_systems(
            Update,
            (
                rhai_rule_module_tick_start_system,
                death_mark_system,
                pvp_block_system,
                spawn_system,
                regeneration_system,
                seed_rotation_system,
                cargo_in_transit_system,
                global_storage_system,
                controller_system,
                controller_repair_system,
                depot_repair_system,
                room_state_system,
                combat_system,
                decay_system,
                memory_upkeep_system,
                drone_env_var_system,
                rhai_rule_module_tick_end_system,
                death_cleanup_system,
                onboarding_system,
            )
                .chain(),
        );
    }
}
#[derive(Debug)]
pub enum WorldConfigLoadError {
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    Parse(toml::de::Error),
}
impl std::fmt::Display for WorldConfigLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "failed to read {}: {source}", path.display()),
            Self::Parse(source) => write!(f, "failed to parse world.toml: {source}"),
        }
    }
}
impl std::error::Error for WorldConfigLoadError {}
fn code_propagation_system(
    config: Res<WorldConfig>,
    latest: Res<LatestCodeVersions>,
    mut commands: Commands,
    positions: Query<(Entity, &Position, &Owner, Option<&CodeVersion>)>,
    spawn_structures: Query<(Entity, &Position), With<Structure>>,
    controllers: Query<(Entity, &Position), With<Controller>>,
) {
    let speed = config.code.propagation_speed;
    if speed == 0 {
        return;
    }

    let sources: Vec<Position> = match config.code.propagation_source {
        CodePropagationSource::Spawn => spawn_structures.iter().map(|(_, pos)| *pos).collect(),
        CodePropagationSource::Controller => controllers.iter().map(|(_, pos)| *pos).collect(),
        CodePropagationSource::AnyDrone => return,
    };

    if sources.is_empty() {
        return;
    }

    for (entity, pos, owner, code_version) in positions.iter() {
        let Some(&latest_version) = latest.0.get(&owner.0) else {
            continue;
        };
        let current = code_version.copied().unwrap_or_default();
        if current.0 >= latest_version {
            continue;
        }
        let nearest = sources
            .iter()
            .map(|src| hex_distance(pos, src))
            .min()
            .unwrap_or(u32::MAX);
        if nearest <= speed {
            commands.entity(entity).insert(CodeVersion(latest_version));
        }
    }
}

fn hex_distance(a: &Position, b: &Position) -> u32 {
    let dx = (a.x - b.x).unsigned_abs();
    let dy = (a.y - b.y).unsigned_abs();
    let dz = ((a.x + a.y) - (b.x + b.y)).unsigned_abs();
    (dx + dy + dz) / 2
}

pub struct SwarmWorld {
    pub app: App,
}

impl SwarmWorld {
    pub fn run_tick(&mut self) {
        self.app.update();
        self.app.world_mut().resource_mut::<CurrentTick>().0 += 1;
    }

    pub fn submit_intent(
        &mut self,
        player_id: PlayerId,
        tick: Tick,
        source: CommandSource,
        intent: CommandIntent,
    ) -> CommandResult {
        let raw = source_gate(player_id, tick, source, intent)?;
        self.submit_raw_command(raw)
    }

    pub fn submit_raw_command(&mut self, raw: RawCommand) -> CommandResult {
        self.app.world_mut().resource_mut::<CurrentTick>().0 = raw.tick;
        let validated = validate_command(self.app.world_mut(), raw)?;
        apply_command(self.app.world_mut(), validated)
    }

    pub fn spawn_drone(&mut self, owner: PlayerId, x: i32, y: i32, body: Vec<BodyPart>) -> Entity {
        self.spawn_drone_in_room(owner, RoomId(0), x, y, body)
    }

    pub fn spawn_drone_in_room(
        &mut self,
        owner: PlayerId,
        room: RoomId,
        x: i32,
        y: i32,
        body: Vec<BodyPart>,
    ) -> Entity {
        self.ensure_room(room);
        let position = Position { x, y, room };
        let registry = self.app.world().resource::<BodyPartRegistry>().clone();
        let config = self.app.world().resource::<WorldConfig>().drone.clone();
        let body = body
            .into_iter()
            .take(config.max_body_parts)
            .collect::<Vec<_>>();
        let mut drone = Drone::new_with_lifespan(owner, body, &registry, config.lifespan);
        drone.lifespan = drone.lifespan.max(config.min_lifespan);
        let entity = self
            .app
            .world_mut()
            .spawn((
                position,
                Owner(owner),
                drone,
                SpawningGrace { remaining: 1 },
            ))
            .id();
        let mut counts = self.app.world_mut().resource_mut::<RoomDroneCounts>();
        *counts.0.entry((position.room, owner)).or_default() += 1;
        send_onboarding_event(self.app.world_mut(), OnboardingEvent::DroneSpawned);
        entity
    }

    pub fn ensure_room(&mut self, room: RoomId) -> bool {
        if self
            .app
            .world()
            .resource::<RoomTerrains>()
            .0
            .contains_key(&room)
        {
            return false;
        }
        let terrain = RoomTerrain::default_room();
        self.app
            .world_mut()
            .resource_mut::<RoomTerrains>()
            .0
            .insert(room, terrain.clone());
        for (x, y, terrain_type) in terrain.iter() {
            self.app
                .world_mut()
                .spawn((Position { x, y, room }, Terrain(terrain_type)));
        }
        true
    }

    pub fn queue_spawn(&mut self, owner: PlayerId, x: i32, y: i32, body: Vec<BodyPart>) {
        self.app
            .world_mut()
            .resource_mut::<PendingSpawnQueue>()
            .0
            .push(PendingSpawn {
                owner,
                body,
                position: Position {
                    x,
                    y,
                    room: RoomId(0),
                },
            });
    }

    pub fn get_terrain(&self, room: RoomId, x: i32, y: i32) -> Option<TerrainType> {
        self.app
            .world()
            .resource::<RoomTerrains>()
            .get_terrain(Position { x, y, room })
    }

    pub fn zone_for_room(&self, room: RoomId) -> DifficultyZone {
        zone_for_room(room, &self.app.world().resource::<WorldConfig>().pve)
    }

    pub fn zone_definition_for_room(&self, room: RoomId) -> ZoneDefinition {
        zone_definition_for_room(room, &self.app.world().resource::<WorldConfig>().pve).clone()
    }

    pub fn set_terrain(&mut self, room: RoomId, x: i32, y: i32, terrain: TerrainType) -> bool {
        let position = Position { x, y, room };
        if !self
            .app
            .world_mut()
            .resource_mut::<RoomTerrains>()
            .set_terrain(position, terrain)
        {
            return false;
        }

        let mut query = self.app.world_mut().query::<(&Position, &mut Terrain)>();
        for (terrain_position, mut terrain_component) in query.iter_mut(self.app.world_mut()) {
            if *terrain_position == position {
                terrain_component.0 = terrain;
                return true;
            }
        }

        self.app.world_mut().spawn((position, Terrain(terrain)));
        true
    }

    pub fn is_passable(&self, room: RoomId, x: i32, y: i32) -> bool {
        self.app
            .world()
            .resource::<RoomTerrains>()
            .is_passable(Position { x, y, room })
    }

    pub fn state_checksum(&mut self) -> u64 {
        state_checksum(self.app.world_mut())
    }

    pub fn set_world_mode(&mut self, mode: WorldMode) {
        self.app.world_mut().resource_mut::<RankingState>().mode = mode;
    }

    pub fn rankings(&self) -> &RankingState {
        self.app.world().resource::<RankingState>()
    }

    pub fn rankings_mut(&mut self) -> Mut<'_, RankingState> {
        self.app.world_mut().resource_mut::<RankingState>()
    }

    pub fn record_arena_match(
        &mut self,
        tick: Tick,
        player_one: PlayerId,
        player_two: PlayerId,
        outcome: MatchOutcome,
    ) -> Option<(LeaderboardEntry, LeaderboardEntry)> {
        let result = self
            .app
            .world_mut()
            .resource_mut::<RankingState>()
            .record_match(tick, player_one, player_two, outcome);
        if result.is_some() {
            send_onboarding_event(self.app.world_mut(), OnboardingEvent::ArenaCompleted);
        }
        result
    }

    pub fn record_replay_completed(&mut self) {
        send_onboarding_event(self.app.world_mut(), OnboardingEvent::ReplayCompleted);
    }

    pub fn record_arena_completed(&mut self) {
        send_onboarding_event(self.app.world_mut(), OnboardingEvent::ArenaCompleted);
    }

    pub fn leaderboard(&self) -> Vec<LeaderboardEntry> {
        self.app.world().resource::<RankingState>().leaderboard()
    }
}

pub fn create_world() -> SwarmWorld {
    create_world_with_mode(WorldMode::Default)
}

pub fn create_sharded_world(shard_count: u32) -> Result<MultiShardWorld, ShardConfigError> {
    MultiShardWorld::new(shard_count)
}

pub fn create_world_with_shard_config(config: ShardConfig) -> SwarmWorld {
    let mut world = create_world_with_mode(WorldMode::Default);
    world.app.insert_resource(config);
    world
}

pub fn create_world_with_mode(mode: WorldMode) -> SwarmWorld {
    create_world_with_mode_and_config(mode, WorldConfig::load_or_default("world.toml"))
}

pub fn create_world_with_mode_and_config(mode: WorldMode, config: WorldConfig) -> SwarmWorld {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.init_resource::<PendingSpawnQueue>();
    app.init_resource::<RoomDroneCounts>();
    app.init_resource::<PendingCombat>();
    app.init_resource::<PendingControllerUpgrade>();
    app.init_resource::<ResourceRegistry>();
    app.init_resource::<GlobalStorageConfig>();
    app.init_resource::<PlayerLocalStorage>();
    app.init_resource::<PlayerGlobalStorage>();
    app.init_resource::<PendingGlobalTransfers>();
    app.init_resource::<PveOutputTracker>();
    app.init_resource::<CurrentTick>();
    app.init_resource::<crate::resources::MarketConfig>();
    app.init_resource::<MarketOrders>();
    app.init_resource::<RhaiRuleModules>();
    app.init_resource::<FoundationDbStore>();
    app.init_resource::<DragonflyCache>();
    app.init_resource::<RankingState>();
    app.init_resource::<ShardConfig>();
    app.init_resource::<SeedRotationState>();
    app.init_resource::<RoomStates>();
    app.init_resource::<EventState>();
    app.init_resource::<PendingRoomClaims>();
    app.insert_resource(OnboardingConfig::for_mode(mode));
    app.init_resource::<OnboardingProgress>();
    app.add_event::<OnboardingEvent>();
    app.add_event::<OnboardingSwarmEvent>();

    let namespace = match mode {
        WorldMode::Default => "default".to_string(),
        WorldMode::Tutorial => format!(
            "tutorial_{}",
            NEXT_TUTORIAL_WORLD_ID.fetch_add(1, Ordering::Relaxed)
        ),
        WorldMode::Arena => "arena".to_string(),
    };
    let mut settings = WorldSettings::new(mode, namespace.clone());
    if mode != WorldMode::Tutorial {
        settings.tick_interval_ms = config.world.tick_interval_ms;
    }
    app.insert_resource(settings);
    app.world_mut().resource_mut::<RankingState>().mode = mode;
    app.world_mut()
        .resource_mut::<GlobalStorageConfig>()
        .namespace = namespace;

    config.install_resources(&mut app);
    config.register_systems(&mut app);

    if mode == WorldMode::Tutorial {
        let mut registry = app.world_mut().resource_mut::<ResourceRegistry>();
        if let Some(source) = registry.sources.get_mut("EnergyField") {
            source.capacity *= 10;
            source.regeneration = (source.regeneration / 10).max(1);
        }
    }

    let room = RoomId(0);
    let mut terrains = RoomTerrains::default();
    terrains.0.insert(room, RoomTerrain::default_room());
    app.insert_resource(terrains.clone());

    for (x, y, terrain) in terrains.0[&room].iter() {
        app.world_mut()
            .spawn((Position { x, y, room }, Terrain(terrain)));
    }

    let source_def = app
        .world()
        .resource::<ResourceRegistry>()
        .source("EnergyField")
        .cloned()
        .expect("default ResourceRegistry must define EnergyField");
    app.world_mut().spawn((
        Position { x: 25, y: 25, room },
        Source {
            produces: source_def.produces,
            capacity: source_def.capacity,
            ticks_to_regeneration: source_def.regeneration,
        },
    ));

    let mut world = SwarmWorld { app };
    run_init_scripts(world.app.world_mut());
    world
}

/// Compute a deterministic, stable checksum over the full world state.
///
/// The checksum is built from a canonical byte stream of tracked ECS components:
/// each component group is tagged, each entity/component row is sorted into a
/// deterministic order, and the resulting stream is hashed with BLAKE3. The
/// first eight digest bytes are returned as a little-endian `u64` for compact
/// tick traces and replay comparisons.
pub fn state_checksum(world: &mut World) -> u64 {
    fn hash_bytes(hasher: &mut blake3::Hasher, bytes: &[u8]) {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }

    fn tag(hasher: &mut blake3::Hasher, name: &str) {
        hash_bytes(hasher, name.as_bytes());
    }

    fn opt_u32_bytes(value: Option<u32>) -> [u8; 8] {
        match value {
            Some(value) => {
                let mut bytes = [0_u8; 8];
                bytes[0] = 1;
                bytes[4..].copy_from_slice(&value.to_le_bytes());
                bytes
            }
            None => [0_u8; 8],
        }
    }

    let mut hasher = blake3::Hasher::new();

    // --- Terrain ---
    tag(&mut hasher, "terrain");
    let mut terrain = world
        .query::<(&Position, &Terrain)>()
        .iter(world)
        .map(|(p, t)| (p.room.0, p.x, p.y, t.0 as u8))
        .collect::<Vec<_>>();
    terrain.sort_unstable();
    for (room, x, y, t) in &terrain {
        hasher.update(&room.to_le_bytes());
        hasher.update(&x.to_le_bytes());
        hasher.update(&y.to_le_bytes());
        hasher.update(&[*t]);
    }

    // --- Sources ---
    tag(&mut hasher, "sources");
    let mut sources = world
        .query::<(&Position, &Source)>()
        .iter(world)
        .map(|(p, s)| {
            // Flatten produces into sorted vec for determinism.
            let mut produces: Vec<(String, u32)> =
                s.produces.iter().map(|(k, v)| (k.clone(), *v)).collect();
            produces.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            (
                p.room.0,
                p.x,
                p.y,
                produces,
                s.capacity,
                s.ticks_to_regeneration,
            )
        })
        .collect::<Vec<_>>();
    sources.sort_unstable_by_key(|(room, x, y, _, capacity, regen)| {
        (*room, *x, *y, *capacity, *regen)
    });
    for (room, x, y, produces, capacity, regen) in &sources {
        hasher.update(&room.to_le_bytes());
        hasher.update(&x.to_le_bytes());
        hasher.update(&y.to_le_bytes());
        for (k, v) in produces {
            hash_bytes(&mut hasher, k.as_bytes());
            hasher.update(&v.to_le_bytes());
        }
        hasher.update(&capacity.to_le_bytes());
        hasher.update(&regen.to_le_bytes());
    }

    // --- Drones ---
    tag(&mut hasher, "drones");
    let mut drones = world
        .query::<(&Position, &Drone)>()
        .iter(world)
        .map(|(p, d)| {
            let body_bytes: Vec<u8> = d.body.iter().map(|b| *b as u8).collect();
            (
                p.room.0,
                p.x,
                p.y,
                d.owner,
                body_bytes,
                d.carry
                    .iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .collect::<Vec<_>>(),
                d.carry_capacity,
                d.fatigue,
                d.hits,
                d.hits_max,
                d.spawning as u8,
                d.age,
                d.lifespan,
            )
        })
        .collect::<Vec<_>>();
    drones.sort_unstable_by_key(
        |(
            room,
            x,
            y,
            owner,
            _,
            _,
            carry_capacity,
            fatigue,
            hits,
            hits_max,
            spawning,
            age,
            lifespan,
        )| {
            (
                *room,
                *x,
                *y,
                *owner,
                *carry_capacity,
                *fatigue,
                *hits,
                *hits_max,
                *spawning,
                *age,
                *lifespan,
            )
        },
    );
    for (
        room,
        x,
        y,
        owner,
        body,
        carry,
        carry_capacity,
        fatigue,
        hits,
        hits_max,
        spawning,
        age,
        lifespan,
    ) in &drones
    {
        hasher.update(&room.to_le_bytes());
        hasher.update(&x.to_le_bytes());
        hasher.update(&y.to_le_bytes());
        hasher.update(&owner.to_le_bytes());
        hash_bytes(&mut hasher, body);
        for (name, amount) in carry {
            hash_bytes(&mut hasher, name.as_bytes());
            hasher.update(&amount.to_le_bytes());
        }
        hasher.update(&carry_capacity.to_le_bytes());
        hasher.update(&fatigue.to_le_bytes());
        hasher.update(&hits.to_le_bytes());
        hasher.update(&hits_max.to_le_bytes());
        hasher.update(&[*spawning]);
        hasher.update(&age.to_le_bytes());
        hasher.update(&lifespan.to_le_bytes());
    }

    // --- Structures ---
    tag(&mut hasher, "structures");
    let mut structures = world
        .query::<(&Position, &Structure)>()
        .iter(world)
        .map(|(p, s)| {
            (
                p.room.0,
                p.x,
                p.y,
                s.structure_type.as_str().to_string(),
                s.owner.unwrap_or(u32::MAX),
                s.hits,
                s.hits_max,
                s.energy,
                s.energy_capacity,
                s.cooldown,
            )
        })
        .collect::<Vec<_>>();
    structures.sort_unstable_by_key(
        |(room, x, y, structure_type, owner, hits, hits_max, energy, capacity, cooldown)| {
            (
                *room,
                *x,
                *y,
                structure_type.clone(),
                *owner,
                *hits,
                *hits_max,
                *energy,
                *capacity,
                *cooldown,
            )
        },
    );
    for (room, x, y, structure_type, owner, hits, hits_max, energy, capacity, cooldown) in
        &structures
    {
        hasher.update(&room.to_le_bytes());
        hasher.update(&x.to_le_bytes());
        hasher.update(&y.to_le_bytes());
        hash_bytes(&mut hasher, structure_type.as_bytes());
        hasher.update(&owner.to_le_bytes());
        hasher.update(&hits.to_le_bytes());
        hasher.update(&hits_max.to_le_bytes());
        hasher.update(&opt_u32_bytes(*energy));
        hasher.update(&opt_u32_bytes(*capacity));
        hasher.update(&cooldown.to_le_bytes());
    }

    // --- Controllers ---
    tag(&mut hasher, "controllers");
    let mut controllers = world
        .query::<(&Position, &Controller)>()
        .iter(world)
        .map(|(p, c)| {
            (
                p.room.0,
                p.x,
                p.y,
                c.owner.unwrap_or(u32::MAX),
                c.level,
                c.progress,
                c.progress_total,
                c.downgrade_timer,
                c.safe_mode,
                c.safe_mode_available,
                c.safe_mode_cooldown,
            )
        })
        .collect::<Vec<_>>();
    controllers.sort_unstable();
    for (
        room,
        x,
        y,
        owner,
        level,
        progress,
        progress_total,
        downgrade_timer,
        safe_mode,
        safe_mode_available,
        safe_mode_cooldown,
    ) in &controllers
    {
        hasher.update(&room.to_le_bytes());
        hasher.update(&x.to_le_bytes());
        hasher.update(&y.to_le_bytes());
        hasher.update(&owner.to_le_bytes());
        hasher.update(&[*level]);
        hasher.update(&progress.to_le_bytes());
        hasher.update(&progress_total.to_le_bytes());
        hasher.update(&downgrade_timer.to_le_bytes());
        hasher.update(&safe_mode.to_le_bytes());
        hasher.update(&safe_mode_available.to_le_bytes());
        hasher.update(&safe_mode_cooldown.to_le_bytes());
    }

    // --- Dropped resources ---
    tag(&mut hasher, "resources");
    let mut resources = world
        .query::<(&Position, &crate::components::Resource)>()
        .iter(world)
        .map(|(p, r)| {
            let mut amounts: Vec<(String, u32)> =
                r.amounts.iter().map(|(k, v)| (k.clone(), *v)).collect();
            amounts.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            (p.room.0, p.x, p.y, amounts)
        })
        .collect::<Vec<_>>();
    resources.sort_unstable_by_key(|(room, x, y, _)| (*room, *x, *y));
    for (room, x, y, amounts) in &resources {
        hasher.update(&room.to_le_bytes());
        hasher.update(&x.to_le_bytes());
        hasher.update(&y.to_le_bytes());
        for (name, amount) in amounts {
            hash_bytes(&mut hasher, name.as_bytes());
            hasher.update(&amount.to_le_bytes());
        }
    }

    tag(&mut hasher, "player_local_storage");
    hash_player_storage(&mut hasher, &world.resource::<PlayerLocalStorage>().0);

    tag(&mut hasher, "player_global_storage");
    hash_player_storage(&mut hasher, &world.resource::<PlayerGlobalStorage>().0);

    tag(&mut hasher, "pending_global_transfers");
    let mut transfers = world.resource::<PendingGlobalTransfers>().0.clone();
    transfers.sort_by_key(|transfer| {
        (
            transfer.player_id,
            transfer.direction as u8,
            transfer.resource.clone(),
            transfer.amount,
            transfer.deliver_amount,
            transfer.remaining_ticks,
            transfer.start.room,
            transfer.start.x,
            transfer.start.y,
            transfer.end.room,
            transfer.end.x,
            transfer.end.y,
        )
    });
    for transfer in transfers {
        hasher.update(&transfer.player_id.to_le_bytes());
        hasher.update(&[transfer.direction as u8]);
        hash_bytes(&mut hasher, transfer.resource.as_bytes());
        hasher.update(&transfer.amount.to_le_bytes());
        hasher.update(&transfer.deliver_amount.to_le_bytes());
        hasher.update(&transfer.remaining_ticks.to_le_bytes());
        hash_position(&mut hasher, transfer.start);
        hash_position(&mut hasher, transfer.end);
    }

    tag(&mut hasher, "market_orders");
    let mut orders = world
        .resource::<MarketOrders>()
        .orders
        .values()
        .cloned()
        .collect::<Vec<_>>();
    orders.sort_by_key(|order| order.id);
    for order in orders {
        hasher.update(&order.id.to_le_bytes());
        hasher.update(&order.seller.to_le_bytes());
        hash_bytes(&mut hasher, order.resource.as_bytes());
        hasher.update(&order.amount.to_le_bytes());
        hash_bytes(&mut hasher, order.price_resource.as_bytes());
        hasher.update(&order.price_amount.to_le_bytes());
    }

    let digest = hasher.finalize();
    u64::from_le_bytes(
        digest.as_bytes()[..8]
            .try_into()
            .expect("BLAKE3 digest has 32 bytes"),
    )
}

fn hash_position(hasher: &mut blake3::Hasher, position: Position) {
    hasher.update(&position.room.0.to_le_bytes());
    hasher.update(&position.x.to_le_bytes());
    hasher.update(&position.y.to_le_bytes());
}

fn hash_player_storage(
    hasher: &mut blake3::Hasher,
    storage: &indexmap::IndexMap<PlayerId, indexmap::IndexMap<String, u32>>,
) {
    let mut rows = storage
        .iter()
        .map(|(player_id, amounts)| {
            let mut amounts = amounts
                .iter()
                .map(|(name, amount)| (name.clone(), *amount))
                .collect::<Vec<_>>();
            amounts.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            (*player_id, amounts)
        })
        .collect::<Vec<_>>();
    rows.sort_unstable_by_key(|(player_id, _)| *player_id);

    for (player_id, amounts) in rows {
        hasher.update(&player_id.to_le_bytes());
        for (name, amount) in amounts {
            hasher.update(&(name.len() as u64).to_le_bytes());
            hasher.update(name.as_bytes());
            hasher.update(&amount.to_le_bytes());
        }
    }
}

#[cfg(test)]
mod shard_tests {
    use super::*;
    use crate::command::{CommandAction, CommandAuth};
    use crate::realtime::InMemoryNats;

    #[test]
    fn world_config_defaults_preserve_existing_rules() {
        let config = WorldConfig::default();
        assert_eq!(config.spawn.policy, SpawnPolicy::RandomRoom);
        assert_eq!(config.spawn.cooldown, 0);
        assert_eq!(config.spawn.safe_mode_duration, 500);
        assert_eq!(config.spawn.respawn_policy, RespawnPolicy::NewRoom);
        assert_eq!(config.code.update_cooldown, 5);
        assert_eq!(config.code.propagation_speed, 0);
        assert!(config.drone.env_vars);
        assert_eq!(config.drone.memory_size, 1024);
        assert_eq!(config.drone.lifespan, DEFAULT_DRONE_LIFESPAN);
        assert_eq!(config.drone.min_lifespan, MIN_LIFESPAN);
        assert_eq!(config.drone.max_body_parts, 50);
        assert_eq!(config.drone.max_drones_per_player, 500);
        assert!(config.combat.pvp_enabled);
        assert!(!config.combat.friendly_fire);
        assert_eq!(config.resources.source_regeneration_rate, 10_000);
        assert_eq!(
            config.resources.max_pve_output_per_tick,
            crate::resources::DEFAULT_MAX_PVE_OUTPUT_PER_TICK
        );
        assert!(config.visibility.fog_of_war);
        assert_eq!(config.visibility.player_view, PlayerViewMode::Drone);
        assert!(!config.visibility.public_spectate);
        assert_eq!(config.visibility.spectate_delay, 0);
        assert_eq!(config.visibility.replay_privacy, ReplayPrivacy::Private);
        assert!(!config.propagation_system_enabled());
        assert_eq!(config.damage_types.len(), 5);
        assert_eq!(
            config
                .damage_types
                .iter()
                .find(|damage_type| damage_type.name == "Kinetic")
                .and_then(|damage_type| damage_type.component_multipliers.get("Tough"))
                .copied(),
            Some(0.5)
        );
        assert_eq!(
            config
                .damage_types
                .iter()
                .find(|damage_type| damage_type.name == "EMP")
                .and_then(|damage_type| damage_type.component_multipliers.get("Claim"))
                .copied(),
            Some(1.3)
        );
        assert_eq!(config.special_effects.len(), 11);
        assert_eq!(config.custom_actions.len(), 8);
        assert_eq!(
            config
                .special_effects
                .iter()
                .find(|effect| effect.name == "hack")
                .map(|effect| effect.handler.as_str()),
            Some("hack")
        );
        assert_eq!(
            config
                .custom_actions
                .iter()
                .find(|action| action.name == "Disrupt")
                .and_then(|action| action.special_effect.as_deref()),
            Some("disrupt")
        );
        assert_eq!(
            config
                .special_effects
                .iter()
                .find(|effect| effect.name == "fortify")
                .map(|effect| effect.duration),
            Some(3)
        );
    }

    #[test]
    fn default_custom_actions_register_tier_one_special_attacks() {
        let config = WorldConfig::default();
        let action = |name: &str| {
            config
                .custom_actions
                .iter()
                .find(|action| action.name == name)
                .unwrap()
        };

        assert_eq!(action("Hack").special_effect.as_deref(), Some("hack"));
        assert_eq!(action("Hack").damage_type.as_deref(), Some("Psionic"));
        assert_eq!(action("Hack").range, 1);
        assert_eq!(action("Hack").cooldown, Some(200));
        assert_eq!(action("Hack").cost.get("Energy"), Some(&1000));

        assert_eq!(action("Drain").special_effect.as_deref(), Some("drain"));
        assert_eq!(action("Drain").damage_type.as_deref(), Some("EMP"));
        assert_eq!(action("Drain").range, 1);
        assert_eq!(action("Drain").cooldown, Some(50));
        assert_eq!(action("Drain").cost.get("Energy"), Some(&200));

        assert_eq!(
            action("Overload").special_effect.as_deref(),
            Some("overload")
        );
        assert_eq!(action("Overload").damage_type.as_deref(), Some("EMP"));
        assert_eq!(action("Overload").range, 1);
        assert_eq!(action("Overload").cooldown, Some(200));
        assert_eq!(action("Overload").cost.get("Energy"), Some(&300));
        assert_eq!(action("Overload").special_param, Some(500_000.0));

        assert_eq!(action("Disrupt").special_effect.as_deref(), Some("disrupt"));
        assert_eq!(action("Disrupt").damage_type.as_deref(), Some("Sonic"));
        assert_eq!(action("Disrupt").range, 1);
        assert_eq!(action("Disrupt").cooldown, Some(50));
        assert_eq!(action("Disrupt").cost.get("Energy"), Some(&100));
    }

    #[test]
    fn world_config_parses_world_toml_rules() {
        let config = WorldConfig::from_toml_str(
            r#"
[world]
tick_interval_ms = 1500
[spawn]
policy = "ManualSelect"
respawn = "OriginalRoom"
cooldown = 12
safe_mode_duration = 42
[code]
update_cost = { Energy = 500 }
update_cooldown = 7
update_window = { every = 100, duration = 10 }
propagation_speed = 3
propagation_source = "Controller"
[drone]
env_vars = false
memory_size = 4096
memory_spawn_cost = { Energy = 1 }
memory_upkeep_cost = { Energy = 2 }
lifespan = 2000
min_lifespan = 250
max_body_parts = 10
max_drones_per_player = 25
[visibility]
fog_of_war = false
player_view = "full"
public_spectate = true
spectate_delay = 100
replay_privacy = "public"
[resources]
source_regeneration_rate = 9000
build_cost_multiplier = 11000
drone_decay_rate = 12000
max_pve_output_per_tick = 1234
[combat]
pvp_enabled = false
friendly_fire = true
damage_multiplier = 1.5
"#,
        )
        .unwrap();
        assert_eq!(config.world.tick_interval_ms, 1500);
        assert_eq!(config.spawn.policy, SpawnPolicy::ManualSelect);
        assert_eq!(config.spawn.respawn_policy, RespawnPolicy::OriginalRoom);
        assert_eq!(config.spawn.safe_mode_duration, 42);
        assert_eq!(config.code.update_cost.get("Energy"), Some(&500));
        assert_eq!(config.code.update_window.duration, 10);
        assert_eq!(config.drone.memory_upkeep_cost.get("Energy"), Some(&2));
        assert_eq!(config.drone.lifespan, 2000);
        assert_eq!(config.drone.min_lifespan, 250);
        assert_eq!(config.drone.max_body_parts, 10);
        assert_eq!(config.drone.max_drones_per_player, 25);
        assert!(!config.visibility.fog_of_war);
        assert_eq!(config.visibility.player_view, PlayerViewMode::Full);
        assert!(config.visibility.public_spectate);
        assert_eq!(config.visibility.spectate_delay, 100);
        assert_eq!(config.visibility.replay_privacy, ReplayPrivacy::Public);
        assert_eq!(config.resources.max_pve_output_per_tick, 1234);
        assert!(!config.combat.pvp_enabled);
        assert!(config.combat.friendly_fire);
        assert_eq!(config.combat_damage_multiplier_fixed(), 15_000);
        assert!(config.propagation_system_enabled());
    }

    #[test]
    fn world_config_parses_damage_type_component_multipliers() {
        let config = WorldConfig::from_toml_str(
            r#"
[[damage_types]]
name = "Acid"
component_multipliers = { Tough = 0.25, Work = 1.5 }
attribute_multipliers = { Shielded = 0.75 }
"#,
        )
        .unwrap();
        let registry = DamageTypeRegistry::from_defs(config.damage_types);
        assert_eq!(
            registry.component_multiplier("Acid", Some(&[BodyPart::Tough, BodyPart::Work])),
            0.375
        );
        assert_eq!(
            registry.attribute_multiplier("Acid", Some(&Attributes(vec!["Shielded".to_string()]))),
            0.75
        );
    }

    #[test]
    fn damage_type_component_multipliers_affect_combat_damage() {
        let mut world = create_world();
        let target = world.spawn_drone(1, 0, 0, vec![BodyPart::Tough]);
        world
            .app
            .world_mut()
            .entity_mut(target)
            .remove::<SpawningGrace>();
        world
            .app
            .world_mut()
            .resource_mut::<PendingCombat>()
            .queue_typed_damage(target, "Kinetic", 100);
        world.run_tick();
        let hits = world.app.world().get::<Drone>(target).unwrap().hits;
        assert_eq!(hits, 75);
    }

    #[test]
    fn world_config_parses_resource_and_source_types() {
        let config = WorldConfig::from_toml_str(
            r#"
[[resource_types]]
name = "Mineral"
display_name = "Mineral"
category = "mineral"
starting_amount = 0
max_storage = 50000
decay_rate = 0.0
tradeable = true

[[source_types]]
name = "MineralVein"
produces = { Mineral = 2 }
capacity = 1000
regeneration = 100
"#,
        )
        .unwrap();
        let world = create_world_with_mode_and_config(WorldMode::Default, config);
        let registry = world.app.world().resource::<ResourceRegistry>();

        assert_eq!(registry.resource("Mineral").unwrap().max_storage, 50_000);
        assert_eq!(registry.source("MineralVein").unwrap().capacity, 1_000);
    }

    #[test]
    fn code_propagation_source_accepts_global_alias() {
        let config = WorldConfig::from_toml_str(
            r#"
[code]
propagation_speed = 1
propagation_source = "AnyDrone"
"#,
        )
        .unwrap();

        assert_eq!(
            config.code.propagation_source,
            CodePropagationSource::AnyDrone
        );
    }

    #[test]
    fn spawn_drone_uses_configured_lifespan_and_body_limit() {
        let config = WorldConfig::from_toml_str(
            r#"
[drone]
lifespan = 2000
max_body_parts = 1
"#,
        )
        .unwrap();
        let mut world = create_world_with_mode_and_config(WorldMode::Default, config);
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Tough, BodyPart::Attack]);
        let drone = world.app.world().entity(drone).get::<Drone>().unwrap();

        assert_eq!(drone.body, vec![BodyPart::Tough]);
        assert_eq!(drone.lifespan, 2_100);
    }

    #[test]
    fn spawn_drone_applies_configured_min_lifespan_floor() {
        let config = WorldConfig::from_toml_str(
            r#"
[drone]
lifespan = 50
min_lifespan = 120
"#,
        )
        .unwrap();
        let mut world = create_world_with_mode_and_config(WorldMode::Default, config);
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Attack]);
        let drone = world.app.world().entity(drone).get::<Drone>().unwrap();

        assert_eq!(drone.lifespan, 120);
    }

    #[test]
    fn world_config_parses_special_effect_registry() {
        let config = WorldConfig::from_toml_str(
            r#"
[[special_effects]]
name = "disable_shields"
description = "Alias fortify handler for parser coverage"
handler = "fortify"
target = "self_or_ally"
duration = 10
resistance = "EMP"

[[custom_actions]]
name = "ShieldPulse"
description = "Custom action referencing a configured effect"
range = 2
special_effect = "disable_shields"
cooldown = 12
cost = { Energy = 33 }
"#,
        )
        .unwrap();

        assert_eq!(config.special_effects.len(), 1);
        assert_eq!(config.special_effects[0].handler, "fortify");
        assert_eq!(config.special_effects[0].resistance.as_deref(), Some("EMP"));
        assert_eq!(config.custom_actions.len(), 1);
        assert_eq!(
            config.custom_actions[0].special_effect.as_deref(),
            Some("disable_shields")
        );
    }

    #[test]
    fn create_world_with_config_installs_resources() {
        let config = WorldConfig::from_toml_str(
            "[combat]\ndamage_multiplier = 0.5\n[world]\ntick_interval_ms = 2500\n",
        )
        .unwrap();
        let world = create_world_with_mode_and_config(WorldMode::Default, config);
        assert_eq!(
            world
                .app
                .world()
                .resource::<WorldSettings>()
                .tick_interval_ms,
            2500
        );
        assert_eq!(
            world
                .app
                .world()
                .resource::<CombatRules>()
                .damage_multiplier,
            5_000
        );
        assert_eq!(
            world
                .app
                .world()
                .resource::<WorldConfig>()
                .code
                .update_cooldown,
            5
        );
        assert!(
            world
                .app
                .world()
                .resource::<SpecialEffectRegistry>()
                .get("hack")
                .is_some()
        );
        assert!(
            world
                .app
                .world()
                .resource::<CustomActionRegistry>()
                .get("Hack")
                .is_some()
        );
    }

    fn test_command(player_id: PlayerId) -> RawCommand {
        RawCommand {
            player_id,
            tick: 7,
            source: CommandSource::TestHarness,
            auth: CommandAuth {
                source: CommandSource::TestHarness,
                player_id,
                tick_submitted: 7,
                tick_target: 7,
            },
            sequence: 1,
            action: CommandAction::TransferToGlobal {
                resource: "Energy".to_string(),
                amount: 1,
            },
        }
    }

    #[test]
    fn shard_key_uses_deterministic_player_hash() {
        let first = ShardKey::for_player(42, 4);
        let second = ShardKey::for_player(42, 4);
        let other = ShardKey::for_player(43, 4);

        assert_eq!(first, second);
        assert_eq!(first.player_id, 42);
        assert!(first.shard_id.0 < 4);
        assert!(other.shard_id.0 < 4);

        let config = ShardConfig::new(4, first.shard_id).unwrap();
        assert!(config.owns_player(42));
        assert_eq!(config.shard_for_player(42), first.shard_id);
    }

    #[test]
    fn remote_shard_command_publishes_to_nats_subject() {
        let remote_player = (1..=100)
            .find(|player_id| shard_for_player(*player_id, 2) == ShardId(1))
            .expect("test setup should find a player routed to shard 1");
        let registry = ShardRegistry::new(ShardConfig::new(2, ShardId(0)).unwrap()).unwrap();
        let mut router = ShardRouter::new(registry, InMemoryNats::default());

        let routed = router
            .route_raw_command(test_command(remote_player))
            .unwrap();
        assert_eq!(
            routed,
            RoutedCommand::Published {
                subject: "swarm.shard.1.commands".to_string()
            }
        );

        let nats = router.into_publisher();
        assert_eq!(nats.messages.len(), 1);
        assert_eq!(nats.messages[0].0, "swarm.shard.1.commands");
        let envelope: ShardEnvelope = serde_json::from_slice(&nats.messages[0].1).unwrap();
        assert_eq!(envelope.source_shard, ShardId(0));
        assert_eq!(envelope.target_shard, ShardId(1));
        assert_eq!(envelope.command.player_id, remote_player);
    }

    #[test]
    fn multi_shard_tick_checksum_is_consistent() {
        let mut first = create_sharded_world(3).unwrap();
        let mut second = create_sharded_world(3).unwrap();

        assert_eq!(first.state_checksum(), second.state_checksum());
        for player_id in 1..=12 {
            let shard_id = first.shard_for_player(player_id);
            first.shard_mut(shard_id).unwrap().queue_spawn(
                player_id,
                player_id as i32,
                10,
                vec![BodyPart::Move],
            );
            second.shard_mut(shard_id).unwrap().queue_spawn(
                player_id,
                player_id as i32,
                10,
                vec![BodyPart::Move],
            );
        }

        first.run_tick();
        second.run_tick();
        assert_eq!(first.state_checksum(), second.state_checksum());

        let before = first.state_checksum();
        let routed = (1..=12)
            .map(|player_id| first.shard_for_player(player_id))
            .collect::<std::collections::BTreeSet<_>>();
        assert!(routed.len() > 1);
        first.run_tick();
        assert_ne!(before, first.state_checksum());
    }
}
