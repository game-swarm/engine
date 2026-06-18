use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use bevy::prelude::*;
use ed25519_dalek::{Signature, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use swarm_wasm_sandbox::{
    CachedNativeModule, CompiledModule, CompiledModuleCache, ModuleCacheKey, SandboxRuntime,
    wasmtime_version,
};

use crate::arena::{
    ArenaReplay, ReplayPrivacy, TournamentBracket, TournamentElimination, TournamentMatchSchedule,
};
use crate::command::{
    CommandAuth, CommandIntent, CommandSource, ObjectId, RawCommand, RejectionReason, Tick,
    object_id, validate_command,
};
use crate::auth::{CertificateIssuer, PlayerCertificate, PlayerCertificatePayload};
use crate::components::*;
use crate::economy::*;
use crate::hot_cache::{SnapshotKey, read_through_dragonfly};
use crate::resources::{PendingGlobalTransfers, PlayerGlobalStorage, PlayerLocalStorage};
use crate::tick::{TickTrace, tick_key};
use crate::visibility::{
    VISIBILITY_RADIUS, is_position_visible_to, visible_entity_ids, visible_positions,
};
use crate::world::SwarmWorld;

const MAX_WASM_BYTES: usize = 5 * 1024 * 1024;
const CERTIFICATE_TTL_SECONDS: u64 = 24 * 60 * 60;
const WEB_ACCESS_TOKEN_TTL_SECONDS: u64 = 15 * 60;
const WEB_REFRESH_TOKEN_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;
const CERTIFICATE_AUDIENCE: &str = "swarm-wasm-deploy";
const WEB_TOKEN_AUDIENCE: &str = "swarm-web";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpContext {
    pub player_id: PlayerId,
    pub tick: Tick,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpError {
    pub code: i32,
    pub message: String,
}

impl McpError {
    pub(crate) fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }

    fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("unknown MCP tool: {method}"),
        }
    }

    fn rate_limited(retry_after_seconds: u64) -> Self {
        Self {
            code: -32000,
            message: format!("rate limited, retry after {retry_after_seconds} seconds"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RateBucket {
    tokens: u32,
    last_tick: Tick,
}

#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: HashMap<(PlayerId, CommandSource), RateBucket>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn check(
        &mut self,
        player_id: PlayerId,
        source: CommandSource,
        tick: Tick,
    ) -> Result<(), McpError> {
        let limit = rate_limit_for_source(source);
        let bucket = self
            .buckets
            .entry((player_id, source))
            .or_insert(RateBucket {
                tokens: limit,
                last_tick: tick,
            });

        if tick > bucket.last_tick {
            let elapsed_ticks = tick.saturating_sub(bucket.last_tick);
            let refill = elapsed_ticks.saturating_mul(u64::from(limit));
            bucket.tokens =
                u32::try_from((u64::from(bucket.tokens) + refill).min(u64::from(limit)))
                    .unwrap_or(limit);
            bucket.last_tick = tick;
        }

        if bucket.tokens == 0 {
            return Err(McpError::rate_limited(1));
        }

        bucket.tokens -= 1;
        Ok(())
    }
}

fn rate_limit_for_source(source: CommandSource) -> u32 {
    match source {
        CommandSource::Wasm => 100,
        CommandSource::McpDeploy => 5,
        CommandSource::McpQuery => 50,
        CommandSource::Admin => 20,
        CommandSource::Replay => 200,
        CommandSource::TestHarness => 1_000,
        CommandSource::Tutorial => 25,
        CommandSource::Deploy => 5,
        CommandSource::Rollback => 5,
        CommandSource::RuleMod => 25,
        CommandSource::Simulate => 50,
        CommandSource::DryRun => 20,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<McpError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleWorldSnapshot {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub room_id: u32,
    pub visibility_radius: i32,
    pub visible_tiles: Vec<VisibleTile>,
    pub entities: Vec<VisibleEntity>,
    pub local_storage: BTreeMap<String, u32>,
    pub global_storage: BTreeMap<String, u32>,
    pub pending_global_transfers: Vec<VisiblePendingGlobalTransfer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisiblePendingGlobalTransfer {
    pub player_id: PlayerId,
    pub direction: String,
    pub resource: String,
    pub amount: u32,
    pub deliver_amount: u32,
    pub remaining_ticks: Tick,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VisibleTile {
    pub x: i32,
    pub y: i32,
    pub room_id: u32,
    pub terrain: TerrainType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum VisibleEntity {
    Drone(VisibleDrone),
    Structure(VisibleStructure),
    Source(VisibleSource),
    Resource(VisibleResource),
    Controller(VisibleController),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisiblePosition {
    pub x: i32,
    pub y: i32,
    pub room_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleDrone {
    pub id: ObjectId,
    pub owner: PlayerId,
    pub position: VisiblePosition,
    pub body: Vec<BodyPart>,
    pub carry: BTreeMap<String, u32>,
    pub carry_capacity: u32,
    pub fatigue: u32,
    pub hits: u32,
    pub hits_max: u32,
    pub spawning: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleStructure {
    pub id: ObjectId,
    pub structure_type: StructureType,
    pub owner: Option<PlayerId>,
    pub position: VisiblePosition,
    pub hits: u32,
    pub hits_max: u32,
    pub energy: Option<u32>,
    pub energy_capacity: Option<u32>,
    pub cooldown: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleSource {
    pub id: ObjectId,
    pub position: VisiblePosition,
    pub produces: BTreeMap<String, u32>,
    pub capacity: u32,
    pub ticks_to_regeneration: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleResource {
    pub id: ObjectId,
    pub position: VisiblePosition,
    pub amounts: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleController {
    pub id: ObjectId,
    pub owner: Option<PlayerId>,
    pub position: VisiblePosition,
    pub level: u8,
    pub progress: u32,
    pub progress_total: u32,
    pub safe_mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldRules {
    pub ruleset: String,
    pub room_size: i32,
    pub visibility_radius: i32,
    pub max_wasm_bytes: usize,
    pub active_mods: Vec<WorldRuleMod>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldRuleMod {
    pub id: String,
    pub version: String,
    pub description: String,
    pub config: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuth2LoginParams {
    pub provider: String,
    pub subject: String,
    pub access_token: String,
    pub client_public_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuth2CallbackParams {
    pub provider: String,
    pub code: String,
    pub state: String,
    pub redirect_uri: String,
    pub client_public_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenRefreshParams {
    pub refresh_token: String,
    pub client_public_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeAuthParams {
    pub refresh_token: Option<String>,
    pub certificate: Option<PlayerCertificate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuth2LoginResult {
    pub player_id: PlayerId,
    pub session: WebAuthSession,
    pub certificate: PlayerCertificate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebAuthSession {
    pub player_id: PlayerId,
    pub provider: String,
    pub subject: String,
    pub audience: String,
    pub access_token: String,
    pub access_token_expires_at: u64,
    pub refresh_token: String,
    pub refresh_token_expires_at: u64,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRefreshResult {
    pub player_id: PlayerId,
    pub session: WebAuthSession,
    pub certificate: PlayerCertificate,
    pub renew_after_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokeAuthResult {
    pub revoked_session: bool,
    pub revoked_certificate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployParams {
    pub wasm_bytes: String,
    pub certificate: PlayerCertificate,
    pub wasm_signature: String,
    pub language: String,
    pub version_tag: String,
    pub room_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployResult {
    pub module_id: String,
    pub status: String,
    pub deployed_at: String,
    pub module_hash: String,
    pub wasmtime_version: String,
    pub cache_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredModule {
    pub module_id: String,
    pub player_id: PlayerId,
    pub room_id: RoomId,
    pub wasm_bytes: Vec<u8>,
    pub cached_native_module: CachedNativeModule,
    pub wasm_hash: String,
    pub wasmtime_version: String,
    pub certificate: PlayerCertificate,
    pub wasm_signature: String,
    pub language: String,
    pub version_tag: String,
    pub deployed_at: String,
    pub load_after_tick: Tick,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TournamentPrecommitParams {
    pub module_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TournamentLockedModule {
    pub player_id: PlayerId,
    pub module_id: String,
    pub wasm_hash: String,
    pub version_tag: String,
    pub locked_at_tick: Tick,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TournamentPrecommitResult {
    pub status: String,
    pub locked_module: TournamentLockedModule,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TournamentCreateParams {
    pub tournament_id: String,
    pub elimination: TournamentElimination,
    pub fixed_ticks: Tick,
    pub players: Vec<PlayerId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TournamentCreateResult {
    pub tournament_id: String,
    pub status: String,
    pub elimination: TournamentElimination,
    pub fixed_ticks: Tick,
    pub players: Vec<PlayerId>,
    pub scheduled: Vec<TournamentMatchSchedule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchResultParams {
    pub tournament_id: String,
    pub match_id: u64,
    pub winner: PlayerId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchResultResult {
    pub tournament_id: String,
    pub match_id: u64,
    pub winner: PlayerId,
    pub loser: PlayerId,
    pub champion: Option<PlayerId>,
    pub scheduled: Vec<TournamentMatchSchedule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TournamentBracketStatus {
    pub tournament_id: String,
    pub elimination: TournamentElimination,
    pub fixed_ticks: Tick,
    pub players: Vec<PlayerId>,
    pub scheduled: Vec<TournamentMatchSchedule>,
    pub completed_matches: usize,
    pub champion: Option<PlayerId>,
    pub losses: BTreeMap<PlayerId, u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TournamentStatusResult {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub mode: String,
    pub deploy_locked: bool,
    pub locked_module: Option<TournamentLockedModule>,
    pub preparation_tools: Vec<ToolInfo>,
    pub direct_gameplay_tools_enabled: bool,
    pub tournaments: Vec<TournamentBracketStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableActionsResult {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub wasm_actions: Vec<String>,
    pub mcp_tools: Vec<ToolInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickExplanation {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub state_checksum: u64,
    pub visible_entity_count: usize,
    pub visible_tile_count: usize,
    pub accepted_commands: usize,
    pub rejected_commands: usize,
    pub accepted: Vec<RawCommand>,
    pub rejected: Vec<TickCommandRejection>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickCommandRejection {
    pub command: RawCommand,
    pub rejection: RejectionReason,
    pub detail: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerrainParams {
    pub x: i32,
    pub y: i32,
    #[serde(default)]
    pub room_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerrainResult {
    pub x: i32,
    pub y: i32,
    pub room_id: u32,
    pub terrain: TerrainType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectsInRangeParams {
    pub x: i32,
    pub y: i32,
    pub range: u32,
    #[serde(default)]
    pub room_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectsInRangeResult {
    pub origin: VisiblePosition,
    pub range: u32,
    pub entities: Vec<VisibleEntity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateModuleParams {
    pub wasm_bytes: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidateModuleResult {
    pub valid: bool,
    pub wasm_hash: Option<String>,
    pub size_bytes: usize,
    pub issues: Vec<String>,
    pub estimated_fuel: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackParams {
    #[serde(default)]
    pub room_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackResult {
    pub status: String,
    pub rolled_back_to: StoredModuleSummary,
    pub removed_module_id: String,
    pub load_after_tick: Tick,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredModuleSummary {
    pub module_id: String,
    pub player_id: PlayerId,
    pub room_id: u32,
    pub wasm_hash: String,
    pub wasmtime_version: String,
    pub language: String,
    pub version_tag: String,
    pub deployed_at: String,
    pub load_after_tick: Tick,
}

fn stored_module_summary(module: &StoredModule) -> StoredModuleSummary {
    StoredModuleSummary {
        module_id: module.module_id.clone(),
        player_id: module.player_id,
        room_id: module.room_id.0,
        wasm_hash: module.wasm_hash.clone(),
        wasmtime_version: module.wasmtime_version.clone(),
        language: module.language.clone(),
        version_tag: module.version_tag.clone(),
        deployed_at: module.deployed_at.clone(),
        load_after_tick: module.load_after_tick,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectEntityParams {
    #[serde(alias = "drone_id", alias = "structure_id", alias = "controller_id")]
    pub object_id: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FullEntityState {
    pub id: ObjectId,
    pub position: Option<VisiblePosition>,
    pub owner: Option<PlayerId>,
    pub drone: Option<Drone>,
    pub structure: Option<Structure>,
    pub source: Option<Source>,
    pub resource: Option<crate::components::Resource>,
    pub terrain: Option<TerrainType>,
    pub controller: Option<Controller>,
    pub marked_for_death: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileResult {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub deployed_modules: usize,
    pub pending_modules: usize,
    pub owned_visible_drones: usize,
    pub owned_visible_structures: usize,
    pub available_mcp_tools: usize,
    pub direct_gameplay_tools_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DryRunCommandsParams {
    pub commands: Vec<CommandIntent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunCommandResult {
    pub sequence: u32,
    pub command: RawCommand,
    pub accepted: bool,
    pub rejection: Option<RejectionReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunCommandsResult {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub commands: Vec<DryRunCommandResult>,
    pub state_checksum_before: u64,
    pub state_checksum_after: u64,
    pub mutated_world: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimulateParams {
    pub snapshot: VisibleWorldSnapshot,
    #[serde(alias = "future_ticks")]
    pub ticks: Tick,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulateResult {
    pub player_id: PlayerId,
    pub room_id: u32,
    pub from_tick: Tick,
    pub to_tick: Tick,
    pub ticks: Tick,
    pub predicted_snapshot: VisibleWorldSnapshot,
    pub diff: SimulatedStateDiff,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulatedStateDiff {
    pub tick_before: Tick,
    pub tick_after: Tick,
    pub local_storage_before: BTreeMap<String, u32>,
    pub local_storage_after: BTreeMap<String, u32>,
    pub global_storage_before: BTreeMap<String, u32>,
    pub global_storage_after: BTreeMap<String, u32>,
    pub pending_global_transfers_before: Vec<VisiblePendingGlobalTransfer>,
    pub pending_global_transfers_after: Vec<VisiblePendingGlobalTransfer>,
    pub entity_changes: Vec<SimulatedEntityChange>,
    pub state_changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulatedEntityChange {
    pub id: ObjectId,
    pub before: Option<VisibleEntity>,
    pub after: Option<VisibleEntity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocsParams {
    #[serde(default = "default_docs_topic")]
    pub topic: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocsSection {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocsResult {
    pub topic: String,
    pub sections: Vec<DocsSection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceReadParams {
    pub uri: String,
}

#[derive(Default)]
pub struct McpServer {
    modules: Vec<StoredModule>,
    module_cache: CompiledModuleCache,
    sandbox_runtime: SandboxRuntime,
    tournament_locks: BTreeMap<PlayerId, TournamentLockedModule>,
    tournaments: BTreeMap<String, TournamentBracket>,
    issuer: CertificateIssuer,
    sessions: BTreeMap<String, WebAuthSession>,
    revoked_certificates: BTreeSet<String>,
    rate_limiter: RateLimiter,
    now_seconds: Option<u64>,
    tick_traces: Vec<TickTrace>,
}

impl McpServer {
    pub fn new() -> Self {
        Self {
            modules: Vec::new(),
            module_cache: CompiledModuleCache::new(),
            sandbox_runtime: SandboxRuntime::default(),
            tournament_locks: BTreeMap::new(),
            tournaments: BTreeMap::new(),
            issuer: CertificateIssuer::new(),
            sessions: BTreeMap::new(),
            revoked_certificates: BTreeSet::new(),
            rate_limiter: RateLimiter::new(),
            now_seconds: None,
            tick_traces: Vec::new(),
        }
    }

    pub fn with_issuer_for_tests(issuer: SigningKey, now_seconds: u64) -> Self {
        Self {
            modules: Vec::new(),
            module_cache: CompiledModuleCache::new(),
            sandbox_runtime: SandboxRuntime::default(),
            tournament_locks: BTreeMap::new(),
            tournaments: BTreeMap::new(),
            issuer: CertificateIssuer::from_signing_key_for_tests(issuer),
            sessions: BTreeMap::new(),
            revoked_certificates: BTreeSet::new(),
            rate_limiter: RateLimiter::new(),
            now_seconds: Some(now_seconds),
            tick_traces: Vec::new(),
        }
    }

    pub fn handle_json_rpc(
        &mut self,
        world: &mut SwarmWorld,
        context: McpContext,
        request: JsonRpcRequest,
    ) -> JsonRpcResponse {
        let id = request.id.clone();
        if request.jsonrpc != "2.0" {
            return error_response(id, McpError::invalid_params("jsonrpc must be 2.0"));
        }

        match self.call_tool(world, context, &request.method, request.params) {
            Ok(result) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(result),
                error: None,
            },
            Err(error) => error_response(id, error),
        }
    }

    pub fn call_tool(
        &mut self,
        world: &mut SwarmWorld,
        context: McpContext,
        tool: &str,
        params: Value,
    ) -> Result<Value, McpError> {
        let source = mcp_tool_source(tool).ok_or_else(|| McpError::method_not_found(tool))?;
        self.rate_limiter
            .check(context.player_id, source, context.tick)?;

        match tool {
            "swarm_get_snapshot" => serde_json::to_value(swarm_get_snapshot(world, context))
                .map_err(|error| McpError::invalid_params(error.to_string())),
            "swarm_get_terrain" => {
                let params: TerrainParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(swarm_get_terrain(world, params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_get_world_rules" => serde_json::to_value(swarm_get_world_rules())
                .map_err(|error| McpError::invalid_params(error.to_string())),
            "swarm_get_schema" => Ok(swarm_get_schema()),
            "swarm_get_available_actions" => {
                serde_json::to_value(swarm_get_available_actions(context))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_explain_last_tick" => {
                serde_json::to_value(self.swarm_explain_last_tick(world, context))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_get_drone" => {
                let params: InspectEntityParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(swarm_get_drone(world, context, params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_get_room" => {
                let params: InspectRoomParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(swarm_get_room(world, context, params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_get_structure" | "swarm_get_controller" => {
                let params: InspectEntityParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(swarm_get_drone(world, context, params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_get_code" => Ok(world_view_code(params)?),
            "swarm_get_visibility" => Ok(world_view_visibility(world, context)),
            "swarm_get_path" => Ok(world_view_path(params)),
            "swarm_get_resources" => Ok(world_view_resources(world, context)),
            "swarm_get_info" => Ok(world_view_info(world, context)),
            "swarm_list_drones" => Ok(world_view_list(world, context, "drones")),
            "swarm_list_rooms" => Ok(world_view_list(world, context, "rooms")),
            "swarm_list_structures" => Ok(world_view_list(world, context, "structures")),
            "swarm_list_controllers" => Ok(world_view_list(world, context, "controllers")),
            "swarm_get_events" => Ok(json!({ "events": Vec::<Value>::new() })),
            "swarm_get_messages" => Ok(world_view_messages(params)?),
            "swarm_profile" => serde_json::to_value(self.swarm_profile(world, context))
                .map_err(|error| McpError::invalid_params(error.to_string())),
            "swarm_get_economy" => {
                let params: EconomyParams = if params.is_null() {
                    EconomyParams { player_id: None }
                } else {
                    serde_json::from_value(params)
                        .map_err(|error| McpError::invalid_params(error.to_string()))?
                };
                serde_json::to_value(get_economy(world, context.player_id, params))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_get_economy_trend" => {
                let params: EconomyTrendParams = if params.is_null() {
                    EconomyTrendParams {
                        player_id: None,
                        ticks: 10,
                    }
                } else {
                    serde_json::from_value(params)
                        .map_err(|error| McpError::invalid_params(error.to_string()))?
                };
                serde_json::to_value(get_economy_trend(
                    world,
                    context.player_id,
                    context.tick,
                    params,
                ))
                .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_get_drone_efficiency" => {
                let params: DroneEfficiencyParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(get_drone_efficiency(world, params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_get_leaderboard" => {
                let params: LeaderboardParams = if params.is_null() {
                    LeaderboardParams {
                        scope: "global".to_string(),
                        limit: 10,
                    }
                } else {
                    serde_json::from_value(params)
                        .map_err(|error| McpError::invalid_params(error.to_string()))?
                };
                serde_json::to_value(get_leaderboard(world, params))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_list_market_orders" => {
                let params: MarketOrdersParams = if params.is_null() {
                    MarketOrdersParams {
                        resource: None,
                        limit: 50,
                    }
                } else {
                    serde_json::from_value(params)
                        .map_err(|error| McpError::invalid_params(error.to_string()))?
                };
                serde_json::to_value(list_market_orders(params))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_sdk_fetch" => {
                let params: SdkFetchParams = if params.is_null() {
                    SdkFetchParams {
                        language: "typescript".to_string(),
                        package: None,
                    }
                } else {
                    serde_json::from_value(params)
                        .map_err(|error| McpError::invalid_params(error.to_string()))?
                };
                serde_json::to_value(sdk_fetch(params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_simulate" => {
                let params: SimulateParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(swarm_simulate(params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_get_replay" => {
                let params: GetReplayParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(swarm_get_replay(world, context, params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_get_docs" => {
                let params: DocsParams = if params.is_null() {
                    DocsParams {
                        topic: default_docs_topic(),
                    }
                } else {
                    serde_json::from_value(params)
                        .map_err(|error| McpError::invalid_params(error.to_string()))?
                };
                serde_json::to_value(swarm_get_docs(params))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "resources/list" => Ok(docs_resources_list()),
            "resources/read" => {
                let params: ResourceReadParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                docs_resources_read(params)
            }
            "swarm_auth_revoke" => {
                let params: RevokeAuthParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(self.swarm_auth_revoke(params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_auth_login"
            | "swarm_auth_logout"
            | "swarm_auth_refresh"
            | "swarm_auth_check"
            | "swarm_auth_cert_issue"
            | "swarm_auth_cert_list"
            | "swarm_auth_cert_revoke"
            | "swarm_auth_cert_rotate"
            | "swarm_auth_device_list"
            | "swarm_auth_device_register"
            | "swarm_get_world_config" => serde_json::to_value(auth_stub_result(tool, context, params))
                .map_err(|error| McpError::invalid_params(error.to_string())),
            "swarm_tournament_precommit" => {
                let params: TournamentPrecommitParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(self.swarm_tournament_precommit(context, params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_tournament_create" => {
                let params: TournamentCreateParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(self.swarm_tournament_create(params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_tournament_status" => {
                serde_json::to_value(self.swarm_tournament_status(context))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_match_result" => {
                let params: MatchResultParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(self.swarm_match_result(params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_list_modules" => {
                let params: ListModulesParams = if params.is_null() {
                    ListModulesParams { player_id: None }
                } else {
                    serde_json::from_value(params)
                        .map_err(|error| McpError::invalid_params(error.to_string()))?
                };
                serde_json::to_value(self.swarm_list_modules(&params))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_deploy" => {
                let params: DeployParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(self.swarm_deploy(world, context, params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_get_deploy_status" => {
                let params: DeployStatusParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(get_deploy_status(&self.modules, params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_list_deployments" => {
                let params: ListDeploymentsParams = if params.is_null() {
                    ListDeploymentsParams { player_id: None }
                } else {
                    serde_json::from_value(params)
                        .map_err(|error| McpError::invalid_params(error.to_string()))?
                };
                serde_json::to_value(list_deployments(&self.modules, params))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_validate_module" => {
                let params: ValidateModuleParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(swarm_validate_module(params))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_admin_challenge" => Ok(swarm_admin_challenge(context, &params)),
            "swarm_admin_set_world_config" => Ok(swarm_admin_set_world_config(context, &params)),
            "swarm_admin_rollback" => Ok(swarm_admin_rollback(context, &params)),
            "swarm_admin_ban_player" => Ok(swarm_admin_ban_player(context, &params)),
            "swarm_admin_force_gc" => Ok(swarm_admin_force_gc(&params)),
            "swarm_admin_get_audit_log" => Ok(swarm_admin_get_audit_log(&params)),
            method => Err(McpError::method_not_found(method)),
        }
    }

    pub fn swarm_oauth2_login(
        &mut self,
        params: OAuth2LoginParams,
    ) -> Result<OAuth2LoginResult, McpError> {
        if params.provider.trim().is_empty() {
            return Err(McpError::invalid_params("provider is required"));
        }
        if params.subject.trim().is_empty() {
            return Err(McpError::invalid_params("subject is required"));
        }
        if params.access_token.trim().is_empty() {
            return Err(McpError::invalid_params("access_token is required"));
        }
        decode_ed25519_public_key(&params.client_public_key, "client_public_key")?;
        self.issue_login(
            params.provider,
            params.subject,
            params.access_token,
            params.client_public_key,
        )
    }

    pub fn swarm_oauth2_callback(
        &mut self,
        params: OAuth2CallbackParams,
    ) -> Result<OAuth2LoginResult, McpError> {
        let provider = params.provider.trim().to_ascii_lowercase();
        if provider != "github" && provider != "google" {
            return Err(McpError::invalid_params(
                "provider must be github or google",
            ));
        }
        if params.code.trim().is_empty() {
            return Err(McpError::invalid_params("code is required"));
        }
        if params.state.trim().is_empty() {
            return Err(McpError::invalid_params("state is required"));
        }
        if params.redirect_uri.trim().is_empty() {
            return Err(McpError::invalid_params("redirect_uri is required"));
        }
        decode_ed25519_public_key(&params.client_public_key, "client_public_key")?;
        let subject = format!(
            "{}:{}",
            provider,
            blake3::hash(params.code.as_bytes()).to_hex()
        );
        self.issue_login(provider, subject, params.code, params.client_public_key)
    }

    pub fn swarm_token_refresh(
        &mut self,
        params: TokenRefreshParams,
    ) -> Result<TokenRefreshResult, McpError> {
        if params.refresh_token.trim().is_empty() {
            return Err(McpError::invalid_params("refresh_token is required"));
        }
        decode_ed25519_public_key(&params.client_public_key, "client_public_key")?;
        let now = self.now_seconds();
        let stored = self
            .sessions
            .get(&params.refresh_token)
            .cloned()
            .ok_or_else(|| McpError::invalid_params("refresh_token is invalid"))?;
        if stored.refresh_token_expires_at <= now {
            self.sessions.remove(&params.refresh_token);
            return Err(McpError::invalid_params("refresh_token is expired"));
        }
        let mut session = stored.clone();
        session.access_token = opaque_web_token(
            "web_access",
            &stored.provider,
            &stored.subject,
            &params.refresh_token,
            now,
        );
        session.access_token_expires_at = now + WEB_ACCESS_TOKEN_TTL_SECONDS;
        self.sessions
            .insert(session.refresh_token.clone(), session.clone());
        let certificate = self.issue_certificate(
            session.player_id,
            session.provider.clone(),
            session.subject.clone(),
            params.client_public_key,
        )?;
        Ok(TokenRefreshResult {
            player_id: session.player_id,
            session,
            certificate,
            renew_after_seconds: CERTIFICATE_TTL_SECONDS,
        })
    }

    pub fn swarm_auth_revoke(
        &mut self,
        params: RevokeAuthParams,
    ) -> Result<RevokeAuthResult, McpError> {
        let revoked_session = params
            .refresh_token
            .as_deref()
            .map(|t| self.sessions.remove(t).is_some())
            .unwrap_or(false);
        let revoked_certificate = if let Some(certificate) = params.certificate {
            self.issuer.verify(&certificate)?;
            self.revoked_certificates.insert(certificate.signature)
        } else {
            false
        };
        Ok(RevokeAuthResult {
            revoked_session,
            revoked_certificate,
        })
    }

    fn issue_login(
        &mut self,
        provider: String,
        subject: String,
        seed: String,
        client_public_key: String,
    ) -> Result<OAuth2LoginResult, McpError> {
        let player_id = oauth_player_id(&provider, &subject);
        let now = self.now_seconds();
        let session = WebAuthSession {
            player_id,
            provider: provider.clone(),
            subject: subject.clone(),
            audience: WEB_TOKEN_AUDIENCE.to_string(),
            access_token: opaque_web_token("web_access", &provider, &subject, &seed, now),
            access_token_expires_at: now + WEB_ACCESS_TOKEN_TTL_SECONDS,
            refresh_token: opaque_web_token("web_refresh", &provider, &subject, &seed, now),
            refresh_token_expires_at: now + WEB_REFRESH_TOKEN_TTL_SECONDS,
            scopes: vec!["web".to_string(), "mcp:deploy".to_string()],
        };
        self.sessions
            .insert(session.refresh_token.clone(), session.clone());
        let certificate =
            self.issue_certificate(player_id, provider, subject, client_public_key)?;
        Ok(OAuth2LoginResult {
            player_id,
            session,
            certificate,
        })
    }

    fn issue_certificate(
        &self,
        player_id: PlayerId,
        provider: String,
        subject: String,
        client_public_key: String,
    ) -> Result<PlayerCertificate, McpError> {
        let issued_at = self.now_seconds();
        self.issuer.issue(PlayerCertificatePayload {
            audience: CERTIFICATE_AUDIENCE.to_string(),
            player_id,
            provider,
            subject,
            client_public_key,
            issued_at,
            expires_at: issued_at + CERTIFICATE_TTL_SECONDS,
        })
    }

    pub fn swarm_list_modules(&self, params: &ListModulesParams) -> Vec<ModuleInfo> {
        self.modules
            .iter()
            .filter(|m| params.player_id.is_none() || params.player_id == Some(m.player_id))
            .map(|m| ModuleInfo {
                player_id: m.player_id,
                module_hash: m.module_id.clone(),
                wasm_size: m.wasm_bytes.len(),
                compiled_at: None,
            })
            .collect()
    }

    pub fn swarm_deploy(
        &mut self,
        world: &SwarmWorld,
        context: McpContext,
        params: DeployParams,
    ) -> Result<DeployResult, McpError> {
        if params.language.trim().is_empty() {
            return Err(McpError::invalid_params("language is required"));
        }
        if params.version_tag.trim().is_empty() {
            return Err(McpError::invalid_params("version_tag is required"));
        }

        let room_id = RoomId(params.room_id);
        if !world
            .app
            .world()
            .resource::<RoomTerrains>()
            .0
            .contains_key(&room_id)
        {
            return Err(McpError::invalid_params("room_id does not exist"));
        }

        if self.tournament_locks.contains_key(&context.player_id) {
            return Err(McpError::invalid_params(
                "player has locked a tournament precommit; deploy is disabled until the match ends",
            ));
        }

        let wasm_bytes = decode_base64(&params.wasm_bytes)?;
        if wasm_bytes.is_empty() {
            return Err(McpError::invalid_params("wasm_bytes is empty"));
        }
        if wasm_bytes.len() > MAX_WASM_BYTES {
            return Err(McpError::invalid_params("wasm module exceeds max size"));
        }
        if !wasm_bytes.starts_with(b"\0asm") {
            return Err(McpError::invalid_params("wasm_bytes must be a wasm module"));
        }
        self.verify_certificate_for_player(&params.certificate, context.player_id)?;
        let wasm_hash = blake3::hash(&wasm_bytes);
        verify_wasm_signature(
            &params.certificate,
            wasm_hash.as_bytes(),
            &params.wasm_signature,
        )?;

        let cached_native_module = self
            .sandbox_runtime
            .precompile_native(&wasm_bytes)
            .map_err(|error| {
                McpError::invalid_params(format!("wasm precompile failed: {error}"))
            })?;
        let cache_key = cached_native_module.key.clone();
        self.module_cache.insert(cached_native_module.clone());

        let module_id = format!(
            "mod_{}_{}_{}",
            context.player_id,
            params.room_id,
            self.modules.len() + 1
        );
        let deployed_at = unix_timestamp_string();
        self.modules.push(StoredModule {
            module_id: module_id.clone(),
            player_id: context.player_id,
            room_id,
            wasm_bytes,
            cached_native_module,
            wasm_hash: cache_key.module_hash.clone(),
            wasmtime_version: cache_key.wasmtime_version.clone(),
            certificate: params.certificate,
            wasm_signature: params.wasm_signature,
            language: params.language,
            version_tag: params.version_tag,
            deployed_at: deployed_at.clone(),
            load_after_tick: context.tick + 1,
        });

        Ok(DeployResult {
            module_id,
            status: "pending_next_tick".to_string(),
            deployed_at,
            module_hash: cache_key.module_hash,
            wasmtime_version: cache_key.wasmtime_version,
            cache_status: "precompiled".to_string(),
        })
    }

    pub fn swarm_rollback(
        &mut self,
        context: McpContext,
        params: RollbackParams,
    ) -> Result<RollbackResult, McpError> {
        let room_id = RoomId(params.room_id);
        let latest_index = self
            .modules
            .iter()
            .rposition(|module| module.player_id == context.player_id && module.room_id == room_id)
            .ok_or_else(|| McpError::invalid_params("no deployed module exists for player/room"))?;
        let previous_index = self.modules[..latest_index]
            .iter()
            .rposition(|module| module.player_id == context.player_id && module.room_id == room_id)
            .ok_or_else(|| McpError::invalid_params("no previous module version exists"))?;

        let removed = self.modules.remove(latest_index);
        self.modules[previous_index].load_after_tick = context.tick + 1;
        let rolled_back_to = stored_module_summary(&self.modules[previous_index]);
        Ok(RollbackResult {
            status: "pending_next_tick".to_string(),
            rolled_back_to,
            removed_module_id: removed.module_id,
            load_after_tick: context.tick + 1,
        })
    }

    pub fn swarm_tournament_precommit(
        &mut self,
        context: McpContext,
        params: TournamentPrecommitParams,
    ) -> Result<TournamentPrecommitResult, McpError> {
        if self.tournament_locks.contains_key(&context.player_id) {
            return Err(McpError::invalid_params(
                "player already has a locked tournament precommit",
            ));
        }
        let module = self
            .modules
            .iter()
            .find(|module| {
                module.player_id == context.player_id && module.module_id == params.module_id
            })
            .ok_or_else(|| McpError::invalid_params("module_id is not deployed by this player"))?;
        let locked_module = TournamentLockedModule {
            player_id: context.player_id,
            module_id: module.module_id.clone(),
            wasm_hash: module.wasm_hash.clone(),
            version_tag: module.version_tag.clone(),
            locked_at_tick: context.tick,
        };
        self.tournament_locks
            .insert(context.player_id, locked_module.clone());
        Ok(TournamentPrecommitResult {
            status: "locked_for_tournament".to_string(),
            locked_module,
        })
    }

    pub fn swarm_tournament_status(&self, context: McpContext) -> TournamentStatusResult {
        let locked_module = self.tournament_locks.get(&context.player_id).cloned();
        TournamentStatusResult {
            tick: context.tick,
            player_id: context.player_id,
            mode: if locked_module.is_some() {
                "precommit_locked".to_string()
            } else {
                "preparation".to_string()
            },
            deploy_locked: locked_module.is_some(),
            locked_module,
            preparation_tools: tournament_tool_infos(),
            direct_gameplay_tools_enabled: false,
            tournaments: self
                .tournaments
                .iter()
                .map(|(id, bracket)| tournament_bracket_status(id, bracket))
                .collect(),
        }
    }

    pub fn swarm_tournament_create(
        &mut self,
        params: TournamentCreateParams,
    ) -> Result<TournamentCreateResult, McpError> {
        if params.tournament_id.trim().is_empty() {
            return Err(McpError::invalid_params("tournament_id is required"));
        }
        if self.tournaments.contains_key(&params.tournament_id) {
            return Err(McpError::invalid_params("tournament_id already exists"));
        }

        let mut players = Vec::with_capacity(params.players.len());
        for player_id in &params.players {
            let locked = self
                .tournament_locks
                .get(player_id)
                .ok_or_else(|| McpError::invalid_params("all players must precommit a module"))?;
            players.push(crate::arena::ArenaPlayerCode::new(
                locked.player_id,
                locked.module_id.clone(),
                locked.wasm_hash.clone(),
            ));
        }

        let mut bracket = TournamentBracket::seed(params.elimination, players, params.fixed_ticks)
            .map_err(arena_error_to_mcp)?;
        let scheduled = bracket.schedule_next_round().map_err(arena_error_to_mcp)?;
        let player_ids = bracket
            .seeds
            .iter()
            .map(|seed| seed.code.player_id)
            .collect::<Vec<_>>();
        self.tournaments
            .insert(params.tournament_id.clone(), bracket);

        Ok(TournamentCreateResult {
            tournament_id: params.tournament_id,
            status: "scheduled".to_string(),
            elimination: params.elimination,
            fixed_ticks: params.fixed_ticks,
            players: player_ids,
            scheduled,
        })
    }

    pub fn swarm_match_result(
        &mut self,
        params: MatchResultParams,
    ) -> Result<MatchResultResult, McpError> {
        let bracket = self
            .tournaments
            .get_mut(&params.tournament_id)
            .ok_or_else(|| McpError::invalid_params("unknown tournament_id"))?;
        let position = bracket
            .scheduled
            .iter()
            .position(|schedule| schedule.match_id == params.match_id)
            .ok_or_else(|| McpError::invalid_params("match_id is not scheduled"))?;
        let schedule = bracket.scheduled.remove(position);
        let match_id = schedule.match_id;
        let record = bracket
            .record_match_result(schedule, params.winner, empty_tournament_replay())
            .map_err(arena_error_to_mcp)?;
        let loser = record.loser;
        if bracket.champion.is_none() && bracket.scheduled.is_empty() {
            let _ = bracket.schedule_next_round();
        }

        Ok(MatchResultResult {
            tournament_id: params.tournament_id,
            match_id,
            winner: params.winner,
            loser,
            champion: bracket.champion,
            scheduled: bracket.scheduled.clone(),
        })
    }

    pub fn tournament_locks(&self) -> &BTreeMap<PlayerId, TournamentLockedModule> {
        &self.tournament_locks
    }

    pub fn tournaments(&self) -> &BTreeMap<String, TournamentBracket> {
        &self.tournaments
    }

    pub fn modules(&self) -> &[StoredModule] {
        &self.modules
    }

    pub fn record_tick_trace(&mut self, trace: TickTrace) {
        self.tick_traces.push(trace);
    }

    pub fn tick_traces(&self) -> &[TickTrace] {
        &self.tick_traces
    }

    pub fn swarm_explain_last_tick(
        &self,
        world: &mut SwarmWorld,
        context: McpContext,
    ) -> TickExplanation {
        swarm_explain_last_tick_from_traces(world, context, &self.tick_traces)
    }

    fn verify_certificate_for_player(
        &self,
        certificate: &PlayerCertificate,
        player_id: PlayerId,
    ) -> Result<(), McpError> {
        if certificate.payload.player_id != player_id {
            return Err(McpError::invalid_params(
                "certificate player_id does not match context",
            ));
        }
        if certificate.payload.audience != CERTIFICATE_AUDIENCE {
            return Err(McpError::invalid_params("certificate audience is invalid"));
        }
        if certificate.payload.expires_at <= self.now_seconds() {
            return Err(McpError::invalid_params("certificate is expired"));
        }
        if self.revoked_certificates.contains(&certificate.signature) {
            return Err(McpError::invalid_params("certificate is revoked"));
        }
        self.issuer.verify(certificate)
    }

    fn now_seconds(&self) -> u64 {
        self.now_seconds.unwrap_or_else(unix_timestamp_seconds)
    }

    pub fn compile_module_for_tick(&mut self, module_id: &str) -> Result<CompiledModule, McpError> {
        let module = self
            .modules
            .iter_mut()
            .find(|module| module.module_id == module_id)
            .ok_or_else(|| McpError::invalid_params("module_id is not deployed"))?;

        let compiled = if module.wasmtime_version == wasmtime_version() {
            self.sandbox_runtime
                .compile_from_cached_native(&module.cached_native_module, &module.wasm_bytes)
        } else {
            self.sandbox_runtime.compile_cached_with_version(
                &mut self.module_cache,
                &module.wasm_bytes,
                &module.wasmtime_version,
            )
        }
        .map_err(|error| {
            McpError::invalid_params(format!("wasm module compile failed: {error}"))
        })?;

        if module.wasmtime_version != compiled.wasmtime_version()
            || module.wasm_hash != compiled.module_hash()
        {
            let refreshed = self
                .module_cache
                .get(&ModuleCacheKey::for_wasm(&module.wasm_bytes))
                .cloned()
                .ok_or_else(|| McpError::invalid_params("module cache refresh failed"))?;
            module.cached_native_module = refreshed;
            module.wasm_hash = compiled.module_hash().to_string();
            module.wasmtime_version = compiled.wasmtime_version().to_string();
        }

        Ok(compiled)
    }

    pub fn module_cache_stats(&self) -> swarm_wasm_sandbox::ModuleCacheStats {
        self.module_cache.stats()
    }

    pub fn swarm_profile(&self, world: &mut SwarmWorld, context: McpContext) -> ProfileResult {
        swarm_profile(world, context, self.modules.len())
    }
}

fn mcp_tool_infos() -> Vec<ToolInfo> {
    vec![
        ToolInfo {
            name: "swarm_get_snapshot".to_string(),
            description: "Get the visible world state for a player at the current tick".to_string(),
        },
        ToolInfo {
            name: "swarm_get_terrain".to_string(),
            description: "Get terrain type at room coordinates".to_string(),
        },
        ToolInfo {
            name: "swarm_get_world_rules".to_string(),
            description: "Get the world rules and mods configuration".to_string(),
        },
        ToolInfo {
            name: "swarm_get_schema".to_string(),
            description: "Get the CommandIntent JSON Schema".to_string(),
        },
        ToolInfo {
            name: "swarm_get_available_actions".to_string(),
            description: "List all WASM actions and MCP tools available to the player".to_string(),
        },
        ToolInfo {
            name: "swarm_explain_last_tick".to_string(),
            description: "Explain the last tick's results for a player".to_string(),
        },
        ToolInfo {
            name: "swarm_get_drone".to_string(),
            description: "Inspect full state for an owned or visible entity".to_string(),
        },
        ToolInfo {
            name: "swarm_get_room".to_string(),
            description: "Inspect a room visible to the player: drone count, structures, controller".to_string(),
        },
        ToolInfo { name: "swarm_get_drone".to_string(), description: "Get full state for an owned or visible drone".to_string() },
        ToolInfo { name: "swarm_get_room".to_string(), description: "Get a room visible to the player".to_string() },
        ToolInfo { name: "swarm_get_structure".to_string(), description: "Get visible structure state by id".to_string() },
        ToolInfo { name: "swarm_get_controller".to_string(), description: "Get visible controller state by id".to_string() },
        ToolInfo { name: "swarm_get_code".to_string(), description: "Get deployed code metadata for a drone".to_string() },
        ToolInfo { name: "swarm_get_visibility".to_string(), description: "Get visible rooms and entities".to_string() },
        ToolInfo { name: "swarm_get_path".to_string(), description: "Return a simple path between visible positions".to_string() },
        ToolInfo { name: "swarm_get_resources".to_string(), description: "Get visible player resources".to_string() },
        ToolInfo { name: "swarm_get_info".to_string(), description: "Get world metadata".to_string() },
        ToolInfo { name: "swarm_list_drones".to_string(), description: "List visible drones".to_string() },
        ToolInfo { name: "swarm_list_rooms".to_string(), description: "List visible rooms".to_string() },
        ToolInfo { name: "swarm_list_structures".to_string(), description: "List visible structures".to_string() },
        ToolInfo { name: "swarm_list_controllers".to_string(), description: "List visible controllers".to_string() },
        ToolInfo { name: "swarm_get_events".to_string(), description: "Get visible events".to_string() },
        ToolInfo { name: "swarm_get_messages".to_string(), description: "Get drone messages".to_string() },
        ToolInfo {
            name: "swarm_profile".to_string(),
            description: "Profile a player's current world state".to_string(),
        },
        ToolInfo {
            name: "swarm_get_economy".to_string(),
            description:
                "Summarize player economy income, expenses, storage tax, and maintenance"
                    .to_string(),
        },
        ToolInfo {
            name: "swarm_get_economy_trend".to_string(),
            description: "Return deterministic economy trend points for recent ticks".to_string(),
        },
        ToolInfo {
            name: "swarm_get_drone_efficiency".to_string(),
            description: "Estimate a drone efficiency percentage from fatigue, health, spawning, and carry state".to_string(),
        },
        ToolInfo {
            name: "swarm_get_leaderboard".to_string(),
            description: "Return leaderboard entries from ranking state and live world counts".to_string(),
        },
        ToolInfo {
            name: "swarm_list_market_orders".to_string(),
            description: "List market orders when a market order book is installed".to_string(),
        },
        ToolInfo {
            name: "swarm_sdk_fetch".to_string(),
            description: "Fetch a minimal SDK starter package for bot development".to_string(),
        },
        ToolInfo {
            name: "swarm_dry_run".to_string(),
            description: "Dry-run commands without mutating the world".to_string(),
        },
        ToolInfo {
            name: "swarm_simulate".to_string(),
            description: "Predict future ticks from a visible world snapshot without mutating live state".to_string(),
        },
        ToolInfo {
            name: "swarm_get_replay".to_string(),
            description: "Retrieve replay data as entity-change deltas between two ticks, anchored on the nearest keyframe".to_string(),
        },
        ToolInfo {
            name: "swarm_get_docs".to_string(),
            description: "Get Swarm documentation and reference material".to_string(),
        },
        ToolInfo {
            name: "resources/list".to_string(),
            description: "List available resource types".to_string(),
        },
        ToolInfo {
            name: "resources/read".to_string(),
            description: "Read resource definitions".to_string(),
        },
        ToolInfo {
            name: "swarm_auth_revoke".to_string(),
            description: "Revoke a session or certificate".to_string(),
        },
        ToolInfo { name: "swarm_auth_login".to_string(), description: "Authenticate a user or agent against the Auth control plane".to_string() },
        ToolInfo { name: "swarm_auth_logout".to_string(), description: "Terminate an Auth control-plane session".to_string() },
        ToolInfo { name: "swarm_auth_refresh".to_string(), description: "Refresh Auth control-plane session credentials".to_string() },
        ToolInfo { name: "swarm_auth_check".to_string(), description: "Check Auth control-plane session or certificate status".to_string() },
        ToolInfo { name: "swarm_auth_cert_issue".to_string(), description: "Issue an application-layer client/code certificate bundle".to_string() },
        ToolInfo { name: "swarm_auth_cert_list".to_string(), description: "List application-layer certificates for the caller".to_string() },
        ToolInfo { name: "swarm_auth_cert_revoke".to_string(), description: "Revoke an application-layer certificate".to_string() },
        ToolInfo { name: "swarm_auth_cert_rotate".to_string(), description: "Rotate Auth intermediate CA or caller certificate material".to_string() },
        ToolInfo { name: "swarm_auth_device_list".to_string(), description: "List registered Auth devices for the caller".to_string() },
        ToolInfo { name: "swarm_auth_device_register".to_string(), description: "Register an Auth device and key label".to_string() },
        ToolInfo { name: "swarm_get_world_config".to_string(), description: "Get world-level Auth and rules configuration metadata".to_string() },
        ToolInfo {
            name: "swarm_list_modules".to_string(),
            description: "List all deployed WASM modules across all players".to_string(),
        },
        ToolInfo {
            name: "swarm_deploy".to_string(),
            description: "Deploy a WASM module for a player".to_string(),
        },
        ToolInfo {
            name: "swarm_get_deploy_status".to_string(),
            description: "Inspect status and object-store pointer for a deployed module".to_string(),
        },
        ToolInfo {
            name: "swarm_list_deployments".to_string(),
            description: "List deployments, optionally filtered by player".to_string(),
        },
        ToolInfo {
            name: "swarm_validate_module".to_string(),
            description: "Validate a WASM module before deployment".to_string(),
        },
        ToolInfo {
            name: "swarm_tournament_precommit".to_string(),
            description:
            "Lock a previously deployed WASM module for an AI tournament before match start"
                .to_string(),
        },
        ToolInfo {
        name: "swarm_admin_challenge".to_string(),
        description: "Verify an admin challenge response and grant admin scope".to_string(),
        },
        ToolInfo {
        name: "swarm_admin_set_world_config".to_string(),
        description: "Accept an admin world configuration update".to_string(),
        },
        ToolInfo {
        name: "swarm_admin_rollback".to_string(),
        description: "Queue an admin rollback to a target tick".to_string(),
        },
        ToolInfo {
        name: "swarm_admin_ban_player".to_string(),
        description: "Apply an admin player ban".to_string(),
        },
        ToolInfo {
        name: "swarm_admin_force_gc".to_string(),
        description: "Run admin garbage collection for a requested scope".to_string(),
        },
        ToolInfo {
        name: "swarm_admin_get_audit_log".to_string(),
        description: "Read admin audit log entries".to_string(),
        },
        ToolInfo {
        name: "swarm_tournament_status".to_string(),
        description: "Inspect AI tournament preparation and locked-code status".to_string(),
        },
        ToolInfo {
            name: "swarm_tournament_create".to_string(),
            description: "Create and schedule a single- or double-elimination AI tournament from precommitted modules".to_string(),
        },
        ToolInfo {
            name: "swarm_match_result".to_string(),
            description: "Record a scheduled tournament match winner and advance the bracket".to_string(),
        },
    ]
}

fn mcp_tool_source(tool: &str) -> Option<CommandSource> {
    match tool {
        "swarm_deploy"
        | "swarm_validate_module"
        | "swarm_tournament_precommit"
        | "swarm_tournament_create" => Some(CommandSource::McpDeploy),
        "swarm_admin_challenge"
        | "swarm_admin_set_world_config"
        | "swarm_admin_rollback"
        | "swarm_admin_ban_player"
        | "swarm_admin_force_gc"
        | "swarm_admin_get_audit_log" => Some(CommandSource::Admin),
        "swarm_get_snapshot"
        | "swarm_get_drone"
        | "swarm_get_room"
        | "swarm_get_structure"
        | "swarm_get_controller"
        | "swarm_get_code"
        | "swarm_get_visibility"
        | "swarm_get_path"
        | "swarm_get_resources"
        | "swarm_get_info"
        | "swarm_list_drones"
        | "swarm_list_rooms"
        | "swarm_list_structures"
        | "swarm_list_controllers"
        | "swarm_get_events"
        | "swarm_get_messages"
        | "swarm_get_world_rules"
        | "swarm_get_schema"
        | "swarm_get_available_actions"
        | "swarm_explain_last_tick"
        | "swarm_profile"
        | "swarm_get_economy"
        | "swarm_get_economy_trend"
        | "swarm_get_drone_efficiency"
        | "swarm_get_leaderboard"
        | "swarm_list_market_orders"
        | "swarm_sdk_fetch"
        | "swarm_dry_run"
        | "swarm_get_docs"
        | "resources/list"
        | "resources/read"
        | "swarm_auth_revoke"
        | "swarm_tournament_status"
        | "swarm_match_result" => Some(CommandSource::McpQuery),
        "swarm_simulate" => Some(CommandSource::Simulate),
        "swarm_list_modules"
        | "swarm_get_deploy_status"
        | "swarm_list_deployments"
        | "swarm_get_replay" => Some(CommandSource::McpQuery),
        _ => None,
    }
}

fn swarm_admin_challenge(context: McpContext, params: &Value) -> Value {
    let scope = params
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("swarm:admin");
    json!({
        "granted": true,
        "scope": scope,
        "expiry": context.tick.saturating_add(300),
    })
}

fn swarm_admin_set_world_config(context: McpContext, _params: &Value) -> Value {
    json!({
        "accepted": true,
        "applied_at": context.tick,
    })
}

fn swarm_admin_rollback(context: McpContext, params: &Value) -> Value {
    let target_tick = params
        .get("target_tick")
        .and_then(Value::as_u64)
        .unwrap_or(context.tick);
    json!({
        "rollback_id": format!("rollback_{}_{}", context.player_id, target_tick),
        "state": "queued",
    })
}

fn swarm_admin_ban_player(context: McpContext, params: &Value) -> Value {
    let expiry = params
        .get("duration")
        .and_then(Value::as_u64)
        .map(|duration| context.tick.saturating_add(duration));
    json!({
        "banned": true,
        "expiry": expiry,
    })
}

fn swarm_admin_force_gc(_params: &Value) -> Value {
    json!({
        "freed_bytes": 0_u64,
        "duration": 0_u64,
    })
}

fn swarm_admin_get_audit_log(_params: &Value) -> Value {
    json!({
        "entries": [],
    })
}

fn auth_stub_result(tool: &str, context: McpContext, params: Value) -> Value {
    json!({
        "tool": tool,
        "status": "stubbed",
        "auth_control_plane": true,
        "player_id": context.player_id,
        "tick": context.tick,
        "params": params,
    })
}

fn tournament_tool_infos() -> Vec<ToolInfo> {
    mcp_tool_infos()
        .into_iter()
        .filter(|tool| {
            matches!(
                tool.name.as_str(),
                "swarm_get_snapshot"
                    | "swarm_get_terrain"
                    | "swarm_get_world_rules"
                    | "swarm_get_schema"
                    | "swarm_get_available_actions"
                    | "swarm_explain_last_tick"
                    | "swarm_get_drone"
                    | "swarm_profile"
                    | "swarm_dry_run"
                    | "swarm_simulate"
                    | "swarm_get_docs"
                    | "swarm_deploy"
                    | "swarm_validate_module"
                    | "swarm_tournament_precommit"
                    | "swarm_tournament_status"
                    | "swarm_tournament_create"
                    | "swarm_match_result"
            )
        })
        .collect()
}

fn tournament_bracket_status(id: &str, bracket: &TournamentBracket) -> TournamentBracketStatus {
    TournamentBracketStatus {
        tournament_id: id.to_string(),
        elimination: bracket.elimination,
        fixed_ticks: bracket.fixed_ticks,
        players: bracket
            .seeds
            .iter()
            .map(|seed| seed.code.player_id)
            .collect(),
        scheduled: bracket.scheduled.clone(),
        completed_matches: bracket.completed.len(),
        champion: bracket.champion,
        losses: bracket.losses.iter().map(|(k, v)| (*k, *v)).collect(),
    }
}

fn empty_tournament_replay() -> ArenaReplay {
    ArenaReplay {
        privacy: ReplayPrivacy::Public,
        public: true,
        traces: Vec::new(),
    }
}

fn arena_error_to_mcp(error: crate::arena::ArenaError) -> McpError {
    McpError::invalid_params(format!("arena tournament error: {error:?}"))
}

fn wasm_action_names() -> Vec<String> {
    [
        "Move", "Harvest", "Transfer", "Withdraw", "Attack", "Heal", "Spawn", "Build",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

pub fn swarm_get_available_actions(context: McpContext) -> AvailableActionsResult {
    AvailableActionsResult {
        tick: context.tick,
        player_id: context.player_id,
        wasm_actions: wasm_action_names(),
        mcp_tools: mcp_tool_infos(),
    }
}

pub fn swarm_get_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "CommandIntent",
        "type": "object",
        "additionalProperties": false,
        "required": ["sequence", "action"],
        "properties": {
            "sequence": {"type": "integer", "minimum": 0, "maximum": 4294967295_u64},
            "action": {"oneOf": command_action_schemas()},
        }
    })
}

fn command_action_schemas() -> Vec<Value> {
    vec![
        command_action_schema(
            "Move",
            &["object_id", "direction"],
            json!({"object_id": object_id_schema(), "direction": direction_schema()}),
        ),
        command_action_schema(
            "Harvest",
            &["object_id", "target_id"],
            json!({"object_id": object_id_schema(), "target_id": object_id_schema(), "resource": {"type": "string"}}),
        ),
        command_action_schema(
            "Transfer",
            &["object_id", "target_id", "resource", "amount"],
            json!({"object_id": object_id_schema(), "target_id": object_id_schema(), "resource": {"type": "string"}, "amount": amount_schema()}),
        ),
        command_action_schema(
            "Withdraw",
            &["object_id", "target_id", "resource", "amount"],
            json!({"object_id": object_id_schema(), "target_id": object_id_schema(), "resource": {"type": "string"}, "amount": amount_schema()}),
        ),
        command_action_schema(
            "Attack",
            &["object_id", "target_id"],
            json!({"object_id": object_id_schema(), "target_id": object_id_schema()}),
        ),
        command_action_schema(
            "RangedAttack",
            &["object_id", "target_id", "range"],
            json!({"object_id": object_id_schema(), "target_id": object_id_schema(), "range": {"type": "integer", "minimum": 1, "maximum": 50}}),
        ),
        command_action_schema(
            "Heal",
            &["object_id", "target_id"],
            json!({"object_id": object_id_schema(), "target_id": object_id_schema()}),
        ),
        command_action_schema(
            "ClaimController",
            &["object_id", "controller_id"],
            json!({"object_id": object_id_schema(), "controller_id": object_id_schema()}),
        ),
        command_action_schema(
            "Spawn",
            &["spawn_id", "body"],
            json!({"spawn_id": object_id_schema(), "body": {"type": "array", "items": body_part_schema(), "minItems": 1, "maxItems": crate::command::MAX_BODY_PARTS}}),
        ),
        command_action_schema(
            "Build",
            &["object_id", "x", "y", "structure"],
            json!({"object_id": object_id_schema(), "x": coord_schema(), "y": coord_schema(), "structure": structure_type_schema()}),
        ),
        command_action_schema(
            "TransferToGlobal",
            &["resource", "amount"],
            json!({"resource": {"type": "string"}, "amount": amount_schema()}),
        ),
        command_action_schema(
            "TransferFromGlobal",
            &["resource", "amount"],
            json!({"resource": {"type": "string"}, "amount": amount_schema()}),
        ),
        json!({"type": "object", "additionalProperties": false, "required": ["type", "object_id"], "properties": {"type": {"type": "string", "not": {"enum": wasm_action_names()}}, "object_id": object_id_schema(), "target_id": object_id_schema(), "resource": {"type": "string"}, "amount": amount_schema(), "structure": structure_type_schema()}}),
    ]
}

fn command_action_schema(action_type: &str, required: &[&str], properties: Value) -> Value {
    let mut required_fields = vec!["type".to_string()];
    required_fields.extend(required.iter().map(|field| (*field).to_string()));
    let mut map = serde_json::Map::new();
    map.insert("type".to_string(), json!({"const": action_type}));
    if let Some(properties) = properties.as_object() {
        map.extend(properties.clone());
    }
    json!({"type": "object", "additionalProperties": false, "required": required_fields, "properties": map})
}

fn object_id_schema() -> Value {
    json!({"type": "integer", "minimum": 0})
}
fn amount_schema() -> Value {
    json!({"type": "integer", "minimum": 1, "maximum": 4294967295_u64})
}
fn coord_schema() -> Value {
    json!({"type": "integer"})
}
fn direction_schema() -> Value {
    json!({"type": "string", "enum": ["Top", "TopRight", "BottomRight", "Bottom", "BottomLeft", "TopLeft"]})
}
fn body_part_schema() -> Value {
    json!({"type": "string", "enum": ["Move", "Work", "Carry", "Attack", "RangedAttack", "Heal", "Claim", "Tough"]})
}
fn structure_type_schema() -> Value {
    json!({"type": "string", "enum": ["Spawn", "Extension", "Tower", "Road", "Wall", "Rampart", "Storage", "Container", "Controller"]})
}

pub fn swarm_explain_last_tick(world: &mut SwarmWorld, context: McpContext) -> TickExplanation {
    swarm_explain_last_tick_from_traces(world, context, &[])
}

fn swarm_explain_last_tick_from_traces(
    world: &mut SwarmWorld,
    context: McpContext,
    traces: &[TickTrace],
) -> TickExplanation {
    let last_tick = context.tick.saturating_sub(1);
    let snapshot = swarm_get_snapshot(world, context.clone());
    let trace = traces.iter().rev().find(|trace| {
        trace.tick == last_tick && (trace.player_id == context.player_id || trace.player_id == 0)
    });
    let accepted = trace
        .map(|trace| {
            trace
                .commands
                .iter()
                .filter(|command| command.player_id == context.player_id)
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let rejected = trace
        .map(|trace| {
            trace
                .rejections
                .iter()
                .filter(|rejection| rejection.command.player_id == context.player_id)
                .map(|rejection| TickCommandRejection {
                    command: rejection.command.clone(),
                    rejection: rejection.rejection.clone(),
                    detail: rejection.detail.clone(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let notes = if let Some(trace) = trace {
        vec![format!(
            "Loaded tick trace from {} for {}",
            String::from_utf8_lossy(&tick_key(trace.tick, "commands")),
            if trace.player_id == 0 {
                "multi-player tick"
            } else {
                "player tick"
            }
        )]
    } else {
        vec![
            "No persisted tick trace is attached to this in-process MCP server".to_string(),
            "Call McpServer::record_tick_trace from the tick committer path to enable command explanations".to_string(),
        ]
    };
    TickExplanation {
        tick: last_tick,
        player_id: context.player_id,
        state_checksum: trace
            .map(|trace| trace.state_checksum)
            .unwrap_or_else(|| world.state_checksum()),
        visible_entity_count: snapshot.entities.len(),
        visible_tile_count: snapshot.visible_tiles.len(),
        accepted_commands: accepted.len(),
        rejected_commands: rejected.len(),
        accepted,
        rejected,
        notes,
    }
}

pub fn swarm_get_terrain(
    world: &SwarmWorld,
    params: TerrainParams,
) -> Result<TerrainResult, McpError> {
    let room = RoomId(params.room_id);
    let terrain = world
        .get_terrain(room, params.x, params.y)
        .ok_or_else(|| McpError::invalid_params("coordinates are outside a known room"))?;
    Ok(TerrainResult {
        x: params.x,
        y: params.y,
        room_id: params.room_id,
        terrain,
    })
}

pub fn swarm_get_objects_in_range(
    world: &mut SwarmWorld,
    context: McpContext,
    params: ObjectsInRangeParams,
) -> Result<ObjectsInRangeResult, McpError> {
    let origin = Position {
        x: params.x,
        y: params.y,
        room: RoomId(params.room_id),
    };
    if world.get_terrain(origin.room, origin.x, origin.y).is_none() {
        return Err(McpError::invalid_params(
            "coordinates are outside a known room",
        ));
    }
    let visible_positions = visible_positions(world.app.world_mut(), context.player_id);
    let visible_ids = visible_entity_ids(world.app.world_mut(), context.player_id, context.tick);
    let mut entities = visible_entities(world.app.world_mut(), &visible_positions, &visible_ids)
        .into_iter()
        .filter(|entity| mcp_hex_distance(origin, visible_entity_position(entity)) <= params.range)
        .collect::<Vec<_>>();
    entities.sort_by_key(entity_sort_key);
    Ok(ObjectsInRangeResult {
        origin: visible_position(origin),
        range: params.range,
        entities,
    })
}

fn world_view_code(params: Value) -> Result<Value, McpError> {
    let params: InspectEntityParams = serde_json::from_value(params)
        .map_err(|error| McpError::invalid_params(error.to_string()))?;
    Ok(json!({ "drone_id": params.object_id, "modules": Vec::<Value>::new() }))
}

fn world_view_messages(params: Value) -> Result<Value, McpError> {
    let params: InspectEntityParams = serde_json::from_value(params)
        .map_err(|error| McpError::invalid_params(error.to_string()))?;
    Ok(json!({ "drone_id": params.object_id, "messages": Vec::<Value>::new() }))
}

fn world_view_visibility(world: &mut SwarmWorld, context: McpContext) -> Value {
    let snapshot = swarm_get_snapshot(world, context);
    json!({ "visible_rooms": world_view_rooms(&snapshot), "visible_entities": snapshot.entities.len(), "visible_tiles": snapshot.visible_tiles.len() })
}

fn world_view_resources(world: &mut SwarmWorld, context: McpContext) -> Value {
    let snapshot = swarm_get_snapshot(world, context);
    json!({ "resources": snapshot.local_storage, "storage": snapshot.global_storage, "pending_global_transfers": snapshot.pending_global_transfers })
}

fn world_view_info(world: &mut SwarmWorld, context: McpContext) -> Value {
    let snapshot = swarm_get_snapshot(world, context);
    json!({ "version": env!("CARGO_PKG_VERSION"), "tick_rate": 1, "world_name": "swarm", "player_count": 1, "tick": snapshot.tick })
}

fn world_view_list(world: &mut SwarmWorld, context: McpContext, kind: &str) -> Value {
    let snapshot = swarm_get_snapshot(world, context);
    match kind {
        "drones" => {
            json!({ "drones": snapshot.entities.into_iter().filter(|entity| matches!(entity, VisibleEntity::Drone(_))).collect::<Vec<_>>() })
        }
        "structures" => {
            json!({ "structures": snapshot.entities.into_iter().filter(|entity| matches!(entity, VisibleEntity::Structure(_))).collect::<Vec<_>>() })
        }
        "controllers" => {
            json!({ "controllers": snapshot.entities.into_iter().filter(|entity| matches!(entity, VisibleEntity::Controller(_))).collect::<Vec<_>>() })
        }
        "rooms" => json!({ "rooms": world_view_rooms(&snapshot) }),
        _ => json!({}),
    }
}

fn world_view_rooms(snapshot: &VisibleWorldSnapshot) -> Vec<Value> {
    snapshot
        .visible_tiles
        .iter()
        .map(|tile| tile.room_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|room_id| json!({ "id": room_id, "level": 0, "controller_level": null }))
        .collect()
}

fn world_view_path(params: Value) -> Value {
    json!({ "path": Vec::<Value>::new(), "distance": 0, "cost": 0, "params": params })
}

pub fn swarm_get_drone(
    world: &mut SwarmWorld,
    context: McpContext,
    params: InspectEntityParams,
) -> Result<FullEntityState, McpError> {
    let entity = Entity::from_bits(params.object_id);
    let (
        position,
        owner,
        drone,
        structure,
        source,
        resource,
        terrain,
        controller,
        marked_for_death,
    ) = {
        let entity_ref = world
            .app
            .world()
            .get_entity(entity)
            .map_err(|_| McpError::invalid_params("entity is not visible or does not exist"))?;
        (
            entity_ref.get::<Position>().copied(),
            entity_ref.get::<Owner>().copied(),
            entity_ref.get::<Drone>().cloned(),
            entity_ref.get::<Structure>().cloned(),
            entity_ref.get::<Source>().cloned(),
            entity_ref.get::<crate::components::Resource>().cloned(),
            entity_ref.get::<Terrain>().copied(),
            entity_ref.get::<Controller>().cloned(),
            entity_ref.contains::<MarkedForDeath>(),
        )
    };
    let visible = position
        .is_some_and(|position| is_visible_to(world.app.world_mut(), context.player_id, position));
    let owned = owner.is_some_and(|owner| owner.0 == context.player_id)
        || drone
            .as_ref()
            .is_some_and(|drone| drone.owner == context.player_id)
        || structure
            .as_ref()
            .is_some_and(|structure| structure.owner == Some(context.player_id))
        || controller
            .as_ref()
            .is_some_and(|controller| controller.owner == Some(context.player_id));
    if !visible && !owned {
        return Err(McpError::invalid_params(
            "entity is not visible or does not exist",
        ));
    }
    Ok(FullEntityState {
        id: params.object_id,
        position: position.map(visible_position),
        owner: owner.map(|owner| owner.0),
        drone,
        structure,
        source,
        resource,
        terrain: terrain.map(|terrain| terrain.0),
        controller,
        marked_for_death,
    })
}

pub fn swarm_validate_module(params: ValidateModuleParams) -> ValidateModuleResult {
    let mut issues = Vec::new();
    let wasm_bytes = match decode_base64(&params.wasm_bytes) {
        Ok(bytes) => bytes,
        Err(error) => {
            issues.push(error.message);
            Vec::new()
        }
    };
    if wasm_bytes.is_empty() {
        issues.push("wasm_bytes is empty".to_string());
    }
    if wasm_bytes.len() > MAX_WASM_BYTES {
        issues.push("wasm module exceeds max size".to_string());
    }
    if !wasm_bytes.is_empty() && !wasm_bytes.starts_with(b"\0asm") {
        issues.push("wasm_bytes must start with the WebAssembly magic header".to_string());
    }
    if wasm_bytes.len() >= 8 && &wasm_bytes[4..8] != b"\x01\0\0\0" {
        issues.push("wasm module version must be 1".to_string());
    }
    if wasm_bytes.len() > 8 {
        validate_wasm_sections(&wasm_bytes, &mut issues);
    }
    let wasm_hash =
        (!wasm_bytes.is_empty()).then(|| blake3::hash(&wasm_bytes).to_hex().to_string());
    let estimated_fuel = u64::try_from(wasm_bytes.len())
        .unwrap_or(u64::MAX)
        .saturating_mul(10);
    ValidateModuleResult {
        valid: issues.is_empty(),
        wasm_hash,
        size_bytes: wasm_bytes.len(),
        issues,
        estimated_fuel,
    }
}

fn validate_wasm_sections(bytes: &[u8], issues: &mut Vec<String>) {
    if bytes.len() < 8 {
        return;
    }
    let mut offset = 8;
    let mut last_known_section = 0_u8;
    while offset < bytes.len() {
        let section_id = bytes[offset];
        offset += 1;
        let Some(section_size) = read_uleb_u32(bytes, &mut offset) else {
            issues.push("wasm section has invalid LEB128 size".to_string());
            return;
        };
        let section_size = section_size as usize;
        let Some(section_end) = offset.checked_add(section_size) else {
            issues.push("wasm section size overflows".to_string());
            return;
        };
        if section_end > bytes.len() {
            issues.push("wasm section extends past end of module".to_string());
            return;
        }
        if section_id > 12 {
            issues.push(format!("unknown wasm section id {section_id}"));
        }
        if section_id != 0 {
            if section_id <= last_known_section {
                issues.push("wasm sections are out of order".to_string());
            }
            last_known_section = section_id;
        }
        offset = section_end;
    }
}

fn read_uleb_u32(bytes: &[u8], offset: &mut usize) -> Option<u32> {
    let mut result = 0_u32;
    let mut shift = 0;
    for _ in 0..5 {
        let byte = *bytes.get(*offset)?;
        *offset += 1;
        result |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
    }
    None
}

pub fn swarm_profile(
    world: &mut SwarmWorld,
    context: McpContext,
    deployed_modules: usize,
) -> ProfileResult {
    let snapshot = swarm_get_snapshot(world, context.clone());
    let owned_visible_drones = snapshot.entities.iter().filter(|entity| matches!(entity, VisibleEntity::Drone(drone) if drone.owner == context.player_id)).count();
    let owned_visible_structures = snapshot.entities.iter().filter(|entity| matches!(entity, VisibleEntity::Structure(structure) if structure.owner == Some(context.player_id))).count();
    ProfileResult {
        tick: context.tick,
        player_id: context.player_id,
        deployed_modules,
        pending_modules: deployed_modules,
        owned_visible_drones,
        owned_visible_structures,
        available_mcp_tools: mcp_tool_infos().len(),
        direct_gameplay_tools_enabled: false,
    }
}

pub fn swarm_dry_run(
    world: &mut SwarmWorld,
    context: McpContext,
    params: DryRunCommandsParams,
) -> DryRunCommandsResult {
    let state_checksum_before = world.state_checksum();
    let mut results = Vec::new();
    for intent in params.commands {
        let raw = RawCommand {
            player_id: context.player_id,
            tick: context.tick,
            source: CommandSource::DryRun,
            auth: CommandAuth {
                source: CommandSource::DryRun,
                player_id: context.player_id,
                tick_submitted: context.tick,
                tick_target: context.tick,
            },
            sequence: intent.sequence,
            action: intent.action,
        };
        let sequence = raw.sequence;
        let cmd = raw.clone();
        match validate_command(world.app.world_mut(), cmd) {
            Ok(_) => results.push(DryRunCommandResult {
                sequence,
                command: raw,
                accepted: true,
                rejection: None,
            }),
            Err(rejection) => results.push(DryRunCommandResult {
                sequence,
                command: raw,
                accepted: false,
                rejection: Some(rejection),
            }),
        }
    }
    let state_checksum_after = world.state_checksum();
    DryRunCommandsResult {
        tick: context.tick,
        player_id: context.player_id,
        commands: results,
        state_checksum_before,
        state_checksum_after,
        mutated_world: state_checksum_before != state_checksum_after,
    }
}

pub fn swarm_simulate(params: SimulateParams) -> Result<SimulateResult, McpError> {
    const MAX_SIMULATE_TICKS: Tick = 10_000;
    if params.ticks > MAX_SIMULATE_TICKS {
        return Err(McpError::invalid_params(format!(
            "ticks must be <= {MAX_SIMULATE_TICKS}"
        )));
    }

    let before = params.snapshot;
    let mut after = before.clone();
    after.tick = before.tick.saturating_add(params.ticks);

    simulate_visible_snapshot(&mut after, params.ticks);

    let diff = SimulatedStateDiff {
        tick_before: before.tick,
        tick_after: after.tick,
        local_storage_before: before.local_storage.clone(),
        local_storage_after: after.local_storage.clone(),
        global_storage_before: before.global_storage.clone(),
        global_storage_after: after.global_storage.clone(),
        pending_global_transfers_before: before.pending_global_transfers.clone(),
        pending_global_transfers_after: after.pending_global_transfers.clone(),
        entity_changes: visible_entity_diff(&before.entities, &after.entities),
        state_changed: before != after,
    };

    Ok(SimulateResult {
        player_id: after.player_id,
        room_id: after.room_id,
        from_tick: before.tick,
        to_tick: after.tick,
        ticks: params.ticks,
        predicted_snapshot: after,
        diff,
    })
}

fn simulate_visible_snapshot(snapshot: &mut VisibleWorldSnapshot, ticks: Tick) {
    if ticks == 0 {
        return;
    }

    for entity in &mut snapshot.entities {
        simulate_visible_entity(entity, ticks);
    }

    let mut remaining_transfers = Vec::new();
    for mut transfer in std::mem::take(&mut snapshot.pending_global_transfers) {
        transfer.remaining_ticks = transfer.remaining_ticks.saturating_sub(ticks);
        if transfer.remaining_ticks == 0 {
            match transfer.direction.as_str() {
                "ToGlobal" => add_simulated_resource(
                    &mut snapshot.global_storage,
                    transfer.resource,
                    transfer.deliver_amount,
                ),
                "FromGlobal" => add_simulated_resource(
                    &mut snapshot.local_storage,
                    transfer.resource,
                    transfer.deliver_amount,
                ),
                _ => remaining_transfers.push(transfer),
            }
        } else {
            remaining_transfers.push(transfer);
        }
    }
    snapshot.pending_global_transfers = remaining_transfers;
}

fn simulate_visible_entity(entity: &mut VisibleEntity, ticks: Tick) {
    let ticks = u32::try_from(ticks).unwrap_or(u32::MAX);
    match entity {
        VisibleEntity::Drone(drone) => {
            drone.fatigue = drone.fatigue.saturating_sub(ticks);
        }
        VisibleEntity::Structure(structure) => {
            structure.cooldown = structure.cooldown.saturating_sub(ticks);
        }
        VisibleEntity::Source(source) => {
            source.ticks_to_regeneration = source.ticks_to_regeneration.saturating_sub(ticks);
        }
        VisibleEntity::Resource(_) => {}
        VisibleEntity::Controller(controller) => {
            controller.safe_mode = controller.safe_mode.saturating_sub(ticks);
        }
    }
}

fn add_simulated_resource(storage: &mut BTreeMap<String, u32>, resource: String, amount: u32) {
    let current = storage.entry(resource).or_default();
    *current = current.saturating_add(amount);
}

fn visible_entity_diff(
    before: &[VisibleEntity],
    after: &[VisibleEntity],
) -> Vec<SimulatedEntityChange> {
    fn entity_id(entity: &VisibleEntity) -> ObjectId {
        match entity {
            VisibleEntity::Drone(drone) => drone.id,
            VisibleEntity::Structure(structure) => structure.id,
            VisibleEntity::Source(source) => source.id,
            VisibleEntity::Resource(resource) => resource.id,
            VisibleEntity::Controller(controller) => controller.id,
        }
    }

    let before_by_id = before
        .iter()
        .cloned()
        .map(|entity| (entity_id(&entity), entity))
        .collect::<BTreeMap<_, _>>();
    let after_by_id = after
        .iter()
        .cloned()
        .map(|entity| (entity_id(&entity), entity))
        .collect::<BTreeMap<_, _>>();
    before_by_id
        .keys()
        .chain(after_by_id.keys())
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|id| {
            let before = before_by_id.get(&id).cloned();
            let after = after_by_id.get(&id).cloned();
            (before != after).then_some(SimulatedEntityChange { id, before, after })
        })
        .collect()
}

pub fn swarm_get_docs(params: DocsParams) -> DocsResult {
    let topic = normalize_docs_topic(&params.topic);
    let sections = match topic.as_str() {
        "mcp" => vec![
            docs_section(
                "MCP contract",
                "MCP exposes read/debug/deploy tools, but never direct gameplay actions such as move, attack, build, or spawn. MCP is not a game controller; world state changes only through validated WASM CommandIntent output.",
            ),
            docs_section(
                "Eight phase-2 tools",
                &mcp_tool_infos()
                    .iter()
                    .map(|tool| format!("{}: {}", tool.name, tool.description))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            docs_section("Resource tree", &docs_resource_uris().join("\n")),
        ],
        "commands" => vec![docs_section(
            "WASM CommandIntent actions",
            &wasm_action_names().join(", "),
        )],
        "api" => api_reference_sections(),
        "basic-agent" => basic_agent_tutorial_sections(),
        "tournament" => tournament_docs_sections(),
        _ => vec![
            docs_section(
                "Swarm docs",
                "Topics: mcp, commands, api, basic-agent. MCP resources include swarm://docs/tutorials/basic-agent for a complete 30-minute AI player deployment tutorial.",
            ),
            docs_section("Resource URIs", &docs_resource_uris().join("\n")),
        ],
    };
    DocsResult { topic, sections }
}

fn normalize_docs_topic(topic: &str) -> String {
    match topic.trim() {
        "swarm://docs/tutorials/basic-agent"
        | "swarm://docs/tutorial/quickstart.md"
        | "tutorials/basic-agent"
        | "tutorial/basic-agent"
        | "basic-agent" => "basic-agent".to_string(),
        "swarm://docs/tournament/ai.md" | "tournament" | "ai-tournament" => {
            "tournament".to_string()
        }
        "swarm://docs/api/reference.md" | "api/reference" | "api" => "api".to_string(),
        "swarm://docs/security/mcp-contract.md" | "security/mcp-contract" | "mcp" => {
            "mcp".to_string()
        }
        "swarm://docs/api/commands" | "commands" => "commands".to_string(),
        "" | "swarm://docs/README.md" | "overview" => "overview".to_string(),
        other => other.to_string(),
    }
}

fn docs_section(title: &str, body: &str) -> DocsSection {
    DocsSection {
        title: title.to_string(),
        body: body.to_string(),
    }
}

fn docs_resources_list() -> Value {
    json!({
        "resources": docs_resource_uris().into_iter().map(|uri| json!({
            "uri": uri,
            "name": docs_resource_name(uri),
            "description": docs_resource_description(uri),
            "mimeType": "text/markdown",
        })).collect::<Vec<_>>()
    })
}

fn docs_resources_read(params: ResourceReadParams) -> Result<Value, McpError> {
    let text = docs_markdown_for_uri(&params.uri).ok_or_else(|| {
        McpError::invalid_params(format!("unknown docs resource uri: {}", params.uri))
    })?;
    Ok(json!({"contents": [{"uri": params.uri, "mimeType": "text/markdown", "text": text}]}))
}

fn docs_resource_uris() -> Vec<&'static str> {
    vec![
        "swarm://docs/README.md",
        "swarm://docs/tutorial/quickstart.md",
        "swarm://docs/tutorials/basic-agent",
        "swarm://docs/api/reference.md",
        "swarm://docs/api/commands/Move.md",
        "swarm://docs/api/commands/Harvest.md",
        "swarm://docs/api/commands/Transfer.md",
        "swarm://docs/api/commands/Spawn.md",
        "swarm://docs/api/commands/Build.md",
        "swarm://docs/security/mcp-contract.md",
        "swarm://docs/tournament/ai.md",
    ]
}

fn docs_resource_name(uri: &str) -> &'static str {
    match uri {
        "swarm://docs/tutorials/basic-agent" => "Basic AI agent tutorial",
        "swarm://docs/api/reference.md" => "Game API reference",
        "swarm://docs/security/mcp-contract.md" => "MCP security contract",
        "swarm://docs/tournament/ai.md" => "AI tournament precommit guide",
        "swarm://docs/tutorial/quickstart.md" => "Quickstart",
        "swarm://docs/README.md" => "Documentation index",
        "swarm://docs/api/commands/Move.md" => "Move command",
        "swarm://docs/api/commands/Harvest.md" => "Harvest command",
        "swarm://docs/api/commands/Transfer.md" => "Transfer command",
        "swarm://docs/api/commands/Spawn.md" => "Spawn command",
        "swarm://docs/api/commands/Build.md" => "Build command",
        _ => "Swarm docs resource",
    }
}

fn docs_resource_description(uri: &str) -> &'static str {
    match uri {
        "swarm://docs/tutorials/basic-agent" => {
            "Complete zero-to-deploy tutorial for an AI player agent, designed for completion within 30 minutes."
        }
        "swarm://docs/api/reference.md" => {
            "CommandIntent and MCP API reference derived from the Phase 0 IDL contract."
        }
        "swarm://docs/security/mcp-contract.md" => {
            "Safety boundary: MCP read/debug/deploy tools only; no direct gameplay actions."
        }
        "swarm://docs/tournament/ai.md" => {
            "AI tournament preparation flow: deploy WASM during prep, precommit to lock module_id + BLAKE3 hash before match start."
        }
        _ => "Markdown documentation for Swarm AI agents.",
    }
}

fn docs_markdown_for_uri(uri: &str) -> Option<String> {
    match uri {
        "swarm://docs/README.md" => Some(docs_sections_markdown(&swarm_get_docs(DocsParams {
            topic: "overview".to_string(),
        }))),
        "swarm://docs/tutorial/quickstart.md" | "swarm://docs/tutorials/basic-agent" => {
            Some(docs_sections_markdown(&swarm_get_docs(DocsParams {
                topic: "basic-agent".to_string(),
            })))
        }
        "swarm://docs/api/reference.md" => {
            Some(docs_sections_markdown(&swarm_get_docs(DocsParams {
                topic: "api".to_string(),
            })))
        }
        "swarm://docs/security/mcp-contract.md" => {
            Some(docs_sections_markdown(&swarm_get_docs(DocsParams {
                topic: "mcp".to_string(),
            })))
        }
        "swarm://docs/tournament/ai.md" => {
            Some(docs_sections_markdown(&swarm_get_docs(DocsParams {
                topic: "tournament".to_string(),
            })))
        }
        "swarm://docs/api/commands/Move.md" => Some(command_doc(
            "Move",
            "params: object_id: ObjectId, direction: Direction",
            "validator: exists, owner, drone, fatigue, body_part(Move), passable, !spawning",
            "MCP cannot call Move directly. Emit this as a WASM CommandIntent action from tick().",
        )),
        "swarm://docs/api/commands/Harvest.md" => Some(command_doc(
            "Harvest",
            "params: object_id: ObjectId, target_id: ObjectId, resource: ResourceName?",
            "validator: exists, owner, drone, body_part(Work,Carry), carry_space, is_source, source_not_empty, in_range(1)",
            "MCP cannot call Harvest directly. Use swarm_dry_run, then return CommandIntent from WASM.",
        )),
        "swarm://docs/api/commands/Transfer.md" => Some(command_doc(
            "Transfer",
            "params: object_id: ObjectId, target_id: ObjectId, resource: ResourceName, amount: ResourceAmount",
            "validator: exists, owner, drone, body_part(Carry), has_resource, target_has_space, in_range(1)",
            "MCP cannot call Transfer directly. Use validated WASM CommandIntent output.",
        )),
        "swarm://docs/api/commands/Spawn.md" => Some(command_doc(
            "Spawn",
            "params: spawn_id: ObjectId, body: BodyPart[]",
            "validator: owned spawn, body size, resource cost, room capacity, cooldown",
            "MCP cannot call Spawn directly; deploy a WASM agent that emits a Spawn CommandIntent.",
        )),
        "swarm://docs/api/commands/Build.md" => Some(command_doc(
            "Build",
            "params: object_id: ObjectId, x: i32, y: i32, structure: StructureType",
            "validator: exists, owner, drone, body_part(Work,Carry), in_your_room, tile_empty, plain_terrain, in_range(3)",
            "MCP cannot call Build directly; use dry-run feedback before deploying build logic.",
        )),
        _ => None,
    }
}

fn docs_sections_markdown(docs: &DocsResult) -> String {
    docs.sections
        .iter()
        .map(|section| format!("# {}\n\n{}", section.title, section.body))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn command_doc(name: &str, params: &str, validator: &str, note: &str) -> String {
    format!(
        "# {name}\n\n{params}\n\n{validator}\n\nSecurity note: {note}\n\nCommandIntent shape: {{\"sequence\":1,\"action\":{{\"type\":\"{name}\"}}}}"
    )
}

fn tournament_docs_sections() -> Vec<DocsSection> {
    vec![
        docs_section(
            "AI tournament model",
            "AI tournaments use WASM pre-submission. During preparation, agents may inspect the world, dry-run CommandIntent JSON, compile/sign WASM, and call swarm_deploy. Before match start they must call swarm_tournament_precommit with a module_id; the server locks that module_id plus its BLAKE3 wasm_hash and version_tag.",
        ),
        docs_section(
            "Locked-code rule",
            "After precommit, swarm_deploy rejects further uploads for that player until the tournament match ends. The match executor uses the locked module snapshot, making the contest a deterministic pre-submitted WASM match rather than an in-match code-editing loop.",
        ),
        docs_section(
            "AI MCP interface",
            "Tournament MCP tools are read/debug/deploy/precommit/create/status/result only: swarm_get_snapshot, swarm_get_world_rules, swarm_get_available_actions, swarm_explain_last_tick, swarm_profile, swarm_dry_run, swarm_get_docs, swarm_deploy, swarm_tournament_precommit, swarm_tournament_create, swarm_tournament_status, and swarm_match_result. No swarm_move, swarm_attack, swarm_build, or other direct gameplay MCP tools exist.",
        ),
    ]
}

fn api_reference_sections() -> Vec<DocsSection> {
    vec![
        docs_section(
            "Game API IDL v1.0.0",
            "P0-8 Game API IDL is the source of truth for WASM host functions, CommandIntent, validators, SDK types, MCP schemas, and docs resources. CommandIntent has exactly sequence + action; RawCommand envelope fields are injected by the server Source Gate.",
        ),
        docs_section(
            "CommandIntent",
            "WASM tick() returns CommandIntent objects, not RawCommand envelopes. Each object has only sequence and action. The server injects player_id, source, auth, and tick before validation.",
        ),
        docs_section(
            "Move",
            "object_id: ObjectId\ndirection: Direction\nvalidator: exists, owner, drone, fatigue, body_part(Move), passable, !spawning",
        ),
        docs_section(
            "Harvest",
            "object_id: ObjectId\ntarget_id: ObjectId\nresource: ResourceName?\nvalidator: exists, owner, drone, body_part(Work,Carry), carry_space, is_source, source_not_empty, in_range(1)",
        ),
        docs_section(
            "Transfer",
            "object_id: ObjectId\ntarget_id: ObjectId\nresource: ResourceName\namount: ResourceAmount\nvalidator: exists, owner, drone, body_part(Carry), has_resource, target_has_space, in_range(1)",
        ),
        docs_section(
            "Deploy security",
            "Deploy requires OAuth-derived player certificate, Ed25519 wasm_signature over the BLAKE3 wasm hash, max module size enforcement, and next-tick loading.",
        ),
    ]
}

fn basic_agent_tutorial_sections() -> Vec<DocsSection> {
    vec![
        docs_section(
            "Goal",
            "Build and deploy a safe AI player from zero within 30 minutes. The loop is LEARN -> DECIDE -> ACT -> UNDERSTAND: read MCP resources, inspect visible state, dry-run CommandIntent candidates, deploy a signed WASM module, then use explanations and P0-6 feedback to improve.",
        ),
        docs_section(
            "30 minute checklist",
            "0-5 min: call resources/list and read swarm://docs/tutorials/basic-agent plus api/reference. 5-10 min: read the certificate issuance flow and call swarm_get_available_actions. 10-15 min: inspect swarm_get_snapshot and swarm_profile. 15-20 min: dry-run Spawn/Harvest/Transfer/Build CommandIntent JSON. 20-25 min: compile/sign a WASM module. 25-30 min: call swarm_deploy and confirm pending_next_tick, then inspect swarm_explain_last_tick.",
        ),
        docs_section(
            "1. Learn the contract",
            r#"MCP is not a game controller. There are no direct gameplay MCP tools. world state changes only through WASM sandbox execution. Use swarm_get_docs({topic:"api"}) or swarm://docs/api/reference.md for P0-8 CommandIntent details."#,
        ),
        docs_section(
            "2. Authenticate for deploy",
            "Generate an Ed25519 client key, then use the certificate issuance flow with provider, subject, access_token, and client_public_key. The result is a 24h player certificate for audience swarm-wasm-deploy. Keep the private key local; sign the BLAKE3 hash of the wasm bytes for swarm_deploy.",
        ),
        docs_section(
            "3. Inspect state",
            "Call swarm_get_snapshot for visible tiles, entities, local_storage, global_storage, and pending_global_transfers. Call swarm_profile for owned visible drones/structures and deployed module count. The snapshot respects fog_of_war/player_view visibility.",
        ),
        docs_section(
            "4. Plan CommandIntent output",
            r#"WASM tick() returns CommandIntent objects, not RawCommand envelopes. Each object has only sequence and action. The server injects player_id, source, and tick. Example Spawn: {"sequence":1,"action":{"type":"Spawn","spawn_id":42,"body":["Move","Work","Carry"]}}."#,
        ),
        docs_section(
            "5. Starter harvesting loop",
            r#"For each tick: choose the nearest visible source or resource drop; if a Work+Carry drone is adjacent and has free capacity, emit {"action":{"type":"Harvest","object_id":100,"target_id":200,"resource":"Energy"}}. If carrying Energy and adjacent to spawn/storage, emit {"action":{"type":"Transfer","object_id":100,"target_id":42,"resource":"Energy","amount":50}}."#,
        ),
        docs_section(
            "6. Dry-run before deploy",
            "Call swarm_dry_run with candidate CommandIntent objects. Dry-run validates commands and returns accepted/rejection without applying a tick. Treat rejection reasons as compiler errors for behavior: ObjectNotFound, NotOwner, OutOfRange, InsufficientResource, TargetFull, SpawnOnCooldown, or RoomDroneCapReached.",
        ),
        docs_section(
            "7. Deploy",
            "Compile a small WASM module exporting tick(). Base64 encode bytes, compute BLAKE3 hash, sign the hash with the Ed25519 client key in the certificate, and call swarm_deploy with language, version_tag, room_id, wasm_bytes, certificate, and wasm_signature. Success returns status pending_next_tick.",
        ),
        docs_section(
            "8. Understand feedback (P0-6)",
            "After the next tick, call swarm_explain_last_tick. Use accepted/rejected command counts, rejection detail, resource deltas, timeout/fuel signals, onboarding achievements, and replay tools to close the P0-6 MVP feedback loop.",
        ),
        docs_section(
            "Security invariants",
            "Direct gameplay MCP tools remain disabled: no swarm_move, swarm_harvest, swarm_build, swarm_spawn, swarm_attack, swarm_heal, swarm_transfer, or swarm_withdraw. MCP may read, dry-run, explain, authenticate, and deploy only. All real actions come from validated WASM CommandIntent output.",
        ),
        docs_section(
            "Minimal JSON examples",
            r#"Spawn: {"sequence":1,"action":{"type":"Spawn","spawn_id":42,"body":["Move","Work","Carry"]}}
Harvest: {"sequence":2,"action":{"type":"Harvest","object_id":100,"target_id":200,"resource":"Energy"}}
Transfer: {"sequence":3,"action":{"type":"Transfer","object_id":100,"target_id":42,"resource":"Energy","amount":50}}"#,
        ),
        docs_section(
            "Done criteria",
            "The agent has read docs, authenticated, inspected visible state, dry-run at least one command, deployed a signed WASM module, observed pending_next_tick, then used swarm_explain_last_tick or replay feedback to improve its next tick. This is the intended 30-minute zero-to-deploy path.",
        ),
    ]
}

fn default_docs_topic() -> String {
    "overview".to_string()
}

pub fn swarm_get_snapshot(world: &mut SwarmWorld, context: McpContext) -> VisibleWorldSnapshot {
    let snapshot = build_visible_snapshot(world, context.clone());
    let key = SnapshotKey::new(context.player_id, context.tick);
    world
        .app
        .world_mut()
        .resource_mut::<crate::fdb::FoundationDbStore>()
        .write_visible_snapshot(snapshot.clone());

    world
        .app
        .world_mut()
        .resource_scope(
            |ecs, mut cache: Mut<'_, crate::dragonfly::DragonflyCache>| {
                let store = ecs.resource::<crate::fdb::FoundationDbStore>();
                read_through_dragonfly(&mut *cache, key, store)
            },
        )
        .unwrap_or(snapshot)
}

fn build_visible_snapshot(world: &mut SwarmWorld, context: McpContext) -> VisibleWorldSnapshot {
    let room_id = RoomId(0);
    let visible_positions = visible_positions(world.app.world_mut(), context.player_id);
    let terrains = world.app.world().resource::<RoomTerrains>();
    let mut visible_tiles = terrains
        .0
        .iter()
        .flat_map(|(room_id, room)| {
            let visible_positions = &visible_positions;
            room.iter().filter_map(move |(x, y, terrain)| {
                visible_positions
                    .contains(&(*room_id, x, y))
                    .then_some(VisibleTile {
                        x,
                        y,
                        room_id: room_id.0,
                        terrain,
                    })
            })
        })
        .collect::<Vec<_>>();
    visible_tiles.sort();

    let visible_ids = visible_entity_ids(world.app.world_mut(), context.player_id, context.tick);
    let mut entities = visible_entities(world.app.world_mut(), &visible_positions, &visible_ids);
    entities.sort_by_key(entity_sort_key);

    VisibleWorldSnapshot {
        tick: context.tick,
        player_id: context.player_id,
        room_id: room_id.0,
        visibility_radius: VISIBILITY_RADIUS,
        visible_tiles,
        entities,
        local_storage: player_storage_snapshot(
            &world.app.world().resource::<PlayerLocalStorage>().0,
            context.player_id,
        ),
        global_storage: player_storage_snapshot(
            &world.app.world().resource::<PlayerGlobalStorage>().0,
            context.player_id,
        ),
        pending_global_transfers: world
            .app
            .world()
            .resource::<PendingGlobalTransfers>()
            .0
            .iter()
            .filter(|transfer| transfer.player_id == context.player_id)
            .map(|transfer| VisiblePendingGlobalTransfer {
                player_id: transfer.player_id,
                direction: format!("{:?}", transfer.direction),
                resource: transfer.resource.clone(),
                amount: transfer.amount,
                deliver_amount: transfer.deliver_amount,
                remaining_ticks: transfer.remaining_ticks,
            })
            .collect(),
    }
}

fn player_storage_snapshot(
    storage: &indexmap::IndexMap<PlayerId, indexmap::IndexMap<String, u32>>,
    player_id: PlayerId,
) -> BTreeMap<String, u32> {
    storage
        .get(&player_id)
        .map(|amounts| {
            amounts
                .iter()
                .map(|(name, amount)| (name.clone(), *amount))
                .collect()
        })
        .unwrap_or_default()
}

pub fn swarm_get_world_rules() -> WorldRules {
    let mut engine_config = BTreeMap::new();
    engine_config.insert("mcp_direct_gameplay_actions".to_string(), json!(false));
    engine_config.insert(
        "snapshot_visibility".to_string(),
        json!("player_visible_tiles_only"),
    );

    let mut base_config = BTreeMap::new();
    base_config.insert(
        "max_body_parts".to_string(),
        json!(crate::command::MAX_BODY_PARTS),
    );
    base_config.insert(
        "max_commands_per_player".to_string(),
        json!(crate::command::MAX_COMMANDS_PER_PLAYER),
    );
    base_config.insert(
        "max_drones_per_player".to_string(),
        json!(crate::command::MAX_DRONES_PER_PLAYER),
    );

    WorldRules {
        ruleset: "phase1".to_string(),
        room_size: DEFAULT_ROOM_SIZE,
        visibility_radius: VISIBILITY_RADIUS,
        max_wasm_bytes: MAX_WASM_BYTES,
        active_mods: vec![
            WorldRuleMod {
                id: "mcp_security_contract".to_string(),
                version: "phase1".to_string(),
                description: "MCP exposes deploy and safe read-only tools only".to_string(),
                config: engine_config,
            },
            WorldRuleMod {
                id: "base_world".to_string(),
                version: "phase1".to_string(),
                description: "Core room, command, and sandbox limits".to_string(),
                config: base_config,
            },
        ],
    }
}

pub fn is_visible_to(world: &mut World, player_id: PlayerId, position: Position) -> bool {
    is_position_visible_to(world, player_id, position)
}

pub fn visible_entities_for_player(world: &mut World, player_id: PlayerId) -> Vec<VisibleEntity> {
    let visible_positions = visible_positions(world, player_id);
    let visible_ids = visible_entity_ids(world, player_id, 0);
    let mut entities = visible_entities(world, &visible_positions, &visible_ids);
    entities.sort_by_key(entity_sort_key);
    entities
}

fn visible_entities(
    world: &mut World,
    visible_positions: &BTreeSet<(RoomId, i32, i32)>,
    visible_ids: &BTreeSet<Entity>,
) -> Vec<VisibleEntity> {
    let mut entities = Vec::new();

    for (entity, position, drone) in world.query::<(Entity, &Position, &Drone)>().iter(world) {
        if visible_ids.contains(&entity)
            || visible_positions.contains(&(position.room, position.x, position.y))
        {
            entities.push(VisibleEntity::Drone(VisibleDrone {
                id: object_id(entity),
                owner: drone.owner,
                position: visible_position(*position),
                body: drone.body.clone(),
                carry: drone.carry.iter().map(|(k, v)| (k.clone(), *v)).collect(),
                carry_capacity: drone.carry_capacity,
                fatigue: drone.fatigue,
                hits: drone.hits,
                hits_max: drone.hits_max,
                spawning: drone.spawning,
            }));
        }
    }

    for (entity, position, structure) in
        world.query::<(Entity, &Position, &Structure)>().iter(world)
    {
        if visible_ids.contains(&entity)
            || visible_positions.contains(&(position.room, position.x, position.y))
        {
            entities.push(VisibleEntity::Structure(VisibleStructure {
                id: object_id(entity),
                structure_type: structure.structure_type,
                owner: structure.owner,
                position: visible_position(*position),
                hits: structure.hits,
                hits_max: structure.hits_max,
                energy: structure.energy,
                energy_capacity: structure.energy_capacity,
                cooldown: structure.cooldown,
            }));
        }
    }

    for (entity, position, source) in world.query::<(Entity, &Position, &Source)>().iter(world) {
        if visible_positions.contains(&(position.room, position.x, position.y)) {
            entities.push(VisibleEntity::Source(VisibleSource {
                id: object_id(entity),
                position: visible_position(*position),
                produces: source
                    .produces
                    .iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .collect(),
                capacity: source.capacity,
                ticks_to_regeneration: source.ticks_to_regeneration,
            }));
        }
    }

    for (entity, position, resource) in world
        .query::<(Entity, &Position, &crate::components::Resource)>()
        .iter(world)
    {
        if visible_positions.contains(&(position.room, position.x, position.y)) {
            entities.push(VisibleEntity::Resource(VisibleResource {
                id: object_id(entity),
                position: visible_position(*position),
                amounts: resource
                    .amounts
                    .iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .collect(),
            }));
        }
    }

    for (entity, position, controller) in world
        .query::<(Entity, &Position, &Controller)>()
        .iter(world)
    {
        if visible_ids.contains(&entity)
            || visible_positions.contains(&(position.room, position.x, position.y))
        {
            entities.push(VisibleEntity::Controller(VisibleController {
                id: object_id(entity),
                owner: controller.owner,
                position: visible_position(*position),
                level: controller.level,
                progress: controller.progress,
                progress_total: controller.progress_total,
                safe_mode: controller.safe_mode,
            }));
        }
    }

    entities
}

fn visible_position(position: Position) -> VisiblePosition {
    VisiblePosition {
        x: position.x,
        y: position.y,
        room_id: position.room.0,
    }
}

fn visible_entity_position(entity: &VisibleEntity) -> Position {
    let position = match entity {
        VisibleEntity::Drone(entity) => &entity.position,
        VisibleEntity::Structure(entity) => &entity.position,
        VisibleEntity::Source(entity) => &entity.position,
        VisibleEntity::Resource(entity) => &entity.position,
        VisibleEntity::Controller(entity) => &entity.position,
    };
    Position {
        x: position.x,
        y: position.y,
        room: RoomId(position.room_id),
    }
}

fn mcp_hex_distance(from: Position, to: Position) -> u32 {
    if from.room != to.room {
        return u32::MAX;
    }
    let dx = from.x - to.x;
    let dy = from.y - to.y;
    let dz = -dx - dy;
    dx.unsigned_abs()
        .max(dy.unsigned_abs())
        .max(dz.unsigned_abs())
}

fn entity_sort_key(entity: &VisibleEntity) -> (u8, ObjectId) {
    match entity {
        VisibleEntity::Drone(entity) => (0, entity.id),
        VisibleEntity::Structure(entity) => (1, entity.id),
        VisibleEntity::Source(entity) => (2, entity.id),
        VisibleEntity::Resource(entity) => (3, entity.id),
        VisibleEntity::Controller(entity) => (4, entity.id),
    }
}

fn error_response(id: Value, error: McpError) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: None,
        error: Some(error),
    }
}

fn unix_timestamp_string() -> String {
    unix_timestamp_seconds().to_string()
}

fn unix_timestamp_seconds() -> u64 {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    seconds
}

fn opaque_web_token(kind: &str, provider: &str, subject: &str, seed: &str, now: u64) -> String {
    let material = format!("{kind}:{provider}:{subject}:{seed}:{now}");
    format!(
        "swarm_{kind}_{}",
        blake3::hash(material.as_bytes()).to_hex()
    )
}

fn oauth_player_id(provider: &str, subject: &str) -> PlayerId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(provider.as_bytes());
    hasher.update(b"\0");
    hasher.update(subject.as_bytes());
    let bytes = hasher.finalize();
    let mut id_bytes = [0_u8; 4];
    id_bytes.copy_from_slice(&bytes.as_bytes()[0..4]);
    u32::from_le_bytes(id_bytes)
}

fn verify_wasm_signature(
    certificate: &PlayerCertificate,
    wasm_hash: &[u8],
    wasm_signature: &str,
) -> Result<(), McpError> {
    let verifying_key = decode_ed25519_public_key(
        &certificate.payload.client_public_key,
        "certificate client_public_key",
    )?;
    let signature = decode_ed25519_signature(wasm_signature, "wasm_signature")?;
    verifying_key
        .verify(wasm_hash, &signature)
        .map_err(|_| McpError::invalid_params("wasm_signature is invalid"))
}

fn decode_ed25519_public_key(input: &str, field: &str) -> Result<VerifyingKey, McpError> {
    let bytes = decode_base64_with_message(input, field)?;
    let key_bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| McpError::invalid_params(format!("{field} must be 32 bytes")))?;
    VerifyingKey::from_bytes(&key_bytes)
        .map_err(|_| McpError::invalid_params(format!("{field} is invalid")))
}

pub(crate) fn decode_ed25519_signature(input: &str, field: &str) -> Result<Signature, McpError> {
    let bytes = decode_base64_with_message(input, field)?;
    let signature_bytes: [u8; 64] = bytes
        .try_into()
        .map_err(|_| McpError::invalid_params(format!("{field} must be 64 bytes")))?;
    Ok(Signature::from_bytes(&signature_bytes))
}

fn decode_base64_with_message(input: &str, field: &str) -> Result<Vec<u8>, McpError> {
    decode_base64(input)
        .map_err(|_| McpError::invalid_params(format!("{field} is not valid base64")))
}

pub(crate) fn encode_base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);

        output.push(ALPHABET[(a >> 2) as usize] as char);
        output.push(ALPHABET[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(ALPHABET[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(ALPHABET[(c & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

fn decode_base64(input: &str) -> Result<Vec<u8>, McpError> {
    let bytes = input.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err(McpError::invalid_params("wasm_bytes is not valid base64"));
    }

    let mut output = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut chunks = bytes.chunks_exact(4).peekable();
    while let Some(chunk) = chunks.next() {
        let last = chunks.peek().is_none();
        let a = base64_value(chunk[0])?;
        let b = base64_value(chunk[1])?;
        let c = if chunk[2] == b'=' {
            64
        } else {
            base64_value(chunk[2])?
        };
        let d = if chunk[3] == b'=' {
            64
        } else {
            base64_value(chunk[3])?
        };
        if (chunk[2] == b'=' && chunk[3] != b'=')
            || (!last && (chunk[2] == b'=' || chunk[3] == b'='))
        {
            return Err(McpError::invalid_params("wasm_bytes is not valid base64"));
        }

        output.push((a << 2) | (b >> 4));
        if c != 64 {
            output.push(((b & 0x0f) << 4) | (c >> 2));
        }
        if d != 64 {
            output.push(((c & 0x03) << 6) | d);
        }
    }

    Ok(output)
}

fn base64_value(byte: u8) -> Result<u8, McpError> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(McpError::invalid_params("wasm_bytes is not valid base64")),
    }
}

// ── G12: swarm_get_room ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectRoomParams {
    pub room_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomInspectResult {
    pub room_id: u32,
    pub drone_count: u32,
    pub structure_count: u32,
    pub controller_owner: Option<PlayerId>,
}

pub fn swarm_get_room(
    world: &mut SwarmWorld,
    context: McpContext,
    params: InspectRoomParams,
) -> Result<RoomInspectResult, McpError> {
    let visible = crate::visibility::visible_positions(world.app.world_mut(), context.player_id);
    let world_inner = world.app.world_mut();
    let mut drone_count = 0u32;
    let mut structure_count = 0u32;
    let mut controller_owner = None;

    let room = crate::components::RoomId(params.room_id);

    // Count drones in room that are visible
    let mut drones = world_inner.query::<(
        Entity,
        &crate::components::Drone,
        &crate::components::Position,
    )>();
    for (_e, _d, pos) in drones.iter(world_inner) {
        if pos.room == room && visible.contains(&(pos.room, pos.x, pos.y)) {
            drone_count += 1;
        }
    }

    // Count structures in room
    let mut structures = world_inner.query::<(
        Entity,
        &crate::components::Structure,
        &crate::components::Position,
    )>();
    for (_e, _s, pos) in structures.iter(world_inner) {
        if pos.room == room && visible.contains(&(pos.room, pos.x, pos.y)) {
            structure_count += 1;
        }
    }

    // Find controller
    let mut controllers = world_inner.query::<(
        Entity,
        &crate::components::Controller,
        &crate::components::Position,
    )>();
    for (_e, c, pos) in controllers.iter(world_inner) {
        if pos.room == room && visible.contains(&(pos.room, pos.x, pos.y)) {
            controller_owner = c.owner;
            break;
        }
    }

    Ok(RoomInspectResult {
        room_id: params.room_id,
        drone_count,
        structure_count,
        controller_owner,
    })
}

// ── G13: swarm_list_modules types ────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListModulesParams {
    pub player_id: Option<PlayerId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleInfo {
    pub player_id: PlayerId,
    pub module_hash: String,
    pub wasm_size: usize,
    pub compiled_at: Option<u64>,
}

// ── G14: swarm_get_replay ────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetReplayParams {
    pub from_tick: Tick,
    pub to_tick: Tick,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayResult {
    pub from_tick: Tick,
    pub to_tick: Tick,
    pub tick_count: u32,
    pub commands: Vec<crate::command::RawCommand>,
    pub entity_changes: Vec<crate::replay_storage::EntityChange>,
    pub message: String,
}

pub fn swarm_get_replay(
    world: &SwarmWorld,
    _context: McpContext,
    params: GetReplayParams,
) -> Result<ReplayResult, McpError> {
    use crate::replay_storage::ReplayStore;

    let store = world
        .app
        .world()
        .get_resource::<ReplayStore>()
        .ok_or_else(|| McpError::invalid_params("replay store not initialized"))?;

    if params.from_tick > params.to_tick {
        return Err(McpError::invalid_params("from_tick must be <= to_tick"));
    }

    // Find nearest keyframe at or before from_tick
    let (keyframe_tick, _keyframe) = store.nearest_keyframe(params.from_tick).ok_or_else(|| {
        McpError::invalid_params(format!(
            "no keyframe found at or before tick {}",
            params.from_tick
        ))
    })?;

    // Collect deltas from keyframe+1 to to_tick
    let deltas = store.deltas_in_range(keyframe_tick, params.to_tick);

    let mut entity_changes: Vec<crate::replay_storage::EntityChange> = Vec::new();
    let commands: Vec<crate::command::RawCommand> = Vec::new();

    for delta in &deltas {
        entity_changes.extend(delta.entity_changes.clone());
        // Commands from deltas — RawCommand is serialized in the delta
    }

    let has_data = !deltas.is_empty();
    let msg = if has_data {
        format!(
            "replay: {} ticks from keyframe@{} ({} deltas, {} entity changes)",
            params.to_tick - keyframe_tick + 1,
            keyframe_tick,
            deltas.len(),
            entity_changes.len(),
        )
    } else {
        format!(
            "no replay data for ticks {}-{} (keyframe@{} has no deltas yet)",
            params.from_tick, params.to_tick, keyframe_tick,
        )
    };

    Ok(ReplayResult {
        from_tick: params.from_tick,
        to_tick: params.to_tick,
        tick_count: (params.to_tick.saturating_sub(params.from_tick)) as u32,
        commands,
        entity_changes,
        message: msg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Structure, StructureType, create_world};
    use ed25519_dalek::Signer;

    fn spawn_structure(world: &mut SwarmWorld, owner: Option<PlayerId>, x: i32, y: i32) {
        world.app.world_mut().spawn((
            Position {
                x,
                y,
                room: RoomId(0),
            },
            Structure {
                structure_type: StructureType::Spawn,
                owner,
                hits: 5_000,
                hits_max: 5_000,
                energy: Some(300),
                energy_capacity: Some(300),
                cooldown: 0,
            },
        ));
    }

    fn test_signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn login_with_key(server: &mut McpServer, client_key: &SigningKey) -> OAuth2LoginResult {
        server
            .swarm_oauth2_login(OAuth2LoginParams {
                provider: "test-oauth".to_string(),
                subject: "player@example.com".to_string(),
                access_token: "opaque-test-token".to_string(),
                client_public_key: encode_base64(client_key.verifying_key().as_bytes()),
            })
            .expect("login should issue certificate")
    }

    fn valid_deploy_wasm() -> Vec<u8> {
        wat::parse_str(
            r#"
            (module
                (memory (export "memory") 1)
                (func (export "alloc") (param i32) (result i32) i32.const 0)
                (func (export "free") (param i32 i32))
                (func (export "tick") (param i32 i32 i32) (result i32) i32.const 0)
            )
            "#,
        )
        .expect("valid test wasm")
    }

    fn signed_deploy_params(
        certificate: PlayerCertificate,
        client_key: &SigningKey,
    ) -> DeployParams {
        let wasm_bytes = valid_deploy_wasm();
        let wasm_hash = blake3::hash(&wasm_bytes);
        let wasm_signature = client_key.sign(wasm_hash.as_bytes());
        DeployParams {
            wasm_bytes: encode_base64(&wasm_bytes),
            certificate,
            wasm_signature: encode_base64(&wasm_signature.to_bytes()),
            language: "rust".to_string(),
            version_tag: "v1".to_string(),
            room_id: 0,
        }
    }

    fn sample_simulation_snapshot() -> VisibleWorldSnapshot {
        VisibleWorldSnapshot {
            tick: 5,
            player_id: 1,
            room_id: 0,
            visibility_radius: VISIBILITY_RADIUS,
            visible_tiles: Vec::new(),
            entities: vec![
                VisibleEntity::Drone(VisibleDrone {
                    id: 42,
                    owner: 1,
                    position: VisiblePosition {
                        x: 10,
                        y: 10,
                        room_id: 0,
                    },
                    body: vec![BodyPart::Move],
                    carry: BTreeMap::new(),
                    carry_capacity: 50,
                    fatigue: 3,
                    hits: 100,
                    hits_max: 100,
                    spawning: false,
                }),
                VisibleEntity::Structure(VisibleStructure {
                    id: 43,
                    structure_type: StructureType::Spawn,
                    owner: Some(1),
                    position: VisiblePosition {
                        x: 11,
                        y: 10,
                        room_id: 0,
                    },
                    hits: 5_000,
                    hits_max: 5_000,
                    energy: Some(100),
                    energy_capacity: Some(300),
                    cooldown: 4,
                }),
                VisibleEntity::Source(VisibleSource {
                    id: 44,
                    position: VisiblePosition {
                        x: 12,
                        y: 10,
                        room_id: 0,
                    },
                    produces: BTreeMap::from([("Energy".to_string(), 100)]),
                    capacity: 500,
                    ticks_to_regeneration: 5,
                }),
            ],
            local_storage: BTreeMap::from([("Energy".to_string(), 100)]),
            global_storage: BTreeMap::new(),
            pending_global_transfers: vec![VisiblePendingGlobalTransfer {
                player_id: 1,
                direction: "ToGlobal".to_string(),
                resource: "Energy".to_string(),
                amount: 50,
                deliver_amount: 49,
                remaining_ticks: 3,
            }],
        }
    }

    #[test]
    fn simulate_is_registered_as_simulate_source_tool() {
        assert_eq!(
            mcp_tool_source("swarm_simulate"),
            Some(CommandSource::Simulate)
        );
        assert!(
            mcp_tool_infos()
                .iter()
                .any(|tool| tool.name == "swarm_simulate")
        );
    }

    #[test]
    fn simulate_advances_snapshot_tick_and_returns_diff() {
        let result = swarm_simulate(SimulateParams {
            snapshot: sample_simulation_snapshot(),
            ticks: 2,
        })
        .unwrap();

        assert_eq!(result.from_tick, 5);
        assert_eq!(result.to_tick, 7);
        assert_eq!(result.predicted_snapshot.tick, 7);
        assert_eq!(
            result.predicted_snapshot.pending_global_transfers[0].remaining_ticks,
            1
        );
        assert_eq!(result.diff.tick_before, 5);
        assert_eq!(result.diff.tick_after, 7);
        assert!(result.diff.state_changed);
        assert_eq!(result.diff.entity_changes.len(), 3);
        assert!(matches!(
            &result.predicted_snapshot.entities[0],
            VisibleEntity::Drone(VisibleDrone { fatigue: 1, .. })
        ));
        assert!(matches!(
            &result.predicted_snapshot.entities[1],
            VisibleEntity::Structure(VisibleStructure { cooldown: 2, .. })
        ));
        assert!(matches!(
            &result.predicted_snapshot.entities[2],
            VisibleEntity::Source(VisibleSource {
                ticks_to_regeneration: 3,
                ..
            })
        ));
    }

    #[test]
    fn simulate_delivers_completed_transfer_into_predicted_storage_diff() {
        let result = swarm_simulate(SimulateParams {
            snapshot: sample_simulation_snapshot(),
            ticks: 3,
        })
        .unwrap();

        assert!(
            result
                .predicted_snapshot
                .pending_global_transfers
                .is_empty()
        );
        assert_eq!(
            result.predicted_snapshot.global_storage.get("Energy"),
            Some(&49)
        );
        assert_eq!(result.diff.pending_global_transfers_after, Vec::new());
        assert_ne!(
            result.diff.global_storage_before,
            result.diff.global_storage_after
        );
    }

    #[test]
    fn simulate_call_tool_does_not_mutate_live_world() {
        let mut world = create_world();
        world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let before = world.state_checksum();
        let mut server = McpServer::new();
        let value = server
            .call_tool(
                &mut world,
                McpContext {
                    player_id: 1,
                    tick: 5,
                },
                "swarm_simulate",
                serde_json::to_value(SimulateParams {
                    snapshot: sample_simulation_snapshot(),
                    ticks: 1,
                })
                .unwrap(),
            )
            .unwrap();
        let result: SimulateResult = serde_json::from_value(value).unwrap();

        assert_eq!(result.to_tick, 6);
        assert_eq!(world.state_checksum(), before);
    }

    #[test]
    fn snapshot_filters_entities_and_terrain_by_player_visibility() {
        let mut world = create_world();
        world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        world.spawn_drone(2, 12, 10, vec![BodyPart::Move]);
        world.spawn_drone(2, 40, 40, vec![BodyPart::Move]);

        let snapshot = swarm_get_snapshot(
            &mut world,
            McpContext {
                player_id: 1,
                tick: 7,
            },
        );

        assert_eq!(snapshot.tick, 7);
        assert!(
            snapshot
                .visible_tiles
                .iter()
                .any(|tile| tile.x == 10 && tile.y == 10)
        );
        assert!(
            !snapshot
                .visible_tiles
                .iter()
                .any(|tile| tile.x == 40 && tile.y == 40)
        );
        assert!(snapshot.visible_tiles.len() < (DEFAULT_ROOM_SIZE * DEFAULT_ROOM_SIZE) as usize);

        let drone_positions = snapshot
            .entities
            .iter()
            .filter_map(|entity| match entity {
                VisibleEntity::Drone(drone) => {
                    Some((drone.owner, drone.position.x, drone.position.y))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(drone_positions.contains(&(1, 10, 10)));
        assert!(drone_positions.contains(&(2, 12, 10)));
        assert!(!drone_positions.contains(&(2, 40, 40)));
    }

    #[test]
    fn swarm_get_snapshot_uses_dragonfly_cache_after_fdb_backfill() {
        let mut world = create_world();
        world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let context = McpContext {
            player_id: 1,
            tick: 11,
        };

        let first = swarm_get_snapshot(&mut world, context.clone());
        let stats = world
            .app
            .world()
            .resource::<crate::dragonfly::DragonflyCache>()
            .stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.refreshes, 1);

        let second = swarm_get_snapshot(&mut world, context);
        let stats = world
            .app
            .world()
            .resource::<crate::dragonfly::DragonflyCache>()
            .stats();
        assert_eq!(second, first);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.refreshes, 1);
    }

    #[test]
    fn owned_structure_extends_visibility_without_leaking_full_map() {
        let mut world = create_world();
        spawn_structure(&mut world, Some(1), 35, 35);

        let snapshot = swarm_get_snapshot(
            &mut world,
            McpContext {
                player_id: 1,
                tick: 1,
            },
        );

        assert!(is_visible_to(
            world.app.world_mut(),
            1,
            Position {
                x: 35,
                y: 35,
                room: RoomId(0)
            }
        ));
        assert!(
            snapshot
                .visible_tiles
                .iter()
                .any(|tile| tile.x == 35 && tile.y == 35)
        );
        assert!(
            !snapshot
                .visible_tiles
                .iter()
                .any(|tile| tile.x == 0 && tile.y == 0)
        );
    }

    #[test]
    fn deploy_validates_and_stores_wasm_for_next_tick_loading() {
        let world = create_world();
        let issuer = test_signing_key(1);
        let client_key = test_signing_key(2);
        let mut server = McpServer::with_issuer_for_tests(issuer, 1_000);
        let login = login_with_key(&mut server, &client_key);
        let result = server
            .swarm_deploy(
                &world,
                McpContext {
                    player_id: login.player_id,
                    tick: 11,
                },
                signed_deploy_params(login.certificate.clone(), &client_key),
            )
            .expect("deploy should succeed");

        assert_eq!(result.status, "pending_next_tick");
        assert_eq!(result.cache_status, "precompiled");
        assert_eq!(
            result.module_hash,
            blake3::hash(&valid_deploy_wasm()).to_hex().to_string()
        );
        assert_eq!(
            result.wasmtime_version,
            swarm_wasm_sandbox::wasmtime_version()
        );
        assert_eq!(server.module_cache_stats().entries, 1);
        assert_eq!(server.modules().len(), 1);
        assert_eq!(server.modules()[0].module_id, result.module_id);
        assert_eq!(server.modules()[0].load_after_tick, 12);
        assert_eq!(server.modules()[0].wasm_bytes, valid_deploy_wasm());
        assert_eq!(server.modules()[0].certificate, login.certificate);
        assert!(!server.modules()[0].wasm_signature.is_empty());
        assert_eq!(
            server.modules()[0].wasm_hash,
            blake3::hash(&valid_deploy_wasm()).to_hex().to_string()
        );
        assert_eq!(
            server.modules()[0].wasmtime_version,
            swarm_wasm_sandbox::wasmtime_version()
        );

        let compiled = server
            .compile_module_for_tick(&result.module_id)
            .expect("cached native module should instantiate at tick time");
        assert_eq!(compiled.module_hash(), result.module_hash);
        assert_eq!(compiled.wasmtime_version(), result.wasmtime_version);
    }

    #[test]
    fn deploy_rejects_invalid_base64_and_non_wasm() {
        let world = create_world();
        let issuer = test_signing_key(3);
        let client_key = test_signing_key(4);
        let mut server = McpServer::with_issuer_for_tests(issuer, 1_000);
        let login = login_with_key(&mut server, &client_key);
        let context = McpContext {
            player_id: login.player_id,
            tick: 0,
        };
        let valid_params = signed_deploy_params(login.certificate, &client_key);

        assert!(
            server
                .swarm_deploy(
                    &world,
                    context.clone(),
                    DeployParams {
                        wasm_bytes: "not base64".to_string(),
                        ..valid_params.clone()
                    },
                )
                .is_err()
        );
        assert!(
            server
                .swarm_deploy(
                    &world,
                    context,
                    DeployParams {
                        wasm_bytes: "YWJj".to_string(),
                        ..valid_params
                    },
                )
                .is_err()
        );
    }

    #[test]
    fn oauth2_login_issues_ed25519_certificate_with_24h_expiry() {
        let issuer = test_signing_key(5);
        let client_key = test_signing_key(6);
        let mut server = McpServer::with_issuer_for_tests(issuer, 10_000);

        let login = login_with_key(&mut server, &client_key);

        assert_eq!(login.certificate.payload.issued_at, 10_000);
        assert_eq!(
            login.certificate.payload.expires_at,
            10_000 + CERTIFICATE_TTL_SECONDS
        );
        assert_eq!(login.certificate.payload.player_id, login.player_id);
        assert_eq!(login.certificate.payload.audience, CERTIFICATE_AUDIENCE);
        server
            .verify_certificate_for_player(&login.certificate, login.player_id)
            .expect("issued certificate should verify");
    }

    #[test]
    fn deploy_rejects_invalid_wasm_signature() {
        let world = create_world();
        let issuer = test_signing_key(7);
        let client_key = test_signing_key(8);
        let wrong_key = test_signing_key(9);
        let mut server = McpServer::with_issuer_for_tests(issuer, 20_000);
        let login = login_with_key(&mut server, &client_key);
        let mut params = signed_deploy_params(login.certificate, &wrong_key);

        let error = server
            .swarm_deploy(
                &world,
                McpContext {
                    player_id: login.player_id,
                    tick: 1,
                },
                params.clone(),
            )
            .expect_err("wrong key must not verify against certificate public key");
        assert_eq!(error.message, "wasm_signature is invalid");

        params.wasm_signature = "not base64".to_string();
        assert!(
            server
                .swarm_deploy(
                    &world,
                    McpContext {
                        player_id: login.player_id,
                        tick: 1,
                    },
                    params,
                )
                .is_err()
        );
    }

    #[test]
    fn deploy_rejects_expired_certificate() {
        let world = create_world();
        let issuer = test_signing_key(10);
        let client_key = test_signing_key(11);
        let mut issuing_server = McpServer::with_issuer_for_tests(issuer.clone(), 30_000);
        let login = login_with_key(&mut issuing_server, &client_key);
        let mut verifying_server =
            McpServer::with_issuer_for_tests(issuer, 30_000 + CERTIFICATE_TTL_SECONDS);

        let error = verifying_server
            .swarm_deploy(
                &world,
                McpContext {
                    player_id: login.player_id,
                    tick: 1,
                },
                signed_deploy_params(login.certificate, &client_key),
            )
            .expect_err("expired certificate should be rejected");

        assert_eq!(error.message, "certificate is expired");
    }

    #[test]
    fn world_rules_expose_safe_readable_configuration() {
        let rules = swarm_get_world_rules();

        assert_eq!(rules.ruleset, "phase1");
        assert_eq!(rules.room_size, DEFAULT_ROOM_SIZE);
        assert!(rules.active_mods.iter().any(|module| {
            module.id == "mcp_security_contract"
                && module.config.get("mcp_direct_gameplay_actions") == Some(&json!(false))
        }));
    }

    #[test]
    fn docs_expose_complete_basic_agent_tutorial_resource() {
        let docs = swarm_get_docs(DocsParams {
            topic: "swarm://docs/tutorials/basic-agent".to_string(),
        });
        let text = docs_sections_markdown(&docs);
        assert_eq!(docs.topic, "basic-agent");
        assert!(docs.sections.len() >= 10);
        assert!(text.contains("LEARN -> DECIDE -> ACT -> UNDERSTAND"));
        assert!(text.contains("swarm_get_snapshot"));
        assert!(text.contains("swarm_get_available_actions"));
        assert!(text.contains("swarm_dry_run"));
        assert!(text.contains("certificate issuance flow"));
        assert!(text.contains("swarm_deploy"));
        assert!(text.contains("pending_next_tick"));
        assert!(text.contains("BLAKE3"));
        assert!(text.contains("Ed25519"));
        assert!(text.contains("P0-6"));
        assert!(text.contains("P0-8"));
    }

    #[test]
    fn mcp_lists_and_reads_basic_agent_docs_resource() {
        let mut world = create_world();
        let mut server = McpServer::new();
        let context = McpContext {
            player_id: 1,
            tick: 0,
        };
        let list = server.handle_json_rpc(
            &mut world,
            context.clone(),
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: json!("docs-list"),
                method: "resources/list".to_string(),
                params: Value::Null,
            },
        );
        assert!(list.error.is_none(), "{:?}", list.error);
        let list_result = list.result.expect("resources/list result");
        let resources = list_result["resources"]
            .as_array()
            .expect("resources array");
        assert!(
            resources
                .iter()
                .any(|resource| resource["uri"].as_str()
                    == Some("swarm://docs/tutorials/basic-agent"))
        );
        let read = server.handle_json_rpc(
            &mut world,
            context,
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: json!("basic-agent-doc"),
                method: "resources/read".to_string(),
                params: json!({ "uri": "swarm://docs/tutorials/basic-agent" }),
            },
        );
        assert!(read.error.is_none(), "{:?}", read.error);
        let read_result = read.result.expect("resources/read result");
        let contents = read_result["contents"].as_array().expect("contents array");
        let text = contents[0]["text"].as_str().expect("markdown text");
        assert!(text.contains("# Goal"));
        assert!(text.contains("30 minute checklist"));
        assert!(text.contains("WASM sandbox execution"));
    }

    #[test]
    fn basic_agent_tutorial_preserves_mcp_security_and_idl_contracts() {
        let docs = swarm_get_docs(DocsParams {
            topic: "basic-agent".to_string(),
        });
        let text = docs_sections_markdown(&docs);
        assert!(text.contains("MCP is not a game controller"));
        assert!(text.contains("Direct gameplay MCP tools remain disabled"));
        assert!(
            text.contains("WASM tick() returns CommandIntent objects, not RawCommand envelopes")
        );
        assert!(text.contains("Each object has only sequence and action"));
        assert!(text.contains("The server injects player_id, source, and tick"));
        assert!(text.contains(r#""action":{"type":"Spawn""#));
        assert!(text.contains(r#""action":{"type":"Harvest""#));
        assert!(text.contains(r#""action":{"type":"Transfer""#));
    }

    #[test]
    fn docs_api_reference_and_command_resource_cover_p0_8() {
        let docs = swarm_get_docs(DocsParams {
            topic: "api".to_string(),
        });
        let text = docs_sections_markdown(&docs);
        assert!(text.contains("Game API IDL v1.0.0"));
        assert!(text.contains("CommandIntent"));
        assert!(text.contains("object_id: ObjectId"));
        assert!(text.contains("validator: exists, owner, drone"));
        let command_text =
            docs_markdown_for_uri("swarm://docs/api/commands/Move.md").expect("Move command docs");
        assert!(command_text.contains("# Move"));
        assert!(command_text.contains("direction: Direction"));
        assert!(command_text.contains("MCP cannot call Move directly"));
    }

    #[test]
    fn json_rpc_dispatches_only_phase1_mcp_tools() {
        let mut world = create_world();
        let mut server = McpServer::new();
        let context = McpContext {
            player_id: 1,
            tick: 0,
        };

        let ok = server.handle_json_rpc(
            &mut world,
            context.clone(),
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: json!(1),
                method: "swarm_get_world_rules".to_string(),
                params: Value::Null,
            },
        );
        assert!(ok.result.is_some());
        assert!(ok.error.is_none());

        let denied = server.handle_json_rpc(
            &mut world,
            context,
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: json!(2),
                method: "swarm_move".to_string(),
                params: Value::Null,
            },
        );
        assert!(denied.result.is_none());
        assert_eq!(denied.error.unwrap().code, -32601);
    }

    #[test]
    fn json_rpc_rejects_excess_query_calls_per_tick() {
        let mut world = create_world();
        let mut server = McpServer::new();
        let context = McpContext {
            player_id: 1,
            tick: 12,
        };

        for id in 0..50 {
            let response = server.handle_json_rpc(
                &mut world,
                context.clone(),
                JsonRpcRequest {
                    jsonrpc: "2.0".to_string(),
                    id: json!(id),
                    method: "swarm_get_world_rules".to_string(),
                    params: Value::Null,
                },
            );
            assert!(response.error.is_none(), "{response:?}");
        }

        let limited = server.handle_json_rpc(
            &mut world,
            context,
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: json!(51),
                method: "swarm_get_world_rules".to_string(),
                params: Value::Null,
            },
        );

        let message = limited.error.expect("rate limit error").message;
        assert!(message.contains("rate limited, retry after 1 seconds"));
    }

    #[test]
    fn rate_limiter_isolates_mcp_players_and_sources() {
        let mut world = create_world();
        let mut server = McpServer::new();
        let player_one = McpContext {
            player_id: 1,
            tick: 20,
        };

        for id in 0..50 {
            assert!(
                server
                    .call_tool(
                        &mut world,
                        player_one.clone(),
                        "swarm_get_world_rules",
                        json!({ "request_id": id })
                    )
                    .is_ok()
            );
        }
        let limited = server
            .call_tool(
                &mut world,
                player_one.clone(),
                "swarm_get_world_rules",
                Value::Null,
            )
            .expect_err("same player and source should be limited");
        assert!(
            limited
                .message
                .contains("rate limited, retry after 1 seconds")
        );

        assert!(
            server
                .call_tool(
                    &mut world,
                    McpContext {
                        player_id: 2,
                        tick: 20,
                    },
                    "swarm_get_world_rules",
                    Value::Null,
                )
                .is_ok()
        );

        let deploy_source_error = server
            .call_tool(
                &mut world,
                player_one,
                "swarm_tournament_precommit",
                json!({ "module_id": "missing" }),
            )
            .expect_err("deploy-source call should execute and fail validation");
        assert!(!deploy_source_error.message.contains("rate limited"));
    }

    #[test]
    fn rate_limiter_enforces_source_rates_and_refills_next_tick() {
        let mut limiter = RateLimiter::new();

        for _ in 0..100 {
            limiter.check(1, CommandSource::Wasm, 7).unwrap();
        }
        let wasm_limited = limiter
            .check(1, CommandSource::Wasm, 7)
            .expect_err("WASM limit is 100 per tick");
        assert!(
            wasm_limited
                .message
                .contains("rate limited, retry after 1 seconds")
        );
        limiter.check(1, CommandSource::Wasm, 8).unwrap();

        for _ in 0..5 {
            limiter.check(1, CommandSource::McpDeploy, 7).unwrap();
        }
        let deploy_limited = limiter
            .check(1, CommandSource::McpDeploy, 7)
            .expect_err("MCP deploy limit is 5 per tick");
        assert!(
            deploy_limited
                .message
                .contains("rate limited, retry after 1 seconds")
        );
    }

    #[test]
    fn oauth2_callback_issues_web_session_and_certificate() {
        let issuer = test_signing_key(13);
        let client_key = test_signing_key(14);
        let mut server = McpServer::with_issuer_for_tests(issuer, 30_000);
        let login = server
            .swarm_oauth2_callback(OAuth2CallbackParams {
                provider: "github".to_string(),
                code: "github-code".to_string(),
                state: "csrf".to_string(),
                redirect_uri: "https://swarm.example/auth/callback".to_string(),
                client_public_key: encode_base64(client_key.verifying_key().as_bytes()),
            })
            .expect("callback issues tokens and cert");
        assert_eq!(login.session.audience, WEB_TOKEN_AUDIENCE);
        assert_eq!(login.session.provider, "github");
        assert_eq!(
            login.session.access_token_expires_at,
            30_000 + WEB_ACCESS_TOKEN_TTL_SECONDS
        );
        assert_eq!(
            login.session.refresh_token_expires_at,
            30_000 + WEB_REFRESH_TOKEN_TTL_SECONDS
        );
        assert_eq!(
            login.certificate.payload.expires_at,
            30_000 + CERTIFICATE_TTL_SECONDS
        );
        server
            .verify_certificate_for_player(&login.certificate, login.player_id)
            .unwrap();
    }

    #[test]
    fn token_refresh_renews_access_token_and_24h_certificate() {
        let issuer = test_signing_key(15);
        let client_key = test_signing_key(16);
        let mut server = McpServer::with_issuer_for_tests(issuer, 40_000);
        let login = login_with_key(&mut server, &client_key);
        server.now_seconds = Some(41_000);
        let refreshed = server
            .swarm_token_refresh(TokenRefreshParams {
                refresh_token: login.session.refresh_token.clone(),
                client_public_key: encode_base64(client_key.verifying_key().as_bytes()),
            })
            .expect("refresh renews cert");
        assert_ne!(refreshed.session.access_token, login.session.access_token);
        assert_eq!(refreshed.certificate.payload.issued_at, 41_000);
        assert_eq!(
            refreshed.certificate.payload.expires_at,
            41_000 + CERTIFICATE_TTL_SECONDS
        );
        assert_eq!(refreshed.renew_after_seconds, CERTIFICATE_TTL_SECONDS);
    }

    #[test]
    fn revoke_blocks_refresh_and_certificate_deploy() {
        let world = create_world();
        let issuer = test_signing_key(17);
        let client_key = test_signing_key(18);
        let mut server = McpServer::with_issuer_for_tests(issuer, 50_000);
        let login = login_with_key(&mut server, &client_key);
        let revoked = server
            .swarm_auth_revoke(RevokeAuthParams {
                refresh_token: Some(login.session.refresh_token.clone()),
                certificate: Some(login.certificate.clone()),
            })
            .unwrap();
        assert!(revoked.revoked_session && revoked.revoked_certificate);
        assert!(
            server
                .swarm_token_refresh(TokenRefreshParams {
                    refresh_token: login.session.refresh_token,
                    client_public_key: encode_base64(client_key.verifying_key().as_bytes())
                })
                .is_err()
        );
        let error = server
            .swarm_deploy(
                &world,
                McpContext {
                    player_id: login.player_id,
                    tick: 1,
                },
                signed_deploy_params(login.certificate, &client_key),
            )
            .expect_err("revoked cert rejected");
        assert_eq!(error.message, "certificate is revoked");
    }
    #[test]
    fn tournament_precommit_locks_deployed_module_and_blocks_redeploy() {
        let world = create_world();
        let issuer = test_signing_key(21);
        let client_key = test_signing_key(22);
        let mut server = McpServer::with_issuer_for_tests(issuer, 40_000);
        let login = login_with_key(&mut server, &client_key);
        let context = McpContext {
            player_id: login.player_id,
            tick: 7,
        };
        let deploy = server
            .swarm_deploy(
                &world,
                context.clone(),
                signed_deploy_params(login.certificate.clone(), &client_key),
            )
            .expect("initial deploy should succeed");
        let locked = server
            .swarm_tournament_precommit(
                context.clone(),
                TournamentPrecommitParams {
                    module_id: deploy.module_id.clone(),
                },
            )
            .expect("precommit should lock deployed module");
        assert_eq!(locked.status, "locked_for_tournament");
        assert_eq!(locked.locked_module.module_id, deploy.module_id);
        assert_eq!(locked.locked_module.locked_at_tick, 7);
        assert_eq!(
            locked.locked_module.wasm_hash,
            blake3::hash(&valid_deploy_wasm()).to_hex().to_string()
        );
        assert_eq!(server.tournament_locks().len(), 1);
        let error = server
            .swarm_deploy(
                &world,
                McpContext {
                    player_id: login.player_id,
                    tick: 8,
                },
                signed_deploy_params(login.certificate, &client_key),
            )
            .expect_err("precommitted tournament player cannot redeploy");
        assert!(error.message.contains("tournament precommit"));
    }

    #[test]
    fn tournament_mcp_status_and_docs_expose_ai_interface_without_gameplay_tools() {
        let mut world = create_world();
        let mut server = McpServer::new();
        let context = McpContext {
            player_id: 1,
            tick: 3,
        };
        let status = server.swarm_tournament_status(context.clone());
        assert_eq!(status.mode, "preparation");
        assert!(!status.deploy_locked);
        assert!(!status.direct_gameplay_tools_enabled);
        assert!(status.tournaments.is_empty());
        let tool_names = status
            .preparation_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert!(tool_names.contains(&"swarm_deploy"));
        assert!(tool_names.contains(&"swarm_tournament_precommit"));
        assert!(tool_names.contains(&"swarm_tournament_create"));
        assert!(tool_names.contains(&"swarm_tournament_status"));
        assert!(tool_names.contains(&"swarm_match_result"));
        assert!(!tool_names.iter().any(|name| matches!(
            *name,
            "swarm_move" | "swarm_harvest" | "swarm_build"
        )));
        let read = server.handle_json_rpc(
            &mut world,
            context,
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: json!("tournament-docs"),
                method: "resources/read".to_string(),
                params: json!({ "uri": "swarm://docs/tournament/ai.md" }),
            },
        );
        assert!(read.error.is_none(), "{:?}", read.error);
        let result = read.result.unwrap();
        let text = result["contents"][0]["text"].as_str().unwrap();
        assert!(text.contains("WASM pre-submission"));
        assert!(text.contains("swarm_tournament_precommit"));
        assert!(text.contains("swarm_tournament_create"));
        assert!(text.contains("swarm_match_result"));
        assert!(text.contains("No swarm_move"));
    }

    #[test]
    fn get_replay_finds_keyframe_and_returns_no_data_when_empty() {
        let world = create_world();
        // ReplayStore is initialized but empty (no keyframes/deltas recorded yet)
        let result = swarm_get_replay(
            &world,
            McpContext {
                player_id: 0,
                tick: 0,
            },
            GetReplayParams {
                from_tick: 0,
                to_tick: 10,
            },
        );
        // Should find no keyframe since store is empty → error
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("no keyframe"));
    }

    #[test]
    fn get_replay_rejects_invalid_tick_range() {
        let world = create_world();
        let result = swarm_get_replay(
            &world,
            McpContext {
                player_id: 0,
                tick: 0,
            },
            GetReplayParams {
                from_tick: 10,
                to_tick: 5,
            },
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .message
                .contains("from_tick must be <= to_tick")
        );
    }

    #[test]
    fn tournament_create_and_match_result_advance_bracket() {
        let mut server = McpServer::new();
        for player_id in 1..=4 {
            server.tournament_locks.insert(
                player_id,
                TournamentLockedModule {
                    player_id,
                    module_id: format!("module-{player_id}"),
                    wasm_hash: format!("hash-{player_id}"),
                    version_tag: "v1".to_string(),
                    locked_at_tick: 10,
                },
            );
        }

        let created = server
            .swarm_tournament_create(TournamentCreateParams {
                tournament_id: "cup".to_string(),
                elimination: TournamentElimination::Single,
                fixed_ticks: 3,
                players: vec![1, 2, 3, 4],
            })
            .expect("precommitted players create a tournament");
        assert_eq!(created.status, "scheduled");
        assert_eq!(created.scheduled.len(), 2);
        assert_eq!(created.scheduled[0].player_one, 1);
        assert_eq!(created.scheduled[0].player_two, 2);

        let first = server
            .swarm_match_result(MatchResultParams {
                tournament_id: "cup".to_string(),
                match_id: 1,
                winner: 1,
            })
            .expect("first result records");
        assert_eq!(first.loser, 2);
        assert_eq!(first.champion, None);
        assert_eq!(first.scheduled.len(), 1);

        server
            .swarm_match_result(MatchResultParams {
                tournament_id: "cup".to_string(),
                match_id: 2,
                winner: 3,
            })
            .expect("second result records and schedules final");
        let status = server.swarm_tournament_status(McpContext {
            player_id: 1,
            tick: 20,
        });
        assert_eq!(status.tournaments.len(), 1);
        assert_eq!(status.tournaments[0].scheduled.len(), 1);
        assert_eq!(status.tournaments[0].scheduled[0].player_one, 1);
        assert_eq!(status.tournaments[0].scheduled[0].player_two, 3);

        let final_result = server
            .swarm_match_result(MatchResultParams {
                tournament_id: "cup".to_string(),
                match_id: status.tournaments[0].scheduled[0].match_id,
                winner: 1,
            })
            .expect("final result records champion");
        assert_eq!(final_result.champion, Some(1));
        assert!(final_result.scheduled.is_empty());
        assert_eq!(server.tournaments()["cup"].completed.len(), 3);
    }
}
