use std::{collections::HashMap, thread, time::Instant};

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::command::{
    CommandAction, CommandIntent, CommandRejection, CommandSource, MAX_FUEL, ObjectId, RawCommand,
    RefundAccumulator, Tick, apply_command, collect_command_intents, sort_raw_commands,
    validate_command,
};
use crate::components::*;
use crate::resource_ledger::ResourceLedger;
use crate::resources::{
    CurrentTick, PendingGlobalTransfers, PlayerGlobalStorage, PlayerLocalStorage, ResourceCost,
};
use crate::scheduler::{SYSTEM_MANIFEST, manifest_hash};
use crate::security::{SecurityAlert, SecurityAuditor};
use crate::sim::{SnapshotConfig, collect_snapshots};
use crate::systems::{PendingCombat, PendingSpawnQueue, RoomDroneCounts};
use crate::world::{SwarmWorld, WorldConfig};

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutorError {
    Error(String),
    Timeout,
}

pub trait PlayerExecutor: Send {
    fn collect(&mut self, snapshot: TickSnapshot) -> Result<Vec<CommandIntent>, ExecutorError>;
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
        }
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
}

impl TickTrace {
    pub fn accepted(&self) -> &[RawCommand] {
        &self.commands
    }
}

fn system_manifest_hash() -> [u8; 32] {
    *manifest_hash(SYSTEM_MANIFEST).as_bytes()
}

fn action_manifest_hash() -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for action in crate::command::CORE_COMMAND_ACTIONS {
        hasher.update(action.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

pub type TickCommitRecord = TickTrace;

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
    pub player_id: PlayerId,
    pub accepted: Vec<RawCommand>,
    pub rejections: Vec<CommandRejection>,
    pub state_checksum: u64,
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
        self.fuel_consumed = self
            .fuel_consumed
            .saturating_add(total_commands.saturating_mul(COMMAND_REJECTION_FUEL_COST));
        for rejection in rejections {
            if let Some(refund_fuel) = rejection
                .detail
                .get("refund_fuel")
                .and_then(serde_json::Value::as_u64)
            {
                if refund_fuel > 0 {
                    self.refund_events += 1;
                    self.refund_fuel += refund_fuel;
                }
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
}

impl CollectCache {
    fn raw_commands(&self) -> Vec<RawCommand> {
        serial_execution_queue(
            self.by_player
                .iter()
                .map(|(&player_id, commands)| CollectedPlayerCommands {
                    player_id,
                    commands: commands.clone(),
                })
                .collect(),
        )
    }

    fn matches(&self, tick: Tick, state_checksum: u64) -> bool {
        self.tick == tick && self.state_checksum == state_checksum
    }
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
        accumulator.fuel_consumed += total_commands.saturating_mul(COMMAND_REJECTION_FUEL_COST);
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
}

pub fn seeded_player_shuffle(
    mut players: Vec<PlayerId>,
    tick: Tick,
    state_checksum: u64,
) -> Vec<PlayerId> {
    let mut seed_input = Vec::with_capacity(16);
    seed_input.extend_from_slice(&tick.to_le_bytes());
    seed_input.extend_from_slice(&state_checksum.to_le_bytes());
    let mut hasher = blake3::Hasher::new();
    hasher.update(&seed_input);
    let mut reader = hasher.finalize_xof();

    for i in 0..players.len() {
        let remaining = players.len() - i;
        let mut bytes = [0_u8; 8];
        reader.fill(&mut bytes);
        let offset = (u64::from_le_bytes(bytes) as usize) % remaining;
        players.swap(i, i + offset);
    }

    players
}

fn collect_player_commands<E: PlayerExecutor + ?Sized>(
    tick: Tick,
    player_id: PlayerId,
    state_checksum: u64,
    executor: &mut E,
) -> PlayerCollectResult {
    let snapshot = TickSnapshot {
        tick,
        player_id,
        state_checksum,
    };
    let mut metrics = TickMetrics::default();
    let intents = match executor.collect(snapshot) {
        Ok(intents) => intents,
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
    }
}

fn serial_execution_queue(collected: Vec<CollectedPlayerCommands>) -> Vec<RawCommand> {
    let mut queue = Vec::new();
    for collected in collected {
        queue.extend(collected.commands);
    }
    sort_raw_commands(&mut queue);
    queue
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

        // S1: Check COLLECT cache before executing WASM.
        // If redb commit failed last tick and we're retrying with the same state,
        // reuse cached commands to avoid double fuel charges.
        let cache_hit = self
            .collect_cache
            .as_ref()
            .filter(|cache| cache.matches(tick, state_checksum));

        let collect_started_at = Instant::now();
        tick_loop.enter(TickPhase::Collect);
        let raw_commands = if let Some(cache) = cache_hit {
            debug_assert!(
                cache.fuel_metrics.fuel_consumed <= MAX_FUEL,
                "cached COLLECT fuel exceeds MAX_FUEL"
            );
            cache.raw_commands()
        } else {
            let mut results = thread::scope(|scope| {
                self.executors
                    .iter_mut()
                    .map(|(&player_id, executor)| {
                        scope.spawn(move || {
                            collect_player_commands(
                                tick,
                                player_id,
                                state_checksum,
                                executor.as_mut(),
                            )
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|handle| handle.join().expect("player executor thread panicked"))
                    .collect::<Vec<_>>()
            });
            results.sort_by_key(|result| result.player_id);

            for result in &results {
                self.metrics.executor_errors += result.metrics.executor_errors;
                self.metrics.executor_timeouts += result.metrics.executor_timeouts;
            }

            // Build CollectCache for potential retry
            let mut by_player: indexmap::IndexMap<PlayerId, Vec<RawCommand>> =
                indexmap::IndexMap::new();
            let mut collect_fuel_metrics = TickMetrics::default();
            for result in &results {
                by_player.insert(result.player_id, result.commands.clone());
                collect_fuel_metrics.add(&result.metrics);
            }
            self.collect_cache = Some(CollectCache {
                tick,
                state_checksum,
                by_player,
                fuel_metrics: collect_fuel_metrics,
            });

            let collected = results
                .into_iter()
                .map(|result| CollectedPlayerCommands {
                    player_id: result.player_id,
                    commands: result.commands,
                })
                .collect::<Vec<_>>();
            serial_execution_queue(collected)
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
                action_manifest_hash: action_manifest_hash(),
                security_alerts: Vec::new(),
                trace_events: std::mem::take(
                    &mut self
                        .world
                        .app
                        .world_mut()
                        .resource_mut::<TickTraceEventLog>()
                        .events,
                ),
            };
            trace.security_alerts = SecurityAuditor::default().audit_trace(&trace, None);
            let environment = ReplayEnvironment::capture(self.world.app.world());
            last_accepted = accepted;
            last_rejections = rejections;
            last_security_alerts = trace.security_alerts.clone();
            tick_loop.enter(TickPhase::Persist);
            if self
                .committer
                .commit_with_environment(trace.clone(), environment)
                .is_ok()
            {
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
        let broadcast = TickBroadcast {
            tick,
            player_id: 0,
            accepted: trace.commands.clone(),
            rejections: trace.rejections.clone(),
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
        let snapshot = TickSnapshot {
            tick,
            player_id: self.player_id,
            state_checksum: self.world.state_checksum(),
        };
        let collect_started_at = Instant::now();
        tick_loop.enter(TickPhase::Collect);
        let intents = match self.executor.collect(snapshot) {
            Ok(intents) => intents,
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
        if collect_duration_ms > COLLECT_TIMEOUT_MS {
            self.metrics.collect_timeouts += 1;
        }

        let world_snapshot = WorldSnapshot::capture(self.world.app.world_mut());
        let mut raw_commands =
            collect_command_intents(self.player_id, tick, CommandSource::Wasm, intents)
                .unwrap_or_default();
        sort_raw_commands(&mut raw_commands);

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
                action_manifest_hash: action_manifest_hash(),
                security_alerts: Vec::new(),
                trace_events: std::mem::take(
                    &mut self
                        .world
                        .app
                        .world_mut()
                        .resource_mut::<TickTraceEventLog>()
                        .events,
                ),
            };
            trace.security_alerts = SecurityAuditor::default().audit_trace(&trace, None);
            let environment = ReplayEnvironment::capture(self.world.app.world());
            last_accepted = accepted;
            last_rejections = rejections;
            last_security_alerts = trace.security_alerts.clone();
            tick_loop.enter(TickPhase::Persist);
            if self
                .committer
                .commit_with_environment(trace.clone(), environment)
                .is_ok()
            {
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
        let broadcast = TickBroadcast {
            tick,
            player_id: self.player_id,
            accepted: trace.commands.clone(),
            rejections: trace.rejections.clone(),
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
    fn atomic_commit(&mut self, writes: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), CommitError>;
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

pub fn tick_trace_writes(trace: &TickTrace) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommitError> {
    let environment = ReplayEnvironment {
        mods_lock: ModsLock::default(),
        world_config: WorldConfigSnapshot {
            config: WorldConfig::default(),
        },
    };
    tick_trace_writes_with_environment(trace, &environment)
}

pub fn tick_trace_writes_with_environment(
    trace: &TickTrace,
    environment: &ReplayEnvironment,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommitError> {
    fn encode<T: Serialize>(value: &T, label: &str) -> Result<Vec<u8>, CommitError> {
        serde_json::to_vec(value)
            .map_err(|error| CommitError::Failed(format!("encode {label}: {error}")))
    }

    let mut writes = vec![
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
            tick_key(trace.tick, "security_alerts"),
            encode(&trace.security_alerts, "tick security alerts")?,
        ),
    ];

    if trace.tick == 0 || trace.tick % DEFAULT_KEYFRAME_INTERVAL == 0 {
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
}

const COMMAND_REJECTION_FUEL_COST: u64 = 10_000;

pub fn execute_deterministic(
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
    let _snapshots = collect_snapshots(world.app.world_mut(), &player_ids, tick, &snapshot_config);

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
        match validate_command(world.app.world_mut(), raw.clone()) {
            Ok(validated) => match apply_command(world.app.world_mut(), validated) {
                Ok(()) => accepted.push(raw),
                Err(rejection) => {
                    let refund_fuel =
                        refunds.record_rejection(&raw, &rejection, COMMAND_REJECTION_FUEL_COST);
                    rejections.push(command_rejection_with_refund(raw, rejection, refund_fuel));
                }
            },
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
    DeterministicExecution {
        commands: accepted,
        rejections,
        next_tick_fuel_credit: refunds.next_tick_fuel_credit,
        state,
        state_checksum,
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

pub fn replay_tick(
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

    Ok(replayed.state)
}

pub fn replay(initial_state: &TickState, traces: &[TickTrace]) -> Result<TickState, ReplayError> {
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
        | CommandAction::AlliedTransfer { .. } => {}
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
        let payload = serde_json::to_vec(&event)
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
            if let Some(structure) = &snapshot.structure {
                if let (Some(owner), Some(energy)) = (structure.owner, structure.energy) {
                    *totals.entry(owner).or_default() += energy as u64;
                }
            }
        }

        totals
    }

    pub fn capture(world: &mut World) -> Self {
        let entity_ids: Vec<Entity> = world.query::<Entity>().iter(world).collect();
        let entities = entity_ids
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
            resource_ledger: world.resource::<ResourceLedger>().clone(),
            starting_resources_granted: world
                .resource::<crate::systems::StartingResourcesGranted>()
                .clone(),
            player_first_spawn_tick: world
                .resource::<crate::systems::PlayerFirstSpawnTick>()
                .clone(),
            event_log: world.resource::<EventLog>().clone(),
            entity_total_count: allocator.len(),
            entity_alive_count: allocator.count_spawned(),
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
    struct FakeAtomicStore {
        writes: HashMap<Vec<u8>, Vec<u8>>,
        fail_next: bool,
    }

    impl AtomicTickStore for FakeAtomicStore {
        fn atomic_commit(&mut self, writes: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), CommitError> {
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
            action_manifest_hash: action_manifest_hash(),
            security_alerts: Vec::new(),
            trace_events: Vec::new(),
        }
    }

    #[test]
    fn redb_tick_committer_writes_required_tick_keys_atomically() {
        let trace = sample_trace();
        let mut committer = RedbTickCommitter::new(FakeAtomicStore::default());

        committer
            .commit(trace)
            .expect("atomic tick commit should succeed");
        let store = committer.into_inner();

        assert_eq!(store.writes.len(), 6);
        for suffix in [
            "state",
            "commands",
            "rejections",
            "metrics",
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
        assert_eq!(scheduler.broadcaster.broadcasts.len(), 1);
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
    fn serial_execution_queue_orders_players_by_id_and_commands_by_sequence() {
        let queue = serial_execution_queue(vec![
            CollectedPlayerCommands {
                player_id: 20,
                commands: vec![
                    raw_harvest(20, 3, 1, 300, 400),
                    raw_harvest(20, 1, 1, 301, 401),
                ],
            },
            CollectedPlayerCommands {
                player_id: 10,
                commands: vec![raw_harvest(10, 2, 1, 100, 200)],
            },
        ]);

        assert_eq!(
            queue
                .iter()
                .map(|command| (command.player_id, command.sequence))
                .collect::<Vec<_>>(),
            vec![(10, 2), (20, 1), (20, 3)]
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
            action_manifest_hash: action_manifest_hash(),
            security_alerts: Vec::new(),
            trace_events: Vec::new(),
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
            action_manifest_hash: action_manifest_hash(),
            security_alerts: Vec::new(),
            trace_events: Vec::new(),
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
        assert_eq!(rows[0].fuel_consumed, 10_000);
        assert_eq!(rows[1].fuel_consumed, 10_000);
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
        assert_eq!(metrics.fuel_consumed, 20_000);
        assert_eq!(metrics.refund_events, 1);
        assert_eq!(metrics.refund_fuel, 5_000);
        assert_eq!(metrics.command_rejection_rate(), 0.5);
        assert_eq!(metrics.refund_abuse_rate(), 0.5);
    }

    #[test]
    fn normal_tick_collects_executes_commits_broadcasts_and_increments() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let executor = StaticExecutor {
            result: Ok(vec![CommandIntent {
                sequence: 2,
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

        let report = scheduler.tick();

        assert!(report.committed);
        assert!(report.broadcasted);
        assert_eq!(scheduler.tick_counter, 1);
        assert_eq!(scheduler.committer.records.len(), 1);
        assert_eq!(scheduler.broadcaster.broadcasts.len(), 1);
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
        let spawn = spawn_structure(&mut world, 1, 10, 10);
        let executor = StaticExecutor {
            result: Ok(vec![CommandIntent {
                sequence: 1,
                action: CommandAction::Spawn {
                    object_id: object_id(spawn),
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
        assert_eq!(drone_count(&mut scheduler.world), 1);
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
        assert_eq!(rejection.detail["reason"], "SourceEmpty");
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
        assert_eq!(rejection.detail["reason"], "TileOccupied");
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
        assert_eq!(rejection.detail["reason"], "TargetFull");
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
            auth: CommandAuth {
                source: CommandSource::Wasm,
                player_id,
                tick_submitted: tick,
                tick_target: tick,
            },
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
            auth: CommandAuth {
                source: CommandSource::Wasm,
                player_id,
                tick_submitted: tick,
                tick_target: tick,
            },
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
            auth: CommandAuth {
                source: CommandSource::Wasm,
                player_id,
                tick_submitted: tick,
                tick_target: tick,
            },
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
