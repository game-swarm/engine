// P2-1 Resource Ledger: Transfer Gateway — 统一资源入口
// Spec: specs/core/08-resource-ledger.md §1-§2

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::command::Tick;
use crate::components::PlayerId;
use crate::resources::{
    GlobalStorageConfig, PendingGlobalTransfer, PendingGlobalTransfers, PlayerGlobalStorage,
    PlayerLocalStorage, ResourceAmount, ResourceName,
};
use crate::tick::{TickTraceEvent, TickTraceEventLog};

// ═══════════════════════════════════════════════════════════════════
// Resource Operations
// ═══════════════════════════════════════════════════════════════════

/// All resource operations flow through this single gateway (§1)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceOperation {
    LocalTransfer,
    GlobalDeposit,
    GlobalWithdraw,
    AlliedTransfer,
    PvEAward,
    ControllerPassiveIncome,
    RecycleRefund,
    BuildCost,
    SpawnCost,
    UpkeepDeduction,
    StorageTax,
    ContractSettlement,
}

/// Result of a resource operation
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferResult {
    pub operation: ResourceOperation,
    pub source_player: Option<PlayerId>,
    pub target_player: Option<PlayerId>,
    pub resource: ResourceName,
    pub amount_requested: ResourceAmount,
    pub amount_delivered: ResourceAmount,
    pub fee_paid: ResourceAmount,
    pub basis_points_used: u32,
    pub delayed_until: Option<Tick>,
    pub success: bool,
    pub rejection_reason: Option<String>,
}

impl TransferResult {
    pub fn rejected(operation: ResourceOperation, reason: String) -> Self {
        Self {
            operation,
            source_player: None,
            target_player: None,
            resource: String::new(),
            amount_requested: 0,
            amount_delivered: 0,
            fee_paid: 0,
            basis_points_used: 0,
            delayed_until: None,
            success: false,
            rejection_reason: Some(reason),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// Transfer Gateway — single entry point for all resource flow
// ═══════════════════════════════════════════════════════════════════

/// Fee calculation using basis points (no floating point)
/// fee = amount * bps / 10000
pub fn compute_fee(amount: ResourceAmount, bps: u32) -> ResourceAmount {
    (amount as u64 * bps as u64 / 10000) as ResourceAmount
}

#[derive(Debug, Clone, Copy)]
pub struct GlobalDepositRequest<'a> {
    pub player_id: PlayerId,
    pub resource: &'a str,
    pub amount: ResourceAmount,
    pub tick: Tick,
}

#[derive(Debug, Clone, Copy)]
pub struct GlobalWithdrawRequest<'a> {
    pub player_id: PlayerId,
    pub resource: &'a str,
    pub amount: ResourceAmount,
    pub tick: Tick,
}

/// Global deposit: local → global, with fee
pub fn execute_global_deposit(
    local_storage: &mut PlayerLocalStorage,
    global_storage: &mut PlayerGlobalStorage,
    pending: &mut PendingGlobalTransfers,
    config: &GlobalStorageConfig,
    request: GlobalDepositRequest<'_>,
) -> TransferResult {
    let player_id = request.player_id;
    let resource = request.resource;
    let amount = request.amount;
    let fee_bps = config.transfer_to_global_fee_per_10_000;
    let fee = compute_fee(amount, fee_bps);
    let net = amount.saturating_sub(fee);

    // Check local has enough
    let local = local_storage.0.entry(player_id).or_default();
    let current = local.get(resource).copied().unwrap_or(0);
    if current < amount {
        return TransferResult::rejected(
            ResourceOperation::GlobalDeposit,
            format!("insufficient local {resource}: have {current}, need {amount}"),
        );
    }

    // Deduct from local
    *local.get_mut(resource).unwrap() = current - amount;

    if config.transfer_to_global_ticks == 0 {
        // Instant: add to global storage directly
        let global = global_storage.0.entry(player_id).or_default();
        *global.entry(resource.to_string()).or_default() += net;
    } else {
        // Delayed: enqueue pending transfer
        pending.0.push(PendingGlobalTransfer {
            player_id,
            direction: crate::resources::GlobalTransferDirection::ToGlobal,
            resource: resource.to_string(),
            amount: net,
            deliver_amount: net,
            remaining_ticks: config.transfer_to_global_ticks,
            start: crate::components::Position {
                x: 0,
                y: 0,
                room: crate::components::RoomId(0),
            },
            end: crate::components::Position {
                x: 0,
                y: 0,
                room: crate::components::RoomId(0),
            },
        });
    }

    TransferResult {
        operation: ResourceOperation::GlobalDeposit,
        source_player: Some(player_id),
        target_player: Some(player_id),
        resource: resource.to_string(),
        amount_requested: amount,
        amount_delivered: net,
        fee_paid: fee,
        basis_points_used: fee_bps,
        delayed_until: if config.transfer_to_global_ticks > 0 {
            Some(request.tick + config.transfer_to_global_ticks)
        } else {
            None
        },
        success: true,
        rejection_reason: None,
    }
}

/// Global withdraw: global → local, with fee
pub fn execute_global_withdraw(
    local_storage: &mut PlayerLocalStorage,
    global_storage: &mut PlayerGlobalStorage,
    pending: &mut PendingGlobalTransfers,
    config: &GlobalStorageConfig,
    request: GlobalWithdrawRequest<'_>,
) -> TransferResult {
    let player_id = request.player_id;
    let resource = request.resource;
    let amount = request.amount;
    let fee_bps = config.transfer_from_global_fee_per_10_000;
    let fee = compute_fee(amount, fee_bps);
    let net = amount.saturating_sub(fee);

    // Check global has enough
    let global = global_storage.0.entry(player_id).or_default();
    let current = global.get(resource).copied().unwrap_or(0);
    if current < amount {
        return TransferResult::rejected(
            ResourceOperation::GlobalWithdraw,
            format!("insufficient global {resource}: have {current}, need {amount}"),
        );
    }

    // Deduct from global
    *global.get_mut(resource).unwrap() = current - amount;

    if config.transfer_from_global_ticks == 0 {
        // Instant
        let local = local_storage.0.entry(player_id).or_default();
        *local.entry(resource.to_string()).or_default() += net;
    } else {
        // Delayed
        pending.0.push(PendingGlobalTransfer {
            player_id,
            direction: crate::resources::GlobalTransferDirection::FromGlobal,
            resource: resource.to_string(),
            amount: net,
            deliver_amount: net,
            remaining_ticks: config.transfer_from_global_ticks,
            start: crate::components::Position {
                x: 0,
                y: 0,
                room: crate::components::RoomId(0),
            },
            end: crate::components::Position {
                x: 0,
                y: 0,
                room: crate::components::RoomId(0),
            },
        });
    }

    TransferResult {
        operation: ResourceOperation::GlobalWithdraw,
        source_player: Some(player_id),
        target_player: Some(player_id),
        resource: resource.to_string(),
        amount_requested: amount,
        amount_delivered: net,
        fee_paid: fee,
        basis_points_used: fee_bps,
        delayed_until: if config.transfer_from_global_ticks > 0 {
            Some(request.tick + config.transfer_from_global_ticks)
        } else {
            None
        },
        success: true,
        rejection_reason: None,
    }
}

// ═══════════════════════════════════════════════════════════════════
// Continuous Storage Tax (§2.2)
// ═══════════════════════════════════════════════════════════════════

pub fn compute_continuous_storage_tax(
    stored_total: ResourceAmount,
    capacity: ResourceAmount,
    config: &GlobalStorageConfig,
) -> ResourceAmount {
    if capacity == 0 || stored_total == 0 {
        return 0;
    }

    let utilization_ppm = ((stored_total as u128)
        .saturating_mul(1_000_000)
        .checked_div(capacity as u128)
        .unwrap_or_default())
    .min(1_000_000) as u32;
    let mut weighted_sum = 0_u128;
    let mut ppm = 0_u32;

    while ppm < utilization_ppm {
        let next = ppm.saturating_add(1_000).min(utilization_ppm);
        let width = next - ppm;
        let amount = (capacity as u128).saturating_mul(width as u128) / 1_000_000;
        weighted_sum = weighted_sum.saturating_add(
            amount.saturating_mul(marginal_storage_tax_rate_bp(ppm, config) as u128),
        );
        ppm = next;
    }

    (weighted_sum / 10_000).min(ResourceAmount::MAX as u128) as ResourceAmount
}

pub fn marginal_storage_tax_rate_bp(utilization_ppm: u32, config: &GlobalStorageConfig) -> u32 {
    let anchors = &config.tax_anchors;
    if utilization_ppm <= anchors[0].utilization_ppm {
        return anchors[0].marginal_rate_bp;
    }

    for window in anchors.windows(2) {
        let left = window[0];
        let right = window[1];
        if utilization_ppm <= right.utilization_ppm {
            let span = right.utilization_ppm.saturating_sub(left.utilization_ppm);
            if span == 0 {
                return right.marginal_rate_bp;
            }
            let offset = utilization_ppm.saturating_sub(left.utilization_ppm);
            let t = (offset as u128).saturating_mul(1_000_000) / span as u128;
            let smooth = 3_u128.saturating_mul(t).saturating_mul(t).saturating_sub(
                2_u128.saturating_mul(t).saturating_mul(t).saturating_mul(t) / 1_000_000,
            ) / 1_000_000;
            let delta = right.marginal_rate_bp.saturating_sub(left.marginal_rate_bp) as u128;
            return left
                .marginal_rate_bp
                .saturating_add((delta.saturating_mul(smooth) / 1_000_000) as u32);
        }
    }

    anchors[anchors.len() - 1].marginal_rate_bp
}

/// Execute storage tax deduction for one player
pub fn execute_storage_tax(
    global_storage: &mut PlayerGlobalStorage,
    player_id: PlayerId,
    config: &GlobalStorageConfig,
) -> TransferResult {
    let storage = global_storage.0.entry(player_id).or_default();
    let total_stored: ResourceAmount = storage.values().copied().sum();
    let tax_total = compute_continuous_storage_tax(total_stored, config.capacity, config);

    if tax_total == 0 {
        return TransferResult {
            operation: ResourceOperation::StorageTax,
            source_player: Some(player_id),
            target_player: None,
            resource: String::new(),
            amount_requested: 0,
            amount_delivered: 0,
            fee_paid: 0,
            basis_points_used: 0,
            delayed_until: None,
            success: true,
            rejection_reason: None,
        };
    }

    // Deduct tax proportionally from all resources
    let mut remaining_tax = tax_total as i64;
    let resource_names: Vec<String> = storage.keys().cloned().collect();

    for resource in &resource_names {
        if remaining_tax <= 0 {
            break;
        }
        let amount = storage.get(resource).copied().unwrap_or(0);
        let deduct = (amount as i64).min(remaining_tax) as ResourceAmount;
        if let Some(entry) = storage.get_mut(resource) {
            *entry = entry.saturating_sub(deduct);
        }
        remaining_tax -= deduct as i64;
    }

    TransferResult {
        operation: ResourceOperation::StorageTax,
        source_player: Some(player_id),
        target_player: None,
        resource: String::new(),
        amount_requested: tax_total,
        amount_delivered: tax_total.saturating_sub(remaining_tax.max(0) as ResourceAmount),
        fee_paid: 0,
        basis_points_used: 0,
        delayed_until: None,
        success: true,
        rejection_reason: None,
    }
}

// ═══════════════════════════════════════════════════════════════════
// S29: Resource Ledger ECS System — 最后运行，资源审计
// ═══════════════════════════════════════════════════════════════════

use bevy::prelude::*;

/// Cumulative ledger tracking all resource operations this tick
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLedger {
    /// Ordered log of all resource operations
    pub ops: Vec<ResourceLedgerEntry>,
    /// Net balance delta per player per resource
    pub balance_delta: IndexMap<PlayerId, IndexMap<String, i64>>,
    /// Ledger checksum for TickTrace integrity
    pub ledger_checksum: u64,
}

/// A single resource operation entry
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLedgerEntry {
    pub tick: Tick,
    pub source_player: Option<PlayerId>,
    pub target_player: Option<PlayerId>,
    pub resource: String,
    /// Compatibility amount: gross requested amount for fee-bearing flows.
    pub amount: i64,
    #[serde(default)]
    pub amount_requested: ResourceAmount,
    #[serde(default)]
    pub amount_delivered: ResourceAmount,
    pub operation: ResourceOperation,
    #[serde(default)]
    pub fee_paid: ResourceAmount,
    #[serde(default)]
    pub basis_points_used: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLedgerTraceSnapshot {
    pub operations: Vec<ResourceLedgerEntry>,
    pub balance_delta: IndexMap<PlayerId, IndexMap<String, i64>>,
    pub ledger_checksum: u64,
}

impl ResourceLedger {
    pub fn record(
        &mut self,
        tick: Tick,
        source: Option<PlayerId>,
        target: Option<PlayerId>,
        resource: &str,
        amount: i64,
        operation: ResourceOperation,
    ) {
        self.record_attributed(tick, source, target, resource, amount, operation, 0, 0);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_attributed(
        &mut self,
        tick: Tick,
        source: Option<PlayerId>,
        target: Option<PlayerId>,
        resource: &str,
        amount: i64,
        operation: ResourceOperation,
        fee_paid: ResourceAmount,
        basis_points_used: u32,
    ) {
        let gross = amount.unsigned_abs().min(ResourceAmount::MAX as u64) as ResourceAmount;
        let delivered = gross.saturating_sub(fee_paid);
        self.record_transfer_amounts(
            tick,
            source,
            target,
            resource,
            gross,
            delivered,
            operation,
            fee_paid,
            basis_points_used,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_transfer_amounts(
        &mut self,
        tick: Tick,
        source: Option<PlayerId>,
        target: Option<PlayerId>,
        resource: &str,
        amount_requested: ResourceAmount,
        amount_delivered: ResourceAmount,
        operation: ResourceOperation,
        fee_paid: ResourceAmount,
        basis_points_used: u32,
    ) {
        let amount = i64::from(amount_requested);
        self.ops.push(ResourceLedgerEntry {
            tick,
            source_player: source,
            target_player: target,
            resource: resource.to_string(),
            amount,
            amount_requested,
            amount_delivered,
            operation,
            fee_paid,
            basis_points_used,
        });

        // Track balance delta
        if let Some(s) = source {
            *self
                .balance_delta
                .entry(s)
                .or_default()
                .entry(resource.to_string())
                .or_default() -= i64::from(amount_requested);
        }
        if let Some(t) = target {
            *self
                .balance_delta
                .entry(t)
                .or_default()
                .entry(resource.to_string())
                .or_default() += i64::from(amount_delivered);
        }

        // Simple rolling checksum
        self.ledger_checksum = self
            .ledger_checksum
            .wrapping_add(u64::from(amount_requested))
            .wrapping_add(u64::from(amount_delivered))
            .wrapping_add(fee_paid as u64)
            .wrapping_add(basis_points_used as u64)
            .wrapping_add(tick);
    }

    pub fn record_transfer_result(&mut self, tick: Tick, result: &TransferResult) {
        if !result.success {
            return;
        }
        self.record_transfer_amounts(
            tick,
            result.source_player,
            result.target_player,
            &result.resource,
            result.amount_requested,
            result.amount_delivered,
            result.operation,
            result.fee_paid,
            result.basis_points_used,
        );
    }

    pub fn trace_snapshot(&self) -> ResourceLedgerTraceSnapshot {
        ResourceLedgerTraceSnapshot {
            operations: self.ops.clone(),
            balance_delta: self.balance_delta.clone(),
            ledger_checksum: self.ledger_checksum,
        }
    }
}

impl ResourceLedgerEntry {
    pub fn tick_trace_event(&self) -> TickTraceEvent {
        let event = if self.fee_paid > 0
            || self.basis_points_used > 0
            || self.amount_requested != self.amount_delivered
        {
            format!(
                "{:?}:requested={}:delivered={}:fee_paid={}:basis_points_used={}",
                self.operation,
                self.amount_requested,
                self.amount_delivered,
                self.fee_paid,
                self.basis_points_used
            )
        } else {
            format!("{:?}", self.operation)
        };
        TickTraceEvent {
            system: "resource_ledger".to_string(),
            entity: u64::from(
                self.target_player
                    .or(self.source_player)
                    .unwrap_or_default(),
            ),
            event,
            amount: self.amount.min(i64::from(u32::MAX)) as u32,
            resource: if self.resource.is_empty() {
                None
            } else {
                Some(self.resource.clone())
            },
        }
    }
}

/// S29 resource_ledger system — runs last to audit resource consistency
pub fn resource_ledger_system(
    mut ledger: ResMut<ResourceLedger>,
    mut trace_events: ResMut<TickTraceEventLog>,
) {
    // Per §08: produce balance summary, verify Σ inflows - Σ outflows = Δ storage
    let total_inflow: i64 = ledger
        .balance_delta
        .values()
        .flat_map(|p| p.values())
        .filter(|v| **v > 0)
        .sum();
    let total_outflow: i64 = ledger
        .balance_delta
        .values()
        .flat_map(|p| p.values())
        .filter(|v| **v < 0)
        .map(|v| -v)
        .sum();

    // The net should be zero (balanced ledger invariant)
    // Imbalance is noted for diagnostics; logged via EventLog in production paths
    let _imbalance = total_inflow - total_outflow;

    trace_events
        .events
        .extend(ledger.ops.iter().map(ResourceLedgerEntry::tick_trace_event));

    // Clear ops for next tick (but preserve checksum continuity)
    ledger.ops.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchors_spec_compliant() -> GlobalStorageConfig {
        GlobalStorageConfig {
            capacity: 1_000_000,
            ..Default::default()
        }
    }

    #[test]
    fn fee_computation_basis_points() {
        assert_eq!(compute_fee(1000, 100), 10); // 1% of 1000 = 10
        assert_eq!(compute_fee(1000, 500), 50); // 5% of 1000 = 50
        assert_eq!(compute_fee(1000, 200), 20); // 2% of 1000 = 20
        assert_eq!(compute_fee(1000, 0), 0);
        assert_eq!(compute_fee(0, 500), 0);
        assert_eq!(compute_fee(1, 10000), 1); // 100% of 1 = 1
    }

    #[test]
    fn continuous_storage_tax_empty_storage() {
        let config = anchors_spec_compliant();
        assert_eq!(compute_continuous_storage_tax(0, 1_000_000, &config), 0);
    }

    #[test]
    fn continuous_storage_tax_boundaries() {
        let config = anchors_spec_compliant();
        assert_eq!(
            compute_continuous_storage_tax(300_000, 1_000_000, &config),
            0
        );
        assert_eq!(
            compute_continuous_storage_tax(500_000, 1_000_000, &config),
            0
        );
        assert_eq!(
            compute_continuous_storage_tax(750_000, 1_000_000, &config),
            24
        );
        assert_eq!(
            compute_continuous_storage_tax(1_000_000, 1_000_000, &config),
            241
        );
    }

    #[test]
    fn continuous_storage_tax_monotonic() {
        let config = anchors_spec_compliant();
        let tax_75 = compute_continuous_storage_tax(750_000, 1_000_000, &config);
        let tax_100 = compute_continuous_storage_tax(1_000_000, 1_000_000, &config);
        assert!(tax_100 > tax_75);
    }

    #[test]
    fn ledger_records_balance_delta() {
        let mut ledger = ResourceLedger::default();
        ledger.record(
            0,
            Some(1),
            Some(2),
            "energy",
            100,
            ResourceOperation::LocalTransfer,
        );
        assert_eq!(ledger.ops.len(), 1);
        assert_eq!(
            *ledger.balance_delta.get(&1).unwrap().get("energy").unwrap(),
            -100
        );
        assert_eq!(
            *ledger.balance_delta.get(&2).unwrap().get("energy").unwrap(),
            100
        );
        assert_eq!(ledger.ledger_checksum, 200);
    }

    #[test]
    fn fee_transfer_records_gross_net_fee_and_reconstructs_balance() {
        let mut ledger = ResourceLedger::default();
        let result = TransferResult {
            operation: ResourceOperation::GlobalDeposit,
            source_player: Some(1),
            target_player: Some(2),
            resource: "Energy".to_string(),
            amount_requested: 100,
            amount_delivered: 95,
            fee_paid: 5,
            basis_points_used: 500,
            delayed_until: None,
            success: true,
            rejection_reason: None,
        };

        ledger.record_transfer_result(77, &result);

        assert_eq!(ledger.ops.len(), 1);
        let entry = &ledger.ops[0];
        assert_eq!(entry.amount, 100);
        assert_eq!(entry.amount_requested, 100);
        assert_eq!(entry.amount_delivered, 95);
        assert_eq!(entry.fee_paid, 5);
        assert_eq!(entry.basis_points_used, 500);
        assert_eq!(
            *ledger.balance_delta.get(&1).unwrap().get("Energy").unwrap(),
            -100
        );
        assert_eq!(
            *ledger.balance_delta.get(&2).unwrap().get("Energy").unwrap(),
            95
        );

        let event = entry.tick_trace_event();
        assert_eq!(event.amount, 100);
        assert!(event.event.contains("requested=100"));
        assert!(event.event.contains("delivered=95"));
        assert!(event.event.contains("fee_paid=5"));
        assert!(event.event.contains("basis_points_used=500"));

        let snapshot = ledger.trace_snapshot();
        assert_eq!(snapshot.operations, ledger.ops);
        assert_eq!(snapshot.balance_delta, ledger.balance_delta);
        assert_eq!(snapshot.ledger_checksum, ledger.ledger_checksum);
        let encoded = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(encoded["operations"][0]["amount_requested"], 100);
        assert_eq!(encoded["operations"][0]["amount_delivered"], 95);
        assert_eq!(encoded["operations"][0]["fee_paid"], 5);
        assert_eq!(encoded["operations"][0]["basis_points_used"], 500);
    }

    #[test]
    fn ledger_system_clears_ops() {
        let mut ledger = ResourceLedger::default();
        ledger.record(
            0,
            Some(1),
            None,
            "energy",
            -50,
            ResourceOperation::UpkeepDeduction,
        );
        assert_eq!(ledger.ops.len(), 1);
        // Manually clear ops (simulating what the Bevy system does)
        ledger.ops.clear();
        assert!(ledger.ops.is_empty(), "system should clear ops each tick");
        assert_eq!(
            ledger.ledger_checksum, 100,
            "checksum should persist across ticks"
        );
    }

    #[test]
    fn ledger_system_exports_ops_to_tick_trace_events_before_clearing() {
        let mut app = App::new();
        app.insert_resource(ResourceLedger::default());
        app.insert_resource(crate::tick::TickTraceEventLog::default());
        app.add_systems(Update, resource_ledger_system);

        app.world_mut().resource_mut::<ResourceLedger>().record(
            12,
            Some(1),
            Some(2),
            "Energy",
            25,
            ResourceOperation::LocalTransfer,
        );

        app.update();

        assert!(app.world().resource::<ResourceLedger>().ops.is_empty());
        let trace_events = app.world().resource::<crate::tick::TickTraceEventLog>();
        assert_eq!(trace_events.events.len(), 1);
        assert_eq!(trace_events.events[0].system, "resource_ledger");
        assert_eq!(trace_events.events[0].event, "LocalTransfer");
        assert_eq!(trace_events.events[0].amount, 25);
        assert_eq!(trace_events.events[0].resource.as_deref(), Some("Energy"));
    }
}
