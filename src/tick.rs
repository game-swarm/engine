use std::{collections::HashMap, time::Instant};

use bevy::prelude::*;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::command::{
    CommandAction, CommandIntent, CommandRejection, CommandSource, ObjectId, RawCommand,
    RefundAccumulator, Tick, WorldMutate, collect_command_intents,
    sort_raw_commands_for_active_players,
};
use crate::components::*;
use crate::mcp::{VisibleEntity, visible_entities_for_player};
use crate::realtime::{RealtimeDelta, RealtimeEnvelope};
use crate::resource_ledger::{ResourceLedger, ResourceLedgerTraceSnapshot};
use crate::resources::{
    AlliedTransferCooldowns, AlliedTransferDailyTick, AlliedTransferDailyUsage, CurrentTick,
    PendingAlliedTransfers, PendingGlobalTransfers, PlayerGlobalStorage, PlayerLocalStorage,
    ResourceCost, SettlementState,
};
use crate::sandbox_transport::{ActiveDeployment, ActiveDeployments};
use crate::scheduler::{SYSTEM_MANIFEST, manifest_hash};
use crate::security::{SecurityAlert, SecurityAuditor};
use crate::sim::{
    PerPlayerSnapshot, SnapshotActorContext, SnapshotConfig, collect_player_snapshots,
    snapshot_hash,
};
use crate::systems::{PendingCombat, PendingSpawnQueue, RoomDroneCounts};
use crate::world::{SwarmWorld, WorldConfig};

type TickTraceWrite = (Vec<u8>, Vec<u8>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TickPhase {
    Collect,
    Execute,
    Apply,
    Persist,
}

pub const MAIN_TICK_PHASES: [TickPhase; 4] = [
    TickPhase::Collect,
    TickPhase::Execute,
    TickPhase::Apply,
    TickPhase::Persist,
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct TickLoop {
    tick: Tick,
    phases: Vec<TickPhase>,
}

impl TickLoop {
    fn new(tick: Tick) -> Self {
        Self {
            tick,
            phases: Vec::with_capacity(MAIN_TICK_PHASES.len()),
        }
    }

    fn enter(&mut self, phase: TickPhase) {
        let expected = MAIN_TICK_PHASES[self.phases.len()];
        assert_eq!(phase, expected, "tick phase order violation");
        self.phases.push(phase);
    }

    fn finish(&self) {
        assert_eq!(self.phases, MAIN_TICK_PHASES);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickSnapshot {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub state_checksum: u64,
    pub perception: PerPlayerSnapshot,
    pub snapshot_hash: [u8; 32],
    pub rng_context: RngContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutorError {
    Error(String),
    Timeout,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerCollectMetrics {
    pub fuel_consumed: u64,
    pub refund_events: u64,
    pub refunded: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerCollectOutput {
    pub intents: Vec<CommandIntent>,
    pub metrics: PlayerCollectMetrics,
}

pub trait PlayerExecutor: Send {
    fn collect(&mut self, snapshot: TickSnapshot) -> Result<Vec<CommandIntent>, ExecutorError>;

    fn collect_with_metrics(
        &mut self,
        snapshot: TickSnapshot,
    ) -> Result<PlayerCollectOutput, ExecutorError> {
        self.collect(snapshot).map(|intents| PlayerCollectOutput {
            intents,
            metrics: PlayerCollectMetrics::default(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitError {
    Failed(String),
}

pub trait TickCommitter {
    fn commit(&mut self, trace: TickTrace) -> Result<(), CommitError>;

    fn commit_with_environment(
        &mut self,
        trace: TickTrace,
        _environment: ReplayEnvironment,
    ) -> Result<(), CommitError> {
        self.commit(trace)
    }
}

pub const DEFAULT_KEYFRAME_INTERVAL: Tick = 100;
pub const COLLECT_TIMEOUT_MS: u64 = 2_500;
pub const EXECUTE_TIMEOUT_MS: u64 = 500;
pub const MAX_COMMIT_ATTEMPTS: u32 = 3;
pub const DEGRADED_ABANDON_THRESHOLD: u32 = 3;
pub const DEGRADED_RECOVERY_TICKS: u32 = 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DegradedModeState {
    pub enabled: bool,
    pub join_lock: bool,
    pub mcp_deploy_enabled: bool,
    pub consecutive_abandoned_ticks: u32,
    pub consecutive_normal_ticks: u32,
}

impl Default for DegradedModeState {
    fn default() -> Self {
        Self {
            enabled: false,
            join_lock: false,
            mcp_deploy_enabled: true,
            consecutive_abandoned_ticks: 0,
            consecutive_normal_ticks: 0,
        }
    }
}

impl DegradedModeState {
    fn record_abandoned_tick(&mut self) {
        self.consecutive_abandoned_ticks += 1;
        self.consecutive_normal_ticks = 0;
        if self.consecutive_abandoned_ticks >= DEGRADED_ABANDON_THRESHOLD {
            self.enabled = true;
            self.join_lock = true;
            self.mcp_deploy_enabled = false;
        }
    }

    fn record_committed_tick(&mut self) {
        self.consecutive_abandoned_ticks = 0;
        if self.enabled {
            self.consecutive_normal_ticks += 1;
            if self.consecutive_normal_ticks >= DEGRADED_RECOVERY_TICKS {
                *self = Self::default();
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModsLock {
    pub modules: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldConfigSnapshot {
    pub config: WorldConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayEnvironment {
    pub mods_lock: ModsLock,
    pub world_config: WorldConfigSnapshot,
    pub collect_snapshot_hash: [u8; 32],
    pub rng_context: RngContext,
    pub active_players: Vec<PlayerId>,
    pub player_fuel_metrics: Vec<PlayerFuelMetric>,
    pub deploy_activation_decisions: Vec<DeployActivationDecision>,
}

impl ReplayEnvironment {
    pub fn capture(world: &World) -> Self {
        let modules = world
            .get_resource::<crate::plugins::PluginRegistry>()
            .map(|registry| {
                registry
                    .lock
                    .plugins
                    .iter()
                    .filter(|(_, entry)| entry.enabled)
                    .map(|(name, entry)| (name.clone(), entry.version.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let config = world
            .get_resource::<WorldConfig>()
            .cloned()
            .unwrap_or_default();

        Self {
            mods_lock: ModsLock { modules },
            world_config: WorldConfigSnapshot { config },
            collect_snapshot_hash: [0; 32],
            rng_context: RngContext::default(),
            active_players: Vec::new(),
            player_fuel_metrics: Vec::new(),
            deploy_activation_decisions: Vec::new(),
        }
    }

    fn with_collect_context(
        mut self,
        collect_snapshot_hash: [u8; 32],
        rng_context: RngContext,
        active_players: Vec<PlayerId>,
        player_fuel_metrics: Vec<PlayerFuelMetric>,
    ) -> Self {
        self.collect_snapshot_hash = collect_snapshot_hash;
        self.rng_context = rng_context;
        self.active_players = canonical_active_players(active_players);
        self.player_fuel_metrics = player_fuel_metrics;
        self
    }

    fn with_deploy_activation_decisions(
        mut self,
        deploy_activation_decisions: Vec<DeployActivationDecision>,
    ) -> Self {
        self.deploy_activation_decisions = deploy_activation_decisions;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BroadcastError {
    Failed(String),
}

pub trait TickBroadcaster {
    fn broadcast(&mut self, event: TickBroadcast) -> Result<(), BroadcastError>;
}

impl<B> TickBroadcaster for std::sync::Arc<B>
where
    B: TickBroadcaster + ?Sized,
{
    fn broadcast(&mut self, event: TickBroadcast) -> Result<(), BroadcastError> {
        let Some(broadcaster) = std::sync::Arc::get_mut(self) else {
            return Err(BroadcastError::Failed(
                "tick broadcaster has shared references".to_string(),
            ));
        };
        broadcaster.broadcast(event)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TickTrace {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub commands: Vec<RawCommand>,
    pub state: TickState,
    pub rejections: Vec<CommandRejection>,
    pub metrics: TickMetrics,
    pub state_checksum: u64,
    #[serde(default)]
    pub system_manifest_hash: [u8; 32],
    #[serde(default)]
    pub action_manifest_hash: [u8; 32],
    #[serde(default)]
    pub security_alerts: Vec<SecurityAlert>,
    #[serde(default)]
    pub trace_events: Vec<TickTraceEvent>,
    #[serde(default)]
    pub resource_ledger: ResourceLedgerTraceSnapshot,
}

impl TickTrace {
    pub fn accepted(&self) -> &[RawCommand] {
        &self.commands
    }
}

fn system_manifest_hash() -> [u8; 32] {
    *manifest_hash(SYSTEM_MANIFEST).as_bytes()
}

fn action_manifest_hash(world: &World) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"swarm.action-manifest.v1");
    hasher.update(&serde_json::to_vec(crate::command::CORE_COMMAND_ACTIONS).unwrap_or_default());
    hasher.update(&serde_json::to_vec(crate::command::SPECIAL_COMMAND_ACTIONS).unwrap_or_default());
    if let Some(registry) = world.get_resource::<ActionRegistry>() {
        hasher.update(&serde_json::to_vec(&registry.handlers).unwrap_or_default());
    }
    if let Some(registry) = world.get_resource::<CustomActionRegistry>() {
        let actions = registry
            .actions
            .iter()
            .collect::<std::collections::BTreeMap<_, _>>();
        hasher.update(&serde_json::to_vec(&actions).unwrap_or_default());
    }
    *hasher.finalize().as_bytes()
}

pub const CANONICAL_CODEC_VERSION: u32 = 1;
const HOST_FUEL_SCHEDULE_VERSION: &str = "swarm.host-fuel.v1";
const HOST_FUEL_SCHEDULE: &[(&str, u64, &str)] = &[
    ("host_get_terrain", 500, "none"),
    ("host_get_objects_in_range", 2_000, "+100/entity"),
    ("host_path_find", 0, "500*nodes+200*edges"),
    ("host_get_world_config", 1_000, "none"),
    ("host_get_world_rules", 1_000, "none"),
    ("host_get_random", 200, "+10/32-bytes"),
    ("host_get_fuel_remaining", 20, "none"),
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickFuelEntry {
    pub player_id: PlayerId,
    pub consumed: u64,
    pub refund_events: u64,
    pub refunded: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickFuelLedger {
    pub entries: Vec<TickFuelEntry>,
}

impl TickFuelLedger {
    fn from_environment(environment: &ReplayEnvironment) -> Self {
        let mut entries = environment
            .active_players
            .iter()
            .copied()
            .map(|player_id| {
                (
                    player_id,
                    TickFuelEntry {
                        player_id,
                        ..Default::default()
                    },
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();

        for metric in &environment.player_fuel_metrics {
            let entry = entries
                .entry(metric.player_id)
                .or_insert_with(|| TickFuelEntry {
                    player_id: metric.player_id,
                    ..Default::default()
                });
            entry.consumed = metric.consumed;
            entry.refund_events = metric.refund_events;
            entry.refunded = metric.refunded;
        }

        Self {
            entries: entries.into_values().collect(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerFuelMetric {
    pub player_id: PlayerId,
    pub consumed: u64,
    pub refund_events: u64,
    pub refunded: u64,
}

fn collect_player_fuel_metrics(
    active_players: &[PlayerId],
    collect_metrics: &indexmap::IndexMap<PlayerId, PlayerCollectMetrics>,
) -> Vec<PlayerFuelMetric> {
    let mut entries = active_players
        .iter()
        .copied()
        .map(|player_id| {
            (
                player_id,
                PlayerFuelMetric {
                    player_id,
                    ..Default::default()
                },
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    for (player_id, metrics) in collect_metrics {
        let entry = entries
            .entry(*player_id)
            .or_insert_with(|| PlayerFuelMetric {
                player_id: *player_id,
                ..Default::default()
            });
        entry.consumed = metrics.fuel_consumed;
        entry.refund_events = metrics.refund_events;
        entry.refunded = metrics.refunded;
    }
    entries.into_values().collect()
}

fn single_player_fuel_metrics(
    player_id: PlayerId,
    metrics: &PlayerCollectMetrics,
) -> Vec<PlayerFuelMetric> {
    vec![PlayerFuelMetric {
        player_id,
        consumed: metrics.fuel_consumed,
        refund_events: metrics.refund_events,
        refunded: metrics.refunded,
    }]
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployActivationDecision {
    pub schema_version: u32,
    pub deploy_id: String,
    pub player_id: PlayerId,
    pub world_id: String,
    pub module_slot: String,
    pub drone_id: ObjectId,
    pub wasm_module_hash: [u8; 32],
    pub metadata_hash: String,
    pub signed_payload_hash: String,
    pub compiled_artifact_hash: [u8; 32],
    pub client_version_counter: u64,
    pub redb_version_counter: u64,
    pub certificate_id: String,
    pub certificate_fingerprint: String,
    pub transport: String,
    pub signed_at: String,
    pub accepted_at_tick: Tick,
    pub activation_tick: Tick,
    pub status: String,
    pub archive: bool,
    pub failure: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickCommitRecord {
    pub commands: Vec<RawCommand>,
    pub rejections: Vec<CommandRejection>,
    pub fuel: TickFuelLedger,
    pub deploy_activation_decision: Vec<DeployActivationDecision>,
    pub canonical_codec_version: u32,
    pub snapshot_hash: [u8; 32],
    pub commands_hash: [u8; 32],
    pub state_checksum: u64,
    pub manifest_hash: [u8; 32],
    pub world_config_hash: [u8; 32],
}

impl TickCommitRecord {
    pub fn from_trace(trace: &TickTrace, environment: &ReplayEnvironment) -> Self {
        Self {
            commands: trace.commands.clone(),
            rejections: trace.rejections.clone(),
            fuel: TickFuelLedger::from_environment(environment),
            deploy_activation_decision: environment.deploy_activation_decisions.clone(),
            canonical_codec_version: CANONICAL_CODEC_VERSION,
            snapshot_hash: environment.collect_snapshot_hash,
            commands_hash: commands_hash(&trace.commands, &trace.rejections),
            state_checksum: trace.state_checksum,
            manifest_hash: replay_manifest_hash(trace, environment),
            world_config_hash: world_config_hash(&environment.world_config),
        }
    }
}

fn deploy_activation_decisions_for_tick(
    world: &World,
    tick: Tick,
) -> Vec<DeployActivationDecision> {
    world
        .get_resource::<ActiveDeployments>()
        .map(|deployments| deployments.pending_ready_for_tick(tick))
        .unwrap_or_default()
        .into_iter()
        .map(|deployment| DeployActivationDecision {
            schema_version: 1,
            deploy_id: deployment.deploy_id,
            player_id: deployment.player_id,
            world_id: deployment.world_id,
            module_slot: deployment.module_slot,
            drone_id: deployment.drone_id,
            wasm_module_hash: deployment.module_hash,
            metadata_hash: deployment.metadata_hash,
            signed_payload_hash: deployment.signed_payload_hash,
            compiled_artifact_hash: deployment.compiled_artifact_hash,
            client_version_counter: deployment.client_version_counter,
            redb_version_counter: deployment.redb_version_counter,
            certificate_id: deployment.certificate_id,
            certificate_fingerprint: deployment.certificate_fingerprint,
            transport: deployment.transport,
            signed_at: deployment.signed_at,
            accepted_at_tick: deployment.accepted_at_tick,
            activation_tick: deployment.load_after_tick,
            status: "active".to_string(),
            archive: false,
            failure: None,
        })
        .collect()
}

fn consume_deploy_activations_for_tick(world: &World, tick: Tick) -> Vec<ActiveDeployment> {
    world
        .get_resource::<ActiveDeployments>()
        .map(|deployments| deployments.consume_ready_for_tick(tick))
        .unwrap_or_default()
}

pub fn commands_hash(commands: &[RawCommand], rejections: &[CommandRejection]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&serde_json::to_vec(commands).unwrap_or_default());
    hasher.update(&serde_json::to_vec(rejections).unwrap_or_default());
    *hasher.finalize().as_bytes()
}

fn canonical_active_players(mut active_players: Vec<PlayerId>) -> Vec<PlayerId> {
    active_players.sort_unstable();
    active_players.dedup();
    active_players
}

pub fn world_config_hash(world_config: &WorldConfigSnapshot) -> [u8; 32] {
    *blake3::hash(&serde_json::to_vec(world_config).unwrap_or_default()).as_bytes()
}

fn replay_manifest_hash(trace: &TickTrace, environment: &ReplayEnvironment) -> [u8; 32] {
    replay_manifest_hash_with_fuel_schedule(
        trace,
        environment,
        HOST_FUEL_SCHEDULE_VERSION,
        HOST_FUEL_SCHEDULE,
    )
}

fn replay_manifest_hash_with_fuel_schedule(
    trace: &TickTrace,
    environment: &ReplayEnvironment,
    fuel_schedule_version: &str,
    fuel_schedule: &[(&str, u64, &str)],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"swarm.replay-manifest.v1");
    hasher.update(&trace.system_manifest_hash);
    hasher.update(&trace.action_manifest_hash);
    hasher.update(&serde_json::to_vec(&environment.mods_lock).unwrap_or_default());
    hasher.update(fuel_schedule_version.as_bytes());
    hasher.update(&serde_json::to_vec(fuel_schedule).unwrap_or_default());
    hasher.update(&world_config_hash(&environment.world_config));
    *hasher.finalize().as_bytes()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RngContext {
    pub world_seed: u64,
    pub seed_epoch: u64,
    pub tick: Tick,
    pub namespace: String,
    pub seed: [u8; 32],
}

impl Default for RngContext {
    fn default() -> Self {
        Self::derive(0, 0, 0, "default")
    }
}

impl RngContext {
    pub fn derive(world_seed: u64, seed_epoch: u64, tick: Tick, namespace: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(namespace.as_bytes());
        hasher.update(&world_seed.to_le_bytes());
        hasher.update(&seed_epoch.to_le_bytes());
        hasher.update(&tick.to_le_bytes());
        Self {
            world_seed,
            seed_epoch,
            tick,
            namespace: namespace.to_string(),
            seed: *hasher.finalize().as_bytes(),
        }
    }
}

#[derive(Resource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickTraceEventLog {
    pub events: Vec<TickTraceEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickTraceEvent {
    pub system: String,
    pub entity: u64,
    pub event: String,
    pub amount: u32,
    pub resource: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickBroadcast {
    pub tick: Tick,
    pub last_tick: Tick,
    pub player_id: PlayerId,
    pub full_snapshot: bool,
    pub accepted: Vec<RawCommand>,
    pub rejections: Vec<CommandRejection>,
    pub changed_entities: Vec<VisibleEntity>,
    pub removed_entities: Vec<ObjectId>,
    pub state_checksum: u64,
}

impl TickBroadcast {
    pub fn realtime_delta(&self) -> RealtimeDelta {
        RealtimeDelta {
            tick: self.tick,
            last_tick: self.last_tick,
            player_id: self.player_id,
            full_snapshot: self.full_snapshot,
            changed_entities: self.changed_entities.clone(),
            removed_entities: self.removed_entities.clone(),
            state_checksum: self.state_checksum,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickMetrics {
    pub executor_errors: u64,
    pub executor_timeouts: u64,
    pub accepted_commands: u64,
    pub rejected_commands: u64,
    pub commit_failures: u64,
    pub broadcast_failures: u64,
    pub total_commands: u64,
    pub fuel_consumed: u64,
    pub refund_events: u64,
    pub refund_fuel: u64,
    pub duration_ms: u64,
    pub collect_duration_ms: u64,
    pub execute_duration_ms: u64,
    pub collect_timeouts: u64,
    pub execute_timeouts: u64,
}

impl TickMetrics {
    pub fn record_execution(&mut self, accepted_count: usize, rejections: &[CommandRejection]) {
        self.accepted_commands += accepted_count as u64;
        self.rejected_commands += rejections.len() as u64;
        let total_commands = (accepted_count + rejections.len()) as u64;
        self.total_commands += total_commands;
        for rejection in rejections {
            if let Some(refund_fuel) = rejection
                .detail
                .get("refund_fuel")
                .and_then(serde_json::Value::as_u64)
                && refund_fuel > 0
            {
                self.refund_events += 1;
                self.refund_fuel += refund_fuel;
            }
        }
    }

    pub fn command_rejection_rate(&self) -> f64 {
        ratio(self.rejected_commands, self.total_commands)
    }

    pub fn refund_abuse_rate(&self) -> f64 {
        ratio(self.refund_events, self.total_commands)
    }
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

/// Caches COLLECT results for cross-retry reuse.
///
/// When redb commit fails, the tick is retried. Without caching, WASM COLLECT
/// would be invoked again, leading to double fuel charges and non-deterministic
/// side effects. This cache stores the raw commands and fuel metrics from the
/// first COLLECT call so subsequent retries reuse the same output.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CollectCache {
    tick: Tick,
    state_checksum: u64,
    by_player: indexmap::IndexMap<PlayerId, Vec<RawCommand>>,
    fuel_metrics: TickMetrics,
    collect_snapshot_hash: [u8; 32],
    rng_context: RngContext,
    active_players: Vec<PlayerId>,
    player_fuel_metrics: Vec<PlayerFuelMetric>,
}

impl CollectCache {
    fn raw_commands(&self, world_seed: u64) -> Vec<RawCommand> {
        serial_execution_queue_for_active_players(
            self.by_player
                .iter()
                .map(|(&player_id, commands)| CollectedPlayerCommands {
                    player_id,
                    commands: commands.clone(),
                })
                .collect(),
            world_seed,
            &self.active_players,
        )
    }

    fn matches(&self, tick: Tick, state_checksum: u64) -> bool {
        self.tick == tick && self.state_checksum == state_checksum
    }
}

fn snapshot_config_for_world(world: &World) -> SnapshotConfig {
    let fog_of_war = world
        .get_resource::<WorldConfig>()
        .map(|config| config.visibility.fog_of_war)
        .unwrap_or(true);
    SnapshotConfig {
        fog_of_war,
        ..Default::default()
    }
}

fn rng_context_for_world(world: &World, tick: Tick, namespace: &str) -> RngContext {
    let world_seed = world_seed_for_world(world);
    let seed_epoch = world
        .get_resource::<crate::systems::SeedRotationState>()
        .map(|state| state.next_rotation_at)
        .unwrap_or_default();
    RngContext::derive(world_seed, seed_epoch, tick, namespace)
}

fn world_seed_for_world(world: &World) -> u64 {
    world
        .get_resource::<WorldConfig>()
        .map(|config| config.world.world_seed)
        .unwrap_or_default()
}

fn empty_player_snapshot(player_id: PlayerId, tick: Tick) -> PerPlayerSnapshot {
    PerPlayerSnapshot {
        tick,
        player_id,
        actor_context: SnapshotActorContext {
            active_drones: Vec::new(),
            primary_drone: String::new(),
        },
        truncated: false,
        degraded: false,
        over_budget: false,
        omitted_categories: crate::sim::OmittedCategories::all_zero(),
        entities: Vec::new(),
        resources: Vec::new(),
        events: Vec::new(),
    }
}

fn collect_inputs_for_players(
    world: &mut SwarmWorld,
    player_ids: &[PlayerId],
    tick: Tick,
    state_checksum: u64,
) -> (HashMap<PlayerId, TickSnapshot>, [u8; 32], RngContext) {
    let snapshot_config = snapshot_config_for_world(world.app.world());
    let rng_context = rng_context_for_world(world.app.world(), tick, "collect");
    let player_snapshots =
        collect_player_snapshots(world.app.world_mut(), player_ids, tick, &snapshot_config)
            .into_iter()
            .map(|snapshot| (snapshot.player_id, snapshot))
            .collect::<HashMap<_, _>>();

    let mut inputs = HashMap::new();
    let mut ordered_player_ids = player_ids.to_vec();
    ordered_player_ids.sort_unstable();
    ordered_player_ids.dedup();

    let mut collect_hasher = blake3::Hasher::new();
    for player_id in ordered_player_ids {
        let perception = player_snapshots
            .get(&player_id)
            .cloned()
            .unwrap_or_else(|| empty_player_snapshot(player_id, tick));
        let hash = snapshot_hash(&perception);
        collect_hasher.update(&player_id.to_le_bytes());
        collect_hasher.update(&hash);
        inputs.insert(
            player_id,
            TickSnapshot {
                tick,
                player_id,
                state_checksum,
                perception,
                snapshot_hash: hash,
                rng_context: rng_context.clone(),
            },
        );
    }

    (inputs, *collect_hasher.finalize().as_bytes(), rng_context)
}

impl TickMetrics {
    /// Add another TickMetrics into self (used to aggregate per-player metrics).
    pub fn add(&mut self, other: &TickMetrics) {
        self.executor_errors += other.executor_errors;
        self.executor_timeouts += other.executor_timeouts;
        self.accepted_commands += other.accepted_commands;
        self.rejected_commands += other.rejected_commands;
        self.commit_failures += other.commit_failures;
        self.broadcast_failures += other.broadcast_failures;
        self.total_commands += other.total_commands;
        self.fuel_consumed = self.fuel_consumed.saturating_add(other.fuel_consumed);
        self.refund_events += other.refund_events;
        self.refund_fuel += other.refund_fuel;
        self.duration_ms += other.duration_ms;
        self.collect_duration_ms += other.collect_duration_ms;
        self.execute_duration_ms += other.execute_duration_ms;
        self.collect_timeouts += other.collect_timeouts;
        self.execute_timeouts += other.execute_timeouts;
    }

    /// Subtract execution metrics (used when replaying from cache to avoid
    /// double-counting fuel when the cache's COLLECT already charged).
    pub fn subtract_execution(&mut self, other: &TickMetrics) {
        self.accepted_commands = self
            .accepted_commands
            .saturating_sub(other.accepted_commands);
        self.rejected_commands = self
            .rejected_commands
            .saturating_sub(other.rejected_commands);
        self.total_commands = self.total_commands.saturating_sub(other.total_commands);
        self.fuel_consumed = self.fuel_consumed.saturating_sub(other.fuel_consumed);
        self.refund_events = self.refund_events.saturating_sub(other.refund_events);
        self.refund_fuel = self.refund_fuel.saturating_sub(other.refund_fuel);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClickHouseTickMetricsRow {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub collect_timeout_rate: f64,
    pub tick_abandon_rate: f64,
    pub refund_abuse_rate: f64,
    pub command_rejection_rate: f64,
    pub tick_duration_p99: u64,
    pub accepted_commands: u64,
    pub rejected_commands: u64,
    pub refund_events: u64,
    pub refund_fuel: u64,
}

impl ClickHouseTickMetricsRow {
    pub fn from_trace(trace: &TickTrace, recent_durations_ms: &[u64]) -> Self {
        let mut durations = recent_durations_ms.to_vec();
        if durations.is_empty() && trace.metrics.duration_ms > 0 {
            durations.push(trace.metrics.duration_ms);
        }
        Self {
            tick: trace.tick,
            player_id: trace.player_id,
            collect_timeout_rate: trace.metrics.executor_timeouts.min(1) as f64,
            tick_abandon_rate: trace.metrics.commit_failures.min(1) as f64,
            refund_abuse_rate: trace.metrics.refund_abuse_rate(),
            command_rejection_rate: trace.metrics.command_rejection_rate(),
            tick_duration_p99: percentile_nearest_rank(&mut durations, 99),
            accepted_commands: trace.metrics.accepted_commands,
            rejected_commands: trace.metrics.rejected_commands,
            refund_events: trace.metrics.refund_events,
            refund_fuel: trace.metrics.refund_fuel,
        }
    }

    pub fn insert_sql_values(&self) -> String {
        format!(
            "({}, {}, {:.6}, {:.6}, {:.6}, {:.6}, {}, {}, {}, {}, {})",
            self.tick,
            self.player_id,
            self.collect_timeout_rate,
            self.tick_abandon_rate,
            self.refund_abuse_rate,
            self.command_rejection_rate,
            self.tick_duration_p99,
            self.accepted_commands,
            self.rejected_commands,
            self.refund_events,
            self.refund_fuel
        )
    }
}

fn percentile_nearest_rank(values: &mut [u64], percentile: u64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let rank = ((percentile as usize * values.len()).div_ceil(100)).max(1);
    values[rank.saturating_sub(1).min(values.len() - 1)]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickMetricsWriteError {
    Failed(String),
}

pub trait TickMetricsWriter {
    fn write_tick_metrics(
        &mut self,
        row: ClickHouseTickMetricsRow,
    ) -> Result<(), TickMetricsWriteError>;
}

pub const CLICKHOUSE_TICK_METRICS_INSERT: &str = "INSERT INTO tick_metrics (tick, player_id, collect_timeout_rate, tick_abandon_rate, refund_abuse_rate, command_rejection_rate, tick_duration_p99, accepted_commands, rejected_commands, refund_events, refund_fuel) VALUES";

#[derive(Debug, Clone, Default, PartialEq)]
struct StrategyMetricsAccumulator {
    tick_count: u64,
    executor_timeouts: u64,
    total_commands: u64,
    rejected_commands: u64,
    fuel_consumed: u64,
    first_tick: Option<Tick>,
    last_tick: Option<Tick>,
    resource_start: Option<u64>,
    resource_end: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StrategyMetricsDashboardRow {
    pub player_id: PlayerId,
    pub tick_start: Tick,
    pub tick_end: Tick,
    pub tick_count: u64,
    pub fuel_consumed: u64,
    pub timeout_rate: f64,
    pub command_rejection_rate: f64,
    pub resource_start: u64,
    pub resource_end: u64,
    pub resource_growth_rate: f64,
}

impl StrategyMetricsDashboardRow {
    pub fn insert_sql_values(&self) -> String {
        format!(
            "({}, {}, {}, {}, {}, {:.6}, {:.6}, {}, {}, {:.6})",
            self.player_id,
            self.tick_start,
            self.tick_end,
            self.tick_count,
            self.fuel_consumed,
            self.timeout_rate,
            self.command_rejection_rate,
            self.resource_start,
            self.resource_end,
            self.resource_growth_rate
        )
    }
}

pub const CLICKHOUSE_STRATEGY_METRICS_INSERT: &str = "INSERT INTO strategy_metrics_dashboard (player_id, tick_start, tick_end, tick_count, fuel_consumed, timeout_rate, command_rejection_rate, resource_start, resource_end, resource_growth_rate) VALUES";

/// Aggregates P0-6 strategy dashboard metrics by player over an inclusive tick range.
pub fn aggregate_strategy_metrics_dashboard(
    traces: &[TickTrace],
    tick_start: Tick,
    tick_end: Tick,
) -> Vec<StrategyMetricsDashboardRow> {
    if tick_start > tick_end {
        return Vec::new();
    }

    let mut accumulators = HashMap::<PlayerId, StrategyMetricsAccumulator>::new();
    let mut ordered_traces = traces
        .iter()
        .filter(|trace| trace.tick >= tick_start && trace.tick <= tick_end)
        .collect::<Vec<_>>();
    ordered_traces.sort_by_key(|trace| trace.tick);

    for trace in ordered_traces {
        if trace.player_id != 0 {
            let accumulator = accumulators.entry(trace.player_id).or_default();
            accumulator.tick_count += 1;
            accumulator.executor_timeouts += trace.metrics.executor_timeouts.min(1);
            accumulator.total_commands += trace.metrics.total_commands;
            accumulator.rejected_commands += trace.metrics.rejected_commands;
            accumulator.fuel_consumed += trace.metrics.fuel_consumed;
            accumulator.first_tick.get_or_insert(trace.tick);
            accumulator.last_tick = Some(trace.tick);
        } else {
            aggregate_multiplayer_trace_commands(trace, &mut accumulators);
        }

        for (player_id, resources) in trace.state.player_resource_totals() {
            if player_id == 0 {
                continue;
            }
            let accumulator = accumulators.entry(player_id).or_default();
            if accumulator
                .first_tick
                .is_none_or(|first_tick| trace.tick <= first_tick)
            {
                accumulator.first_tick = Some(trace.tick);
                accumulator.resource_start = Some(resources);
            }
            if accumulator
                .last_tick
                .is_none_or(|last_tick| trace.tick >= last_tick)
            {
                accumulator.last_tick = Some(trace.tick);
                accumulator.resource_end = Some(resources);
            }
        }
    }

    let mut rows = accumulators
        .into_iter()
        .filter_map(|(player_id, accumulator)| {
            let first_tick = accumulator.first_tick?;
            let last_tick = accumulator.last_tick.unwrap_or(first_tick);
            let resource_start = accumulator.resource_start.unwrap_or_default();
            let resource_end = accumulator.resource_end.unwrap_or(resource_start);
            let tick_span = last_tick.saturating_sub(first_tick).max(1);
            Some(StrategyMetricsDashboardRow {
                player_id,
                tick_start: first_tick,
                tick_end: last_tick,
                tick_count: accumulator.tick_count,
                fuel_consumed: accumulator.fuel_consumed,
                timeout_rate: ratio(accumulator.executor_timeouts, accumulator.tick_count),
                command_rejection_rate: ratio(
                    accumulator.rejected_commands,
                    accumulator.total_commands,
                ),
                resource_start,
                resource_end,
                resource_growth_rate: (resource_end as f64 - resource_start as f64)
                    / tick_span as f64,
            })
        })
        .collect::<Vec<_>>();
    rows.sort_by_key(|row| row.player_id);
    rows
}

fn aggregate_multiplayer_trace_commands(
    trace: &TickTrace,
    accumulators: &mut HashMap<PlayerId, StrategyMetricsAccumulator>,
) {
    let mut per_player_commands = HashMap::<PlayerId, (u64, u64)>::new();
    for command in &trace.commands {
        per_player_commands.entry(command.player_id).or_default().0 += 1;
    }
    for rejection in &trace.rejections {
        let counts = per_player_commands
            .entry(rejection.command.player_id)
            .or_default();
        counts.0 += 1;
        counts.1 += 1;
    }

    for (player_id, (total_commands, rejected_commands)) in per_player_commands {
        let accumulator = accumulators.entry(player_id).or_default();
        accumulator.tick_count += 1;
        accumulator.total_commands += total_commands;
        accumulator.rejected_commands += rejected_commands;
        accumulator.first_tick.get_or_insert(trace.tick);
        accumulator.last_tick = Some(trace.tick);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    MissingPreviousState {
        tick: Tick,
    },
    MissingKeyframe {
        tick: Tick,
    },
    MissingDelta {
        tick: Tick,
    },
    StateMismatch {
        tick: Tick,
        expected_checksum: u64,
        actual_checksum: u64,
    },
    ResourceLedgerMismatch {
        tick: Tick,
        expected_checksum: u64,
        actual_checksum: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TickReport {
    pub tick: Tick,
    pub committed: bool,
    pub broadcasted: bool,
    pub accepted: Vec<RawCommand>,
    pub rejections: Vec<CommandRejection>,
    pub metrics: TickMetrics,
    pub security_alerts: Vec<SecurityAlert>,
    pub messages: Vec<DroneMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroneMessage {
    pub sender_id: ObjectId,
    pub recipient_id: ObjectId,
    pub payload: Vec<u8>,
    pub sequence: u32,
}

#[derive(Resource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroneMessageOutbox(pub Vec<DroneMessage>);

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
    pub commands: Vec<RawCommand>,
}

#[derive(Resource, Debug, Clone, Default)]
pub struct ReplayStore {
    pub keyframes: std::collections::BTreeMap<Tick, KeyframeData>,
    pub deltas: std::collections::BTreeMap<Tick, TickDelta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyframeData {
    pub tick: Tick,
    pub world_snapshot: Vec<u8>,
    pub mods_lock: ModsLock,
    pub world_config: WorldConfigSnapshot,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TickDelta {
    pub tick: Tick,
    pub commands_json: String,
    pub entity_changes: Vec<EntityChange>,
}

impl ReplayStore {
    pub fn nearest_keyframe(&self, tick: Tick) -> Option<(Tick, &KeyframeData)> {
        self.keyframes
            .range(..=tick)
            .next_back()
            .map(|(tick, keyframe)| (*tick, keyframe))
    }

    pub fn deltas_in_range(&self, from_tick: Tick, to_tick: Tick) -> Vec<&TickDelta> {
        self.deltas
            .range((from_tick + 1)..=to_tick)
            .map(|(_, delta)| delta)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CollectedPlayerCommands {
    player_id: PlayerId,
    commands: Vec<RawCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerCollectResult {
    pub player_id: PlayerId,
    pub commands: Vec<RawCommand>,
    pub metrics: TickMetrics,
    pub fuel_metrics: PlayerCollectMetrics,
}

pub fn seeded_player_shuffle(
    mut players: Vec<PlayerId>,
    tick: Tick,
    state_checksum: u64,
) -> Vec<PlayerId> {
    players.sort_unstable();
    let context = RngContext::derive(state_checksum, 0, tick, "player_shuffle");
    seeded_player_shuffle_with_context(players, &context)
}

pub fn seeded_player_shuffle_with_context(
    mut players: Vec<PlayerId>,
    context: &RngContext,
) -> Vec<PlayerId> {
    players.sort_unstable();
    let mut reader = blake3::Hasher::new_derive_key("swarm-engine-player-shuffle");
    reader.update(&context.seed);
    let mut reader = reader.finalize_xof();

    for i in 0..players.len() {
        let remaining = players.len() - i;
        let offset = unbiased_xof_index(&mut reader, remaining);
        players.swap(i, i + offset);
    }

    players
}

fn unbiased_xof_index(reader: &mut blake3::OutputReader, remaining: usize) -> usize {
    let bound = remaining as u64;
    let zone = u64::MAX - (u64::MAX % bound);
    loop {
        let mut bytes = [0_u8; 8];
        reader.fill(&mut bytes);
        let value = u64::from_le_bytes(bytes);
        if value < zone {
            return (value % bound) as usize;
        }
    }
}

fn collect_player_commands<E: PlayerExecutor + ?Sized>(
    tick: Tick,
    player_id: PlayerId,
    state_checksum: u64,
    collect_inputs: &HashMap<PlayerId, TickSnapshot>,
    executor: &mut E,
) -> PlayerCollectResult {
    let snapshot = collect_inputs
        .get(&player_id)
        .cloned()
        .unwrap_or_else(|| TickSnapshot {
            tick,
            player_id,
            state_checksum,
            perception: empty_player_snapshot(player_id, tick),
            snapshot_hash: snapshot_hash(&empty_player_snapshot(player_id, tick)),
            rng_context: RngContext::default(),
        });
    let mut metrics = TickMetrics::default();
    let mut fuel_metrics = PlayerCollectMetrics::default();
    let intents = match executor.collect_with_metrics(snapshot) {
        Ok(output) => {
            fuel_metrics = output.metrics;
            metrics.fuel_consumed = fuel_metrics.fuel_consumed;
            metrics.refund_events = fuel_metrics.refund_events;
            metrics.refund_fuel = fuel_metrics.refunded;
            output.intents
        }
        Err(ExecutorError::Timeout) => {
            metrics.executor_timeouts += 1;
            Vec::new()
        }
        Err(ExecutorError::Error(_)) => {
            metrics.executor_errors += 1;
            Vec::new()
        }
    };
    let commands =
        collect_command_intents(player_id, tick, CommandSource::Wasm, intents).unwrap_or_default();

    PlayerCollectResult {
        player_id,
        commands,
        metrics,
        fuel_metrics,
    }
}

fn serial_execution_queue_for_active_players(
    collected: Vec<CollectedPlayerCommands>,
    world_seed: u64,
    active_players: &[PlayerId],
) -> Vec<RawCommand> {
    let mut queue = Vec::new();
    for collected in collected {
        queue.extend(collected.commands);
    }
    sort_raw_commands_for_active_players(&mut queue, world_seed, active_players);
    queue
}

fn collect_player_results(
    executors: &mut HashMap<PlayerId, Box<dyn PlayerExecutor>>,
    tick: Tick,
    state_checksum: u64,
    collect_inputs: &HashMap<PlayerId, TickSnapshot>,
) -> Vec<PlayerCollectResult> {
    if executors.len() <= 1 {
        return executors
            .iter_mut()
            .map(|(&player_id, executor)| {
                collect_player_commands(
                    tick,
                    player_id,
                    state_checksum,
                    collect_inputs,
                    executor.as_mut(),
                )
            })
            .collect();
    }

    executors
        .par_iter_mut()
        .map(|(&player_id, executor)| {
            collect_player_commands(
                tick,
                player_id,
                state_checksum,
                collect_inputs,
                executor.as_mut(),
            )
        })
        .collect()
}

pub struct MultiPlayerTickScheduler<C, B> {
    pub world: SwarmWorld,
    pub executors: HashMap<PlayerId, Box<dyn PlayerExecutor>>,
    pub committer: C,
    pub broadcaster: B,
    pub tick_counter: Tick,
    pub metrics: TickMetrics,
    collect_cache: Option<CollectCache>,
    pub degraded_mode: DegradedModeState,
}

impl<C, B> MultiPlayerTickScheduler<C, B>
where
    C: TickCommitter,
    B: TickBroadcaster,
{
    pub fn new(
        world: SwarmWorld,
        executors: HashMap<PlayerId, Box<dyn PlayerExecutor>>,
        committer: C,
        broadcaster: B,
    ) -> Self {
        let tick_counter = world.tick_head();
        Self {
            world,
            executors,
            committer,
            broadcaster,
            tick_counter,
            metrics: TickMetrics::default(),
            collect_cache: None,
            degraded_mode: DegradedModeState::default(),
        }
    }

    pub fn tick(&mut self) -> TickReport {
        let tick = self.tick_counter;
        let mut tick_loop = TickLoop::new(tick);
        let started_at = Instant::now();
        let state_checksum = self.world.state_checksum();
        let world_seed = world_seed_for_world(self.world.app.world());
        let active_players = canonical_active_players(self.executors.keys().copied().collect());

        // S1: Check COLLECT cache before executing WASM.
        // If redb commit failed last tick and we're retrying with the same state,
        // reuse cached commands to avoid double fuel charges.
        let cache_hit = self
            .collect_cache
            .as_ref()
            .filter(|cache| cache.matches(tick, state_checksum));
        let cache_context = cache_hit.map(|cache| {
            (
                cache.collect_snapshot_hash,
                cache.rng_context.clone(),
                cache.raw_commands(world_seed),
                cache.player_fuel_metrics.clone(),
            )
        });

        let collect_started_at = Instant::now();
        tick_loop.enter(TickPhase::Collect);
        let player_ids = self.executors.keys().copied().collect::<Vec<_>>();
        let (collect_inputs, collect_snapshot_hash, rng_context) =
            collect_inputs_for_players(&mut self.world, &player_ids, tick, state_checksum);
        let (raw_commands, player_fuel_metrics) = if let Some((_, _, commands, fuel_metrics)) =
            &cache_context
        {
            (commands.clone(), fuel_metrics.clone())
        } else {
            let mut results =
                collect_player_results(&mut self.executors, tick, state_checksum, &collect_inputs);
            results.sort_by_key(|result| result.player_id);

            for result in &results {
                self.metrics.add(&result.metrics);
            }

            // Build CollectCache for potential retry
            let mut by_player: indexmap::IndexMap<PlayerId, Vec<RawCommand>> =
                indexmap::IndexMap::new();
            let mut collect_fuel_metrics = TickMetrics::default();
            let player_fuel_metrics = collect_player_fuel_metrics(
                &active_players,
                &results
                    .iter()
                    .map(|result| (result.player_id, result.fuel_metrics.clone()))
                    .collect::<indexmap::IndexMap<_, _>>(),
            );
            for result in &results {
                by_player.insert(result.player_id, result.commands.clone());
                collect_fuel_metrics.add(&result.metrics);
            }
            self.collect_cache = Some(CollectCache {
                tick,
                state_checksum,
                by_player,
                fuel_metrics: collect_fuel_metrics,
                collect_snapshot_hash,
                rng_context: rng_context.clone(),
                active_players: active_players.clone(),
                player_fuel_metrics: player_fuel_metrics.clone(),
            });

            let collected = results
                .into_iter()
                .map(|result| CollectedPlayerCommands {
                    player_id: result.player_id,
                    commands: result.commands,
                })
                .collect::<Vec<_>>();
            (
                serial_execution_queue_for_active_players(collected, world_seed, &active_players),
                player_fuel_metrics,
            )
        };
        let collect_duration_ms = collect_started_at.elapsed().as_millis() as u64;
        self.metrics.collect_duration_ms = collect_duration_ms;
        if collect_duration_ms > COLLECT_TIMEOUT_MS {
            self.metrics.collect_timeouts += 1;
        }

        let world_snapshot = WorldSnapshot::capture(self.world.app.world_mut());
        let mut last_accepted = Vec::new();
        let mut last_rejections = Vec::new();
        let mut last_security_alerts = Vec::new();
        let mut committed_trace = None;
        for attempt in 0..MAX_COMMIT_ATTEMPTS {
            let entity_map = if attempt > 0 {
                world_snapshot.clone().restore(self.world.app.world_mut())
            } else {
                EntityRemap::default()
            };
            let attempt_commands = remap_commands(raw_commands.clone(), &entity_map);
            let execute_started_at = Instant::now();
            let execution =
                execute_deterministic_with_loop(&mut self.world, attempt_commands, &mut tick_loop);
            let execute_duration_ms = execute_started_at.elapsed().as_millis() as u64;
            self.metrics.execute_duration_ms = execute_duration_ms;
            if execute_duration_ms > EXECUTE_TIMEOUT_MS {
                self.metrics.execute_timeouts += 1;
                last_accepted = remap_commands(execution.commands, &entity_map.inverse());
                last_rejections = execution.rejections;
                world_snapshot.clone().restore(self.world.app.world_mut());
                break;
            }

            let accepted = remap_commands(execution.commands, &entity_map.inverse());
            let rejections = execution.rejections;
            let metrics_before_execution = self.metrics.clone();
            self.metrics.record_execution(accepted.len(), &rejections);

            let checksum = self.world.state_checksum();
            let state =
                TickState::capture(self.world.app.world_mut()).remap_keys(&entity_map.inverse());
            let mut trace = TickTrace {
                tick,
                player_id: 0,
                commands: accepted.clone(),
                state,
                rejections: rejections.clone(),
                metrics: self.metrics.clone(),
                state_checksum: checksum,
                system_manifest_hash: system_manifest_hash(),
                action_manifest_hash: action_manifest_hash(self.world.app.world()),
                security_alerts: Vec::new(),
                trace_events: std::mem::take(
                    &mut self
                        .world
                        .app
                        .world_mut()
                        .resource_mut::<TickTraceEventLog>()
                        .events,
                ),
                resource_ledger: execution.resource_ledger.clone(),
            };
            trace.security_alerts = SecurityAuditor::default().audit_trace(&trace, None);
            let (environment_snapshot_hash, environment_rng_context) = cache_context
                .as_ref()
                .map(|(snapshot_hash, rng_context, _, _)| (*snapshot_hash, rng_context.clone()))
                .unwrap_or((collect_snapshot_hash, rng_context.clone()));
            let environment_fuel_metrics = cache_context
                .as_ref()
                .map(|(_, _, _, fuel_metrics)| fuel_metrics.clone())
                .unwrap_or_else(|| player_fuel_metrics.clone());
            let deploy_activation_decisions =
                deploy_activation_decisions_for_tick(self.world.app.world(), tick);
            let environment = ReplayEnvironment::capture(self.world.app.world())
                .with_collect_context(
                    environment_snapshot_hash,
                    environment_rng_context,
                    active_players.clone(),
                    environment_fuel_metrics,
                )
                .with_deploy_activation_decisions(deploy_activation_decisions);
            last_accepted = accepted;
            last_rejections = rejections;
            last_security_alerts = trace.security_alerts.clone();
            tick_loop.enter(TickPhase::Persist);
            let commit_result = validate_resource_ledger_for_commit(&trace).and_then(|_| {
                self.committer
                    .commit_with_environment(trace.clone(), environment)
            });
            if commit_result.is_ok() {
                consume_deploy_activations_for_tick(self.world.app.world(), tick);
                committed_trace = Some(trace);
                break;
            }
            self.metrics = metrics_before_execution;
            self.metrics.commit_failures += 1;
            world_snapshot.clone().restore(self.world.app.world_mut());
            self.world.app.world_mut().resource_mut::<CurrentTick>().0 = tick;
            tick_loop = TickLoop::new(tick);
            tick_loop.enter(TickPhase::Collect);
        }

        let Some(trace) = committed_trace else {
            self.degraded_mode.record_abandoned_tick();
            return TickReport {
                tick,
                committed: false,
                broadcasted: false,
                accepted: last_accepted,
                rejections: last_rejections,
                metrics: self.metrics.clone(),
                security_alerts: last_security_alerts,
                messages: Vec::new(),
            };
        };

        self.tick_counter += 1;
        tick_loop.finish();
        self.collect_cache = None;
        self.degraded_mode.record_committed_tick();
        let mut broadcasted = true;
        for player_id in active_players {
            let changed_entities =
                visible_entities_for_player(self.world.app.world_mut(), player_id);
            let broadcast = TickBroadcast {
                tick,
                last_tick: tick.saturating_sub(1),
                player_id,
                full_snapshot: true,
                accepted: trace.commands.clone(),
                rejections: trace.rejections.clone(),
                changed_entities,
                removed_entities: Vec::new(),
                state_checksum: trace.state_checksum,
            };
            if self.broadcaster.broadcast(broadcast).is_err() {
                self.metrics.broadcast_failures += 1;
                broadcasted = false;
            }
        }

        self.metrics.duration_ms = started_at.elapsed().as_millis() as u64;

        TickReport {
            tick,
            committed: true,
            broadcasted,
            accepted: trace.commands,
            rejections: trace.rejections,
            metrics: self.metrics.clone(),
            security_alerts: trace.security_alerts,
            messages: Vec::new(),
        }
    }
}

pub struct TickScheduler<E, C, B> {
    pub world: SwarmWorld,
    pub player_id: PlayerId,
    pub executor: E,
    pub committer: C,
    pub broadcaster: B,
    pub tick_counter: Tick,
    pub metrics: TickMetrics,
    pub degraded_mode: DegradedModeState,
}

impl<E, C, B> TickScheduler<E, C, B>
where
    E: PlayerExecutor,
    C: TickCommitter,
    B: TickBroadcaster,
{
    pub fn new(
        world: SwarmWorld,
        player_id: PlayerId,
        executor: E,
        committer: C,
        broadcaster: B,
    ) -> Self {
        let tick_counter = world.tick_head();
        Self {
            world,
            player_id,
            executor,
            committer,
            broadcaster,
            tick_counter,
            metrics: TickMetrics::default(),
            degraded_mode: DegradedModeState::default(),
        }
    }

    pub fn tick(&mut self) -> TickReport {
        let tick = self.tick_counter;
        let mut tick_loop = TickLoop::new(tick);
        let started_at = Instant::now();
        let state_checksum = self.world.state_checksum();
        let world_seed = world_seed_for_world(self.world.app.world());
        let active_players = vec![self.player_id];
        let (collect_inputs, collect_snapshot_hash, rng_context) =
            collect_inputs_for_players(&mut self.world, &[self.player_id], tick, state_checksum);
        let snapshot = collect_inputs
            .get(&self.player_id)
            .cloned()
            .unwrap_or_else(|| TickSnapshot {
                tick,
                player_id: self.player_id,
                state_checksum,
                perception: empty_player_snapshot(self.player_id, tick),
                snapshot_hash: snapshot_hash(&empty_player_snapshot(self.player_id, tick)),
                rng_context: rng_context.clone(),
            });
        let collect_started_at = Instant::now();
        tick_loop.enter(TickPhase::Collect);
        let mut player_collect_metrics = PlayerCollectMetrics::default();
        let intents = match self.executor.collect_with_metrics(snapshot) {
            Ok(output) => {
                player_collect_metrics = output.metrics;
                output.intents
            }
            Err(ExecutorError::Timeout) => {
                self.metrics.executor_timeouts += 1;
                Vec::new()
            }
            Err(ExecutorError::Error(_)) => {
                self.metrics.executor_errors += 1;
                Vec::new()
            }
        };
        let collect_duration_ms = collect_started_at.elapsed().as_millis() as u64;
        self.metrics.collect_duration_ms = collect_duration_ms;
        self.metrics.fuel_consumed = self
            .metrics
            .fuel_consumed
            .saturating_add(player_collect_metrics.fuel_consumed);
        self.metrics.refund_events += player_collect_metrics.refund_events;
        self.metrics.refund_fuel = self
            .metrics
            .refund_fuel
            .saturating_add(player_collect_metrics.refunded);
        if collect_duration_ms > COLLECT_TIMEOUT_MS {
            self.metrics.collect_timeouts += 1;
        }

        let world_snapshot = WorldSnapshot::capture(self.world.app.world_mut());
        let mut raw_commands =
            collect_command_intents(self.player_id, tick, CommandSource::Wasm, intents)
                .unwrap_or_default();
        sort_raw_commands_for_active_players(&mut raw_commands, world_seed, &active_players);

        let mut last_accepted = Vec::new();
        let mut last_rejections = Vec::new();
        let mut last_security_alerts = Vec::new();
        let mut committed_trace = None;
        for attempt in 0..MAX_COMMIT_ATTEMPTS {
            let entity_map = if attempt > 0 {
                world_snapshot.clone().restore(self.world.app.world_mut())
            } else {
                EntityRemap::default()
            };
            let attempt_commands = remap_commands(raw_commands.clone(), &entity_map);
            let execute_started_at = Instant::now();
            let execution =
                execute_deterministic_with_loop(&mut self.world, attempt_commands, &mut tick_loop);
            let execute_duration_ms = execute_started_at.elapsed().as_millis() as u64;
            self.metrics.execute_duration_ms = execute_duration_ms;
            if execute_duration_ms > EXECUTE_TIMEOUT_MS {
                self.metrics.execute_timeouts += 1;
                last_accepted = remap_commands(execution.commands, &entity_map.inverse());
                last_rejections = execution.rejections;
                world_snapshot.clone().restore(self.world.app.world_mut());
                break;
            }
            let accepted = remap_commands(execution.commands, &entity_map.inverse());
            let rejections = execution.rejections;
            let metrics_before_execution = self.metrics.clone();
            self.metrics.record_execution(accepted.len(), &rejections);

            let checksum = self.world.state_checksum();
            let state =
                TickState::capture(self.world.app.world_mut()).remap_keys(&entity_map.inverse());
            let mut trace = TickTrace {
                tick,
                player_id: self.player_id,
                commands: accepted.clone(),
                state,
                rejections: rejections.clone(),
                metrics: self.metrics.clone(),
                state_checksum: checksum,
                system_manifest_hash: system_manifest_hash(),
                action_manifest_hash: action_manifest_hash(self.world.app.world()),
                security_alerts: Vec::new(),
                trace_events: std::mem::take(
                    &mut self
                        .world
                        .app
                        .world_mut()
                        .resource_mut::<TickTraceEventLog>()
                        .events,
                ),
                resource_ledger: execution.resource_ledger.clone(),
            };
            trace.security_alerts = SecurityAuditor::default().audit_trace(&trace, None);
            let deploy_activation_decisions =
                deploy_activation_decisions_for_tick(self.world.app.world(), tick);
            let environment = ReplayEnvironment::capture(self.world.app.world())
                .with_collect_context(
                    collect_snapshot_hash,
                    rng_context.clone(),
                    active_players.clone(),
                    single_player_fuel_metrics(self.player_id, &player_collect_metrics),
                )
                .with_deploy_activation_decisions(deploy_activation_decisions);
            last_accepted = accepted;
            last_rejections = rejections;
            last_security_alerts = trace.security_alerts.clone();
            tick_loop.enter(TickPhase::Persist);
            let commit_result = validate_resource_ledger_for_commit(&trace).and_then(|_| {
                self.committer
                    .commit_with_environment(trace.clone(), environment)
            });
            if commit_result.is_ok() {
                consume_deploy_activations_for_tick(self.world.app.world(), tick);
                committed_trace = Some(trace);
                break;
            }
            self.metrics = metrics_before_execution;
            self.metrics.commit_failures += 1;
            world_snapshot.clone().restore(self.world.app.world_mut());
            self.world.app.world_mut().resource_mut::<CurrentTick>().0 = tick;
            tick_loop = TickLoop::new(tick);
            tick_loop.enter(TickPhase::Collect);
        }

        let Some(trace) = committed_trace else {
            self.degraded_mode.record_abandoned_tick();
            return TickReport {
                tick,
                committed: false,
                broadcasted: false,
                accepted: last_accepted,
                rejections: last_rejections,
                metrics: self.metrics.clone(),
                security_alerts: last_security_alerts,
                messages: Vec::new(),
            };
        };

        self.tick_counter += 1;
        tick_loop.finish();
        self.degraded_mode.record_committed_tick();
        let changed_entities =
            visible_entities_for_player(self.world.app.world_mut(), self.player_id);
        let broadcast = TickBroadcast {
            tick,
            last_tick: tick.saturating_sub(1),
            player_id: self.player_id,
            full_snapshot: true,
            accepted: trace.commands.clone(),
            rejections: trace.rejections.clone(),
            changed_entities,
            removed_entities: Vec::new(),
            state_checksum: trace.state_checksum,
        };
        let broadcasted = if self.broadcaster.broadcast(broadcast).is_ok() {
            true
        } else {
            self.metrics.broadcast_failures += 1;
            false
        };

        self.metrics.duration_ms = started_at.elapsed().as_millis() as u64;

        TickReport {
            tick,
            committed: true,
            broadcasted,
            accepted: trace.commands,
            rejections: trace.rejections,
            metrics: self.metrics.clone(),
            security_alerts: trace.security_alerts,
            messages: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryTickCommitter {
    pub records: Vec<TickTrace>,
    pub fail_next: bool,
    pub fail_count: u32,
}

impl TickCommitter for InMemoryTickCommitter {
    fn commit(&mut self, trace: TickTrace) -> Result<(), CommitError> {
        validate_resource_ledger_for_commit(&trace)?;
        if self.fail_next {
            self.fail_next = false;
            return Err(CommitError::Failed("in-memory commit failed".to_string()));
        }
        if self.fail_count > 0 {
            self.fail_count -= 1;
            return Err(CommitError::Failed("in-memory commit failed".to_string()));
        }
        if self.records.iter().any(|record| record.tick == trace.tick) {
            return Err(CommitError::Failed("duplicate tick commit".to_string()));
        }

        self.records.push(trace);
        Ok(())
    }
}

pub trait AtomicTickStore {
    fn atomic_commit(&mut self, writes: Vec<TickTraceWrite>) -> Result<(), CommitError>;
}

#[derive(Debug, Clone)]
pub struct RedbTickCommitter<S> {
    store: S,
}

impl<S> RedbTickCommitter<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    pub fn into_inner(self) -> S {
        self.store
    }
}

impl<S> TickCommitter for RedbTickCommitter<S>
where
    S: AtomicTickStore,
{
    fn commit(&mut self, trace: TickTrace) -> Result<(), CommitError> {
        let environment = ReplayEnvironment {
            mods_lock: ModsLock::default(),
            world_config: WorldConfigSnapshot {
                config: WorldConfig::default(),
            },
            collect_snapshot_hash: [0; 32],
            rng_context: RngContext::default(),
            active_players: Vec::new(),
            player_fuel_metrics: Vec::new(),
            deploy_activation_decisions: Vec::new(),
        };
        self.commit_with_environment(trace, environment)
    }

    fn commit_with_environment(
        &mut self,
        trace: TickTrace,
        environment: ReplayEnvironment,
    ) -> Result<(), CommitError> {
        self.store
            .atomic_commit(tick_trace_writes_with_environment(&trace, &environment)?)
    }
}

fn validate_resource_ledger_for_commit(trace: &TickTrace) -> Result<(), CommitError> {
    trace
        .resource_ledger
        .validate_for_commit()
        .map_err(|error| CommitError::Failed(format!("resource ledger validation failed: {error}")))
}

pub fn tick_trace_writes(trace: &TickTrace) -> Result<Vec<TickTraceWrite>, CommitError> {
    let environment = ReplayEnvironment {
        mods_lock: ModsLock::default(),
        world_config: WorldConfigSnapshot {
            config: WorldConfig::default(),
        },
        collect_snapshot_hash: [0; 32],
        rng_context: RngContext::default(),
        active_players: Vec::new(),
        player_fuel_metrics: Vec::new(),
        deploy_activation_decisions: Vec::new(),
    };
    tick_trace_writes_with_environment(trace, &environment)
}

pub fn tick_trace_writes_with_environment(
    trace: &TickTrace,
    environment: &ReplayEnvironment,
) -> Result<Vec<TickTraceWrite>, CommitError> {
    validate_resource_ledger_for_commit(trace)?;

    fn encode<T: Serialize>(value: &T, label: &str) -> Result<Vec<u8>, CommitError> {
        serde_json::to_vec(value)
            .map_err(|error| CommitError::Failed(format!("encode {label}: {error}")))
    }

    let mut writes = vec![
        (
            tick_key(trace.tick, "commit_record"),
            encode(
                &TickCommitRecord::from_trace(trace, environment),
                "tick commit record",
            )?,
        ),
        (
            tick_key(trace.tick, "state"),
            encode(&trace.state, "tick state")?,
        ),
        (
            tick_key(trace.tick, "commands"),
            encode(&trace.commands, "tick commands")?,
        ),
        (
            tick_key(trace.tick, "rejections"),
            encode(&trace.rejections, "tick rejections")?,
        ),
        (
            tick_key(trace.tick, "metrics"),
            encode(&trace.metrics, "tick metrics")?,
        ),
        (
            tick_key(trace.tick, "resource_ledger"),
            encode(&trace.resource_ledger, "resource ledger")?,
        ),
        (
            tick_key(trace.tick, "security_alerts"),
            encode(&trace.security_alerts, "tick security alerts")?,
        ),
    ];

    if trace.tick == 0 || trace.tick.is_multiple_of(DEFAULT_KEYFRAME_INTERVAL) {
        writes.push((
            tick_key(trace.tick, "keyframe"),
            encode(&trace.state, "tick keyframe")?,
        ));
        writes.push((
            tick_key(trace.tick, "mods_lock"),
            encode(&environment.mods_lock, "mods lock")?,
        ));
        writes.push((
            tick_key(trace.tick, "world_config"),
            encode(&environment.world_config, "world config")?,
        ));
    } else {
        let delta = WorldDelta {
            from_tick: trace.tick.saturating_sub(1),
            to_tick: trace.tick,
            entity_changes: Vec::new(),
            commands: trace.commands.clone(),
        };
        writes.push((tick_key(trace.tick, "delta"), encode(&delta, "tick delta")?));
    }

    Ok(writes)
}

pub fn tick_key(tick: Tick, suffix: &str) -> Vec<u8> {
    format!("/tick/{tick}/{suffix}").into_bytes()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeterministicExecution {
    pub commands: Vec<RawCommand>,
    pub rejections: Vec<CommandRejection>,
    pub next_tick_fuel_credit: u64,
    pub state: TickState,
    pub state_checksum: u64,
    pub resource_ledger: ResourceLedgerTraceSnapshot,
}

const COMMAND_REJECTION_FUEL_COST: u64 = 10_000;

#[cfg(test)]
fn execute_deterministic(
    world: &mut SwarmWorld,
    commands: Vec<RawCommand>,
) -> DeterministicExecution {
    let tick = commands
        .first()
        .map(|command| command.tick)
        .unwrap_or_else(|| world.tick_head());
    let mut tick_loop = TickLoop::new(tick);
    tick_loop.enter(TickPhase::Collect);

    // P0-6: Build per-drone snapshots during COLLECT phase
    let fog_of_war = world
        .app
        .world()
        .resource::<WorldConfig>()
        .visibility
        .fog_of_war;
    let snapshot_config = SnapshotConfig {
        fog_of_war,
        ..Default::default()
    };
    // Collect player IDs from commands
    let player_ids: Vec<PlayerId> = commands
        .iter()
        .map(|c| c.player_id)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let _snapshots =
        collect_player_snapshots(world.app.world_mut(), &player_ids, tick, &snapshot_config);

    execute_deterministic_with_loop(world, commands, &mut tick_loop)
}

fn execute_deterministic_with_loop(
    world: &mut SwarmWorld,
    commands: Vec<RawCommand>,
    tick_loop: &mut TickLoop,
) -> DeterministicExecution {
    tick_loop.enter(TickPhase::Execute);
    let mut accepted = Vec::new();
    let mut rejections = Vec::new();
    let mut refunds = RefundAccumulator::default();
    for raw in commands {
        let current_tick = world.tick_head();
        world.app.world_mut().resource_mut::<CurrentTick>().0 = current_tick.max(raw.tick);
        match raw.clone().validate_and_apply(world.app.world_mut()) {
            Ok(()) => accepted.push(raw),
            Err(rejection) => {
                let refund_fuel =
                    refunds.record_rejection(&raw, &rejection, COMMAND_REJECTION_FUEL_COST);
                rejections.push(command_rejection_with_refund(raw, rejection, refund_fuel));
            }
        }
    }

    tick_loop.enter(TickPhase::Apply);
    world.run_tick_for(tick_loop.tick);
    let state_checksum = world.state_checksum();
    let state = TickState::capture(world.app.world_mut());
    let resource_ledger = world
        .app
        .world()
        .resource::<ResourceLedger>()
        .trace_snapshot();
    DeterministicExecution {
        commands: accepted,
        rejections,
        next_tick_fuel_credit: refunds.next_tick_fuel_credit,
        state,
        state_checksum,
        resource_ledger,
    }
}

fn command_rejection_with_refund(
    raw: RawCommand,
    rejection: crate::command::RejectionReason,
    refund_fuel: u64,
) -> CommandRejection {
    let mut command_rejection = CommandRejection::new(raw, rejection);
    if let Some(detail) = command_rejection.detail.as_object_mut() {
        detail.insert("refund_fuel".to_string(), serde_json::json!(refund_fuel));
    }
    command_rejection
}

pub(crate) fn replay_tick(
    previous_state: &TickState,
    trace: &TickTrace,
) -> Result<TickState, ReplayError> {
    let mut world = crate::world::create_world();
    let entity_map = previous_state.clone().restore(world.app.world_mut());
    let replay_commands = remap_commands(trace.commands.clone(), &entity_map);
    let mut tick_loop = TickLoop::new(trace.tick);
    tick_loop.enter(TickPhase::Collect);
    let mut replayed = execute_deterministic_with_loop(&mut world, replay_commands, &mut tick_loop);
    replayed.state = replayed.state.remap_keys(&entity_map.inverse());
    if replayed.state != trace.state {
        return Err(ReplayError::StateMismatch {
            tick: trace.tick,
            expected_checksum: trace.state_checksum,
            actual_checksum: replayed.state_checksum,
        });
    }
    if trace.resource_ledger.validate_for_replay().is_err()
        || replayed.resource_ledger.validate_for_commit().is_err()
        || !trace
            .resource_ledger
            .replay_equivalent(&replayed.resource_ledger)
    {
        return Err(ReplayError::ResourceLedgerMismatch {
            tick: trace.tick,
            expected_checksum: trace.resource_ledger.ledger_checksum,
            actual_checksum: replayed.resource_ledger.ledger_checksum,
        });
    }

    Ok(replayed.state)
}

#[cfg(test)]
fn replay(initial_state: &TickState, traces: &[TickTrace]) -> Result<TickState, ReplayError> {
    let mut state = initial_state.clone();
    for trace in traces {
        state = replay_tick(&state, trace)?;
    }

    Ok(state)
}

pub fn replay_visible_entities(
    trace: &TickTrace,
    player_id: PlayerId,
) -> Vec<crate::mcp::VisibleEntity> {
    let mut world = crate::world::create_world();
    let entity_map = trace.state.clone().restore(world.app.world_mut());
    crate::mcp::visible_entities_for_player(world.app.world_mut(), player_id)
        .into_iter()
        .map(|entity| remap_visible_entity(entity, &entity_map.inverse()))
        .collect()
}

fn remap_commands(mut commands: Vec<RawCommand>, entity_map: &EntityRemap) -> Vec<RawCommand> {
    for command in &mut commands {
        remap_command_action(&mut command.action, entity_map);
    }
    commands
}

fn remap_command_action(action: &mut CommandAction, entity_map: &EntityRemap) {
    match action {
        CommandAction::Move { object_id, .. } | CommandAction::Build { object_id, .. } => {
            remap_object_id(object_id, entity_map)
        }
        CommandAction::Harvest {
            object_id,
            target_id,
            ..
        }
        | CommandAction::Transfer {
            object_id,
            target_id,
            ..
        }
        | CommandAction::Withdraw {
            object_id,
            target_id,
            ..
        } => {
            remap_object_id(object_id, entity_map);
            remap_object_id(target_id, entity_map);
        }
        CommandAction::ClaimController {
            object_id,
            target_id: controller_id,
        }
        | CommandAction::Repair {
            object_id,
            target_id: controller_id,
        }
        | CommandAction::UpgradeController {
            object_id,
            target_id: controller_id,
        }
        | CommandAction::Attack {
            object_id,
            target_id: controller_id,
            ..
        }
        | CommandAction::RangedAttack {
            object_id,
            target_id: controller_id,
            ..
        }
        | CommandAction::Heal {
            object_id,
            target_id: controller_id,
            ..
        }
        | CommandAction::Hack {
            object_id,
            target_id: controller_id,
            ..
        }
        | CommandAction::Drain {
            object_id,
            target_id: controller_id,
            ..
        }
        | CommandAction::Overload {
            object_id,
            target_id: controller_id,
            ..
        }
        | CommandAction::Debilitate {
            object_id,
            target_id: controller_id,
            ..
        }
        | CommandAction::Disrupt {
            object_id,
            target_id: controller_id,
            ..
        }
        | CommandAction::Fortify {
            object_id,
            target_id: controller_id,
            ..
        }
        | CommandAction::Leech {
            object_id,
            target_id: controller_id,
            ..
        }
        | CommandAction::Fabricate {
            object_id,
            target_id: controller_id,
            ..
        } => {
            remap_object_id(object_id, entity_map);
            remap_object_id(controller_id, entity_map);
        }
        CommandAction::Spawn {
            object_id,
            spawn_id,
            ..
        } => {
            remap_object_id(object_id, entity_map);
            remap_object_id(spawn_id, entity_map);
        }
        CommandAction::Recycle { object_id } => remap_object_id(object_id, entity_map),
        CommandAction::Action {
            object_id,
            target_id,
            ..
        } => {
            remap_object_id(object_id, entity_map);
            if let Some(target_id) = target_id {
                remap_object_id(target_id, entity_map);
            }
        }
        CommandAction::TransferToGlobal { .. }
        | CommandAction::TransferFromGlobal { .. }
        | CommandAction::AlliedTransfer { .. }
        | CommandAction::CreateContractSettlement { .. }
        | CommandAction::SettleContract { .. }
        | CommandAction::CancelContract { .. }
        | CommandAction::CreateMerchantQuote { .. }
        | CommandAction::AcceptMerchantTrade { .. }
        | CommandAction::CreateP2POffer { .. }
        | CommandAction::AcceptP2POffer { .. }
        | CommandAction::CancelP2POffer { .. }
        | CommandAction::RefundP2POffer { .. }
        | CommandAction::CreateAuction { .. }
        | CommandAction::BidAuction { .. }
        | CommandAction::SettleAuction { .. }
        | CommandAction::CancelAuction { .. }
        | CommandAction::CreateEscrow { .. }
        | CommandAction::ReleaseEscrow { .. }
        | CommandAction::RefundEscrow { .. }
        | CommandAction::CreateLoanOffer { .. }
        | CommandAction::AcceptLoan { .. }
        | CommandAction::RepayLoan { .. }
        | CommandAction::DefaultLoan { .. } => {}
    }
}

fn remap_object_id(object_id: &mut ObjectId, entity_map: &EntityRemap) {
    if let Some(entity) = entity_map.get(SnapshotEntity(*object_id)) {
        *object_id = entity.0;
    }
}

fn remap_visible_entity(
    entity: crate::mcp::VisibleEntity,
    entity_map: &EntityRemap,
) -> crate::mcp::VisibleEntity {
    use crate::mcp::VisibleEntity;

    match entity {
        VisibleEntity::Drone(mut drone) => {
            remap_object_id(&mut drone.id, entity_map);
            VisibleEntity::Drone(drone)
        }
        VisibleEntity::Structure(mut structure) => {
            remap_object_id(&mut structure.id, entity_map);
            VisibleEntity::Structure(structure)
        }
        VisibleEntity::Source(mut source) => {
            remap_object_id(&mut source.id, entity_map);
            VisibleEntity::Source(source)
        }
        VisibleEntity::Resource(mut resource) => {
            remap_object_id(&mut resource.id, entity_map);
            VisibleEntity::Resource(resource)
        }
        VisibleEntity::Controller(mut controller) => {
            remap_object_id(&mut controller.id, entity_map);
            VisibleEntity::Controller(controller)
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryTickBroadcaster {
    pub broadcasts: Vec<TickBroadcast>,
    pub fail_next: bool,
}

impl TickBroadcaster for InMemoryTickBroadcaster {
    fn broadcast(&mut self, event: TickBroadcast) -> Result<(), BroadcastError> {
        if self.fail_next {
            self.fail_next = false;
            return Err(BroadcastError::Failed(
                "in-memory broadcast failed".to_string(),
            ));
        }

        self.broadcasts.push(event);
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct NatsTickBroadcaster {
    client: async_nats::Client,
    subject: String,
}

impl NatsTickBroadcaster {
    pub fn new(client: async_nats::Client, subject: impl Into<String>) -> Self {
        Self {
            client,
            subject: subject.into(),
        }
    }
}

impl TickBroadcaster for NatsTickBroadcaster {
    fn broadcast(&mut self, event: TickBroadcast) -> Result<(), BroadcastError> {
        let payload = serde_json::to_vec(&RealtimeEnvelope {
            schema: "swarm.realtime.v1".to_string(),
            payload: event.realtime_delta(),
        })
        .map_err(|error| BroadcastError::Failed(error.to_string()))?;
        let client = self.client.clone();
        let subject = self.subject.clone();
        tokio::runtime::Runtime::new()
            .map_err(|error| BroadcastError::Failed(error.to_string()))?
            .block_on(async move {
                client
                    .publish(subject, payload.into())
                    .await
                    .map_err(|error| BroadcastError::Failed(error.to_string()))
            })
    }
}

pub type TickState = WorldSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SnapshotEntity(pub u64);

impl From<Entity> for SnapshotEntity {
    fn from(entity: Entity) -> Self {
        Self(entity.to_bits())
    }
}

impl From<SnapshotEntity> for Entity {
    fn from(entity: SnapshotEntity) -> Self {
        Entity::from_bits(entity.0)
    }
}

#[derive(Debug, Clone, Default)]
pub struct EntityRemap(HashMap<SnapshotEntity, SnapshotEntity>);

impl EntityRemap {
    fn insert(&mut self, old: SnapshotEntity, new: SnapshotEntity) {
        self.0.insert(old, new);
    }

    fn get(&self, entity: SnapshotEntity) -> Option<SnapshotEntity> {
        self.0.get(&entity).copied()
    }

    fn inverse(&self) -> Self {
        Self(self.0.iter().map(|(old, new)| (*new, *old)).collect())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldSnapshot {
    entities: HashMap<SnapshotEntity, EntitySnapshot>,
    terrains: RoomTerrains,
    pending_spawns: PendingSpawnQueue,
    room_counts: RoomDroneCounts,
    pending_combat: PendingCombat,
    local_storage: PlayerLocalStorage,
    global_storage: PlayerGlobalStorage,
    pending_global_transfers: PendingGlobalTransfers,
    pending_allied_transfers: PendingAlliedTransfers,
    allied_transfer_cooldowns: AlliedTransferCooldowns,
    allied_transfer_daily_usage: AlliedTransferDailyUsage,
    allied_transfer_daily_tick: AlliedTransferDailyTick,
    #[serde(default)]
    settlement_state: SettlementState,
    #[serde(default)]
    resource_ledger: ResourceLedger,
    starting_resources_granted: crate::systems::StartingResourcesGranted,
    player_first_spawn_tick: crate::systems::PlayerFirstSpawnTick,
    /// Per-tick event log for feedback loop replay fidelity.
    event_log: EventLog,
    /// Entity allocator state for deterministic rollback verification.
    pub entity_total_count: u32,
    pub entity_alive_count: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntitySnapshot {
    position: Option<Position>,
    owner: Option<Owner>,
    drone: Option<Drone>,
    structure: Option<Structure>,
    resource: Option<crate::components::Resource>,
    source: Option<Source>,
    terrain: Option<Terrain>,
    controller: Option<Controller>,
    marked_for_death: bool,
    spawning_grace: Option<SpawningGrace>,
    attributes: Option<Attributes>,
    entity_flags: Option<EntityFlags>,
    drone_env: Option<DroneEnv>,
    code_version: Option<CodeVersion>,
    projectile: Option<crate::systems::Projectile>,
    hack_state: Option<HackState>,
    drain_state: Option<DrainState>,
    overload_state: Option<OverloadState>,
    debilitate_state: Option<DebilitateState>,
    disrupt_state: Option<DisruptState>,
    fortify_state: Option<FortifyState>,
    leech_state: Option<LeechState>,
    fabricate_state: Option<FabricateState>,
    hack_buffer: Option<HackBuffer>,
    drain_buffer: Option<DrainBuffer>,
    overload_buffer: Option<OverloadBuffer>,
    debilitate_buffer: Option<DebilitateBuffer>,
    disrupt_buffer: Option<DisruptBuffer>,
    fortify_buffer: Option<FortifyBuffer>,
    leech_buffer: Option<LeechBuffer>,
    fabricate_buffer: Option<FabricateBuffer>,
}

impl WorldSnapshot {
    pub fn entities(&self) -> &HashMap<SnapshotEntity, EntitySnapshot> {
        &self.entities
    }

    pub fn delta_to(
        &self,
        after: &WorldSnapshot,
        from_tick: Tick,
        to_tick: Tick,
        commands: Vec<RawCommand>,
    ) -> WorldDelta {
        let mut entity_changes = Vec::new();
        let mut ids = self
            .entities
            .keys()
            .chain(after.entities.keys())
            .copied()
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();

        for id in ids {
            match (self.entities.get(&id), after.entities.get(&id)) {
                (None, Some(snapshot)) => entity_changes.push(EntityChange::Created {
                    entity_id: id.0,
                    component_data: serialize_entity_snapshot(snapshot),
                }),
                (Some(_), None) => entity_changes.push(EntityChange::Removed { entity_id: id.0 }),
                (Some(before), Some(after)) if before != after => {
                    entity_changes.push(EntityChange::Modified {
                        entity_id: id.0,
                        component_data: serialize_entity_snapshot(after),
                    });
                }
                _ => {}
            }
        }

        WorldDelta {
            from_tick,
            to_tick,
            entity_changes,
            commands,
        }
    }

    fn player_resource_totals(&self) -> HashMap<PlayerId, u64> {
        let mut totals = HashMap::<PlayerId, u64>::new();
        add_player_resource_totals(&mut totals, &self.local_storage.0);
        add_player_resource_totals(&mut totals, &self.global_storage.0);

        for snapshot in self.entities.values() {
            if let Some(drone) = &snapshot.drone {
                *totals.entry(drone.owner).or_default() += resource_total(&drone.carry);
            }
            if let Some(structure) = &snapshot.structure
                && let (Some(owner), Some(energy)) = (structure.owner, structure.energy)
            {
                *totals.entry(owner).or_default() += energy as u64;
            }
        }

        totals
    }

    pub fn capture(world: &mut World) -> Self {
        let entity_ids: Vec<Entity> = world.query::<Entity>().iter(world).collect();
        let entities: HashMap<SnapshotEntity, EntitySnapshot> = entity_ids
            .into_iter()
            .filter_map(|entity| {
                let snapshot = EntitySnapshot {
                    position: world.entity(entity).get::<Position>().copied(),
                    owner: world.entity(entity).get::<Owner>().copied(),
                    drone: world.entity(entity).get::<Drone>().cloned(),
                    structure: world.entity(entity).get::<Structure>().cloned(),
                    resource: world
                        .entity(entity)
                        .get::<crate::components::Resource>()
                        .cloned(),
                    source: world.entity(entity).get::<Source>().cloned(),
                    terrain: world.entity(entity).get::<Terrain>().copied(),
                    controller: world.entity(entity).get::<Controller>().cloned(),
                    marked_for_death: world.entity(entity).get::<DeathMark>().is_some(),
                    spawning_grace: world.entity(entity).get::<SpawningGrace>().copied(),
                    attributes: world.entity(entity).get::<Attributes>().cloned(),
                    entity_flags: world.entity(entity).get::<EntityFlags>().cloned(),
                    drone_env: world.entity(entity).get::<DroneEnv>().cloned(),
                    code_version: world.entity(entity).get::<CodeVersion>().copied(),
                    projectile: world
                        .entity(entity)
                        .get::<crate::systems::Projectile>()
                        .cloned(),
                    hack_state: world.entity(entity).get::<HackState>().cloned(),
                    drain_state: world.entity(entity).get::<DrainState>().cloned(),
                    overload_state: world.entity(entity).get::<OverloadState>().cloned(),
                    debilitate_state: world.entity(entity).get::<DebilitateState>().cloned(),
                    disrupt_state: world.entity(entity).get::<DisruptState>().cloned(),
                    fortify_state: world.entity(entity).get::<FortifyState>().cloned(),
                    leech_state: world.entity(entity).get::<LeechState>().cloned(),
                    fabricate_state: world.entity(entity).get::<FabricateState>().cloned(),
                    hack_buffer: world.entity(entity).get::<HackBuffer>().cloned(),
                    drain_buffer: world.entity(entity).get::<DrainBuffer>().cloned(),
                    overload_buffer: world.entity(entity).get::<OverloadBuffer>().cloned(),
                    debilitate_buffer: world.entity(entity).get::<DebilitateBuffer>().cloned(),
                    disrupt_buffer: world.entity(entity).get::<DisruptBuffer>().cloned(),
                    fortify_buffer: world.entity(entity).get::<FortifyBuffer>().cloned(),
                    leech_buffer: world.entity(entity).get::<LeechBuffer>().cloned(),
                    fabricate_buffer: world.entity(entity).get::<FabricateBuffer>().cloned(),
                };
                snapshot
                    .has_any()
                    .then_some((SnapshotEntity::from(entity), snapshot))
            })
            .collect();

        let tracked_entity_count = entities.len() as u32;
        let allocator = world.entities();
        Self {
            entities,
            terrains: world.resource::<RoomTerrains>().clone(),
            pending_spawns: world.resource::<PendingSpawnQueue>().clone(),
            room_counts: world.resource::<RoomDroneCounts>().clone(),
            pending_combat: world.resource::<PendingCombat>().clone(),
            local_storage: world.resource::<PlayerLocalStorage>().clone(),
            global_storage: world.resource::<PlayerGlobalStorage>().clone(),
            pending_global_transfers: world.resource::<PendingGlobalTransfers>().clone(),
            pending_allied_transfers: world
                .get_resource::<PendingAlliedTransfers>()
                .cloned()
                .unwrap_or_default(),
            allied_transfer_cooldowns: world
                .get_resource::<AlliedTransferCooldowns>()
                .cloned()
                .unwrap_or_default(),
            allied_transfer_daily_usage: world
                .get_resource::<AlliedTransferDailyUsage>()
                .cloned()
                .unwrap_or_default(),
            allied_transfer_daily_tick: world
                .get_resource::<AlliedTransferDailyTick>()
                .cloned()
                .unwrap_or_default(),
            settlement_state: world
                .get_resource::<SettlementState>()
                .cloned()
                .unwrap_or_default(),
            resource_ledger: world.resource::<ResourceLedger>().clone(),
            starting_resources_granted: world
                .resource::<crate::systems::StartingResourcesGranted>()
                .clone(),
            player_first_spawn_tick: world
                .resource::<crate::systems::PlayerFirstSpawnTick>()
                .clone(),
            event_log: world.resource::<EventLog>().clone(),
            entity_total_count: allocator.len(),
            entity_alive_count: tracked_entity_count,
        }
    }

    pub fn restore(self, world: &mut World) -> EntityRemap {
        let current_entities = Self::tracked_entities(world);
        for entity in current_entities {
            let _ = world.despawn(entity);
        }

        let mut entity_map = EntityRemap::default();
        let mut entities = self.entities.into_iter().collect::<Vec<_>>();
        entities.sort_unstable_by_key(|(entity, _)| *entity);
        for (old_entity, snapshot) in entities {
            let mut entity_mut = world.spawn_empty();
            let new_entity = SnapshotEntity::from(entity_mut.id());
            entity_map.insert(old_entity, new_entity);
            restore_component(&mut entity_mut, snapshot.position);
            restore_component(&mut entity_mut, snapshot.owner);
            restore_component(&mut entity_mut, snapshot.drone);
            restore_component(&mut entity_mut, snapshot.structure);
            restore_component(&mut entity_mut, snapshot.resource);
            restore_component(&mut entity_mut, snapshot.source);
            restore_component(&mut entity_mut, snapshot.terrain);
            restore_component(&mut entity_mut, snapshot.controller);
            if snapshot.marked_for_death {
                entity_mut.insert(DeathMark);
            } else {
                entity_mut.remove::<DeathMark>();
            }
            restore_component(&mut entity_mut, snapshot.spawning_grace);
            restore_component(&mut entity_mut, snapshot.attributes);
            restore_component(&mut entity_mut, snapshot.entity_flags);
            restore_component(&mut entity_mut, snapshot.drone_env);
            restore_component(&mut entity_mut, snapshot.code_version);
            restore_component(&mut entity_mut, snapshot.projectile);
            restore_component(&mut entity_mut, snapshot.hack_state);
            restore_component(&mut entity_mut, snapshot.drain_state);
            restore_component(&mut entity_mut, snapshot.overload_state);
            restore_component(&mut entity_mut, snapshot.debilitate_state);
            restore_component(&mut entity_mut, snapshot.disrupt_state);
            restore_component(&mut entity_mut, snapshot.fortify_state);
            restore_component(&mut entity_mut, snapshot.leech_state);
            restore_component(&mut entity_mut, snapshot.fabricate_state);
            restore_component(&mut entity_mut, snapshot.hack_buffer);
            restore_component(&mut entity_mut, snapshot.drain_buffer);
            restore_component(&mut entity_mut, snapshot.overload_buffer);
            restore_component(&mut entity_mut, snapshot.debilitate_buffer);
            restore_component(&mut entity_mut, snapshot.disrupt_buffer);
            restore_component(&mut entity_mut, snapshot.fortify_buffer);
            restore_component(&mut entity_mut, snapshot.leech_buffer);
            restore_component(&mut entity_mut, snapshot.fabricate_buffer);
        }

        *world.resource_mut::<RoomTerrains>() = self.terrains;
        *world.resource_mut::<PendingSpawnQueue>() = self.pending_spawns;
        *world.resource_mut::<RoomDroneCounts>() = self.room_counts;
        *world.resource_mut::<PendingCombat>() = self.pending_combat;
        *world.resource_mut::<PlayerLocalStorage>() = self.local_storage;
        *world.resource_mut::<PlayerGlobalStorage>() = self.global_storage;
        *world.resource_mut::<PendingGlobalTransfers>() = self.pending_global_transfers;
        world.insert_resource(self.pending_allied_transfers);
        world.insert_resource(self.allied_transfer_cooldowns);
        world.insert_resource(self.allied_transfer_daily_usage);
        world.insert_resource(self.allied_transfer_daily_tick);
        world.insert_resource(self.settlement_state);
        *world.resource_mut::<ResourceLedger>() = self.resource_ledger;
        *world.resource_mut::<crate::systems::StartingResourcesGranted>() =
            self.starting_resources_granted;
        *world.resource_mut::<crate::systems::PlayerFirstSpawnTick>() =
            self.player_first_spawn_tick;
        *world.resource_mut::<EventLog>() = self.event_log;
        entity_map
    }

    fn remap_keys(mut self, entity_map: &EntityRemap) -> Self {
        self.entities = self
            .entities
            .into_iter()
            .map(|(entity, snapshot)| (entity_map.get(entity).unwrap_or(entity), snapshot))
            .collect();
        remap_pending_combat(&mut self.pending_combat, entity_map);
        self
    }

    fn tracked_entities(world: &mut World) -> Vec<Entity> {
        let entity_ids = world.query::<Entity>().iter(world).collect::<Vec<_>>();
        entity_ids
            .into_iter()
            .filter(|&entity| {
                world.entity(entity).get::<Position>().is_some()
                    || world.entity(entity).get::<Owner>().is_some()
                    || world.entity(entity).get::<Drone>().is_some()
                    || world.entity(entity).get::<Structure>().is_some()
                    || world
                        .entity(entity)
                        .get::<crate::components::Resource>()
                        .is_some()
                    || world.entity(entity).get::<Source>().is_some()
                    || world.entity(entity).get::<Terrain>().is_some()
                    || world.entity(entity).get::<Controller>().is_some()
                    || world.entity(entity).get::<DeathMark>().is_some()
                    || world.entity(entity).get::<SpawningGrace>().is_some()
                    || world.entity(entity).get::<Attributes>().is_some()
                    || world.entity(entity).get::<EntityFlags>().is_some()
                    || world.entity(entity).get::<DroneEnv>().is_some()
                    || world.entity(entity).get::<CodeVersion>().is_some()
                    || world
                        .entity(entity)
                        .get::<crate::systems::Projectile>()
                        .is_some()
                    || world.entity(entity).get::<HackState>().is_some()
                    || world.entity(entity).get::<DrainState>().is_some()
                    || world.entity(entity).get::<OverloadState>().is_some()
                    || world.entity(entity).get::<DebilitateState>().is_some()
                    || world.entity(entity).get::<DisruptState>().is_some()
                    || world.entity(entity).get::<FortifyState>().is_some()
                    || world.entity(entity).get::<LeechState>().is_some()
                    || world.entity(entity).get::<FabricateState>().is_some()
                    || world.entity(entity).get::<HackBuffer>().is_some()
                    || world.entity(entity).get::<DrainBuffer>().is_some()
                    || world.entity(entity).get::<OverloadBuffer>().is_some()
                    || world.entity(entity).get::<DebilitateBuffer>().is_some()
                    || world.entity(entity).get::<DisruptBuffer>().is_some()
                    || world.entity(entity).get::<FortifyBuffer>().is_some()
                    || world.entity(entity).get::<LeechBuffer>().is_some()
                    || world.entity(entity).get::<FabricateBuffer>().is_some()
            })
            .collect()
    }
}

fn remap_pending_combat(combat: &mut PendingCombat, entity_map: &EntityRemap) {
    for (entity, _) in &mut combat.damage {
        remap_object_id(entity, entity_map);
    }
    for (entity, _, _) in &mut combat.typed_damage {
        remap_object_id(entity, entity_map);
    }
    for (entity, _) in &mut combat.heal {
        remap_object_id(entity, entity_map);
    }
}

impl EntitySnapshot {
    fn has_any(&self) -> bool {
        self.position.is_some()
            || self.owner.is_some()
            || self.drone.is_some()
            || self.structure.is_some()
            || self.resource.is_some()
            || self.source.is_some()
            || self.terrain.is_some()
            || self.controller.is_some()
            || self.marked_for_death
            || self.spawning_grace.is_some()
            || self.attributes.is_some()
            || self.entity_flags.is_some()
            || self.drone_env.is_some()
            || self.code_version.is_some()
            || self.projectile.is_some()
            || self.hack_state.is_some()
            || self.drain_state.is_some()
            || self.overload_state.is_some()
            || self.debilitate_state.is_some()
            || self.disrupt_state.is_some()
            || self.fortify_state.is_some()
            || self.leech_state.is_some()
            || self.fabricate_state.is_some()
            || self.hack_buffer.is_some()
            || self.drain_buffer.is_some()
            || self.overload_buffer.is_some()
            || self.debilitate_buffer.is_some()
            || self.disrupt_buffer.is_some()
            || self.fortify_buffer.is_some()
            || self.leech_buffer.is_some()
            || self.fabricate_buffer.is_some()
    }
}

fn serialize_entity_snapshot(snapshot: &EntitySnapshot) -> Vec<u8> {
    serde_json::to_vec(snapshot).expect("entity snapshots must serialize")
}

fn add_player_resource_totals(
    totals: &mut HashMap<PlayerId, u64>,
    storage: &indexmap::IndexMap<PlayerId, ResourceCost>,
) {
    for (player_id, resources) in storage {
        *totals.entry(*player_id).or_default() += resource_total(resources);
    }
}

fn resource_total(resources: &indexmap::IndexMap<String, u32>) -> u64 {
    resources.values().map(|amount| *amount as u64).sum()
}

fn restore_component<T: Component>(entity: &mut EntityWorldMut<'_>, component: Option<T>) {
    if let Some(component) = component {
        entity.insert(component);
    } else {
        entity.remove::<T>();
    }
}

#[cfg(test)]
mod tests {
    use crate::command::{CommandAction, CommandAuth, Direction, RejectionReason, object_id};
    use crate::systems::PendingSpawnQueue;
    use crate::{BodyPart, CommandIntent, Structure, StructureType, create_world, energy_cost};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    use super::*;

    #[derive(Debug)]
    struct OverlapState {
        arrived: usize,
        active: usize,
        max_active: usize,
    }

    #[derive(Debug)]
    struct OverlapExecutor {
        player_id: PlayerId,
        expected_players: usize,
        sequence: u32,
        action: CommandAction,
        overlap: Arc<(Mutex<OverlapState>, Condvar)>,
    }

    impl PlayerExecutor for OverlapExecutor {
        fn collect(&mut self, snapshot: TickSnapshot) -> Result<Vec<CommandIntent>, ExecutorError> {
            assert_eq!(snapshot.player_id, self.player_id);
            let (state, completed) = &*self.overlap;
            let mut state = state.lock().unwrap();
            state.arrived += 1;
            state.active += 1;
            state.max_active = state.max_active.max(state.active);
            completed.notify_all();

            let wait = completed
                .wait_timeout_while(state, Duration::from_millis(250), |state| {
                    state.arrived < self.expected_players
                })
                .unwrap();
            state = wait.0;
            state.active -= 1;
            completed.notify_all();
            if state.arrived < self.expected_players {
                return Err(ExecutorError::Error(
                    "player collection did not overlap".to_string(),
                ));
            }

            Ok(vec![CommandIntent {
                sequence: self.sequence,
                action: self.action.clone(),
            }])
        }
    }

    #[derive(Debug, Clone)]
    struct StaticExecutor {
        result: Result<Vec<CommandIntent>, ExecutorError>,
    }

    impl PlayerExecutor for StaticExecutor {
        fn collect(
            &mut self,
            _snapshot: TickSnapshot,
        ) -> Result<Vec<CommandIntent>, ExecutorError> {
            self.result.clone()
        }
    }

    #[derive(Debug, Clone, Default)]
    struct EnvironmentRecordingCommitter {
        records: Vec<TickTrace>,
        environments: Vec<ReplayEnvironment>,
        fail_count: u32,
    }

    impl TickCommitter for EnvironmentRecordingCommitter {
        fn commit(&mut self, trace: TickTrace) -> Result<(), CommitError> {
            self.records.push(trace);
            Ok(())
        }

        fn commit_with_environment(
            &mut self,
            trace: TickTrace,
            environment: ReplayEnvironment,
        ) -> Result<(), CommitError> {
            if self.fail_count > 0 {
                self.fail_count -= 1;
                return Err(CommitError::Failed("test commit failed".to_string()));
            }
            self.environments.push(environment);
            self.commit(trace)
        }
    }

    #[derive(Debug, Clone)]
    struct CountingExecutor {
        result: Result<Vec<CommandIntent>, ExecutorError>,
        calls: Arc<Mutex<u32>>,
    }

    impl PlayerExecutor for CountingExecutor {
        fn collect(
            &mut self,
            _snapshot: TickSnapshot,
        ) -> Result<Vec<CommandIntent>, ExecutorError> {
            *self.calls.lock().unwrap() += 1;
            self.result.clone()
        }
    }

    #[derive(Debug, Clone, Default)]
    struct SnapshotRecordingExecutor {
        snapshots: Arc<Mutex<Vec<TickSnapshot>>>,
    }

    impl PlayerExecutor for SnapshotRecordingExecutor {
        fn collect(&mut self, snapshot: TickSnapshot) -> Result<Vec<CommandIntent>, ExecutorError> {
            self.snapshots.lock().unwrap().push(snapshot);
            Ok(Vec::new())
        }
    }

    #[derive(Debug, Clone)]
    struct MetricsExecutor {
        intents: Vec<CommandIntent>,
        metrics: PlayerCollectMetrics,
    }

    impl PlayerExecutor for MetricsExecutor {
        fn collect(
            &mut self,
            _snapshot: TickSnapshot,
        ) -> Result<Vec<CommandIntent>, ExecutorError> {
            Ok(self.intents.clone())
        }

        fn collect_with_metrics(
            &mut self,
            _snapshot: TickSnapshot,
        ) -> Result<PlayerCollectOutput, ExecutorError> {
            Ok(PlayerCollectOutput {
                intents: self.intents.clone(),
                metrics: self.metrics.clone(),
            })
        }
    }

    #[derive(Resource, Debug, Default)]
    struct LedgerValidationAttempts(u32);

    #[derive(Debug, Clone, Default)]
    struct FakeAtomicStore {
        writes: HashMap<Vec<u8>, Vec<u8>>,
        fail_next: bool,
    }

    impl AtomicTickStore for FakeAtomicStore {
        fn atomic_commit(&mut self, writes: Vec<TickTraceWrite>) -> Result<(), CommitError> {
            if self.fail_next {
                self.fail_next = false;
                return Err(CommitError::Failed("fake redb commit failed".to_string()));
            }

            for (key, value) in writes {
                self.writes.insert(key, value);
            }
            Ok(())
        }
    }

    fn sample_trace() -> TickTrace {
        let mut world = create_world();
        let state = TickState::capture(world.app.world_mut());
        TickTrace {
            tick: 42,
            player_id: 7,
            commands: vec![raw_harvest(7, 1, 42, 100, 200)],
            state,
            rejections: Vec::new(),
            metrics: TickMetrics {
                accepted_commands: 1,
                ..Default::default()
            },
            state_checksum: world.state_checksum(),
            system_manifest_hash: system_manifest_hash(),
            action_manifest_hash: action_manifest_hash(world.app.world()),
            security_alerts: Vec::new(),
            trace_events: Vec::new(),
            resource_ledger: ResourceLedgerTraceSnapshot::default(),
        }
    }

    fn transfer_to_global_trace() -> (TickState, TickTrace) {
        let mut world = create_world();
        world
            .app
            .world_mut()
            .resource_mut::<PlayerLocalStorage>()
            .0
            .entry(1)
            .or_default()
            .insert("Energy".to_string(), 20);
        let previous_state = TickState::capture(world.app.world_mut());
        let executor = StaticExecutor {
            result: Ok(vec![CommandIntent {
                sequence: 1,
                action: CommandAction::TransferToGlobal {
                    resource: "Energy".to_string(),
                    amount: 10,
                },
            }]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();
        assert!(report.committed);
        let trace = scheduler.committer.records[0].clone();
        assert!(!trace.resource_ledger.operations.is_empty());
        assert_ne!(trace.resource_ledger.ledger_digest, [0; 32]);
        (previous_state, trace)
    }

    #[test]
    fn redb_tick_committer_writes_required_tick_keys_atomically() {
        let trace = sample_trace();
        let mut committer = RedbTickCommitter::new(FakeAtomicStore::default());

        committer
            .commit(trace)
            .expect("atomic tick commit should succeed");
        let store = committer.into_inner();

        assert_eq!(store.writes.len(), 8);
        for suffix in [
            "commit_record",
            "state",
            "commands",
            "rejections",
            "metrics",
            "resource_ledger",
            "security_alerts",
            "delta",
        ] {
            assert!(
                store.writes.contains_key(&tick_key(42, suffix)),
                "missing /tick/42/{suffix}"
            );
        }
        assert!(serde_json::from_slice::<TickState>(&store.writes[&tick_key(42, "state")]).is_ok());
        let commands: Vec<RawCommand> =
            serde_json::from_slice(&store.writes[&tick_key(42, "commands")]).unwrap();
        assert_eq!(commands.len(), 1);
        let metrics: TickMetrics =
            serde_json::from_slice(&store.writes[&tick_key(42, "metrics")]).unwrap();
        assert_eq!(metrics.accepted_commands, 1);
        let delta: crate::tick::WorldDelta =
            serde_json::from_slice(&store.writes[&tick_key(42, "delta")]).unwrap();
        assert_eq!(delta.to_tick, 42);
        assert_eq!(delta.commands.len(), 1);
    }

    #[test]
    fn redb_tick_committer_writes_ten_field_commit_record() {
        let trace = sample_trace();
        let mut committer = RedbTickCommitter::new(FakeAtomicStore::default());

        committer.commit(trace.clone()).unwrap();
        let store = committer.into_inner();
        let record: TickCommitRecord =
            serde_json::from_slice(&store.writes[&tick_key(42, "commit_record")]).unwrap();
        let record_json = serde_json::to_value(&record).unwrap();

        assert_eq!(record_json.as_object().unwrap().len(), 10);
        assert_eq!(record.commands, trace.commands);
        assert_eq!(record.rejections, trace.rejections);
        assert_eq!(record.state_checksum, trace.state_checksum);
        assert_eq!(
            record.commands_hash,
            commands_hash(&record.commands, &record.rejections)
        );
    }

    #[test]
    fn commands_hash_excludes_active_player_roster_while_fuel_keeps_zero_entries() {
        let trace = sample_trace();
        let world = create_world();
        let environment_for = |active_players| {
            ReplayEnvironment::capture(world.app.world()).with_collect_context(
                [8; 32],
                RngContext::default(),
                active_players,
                Vec::new(),
            )
        };

        let one_player = TickCommitRecord::from_trace(&trace, &environment_for(vec![7]));
        let two_players = TickCommitRecord::from_trace(&trace, &environment_for(vec![7, 8]));

        assert_eq!(one_player.commands_hash, two_players.commands_hash);
        assert_eq!(one_player.fuel.entries.len(), 1);
        assert_eq!(two_players.fuel.entries.len(), 2);
        assert_eq!(two_players.fuel.entries[1].player_id, 8);
        assert_eq!(two_players.fuel.entries[1].consumed, 0);
    }

    #[test]
    fn action_and_replay_manifest_hashes_bind_runtime_actions_and_fuel_schedule() {
        let mut world = create_world();
        let base_action_hash = action_manifest_hash(world.app.world());
        world
            .app
            .world_mut()
            .resource_mut::<ActionRegistry>()
            .handlers
            .insert("mod-action".to_string(), "mod-handler".to_string());
        let changed_action_hash = action_manifest_hash(world.app.world());
        assert_ne!(base_action_hash, changed_action_hash);

        let trace = sample_trace();
        let environment = ReplayEnvironment::capture(world.app.world());
        let current = replay_manifest_hash_with_fuel_schedule(
            &trace,
            &environment,
            HOST_FUEL_SCHEDULE_VERSION,
            HOST_FUEL_SCHEDULE,
        );
        let version_changed = replay_manifest_hash_with_fuel_schedule(
            &trace,
            &environment,
            "swarm.host-fuel.v2",
            HOST_FUEL_SCHEDULE,
        );
        let mut changed_schedule = HOST_FUEL_SCHEDULE.to_vec();
        changed_schedule[0].1 += 1;
        let schedule_changed = replay_manifest_hash_with_fuel_schedule(
            &trace,
            &environment,
            HOST_FUEL_SCHEDULE_VERSION,
            &changed_schedule,
        );

        assert_ne!(current, version_changed);
        assert_ne!(current, schedule_changed);
    }

    #[test]
    fn tick_commit_record_serializes_per_player_fuel_and_resource_ledger_snapshot() {
        let mut trace = sample_trace();
        trace.commands = vec![raw_harvest(7, 1, 42, 100, 200)];
        trace.rejections = vec![command_rejection_with_refund(
            raw_harvest(9, 1, 42, 101, 201),
            RejectionReason::SourceEmpty,
            5_000,
        )];
        let mut resource_ledger = ResourceLedger::default();
        resource_ledger.record_transfer_amounts(
            42,
            Some(7),
            Some(9),
            "Energy",
            100,
            98,
            crate::resource_ledger::ResourceOperation::AlliedTransfer,
            2,
            200,
        );
        trace.resource_ledger = resource_ledger.trace_snapshot();
        let environment = ReplayEnvironment {
            mods_lock: ModsLock::default(),
            world_config: WorldConfigSnapshot {
                config: WorldConfig::default(),
            },
            collect_snapshot_hash: [8; 32],
            rng_context: RngContext::default(),
            active_players: vec![7, 8, 9],
            player_fuel_metrics: vec![
                PlayerFuelMetric {
                    player_id: 7,
                    consumed: 123,
                    refund_events: 0,
                    refunded: 0,
                },
                PlayerFuelMetric {
                    player_id: 9,
                    consumed: 456,
                    refund_events: 1,
                    refunded: 5_000,
                },
            ],
            deploy_activation_decisions: Vec::new(),
        };

        let writes = tick_trace_writes_with_environment(&trace, &environment).unwrap();
        let writes = writes.into_iter().collect::<HashMap<_, _>>();
        let record: TickCommitRecord =
            serde_json::from_slice(&writes[&tick_key(42, "commit_record")]).unwrap();
        let persisted_ledger: ResourceLedgerTraceSnapshot =
            serde_json::from_slice(&writes[&tick_key(42, "resource_ledger")]).unwrap();

        assert_eq!(record.fuel.entries.len(), 3);
        assert_eq!(record.fuel.entries[0].player_id, 7);
        assert_eq!(record.fuel.entries[0].consumed, 123);
        assert_eq!(record.fuel.entries[1].player_id, 8);
        assert_eq!(record.fuel.entries[1].consumed, 0);
        assert_eq!(record.fuel.entries[2].player_id, 9);
        assert_eq!(record.fuel.entries[2].consumed, 456);
        assert_eq!(record.fuel.entries[2].refund_events, 1);
        assert_eq!(record.fuel.entries[2].refunded, 5_000);
        assert_eq!(persisted_ledger, trace.resource_ledger);
        assert_eq!(persisted_ledger.operations.len(), 1);
        assert_eq!(persisted_ledger.operations[0].fee_paid, 2);
        assert_eq!(persisted_ledger.operations[0].basis_points_used, 200);
        assert_eq!(
            *persisted_ledger
                .balance_delta
                .get(&7)
                .unwrap()
                .get("Energy")
                .unwrap(),
            -100
        );
        assert_eq!(
            *persisted_ledger
                .balance_delta
                .get(&9)
                .unwrap()
                .get("Energy")
                .unwrap(),
            98
        );
        assert_ne!(persisted_ledger.ledger_checksum, 0);
        assert!(persisted_ledger.conservation_imbalance.is_empty());
    }

    #[test]
    fn tick_trace_resource_ledger_contains_command_and_system_phase_ops() {
        fn test_system_phase_ledger_op(
            mut ledger: ResMut<ResourceLedger>,
            current_tick: Res<CurrentTick>,
        ) {
            ledger.record_account_transfer(
                current_tick.0,
                crate::resource_ledger::LedgerAccount::system("test_system_award"),
                crate::resource_ledger::LedgerAccount::player(1),
                "Energy",
                3,
                crate::resource_ledger::ResourceOperation::PvEAward,
            );
        }

        let mut world = create_world();
        world
            .app
            .world_mut()
            .resource_mut::<PlayerLocalStorage>()
            .0
            .entry(1)
            .or_default()
            .insert("Energy".to_string(), 20);
        world.app.add_systems(
            Update,
            test_system_phase_ledger_op.before(crate::resource_ledger::resource_ledger_system),
        );
        let executor = MetricsExecutor {
            intents: vec![CommandIntent {
                sequence: 1,
                action: CommandAction::TransferToGlobal {
                    resource: "Energy".to_string(),
                    amount: 10,
                },
            }],
            metrics: PlayerCollectMetrics::default(),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            EnvironmentRecordingCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();

        assert!(report.committed);
        let trace = scheduler
            .committer
            .records
            .first()
            .expect("tick trace should be committed");
        let operations = trace
            .resource_ledger
            .operations
            .iter()
            .map(|entry| entry.operation)
            .collect::<Vec<_>>();
        assert!(operations.contains(&crate::resource_ledger::ResourceOperation::GlobalDeposit));
        assert!(operations.contains(&crate::resource_ledger::ResourceOperation::PvEAward));
        assert!(trace.resource_ledger.conservation_imbalance.is_empty());
        let ledger = scheduler.world.app.world().resource::<ResourceLedger>();
        assert!(ledger.ops.is_empty());
        assert!(ledger.balance_delta.is_empty());
        assert!(ledger.account_delta.is_empty());
        assert_ne!(ledger.ledger_checksum, 0);
        assert_eq!(ledger.last_tick, trace.resource_ledger);
    }

    #[test]
    fn metrics_aware_executor_persists_actual_collect_fuel() {
        let executor = MetricsExecutor {
            intents: Vec::new(),
            metrics: PlayerCollectMetrics {
                fuel_consumed: 777,
                refund_events: 2,
                refunded: 33,
            },
        };
        let mut scheduler = TickScheduler::new(
            create_world(),
            1,
            executor,
            RedbTickCommitter::new(FakeAtomicStore::default()),
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();
        let store = scheduler.committer.into_inner();
        let record: TickCommitRecord =
            serde_json::from_slice(&store.writes[&tick_key(0, "commit_record")]).unwrap();
        let metrics: TickMetrics =
            serde_json::from_slice(&store.writes[&tick_key(0, "metrics")]).unwrap();

        assert!(report.committed);
        assert_eq!(metrics.fuel_consumed, 777);
        assert_eq!(metrics.refund_events, 2);
        assert_eq!(metrics.refund_fuel, 33);
        assert_eq!(record.fuel.entries.len(), 1);
        assert_eq!(record.fuel.entries[0].player_id, 1);
        assert_eq!(record.fuel.entries[0].consumed, 777);
        assert_eq!(record.fuel.entries[0].refund_events, 2);
        assert_eq!(record.fuel.entries[0].refunded, 33);
    }

    #[test]
    fn world_snapshot_without_resource_ledger_defaults_on_deserialize() {
        let mut world = create_world();
        world
            .app
            .world_mut()
            .resource_mut::<ResourceLedger>()
            .record(
                0,
                Some(1),
                Some(2),
                "Energy",
                7,
                crate::resource_ledger::ResourceOperation::AlliedTransfer,
            );
        let snapshot = WorldSnapshot::capture(world.app.world_mut());
        let mut value = serde_json::to_value(snapshot).unwrap();
        value
            .as_object_mut()
            .expect("WorldSnapshot should serialize as object")
            .remove("resource_ledger");

        let restored: WorldSnapshot = serde_json::from_value(value).unwrap();

        assert_eq!(restored.resource_ledger, ResourceLedger::default());
        restored.restore(world.app.world_mut());
        assert_eq!(
            world.app.world().resource::<ResourceLedger>(),
            &ResourceLedger::default()
        );
    }

    #[test]
    fn state_checksum_changes_for_ledger_only_mutation() {
        let mut world = create_world();
        let before = world.state_checksum();

        world
            .app
            .world_mut()
            .resource_mut::<ResourceLedger>()
            .record(
                0,
                Some(1),
                Some(2),
                "Energy",
                7,
                crate::resource_ledger::ResourceOperation::AlliedTransfer,
            );

        assert_ne!(before, world.state_checksum());
    }

    #[test]
    fn redb_tick_committer_does_not_write_partial_trace_on_commit_failure() {
        let trace = sample_trace();
        let mut committer = RedbTickCommitter::new(FakeAtomicStore {
            fail_next: true,
            ..Default::default()
        });

        assert!(committer.commit(trace).is_err());
        assert!(committer.into_inner().writes.is_empty());
    }

    fn drone_count(world: &mut SwarmWorld) -> usize {
        world
            .app
            .world_mut()
            .query::<&Drone>()
            .iter(world.app.world())
            .count()
    }

    fn spawn_structure(world: &mut SwarmWorld, owner: PlayerId, x: i32, y: i32) -> Entity {
        world
            .app
            .world_mut()
            .spawn((
                Position {
                    x,
                    y,
                    room: RoomId(0),
                },
                Structure {
                    structure_type: StructureType::Spawn,
                    owner: Some(owner),
                    hits: 5_000,
                    hits_max: 5_000,
                    energy: Some(300),
                    energy_capacity: Some(300),
                    cooldown: 0,
                },
            ))
            .id()
    }

    #[test]
    fn multi_player_tick_collects_players_in_parallel_and_executes_serially() {
        let mut world = create_world();
        let first = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let second = world.spawn_drone(2, 10, 12, vec![BodyPart::Move]);
        let overlap = Arc::new((
            Mutex::new(OverlapState {
                arrived: 0,
                active: 0,
                max_active: 0,
            }),
            Condvar::new(),
        ));
        let mut executors: HashMap<PlayerId, Box<dyn PlayerExecutor>> = HashMap::new();
        executors.insert(
            1,
            Box::new(OverlapExecutor {
                player_id: 1,
                expected_players: 2,
                sequence: 1,
                action: CommandAction::Move {
                    object_id: object_id(first),
                    direction: Direction::Top,
                },
                overlap: overlap.clone(),
            }),
        );
        executors.insert(
            2,
            Box::new(OverlapExecutor {
                player_id: 2,
                expected_players: 2,
                sequence: 1,
                action: CommandAction::Move {
                    object_id: object_id(second),
                    direction: Direction::Top,
                },
                overlap: overlap.clone(),
            }),
        );
        let mut scheduler = MultiPlayerTickScheduler::new(
            world,
            executors,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();

        assert!(report.committed);
        assert!(report.broadcasted);
        assert_eq!(scheduler.tick_counter, 1);
        assert_eq!(scheduler.committer.records.len(), 1);
        assert_eq!(scheduler.broadcaster.broadcasts.len(), 2);
        assert_eq!(scheduler.broadcaster.broadcasts[0].player_id, 1);
        assert_eq!(scheduler.broadcaster.broadcasts[1].player_id, 2);
        assert!(scheduler.broadcaster.broadcasts[0].full_snapshot);
        assert!(scheduler.broadcaster.broadcasts[1].full_snapshot);
        assert!(
            scheduler.broadcaster.broadcasts[0]
                .removed_entities
                .is_empty()
        );
        assert!(
            scheduler.broadcaster.broadcasts[1]
                .removed_entities
                .is_empty()
        );
        assert_eq!(
            scheduler.broadcaster.broadcasts[0].state_checksum,
            scheduler.committer.records[0].state_checksum
        );
        assert_eq!(
            scheduler.broadcaster.broadcasts[1].state_checksum,
            scheduler.committer.records[0].state_checksum
        );
        assert!(
            scheduler.broadcaster.broadcasts[0]
                .changed_entities
                .iter()
                .any(|entity| matches!(entity, VisibleEntity::Drone(drone) if drone.id == object_id(first)))
        );
        assert!(
            scheduler.broadcaster.broadcasts[1]
                .changed_entities
                .iter()
                .any(|entity| matches!(entity, VisibleEntity::Drone(drone) if drone.id == object_id(second)))
        );
        assert_eq!(report.accepted.len(), 2);
        assert_eq!(report.rejections.len(), 0);
        assert_eq!(overlap.0.lock().unwrap().max_active, 2);
        assert_eq!(
            scheduler
                .world
                .app
                .world()
                .entity(first)
                .get::<Position>()
                .unwrap()
                .y,
            9
        );
        assert_eq!(
            scheduler
                .world
                .app
                .world()
                .entity(second)
                .get::<Position>()
                .unwrap()
                .y,
            11
        );
    }

    #[test]
    fn blake3_xof_player_shuffle_is_deterministic_per_tick_and_checksum() {
        let players = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let first = seeded_player_shuffle(players.clone(), 7, 42);
        let second = seeded_player_shuffle(players.clone(), 7, 42);

        assert_eq!(first, second);
        assert_ne!(first, players);
    }

    #[test]
    fn rng_context_canonicalizes_shuffle_input_and_namespaces_seed() {
        let players = vec![8, 1, 5, 3, 2, 7, 4, 6];
        let same_players_different_input_order = vec![3, 5, 8, 1, 6, 7, 2, 4];
        let context = RngContext::derive(1234, 2, 99, "shuffle");
        let same_context = RngContext::derive(1234, 2, 99, "shuffle");
        let different_namespace = RngContext::derive(1234, 2, 99, "combat");

        assert_eq!(context, same_context);
        assert_ne!(context.seed, different_namespace.seed);
        assert_eq!(context.namespace, "shuffle");
        assert_eq!(
            seeded_player_shuffle_with_context(players, &context),
            seeded_player_shuffle_with_context(same_players_different_input_order, &context)
        );
    }

    fn active_deployment(player_id: PlayerId, load_after_tick: Tick) -> ActiveDeployment {
        ActiveDeployment {
            deploy_id: "deploy-tick".to_string(),
            world_id: "world-alpha".to_string(),
            module_slot: "spawn:10:10".to_string(),
            player_id,
            room_id: RoomId(1),
            drone_id: 99,
            module_hash: [7; 32],
            metadata_hash: "blake3:metadata".to_string(),
            signed_payload_hash: "blake3:signed-payload".to_string(),
            compiled_artifact_hash: [8; 32],
            client_version_counter: 4,
            redb_version_counter: 4,
            certificate_id: "cert-1".to_string(),
            certificate_fingerprint: "fingerprint-1".to_string(),
            transport: "mcp".to_string(),
            signed_at: "1700000000".to_string(),
            accepted_at_tick: load_after_tick.saturating_sub(1),
            wasm_bytes: b"\0asmtest".to_vec(),
            load_after_tick,
        }
    }

    #[test]
    fn serial_execution_queue_uses_world_seed_for_stable_player_shuffle() {
        let collected = |players: &[PlayerId]| {
            players
                .iter()
                .map(|player_id| CollectedPlayerCommands {
                    player_id: *player_id,
                    commands: vec![raw_harvest(*player_id, 1, 1, *player_id as u64, 200)],
                })
                .collect::<Vec<_>>()
        };
        let order = |commands: Vec<RawCommand>| {
            commands
                .into_iter()
                .map(|command| command.player_id)
                .collect::<Vec<_>>()
        };

        let active_players = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let command_players = vec![1, 2, 4, 5, 6, 7, 8];
        let first = order(serial_execution_queue_for_active_players(
            collected(&[8, 1, 5, 2, 7, 4, 6]),
            99,
            &active_players,
        ));
        let second = order(serial_execution_queue_for_active_players(
            collected(&[5, 8, 1, 6, 7, 2, 4]),
            99,
            &active_players,
        ));
        let changed_seed = (0..128)
            .map(|seed| {
                order(serial_execution_queue_for_active_players(
                    collected(&command_players),
                    seed,
                    &active_players,
                ))
            })
            .find(|candidate| *candidate != first)
            .expect("fixture should expose world-seed-sensitive shuffle");
        let command_derived_order = order(serial_execution_queue_for_active_players(
            collected(&command_players),
            99,
            &command_players,
        ));

        assert_eq!(first, second);
        assert_ne!(first, changed_seed);
        assert_ne!(first, command_derived_order);
    }

    #[test]
    fn collect_player_results_uses_single_executor_fast_path() {
        let calls = Arc::new(Mutex::new(0));
        let mut executors: HashMap<PlayerId, Box<dyn PlayerExecutor>> = HashMap::new();
        executors.insert(
            7,
            Box::new(CountingExecutor {
                calls: calls.clone(),
                result: Ok(Vec::new()),
            }),
        );

        let mut world = create_world();
        let (collect_inputs, _, _) = collect_inputs_for_players(&mut world, &[7], 3, 99);

        let results = collect_player_results(&mut executors, 3, 99, &collect_inputs);

        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].player_id, 7);
        assert!(results[0].commands.is_empty());
    }

    #[test]
    fn collect_retry_reuses_per_player_snapshot_hash_without_rerunning_executor() {
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let mut executors: HashMap<PlayerId, Box<dyn PlayerExecutor>> = HashMap::new();
        executors.insert(
            1,
            Box::new(SnapshotRecordingExecutor {
                snapshots: snapshots.clone(),
            }),
        );
        let mut scheduler = MultiPlayerTickScheduler::new(
            create_world(),
            executors,
            InMemoryTickCommitter {
                fail_count: 1,
                ..Default::default()
            },
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();

        let captured = snapshots.lock().unwrap();
        assert!(report.committed);
        assert_eq!(
            captured.len(),
            1,
            "COLLECT must not rerun after commit retry"
        );
        assert_eq!(captured[0].perception.player_id, 1);
        assert_eq!(
            captured[0].snapshot_hash,
            snapshot_hash(&captured[0].perception)
        );
    }

    #[test]
    fn tick_metrics_row_calculates_clickhouse_health_metrics_from_trace() {
        let metrics = TickMetrics {
            executor_timeouts: 1,
            accepted_commands: 3,
            rejected_commands: 1,
            total_commands: 4,
            refund_events: 1,
            refund_fuel: 5_000,
            duration_ms: 42,
            ..Default::default()
        };
        let trace = TickTrace {
            tick: 7,
            player_id: 11,
            commands: Vec::new(),
            state: TickState::capture(create_world().app.world_mut()),
            rejections: Vec::new(),
            metrics,
            state_checksum: 99,
            system_manifest_hash: system_manifest_hash(),
            action_manifest_hash: action_manifest_hash(create_world().app.world()),
            security_alerts: Vec::new(),
            trace_events: Vec::new(),
            resource_ledger: ResourceLedgerTraceSnapshot::default(),
        };

        let row = ClickHouseTickMetricsRow::from_trace(&trace, &[10, 20, 30, 40]);

        assert_eq!(row.tick, 7);
        assert_eq!(row.player_id, 11);
        assert_eq!(row.collect_timeout_rate, 1.0);
        assert_eq!(row.refund_abuse_rate, 0.25);
        assert_eq!(row.command_rejection_rate, 0.25);
        assert_eq!(row.tick_duration_p99, 40);
        assert_eq!(row.refund_fuel, 5_000);
    }

    fn strategy_trace(
        tick: Tick,
        player_id: PlayerId,
        metrics: TickMetrics,
        local_energy: u32,
    ) -> TickTrace {
        let mut world = create_world();
        world
            .app
            .world_mut()
            .resource_mut::<PlayerLocalStorage>()
            .0
            .insert(player_id, energy_cost(local_energy));

        TickTrace {
            tick,
            player_id,
            commands: Vec::new(),
            state: TickState::capture(world.app.world_mut()),
            rejections: Vec::new(),
            metrics,
            state_checksum: world.state_checksum(),
            system_manifest_hash: system_manifest_hash(),
            action_manifest_hash: action_manifest_hash(world.app.world()),
            security_alerts: Vec::new(),
            trace_events: Vec::new(),
            resource_ledger: ResourceLedgerTraceSnapshot::default(),
        }
    }

    #[test]
    fn strategy_dashboard_aggregates_player_metrics_over_tick_range() {
        let traces = vec![
            strategy_trace(
                1,
                7,
                TickMetrics {
                    executor_timeouts: 1,
                    total_commands: 2,
                    rejected_commands: 1,
                    fuel_consumed: 20_000,
                    ..Default::default()
                },
                100,
            ),
            strategy_trace(
                2,
                7,
                TickMetrics {
                    total_commands: 3,
                    fuel_consumed: 30_000,
                    ..Default::default()
                },
                130,
            ),
        ];

        let rows = aggregate_strategy_metrics_dashboard(&traces, 1, 2);

        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.player_id, 7);
        assert_eq!(row.tick_start, 1);
        assert_eq!(row.tick_end, 2);
        assert_eq!(row.tick_count, 2);
        assert_eq!(row.fuel_consumed, 50_000);
        assert_eq!(row.timeout_rate, 0.5);
        assert_eq!(row.command_rejection_rate, 0.2);
        assert_eq!(row.resource_start, 100);
        assert_eq!(row.resource_end, 130);
        assert_eq!(row.resource_growth_rate, 30.0);
        assert!(CLICKHOUSE_STRATEGY_METRICS_INSERT.contains("fuel_consumed"));
        assert!(CLICKHOUSE_STRATEGY_METRICS_INSERT.contains("resource_growth_rate"));
        assert_eq!(
            row.insert_sql_values(),
            "(7, 1, 2, 2, 50000, 0.500000, 0.200000, 100, 130, 30.000000)"
        );
    }

    #[test]
    fn strategy_dashboard_attributes_multiplayer_trace_commands_to_players() {
        let mut trace = strategy_trace(5, 0, TickMetrics::default(), 0);
        trace.commands = vec![raw_harvest(2, 1, 5, 100, 200)];
        trace.rejections = vec![CommandRejection::new(
            raw_harvest(9, 1, 5, 101, 201),
            RejectionReason::ObjectNotFound,
        )];

        let rows = aggregate_strategy_metrics_dashboard(&[trace], 5, 5);

        assert_eq!(
            rows.iter().map(|row| row.player_id).collect::<Vec<_>>(),
            vec![2, 9]
        );
        assert_eq!(rows[0].command_rejection_rate, 0.0);
        assert_eq!(rows[1].command_rejection_rate, 1.0);
        assert_eq!(rows[0].fuel_consumed, 0);
        assert_eq!(rows[1].fuel_consumed, 0);
    }

    #[test]
    fn execution_metrics_include_refund_fuel_for_refund_abuse_rate() {
        let mut world = create_world();
        let first = world.spawn_drone(1, 24, 25, vec![BodyPart::Work, BodyPart::Carry]);
        let second = world.spawn_drone(2, 26, 25, vec![BodyPart::Work, BodyPart::Carry]);
        let source = world
            .app
            .world_mut()
            .query::<(Entity, &mut Source)>()
            .iter_mut(world.app.world_mut())
            .map(|(entity, mut source)| {
                source.capacity = 2;
                entity
            })
            .next()
            .expect("expected source");

        let execution = execute_deterministic(
            &mut world,
            vec![
                raw_harvest(1, 1, 1, object_id(first), object_id(source)),
                raw_harvest(2, 2, 1, object_id(second), object_id(source)),
            ],
        );
        let mut metrics = TickMetrics::default();
        metrics.record_execution(execution.commands.len(), &execution.rejections);

        assert_eq!(metrics.total_commands, 2);
        assert_eq!(metrics.rejected_commands, 1);
        assert_eq!(metrics.fuel_consumed, 0);
        assert_eq!(metrics.refund_events, 1);
        assert_eq!(metrics.refund_fuel, 5_000);
        assert_eq!(metrics.command_rejection_rate(), 0.5);
        assert_eq!(metrics.refund_abuse_rate(), 0.5);
    }

    #[test]
    fn normal_tick_collects_executes_commits_broadcasts_and_increments() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let drone_id = object_id(drone);
        let executor = StaticExecutor {
            result: Ok(vec![CommandIntent {
                sequence: 2,
                action: CommandAction::Move {
                    object_id: drone_id,
                    direction: Direction::Top,
                },
            }]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();

        assert!(report.committed);
        assert!(report.broadcasted);
        assert_eq!(scheduler.tick_counter, 1);
        assert_eq!(scheduler.committer.records.len(), 1);
        assert_eq!(scheduler.broadcaster.broadcasts.len(), 1);
        assert_eq!(scheduler.broadcaster.broadcasts[0].player_id, 1);
        assert!(scheduler.broadcaster.broadcasts[0].full_snapshot);
        assert!(
            scheduler.broadcaster.broadcasts[0]
                .changed_entities
                .iter()
                .any(
                    |entity| matches!(entity, VisibleEntity::Drone(drone) if drone.id == drone_id)
                )
        );
        assert!(
            scheduler.broadcaster.broadcasts[0]
                .removed_entities
                .is_empty()
        );
        assert_eq!(report.accepted.len(), 1);
        assert_eq!(report.rejections.len(), 0);
        assert_eq!(
            scheduler
                .world
                .app
                .world()
                .entity(drone)
                .get::<Position>()
                .unwrap()
                .y,
            9
        );
        // Aging system (S23) increments age by 1 each tick
        assert_eq!(
            scheduler
                .world
                .app
                .world()
                .entity(drone)
                .get::<Drone>()
                .unwrap()
                .age,
            1
        );
    }

    #[test]
    fn normal_tick_full_snapshot_replaces_visible_state_with_empty_removals() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let drone_id = object_id(drone);
        let executor = StaticExecutor {
            result: Ok(vec![CommandIntent {
                sequence: 1,
                action: CommandAction::Recycle {
                    object_id: drone_id,
                },
            }]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();

        assert!(report.committed);
        assert!(report.broadcasted);
        assert_eq!(scheduler.broadcaster.broadcasts.len(), 1);
        let broadcast = &scheduler.broadcaster.broadcasts[0];
        assert_eq!(broadcast.player_id, 1);
        assert!(broadcast.full_snapshot);
        assert!(
            !broadcast.changed_entities.iter().any(
                |entity| matches!(entity, VisibleEntity::Drone(drone) if drone.id == drone_id)
            )
        );
        assert!(broadcast.removed_entities.is_empty());
    }

    #[test]
    fn main_tick_loop_declares_collect_execute_apply_persist_order() {
        let mut tick_loop = TickLoop::new(3);

        for phase in MAIN_TICK_PHASES {
            tick_loop.enter(phase);
        }

        tick_loop.finish();
        assert_eq!(tick_loop.phases[0], TickPhase::Collect);
        assert_eq!(tick_loop.phases[1], TickPhase::Execute);
        assert_eq!(tick_loop.phases[2], TickPhase::Apply);
        assert_eq!(tick_loop.phases[3], TickPhase::Persist);
    }

    #[test]
    fn tick_head_increments_monotonically_with_committed_ticks() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let executor = StaticExecutor {
            result: Ok(vec![CommandIntent {
                sequence: 1,
                action: CommandAction::Move {
                    object_id: object_id(drone),
                    direction: Direction::Top,
                },
            }]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        assert_eq!(scheduler.world.tick_head(), 0);
        scheduler.tick();
        assert_eq!(scheduler.tick_counter, 1);
        assert_eq!(scheduler.world.tick_head(), 1);
        scheduler.executor.result = Ok(Vec::new());
        scheduler.tick();

        assert_eq!(scheduler.tick_counter, 2);
        assert_eq!(scheduler.world.tick_head(), 2);
        assert_eq!(scheduler.committer.records[0].tick, 0);
        assert_eq!(scheduler.committer.records[1].tick, 1);
    }

    #[test]
    fn single_command_failure_is_rejected_without_stopping_following_commands() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let executor = StaticExecutor {
            result: Ok(vec![
                CommandIntent {
                    sequence: 1,
                    action: CommandAction::Harvest {
                        object_id: object_id(drone),
                        target_id: 0,
                        resource: None,
                    },
                },
                CommandIntent {
                    sequence: 2,
                    action: CommandAction::Move {
                        object_id: object_id(drone),
                        direction: Direction::Top,
                    },
                },
            ]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();

        assert!(report.committed);
        assert_eq!(report.rejections.len(), 1);
        assert_eq!(report.accepted.len(), 1);
        assert_eq!(report.metrics.rejected_commands, 1);
        assert_eq!(report.metrics.accepted_commands, 1);
        assert_eq!(
            scheduler
                .world
                .app
                .world()
                .entity(drone)
                .get::<Position>()
                .unwrap()
                .y,
            9
        );
    }

    #[test]
    fn executor_error_and_timeout_record_metrics_and_emit_empty_commands() {
        for result in [
            Err(ExecutorError::Error("boom".to_string())),
            Err(ExecutorError::Timeout),
        ] {
            let executor = StaticExecutor { result };
            let mut scheduler = TickScheduler::new(
                create_world(),
                1,
                executor,
                InMemoryTickCommitter::default(),
                InMemoryTickBroadcaster::default(),
            );

            let report = scheduler.tick();

            assert!(report.committed);
            assert!(report.accepted.is_empty());
            assert!(report.rejections.is_empty());
            assert_eq!(scheduler.committer.records[0].commands.len(), 0);
        }
    }

    #[test]
    fn tick_trace_records_commands_state_rejections_and_metrics() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let executor = StaticExecutor {
            result: Ok(vec![
                CommandIntent {
                    sequence: 1,
                    action: CommandAction::Move {
                        object_id: object_id(drone),
                        direction: Direction::Top,
                    },
                },
                CommandIntent {
                    sequence: 2,
                    action: CommandAction::Harvest {
                        object_id: object_id(drone),
                        target_id: 0,
                        resource: None,
                    },
                },
            ]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();
        let trace = &scheduler.committer.records[0];

        assert!(report.committed);
        assert_eq!(trace.commands.len(), 1);
        assert_eq!(trace.rejections.len(), 1);
        assert_eq!(trace.metrics.accepted_commands, 1);
        assert_eq!(trace.metrics.rejected_commands, 1);
        assert_eq!(
            trace.state,
            TickState::capture(scheduler.world.app.world_mut())
        );
        assert_eq!(trace.state_checksum, scheduler.world.state_checksum());
    }

    #[test]
    fn replay_tick_succeeds_from_previous_state_and_recorded_commands() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let previous_state = TickState::capture(world.app.world_mut());
        let executor = StaticExecutor {
            result: Ok(vec![CommandIntent {
                sequence: 1,
                action: CommandAction::Move {
                    object_id: object_id(drone),
                    direction: Direction::Top,
                },
            }]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        scheduler.tick();
        let trace = scheduler.committer.records[0].clone();

        let replayed = replay_tick(&previous_state, &trace).expect("replay should match trace");

        assert_eq!(replayed, trace.state);
    }

    #[test]
    fn replay_tick_fails_when_recorded_state_does_not_match() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let previous_state = TickState::capture(world.app.world_mut());
        let executor = StaticExecutor {
            result: Ok(vec![CommandIntent {
                sequence: 1,
                action: CommandAction::Move {
                    object_id: object_id(drone),
                    direction: Direction::Top,
                },
            }]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        scheduler.tick();
        let mut trace = scheduler.committer.records[0].clone();
        trace.state = previous_state.clone();

        let error =
            replay_tick(&previous_state, &trace).expect_err("replay should detect mismatch");

        assert!(matches!(error, ReplayError::StateMismatch { tick: 0, .. }));
    }

    #[test]
    fn replay_tick_fails_when_resource_ledger_trace_is_tampered() {
        let (previous_state, mut trace) = transfer_to_global_trace();
        trace.resource_ledger.ledger_checksum =
            trace.resource_ledger.ledger_checksum.wrapping_add(1);

        let error =
            replay_tick(&previous_state, &trace).expect_err("replay should detect ledger tamper");

        assert!(matches!(
            error,
            ReplayError::ResourceLedgerMismatch { tick: 0, .. }
        ));
    }

    #[test]
    fn replay_tick_fails_when_resource_ledger_digest_is_tampered() {
        let (previous_state, mut trace) = transfer_to_global_trace();
        trace.resource_ledger.ledger_digest[0] ^= 0x01;

        let error =
            replay_tick(&previous_state, &trace).expect_err("replay should detect digest tamper");

        assert!(matches!(
            error,
            ReplayError::ResourceLedgerMismatch { tick: 0, .. }
        ));
    }

    #[test]
    fn replay_tick_fails_when_resource_ledger_resource_is_tampered() {
        let (previous_state, mut trace) = transfer_to_global_trace();
        trace.resource_ledger.operations[0].resource = "Mineral".to_string();

        let error = replay_tick(&previous_state, &trace)
            .expect_err("replay should detect ledger resource tamper");

        assert!(matches!(
            error,
            ReplayError::ResourceLedgerMismatch { tick: 0, .. }
        ));
    }

    #[test]
    fn replay_tick_fails_when_resource_ledger_operation_is_tampered() {
        let (previous_state, mut trace) = transfer_to_global_trace();
        trace.resource_ledger.operations[0].operation =
            crate::resource_ledger::ResourceOperation::PvEAward;

        let error = replay_tick(&previous_state, &trace)
            .expect_err("replay should detect ledger operation tamper");

        assert!(matches!(
            error,
            ReplayError::ResourceLedgerMismatch { tick: 0, .. }
        ));
    }

    #[test]
    fn replay_tick_accepts_legacy_zero_resource_ledger_digest() {
        let (previous_state, mut trace) = transfer_to_global_trace();
        trace.resource_ledger.previous_ledger_digest = [0; 32];
        trace.resource_ledger.ledger_digest = [0; 32];

        replay_tick(&previous_state, &trace).expect("legacy zero-digest trace should replay");
    }

    #[test]
    fn replay_replays_multiple_traces_in_order() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let initial_state = TickState::capture(world.app.world_mut());
        let executor = StaticExecutor {
            result: Ok(vec![CommandIntent {
                sequence: 1,
                action: CommandAction::Move {
                    object_id: object_id(drone),
                    direction: Direction::Top,
                },
            }]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        scheduler.tick();
        scheduler.executor.result = Ok(vec![CommandIntent {
            sequence: 1,
            action: CommandAction::Move {
                object_id: object_id(drone),
                direction: Direction::Top,
            },
        }]);
        scheduler.tick();

        let replayed = replay(&initial_state, &scheduler.committer.records)
            .expect("trace sequence should replay");

        assert_eq!(replayed, scheduler.committer.records[1].state);
    }

    #[test]
    fn snapshot_restore_preserves_allied_transfer_resources() {
        let mut world = create_world();
        world
            .app
            .world_mut()
            .insert_resource(PendingAlliedTransfers(vec![
                crate::resources::PendingAlliedTransfer {
                    from_player: 1,
                    to_player: 2,
                    resource: "energy".to_string(),
                    amount: 100,
                    deliver_amount: 98,
                    remaining_ticks: 3,
                },
            ]));
        world
            .app
            .world_mut()
            .insert_resource(AlliedTransferCooldowns(indexmap::indexmap! {(1, 2) => 44}));
        world
            .app
            .world_mut()
            .insert_resource(AlliedTransferDailyUsage(indexmap::indexmap! {1 => 100}));
        world
            .app
            .world_mut()
            .insert_resource(AlliedTransferDailyTick(1440));

        let snapshot = WorldSnapshot::capture(world.app.world_mut());
        world
            .app
            .world_mut()
            .insert_resource(PendingAlliedTransfers::default());
        world
            .app
            .world_mut()
            .insert_resource(AlliedTransferCooldowns::default());
        world
            .app
            .world_mut()
            .insert_resource(AlliedTransferDailyUsage::default());
        world
            .app
            .world_mut()
            .insert_resource(AlliedTransferDailyTick::default());

        snapshot.restore(world.app.world_mut());

        assert_eq!(
            world.app.world().resource::<PendingAlliedTransfers>().0[0].deliver_amount,
            98
        );
        assert_eq!(
            world.app.world().resource::<AlliedTransferCooldowns>().0[&(1, 2)],
            44
        );
        assert_eq!(
            world.app.world().resource::<AlliedTransferDailyUsage>().0[&1],
            100
        );
        assert_eq!(
            world.app.world().resource::<AlliedTransferDailyTick>().0,
            1440
        );
    }

    #[test]
    fn commit_failure_rolls_back_world_and_does_not_increment_or_broadcast() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let before = world.state_checksum();
        let executor = StaticExecutor {
            result: Ok(vec![CommandIntent {
                sequence: 1,
                action: CommandAction::Move {
                    object_id: object_id(drone),
                    direction: Direction::Top,
                },
            }]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter {
                fail_count: MAX_COMMIT_ATTEMPTS,
                ..Default::default()
            },
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();

        assert!(!report.committed);
        assert!(!report.broadcasted);
        assert_eq!(scheduler.tick_counter, 0);
        assert_eq!(report.metrics.commit_failures, MAX_COMMIT_ATTEMPTS as u64);
        assert_eq!(scheduler.broadcaster.broadcasts.len(), 0);
        assert_eq!(before, scheduler.world.state_checksum());
        // Entity ID may change after snapshot restore (Bevy 0.16 spawn_empty),
        // so find the drone by query instead of by entity ID.
        let drone_y = {
            let world = scheduler.world.app.world_mut();
            let mut q = world.query_filtered::<&Position, With<Drone>>();
            q.single(world).unwrap().y
        };
        assert_eq!(drone_y, 10);
    }

    #[test]
    fn conservation_imbalance_prevents_commit_and_broadcast() {
        fn imbalanced_ledger_system(
            mut ledger: ResMut<ResourceLedger>,
            current_tick: Res<CurrentTick>,
        ) {
            ledger.record_transfer_amounts(
                current_tick.0,
                Some(1),
                Some(2),
                "Energy",
                10,
                9,
                crate::resource_ledger::ResourceOperation::LocalTransfer,
                0,
                0,
            );
        }

        let mut world = create_world();
        let before = world.state_checksum();
        world.app.add_systems(
            Update,
            imbalanced_ledger_system.before(crate::resource_ledger::resource_ledger_system),
        );
        let executor = StaticExecutor {
            result: Ok(Vec::new()),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();

        assert!(!report.committed);
        assert!(!report.broadcasted);
        assert_eq!(report.metrics.commit_failures, MAX_COMMIT_ATTEMPTS as u64);
        assert_eq!(scheduler.tick_counter, 0);
        assert_eq!(scheduler.world.tick_head(), 0);
        assert_eq!(scheduler.committer.records.len(), 0);
        assert_eq!(scheduler.broadcaster.broadcasts.len(), 0);
        assert_eq!(before, scheduler.world.state_checksum());
    }

    #[test]
    fn conservation_validation_retry_restores_world_snapshot() {
        fn flaky_ledger_system(
            mut attempts: ResMut<LedgerValidationAttempts>,
            mut local_storage: ResMut<PlayerLocalStorage>,
            mut ledger: ResMut<ResourceLedger>,
            current_tick: Res<CurrentTick>,
        ) {
            attempts.0 += 1;
            *local_storage
                .0
                .entry(1)
                .or_default()
                .entry("Energy".to_string())
                .or_default() += 10;
            let delivered = if attempts.0 == 1 { 9 } else { 10 };
            ledger.record_transfer_amounts(
                current_tick.0,
                Some(1),
                Some(2),
                "Energy",
                10,
                delivered,
                crate::resource_ledger::ResourceOperation::LocalTransfer,
                0,
                0,
            );
        }

        let mut world = create_world();
        world
            .app
            .insert_resource(LedgerValidationAttempts::default());
        world.app.add_systems(
            Update,
            flaky_ledger_system.before(crate::resource_ledger::resource_ledger_system),
        );
        let executor = StaticExecutor {
            result: Ok(Vec::new()),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();

        assert!(report.committed);
        assert_eq!(report.metrics.commit_failures, 1);
        assert_eq!(scheduler.tick_counter, 1);
        assert_eq!(scheduler.committer.records.len(), 1);
        assert_eq!(
            scheduler
                .world
                .app
                .world()
                .resource::<LedgerValidationAttempts>()
                .0,
            2
        );
        assert_eq!(
            scheduler
                .world
                .app
                .world()
                .resource::<PlayerLocalStorage>()
                .0
                .get(&1)
                .unwrap()
                .get("Energy"),
            Some(&10)
        );
        assert!(
            scheduler.committer.records[0]
                .resource_ledger
                .conservation_imbalance
                .is_empty()
        );
    }

    #[test]
    fn deploy_activation_is_not_consumed_when_all_commit_attempts_fail() {
        let mut world = create_world();
        let deployments = ActiveDeployments::default();
        deployments.stage_activation(active_deployment(1, 0));
        world.app.world_mut().insert_resource(deployments.clone());
        let executor = StaticExecutor {
            result: Ok(Vec::new()),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter {
                fail_count: MAX_COMMIT_ATTEMPTS,
                ..Default::default()
            },
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();

        assert!(!report.committed);
        assert_eq!(deployments.pending_ready_for_tick(0).len(), 1);
        assert!(deployments.active_for_player(1, 0).is_none());
        assert!(scheduler.committer.records.is_empty());
    }

    #[test]
    fn deploy_activation_is_consumed_only_after_retry_commit_succeeds() {
        let mut world = create_world();
        let deployments = ActiveDeployments::default();
        deployments.stage_activation(active_deployment(1, 0));
        world.app.world_mut().insert_resource(deployments.clone());
        let executor = StaticExecutor {
            result: Ok(Vec::new()),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            EnvironmentRecordingCommitter {
                fail_count: 1,
                ..Default::default()
            },
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();

        assert!(report.committed);
        assert_eq!(report.metrics.commit_failures, 1);
        assert!(deployments.pending_ready_for_tick(0).is_empty());
        assert_eq!(
            deployments.active_for_player(1, 0).unwrap().deploy_id,
            "deploy-tick"
        );
        assert_eq!(
            scheduler.committer.environments[0].deploy_activation_decisions[0].deploy_id,
            "deploy-tick"
        );
    }

    #[test]
    fn commit_failures_retry_three_times_and_restore_snapshot_between_attempts() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let before = world.state_checksum();
        let executor = StaticExecutor {
            result: Ok(vec![CommandIntent {
                sequence: 1,
                action: CommandAction::Move {
                    object_id: object_id(drone),
                    direction: Direction::Top,
                },
            }]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter {
                fail_count: 2,
                ..Default::default()
            },
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();

        assert!(report.committed);
        assert_eq!(report.metrics.commit_failures, 2);
        assert_eq!(scheduler.tick_counter, 1);
        assert_ne!(before, scheduler.world.state_checksum());
        // Entity ID may change after snapshot restore (Bevy 0.16 spawn_empty).
        let drone_y = {
            let world = scheduler.world.app.world_mut();
            let mut q = world.query_filtered::<&Position, With<Drone>>();
            q.single(world).unwrap().y
        };
        assert_eq!(drone_y, 9);
    }

    #[test]
    fn multiplayer_commit_retry_reuses_collect_results() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let calls = Arc::new(Mutex::new(0));
        let mut executors: HashMap<PlayerId, Box<dyn PlayerExecutor>> = HashMap::new();
        executors.insert(
            1,
            Box::new(CountingExecutor {
                calls: calls.clone(),
                result: Ok(vec![CommandIntent {
                    sequence: 1,
                    action: CommandAction::Move {
                        object_id: object_id(drone),
                        direction: Direction::Top,
                    },
                }]),
            }),
        );
        let mut scheduler = MultiPlayerTickScheduler::new(
            world,
            executors,
            InMemoryTickCommitter {
                fail_count: 2,
                ..Default::default()
            },
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();

        assert!(report.committed);
        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(report.metrics.commit_failures, 2);
        assert!(scheduler.collect_cache.is_none());
    }

    #[test]
    fn three_abandoned_ticks_enter_degraded_mode_and_disable_mcp_deploy() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let executor = StaticExecutor {
            result: Ok(vec![CommandIntent {
                sequence: 1,
                action: CommandAction::Move {
                    object_id: object_id(drone),
                    direction: Direction::Top,
                },
            }]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter {
                fail_count: MAX_COMMIT_ATTEMPTS * DEGRADED_ABANDON_THRESHOLD,
                ..Default::default()
            },
            InMemoryTickBroadcaster::default(),
        );

        for _ in 0..DEGRADED_ABANDON_THRESHOLD {
            assert!(!scheduler.tick().committed);
        }

        assert!(scheduler.degraded_mode.enabled);
        assert!(scheduler.degraded_mode.join_lock);
        assert!(!scheduler.degraded_mode.mcp_deploy_enabled);
    }

    #[test]
    fn broadcast_failure_does_not_rollback_commit_or_tick_increment() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let executor = StaticExecutor {
            result: Ok(vec![CommandIntent {
                sequence: 1,
                action: CommandAction::Move {
                    object_id: object_id(drone),
                    direction: Direction::Top,
                },
            }]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster {
                fail_next: true,
                ..Default::default()
            },
        );

        let report = scheduler.tick();

        assert!(report.committed);
        assert!(!report.broadcasted);
        assert_eq!(scheduler.tick_counter, 1);
        assert_eq!(scheduler.committer.records.len(), 1);
        assert_eq!(scheduler.broadcaster.broadcasts.len(), 0);
        assert_eq!(
            scheduler
                .world
                .app
                .world()
                .entity(drone)
                .get::<Position>()
                .unwrap()
                .y,
            9
        );
    }

    #[test]
    fn spawn_drone_command_materializes_after_phase_2b() {
        let mut world = create_world();
        let actor = world.spawn_drone(1, 9, 10, vec![BodyPart::Move]);
        let spawn = spawn_structure(&mut world, 1, 10, 10);
        let executor = StaticExecutor {
            result: Ok(vec![CommandIntent {
                sequence: 1,
                action: CommandAction::Spawn {
                    object_id: object_id(actor),
                    spawn_id: object_id(spawn),
                    body_parts: vec![BodyPart::Move],
                },
            }]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();

        assert!(report.committed);
        assert_eq!(report.accepted.len(), 1);
        assert_eq!(
            scheduler
                .world
                .app
                .world()
                .resource::<PendingSpawnQueue>()
                .0
                .len(),
            0
        );
        assert_eq!(drone_count(&mut scheduler.world), 2);
    }

    #[test]
    fn fcfs_harvest_conflict_rejects_source_empty_with_refund_and_structured_detail() {
        let mut world = create_world();
        let first = world.spawn_drone(1, 24, 25, vec![BodyPart::Work, BodyPart::Carry]);
        let second = world.spawn_drone(2, 26, 25, vec![BodyPart::Work, BodyPart::Carry]);
        let source = world
            .app
            .world_mut()
            .query::<(Entity, &mut Source)>()
            .iter_mut(world.app.world_mut())
            .map(|(entity, mut source)| {
                source.capacity = 2;
                source.ticks_to_regeneration = 300;
                entity
            })
            .next()
            .expect("expected source");
        let source_id = object_id(source);

        let execution = execute_deterministic(
            &mut world,
            vec![
                raw_harvest(1, 1, 1, object_id(first), source_id),
                raw_harvest(2, 2, 1, object_id(second), source_id),
            ],
        );

        assert_eq!(execution.commands.len(), 1);
        assert_eq!(execution.rejections.len(), 1);
        assert_eq!(execution.next_tick_fuel_credit, 5_000);
        let rejection = &execution.rejections[0];
        assert_eq!(rejection.rejection, RejectionReason::SourceEmpty);
        assert_eq!(rejection.detail["reason"], "InsufficientResource");
        assert_eq!(rejection.detail["internal_reason"], "SourceEmpty");
        assert_eq!(rejection.detail["conflict"], "first_come_first_served");
        assert_eq!(rejection.detail["refund_policy"]["fuel_percent"], 50);
        assert_eq!(rejection.detail["target_id"], source_id);
    }

    #[test]
    fn fcfs_build_conflict_rejects_tile_occupied_with_refund_and_structured_detail() {
        let mut world = create_world();
        let first = world.spawn_drone(1, 10, 10, vec![BodyPart::Work]);
        let second = world.spawn_drone(2, 12, 10, vec![BodyPart::Work]);

        let execution = execute_deterministic(
            &mut world,
            vec![
                raw_build(1, 1, 1, object_id(first), 11, 10),
                raw_build(2, 2, 1, object_id(second), 11, 10),
            ],
        );

        assert_eq!(execution.commands.len(), 1);
        assert_eq!(execution.rejections.len(), 1);
        assert_eq!(execution.next_tick_fuel_credit, 5_000);
        let rejection = &execution.rejections[0];
        assert_eq!(rejection.rejection, RejectionReason::TileOccupied);
        assert_eq!(rejection.detail["reason"], "PositionOccupied");
        assert_eq!(rejection.detail["internal_reason"], "TileOccupied");
        assert_eq!(rejection.detail["conflict"], "first_come_first_served");
        assert_eq!(rejection.detail["refund_policy"]["fuel_percent"], 50);
        assert_eq!(rejection.detail["position"]["x"], 11);
        assert_eq!(rejection.detail["position"]["y"], 10);
    }

    #[test]
    fn fcfs_transfer_conflict_rejects_target_full_with_refund_and_structured_detail() {
        let mut world = create_world();
        let first = world.spawn_drone(1, 10, 10, vec![BodyPart::Carry]);
        let second = world.spawn_drone(2, 12, 10, vec![BodyPart::Carry]);
        let target = world.spawn_drone(3, 11, 10, vec![BodyPart::Carry]);
        for drone in [first, second] {
            world
                .app
                .world_mut()
                .entity_mut(drone)
                .get_mut::<Drone>()
                .unwrap()
                .carry
                .insert("Energy".to_string(), 40);
        }

        let execution = execute_deterministic(
            &mut world,
            vec![
                raw_transfer(1, 1, 1, object_id(first), object_id(target), 30),
                raw_transfer(2, 2, 1, object_id(second), object_id(target), 30),
            ],
        );

        assert_eq!(execution.commands.len(), 1);
        assert_eq!(execution.rejections.len(), 1);
        assert_eq!(execution.next_tick_fuel_credit, 5_000);
        let rejection = &execution.rejections[0];
        assert_eq!(rejection.rejection, RejectionReason::TargetFull);
        assert_eq!(rejection.detail["reason"], "InsufficientResource");
        assert_eq!(rejection.detail["internal_reason"], "TargetFull");
        assert_eq!(rejection.detail["conflict"], "first_come_first_served");
        assert_eq!(rejection.detail["refund_policy"]["fuel_percent"], 50);
        assert_eq!(rejection.detail["target_id"], object_id(target));
        assert_eq!(rejection.detail["amount"], 30);
    }

    fn raw_harvest(
        player_id: PlayerId,
        sequence: u32,
        tick: Tick,
        object_id: u64,
        target_id: u64,
    ) -> RawCommand {
        RawCommand {
            player_id,
            tick,
            source: CommandSource::Wasm,
            auth: CommandAuth::server_injected(CommandSource::Wasm, player_id, tick, tick),
            sequence,
            action: CommandAction::Harvest {
                object_id,
                target_id,
                resource: None,
            },
        }
    }

    fn raw_build(
        player_id: PlayerId,
        sequence: u32,
        tick: Tick,
        object_id: u64,
        x: i32,
        y: i32,
    ) -> RawCommand {
        RawCommand {
            player_id,
            tick,
            source: CommandSource::Wasm,
            auth: CommandAuth::server_injected(CommandSource::Wasm, player_id, tick, tick),
            sequence,
            action: CommandAction::Build {
                object_id,
                x,
                y,
                structure: StructureType::Extension,
            },
        }
    }

    fn raw_transfer(
        player_id: PlayerId,
        sequence: u32,
        tick: Tick,
        object_id: u64,
        target_id: u64,
        amount: u32,
    ) -> RawCommand {
        RawCommand {
            player_id,
            tick,
            source: CommandSource::Wasm,
            auth: CommandAuth::server_injected(CommandSource::Wasm, player_id, tick, tick),
            sequence,
            action: CommandAction::Transfer {
                object_id,
                target_id,
                resource: "Energy".to_string(),
                amount,
            },
        }
    }
}
