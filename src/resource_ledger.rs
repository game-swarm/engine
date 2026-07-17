// P2-1 Resource Ledger: Transfer Gateway — 统一资源入口
// Spec: specs/core/08-resource-ledger.md §1-§2

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::command::Tick;
use crate::components::PlayerId;
use crate::resources::{
    GlobalStorageConfig, PendingGlobalTransfer, PendingGlobalTransfers, PlayerGlobalStorage,
    PlayerLocalStorage, ResourceAmount, ResourceName, SettlementId, SettlementKind,
};
use crate::tick::{TickTraceEvent, TickTraceEventLog};

const RESOURCE_LEDGER_ENTRY_DIGEST_DOMAIN: &[u8] = b"swarm.resource-ledger.entry.v1";
const ZERO_LEDGER_DIGEST: [u8; 32] = [0; 32];

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
    MerchantTradeSettlement,
    P2POfferSettlement,
    AuctionSettlement,
    EscrowSettlement,
    LendingSettlement,
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

/// Account identity used by the authoritative resource ledger.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LedgerAccount {
    Player {
        player_id: PlayerId,
    },
    Reserve {
        kind: SettlementKind,
        id: SettlementId,
        label: String,
    },
    Merchant {
        quote_id: SettlementId,
    },
    System {
        label: String,
    },
    Sink {
        label: String,
    },
}

impl LedgerAccount {
    pub fn player(player_id: PlayerId) -> Self {
        Self::Player { player_id }
    }

    pub fn reserve(kind: SettlementKind, id: SettlementId, label: impl Into<String>) -> Self {
        Self::Reserve {
            kind,
            id,
            label: label.into(),
        }
    }

    pub fn merchant(quote_id: SettlementId) -> Self {
        Self::Merchant { quote_id }
    }

    pub fn system(label: impl Into<String>) -> Self {
        Self::System {
            label: label.into(),
        }
    }

    pub fn sink(label: impl Into<String>) -> Self {
        Self::Sink {
            label: label.into(),
        }
    }

    fn player_id(&self) -> Option<PlayerId> {
        match self {
            Self::Player { player_id } => Some(*player_id),
            _ => None,
        }
    }

    fn key(&self) -> String {
        match self {
            Self::Player { player_id } => format!("player:{player_id}"),
            Self::Reserve { kind, id, label } => format!("reserve:{kind:?}:{id}:{label}"),
            Self::Merchant { quote_id } => format!("merchant:{quote_id}"),
            Self::System { label } => format!("system:{label}"),
            Self::Sink { label } => format!("sink:{label}"),
        }
    }
}

/// Cumulative ledger tracking current-tick and finalized resource operations.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLedger {
    /// Ordered log of current-tick resource operations.
    #[serde(default)]
    pub ops: Vec<ResourceLedgerEntry>,
    /// Current-tick net balance delta per player per resource.
    #[serde(default)]
    pub balance_delta: IndexMap<PlayerId, IndexMap<String, i64>>,
    /// Current-tick net balance delta per ledger account key per resource.
    #[serde(default)]
    pub account_delta: IndexMap<String, IndexMap<String, i64>>,
    /// Persistent cumulative net balance delta per ledger account key per resource.
    #[serde(default)]
    pub cumulative_account_delta: IndexMap<String, IndexMap<String, i64>>,
    /// Persistent ledger checksum for TickTrace integrity.
    #[serde(default)]
    pub ledger_checksum: u64,
    /// Persistent authenticated ledger digest for TickTrace integrity.
    #[serde(default)]
    pub ledger_digest: [u8; 32],
    /// Digest at the start of the current tick chain.
    #[serde(default)]
    pub tick_start_ledger_digest: [u8; 32],
    /// Finalized previous tick snapshot. Used after S29 clears current ops.
    #[serde(default)]
    pub last_tick: ResourceLedgerTraceSnapshot,
}

/// A single resource operation entry
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLedgerEntry {
    pub tick: Tick,
    #[serde(default)]
    pub source_account: Option<LedgerAccount>,
    #[serde(default)]
    pub target_account: Option<LedgerAccount>,
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
    #[serde(default)]
    pub operations: Vec<ResourceLedgerEntry>,
    #[serde(default)]
    pub balance_delta: IndexMap<PlayerId, IndexMap<String, i64>>,
    #[serde(default)]
    pub account_delta: IndexMap<String, IndexMap<String, i64>>,
    #[serde(default)]
    pub cumulative_account_delta: IndexMap<String, IndexMap<String, i64>>,
    #[serde(default)]
    pub conservation_imbalance: IndexMap<String, i64>,
    #[serde(default)]
    pub ledger_checksum: u64,
    #[serde(default)]
    pub previous_ledger_digest: [u8; 32],
    #[serde(default)]
    pub ledger_digest: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceLedgerValidationError {
    ConservationSummaryMismatch {
        expected: IndexMap<String, i64>,
        actual: IndexMap<String, i64>,
    },
    ConservationImbalance {
        imbalance: IndexMap<String, i64>,
    },
    BalanceDeltaMismatch {
        expected: IndexMap<PlayerId, IndexMap<String, i64>>,
        actual: IndexMap<PlayerId, IndexMap<String, i64>>,
    },
    AccountDeltaMismatch {
        expected: IndexMap<String, IndexMap<String, i64>>,
        actual: IndexMap<String, IndexMap<String, i64>>,
    },
    LegacyDigestForNewLedger,
    LedgerDigestMismatch {
        expected: [u8; 32],
        actual: [u8; 32],
    },
}

impl std::fmt::Display for ResourceLedgerValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
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
    pub fn record_account_transfer(
        &mut self,
        tick: Tick,
        source_account: LedgerAccount,
        target_account: LedgerAccount,
        resource: &str,
        amount: ResourceAmount,
        operation: ResourceOperation,
    ) {
        self.record_account_transfer_amounts(
            tick,
            Some(source_account),
            Some(target_account),
            resource,
            amount,
            amount,
            operation,
            0,
            0,
        );
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
        let source_account = source
            .map(LedgerAccount::player)
            .or_else(|| target.map(|_| LedgerAccount::system(format!("{:?}", operation))));
        let target_account = target
            .map(LedgerAccount::player)
            .or_else(|| source.map(|_| LedgerAccount::sink(format!("{:?}", operation))));

        self.record_account_transfer_amounts(
            tick,
            source_account,
            target_account,
            resource,
            amount_requested,
            amount_delivered,
            operation,
            fee_paid,
            basis_points_used,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_account_transfer_amounts(
        &mut self,
        tick: Tick,
        source_account: Option<LedgerAccount>,
        target_account: Option<LedgerAccount>,
        resource: &str,
        amount_requested: ResourceAmount,
        amount_delivered: ResourceAmount,
        operation: ResourceOperation,
        fee_paid: ResourceAmount,
        basis_points_used: u32,
    ) {
        let amount = i64::from(amount_requested);
        let source = source_account.as_ref().and_then(LedgerAccount::player_id);
        let target = target_account.as_ref().and_then(LedgerAccount::player_id);
        if self.ops.is_empty() {
            self.tick_start_ledger_digest = self.ledger_digest;
        }
        let entry = ResourceLedgerEntry {
            tick,
            source_account: source_account.clone(),
            target_account: target_account.clone(),
            source_player: source,
            target_player: target,
            resource: resource.to_string(),
            amount,
            amount_requested,
            amount_delivered,
            operation,
            fee_paid,
            basis_points_used,
        };
        self.ledger_digest = ledger_entry_digest(self.ledger_digest, &entry);
        self.ops.push(entry);

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

        if let Some(account) = &source_account {
            self.add_account_delta(account, resource, -i64::from(amount_requested));
        }
        if let Some(account) = &target_account {
            self.add_account_delta(account, resource, i64::from(amount_delivered));
        }
        if fee_paid > 0 {
            self.add_account_delta(
                &LedgerAccount::sink(format!("{:?}:fee", operation)),
                resource,
                i64::from(fee_paid),
            );
        }

        // Simple rolling checksum
        self.ledger_checksum = self
            .ledger_checksum
            .wrapping_add(u64::from(amount_requested))
            .wrapping_add(u64::from(amount_delivered))
            .wrapping_add(fee_paid as u64)
            .wrapping_add(basis_points_used as u64)
            .wrapping_add(tick)
            .wrapping_add(account_checksum(source_account.as_ref()))
            .wrapping_add(account_checksum(target_account.as_ref()));
    }

    fn add_account_delta(&mut self, account: &LedgerAccount, resource: &str, delta: i64) {
        if delta == 0 {
            return;
        }
        let key = account.key();
        *self
            .account_delta
            .entry(key.clone())
            .or_default()
            .entry(resource.to_string())
            .or_default() += delta;
        *self
            .cumulative_account_delta
            .entry(key)
            .or_default()
            .entry(resource.to_string())
            .or_default() += delta;
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
        if self.ops.is_empty() && self.balance_delta.is_empty() && self.account_delta.is_empty() {
            return self.last_tick.clone();
        }
        self.current_snapshot()
    }

    pub fn finalize_current_tick(&mut self) -> ResourceLedgerTraceSnapshot {
        let snapshot = self.current_snapshot();
        self.last_tick = snapshot.clone();
        self.ops.clear();
        self.balance_delta.clear();
        self.account_delta.clear();
        self.tick_start_ledger_digest = self.ledger_digest;
        snapshot
    }

    fn current_snapshot(&self) -> ResourceLedgerTraceSnapshot {
        ResourceLedgerTraceSnapshot {
            operations: self.ops.clone(),
            balance_delta: self.balance_delta.clone(),
            account_delta: self.account_delta.clone(),
            cumulative_account_delta: self.cumulative_account_delta.clone(),
            conservation_imbalance: conservation_imbalance(&self.account_delta),
            ledger_checksum: self.ledger_checksum,
            previous_ledger_digest: self.tick_start_ledger_digest,
            ledger_digest: self.ledger_digest,
        }
    }
}

impl ResourceLedgerTraceSnapshot {
    pub fn validate_for_commit(&self) -> Result<(), ResourceLedgerValidationError> {
        self.validate(false)
    }

    pub fn validate_for_replay(&self) -> Result<(), ResourceLedgerValidationError> {
        self.validate(true)
    }

    pub fn replay_equivalent(&self, replayed: &Self) -> bool {
        if !self.uses_legacy_digest() {
            return self == replayed;
        }

        let mut expected = self.clone();
        let mut actual = replayed.clone();
        expected.previous_ledger_digest = ZERO_LEDGER_DIGEST;
        expected.ledger_digest = ZERO_LEDGER_DIGEST;
        actual.previous_ledger_digest = ZERO_LEDGER_DIGEST;
        actual.ledger_digest = ZERO_LEDGER_DIGEST;
        expected == actual
    }

    fn validate(&self, allow_legacy_digest: bool) -> Result<(), ResourceLedgerValidationError> {
        let expected_imbalance = conservation_imbalance(&self.account_delta);
        if expected_imbalance != self.conservation_imbalance {
            return Err(ResourceLedgerValidationError::ConservationSummaryMismatch {
                expected: expected_imbalance,
                actual: self.conservation_imbalance.clone(),
            });
        }
        if !self.conservation_imbalance.is_empty() {
            return Err(ResourceLedgerValidationError::ConservationImbalance {
                imbalance: self.conservation_imbalance.clone(),
            });
        }

        self.validate_digest(allow_legacy_digest)?;

        let expected_balance_delta = balance_delta_from_operations(&self.operations);
        if expected_balance_delta != self.balance_delta {
            return Err(ResourceLedgerValidationError::BalanceDeltaMismatch {
                expected: expected_balance_delta,
                actual: self.balance_delta.clone(),
            });
        }

        let expected_account_delta = account_delta_from_operations(&self.operations);
        if expected_account_delta != self.account_delta {
            return Err(ResourceLedgerValidationError::AccountDeltaMismatch {
                expected: expected_account_delta,
                actual: self.account_delta.clone(),
            });
        }

        Ok(())
    }

    fn validate_digest(
        &self,
        allow_legacy_digest: bool,
    ) -> Result<(), ResourceLedgerValidationError> {
        if self.uses_legacy_digest() && !self.operations.is_empty() {
            if allow_legacy_digest {
                return Ok(());
            }
            return Err(ResourceLedgerValidationError::LegacyDigestForNewLedger);
        }

        let expected = ledger_digest_for_entries(self.previous_ledger_digest, &self.operations);
        if expected != self.ledger_digest {
            return Err(ResourceLedgerValidationError::LedgerDigestMismatch {
                expected,
                actual: self.ledger_digest,
            });
        }

        Ok(())
    }

    fn uses_legacy_digest(&self) -> bool {
        self.previous_ledger_digest == ZERO_LEDGER_DIGEST
            && self.ledger_digest == ZERO_LEDGER_DIGEST
    }
}

fn account_checksum(account: Option<&LedgerAccount>) -> u64 {
    let Some(account) = account else {
        return 0;
    };
    let hash = blake3::hash(account.key().as_bytes());
    u64::from_le_bytes(
        hash.as_bytes()[..8]
            .try_into()
            .expect("BLAKE3 digest has 32 bytes"),
    )
}

fn ledger_digest_for_entries(
    previous_digest: [u8; 32],
    entries: &[ResourceLedgerEntry],
) -> [u8; 32] {
    entries.iter().fold(previous_digest, ledger_entry_digest)
}

fn ledger_entry_digest(previous_digest: [u8; 32], entry: &ResourceLedgerEntry) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(RESOURCE_LEDGER_ENTRY_DIGEST_DOMAIN);
    hasher.update(&previous_digest);
    hash_u64(&mut hasher, entry.tick);
    hash_option_account(&mut hasher, entry.source_account.as_ref());
    hash_option_account(&mut hasher, entry.target_account.as_ref());
    hash_option_player(&mut hasher, entry.source_player);
    hash_option_player(&mut hasher, entry.target_player);
    hash_string(&mut hasher, &entry.resource);
    hash_i64(&mut hasher, entry.amount);
    hash_u32(&mut hasher, entry.amount_requested);
    hash_u32(&mut hasher, entry.amount_delivered);
    hash_u8(&mut hasher, resource_operation_tag(entry.operation));
    hash_u32(&mut hasher, entry.fee_paid);
    hash_u32(&mut hasher, entry.basis_points_used);
    *hasher.finalize().as_bytes()
}

fn hash_option_account(hasher: &mut blake3::Hasher, account: Option<&LedgerAccount>) {
    match account {
        Some(account) => {
            hash_u8(hasher, 1);
            hash_account(hasher, account);
        }
        None => hash_u8(hasher, 0),
    }
}

fn hash_account(hasher: &mut blake3::Hasher, account: &LedgerAccount) {
    match account {
        LedgerAccount::Player { player_id } => {
            hash_u8(hasher, 1);
            hash_u32(hasher, *player_id);
        }
        LedgerAccount::Reserve { kind, id, label } => {
            hash_u8(hasher, 2);
            hash_u8(hasher, settlement_kind_tag(*kind));
            hash_u64(hasher, *id);
            hash_string(hasher, label);
        }
        LedgerAccount::Merchant { quote_id } => {
            hash_u8(hasher, 3);
            hash_u64(hasher, *quote_id);
        }
        LedgerAccount::System { label } => {
            hash_u8(hasher, 4);
            hash_string(hasher, label);
        }
        LedgerAccount::Sink { label } => {
            hash_u8(hasher, 5);
            hash_string(hasher, label);
        }
    }
}

fn hash_option_player(hasher: &mut blake3::Hasher, player: Option<PlayerId>) {
    match player {
        Some(player) => {
            hash_u8(hasher, 1);
            hash_u32(hasher, player);
        }
        None => hash_u8(hasher, 0),
    }
}

fn hash_string(hasher: &mut blake3::Hasher, value: &str) {
    hash_u64(hasher, value.len() as u64);
    hasher.update(value.as_bytes());
}

fn hash_u8(hasher: &mut blake3::Hasher, value: u8) {
    hasher.update(&[value]);
}

fn hash_u32(hasher: &mut blake3::Hasher, value: u32) {
    hasher.update(&value.to_le_bytes());
}

fn hash_u64(hasher: &mut blake3::Hasher, value: u64) {
    hasher.update(&value.to_le_bytes());
}

fn hash_i64(hasher: &mut blake3::Hasher, value: i64) {
    hasher.update(&value.to_le_bytes());
}

fn settlement_kind_tag(kind: SettlementKind) -> u8 {
    match kind {
        SettlementKind::Contract => 1,
        SettlementKind::MerchantTrade => 2,
        SettlementKind::P2POffer => 3,
        SettlementKind::Auction => 4,
        SettlementKind::Escrow => 5,
        SettlementKind::Lending => 6,
    }
}

fn resource_operation_tag(operation: ResourceOperation) -> u8 {
    match operation {
        ResourceOperation::LocalTransfer => 1,
        ResourceOperation::GlobalDeposit => 2,
        ResourceOperation::GlobalWithdraw => 3,
        ResourceOperation::AlliedTransfer => 4,
        ResourceOperation::PvEAward => 5,
        ResourceOperation::ControllerPassiveIncome => 6,
        ResourceOperation::RecycleRefund => 7,
        ResourceOperation::BuildCost => 8,
        ResourceOperation::SpawnCost => 9,
        ResourceOperation::UpkeepDeduction => 10,
        ResourceOperation::StorageTax => 11,
        ResourceOperation::ContractSettlement => 12,
        ResourceOperation::MerchantTradeSettlement => 13,
        ResourceOperation::P2POfferSettlement => 14,
        ResourceOperation::AuctionSettlement => 15,
        ResourceOperation::EscrowSettlement => 16,
        ResourceOperation::LendingSettlement => 17,
    }
}

fn balance_delta_from_operations(
    operations: &[ResourceLedgerEntry],
) -> IndexMap<PlayerId, IndexMap<String, i64>> {
    let mut balance_delta = IndexMap::new();
    for entry in operations {
        if let Some(source) = entry.source_player {
            add_player_resource_delta(
                &mut balance_delta,
                source,
                &entry.resource,
                -i64::from(entry.amount_requested),
            );
        }
        if let Some(target) = entry.target_player {
            add_player_resource_delta(
                &mut balance_delta,
                target,
                &entry.resource,
                i64::from(entry.amount_delivered),
            );
        }
    }
    balance_delta
}

fn account_delta_from_operations(
    operations: &[ResourceLedgerEntry],
) -> IndexMap<String, IndexMap<String, i64>> {
    let mut account_delta = IndexMap::new();
    for entry in operations {
        if let Some(account) = &entry.source_account {
            add_account_resource_delta(
                &mut account_delta,
                account.key(),
                &entry.resource,
                -i64::from(entry.amount_requested),
            );
        }
        if let Some(account) = &entry.target_account {
            add_account_resource_delta(
                &mut account_delta,
                account.key(),
                &entry.resource,
                i64::from(entry.amount_delivered),
            );
        }
        if entry.fee_paid > 0 {
            add_account_resource_delta(
                &mut account_delta,
                LedgerAccount::sink(format!("{:?}:fee", entry.operation)).key(),
                &entry.resource,
                i64::from(entry.fee_paid),
            );
        }
    }
    account_delta
}

fn add_player_resource_delta(
    map: &mut IndexMap<PlayerId, IndexMap<String, i64>>,
    player_id: PlayerId,
    resource: &str,
    delta: i64,
) {
    *map.entry(player_id)
        .or_default()
        .entry(resource.to_string())
        .or_default() += delta;
}

fn add_account_resource_delta(
    map: &mut IndexMap<String, IndexMap<String, i64>>,
    account_key: String,
    resource: &str,
    delta: i64,
) {
    *map.entry(account_key)
        .or_default()
        .entry(resource.to_string())
        .or_default() += delta;
}

fn conservation_imbalance(
    account_delta: &IndexMap<String, IndexMap<String, i64>>,
) -> IndexMap<String, i64> {
    let mut imbalance = IndexMap::<String, i64>::new();
    for resources in account_delta.values() {
        for (resource, delta) in resources {
            *imbalance.entry(resource.clone()).or_default() += *delta;
        }
    }
    imbalance.retain(|_, delta| *delta != 0);
    imbalance
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
    let snapshot = ledger.finalize_current_tick();
    trace_events.events.extend(
        snapshot
            .operations
            .iter()
            .map(ResourceLedgerEntry::tick_trace_event),
    );
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
        assert_ne!(ledger.ledger_checksum, 0);
        assert!(ledger.trace_snapshot().conservation_imbalance.is_empty());
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
    fn fee_transfer_digest_changes_with_corrected_entry_data() {
        let mut correct = ResourceLedger::default();
        correct.record_transfer_amounts(
            77,
            Some(1),
            Some(2),
            "Energy",
            100,
            95,
            ResourceOperation::GlobalDeposit,
            5,
            500,
        );

        let mut incorrect = ResourceLedger::default();
        incorrect.record_transfer_amounts(
            77,
            Some(1),
            Some(2),
            "Energy",
            95,
            90,
            ResourceOperation::GlobalDeposit,
            5,
            500,
        );

        assert_ne!(
            correct.trace_snapshot().ledger_digest,
            incorrect.trace_snapshot().ledger_digest
        );
    }

    fn balanced_transfer_snapshot() -> ResourceLedgerTraceSnapshot {
        let mut ledger = ResourceLedger::default();
        ledger.record_transfer_amounts(
            9,
            Some(1),
            Some(2),
            "Energy",
            100,
            95,
            ResourceOperation::AlliedTransfer,
            5,
            500,
        );
        ledger.trace_snapshot()
    }

    #[test]
    fn ledger_digest_detects_resource_mutation() {
        let mut snapshot = balanced_transfer_snapshot();
        snapshot.operations[0].resource = "Mineral".to_string();

        let error = snapshot.validate_for_commit().unwrap_err();

        assert!(matches!(
            error,
            ResourceLedgerValidationError::LedgerDigestMismatch { .. }
        ));
    }

    #[test]
    fn ledger_digest_detects_operation_mutation() {
        let mut snapshot = balanced_transfer_snapshot();
        snapshot.operations[0].operation = ResourceOperation::StorageTax;

        let error = snapshot.validate_for_commit().unwrap_err();

        assert!(matches!(
            error,
            ResourceLedgerValidationError::LedgerDigestMismatch { .. }
        ));
    }

    #[test]
    fn ledger_digest_chains_from_prior_digest() {
        let mut chained = ResourceLedger::default();
        chained.record(
            1,
            Some(1),
            Some(2),
            "Energy",
            10,
            ResourceOperation::LocalTransfer,
        );
        chained.finalize_current_tick();
        let prior_digest = chained.ledger_digest;
        chained.record(
            2,
            Some(2),
            Some(1),
            "Energy",
            4,
            ResourceOperation::LocalTransfer,
        );
        let chained_snapshot = chained.trace_snapshot();

        let mut unchained = ResourceLedger::default();
        unchained.record(
            2,
            Some(2),
            Some(1),
            "Energy",
            4,
            ResourceOperation::LocalTransfer,
        );
        let unchained_snapshot = unchained.trace_snapshot();

        assert_eq!(chained_snapshot.previous_ledger_digest, prior_digest);
        assert_ne!(
            chained_snapshot.ledger_digest,
            unchained_snapshot.ledger_digest
        );
        assert!(chained_snapshot.validate_for_commit().is_ok());
    }

    #[test]
    fn ledger_digest_tamper_is_rejected() {
        let mut snapshot = balanced_transfer_snapshot();
        snapshot.ledger_digest[0] ^= 0x80;

        let error = snapshot.validate_for_commit().unwrap_err();

        assert!(matches!(
            error,
            ResourceLedgerValidationError::LedgerDigestMismatch { .. }
        ));
    }

    #[test]
    fn conservation_imbalance_is_rejected() {
        let mut ledger = ResourceLedger::default();
        ledger.record_transfer_amounts(
            3,
            Some(1),
            Some(2),
            "Energy",
            10,
            9,
            ResourceOperation::LocalTransfer,
            0,
            0,
        );
        let snapshot = ledger.trace_snapshot();

        let error = snapshot.validate_for_commit().unwrap_err();

        assert!(matches!(
            error,
            ResourceLedgerValidationError::ConservationImbalance { .. }
        ));
    }

    #[test]
    fn legacy_digest_fields_default_on_deserialize() {
        let snapshot = balanced_transfer_snapshot();
        let mut value = serde_json::to_value(&snapshot).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("previous_ledger_digest");
        object.remove("ledger_digest");
        let legacy_snapshot: ResourceLedgerTraceSnapshot = serde_json::from_value(value).unwrap();

        assert_eq!(legacy_snapshot.previous_ledger_digest, ZERO_LEDGER_DIGEST);
        assert_eq!(legacy_snapshot.ledger_digest, ZERO_LEDGER_DIGEST);
        assert!(legacy_snapshot.validate_for_replay().is_ok());
        assert!(matches!(
            legacy_snapshot.validate_for_commit().unwrap_err(),
            ResourceLedgerValidationError::LegacyDigestForNewLedger
        ));

        let mut ledger = ResourceLedger::default();
        ledger.record(
            1,
            Some(1),
            Some(2),
            "Energy",
            1,
            ResourceOperation::LocalTransfer,
        );
        let mut ledger_value = serde_json::to_value(&ledger).unwrap();
        let ledger_object = ledger_value.as_object_mut().unwrap();
        ledger_object.remove("ledger_digest");
        ledger_object.remove("tick_start_ledger_digest");
        let legacy_ledger: ResourceLedger = serde_json::from_value(ledger_value).unwrap();
        assert_eq!(legacy_ledger.ledger_digest, ZERO_LEDGER_DIGEST);
        assert_eq!(legacy_ledger.tick_start_ledger_digest, ZERO_LEDGER_DIGEST);
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
        let checksum = ledger.ledger_checksum;
        let finalized = ledger.finalize_current_tick();
        assert!(ledger.ops.is_empty(), "system should clear ops each tick");
        assert!(
            ledger.account_delta.is_empty(),
            "system should clear current account deltas"
        );
        assert_eq!(finalized.operations.len(), 1);
        assert_eq!(ledger.trace_snapshot().operations.len(), 1);
        assert_eq!(
            ledger.ledger_checksum, checksum,
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
