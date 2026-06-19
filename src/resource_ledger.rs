// P2-1 Resource Ledger: Transfer Gateway — 统一资源入口
// Spec: specs/core/08-resource-ledger.md §1-§2

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::command::Tick;
use crate::components::PlayerId;
use crate::resources::{
    GlobalStorageConfig, GlobalStorageTaxTier, PlayerGlobalStorage, PlayerLocalStorage,
    PendingGlobalTransfer, PendingGlobalTransfers, ResourceAmount, ResourceCost, ResourceName,
};

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

/// Global deposit: local → global, with fee
pub fn execute_global_deposit(
    local_storage: &mut PlayerLocalStorage,
    global_storage: &mut PlayerGlobalStorage,
    pending: &mut PendingGlobalTransfers,
    config: &GlobalStorageConfig,
    player_id: PlayerId,
    resource: &str,
    amount: ResourceAmount,
    tick: Tick,
) -> TransferResult {
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
            start: crate::components::Position { x: 0, y: 0, room: crate::components::RoomId(0) },
            end: crate::components::Position { x: 0, y: 0, room: crate::components::RoomId(0) },
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
            Some(tick + config.transfer_to_global_ticks as u64)
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
    player_id: PlayerId,
    resource: &str,
    amount: ResourceAmount,
    tick: Tick,
) -> TransferResult {
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
            start: crate::components::Position { x: 0, y: 0, room: crate::components::RoomId(0) },
            end: crate::components::Position { x: 0, y: 0, room: crate::components::RoomId(0) },
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
            Some(tick + config.transfer_from_global_ticks as u64)
        } else {
            None
        },
        success: true,
        rejection_reason: None,
    }
}

// ═══════════════════════════════════════════════════════════════════
// Tiered Storage Tax (§2.2)
// ═══════════════════════════════════════════════════════════════════

/// Compute tiered storage tax using progressive brackets
/// Formula: Σ over each tier i where storage_pct > tier_threshold[i]:
///   taxable_in_tier_pct = min(storage_pct - tier_threshold[i], tier_width[i])
///   tax += taxable_in_tier_pct × tier_rate[i] × capacity / 10000 / 100
pub fn compute_tiered_storage_tax(
    stored_total: ResourceAmount,
    capacity: ResourceAmount,
    tiers: &[GlobalStorageTaxTier],
) -> ResourceAmount {
    if capacity == 0 || stored_total == 0 || tiers.is_empty() {
        return 0;
    }

    let utilization_pct = (stored_total as u64 * 100 / capacity as u64) as u32;
    let mut tax: u64 = 0;
    let mut prev_threshold: u32 = 0;

    for tier in tiers {
        let tier_threshold = tier.up_to_percent;

        if utilization_pct <= prev_threshold {
            break;
        }

        // Storage in this tier = min(remaining pct, tier width)
        let tier_width = tier_threshold.saturating_sub(prev_threshold);
        let stored_in_tier_pct = (utilization_pct.saturating_sub(prev_threshold)).min(tier_width);

        // taxable amount = stored_pct% × capacity / 100
        let taxable_amount = stored_in_tier_pct as u64 * capacity as u64 / 100;

        // tax = taxable × rate_bps / 10000
        let tier_tax = taxable_amount * tier.rate_per_10_000 as u64 / 10000;
        tax += tier_tax;

        prev_threshold = tier_threshold;
    }

    tax as ResourceAmount
}

/// Execute storage tax deduction for one player
pub fn execute_storage_tax(
    global_storage: &mut PlayerGlobalStorage,
    player_id: PlayerId,
    capacity: ResourceAmount,
    tiers: &[GlobalStorageTaxTier],
) -> TransferResult {
    let storage = global_storage.0.entry(player_id).or_default();
    let total_stored: ResourceAmount = storage.values().copied().sum();
    let tax_total = compute_tiered_storage_tax(total_stored, capacity, tiers);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::GlobalStorageTaxTier;

    fn tiers_spec_compliant() -> Vec<GlobalStorageTaxTier> {
        vec![
            GlobalStorageTaxTier {
                up_to_percent: 30,
                rate_per_10_000: 0,
            },
            GlobalStorageTaxTier {
                up_to_percent: 60,
                rate_per_10_000: 1,
            },
            GlobalStorageTaxTier {
                up_to_percent: 85,
                rate_per_10_000: 5,
            },
            GlobalStorageTaxTier {
                up_to_percent: 100,
                rate_per_10_000: 20,
            },
        ]
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
    fn tiered_storage_tax_empty_storage() {
        let tiers = tiers_spec_compliant();
        assert_eq!(compute_tiered_storage_tax(0, 1_000_000, &tiers), 0);
    }

    #[test]
    fn tiered_storage_tax_below_30_percent_is_free() {
        let tiers = tiers_spec_compliant();
        // 200k / 1M = 20% → tier 0, rate 0
        assert_eq!(
            compute_tiered_storage_tax(200_000, 1_000_000, &tiers),
            0
        );
    }

    #[test]
    fn tiered_storage_tax_spec_example_75_percent() {
        let tiers = tiers_spec_compliant();
        // Per spec §2.2 example: 750k / 1M = 75%
        // Tier 0 (0-30%): 300k × 0 = 0
        // Tier 1 (30-60%): 300k × 1bp = 30
        // Tier 2 (60-75%): 150k × 5bp = 75
        // Total = 105
        let tax = compute_tiered_storage_tax(750_000, 1_000_000, &tiers);
        assert_eq!(tax, 105, "spec example: 750k/1M should be 105");
    }

    #[test]
    fn tiered_storage_tax_100_percent_full() {
        let tiers = tiers_spec_compliant();
        // 1M / 1M = 100%
        // Tier 0: 300k × 0 = 0
        // Tier 1: 300k × 1 = 30
        // Tier 2: 250k × 5 = 125
        // Tier 3: 150k × 20 = 300
        // Total = 455
        let tax = compute_tiered_storage_tax(1_000_000, 1_000_000, &tiers);
        assert!(tax > 400, "100% full should have high tax, got {tax}");
    }
}
