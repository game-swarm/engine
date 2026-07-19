use bevy::prelude::Resource as BevyResource;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use swarm_engine_api::ids::{PlayerId, RoomId};

use crate::command::Tick;

const BASIS_POINTS_DENOMINATOR: u64 = 10_000;
const DEFAULT_GLOBAL_REGEN_LIMIT_BP: u32 = 3_000;
const DEFAULT_ZONE_REGEN_LIMIT_BP: u32 = 5_000;
const DEFAULT_PLAYER_CONTROLLER_LEVEL_LIMIT: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NpcType {
    Creep,
    Guardian,
    Merchant,
    Swarmling,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DifficultyZone {
    #[default]
    Zone1,
    Zone2,
    Zone3,
    Zone4,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZoneDefinition {
    pub zone: DifficultyZone,
    pub max_distance: u32,
    pub npc_spawn_rate: f64,
    pub hp_multiplier: f64,
    pub drop_bonus: f64,
    pub npc_types: Vec<NpcType>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorldPveConfig {
    pub center_x: i32,
    pub center_y: i32,
    pub zones: Vec<ZoneDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PveBudgetConfig {
    pub global_regeneration_limit_bp: u32,
    pub zone_regeneration_limit_bp: u32,
    pub player_controller_level_limit: u64,
}

#[derive(BevyResource, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PveBudget {
    pub config: PveBudgetConfig,
    pub tick: Tick,
    pub global_allocated: u64,
    pub zone_allocated: IndexMap<DifficultyZone, u64>,
    pub player_allocated: IndexMap<PlayerId, u64>,
    pub event_allocated: IndexMap<String, u64>,
    pub exhausted: Vec<PvEBudgetExhausted>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PveBudgetDimension {
    Global,
    Zone,
    Player,
    Event,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PvEBudgetExhausted {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub zone: DifficultyZone,
    pub event_id: String,
    pub requested: u64,
    pub exhausted_dimensions: Vec<PveBudgetDimension>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PveBudgetRequest<'a> {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub zone: DifficultyZone,
    pub event_id: &'a str,
    pub amount: u64,
    pub world_regeneration_total: u64,
    pub zone_base_regeneration: u64,
    pub player_controller_level: u32,
    pub event_budget_pool: u64,
}

impl Default for ZoneDefinition {
    fn default() -> Self {
        Self {
            zone: DifficultyZone::Zone1,
            max_distance: 10,
            npc_spawn_rate: 1.0,
            hp_multiplier: 1.0,
            drop_bonus: 0.0,
            npc_types: vec![NpcType::Creep],
        }
    }
}

impl Default for WorldPveConfig {
    fn default() -> Self {
        Self {
            center_x: 0,
            center_y: 0,
            zones: vec![
                ZoneDefinition {
                    zone: DifficultyZone::Zone1,
                    max_distance: 10,
                    npc_spawn_rate: 1.0,
                    hp_multiplier: 1.0,
                    drop_bonus: 0.0,
                    npc_types: vec![NpcType::Creep],
                },
                ZoneDefinition {
                    zone: DifficultyZone::Zone2,
                    max_distance: 25,
                    npc_spawn_rate: 2.5,
                    hp_multiplier: 1.5,
                    drop_bonus: 0.25,
                    npc_types: vec![NpcType::Creep, NpcType::Guardian],
                },
                ZoneDefinition {
                    zone: DifficultyZone::Zone3,
                    max_distance: 50,
                    npc_spawn_rate: 4.0,
                    hp_multiplier: 2.0,
                    drop_bonus: 0.5,
                    npc_types: vec![NpcType::Creep, NpcType::Guardian],
                },
                ZoneDefinition {
                    zone: DifficultyZone::Zone4,
                    max_distance: u32::MAX,
                    npc_spawn_rate: 6.0,
                    hp_multiplier: 3.0,
                    drop_bonus: 1.0,
                    npc_types: vec![NpcType::Creep, NpcType::Guardian, NpcType::Swarmling],
                },
            ],
        }
    }
}

impl Default for PveBudgetConfig {
    fn default() -> Self {
        Self {
            global_regeneration_limit_bp: DEFAULT_GLOBAL_REGEN_LIMIT_BP,
            zone_regeneration_limit_bp: DEFAULT_ZONE_REGEN_LIMIT_BP,
            player_controller_level_limit: DEFAULT_PLAYER_CONTROLLER_LEVEL_LIMIT,
        }
    }
}

impl Default for PveBudget {
    fn default() -> Self {
        Self::new(PveBudgetConfig::default())
    }
}

impl PveBudget {
    pub fn new(config: PveBudgetConfig) -> Self {
        Self {
            config,
            tick: 0,
            global_allocated: 0,
            zone_allocated: IndexMap::new(),
            player_allocated: IndexMap::new(),
            event_allocated: IndexMap::new(),
            exhausted: Vec::new(),
        }
    }

    pub fn reset_for_tick(&mut self, tick: Tick) {
        if self.tick != tick {
            self.tick = tick;
            self.global_allocated = 0;
            self.zone_allocated.clear();
            self.player_allocated.clear();
            self.exhausted.clear();
        }
    }
}

pub fn budget_check(
    budget: &PveBudget,
    request: &PveBudgetRequest<'_>,
) -> Result<(), PvEBudgetExhausted> {
    let global_allocated = if budget.tick == request.tick {
        budget.global_allocated
    } else {
        0
    };
    let zone_allocated = if budget.tick == request.tick {
        budget
            .zone_allocated
            .get(&request.zone)
            .copied()
            .unwrap_or(0)
    } else {
        0
    };
    let player_allocated = if budget.tick == request.tick {
        budget
            .player_allocated
            .get(&request.player_id)
            .copied()
            .unwrap_or(0)
    } else {
        0
    };
    let event_allocated = budget
        .event_allocated
        .get(request.event_id)
        .copied()
        .unwrap_or(0);

    let global_limit = percent_limit(
        request.world_regeneration_total,
        budget.config.global_regeneration_limit_bp,
    );
    let zone_limit = percent_limit(
        request.zone_base_regeneration,
        budget.config.zone_regeneration_limit_bp,
    );
    let player_limit = u64::from(request.player_controller_level)
        .saturating_mul(budget.config.player_controller_level_limit);
    let event_limit = request.event_budget_pool;

    let mut exhausted_dimensions = Vec::new();
    if exceeds_limit(global_allocated, request.amount, global_limit) {
        exhausted_dimensions.push(PveBudgetDimension::Global);
    }
    if exceeds_limit(zone_allocated, request.amount, zone_limit) {
        exhausted_dimensions.push(PveBudgetDimension::Zone);
    }
    if exceeds_limit(player_allocated, request.amount, player_limit) {
        exhausted_dimensions.push(PveBudgetDimension::Player);
    }
    if exceeds_limit(event_allocated, request.amount, event_limit) {
        exhausted_dimensions.push(PveBudgetDimension::Event);
    }

    if exhausted_dimensions.is_empty() {
        Ok(())
    } else {
        Err(PvEBudgetExhausted {
            tick: request.tick,
            player_id: request.player_id,
            zone: request.zone,
            event_id: request.event_id.to_string(),
            requested: request.amount,
            exhausted_dimensions,
        })
    }
}

pub fn try_allocate(
    budget: &mut PveBudget,
    request: &PveBudgetRequest<'_>,
) -> Result<(), PvEBudgetExhausted> {
    budget.reset_for_tick(request.tick);
    match budget_check(budget, request) {
        Ok(()) => {
            budget.global_allocated = budget.global_allocated.saturating_add(request.amount);
            let zone_allocated = budget.zone_allocated.entry(request.zone).or_insert(0);
            *zone_allocated = zone_allocated.saturating_add(request.amount);
            let player_allocated = budget
                .player_allocated
                .entry(request.player_id)
                .or_insert(0);
            *player_allocated = player_allocated.saturating_add(request.amount);
            let event_allocated = budget
                .event_allocated
                .entry(request.event_id.to_string())
                .or_insert(0);
            *event_allocated = event_allocated.saturating_add(request.amount);
            Ok(())
        }
        Err(exhausted) => {
            budget.exhausted.push(exhausted.clone());
            Err(exhausted)
        }
    }
}

fn percent_limit(base: u64, basis_points: u32) -> u64 {
    base.saturating_mul(u64::from(basis_points)) / BASIS_POINTS_DENOMINATOR
}

fn exceeds_limit(allocated: u64, amount: u64, limit: u64) -> bool {
    allocated.saturating_add(amount) > limit
}

pub fn room_distance_from_world_center(room: RoomId, config: &WorldPveConfig) -> u32 {
    let (_, x, y) = room.sector_coordinates();
    (x - config.center_x)
        .unsigned_abs()
        .max((y - config.center_y).unsigned_abs())
}

pub fn zone_for_room(room: RoomId, config: &WorldPveConfig) -> DifficultyZone {
    zone_definition_for_room(room, config).zone
}

pub fn zone_definition_for_room(room: RoomId, config: &WorldPveConfig) -> &ZoneDefinition {
    let distance = room_distance_from_world_center(room, config);
    config
        .zones
        .iter()
        .filter(|zone| distance <= zone.max_distance)
        .min_by_key(|zone| zone.max_distance)
        .or_else(|| config.zones.iter().max_by_key(|zone| zone.max_distance))
        .expect("pve zone config must contain at least one zone")
}

pub fn zone_definition(zone: DifficultyZone, config: &WorldPveConfig) -> Option<&ZoneDefinition> {
    config
        .zones
        .iter()
        .find(|definition| definition.zone == zone)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn determines_zone_by_room_distance_from_center() {
        let config = WorldPveConfig::default();

        assert_eq!(
            zone_for_room(RoomId::from_room_name("A0N0E").unwrap(), &config),
            DifficultyZone::Zone1
        );
        assert_eq!(
            zone_for_room(RoomId::from_room_name("A0N11E").unwrap(), &config),
            DifficultyZone::Zone2
        );
        assert_eq!(
            zone_for_room(RoomId::from_room_name("A26N0E").unwrap(), &config),
            DifficultyZone::Zone3
        );
        assert_eq!(
            zone_for_room(RoomId::from_room_name("A51S0E").unwrap(), &config),
            DifficultyZone::Zone4
        );
    }

    #[test]
    fn parses_zone_multipliers_from_config() {
        let config: WorldPveConfig = toml::from_str(
            r#"
center_x = 5
center_y = -5

[[zones]]
zone = "Zone1"
max_distance = 3
npc_spawn_rate = 0.5
hp_multiplier = 1.25
drop_bonus = 0.1
npc_types = ["Creep", "Merchant"]

[[zones]]
zone = "Zone4"
max_distance = 4294967295
npc_spawn_rate = 5.0
hp_multiplier = 4.0
drop_bonus = 2.0
npc_types = ["Guardian", "Swarmling"]
"#,
        )
        .unwrap();

        let zone = zone_definition_for_room(RoomId::from_room_name("A5S8E").unwrap(), &config);
        assert_eq!(zone.zone, DifficultyZone::Zone1);
        assert_eq!(zone.npc_spawn_rate, 0.5);
        assert_eq!(zone.hp_multiplier, 1.25);
        assert_eq!(zone.drop_bonus, 0.1);
        assert_eq!(zone.npc_types, vec![NpcType::Creep, NpcType::Merchant]);
    }

    fn budget_request(amount: u64, tick: Tick) -> PveBudgetRequest<'static> {
        PveBudgetRequest {
            tick,
            player_id: 7,
            zone: DifficultyZone::Zone2,
            event_id: "event-1",
            amount,
            world_regeneration_total: 10_000,
            zone_base_regeneration: 2_000,
            player_controller_level: 2,
            event_budget_pool: 3_000,
        }
    }

    #[test]
    fn pve_budget_allocation_succeeds_within_all_limits() {
        let mut budget = PveBudget::default();

        assert!(try_allocate(&mut budget, &budget_request(1_000, 1)).is_ok());
        assert_eq!(budget.global_allocated, 1_000);
        assert_eq!(budget.zone_allocated[&DifficultyZone::Zone2], 1_000);
        assert_eq!(budget.player_allocated[&7], 1_000);
        assert_eq!(budget.event_allocated["event-1"], 1_000);
        assert!(budget.exhausted.is_empty());
    }

    #[test]
    fn pve_budget_rejects_over_limit_allocation_without_spending() {
        let mut budget = PveBudget::default();

        let exhausted = try_allocate(&mut budget, &budget_request(1_001, 1)).unwrap_err();

        assert_eq!(
            exhausted.exhausted_dimensions,
            vec![PveBudgetDimension::Zone]
        );
        assert_eq!(budget.global_allocated, 0);
        assert!(budget.zone_allocated.is_empty());
        assert_eq!(budget.exhausted, vec![exhausted]);
    }

    #[test]
    fn pve_budget_resets_tick_scoped_limits_per_tick() {
        let mut budget = PveBudget::default();

        assert!(try_allocate(&mut budget, &budget_request(1_000, 1)).is_ok());
        assert!(try_allocate(&mut budget, &budget_request(1_000, 2)).is_ok());

        assert_eq!(budget.tick, 2);
        assert_eq!(budget.global_allocated, 1_000);
        assert_eq!(budget.zone_allocated[&DifficultyZone::Zone2], 1_000);
        assert_eq!(budget.player_allocated[&7], 1_000);
        assert_eq!(budget.event_allocated["event-1"], 2_000);
    }
}
