use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bevy::prelude::*;
use ed25519_dalek::{Signature, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::arena::{
    ArenaReplay, ReplayPrivacy, TournamentBracket, TournamentElimination, TournamentMatchSchedule,
};
use crate::auth::{
    AuthCertificate, AuthCertificatePayload, AuthChallenge, CertificateBundle, CertificateIssuer,
    PlayerCertificate, PlayerCertificatePayload, PlayerRecord, StoredCertificate,
    public_key_from_csr, validate_pow, verify_csr_signature, verify_renewal_signature,
};
use crate::command::{
    CORE_COMMAND_ACTIONS, CommandAuth, CommandIntent, CommandSource, ObjectId, RawCommand,
    RejectionReason, SPECIAL_COMMAND_ACTIONS, Tick, canonical_rejection_reason, object_id,
    validate_command,
};
use crate::components::*;
use crate::economy::*;
use crate::hot_cache::{SnapshotKey, read_through_snapshot_cache};
use crate::resources::{PendingGlobalTransfers, PlayerGlobalStorage, PlayerLocalStorage};
use crate::sandbox_transport::{
    ActiveDeployment, ActiveDeployments, SandboxBackend, deploy_module_remote,
};
use crate::tick::{TickTrace, tick_key};
use crate::visibility::{
    VISIBILITY_RADIUS, is_position_visible_to, visible_entity_ids_with_positions, visible_positions,
};
use crate::world::SwarmWorld;

const MAX_WASM_BYTES: usize = 5 * 1024 * 1024;
const CERTIFICATE_TTL_SECONDS: u64 = 24 * 60 * 60;
const WEB_ACCESS_TOKEN_TTL_SECONDS: u64 = 15 * 60;
const WEB_REFRESH_TOKEN_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;
const CERTIFICATE_AUDIENCE: &str = "swarm-wasm-deploy";
const WEB_TOKEN_AUDIENCE: &str = "swarm-web";
const AUTH_CHALLENGE_TTL_SECONDS: u64 = 5 * 60;
const AUTH_POW_DIFFICULTY_BITS: u32 = 24;
const AUTH_POW_MIN_DIFFICULTY_BITS: u32 = 20;
const AUTH_POW_MAX_DIFFICULTY_BITS: u32 = 32;
const AUTH_CLIENT_CERT_TTL_SECONDS: u64 = 24 * 60 * 60;
const AUTH_CODE_SIGNING_CERT_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;
const AUTH_CLIENT_AUDIENCE: &str = "swarm-mcp";
const AUTH_CODE_SIGNING_AUDIENCE: &str = "swarm-wasm-deploy";

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

    fn feature_gated(feature: impl Into<String>) -> Self {
        Self {
            code: -32000,
            message: format!("ERR_FEATURE_GATED: {}", feature.into()),
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterChallengeParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterChallengeResult {
    pub challenge_id: String,
    pub challenge: String,
    pub difficulty_bits: u32,
    pub expires_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitCsrParams {
    pub username: String,
    pub csr: String,
    pub certificate_profile: String,
    pub challenge_id: String,
    pub nonce: String,
    pub csr_signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitCsrResult {
    pub certificate_bundle: CertificateBundle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenewCertificateParams {
    pub certificate_id: String,
    pub renewal_csr: String,
    pub proof_signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenewCertificateResult {
    pub certificate_bundle: CertificateBundle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeCertificateParams {
    pub certificate_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokeCertificateResult {
    pub revoked: bool,
    pub revocation_time: u64,
    pub crl_updated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertListParams {
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertListEntry {
    pub cert_id: String,
    pub usage: String,
    pub label: String,
    pub fingerprint: String,
    pub issued_at: u64,
    pub expires_at: u64,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertListResult {
    pub certificates: Vec<CertListEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertCheckParams {
    pub certificate_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertCheckResult {
    pub valid: bool,
    pub certificate_id: String,
    pub player_id: PlayerId,
    pub client_public_key: String,
    pub public_key_fingerprint: String,
    pub usage: String,
    pub scope: String,
    pub audience: String,
    pub expires_at: u64,
    pub revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerTrustResult {
    pub server_id: String,
    pub server_ca_fingerprint: String,
    pub server_ca_certificate: String,
    pub supported_algorithms: Vec<String>,
    pub supported_audiences: Vec<String>,
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
    pub cache_status: String,
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
    pub redb_version_counter: u64,
    pub object_store_key: String,
    pub module_hash: String,
    pub load_after_tick: Tick,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListDeploymentsParams {
    pub player_id: Option<PlayerId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentInfo {
    pub id: String,
    pub drone_id: Option<ObjectId>,
    pub room_id: u32,
    pub player_id: PlayerId,
    pub status: String,
    pub at: String,
    pub redb_version_counter: u64,
    pub object_store_key: String,
    pub hash: String,
    pub language: String,
    pub size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListDeploymentsResult {
    pub deployments: Vec<DeploymentInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredModule {
    pub module_id: String,
    pub player_id: PlayerId,
    pub room_id: RoomId,
    pub wasm_bytes: Vec<u8>,
    pub wasm_hash: String,
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
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    Unauthenticated,
    WebSessionOk,
    AppCertRequired,
    AdminCertRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub auth_mode: AuthMode,
    pub input_schema: Value,
    pub output_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableActionsResult {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub wasm_actions: Vec<String>,
    pub mcp_tools: Vec<ToolInfo>,
}

fn tool_info(name: &str, description: &str) -> ToolInfo {
    ToolInfo {
        name: name.to_string(),
        description: description.to_string(),
        auth_mode: mcp_tool_auth_mode(name).unwrap_or(AuthMode::WebSessionOk),
        input_schema: json!({}),
        output_schema: json!({}),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TickTraceParams {
    pub tick: Tick,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TickTraceResult {
    pub tick: Tick,
    pub commands: Vec<RawCommand>,
    pub state_diff: Value,
    pub rejections: Vec<crate::command::CommandRejection>,
    pub metrics: crate::tick::TickMetrics,
    pub state_checksum: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineStatsResult {
    pub tick_duration: u64,
    pub player_count: usize,
    pub memory: EngineMemoryStats,
    pub cpu: EngineCpuStats,
    pub sandbox_stats: SandboxStats,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineMemoryStats {
    pub deployed_modules: usize,
    pub cached_modules: usize,
    pub wasm_bytes: usize,
    pub tick_traces: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineCpuStats {
    pub total_commands: u64,
    pub accepted_commands: u64,
    pub rejected_commands: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxStats {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cached_modules: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleMetadata {
    pub module_hash: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModuleCacheStats {
    pub entries: usize,
    pub hits: u64,
    pub misses: u64,
    pub recompiles: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxProfileParams {
    pub drone_id: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxProfileResult {
    pub drone_id: ObjectId,
    pub fuel_used: u64,
    pub host_calls: u64,
    pub memory_peak: usize,
    pub execution_time: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListErrorsParams {
    pub player_id: Option<PlayerId>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorInfo {
    pub tick: Tick,
    pub drone: Option<ObjectId>,
    pub code: RejectionReason,
    pub detail: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListErrorsResult {
    pub errors: Vec<ErrorInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateChecksumParams {
    pub tick: Option<Tick>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateChecksumResult {
    pub checksum: String,
    pub algorithm: String,
    pub scope: String,
    pub tick: Tick,
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
    pub rejection: String,
    pub code: String,
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
    sandbox_backend: Option<SandboxBackend>,
    active_deployments: Option<ActiveDeployments>,
    sandbox_runtime: Option<Arc<tokio::runtime::Runtime>>,
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
            sandbox_backend: None,
            active_deployments: None,
            sandbox_runtime: None,
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
            sandbox_backend: None,
            active_deployments: None,
            sandbox_runtime: None,
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

    pub fn with_sandbox_backend(sandbox_backend: SandboxBackend) -> Self {
        Self {
            sandbox_backend: Some(sandbox_backend),
            active_deployments: None,
            sandbox_runtime: Some(new_sandbox_runtime()),
            ..Self::new()
        }
    }

    pub fn with_runtime_state(
        sandbox_backend: SandboxBackend,
        active_deployments: ActiveDeployments,
    ) -> Self {
        Self {
            sandbox_backend: Some(sandbox_backend),
            active_deployments: Some(active_deployments),
            sandbox_runtime: Some(new_sandbox_runtime()),
            ..Self::new()
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
            "swarm_get_structure" => {
                let params: InspectEntityParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(swarm_get_structure(world, context, params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_get_controller" => {
                let params: InspectEntityParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(swarm_get_controller(world, context, params)?)
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
            "swarm_get_events" => Ok(self.swarm_get_events(world, context)),
            "swarm_get_messages" => Ok(world_view_messages(params)?),

            "swarm_get_tick_trace" => {
                let params: TickTraceParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(self.swarm_get_tick_trace(params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_get_engine_stats" => serde_json::to_value(self.swarm_get_engine_stats(world))
                .map_err(|error| McpError::invalid_params(error.to_string())),
            "swarm_get_state_checksum" => {
                let params: StateChecksumParams = if params.is_null() {
                    StateChecksumParams { tick: None }
                } else {
                    serde_json::from_value(params)
                        .map_err(|error| McpError::invalid_params(error.to_string()))?
                };
                serde_json::to_value(self.swarm_get_state_checksum(world, context, params))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_get_sandbox_profile" => {
                let params: SandboxProfileParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(self.swarm_get_sandbox_profile(params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_list_errors" => {
                let params: ListErrorsParams = if params.is_null() {
                    ListErrorsParams {
                        player_id: None,
                        limit: None,
                    }
                } else {
                    serde_json::from_value(params)
                        .map_err(|error| McpError::invalid_params(error.to_string()))?
                };
                serde_json::to_value(self.swarm_list_errors(params))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }

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
            "swarm_register_challenge" => {
                let _params: RegisterChallengeParams = parse_empty_params(params)?;
                serde_json::to_value(self.swarm_register_challenge(world)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_submit_csr" => {
                let params: SubmitCsrParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(self.swarm_submit_csr(world, params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_renew_certificate" => {
                let params: RenewCertificateParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(self.swarm_renew_certificate(world, params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_revoke_certificate" => {
                let params: RevokeCertificateParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(self.swarm_revoke_certificate(world, params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_cert_list" => {
                let params: CertListParams = if params.is_null() {
                    CertListParams { status: None }
                } else {
                    serde_json::from_value(params)
                        .map_err(|error| McpError::invalid_params(error.to_string()))?
                };
                serde_json::to_value(self.swarm_cert_list(world, params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_cert_check" => {
                let params: CertCheckParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(self.swarm_cert_check(world, params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_get_server_trust" => {
                let _params: RegisterChallengeParams = parse_empty_params(params)?;
                serde_json::to_value(self.swarm_get_server_trust())
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_get_world_config" => Ok(swarm_get_world_config()),
            "swarm_tournament_precommit"
            | "swarm_tournament_create"
            | "swarm_tournament_status" => Err(McpError::feature_gated(tool)),
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
                serde_json::to_value(self.swarm_get_deploy_status(params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_list_deployments" => {
                let params: ListDeploymentsParams = if params.is_null() {
                    ListDeploymentsParams { player_id: None }
                } else {
                    serde_json::from_value(params)
                        .map_err(|error| McpError::invalid_params(error.to_string()))?
                };
                serde_json::to_value(self.swarm_list_deployments(params))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_validate_module" => {
                let params: ValidateModuleParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(swarm_validate_module(params))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_dry_run" => {
                let params: DryRunCommandsParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(swarm_dry_run(world, context, params))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
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

    pub fn swarm_register_challenge(
        &mut self,
        world: &mut SwarmWorld,
    ) -> Result<RegisterChallengeResult, McpError> {
        let mut challenge_bytes = [0_u8; 16];
        getrandom::fill(&mut challenge_bytes)
            .map_err(|error| McpError::invalid_params(error.to_string()))?;
        let challenge = bytes_to_hex(&challenge_bytes);
        let challenge_id = format!(
            "chal_{}",
            blake3::hash(format!("{challenge}:{}", self.now_seconds()).as_bytes()).to_hex()
        );
        let difficulty_bits = AUTH_POW_DIFFICULTY_BITS
            .clamp(AUTH_POW_MIN_DIFFICULTY_BITS, AUTH_POW_MAX_DIFFICULTY_BITS);
        let expires_at = self.now_seconds() + AUTH_CHALLENGE_TTL_SECONDS;
        let row = AuthChallenge {
            challenge_id: challenge_id.clone(),
            challenge: challenge.clone(),
            difficulty_bits,
            expires_at,
            consumed: false,
        };
        auth_store_write(
            world,
            &auth_challenge_key(&challenge_id),
            &row,
            "auth challenge",
        )?;
        Ok(RegisterChallengeResult {
            challenge_id,
            challenge,
            difficulty_bits,
            expires_at,
        })
    }

    pub fn swarm_submit_csr(
        &mut self,
        world: &mut SwarmWorld,
        params: SubmitCsrParams,
    ) -> Result<SubmitCsrResult, McpError> {
        let username = normalized_username(&params.username)?;
        if params.challenge_id.trim().is_empty() {
            return Err(McpError::invalid_params("challenge_id is required"));
        }
        if params.nonce.trim().is_empty() {
            return Err(McpError::invalid_params("nonce is required"));
        }
        if params.certificate_profile.trim().is_empty() {
            return Err(McpError::invalid_params("certificate_profile is required"));
        }
        let challenge_key = auth_challenge_key(&params.challenge_id);
        let mut challenge: AuthChallenge = auth_store_read(world, &challenge_key)?
            .ok_or_else(|| McpError::invalid_params("challenge_id is invalid"))?;
        let now = self.now_seconds();
        if challenge.expires_at <= now {
            return Err(McpError::invalid_params("challenge is expired"));
        }
        if challenge.consumed {
            return Err(McpError::invalid_params("challenge is consumed"));
        }
        if !validate_pow(
            &challenge.challenge,
            &params.nonce,
            challenge.difficulty_bits,
        ) {
            return Err(McpError::invalid_params("proof of work is invalid"));
        }
        let public_key = public_key_from_csr(&params.csr)?;
        verify_csr_signature(
            &public_key,
            &params.csr,
            &params.challenge_id,
            &params.nonce,
            &params.csr_signature,
        )?;
        let player_id = local_player_id(&username);
        if auth_store_read::<PlayerRecord>(world, &auth_player_key(player_id))?.is_some() {
            return Err(McpError::invalid_params("username is already registered"));
        }
        let bundle = self.issue_auth_bundle(player_id, &username, &public_key)?;
        let player = PlayerRecord {
            username,
            public_key,
            created_at: now,
        };
        challenge.consumed = true;
        let writes = auth_bundle_writes(&bundle, &player, &challenge)?;
        auth_store_write_batch(world, writes)?;
        Ok(SubmitCsrResult {
            certificate_bundle: bundle,
        })
    }

    pub fn swarm_renew_certificate(
        &mut self,
        world: &mut SwarmWorld,
        params: RenewCertificateParams,
    ) -> Result<RenewCertificateResult, McpError> {
        let stored = self.read_stored_certificate(world, &params.certificate_id)?;
        if stored.revoked {
            return Err(McpError::invalid_params("certificate is revoked"));
        }
        if stored.expires_at <= self.now_seconds() {
            return Err(McpError::invalid_params("certificate is expired"));
        }
        let old_cert = parse_auth_certificate(&stored.certificate_json)?;
        let old_key =
            decode_ed25519_public_key(&old_cert.payload.public_key, "certificate public_key")?;
        verify_renewal_signature(
            &old_key,
            &params.renewal_csr,
            &params.certificate_id,
            &params.proof_signature,
        )?;
        let public_key = public_key_from_csr(&params.renewal_csr)?;
        let username = auth_store_read::<PlayerRecord>(world, &auth_player_key(stored.player_id))?
            .map(|record| record.username)
            .unwrap_or_else(|| format!("player-{}", stored.player_id));
        let bundle = self.issue_auth_bundle(stored.player_id, &username, &public_key)?;
        let player = PlayerRecord {
            username,
            public_key,
            created_at: self.now_seconds(),
        };
        let writes = auth_bundle_writes_without_challenge(&bundle, &player)?;
        auth_store_write_batch(world, writes)?;
        Ok(RenewCertificateResult {
            certificate_bundle: bundle,
        })
    }

    pub fn swarm_revoke_certificate(
        &mut self,
        world: &mut SwarmWorld,
        params: RevokeCertificateParams,
    ) -> Result<RevokeCertificateResult, McpError> {
        if params.reason.trim().is_empty() {
            return Err(McpError::invalid_params("reason is required"));
        }
        let key = auth_certificate_key(&params.certificate_id);
        let mut stored: StoredCertificate = auth_store_read(world, &key)?
            .ok_or_else(|| McpError::invalid_params("certificate_id is invalid"))?;
        let already_revoked = stored.revoked;
        stored.revoked = true;
        let revocation_time = self.now_seconds();
        let revocation = json!({
            "certificate_id": params.certificate_id,
            "reason": params.reason,
            "revocation_time": revocation_time,
        });
        let writes = vec![
            (
                key,
                crate::redb_store::RedbStore::encode_json(&stored, "auth certificate")
                    .map_err(redb_to_mcp)?,
            ),
            (
                auth_revocation_key(&params.certificate_id),
                crate::redb_store::RedbStore::encode_json(&revocation, "auth revocation")
                    .map_err(redb_to_mcp)?,
            ),
        ];
        auth_store_write_batch(world, writes)?;
        Ok(RevokeCertificateResult {
            revoked: !already_revoked,
            revocation_time,
            crl_updated: true,
        })
    }

    pub fn swarm_cert_list(
        &self,
        world: &mut SwarmWorld,
        params: CertListParams,
    ) -> Result<CertListResult, McpError> {
        let now = self.now_seconds();
        let mut certificates = Vec::new();
        for (key, stored) in auth_store_scan::<StoredCertificate>(world, b"auth/certificates/")? {
            let cert_id = String::from_utf8_lossy(&key)
                .trim_start_matches("auth/certificates/")
                .to_string();
            let cert = parse_auth_certificate(&stored.certificate_json)?;
            let status = certificate_status(&stored, now);
            if params
                .status
                .as_deref()
                .is_some_and(|filter| filter != status)
            {
                continue;
            }
            certificates.push(CertListEntry {
                cert_id,
                usage: stored.usage,
                label: cert.payload.label,
                fingerprint: stored.fingerprint,
                issued_at: stored.issued_at,
                expires_at: stored.expires_at,
                status: status.to_string(),
            });
        }
        certificates.sort_by(|left, right| left.cert_id.cmp(&right.cert_id));
        Ok(CertListResult { certificates })
    }

    pub fn swarm_cert_check(
        &self,
        world: &mut SwarmWorld,
        params: CertCheckParams,
    ) -> Result<CertCheckResult, McpError> {
        let stored = self.read_stored_certificate(world, &params.certificate_id)?;
        let cert = parse_auth_certificate(&stored.certificate_json)?;
        let verified = self.issuer.verify_auth(&cert).is_ok();
        let revoked = stored.revoked;
        let valid = verified && !revoked && stored.expires_at > self.now_seconds();
        Ok(CertCheckResult {
            valid,
            certificate_id: cert.payload.cert_id,
            player_id: stored.player_id,
            client_public_key: cert.payload.public_key,
            public_key_fingerprint: cert.payload.public_key_fingerprint,
            usage: stored.usage,
            scope: cert.payload.scope,
            audience: cert.payload.audience,
            expires_at: stored.expires_at,
            revoked,
        })
    }

    pub fn swarm_get_server_trust(&self) -> ServerTrustResult {
        let fingerprint = self.issuer.public_key_fingerprint();
        ServerTrustResult {
            server_id: format!("swarm-server-{fingerprint}"),
            server_ca_fingerprint: fingerprint,
            server_ca_certificate: self.issuer.public_key(),
            supported_algorithms: vec!["Ed25519".to_string(), "BLAKE3".to_string()],
            supported_audiences: vec![
                AUTH_CLIENT_AUDIENCE.to_string(),
                AUTH_CODE_SIGNING_AUDIENCE.to_string(),
            ],
        }
    }

    fn read_stored_certificate(
        &self,
        world: &mut SwarmWorld,
        certificate_id: &str,
    ) -> Result<StoredCertificate, McpError> {
        if certificate_id.trim().is_empty() {
            return Err(McpError::invalid_params("certificate_id is required"));
        }
        auth_store_read(world, &auth_certificate_key(certificate_id))?
            .ok_or_else(|| McpError::invalid_params("certificate_id is invalid"))
    }

    fn issue_auth_bundle(
        &self,
        player_id: PlayerId,
        username: &str,
        public_key: &str,
    ) -> Result<CertificateBundle, McpError> {
        let issued_at = self.now_seconds();
        let fingerprint = public_key_fingerprint(public_key)?;
        let cert_id = format!(
            "cert_{}",
            blake3::hash(format!("{player_id}:{fingerprint}:{issued_at}").as_bytes()).to_hex()
        );
        let client_auth_cert = self.issue_auth_certificate(
            &cert_id,
            "client_auth",
            player_id,
            public_key,
            &fingerprint,
            "mcp rest websocket",
            AUTH_CLIENT_AUDIENCE,
            &format!("{username} client auth"),
            issued_at,
            issued_at + AUTH_CLIENT_CERT_TTL_SECONDS,
        )?;
        let code_signing_cert = self.issue_auth_certificate(
            &format!("{cert_id}:code"),
            "code_signing",
            player_id,
            public_key,
            &fingerprint,
            "wasm:deploy",
            AUTH_CODE_SIGNING_AUDIENCE,
            &format!("{username} code signing"),
            issued_at,
            issued_at + AUTH_CODE_SIGNING_CERT_TTL_SECONDS,
        )?;
        Ok(CertificateBundle {
            client_auth_cert: serde_json::to_string(&client_auth_cert)
                .map_err(|error| McpError::invalid_params(error.to_string()))?,
            code_signing_cert: serde_json::to_string(&code_signing_cert)
                .map_err(|error| McpError::invalid_params(error.to_string()))?,
            cert_id,
            player_id,
            public_key_fingerprint: fingerprint,
            issued_at,
            expires_at: issued_at + AUTH_CLIENT_CERT_TTL_SECONDS,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn issue_auth_certificate(
        &self,
        cert_id: &str,
        usage: &str,
        player_id: PlayerId,
        public_key: &str,
        public_key_fingerprint: &str,
        scope: &str,
        audience: &str,
        label: &str,
        issued_at: u64,
        expires_at: u64,
    ) -> Result<AuthCertificate, McpError> {
        self.issuer.issue_auth(AuthCertificatePayload {
            cert_id: cert_id.to_string(),
            usage: usage.to_string(),
            player_id,
            public_key: public_key.to_string(),
            public_key_fingerprint: public_key_fingerprint.to_string(),
            scope: scope.to_string(),
            audience: audience.to_string(),
            label: label.to_string(),
            issued_at,
            expires_at,
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
        let wasm_hash_hex = wasm_hash.to_hex().to_string();

        let module_id = format!(
            "mod_{}_{}_{}",
            context.player_id,
            params.room_id,
            self.modules.len() + 1
        );
        let deployed_at = unix_timestamp_string();
        let module = StoredModule {
            module_id: module_id.clone(),
            player_id: context.player_id,
            room_id,
            wasm_bytes: wasm_bytes.clone(),
            wasm_hash: wasm_hash_hex.clone(),
            certificate: params.certificate,
            wasm_signature: params.wasm_signature,
            language: params.language,
            version_tag: params.version_tag,
            deployed_at: deployed_at.clone(),
            load_after_tick: context.tick + 1,
        };

        let wasm_hash_raw = *wasm_hash.as_bytes();
        if let Some(SandboxBackend::Remote { nats_client, .. }) = &self.sandbox_backend {
            let runtime = self
                .sandbox_runtime
                .as_ref()
                .ok_or_else(|| McpError::invalid_params("sandbox runtime unavailable"))?;
            runtime
                .block_on(deploy_module_remote(
                    nats_client,
                    &wasm_hash_raw,
                    &wasm_bytes,
                ))
                .map_err(McpError::invalid_params)?;
        }

        self.modules.push(module);

        if let Some(active_deployments) = &self.active_deployments {
            active_deployments.activate(ActiveDeployment {
                player_id: context.player_id,
                room_id,
                module_hash: wasm_hash_raw,
                wasm_bytes: wasm_bytes.clone(),
                load_after_tick: context.tick + 1,
            });
        }

        Ok(DeployResult {
            module_id,
            status: "pending_next_tick".to_string(),
            deployed_at,
            module_hash: wasm_hash_hex,
            cache_status: "remote_pending".to_string(),
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

    pub fn swarm_get_events(&self, world: &mut SwarmWorld, context: McpContext) -> Value {
        let mut events = Vec::new();
        if let Some(log) = world.app.world().get_resource::<EventLog>() {
            events.extend(
                log.entries
                    .iter()
                    .filter(|entry| {
                        entry
                            .player_id
                            .is_none_or(|player| player == context.player_id)
                    })
                    .map(|entry| {
                        json!({
                            "tick": entry.tick,
                            "type": entry.event_type,
                            "data": {
                                "message": entry.message,
                                "player_id": entry.player_id,
                            }
                        })
                    }),
            );
        }
        for trace in &self.tick_traces {
            for event in &trace.trace_events {
                if !trace_event_visible(world, context.player_id, event.entity) {
                    continue;
                }
                events.push(json!({
                    "tick": trace.tick,
                    "type": event.event,
                    "data": {
                        "system": event.system,
                        "entity": event.entity,
                        "amount": event.amount,
                        "resource": event.resource,
                    }
                }));
            }
        }
        events.sort_by_key(|event| {
            event
                .get("tick")
                .and_then(Value::as_u64)
                .unwrap_or_default()
        });
        json!({ "events": events })
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

    pub fn compile_module_for_tick(&mut self, module_id: &str) -> Result<ModuleMetadata, McpError> {
        let module = self
            .modules
            .iter()
            .find(|module| module.module_id == module_id)
            .ok_or_else(|| McpError::invalid_params("module_id is not deployed"))?;

        Ok(ModuleMetadata {
            module_hash: module.wasm_hash.clone(),
        })
    }

    pub fn module_cache_stats(&self) -> ModuleCacheStats {
        ModuleCacheStats::default()
    }

    pub fn swarm_profile(&self, world: &mut SwarmWorld, context: McpContext) -> ProfileResult {
        swarm_profile(world, context, self.modules.len())
    }

    pub fn swarm_get_tick_trace(
        &self,
        params: TickTraceParams,
    ) -> Result<TickTraceResult, McpError> {
        let trace = self
            .tick_traces
            .iter()
            .rev()
            .find(|trace| trace.tick == params.tick)
            .ok_or_else(|| McpError::invalid_params("tick trace not found"))?;
        Ok(TickTraceResult {
            tick: trace.tick,
            commands: trace.commands.clone(),
            state_diff: serde_json::to_value(&trace.state)
                .map_err(|error| McpError::invalid_params(error.to_string()))?,
            rejections: trace.rejections.clone(),
            metrics: trace.metrics.clone(),
            state_checksum: trace.state_checksum,
        })
    }

    pub fn swarm_get_engine_stats(&self, world: &mut SwarmWorld) -> EngineStatsResult {
        let metrics = aggregate_tick_metrics(&self.tick_traces);
        EngineStatsResult {
            tick_duration: metrics.duration_ms,
            player_count: world_player_count(world),
            memory: EngineMemoryStats {
                deployed_modules: self.modules.len(),
                cached_modules: 0,
                wasm_bytes: self
                    .modules
                    .iter()
                    .map(|module| module.wasm_bytes.len())
                    .sum(),
                tick_traces: self.tick_traces.len(),
            },
            cpu: EngineCpuStats {
                total_commands: metrics.total_commands,
                accepted_commands: metrics.accepted_commands,
                rejected_commands: metrics.rejected_commands,
                duration_ms: metrics.duration_ms,
            },
            sandbox_stats: SandboxStats {
                cache_hits: 0,
                cache_misses: 0,
                cached_modules: 0,
            },
        }
    }

    pub fn swarm_get_sandbox_profile(
        &self,
        params: SandboxProfileParams,
    ) -> Result<SandboxProfileResult, McpError> {
        let mut fuel_used = 0;
        let mut host_calls = 0;
        let mut execution_time = 0;
        for trace in &self.tick_traces {
            fuel_used += trace.metrics.fuel_consumed;
            host_calls += trace.metrics.total_commands;
            execution_time += trace.metrics.execute_duration_ms;
        }
        Ok(SandboxProfileResult {
            drone_id: params.drone_id,
            fuel_used,
            host_calls,
            memory_peak: self
                .modules
                .iter()
                .map(|module| module.wasm_bytes.len())
                .max()
                .unwrap_or(0),
            execution_time,
        })
    }

    pub fn swarm_list_errors(&self, params: ListErrorsParams) -> ListErrorsResult {
        let mut errors = self
            .tick_traces
            .iter()
            .flat_map(|trace| {
                trace.rejections.iter().filter_map(move |rejection| {
                    if params
                        .player_id
                        .is_some_and(|player_id| player_id != rejection.command.player_id)
                    {
                        return None;
                    }
                    Some(ErrorInfo {
                        tick: trace.tick,
                        drone: Some(ObjectId::from(rejection.command.sequence)),
                        code: rejection.rejection.clone(),
                        detail: rejection.detail.clone(),
                    })
                })
            })
            .collect::<Vec<_>>();
        errors.sort_by_key(|error| error.tick);
        if let Some(limit) = params.limit {
            let keep_from = errors.len().saturating_sub(limit);
            errors = errors.split_off(keep_from);
        }
        ListErrorsResult { errors }
    }

    pub fn swarm_get_state_checksum(
        &self,
        world: &mut SwarmWorld,
        context: McpContext,
        params: StateChecksumParams,
    ) -> StateChecksumResult {
        let tick = params.tick.unwrap_or(context.tick);
        let checksum = self
            .tick_traces
            .iter()
            .rev()
            .find(|trace| trace.tick == tick)
            .map(|trace| trace.state_checksum)
            .unwrap_or_else(|| world.state_checksum());
        StateChecksumResult {
            checksum: format!("blake3-u64:{checksum:016x}"),
            algorithm: "blake3-u64".to_string(),
            scope: "world".to_string(),
            tick,
        }
    }

    pub fn swarm_get_deploy_status(
        &self,
        params: DeployStatusParams,
    ) -> Result<DeployStatusResult, McpError> {
        let module = self
            .modules
            .iter()
            .find(|module| module.module_id == params.deploy_id)
            .ok_or_else(|| McpError::invalid_params("deploy_id is not deployed"))?;
        Ok(deploy_status_for_module(module))
    }

    pub fn swarm_list_deployments(&self, params: ListDeploymentsParams) -> ListDeploymentsResult {
        ListDeploymentsResult {
            deployments: self
                .modules
                .iter()
                .filter(|module| {
                    params.player_id.is_none() || params.player_id == Some(module.player_id)
                })
                .map(deployment_info_for_module)
                .collect(),
        }
    }
}

fn new_sandbox_runtime() -> Arc<tokio::runtime::Runtime> {
    Arc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build MCP sandbox runtime"),
    )
}

fn mcp_tool_infos() -> Vec<ToolInfo> {
    vec![
        tool_info(
            "swarm_get_snapshot",
            "Get the visible world state for a player at the current tick",
        ),
        tool_info("swarm_get_terrain", "Get terrain type at room coordinates"),
        tool_info(
            "swarm_get_world_rules",
            "Get the world rules and mods configuration",
        ),
        tool_info("swarm_get_schema", "Get the CommandIntent JSON Schema"),
        tool_info(
            "swarm_get_available_actions",
            "List all WASM actions and MCP tools available to the player",
        ),
        tool_info(
            "swarm_explain_last_tick",
            "Explain the last tick's results for a player",
        ),
        tool_info(
            "swarm_get_tick_trace",
            "Get commands, state diff, rejections, and metrics for a tick",
        ),
        tool_info(
            "swarm_get_engine_stats",
            "Get engine tick, memory, CPU, and sandbox cache statistics",
        ),
        tool_info(
            "swarm_get_state_checksum",
            "Get deterministic world state checksum for current or traced tick",
        ),
        tool_info(
            "swarm_get_sandbox_profile",
            "Get sandbox fuel, host call, memory, and execution profile for a drone",
        ),
        tool_info(
            "swarm_list_errors",
            "List command rejections collected from tick traces",
        ),
        tool_info(
            "swarm_get_drone",
            "Get full state for an owned or visible drone",
        ),
        tool_info("swarm_get_room", "Get a room visible to the player"),
        tool_info("swarm_get_structure", "Get visible structure state by id"),
        tool_info("swarm_get_controller", "Get visible controller state by id"),
        tool_info("swarm_get_code", "Get deployed code metadata for a drone"),
        tool_info("swarm_get_visibility", "Get visible rooms and entities"),
        tool_info(
            "swarm_get_path",
            "Return a simple path between visible positions",
        ),
        tool_info("swarm_get_resources", "Get visible player resources"),
        tool_info("swarm_get_info", "Get world metadata"),
        tool_info("swarm_list_drones", "List visible drones"),
        tool_info("swarm_list_rooms", "List visible rooms"),
        tool_info("swarm_list_structures", "List visible structures"),
        tool_info("swarm_list_controllers", "List visible controllers"),
        tool_info("swarm_get_events", "Get visible events"),
        tool_info("swarm_get_messages", "Get drone messages"),
        tool_info("swarm_profile", "Profile a player's current world state"),
        tool_info(
            "swarm_get_economy",
            "Summarize player economy income, expenses, storage tax, and maintenance",
        ),
        tool_info(
            "swarm_get_economy_trend",
            "Return deterministic economy trend points for recent ticks",
        ),
        tool_info(
            "swarm_get_drone_efficiency",
            "Estimate a drone efficiency percentage from fatigue, health, spawning, and carry state",
        ),
        tool_info(
            "swarm_sdk_fetch",
            "Fetch a minimal SDK starter package for bot development",
        ),
        tool_info(
            "swarm_dry_run",
            "Dry-run commands without mutating the world",
        ),
        tool_info(
            "swarm_simulate",
            "Predict future ticks from a visible world snapshot without mutating live state",
        ),
        tool_info(
            "swarm_get_replay",
            "Retrieve replay data as entity-change deltas between two ticks, anchored on the nearest keyframe",
        ),
        tool_info(
            "swarm_get_docs",
            "Get Swarm documentation and reference material",
        ),
        tool_info("resources/list", "List available resource types"),
        tool_info("resources/read", "Read resource definitions"),
        tool_info(
            "swarm_register_challenge",
            "Create a proof-of-work challenge for CSR certificate registration",
        ),
        tool_info(
            "swarm_submit_csr",
            "Submit a certificate signing request for application-layer auth",
        ),
        tool_info(
            "swarm_renew_certificate",
            "Renew an application-layer certificate",
        ),
        tool_info(
            "swarm_revoke_certificate",
            "Revoke an application-layer certificate",
        ),
        tool_info(
            "swarm_cert_list",
            "List application-layer certificates for the caller",
        ),
        tool_info(
            "swarm_cert_check",
            "Check application-layer certificate status",
        ),
        tool_info(
            "swarm_get_server_trust",
            "Return server trust anchors for application-layer auth",
        ),
        tool_info(
            "swarm_get_world_config",
            "Get world-level Auth and rules configuration metadata",
        ),
        tool_info(
            "swarm_list_modules",
            "List all deployed WASM modules across all players",
        ),
        tool_info("swarm_deploy", "Deploy a WASM module for a player"),
        tool_info(
            "swarm_get_deploy_status",
            "Inspect status and object-store pointer for a deployed module",
        ),
        tool_info(
            "swarm_list_deployments",
            "List deployments, optionally filtered by player",
        ),
        tool_info(
            "swarm_validate_module",
            "Validate a WASM module before deployment",
        ),
        tool_info(
            "swarm_tournament_precommit",
            "Lock a previously deployed WASM module for an AI tournament before match start",
        ),
        tool_info(
            "swarm_tournament_status",
            "Inspect AI tournament preparation and locked-code status",
        ),
        tool_info(
            "swarm_tournament_create",
            "Create and schedule a single- or double-elimination AI tournament from precommitted modules",
        ),
        tool_info(
            "swarm_match_result",
            "Record a scheduled tournament match winner and advance the bracket",
        ),
    ]
}

fn mcp_tool_source(tool: &str) -> Option<CommandSource> {
    match tool {
        "swarm_deploy"
        | "swarm_validate_module"
        | "swarm_tournament_precommit"
        | "swarm_tournament_create" => Some(CommandSource::McpDeploy),
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
        | "swarm_sdk_fetch"
        | "swarm_get_tick_trace"
        | "swarm_get_engine_stats"
        | "swarm_get_state_checksum"
        | "swarm_get_sandbox_profile"
        | "swarm_list_errors"
        | "swarm_get_docs"
        | "resources/list"
        | "resources/read"
        | "swarm_register_challenge"
        | "swarm_submit_csr"
        | "swarm_renew_certificate"
        | "swarm_revoke_certificate"
        | "swarm_cert_list"
        | "swarm_cert_check"
        | "swarm_get_server_trust"
        | "swarm_get_world_config"
        | "swarm_tournament_status"
        | "swarm_match_result"
        | "swarm_list_modules"
        | "swarm_get_deploy_status"
        | "swarm_list_deployments"
        | "swarm_get_replay"
        | "swarm_get_terrain" => Some(CommandSource::McpQuery),
        "swarm_dry_run" => Some(CommandSource::DryRun),
        "swarm_simulate" => Some(CommandSource::Simulate),
        _ => None,
    }
}

fn mcp_tool_auth_mode(tool: &str) -> Option<AuthMode> {
    if matches!(
        tool,
        "swarm_register_challenge" | "swarm_submit_csr" | "swarm_get_server_trust"
    ) {
        return Some(AuthMode::Unauthenticated);
    }
    match mcp_tool_source(tool)? {
        CommandSource::Admin => Some(AuthMode::AdminCertRequired),
        CommandSource::McpDeploy | CommandSource::DryRun | CommandSource::Simulate => {
            Some(AuthMode::AppCertRequired)
        }
        CommandSource::McpQuery => Some(AuthMode::WebSessionOk),
        _ => Some(AuthMode::AppCertRequired),
    }
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

fn aggregate_tick_metrics(traces: &[TickTrace]) -> crate::tick::TickMetrics {
    let mut metrics = crate::tick::TickMetrics::default();
    for trace in traces {
        metrics.add(&trace.metrics);
    }
    metrics
}

fn world_player_count(_world: &mut SwarmWorld) -> usize {
    0
}

fn deploy_status_for_module(module: &StoredModule) -> DeployStatusResult {
    DeployStatusResult {
        deploy_id: module.module_id.clone(),
        status: "pending_next_tick".to_string(),
        errors: Vec::new(),
        deployed_at: module.deployed_at.clone(),
        redb_version_counter: module.load_after_tick,
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
        redb_version_counter: module.load_after_tick,
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
    let mut schemas = vec![
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
            "Action",
            &["action_type", "object_id"],
            json!({"action_type": {"type": "string", "not": {"enum": CORE_COMMAND_ACTIONS}}, "object_id": object_id_schema(), "target_id": object_id_schema(), "payload": {}}),
        ),
        command_action_schema(
            "ClaimController",
            &["object_id", "target_id"],
            json!({"object_id": object_id_schema(), "target_id": object_id_schema()}),
        ),
        command_action_schema(
            "Spawn",
            &["object_id", "spawn_id", "body_parts"],
            json!({"object_id": object_id_schema(), "spawn_id": object_id_schema(), "body_parts": {"type": "array", "items": body_part_schema(), "minItems": 1, "maxItems": crate::command::MAX_BODY_PARTS}}),
        ),
        command_action_schema(
            "Recycle",
            &["object_id"],
            json!({"object_id": object_id_schema()}),
        ),
        command_action_schema(
            "Build",
            &["object_id", "x", "y", "structure"],
            json!({"object_id": object_id_schema(), "x": coord_schema(), "y": coord_schema(), "structure": structure_type_schema()}),
        ),
        command_action_schema(
            "Repair",
            &["object_id", "target_id"],
            json!({"object_id": object_id_schema(), "target_id": object_id_schema()}),
        ),
        command_action_schema(
            "UpgradeController",
            &["object_id", "target_id"],
            json!({"object_id": object_id_schema(), "target_id": object_id_schema()}),
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
        command_action_schema(
            "AlliedTransfer",
            &["target_player", "resource", "amount"],
            json!({"target_player": player_id_schema(), "resource": {"type": "string"}, "amount": amount_schema()}),
        ),
    ];

    schemas.extend(
        SPECIAL_COMMAND_ACTIONS
            .iter()
            .map(|action_type| special_command_action_schema(action_type)),
    );
    schemas.push(custom_command_action_schema());
    debug_assert_eq!(
        schemas.len(),
        CORE_COMMAND_ACTIONS.len() + SPECIAL_COMMAND_ACTIONS.len() + 1
    );
    schemas
}

fn special_command_action_schema(action_type: &str) -> Value {
    command_action_schema(
        action_type,
        &["object_id", "target_id"],
        json!({"object_id": object_id_schema(), "target_id": object_id_schema(), "payload": {}, "resource": {"type": "string"}, "amount": amount_schema(), "range": uint32_schema(), "structure": structure_type_schema()}),
    )
}

fn custom_command_action_schema() -> Value {
    json!({"type": "object", "additionalProperties": false, "required": ["type", "object_id"], "properties": {"type": {"type": "string", "not": {"enum": reserved_command_action_names()}}, "object_id": object_id_schema(), "target_id": object_id_schema(), "payload": {}, "resource": {"type": "string"}, "amount": amount_schema(), "range": uint32_schema(), "structure": structure_type_schema()}})
}

fn reserved_command_action_names() -> Vec<&'static str> {
    CORE_COMMAND_ACTIONS
        .iter()
        .chain(SPECIAL_COMMAND_ACTIONS.iter())
        .copied()
        .collect()
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
fn player_id_schema() -> Value {
    uint32_schema()
}
fn amount_schema() -> Value {
    uint32_schema()
}
fn uint32_schema() -> Value {
    json!({"type": "integer", "minimum": 0, "maximum": 4294967295_u64})
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
    json!({"type": "string", "minLength": 1})
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
                    rejection: canonical_rejection_reason(&rejection.rejection).to_string(),
                    code: canonical_rejection_reason(&rejection.rejection).to_string(),
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
    let visible_ids = visible_entity_ids_with_positions(
        world.app.world_mut(),
        context.player_id,
        context.tick,
        &visible_positions,
    );
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

fn trace_event_visible(world: &mut SwarmWorld, player_id: PlayerId, object_id: ObjectId) -> bool {
    let entity = Entity::from_bits(object_id);
    let Ok(entity_ref) = world.app.world().get_entity(entity) else {
        return false;
    };
    let owned = entity_ref
        .get::<Owner>()
        .is_some_and(|owner| owner.0 == player_id)
        || entity_ref
            .get::<Drone>()
            .is_some_and(|drone| drone.owner == player_id)
        || entity_ref
            .get::<Structure>()
            .is_some_and(|structure| structure.owner == Some(player_id))
        || entity_ref
            .get::<Controller>()
            .is_some_and(|controller| controller.owner == Some(player_id));
    let position = entity_ref.get::<Position>().copied();
    owned
        || position
            .is_some_and(|position| is_visible_to(world.app.world_mut(), player_id, position))
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
            entity_ref.contains::<DeathMark>(),
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

pub fn swarm_get_structure(
    world: &mut SwarmWorld,
    context: McpContext,
    params: InspectEntityParams,
) -> Result<VisibleStructure, McpError> {
    let state = swarm_get_drone(world, context, params)?;
    let Some(structure) = state.structure else {
        return Err(McpError::invalid_params(
            "entity is not a visible structure",
        ));
    };
    let Some(position) = state.position else {
        return Err(McpError::invalid_params(
            "structure has no visible position",
        ));
    };
    Ok(VisibleStructure {
        id: state.id,
        structure_type: structure.structure_type,
        owner: structure.owner,
        position,
        hits: structure.hits,
        hits_max: structure.hits_max,
        energy: structure.energy,
        energy_capacity: structure.energy_capacity,
        cooldown: structure.cooldown,
    })
}

pub fn swarm_get_controller(
    world: &mut SwarmWorld,
    context: McpContext,
    params: InspectEntityParams,
) -> Result<VisibleController, McpError> {
    let state = swarm_get_drone(world, context, params)?;
    let Some(controller) = state.controller else {
        return Err(McpError::invalid_params(
            "entity is not a visible controller",
        ));
    };
    let Some(position) = state.position else {
        return Err(McpError::invalid_params(
            "controller has no visible position",
        ));
    };
    Ok(VisibleController {
        id: state.id,
        owner: controller.owner,
        position,
        level: controller.level,
        progress: controller.progress,
        progress_total: controller.progress_total,
        safe_mode: controller.safe_mode,
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
        .resource_mut::<crate::redb_store::RedbStore>()
        .write_visible_snapshot(snapshot.clone());

    world
        .app
        .world_mut()
        .resource_scope(
            |ecs, mut cache: Mut<'_, crate::hot_cache::InMemorySnapshotCache>| {
                let store = ecs.resource::<crate::redb_store::RedbStore>();
                read_through_snapshot_cache(&mut *cache, key, store)
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

    let visible_ids = visible_entity_ids_with_positions(
        world.app.world_mut(),
        context.player_id,
        context.tick,
        &visible_positions,
    );
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

fn swarm_get_world_config() -> Value {
    let rules = swarm_get_world_rules();
    let limits = json!({
        "room_size": rules.room_size,
        "visibility_radius": rules.visibility_radius,
        "max_wasm_bytes": rules.max_wasm_bytes,
        "max_body_parts": crate::command::MAX_BODY_PARTS,
        "max_commands_per_player": crate::command::MAX_COMMANDS_PER_PLAYER,
        "max_drones_per_player": crate::command::MAX_DRONES_PER_PLAYER,
    });
    json!({
        "rules": {
            "ruleset": rules.ruleset,
            "room_size": rules.room_size,
            "visibility_radius": rules.visibility_radius,
        },
        "mods": rules.active_mods,
        "limits": limits,
        "tick_rate": 1,
    })
}

pub fn is_visible_to(world: &mut World, player_id: PlayerId, position: Position) -> bool {
    is_position_visible_to(world, player_id, position)
}

pub fn visible_entities_for_player(world: &mut World, player_id: PlayerId) -> Vec<VisibleEntity> {
    let visible_positions = visible_positions(world, player_id);
    let visible_ids = visible_entity_ids_with_positions(world, player_id, 0, &visible_positions);
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
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

fn local_player_id(username_lowercase: &str) -> PlayerId {
    let bytes = blake3::hash(format!("local:{username_lowercase}").as_bytes());
    let mut id_bytes = [0_u8; 4];
    id_bytes.copy_from_slice(&bytes.as_bytes()[0..4]);
    u32::from_le_bytes(id_bytes)
}

fn normalized_username(username: &str) -> Result<String, McpError> {
    let normalized = username.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(McpError::invalid_params("username is required"));
    }
    Ok(normalized)
}

fn parse_empty_params<T: for<'de> Deserialize<'de> + Default>(
    params: Value,
) -> Result<T, McpError> {
    if params.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(params).map_err(|error| McpError::invalid_params(error.to_string()))
}

type AuthStoreWrite = (Vec<u8>, Vec<u8>);

fn auth_bundle_writes(
    bundle: &CertificateBundle,
    player: &PlayerRecord,
    challenge: &AuthChallenge,
) -> Result<Vec<AuthStoreWrite>, McpError> {
    let mut writes = auth_bundle_writes_without_challenge(bundle, player)?;
    writes.push((
        auth_challenge_key(&challenge.challenge_id),
        crate::redb_store::RedbStore::encode_json(challenge, "auth challenge")
            .map_err(redb_to_mcp)?,
    ));
    Ok(writes)
}

fn auth_bundle_writes_without_challenge(
    bundle: &CertificateBundle,
    player: &PlayerRecord,
) -> Result<Vec<AuthStoreWrite>, McpError> {
    let client_cert = parse_auth_certificate(&bundle.client_auth_cert)?;
    let code_cert = parse_auth_certificate(&bundle.code_signing_cert)?;
    Ok(vec![
        (
            auth_player_key(bundle.player_id),
            crate::redb_store::RedbStore::encode_json(player, "auth player")
                .map_err(redb_to_mcp)?,
        ),
        (
            auth_certificate_key(&bundle.cert_id),
            crate::redb_store::RedbStore::encode_json(
                &stored_certificate_from_auth(&client_cert, &bundle.client_auth_cert),
                "auth certificate",
            )
            .map_err(redb_to_mcp)?,
        ),
        (
            auth_certificate_key(&format!("{}:code", bundle.cert_id)),
            crate::redb_store::RedbStore::encode_json(
                &stored_certificate_from_auth(&code_cert, &bundle.code_signing_cert),
                "auth certificate",
            )
            .map_err(redb_to_mcp)?,
        ),
    ])
}

fn stored_certificate_from_auth(
    cert: &AuthCertificate,
    certificate_json: &str,
) -> StoredCertificate {
    StoredCertificate {
        player_id: cert.payload.player_id,
        usage: cert.payload.usage.clone(),
        fingerprint: cert.payload.public_key_fingerprint.clone(),
        issued_at: cert.payload.issued_at,
        expires_at: cert.payload.expires_at,
        revoked: false,
        certificate_json: certificate_json.to_string(),
    }
}

fn parse_auth_certificate(certificate_json: &str) -> Result<AuthCertificate, McpError> {
    serde_json::from_str(certificate_json)
        .map_err(|error| McpError::invalid_params(format!("certificate_json is invalid: {error}")))
}

fn public_key_fingerprint(public_key: &str) -> Result<String, McpError> {
    let key = decode_ed25519_public_key(public_key, "public_key")?;
    Ok(blake3::hash(key.as_bytes()).to_hex().to_string())
}

fn certificate_status(stored: &StoredCertificate, now: u64) -> &'static str {
    if stored.revoked {
        "revoked"
    } else if stored.expires_at <= now {
        "expired"
    } else {
        "active"
    }
}

fn auth_challenge_key(challenge_id: &str) -> Vec<u8> {
    format!("auth/challenges/{challenge_id}").into_bytes()
}

fn auth_player_key(player_id: PlayerId) -> Vec<u8> {
    format!("auth/players/{player_id}").into_bytes()
}

fn auth_certificate_key(certificate_id: &str) -> Vec<u8> {
    format!("auth/certificates/{certificate_id}").into_bytes()
}

fn auth_revocation_key(certificate_id: &str) -> Vec<u8> {
    format!("auth/revocations/{certificate_id}").into_bytes()
}

fn auth_store_write<T: Serialize>(
    world: &mut SwarmWorld,
    key: &[u8],
    value: &T,
    label: &str,
) -> Result<(), McpError> {
    world
        .app
        .world_mut()
        .resource_mut::<crate::redb_store::RedbStore>()
        .write_json(key, value, label)
        .map_err(redb_to_mcp)
}

fn auth_store_write_batch(
    world: &mut SwarmWorld,
    writes: Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<(), McpError> {
    world
        .app
        .world_mut()
        .resource_mut::<crate::redb_store::RedbStore>()
        .write_json_batch(writes)
        .map_err(redb_to_mcp)
}

fn auth_store_read<T: for<'de> Deserialize<'de>>(
    world: &mut SwarmWorld,
    key: &[u8],
) -> Result<Option<T>, McpError> {
    world
        .app
        .world()
        .resource::<crate::redb_store::RedbStore>()
        .read_json_value(key)
        .map_err(redb_to_mcp)
}

fn auth_store_scan<T: for<'de> Deserialize<'de>>(
    world: &mut SwarmWorld,
    prefix: &[u8],
) -> Result<Vec<(Vec<u8>, T)>, McpError> {
    world
        .app
        .world()
        .resource::<crate::redb_store::RedbStore>()
        .scan_json_prefix(prefix)
        .map_err(redb_to_mcp)
}

fn redb_to_mcp(error: crate::redb_store::RedbError) -> McpError {
    McpError::invalid_params(error.to_string())
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
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

pub(crate) fn decode_ed25519_public_key(
    input: &str,
    field: &str,
) -> Result<VerifyingKey, McpError> {
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
    if !bytes.len().is_multiple_of(4) {
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
    pub entity_changes: Vec<crate::tick::EntityChange>,
    pub message: String,
}

pub fn swarm_get_replay(
    world: &SwarmWorld,
    _context: McpContext,
    params: GetReplayParams,
) -> Result<ReplayResult, McpError> {
    use crate::tick::ReplayStore;

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

    let mut entity_changes: Vec<crate::tick::EntityChange> = Vec::new();
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
    use crate::{CommandAction, Structure, StructureType, create_world};
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

    fn install_auth_challenge(world: &mut SwarmWorld, challenge_id: &str) -> AuthChallenge {
        let challenge = AuthChallenge {
            challenge_id: challenge_id.to_string(),
            challenge: format!("challenge-{challenge_id}"),
            difficulty_bits: 0,
            expires_at: 10_000,
            consumed: false,
        };
        auth_store_write(
            world,
            &auth_challenge_key(challenge_id),
            &challenge,
            "auth challenge",
        )
        .expect("challenge should be stored");
        challenge
    }

    const REMOVED_LEGACY_AUTH_TOOLS: &[&str] = &[
        "swarm_auth_revoke",
        "swarm_auth_login",
        "swarm_auth_logout",
        "swarm_auth_refresh",
        "swarm_auth_check",
        "swarm_auth_cert_issue",
        "swarm_auth_cert_list",
        "swarm_auth_cert_revoke",
        "swarm_auth_cert_rotate",
        "swarm_auth_device_list",
        "swarm_auth_device_register",
    ];

    const EXPECTED_PUBLIC_TOOLS: &[&str] = &[
        "resources/list",
        "resources/read",
        "swarm_cert_check",
        "swarm_cert_list",
        "swarm_deploy",
        "swarm_dry_run",
        "swarm_explain_last_tick",
        "swarm_get_available_actions",
        "swarm_get_code",
        "swarm_get_controller",
        "swarm_get_deploy_status",
        "swarm_get_docs",
        "swarm_get_drone",
        "swarm_get_drone_efficiency",
        "swarm_get_economy",
        "swarm_get_economy_trend",
        "swarm_get_engine_stats",
        "swarm_get_events",
        "swarm_get_info",
        "swarm_get_messages",
        "swarm_get_path",
        "swarm_get_replay",
        "swarm_get_resources",
        "swarm_get_room",
        "swarm_get_sandbox_profile",
        "swarm_get_schema",
        "swarm_get_server_trust",
        "swarm_get_snapshot",
        "swarm_get_state_checksum",
        "swarm_get_structure",
        "swarm_get_terrain",
        "swarm_get_tick_trace",
        "swarm_get_visibility",
        "swarm_get_world_config",
        "swarm_get_world_rules",
        "swarm_list_controllers",
        "swarm_list_deployments",
        "swarm_list_drones",
        "swarm_list_errors",
        "swarm_list_modules",
        "swarm_list_rooms",
        "swarm_list_structures",
        "swarm_match_result",
        "swarm_profile",
        "swarm_register_challenge",
        "swarm_renew_certificate",
        "swarm_revoke_certificate",
        "swarm_sdk_fetch",
        "swarm_simulate",
        "swarm_submit_csr",
        "swarm_tournament_create",
        "swarm_tournament_precommit",
        "swarm_tournament_status",
        "swarm_validate_module",
    ];

    fn signed_csr_params(
        username: &str,
        challenge_id: &str,
        nonce: &str,
        key: &SigningKey,
    ) -> SubmitCsrParams {
        let csr = encode_base64(key.verifying_key().as_bytes());
        let mut message = Vec::new();
        message.extend_from_slice(csr.as_bytes());
        message.extend_from_slice(challenge_id.as_bytes());
        message.extend_from_slice(nonce.as_bytes());
        let signature = key.sign(&message);
        SubmitCsrParams {
            username: username.to_string(),
            csr,
            certificate_profile: "client_auth".to_string(),
            challenge_id: challenge_id.to_string(),
            nonce: nonce.to_string(),
            csr_signature: encode_base64(&signature.to_bytes()),
        }
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

    #[test]
    fn submit_csr_rejects_duplicate_username_before_writes() {
        let mut world = create_world();
        world
            .app
            .insert_resource(crate::redb_store::RedbStore::in_memory());
        let mut server = McpServer::with_issuer_for_tests(test_signing_key(50), 1_000);
        let first_key = test_signing_key(51);
        let second_key = test_signing_key(52);

        install_auth_challenge(&mut world, "challenge-1");
        let first = server
            .swarm_submit_csr(
                &mut world,
                signed_csr_params("Alice", "challenge-1", "nonce-1", &first_key),
            )
            .expect("first registration should succeed");
        assert_eq!(first.certificate_bundle.player_id, local_player_id("alice"));

        install_auth_challenge(&mut world, "challenge-2");
        let error = server
            .swarm_submit_csr(
                &mut world,
                signed_csr_params(" ALICE ", "challenge-2", "nonce-2", &second_key),
            )
            .expect_err("duplicate username should be rejected");
        assert_eq!(error.message, "username is already registered");

        let challenge: AuthChallenge =
            auth_store_read(&mut world, &auth_challenge_key("challenge-2"))
                .unwrap()
                .expect("second challenge should remain stored");
        assert!(!challenge.consumed);
        let player: PlayerRecord = auth_store_read(
            &mut world,
            &auth_player_key(first.certificate_bundle.player_id),
        )
        .unwrap()
        .expect("original player record should remain stored");
        assert_eq!(player.username, "alice");
        assert_eq!(
            player.public_key,
            encode_base64(first_key.verifying_key().as_bytes())
        );
    }

    #[test]
    fn cert_check_returns_trusted_stored_certificate_key_not_self_minted_key() {
        let mut world = create_world();
        world
            .app
            .insert_resource(crate::redb_store::RedbStore::in_memory());
        let issuer = test_signing_key(41);
        let trusted_key = test_signing_key(42);
        let self_minted_key = test_signing_key(43);
        let server = McpServer::with_issuer_for_tests(issuer, 1_000);

        let trusted_public_key = encode_base64(trusted_key.verifying_key().as_bytes());
        let self_minted_public_key = encode_base64(self_minted_key.verifying_key().as_bytes());
        let self_minted_fingerprint = public_key_fingerprint(&self_minted_public_key).unwrap();
        let bundle = server
            .issue_auth_bundle(7, "alice", &trusted_public_key)
            .expect("auth bundle should issue");
        let trusted_cert = parse_auth_certificate(&bundle.client_auth_cert).unwrap();
        let certificate_key = auth_certificate_key(&bundle.cert_id);
        let mut stored = stored_certificate_from_auth(&trusted_cert, &bundle.client_auth_cert);
        stored.fingerprint = self_minted_fingerprint.clone();
        auth_store_write(&mut world, &certificate_key, &stored, "auth certificate").unwrap();

        let result = server
            .swarm_cert_check(
                &mut world,
                CertCheckParams {
                    certificate_id: bundle.cert_id.clone(),
                },
            )
            .expect("cert check should use stored issuer-signed certificate");

        assert!(result.valid);
        assert_eq!(result.certificate_id, bundle.cert_id);
        assert_eq!(result.client_public_key, trusted_public_key);
        assert_eq!(result.public_key_fingerprint, bundle.public_key_fingerprint);
        assert_ne!(result.client_public_key, self_minted_public_key);
        assert_ne!(result.public_key_fingerprint, self_minted_fingerprint);

        let json = serde_json::to_value(result).unwrap();
        assert_eq!(json["certificate_id"], bundle.cert_id);
        assert_eq!(json["client_public_key"], trusted_public_key);
        assert_eq!(
            json["public_key_fingerprint"],
            bundle.public_key_fingerprint
        );
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
    fn debug_and_deploy_registry_tools_are_registered_and_sourced() {
        let tools = mcp_tool_infos()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<BTreeSet<_>>();
        for name in [
            "swarm_dry_run",
            "swarm_get_tick_trace",
            "swarm_get_engine_stats",
            "swarm_get_state_checksum",
            "swarm_get_sandbox_profile",
            "swarm_list_errors",
            "swarm_get_deploy_status",
            "swarm_list_deployments",
        ] {
            assert!(tools.contains(name), "missing ToolInfo for {name}");
            assert!(mcp_tool_source(name).is_some(), "missing source for {name}");
        }
        assert_eq!(
            mcp_tool_source("swarm_dry_run"),
            Some(CommandSource::DryRun)
        );
        assert_eq!(
            mcp_tool_source("swarm_get_deploy_status"),
            Some(CommandSource::McpQuery)
        );
        assert_eq!(
            mcp_tool_source("swarm_list_deployments"),
            Some(CommandSource::McpQuery)
        );
    }

    #[test]
    fn public_mcp_tool_inventory_matches_exact_runtime_set() {
        let tools = mcp_tool_infos();
        let tool_names = tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<BTreeSet<_>>();
        let expected = EXPECTED_PUBLIC_TOOLS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();

        assert_eq!(tools.len(), 54);
        assert_eq!(tool_names.len(), tools.len());
        assert_eq!(tool_names, expected);
        assert!(tool_names.contains("swarm_get_drone_efficiency"));
        for tool in &tools {
            assert_eq!(mcp_tool_auth_mode(&tool.name), Some(tool.auth_mode.clone()));
            assert!(mcp_tool_source(&tool.name).is_some());
        }
    }

    #[test]
    fn legacy_auth_tools_are_not_public_or_dispatchable() {
        let tools = mcp_tool_infos();
        let tool_names = tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<BTreeSet<_>>();
        let mut world = create_world();
        let mut server = McpServer::new();
        let context = McpContext {
            player_id: 1,
            tick: 0,
        };

        for tool in REMOVED_LEGACY_AUTH_TOOLS {
            assert!(!tool_names.contains(tool), "{tool} must not be advertised");
            assert_eq!(mcp_tool_source(tool), None, "{tool} source must be absent");
            assert_eq!(mcp_tool_auth_mode(tool), None, "{tool} auth must be absent");
            let error = server
                .call_tool(&mut world, context.clone(), tool, Value::Null)
                .expect_err("removed tool should not dispatch");
            assert_eq!(error.code, -32601, "{tool} should be method-not-found");
        }
    }

    #[test]
    fn leaderboard_and_admin_tools_are_not_public_or_dispatchable() {
        const REMOVED_TOOLS: &[&str] = &[
            "swarm_get_leaderboard",
            "swarm_admin_challenge",
            "swarm_admin_set_world_config",
            "swarm_admin_rollback",
            "swarm_admin_ban_player",
            "swarm_admin_force_gc",
            "swarm_admin_get_audit_log",
        ];

        let tools = mcp_tool_infos();
        assert_eq!(tools.len(), 54);
        let tool_names = tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(tool_names.len(), tools.len());
        for tool in &tools {
            assert_eq!(mcp_tool_auth_mode(&tool.name), Some(tool.auth_mode.clone()));
            assert!(mcp_tool_source(&tool.name).is_some());
        }

        let mut world = create_world();
        let mut server = McpServer::new();
        let context = McpContext {
            player_id: 1,
            tick: 0,
        };
        for tool in REMOVED_TOOLS {
            assert!(!tool_names.contains(tool), "{tool} must not be advertised");
            assert_eq!(mcp_tool_source(tool), None, "{tool} source must be absent");
            assert_eq!(mcp_tool_auth_mode(tool), None, "{tool} auth must be absent");
            let error = server
                .call_tool(&mut world, context.clone(), tool, Value::Null)
                .expect_err("removed tool should not dispatch");
            assert_eq!(error.code, -32601, "{tool} should be method-not-found");
        }
    }

    fn schema_branch_matches(schema: &Value, action: &Value) -> bool {
        let Some(action) = action.as_object() else {
            return false;
        };
        if schema.get("type").and_then(Value::as_str) != Some("object") {
            return false;
        }
        let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
            return false;
        };
        if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false)
            && action.keys().any(|key| !properties.contains_key(key))
        {
            return false;
        }
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for field in required {
                let Some(field) = field.as_str() else {
                    return false;
                };
                if !action.contains_key(field) {
                    return false;
                }
            }
        }
        properties.iter().all(|(key, property_schema)| {
            action
                .get(key)
                .is_none_or(|value| schema_value_matches(property_schema, value))
        })
    }

    fn schema_value_matches(schema: &Value, value: &Value) -> bool {
        if let Some(const_value) = schema.get("const")
            && value != const_value
        {
            return false;
        }
        if let Some(enum_values) = schema.get("enum").and_then(Value::as_array)
            && !enum_values.iter().any(|allowed| allowed == value)
        {
            return false;
        }
        if let Some(not_schema) = schema.get("not")
            && schema_value_matches(not_schema, value)
        {
            return false;
        }
        match schema.get("type").and_then(Value::as_str) {
            Some("string") => schema_string_matches(schema, value),
            Some("integer") => schema_integer_matches(schema, value),
            Some("array") => schema_array_matches(schema, value),
            Some(_) => false,
            None => true,
        }
    }

    fn schema_string_matches(schema: &Value, value: &Value) -> bool {
        let Some(value) = value.as_str() else {
            return false;
        };
        if let Some(min_length) = schema.get("minLength").and_then(Value::as_u64)
            && value.chars().count() < min_length as usize
        {
            return false;
        }
        true
    }

    fn schema_integer_matches(schema: &Value, value: &Value) -> bool {
        let Some(value) = integer_value(value) else {
            return false;
        };
        if let Some(minimum) = schema.get("minimum").and_then(integer_value)
            && value < minimum
        {
            return false;
        }
        if let Some(maximum) = schema.get("maximum").and_then(integer_value)
            && value > maximum
        {
            return false;
        }
        true
    }

    fn schema_array_matches(schema: &Value, value: &Value) -> bool {
        let Some(items) = value.as_array() else {
            return false;
        };
        if let Some(min_items) = schema.get("minItems").and_then(Value::as_u64)
            && items.len() < min_items as usize
        {
            return false;
        }
        if let Some(max_items) = schema.get("maxItems").and_then(Value::as_u64)
            && items.len() > max_items as usize
        {
            return false;
        }
        schema.get("items").is_none_or(|item_schema| {
            items
                .iter()
                .all(|item| schema_value_matches(item_schema, item))
        })
    }

    fn integer_value(value: &Value) -> Option<i128> {
        value
            .as_i64()
            .map(i128::from)
            .or_else(|| value.as_u64().map(i128::from))
    }

    fn matching_action_schema_count(action: &Value) -> usize {
        command_action_schemas()
            .iter()
            .filter(|schema| schema_branch_matches(schema, action))
            .count()
    }

    fn valid_action_shape(action_type: &str) -> Value {
        match action_type {
            "Move" => json!({"type": "Move", "object_id": 1, "direction": "Top"}),
            "Harvest" => json!({"type": "Harvest", "object_id": 1, "target_id": 2}),
            "Transfer" => {
                json!({"type": "Transfer", "object_id": 1, "target_id": 2, "resource": "Energy", "amount": 0})
            }
            "Withdraw" => {
                json!({"type": "Withdraw", "object_id": 1, "target_id": 2, "resource": "Energy", "amount": 0})
            }
            "Action" => {
                json!({"type": "Action", "action_type": "CustomAction", "object_id": 1, "target_id": 2, "payload": {"resource": "Energy"}})
            }
            "ClaimController" => json!({"type": "ClaimController", "object_id": 1, "target_id": 2}),
            "Spawn" => {
                json!({"type": "Spawn", "object_id": 1, "spawn_id": 2, "body_parts": ["Move"]})
            }
            "Recycle" => json!({"type": "Recycle", "object_id": 1}),
            "Build" => {
                json!({"type": "Build", "object_id": 1, "x": 0, "y": 0, "structure": "Tower"})
            }
            "Repair" => json!({"type": "Repair", "object_id": 1, "target_id": 2}),
            "UpgradeController" => {
                json!({"type": "UpgradeController", "object_id": 1, "target_id": 2})
            }
            "TransferToGlobal" => {
                json!({"type": "TransferToGlobal", "resource": "Energy", "amount": 0})
            }
            "TransferFromGlobal" => {
                json!({"type": "TransferFromGlobal", "resource": "Energy", "amount": 0})
            }
            "AlliedTransfer" => {
                json!({"type": "AlliedTransfer", "target_player": 2, "resource": "Energy", "amount": 0})
            }
            special if SPECIAL_COMMAND_ACTIONS.contains(&special) => {
                json!({"type": special, "object_id": 1, "target_id": 2})
            }
            _ => panic!("missing valid action shape for {action_type}"),
        }
    }

    #[test]
    fn command_action_schema_uses_canonical_spawn_and_claim_fields() {
        let schemas = command_action_schemas();
        let schema_for = |name: &str| {
            schemas
                .iter()
                .find(|schema| schema["properties"]["type"]["const"] == json!(name))
                .expect("schema must be present")
        };

        let spawn = schema_for("Spawn");
        assert_eq!(
            spawn["required"],
            json!(["type", "object_id", "spawn_id", "body_parts"])
        );
        assert!(spawn["properties"].get("body").is_none());
        assert!(spawn["properties"].get("body_parts").is_some());

        let claim = schema_for("ClaimController");
        assert_eq!(claim["required"], json!(["type", "object_id", "target_id"]));
        assert!(claim["properties"].get("controller_id").is_none());
        assert!(claim["properties"].get("target_id").is_some());
    }

    #[test]
    fn command_action_schema_accepts_dynamic_structure_type_strings() {
        let schemas = command_action_schemas();
        let build = schemas
            .iter()
            .find(|schema| schema["properties"]["type"]["const"] == json!("Build"))
            .expect("Build schema must be present");

        assert_eq!(
            build["properties"]["structure"],
            json!({"type": "string", "minLength": 1})
        );
        assert_eq!(
            matching_action_schema_count(
                &json!({"type": "Build", "object_id": 1, "x": 0, "y": 0, "structure": "CustomDepot"})
            ),
            1
        );
        assert_eq!(
            matching_action_schema_count(
                &json!({"type": "Build", "object_id": 1, "x": 0, "y": 0, "structure": ""})
            ),
            0
        );
    }

    #[test]
    fn command_action_wrapper_uses_canonical_action_type_schema() {
        let schemas = command_action_schemas();
        let action = schemas
            .iter()
            .find(|schema| schema["properties"]["type"]["const"] == json!("Action"))
            .expect("Action wrapper schema must be present");

        assert_eq!(
            action["required"],
            json!(["type", "action_type", "object_id"])
        );
        assert_eq!(
            action["properties"]["action_type"],
            json!({"type": "string", "not": {"enum": CORE_COMMAND_ACTIONS}})
        );
        assert!(action["properties"].get("action_name").is_none());
    }

    #[test]
    fn command_action_wrapper_deserializes_canonical_and_legacy_alias_only() {
        let canonical = serde_json::from_value::<CommandIntent>(json!({
            "sequence": 0,
            "action": {"type": "Action", "action_type": "CustomAction", "object_id": 1, "target_id": 2, "payload": {"amount": 1}}
        }))
        .expect("canonical action_type wrapper should deserialize");
        assert_eq!(
            canonical.action,
            CommandAction::Action {
                action_type: "CustomAction".to_string(),
                object_id: 1,
                target_id: Some(2),
                payload: json!({"amount": 1}),
            }
        );

        let legacy = serde_json::from_value::<CommandIntent>(json!({
            "sequence": 0,
            "action": {"type": "Action", "action_name": "LegacyAction", "object_id": 1}
        }))
        .expect("legacy action_name wrapper alias should deserialize");
        assert_eq!(
            legacy.action,
            CommandAction::Action {
                action_type: "LegacyAction".to_string(),
                object_id: 1,
                target_id: None,
                payload: json!({}),
            }
        );

        let serialized = serde_json::to_value(CommandIntent {
            sequence: 0,
            action: canonical.action,
        })
        .expect("Action serialization should remain flattened");
        assert_eq!(
            serialized,
            json!({"sequence": 0, "action": {"type": "CustomAction", "object_id": 1, "target_id": 2, "amount": 1}})
        );
    }

    #[test]
    fn command_action_wrapper_rejects_core_names_and_accepts_special_custom_names() {
        for rejected in ["Move", "Spawn", "Action"] {
            let action = json!({"type": "Action", "action_type": rejected, "object_id": 1});
            assert_eq!(
                matching_action_schema_count(&action),
                0,
                "literal Action wrapper must reject core action_type {rejected}"
            );
            let error =
                serde_json::from_value::<CommandIntent>(json!({"sequence": 0, "action": action}))
                    .expect_err("core action_type must not bypass core command semantics");
            assert!(
                error.to_string().contains("core command action"),
                "unexpected error for {rejected}: {error}"
            );

            serde_json::from_value::<CommandIntent>(json!({
                "sequence": 0,
                "action": {"type": "Action", "action_name": rejected, "object_id": 1}
            }))
            .expect_err("legacy action_name alias must reject core action names too");
        }

        let special =
            json!({"type": "Action", "action_type": "Hack", "object_id": 1, "target_id": 2});
        assert_eq!(matching_action_schema_count(&special), 1);
        serde_json::from_value::<CommandIntent>(json!({"sequence": 0, "action": special}))
            .expect("special action_type remains allowed through literal Action wrapper");

        let custom = json!({"type": "Action", "action_type": "CustomHarvest", "object_id": 1});
        assert_eq!(matching_action_schema_count(&custom), 1);
        serde_json::from_value::<CommandIntent>(json!({"sequence": 0, "action": custom}))
            .expect("custom action_type remains allowed through literal Action wrapper");

        serde_json::from_value::<CommandIntent>(json!({
            "sequence": 0,
            "action": {"type": "Action", "action_name": "Hack", "object_id": 1, "target_id": 2}
        }))
        .expect("legacy action_name alias remains allowed for special actions");
        serde_json::from_value::<CommandIntent>(json!({
            "sequence": 0,
            "action": {"type": "Action", "action_name": "CustomHarvest", "object_id": 1}
        }))
        .expect("legacy action_name alias remains allowed for custom actions");
    }

    #[test]
    fn command_action_wrapper_rejects_duplicate_action_aliases() {
        let error = serde_json::from_value::<CommandIntent>(json!({
            "sequence": 0,
            "action": {"type": "Action", "action_type": "Canonical", "action_name": "Legacy", "object_id": 1}
        }))
        .expect_err("supplying action_type and action_name should be rejected");

        assert!(
            error.to_string().contains("duplicate field"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn command_action_schema_coverage_tracks_action_metadata() {
        let schemas = command_action_schemas();
        let branch_names = schemas
            .iter()
            .filter_map(|schema| schema["properties"]["type"]["const"].as_str())
            .collect::<BTreeSet<_>>();
        let expected_names = CORE_COMMAND_ACTIONS
            .iter()
            .chain(SPECIAL_COMMAND_ACTIONS.iter())
            .copied()
            .collect::<BTreeSet<_>>();

        assert_eq!(branch_names, expected_names);
        assert_eq!(schemas.len(), expected_names.len() + 1);
        assert_eq!(
            schemas
                .iter()
                .filter(|schema| schema["properties"]["type"]["not"]["enum"].is_array())
                .count(),
            1
        );
    }

    #[test]
    fn public_builtin_and_special_actions_match_exactly_one_schema_branch() {
        for action_type in CORE_COMMAND_ACTIONS
            .iter()
            .chain(SPECIAL_COMMAND_ACTIONS.iter())
        {
            let action = valid_action_shape(action_type);
            assert_eq!(
                matching_action_schema_count(&action),
                1,
                "{action_type} should match exactly one oneOf branch"
            );
            serde_json::from_value::<CommandIntent>(json!({"sequence": 0, "action": action}))
                .unwrap_or_else(|error| panic!("{action_type} should deserialize: {error}"));
        }

        let special_with_payload_fields = json!({"type": "Hack", "object_id": 1, "target_id": 2, "resource": "Energy", "amount": 0, "range": 0, "structure": "Tower"});
        assert_eq!(
            matching_action_schema_count(&special_with_payload_fields),
            1
        );
    }

    #[test]
    fn canonical_action_wrapper_schema_one_of_parity_matches_deserialization() {
        let canonical = json!({"type": "Action", "action_type": "CustomAction", "object_id": 1, "target_id": 2, "payload": {"resource": "Energy"}});
        assert_eq!(matching_action_schema_count(&canonical), 1);
        serde_json::from_value::<CommandIntent>(json!({"sequence": 0, "action": canonical}))
            .expect("canonical Action wrapper should deserialize");

        let legacy = json!({"type": "Action", "action_name": "CustomAction", "object_id": 1});
        assert_eq!(matching_action_schema_count(&legacy), 0);
        serde_json::from_value::<CommandIntent>(json!({"sequence": 0, "action": legacy}))
            .expect("legacy Action alias remains deserialize-only");
    }

    #[test]
    fn malformed_reserved_builtin_actions_do_not_fall_through_to_custom_schema() {
        let malformed = [
            json!({"type": "Move", "object_id": 1}),
            json!({"type": "Action", "object_id": 1}),
            json!({"type": "ClaimController", "object_id": 1, "controller_id": 2}),
            json!({"type": "Spawn", "object_id": 1, "spawn_id": 2, "body": ["Move"]}),
            json!({"type": "Hack", "object_id": 1}),
        ];

        for action in malformed {
            assert_eq!(
                matching_action_schema_count(&action),
                0,
                "malformed reserved action should be rejected: {action}"
            );
        }

        serde_json::from_value::<CommandIntent>(
            json!({"sequence": 0, "action": {"type": "ClaimController", "object_id": 1, "controller_id": 2}}),
        )
        .expect("serde compatibility alias should remain accepted");
        serde_json::from_value::<CommandIntent>(
            json!({"sequence": 0, "action": {"type": "Spawn", "object_id": 1, "spawn_id": 2, "body": ["Move"]}}),
        )
        .expect("serde compatibility alias should remain accepted");
    }

    #[test]
    fn custom_action_fallback_accepts_flattened_payload_fields_and_excludes_reserved_names() {
        let schemas = command_action_schemas();
        let fallback = schemas
            .iter()
            .find(|schema| schema["properties"]["type"]["not"]["enum"].is_array())
            .expect("custom fallback schema should exist");

        for action_type in CORE_COMMAND_ACTIONS
            .iter()
            .chain(SPECIAL_COMMAND_ACTIONS.iter())
        {
            let action = json!({"type": action_type, "object_id": 1, "target_id": 2});
            assert!(
                !schema_branch_matches(fallback, &action),
                "{action_type} must be excluded from custom fallback"
            );
        }

        let custom = json!({"type": "CustomHarvest", "object_id": 1, "target_id": 2, "resource": "Energy", "amount": 0, "range": 0, "structure": "Tower"});
        assert!(schema_branch_matches(fallback, &custom));
        assert_eq!(matching_action_schema_count(&custom), 1);
        serde_json::from_value::<CommandIntent>(json!({"sequence": 0, "action": custom}))
            .expect("custom flattened action should deserialize");
    }

    #[test]
    fn tick_explanation_rejections_use_canonical_codes() {
        let mut world = create_world();
        let mut server = McpServer::new();
        let command = RawCommand {
            player_id: 7,
            tick: 3,
            source: CommandSource::Wasm,
            auth: CommandAuth {
                source: CommandSource::Wasm,
                player_id: 7,
                tick_submitted: 3,
                tick_target: 3,
            },
            sequence: 1,
            action: CommandAction::Transfer {
                object_id: 10,
                target_id: 11,
                resource: "Energy".to_string(),
                amount: 1,
            },
        };
        server.record_tick_trace(TickTrace {
            tick: 3,
            player_id: 7,
            commands: Vec::new(),
            state: crate::tick::WorldSnapshot::capture(world.app.world_mut()),
            rejections: vec![crate::command::CommandRejection::new(
                command.clone(),
                RejectionReason::TargetFull,
            )],
            metrics: Default::default(),
            state_checksum: 0,
            system_manifest_hash: [0; 32],
            action_manifest_hash: [0; 32],
            security_alerts: Vec::new(),
            trace_events: Vec::new(),
        });

        let explanation = server.swarm_explain_last_tick(
            &mut world,
            McpContext {
                player_id: 7,
                tick: 4,
            },
        );

        assert_eq!(explanation.rejected.len(), 1);
        let rejection = &explanation.rejected[0];
        assert_eq!(rejection.rejection, "InsufficientResource");
        assert_eq!(rejection.code, "InsufficientResource");
        assert_eq!(rejection.detail["reason"], json!("InsufficientResource"));
        assert_eq!(
            rejection.detail["canonical_reason"],
            json!("InsufficientResource")
        );
        assert_eq!(rejection.detail["internal_reason"], json!("TargetFull"));

        let daily_transfer = crate::command::CommandRejection::new(
            command,
            RejectionReason::DailyTransferCapExceeded,
        );
        assert_eq!(daily_transfer.detail["reason"], json!("RateLimited"));
        assert_eq!(
            daily_transfer.detail["canonical_reason"],
            json!("RateLimited")
        );
        assert_eq!(
            daily_transfer.detail["internal_reason"],
            json!("DailyTransferCapExceeded")
        );
    }

    #[test]
    fn debug_tools_call_through_json_rpc_dispatch() {
        let mut world = create_world();
        let mut server = McpServer::new();
        let context = McpContext {
            player_id: 1,
            tick: 9,
        };

        let stats = server
            .call_tool(
                &mut world,
                context.clone(),
                "swarm_get_engine_stats",
                Value::Null,
            )
            .unwrap();
        assert_eq!(stats["memory"]["deployed_modules"], 0);

        let checksum = server
            .call_tool(
                &mut world,
                context.clone(),
                "swarm_get_state_checksum",
                Value::Null,
            )
            .unwrap();
        assert_eq!(checksum["algorithm"], "blake3-u64");

        let errors = server
            .call_tool(
                &mut world,
                context.clone(),
                "swarm_list_errors",
                Value::Null,
            )
            .unwrap();
        assert!(errors["errors"].as_array().unwrap().is_empty());

        let profile = server
            .call_tool(
                &mut world,
                context,
                "swarm_get_sandbox_profile",
                json!({ "drone_id": 1 }),
            )
            .unwrap();
        assert_eq!(profile["fuel_used"], 0);
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
    fn swarm_get_snapshot_uses_snapshot_cache_after_redb_backfill() {
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
            .resource::<crate::hot_cache::InMemorySnapshotCache>()
            .stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.refreshes, 1);

        let second = swarm_get_snapshot(&mut world, context);
        let stats = world
            .app
            .world()
            .resource::<crate::hot_cache::InMemorySnapshotCache>()
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
        assert_eq!(result.cache_status, "remote_pending");
        assert_eq!(
            result.module_hash,
            blake3::hash(&valid_deploy_wasm()).to_hex().to_string()
        );
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
        let metadata = server
            .compile_module_for_tick(&result.module_id)
            .expect("deployed module metadata should be available at tick time");
        assert_eq!(metadata.module_hash, result.module_hash);
    }

    #[test]
    fn deploy_activates_shared_runtime_state_after_success() {
        let world = create_world();
        let issuer = test_signing_key(31);
        let client_key = test_signing_key(32);
        let active_deployments = ActiveDeployments::default();
        let mut server = McpServer {
            active_deployments: Some(active_deployments.clone()),
            ..McpServer::with_issuer_for_tests(issuer, 1_000)
        };
        let login = login_with_key(&mut server, &client_key);

        let result = server
            .swarm_deploy(
                &world,
                McpContext {
                    player_id: login.player_id,
                    tick: 21,
                },
                signed_deploy_params(login.certificate, &client_key),
            )
            .expect("deploy should succeed");

        assert!(
            active_deployments
                .active_for_player(login.player_id, 21)
                .is_none()
        );
        let deployment = active_deployments
            .active_for_player(login.player_id, 22)
            .expect("deployment should activate after successful deploy");
        assert_eq!(deployment.player_id, login.player_id);
        assert_eq!(deployment.room_id, RoomId(0));
        assert_eq!(
            blake3::Hash::from(deployment.module_hash)
                .to_hex()
                .to_string(),
            result.module_hash
        );
        assert_eq!(deployment.wasm_bytes, valid_deploy_wasm());
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
    fn curated_tournament_mode_exposes_preparation_tools_without_gameplay_tools() {
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
        assert_eq!(
            tool_names,
            vec![
                "swarm_get_snapshot",
                "swarm_get_terrain",
                "swarm_get_world_rules",
                "swarm_get_schema",
                "swarm_get_available_actions",
                "swarm_explain_last_tick",
                "swarm_get_drone",
                "swarm_profile",
                "swarm_dry_run",
                "swarm_simulate",
                "swarm_get_docs",
                "swarm_deploy",
                "swarm_validate_module",
                "swarm_tournament_precommit",
                "swarm_tournament_status",
                "swarm_tournament_create",
                "swarm_match_result",
            ]
        );
        assert!(tool_names.contains(&"swarm_deploy"));
        assert!(tool_names.contains(&"swarm_tournament_precommit"));
        assert!(tool_names.contains(&"swarm_tournament_create"));
        assert!(tool_names.contains(&"swarm_tournament_status"));
        assert!(tool_names.contains(&"swarm_match_result"));
        assert!(!tool_names.iter().any(|name| matches!(
            *name,
            "swarm_move" | "swarm_attack" | "swarm_build" | "swarm_spawn" | "swarm_harvest"
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
