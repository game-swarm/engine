use std::collections::{BTreeMap, BTreeSet};

use bevy::prelude::Entity;
use serde::{Deserialize, Serialize};

use crate::command::{ObjectId, Tick, object_id};
use crate::components::{Controller, Drone, PlayerId, Position, RoomId, Source};
use crate::mcp::{McpError, StoredModule};
use crate::ranking::RankingState;
use crate::resource_ledger::{compute_continuous_storage_tax, marginal_storage_tax_rate_bp};
use crate::resources::{
    GlobalStorageConfig, PlayerGlobalStorage, PlayerLocalStorage, ResourceCost,
};
use crate::world::SwarmWorld;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EconomyParams {
    #[serde(default)]
    pub player_id: Option<PlayerId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EconomySnapshot {
    pub player_id: PlayerId,
    pub income: ResourceCost,
    pub expenses: ResourceCost,
    pub storage_tax: StorageTaxSummary,
    pub maintenance: ResourceCost,
    pub local_storage: ResourceCost,
    pub global_storage: ResourceCost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageTaxSummary {
    pub stored: u32,
    pub capacity: u32,
    pub utilization_pct: u32,
    pub effective_rate: u32,
    pub estimated_tax_per_tick: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EconomyTrendParams {
    #[serde(default)]
    pub player_id: Option<PlayerId>,
    #[serde(default = "default_trend_ticks")]
    pub ticks: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EconomyTrendResult {
    pub player_id: PlayerId,
    pub trend: Vec<EconomyTrendPoint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EconomyTrendPoint {
    pub tick: Tick,
    pub metric: String,
    pub value: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DroneEfficiencyParams {
    pub drone_id: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroneEfficiencyResult {
    pub drone_id: ObjectId,
    pub owner: PlayerId,
    pub efficiency: u32,
    pub factors: BTreeMap<String, i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaderboardParams {
    #[serde(default = "default_leaderboard_scope")]
    pub scope: String,
    #[serde(default = "default_leaderboard_limit")]
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EconomyLeaderboardResult {
    pub scope: String,
    pub entries: Vec<EconomyLeaderboardEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EconomyLeaderboardEntry {
    pub player: PlayerId,
    pub gcl: u32,
    pub rooms: u32,
    pub drones: u32,
    pub score: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketOrdersParams {
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default = "default_market_limit")]
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketOrdersResult {
    pub orders: Vec<MarketOrder>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketOrder {
    pub order_id: String,
    pub player_id: PlayerId,
    pub resource: String,
    pub order_type: String,
    pub price_micro: u64,
    pub remaining: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SdkFetchParams {
    #[serde(default = "default_sdk_language")]
    pub language: String,
    #[serde(default)]
    pub package: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdkFetchResult {
    pub language: String,
    pub package: String,
    pub version: String,
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployStatusParams {
    pub deploy_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployStatusResult {
    pub deploy_id: String,
    pub status: String,
    pub errors: Vec<String>,
    pub deployed_at: String,
    pub tikv_version_counter: Tick,
    pub object_store_key: String,
    pub module_hash: String,
    pub load_after_tick: Tick,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListDeploymentsParams {
    #[serde(default)]
    pub player_id: Option<PlayerId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListDeploymentsResult {
    pub deployments: Vec<DeploymentInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentInfo {
    pub id: String,
    pub drone_id: Option<ObjectId>,
    pub room_id: u32,
    pub player_id: PlayerId,
    pub status: String,
    pub at: String,
    pub tikv_version_counter: Tick,
    pub object_store_key: String,
    pub hash: String,
    pub language: String,
    pub size: usize,
}

pub fn get_economy(
    world: &mut SwarmWorld,
    default_player_id: PlayerId,
    params: EconomyParams,
) -> EconomySnapshot {
    let player_id = params.player_id.unwrap_or(default_player_id);
    let rooms = owned_rooms(world, player_id);
    let income = income_for_rooms(world, &rooms);
    let local_storage = world
        .app
        .world()
        .resource::<PlayerLocalStorage>()
        .0
        .get(&player_id)
        .cloned()
        .unwrap_or_default();
    let global_storage = world
        .app
        .world()
        .resource::<PlayerGlobalStorage>()
        .0
        .get(&player_id)
        .cloned()
        .unwrap_or_default();
    let maintenance = maintenance_for_player(world, player_id);
    let storage_tax = storage_tax(world, &global_storage);

    EconomySnapshot {
        player_id,
        income,
        expenses: maintenance.clone(),
        storage_tax,
        maintenance,
        local_storage,
        global_storage,
    }
}

pub fn get_economy_trend(
    world: &mut SwarmWorld,
    default_player_id: PlayerId,
    current_tick: Tick,
    params: EconomyTrendParams,
) -> EconomyTrendResult {
    let player_id = params.player_id.unwrap_or(default_player_id);
    let snapshot = get_economy(
        world,
        player_id,
        EconomyParams {
            player_id: Some(player_id),
        },
    );
    let ticks = params.ticks.clamp(1, 100);
    let first_tick = current_tick.saturating_sub(u64::from(ticks.saturating_sub(1)));
    let income_total = resource_total(&snapshot.income);
    let expense_total = resource_total(&snapshot.expenses);
    let storage_total =
        resource_total(&snapshot.local_storage) + resource_total(&snapshot.global_storage);
    let mut trend = Vec::with_capacity((ticks as usize) * 3);

    for offset in 0..ticks {
        let tick = first_tick + u64::from(offset);
        trend.push(EconomyTrendPoint {
            tick,
            metric: "income".to_string(),
            value: i64::from(income_total),
        });
        trend.push(EconomyTrendPoint {
            tick,
            metric: "expenses".to_string(),
            value: i64::from(expense_total),
        });
        trend.push(EconomyTrendPoint {
            tick,
            metric: "storage".to_string(),
            value: i64::from(storage_total),
        });
    }

    EconomyTrendResult { player_id, trend }
}

pub fn get_drone_efficiency(
    world: &mut SwarmWorld,
    params: DroneEfficiencyParams,
) -> Result<DroneEfficiencyResult, McpError> {
    let mut query = world.app.world_mut().query::<(Entity, &Drone)>();
    for (entity, drone) in query.iter(world.app.world()) {
        if object_id(entity) == params.drone_id {
            let carry_used: u32 = drone.carry.values().copied().sum();
            let carry_utilization = if drone.carry_capacity == 0 {
                0
            } else {
                carry_used.saturating_mul(100) / drone.carry_capacity
            };
            let hits_percent = if drone.hits_max == 0 {
                0
            } else {
                drone.hits.saturating_mul(100) / drone.hits_max
            };
            let fatigue_penalty = drone.fatigue.min(100);
            let spawning_penalty = if drone.spawning { 50 } else { 0 };
            let efficiency = 100_u32
                .saturating_sub(fatigue_penalty)
                .saturating_sub(spawning_penalty)
                .saturating_mul(hits_percent)
                / 100;
            let mut factors = BTreeMap::new();
            factors.insert("carry_used".to_string(), i64::from(carry_used));
            factors.insert(
                "carry_capacity".to_string(),
                i64::from(drone.carry_capacity),
            );
            factors.insert(
                "carry_utilization_percent".to_string(),
                i64::from(carry_utilization),
            );
            factors.insert("fatigue_penalty".to_string(), i64::from(fatigue_penalty));
            factors.insert("hits_percent".to_string(), i64::from(hits_percent));
            factors.insert("spawning_penalty".to_string(), i64::from(spawning_penalty));

            return Ok(DroneEfficiencyResult {
                drone_id: params.drone_id,
                owner: drone.owner,
                efficiency,
                factors,
            });
        }
    }

    Err(McpError::invalid_params("drone_id not found"))
}

pub fn get_leaderboard(
    world: &mut SwarmWorld,
    params: LeaderboardParams,
) -> EconomyLeaderboardResult {
    let scope = params.scope;
    let limit = params.limit.clamp(1, 100);
    let mut players = player_world_counts(world);
    if let Some(ranking) = world.app.world().get_resource::<RankingState>() {
        for (player_id, ranking) in &ranking.players {
            let entry = players
                .entry(*player_id)
                .or_insert_with(|| EconomyLeaderboardEntry {
                    player: *player_id,
                    gcl: 0,
                    rooms: 0,
                    drones: 0,
                    score: 0,
                });
            entry.score = i64::from(ranking.season_points)
                + i64::from(ranking.elo.rating)
                + i64::from(ranking.glicko.rating);
        }
    }

    let mut entries: Vec<_> = players.into_values().collect();
    entries.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| right.rooms.cmp(&left.rooms))
            .then_with(|| right.drones.cmp(&left.drones))
            .then_with(|| left.player.cmp(&right.player))
    });
    entries.truncate(limit);

    EconomyLeaderboardResult { scope, entries }
}

pub fn list_market_orders(params: MarketOrdersParams) -> MarketOrdersResult {
    let resource = params.resource.unwrap_or_else(|| "all".to_string());
    MarketOrdersResult {
        orders: Vec::new(),
        message: format!(
            "no market order book resource is installed yet; returning an empty deterministic {resource} order list"
        ),
    }
}

pub fn sdk_fetch(params: SdkFetchParams) -> Result<SdkFetchResult, McpError> {
    let language = params.language.to_ascii_lowercase();
    let package = params.package.unwrap_or_else(|| match language.as_str() {
        "rust" => "swarm-sdk-rust".to_string(),
        "typescript" | "ts" => "swarm-sdk-ts".to_string(),
        _ => "swarm-sdk".to_string(),
    });
    let mut files = BTreeMap::new();
    match language.as_str() {
        "rust" => {
            files.insert(
                "Cargo.toml".to_string(),
                "[package]\nname = \"swarm-bot\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"
                    .to_string(),
            );
            files.insert(
                "src/lib.rs".to_string(),
                "use swarm_sdk::*;\n\npub fn tick(_ctx: TickContext) -> Vec<CommandIntent> {\n    Vec::new()\n}\n".to_string(),
            );
        }
        "typescript" | "ts" => {
            files.insert(
                "package.json".to_string(),
                "{\"type\":\"module\",\"dependencies\":{\"@swarm/sdk\":\"latest\"}}".to_string(),
            );
            files.insert(
                "src/bot.ts".to_string(),
                "import { TickContext, CommandIntent } from '@swarm/sdk';\n\nexport function tick(_ctx: TickContext): CommandIntent[] {\n  return [];\n}\n".to_string(),
            );
        }
        other => {
            return Err(McpError::invalid_params(format!(
                "unsupported SDK language: {other}"
            )));
        }
    }

    Ok(SdkFetchResult {
        language,
        package,
        version: env!("CARGO_PKG_VERSION").to_string(),
        files,
    })
}

pub fn get_deploy_status(
    modules: &[StoredModule],
    params: DeployStatusParams,
) -> Result<DeployStatusResult, McpError> {
    modules
        .iter()
        .find(|module| module.module_id == params.deploy_id)
        .map(deploy_status_for_module)
        .ok_or_else(|| McpError::invalid_params("deploy_id not found"))
}

pub fn list_deployments(
    modules: &[StoredModule],
    params: ListDeploymentsParams,
) -> ListDeploymentsResult {
    let deployments = modules
        .iter()
        .filter(|module| params.player_id.is_none() || params.player_id == Some(module.player_id))
        .map(deployment_info_for_module)
        .collect();
    ListDeploymentsResult { deployments }
}

fn default_trend_ticks() -> u32 {
    10
}

fn default_leaderboard_scope() -> String {
    "global".to_string()
}

fn default_leaderboard_limit() -> usize {
    10
}

fn default_market_limit() -> usize {
    50
}

fn default_sdk_language() -> String {
    "typescript".to_string()
}

fn owned_rooms(world: &mut SwarmWorld, player_id: PlayerId) -> BTreeSet<RoomId> {
    let mut rooms = BTreeSet::new();
    {
        let mut query = world.app.world_mut().query::<(&Position, &Drone)>();
        for (position, drone) in query.iter(world.app.world()) {
            if drone.owner == player_id {
                rooms.insert(position.room);
            }
        }
    }
    {
        let mut query = world.app.world_mut().query::<(&Position, &Controller)>();
        for (position, controller) in query.iter(world.app.world()) {
            if controller.owner == Some(player_id) {
                rooms.insert(position.room);
            }
        }
    }
    rooms
}

fn income_for_rooms(world: &mut SwarmWorld, rooms: &BTreeSet<RoomId>) -> ResourceCost {
    let mut income = ResourceCost::new();
    let mut query = world.app.world_mut().query::<(&Position, &Source)>();
    for (position, source) in query.iter(world.app.world()) {
        if rooms.contains(&position.room) {
            merge_cost(&mut income, &source.produces);
        }
    }
    income
}

fn maintenance_for_player(world: &mut SwarmWorld, player_id: PlayerId) -> ResourceCost {
    let mut energy = 0_u32;
    let mut query = world.app.world_mut().query::<&Drone>();
    for drone in query.iter(world.app.world()) {
        if drone.owner == player_id {
            energy = energy.saturating_add(drone.body.len() as u32);
        }
    }
    let mut maintenance = ResourceCost::new();
    if energy > 0 {
        maintenance.insert("Energy".to_string(), energy);
    }
    maintenance
}

fn storage_tax(world: &SwarmWorld, global_storage: &ResourceCost) -> StorageTaxSummary {
    let config = world.app.world().resource::<GlobalStorageConfig>();
    let stored = resource_total(global_storage);
    let utilization_ppm = if config.capacity == 0 {
        0
    } else {
        ((stored as u64).saturating_mul(1_000_000) / config.capacity as u64).min(1_000_000) as u32
    };
    let estimated_tax_per_tick = compute_continuous_storage_tax(stored, config.capacity, config);
    let effective_rate = marginal_storage_tax_rate_bp(utilization_ppm, config);
    StorageTaxSummary {
        stored,
        capacity: config.capacity,
        utilization_pct: utilization_ppm / 10_000,
        effective_rate,
        estimated_tax_per_tick,
    }
}

fn player_world_counts(world: &mut SwarmWorld) -> BTreeMap<PlayerId, EconomyLeaderboardEntry> {
    let mut entries = BTreeMap::new();
    {
        let mut query = world.app.world_mut().query::<(&Position, &Drone)>();
        for (_position, drone) in query.iter(world.app.world()) {
            let entry = entries
                .entry(drone.owner)
                .or_insert_with(|| EconomyLeaderboardEntry {
                    player: drone.owner,
                    gcl: 0,
                    rooms: 0,
                    drones: 0,
                    score: 0,
                });
            entry.drones = entry.drones.saturating_add(1);
            entry.score = entry.score.saturating_add(1);
        }
    }
    {
        let mut per_player_rooms: BTreeMap<PlayerId, BTreeSet<RoomId>> = BTreeMap::new();
        let mut query = world.app.world_mut().query::<(&Position, &Controller)>();
        for (position, controller) in query.iter(world.app.world()) {
            if let Some(player_id) = controller.owner {
                per_player_rooms
                    .entry(player_id)
                    .or_default()
                    .insert(position.room);
            }
        }
        for (player_id, rooms) in per_player_rooms {
            let entry = entries
                .entry(player_id)
                .or_insert_with(|| EconomyLeaderboardEntry {
                    player: player_id,
                    gcl: 0,
                    rooms: 0,
                    drones: 0,
                    score: 0,
                });
            entry.rooms = rooms.len() as u32;
            entry.score = entry.score.saturating_add(i64::from(entry.rooms) * 100);
        }
    }
    entries
}

fn deploy_status_for_module(module: &StoredModule) -> DeployStatusResult {
    DeployStatusResult {
        deploy_id: module.module_id.clone(),
        status: "pending_next_tick".to_string(),
        errors: Vec::new(),
        deployed_at: module.deployed_at.clone(),
        tikv_version_counter: module.load_after_tick,
        object_store_key: module_object_store_key(module),
        module_hash: module.wasm_hash.clone(),
        load_after_tick: module.load_after_tick,
    }
}

fn deployment_info_for_module(module: &StoredModule) -> DeploymentInfo {
    DeploymentInfo {
        id: module.module_id.clone(),
        drone_id: None,
        room_id: module.room_id.0,
        player_id: module.player_id,
        status: "pending_next_tick".to_string(),
        at: module.deployed_at.clone(),
        tikv_version_counter: module.load_after_tick,
        object_store_key: module_object_store_key(module),
        hash: module.wasm_hash.clone(),
        language: module.language.clone(),
        size: module.wasm_bytes.len(),
    }
}

fn module_object_store_key(module: &StoredModule) -> String {
    format!(
        "wasm/{}/{}/{}",
        module.player_id, module.room_id.0, module.wasm_hash
    )
}

fn merge_cost(target: &mut ResourceCost, source: &ResourceCost) {
    for (resource, amount) in source {
        *target.entry(resource.clone()).or_insert(0) += *amount;
    }
}

fn resource_total(resources: &ResourceCost) -> u32 {
    resources.values().copied().sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::BodyPart;
    use crate::create_world;

    #[test]
    fn economy_snapshot_counts_owned_room_income_and_maintenance() {
        let mut world = create_world();
        world.spawn_drone(7, 24, 25, vec![BodyPart::Move, BodyPart::Work]);
        world
            .app
            .world_mut()
            .resource_mut::<PlayerGlobalStorage>()
            .0
            .insert(7, [("Energy".to_string(), 60_000)].into_iter().collect());

        let snapshot = get_economy(&mut world, 7, EconomyParams { player_id: None });

        assert_eq!(snapshot.player_id, 7);
        assert_eq!(snapshot.income.get("Energy"), Some(&1));
        assert_eq!(snapshot.maintenance.get("Energy"), Some(&2));
        assert!(snapshot.storage_tax.effective_rate > 0);
    }

    #[test]
    fn drone_efficiency_reports_factors() {
        let mut world = create_world();
        let drone = world.spawn_drone(7, 10, 10, vec![BodyPart::Move, BodyPart::Carry]);
        let result = get_drone_efficiency(
            &mut world,
            DroneEfficiencyParams {
                drone_id: object_id(drone),
            },
        )
        .expect("drone exists");

        assert_eq!(result.owner, 7);
        assert_eq!(result.efficiency, 100);
        assert_eq!(result.factors.get("carry_capacity"), Some(&50));
    }

    #[test]
    fn leaderboard_uses_world_counts() {
        let mut world = create_world();
        world.spawn_drone(9, 10, 10, vec![BodyPart::Move]);

        let leaderboard = get_leaderboard(
            &mut world,
            LeaderboardParams {
                scope: "global".to_string(),
                limit: 10,
            },
        );

        assert!(leaderboard.entries.iter().any(|entry| entry.player == 9));
    }

    #[test]
    fn sdk_fetch_rejects_unknown_language() {
        let error = sdk_fetch(SdkFetchParams {
            language: "lua".to_string(),
            package: None,
        })
        .expect_err("unknown language rejected");

        assert!(error.message.contains("unsupported SDK language"));
    }
}
