use bevy::prelude::*;

use crate::resources::{
    GlobalStorageConfig, GlobalTransferDirection, PendingGlobalTransfers, PlayerGlobalStorage,
    PlayerLocalStorage, ResourceCost, ResourceName,
};

pub fn global_storage_system(
    config: Res<GlobalStorageConfig>,
    mut global_storage: ResMut<PlayerGlobalStorage>,
    mut local_storage: ResMut<PlayerLocalStorage>,
    mut pending_transfers: ResMut<PendingGlobalTransfers>,
) {
    let mut delivered = Vec::new();
    let mut remaining = Vec::new();

    for mut transfer in std::mem::take(&mut pending_transfers.0) {
        transfer.remaining_ticks = transfer.remaining_ticks.saturating_sub(1);
        if transfer.remaining_ticks == 0 {
            delivered.push(transfer);
        } else {
            remaining.push(transfer);
        }
    }
    pending_transfers.0 = remaining;

    for transfer in delivered {
        let target = match transfer.direction {
            GlobalTransferDirection::ToGlobal => {
                global_storage.0.entry(transfer.player_id).or_default()
            }
            GlobalTransferDirection::FromGlobal => {
                local_storage.0.entry(transfer.player_id).or_default()
            }
        };
        add_resource(target, transfer.resource, transfer.deliver_amount);
    }

    for storage in global_storage.0.values_mut() {
        apply_progressive_tax(storage, config.capacity, &config.tax_tiers);
    }
}

fn add_resource(storage: &mut ResourceCost, resource: ResourceName, amount: u32) {
    *storage.entry(resource).or_default() += amount;
}

fn apply_progressive_tax(
    storage: &mut ResourceCost,
    capacity: u32,
    tiers: &[crate::resources::GlobalStorageTaxTier],
) {
    if capacity == 0 || tiers.is_empty() {
        return;
    }

    let total: u32 = storage.values().sum();
    let tax = progressive_tax(total, capacity, tiers);
    if tax == 0 {
        return;
    }

    let mut remaining_tax = tax;
    for amount in storage.values_mut().rev() {
        let taken = (*amount).min(remaining_tax);
        *amount -= taken;
        remaining_tax -= taken;
        if remaining_tax == 0 {
            break;
        }
    }
}

fn progressive_tax(
    total: u32,
    capacity: u32,
    tiers: &[crate::resources::GlobalStorageTaxTier],
) -> u32 {
    let mut previous_limit = 0_u32;
    let mut tax = 0_u32;

    for tier in tiers {
        let limit =
            ((capacity as u64 * tier.up_to_percent as u64) / 100).min(u32::MAX as u64) as u32;
        if total > previous_limit {
            let taxable = total.min(limit).saturating_sub(previous_limit);
            tax = tax.saturating_add(taxable.saturating_mul(tier.rate_per_10_000) / 10_000);
        }
        previous_limit = limit;
    }

    if total > previous_limit {
        let rate = tiers
            .last()
            .map(|tier| tier.rate_per_10_000)
            .unwrap_or_default();
        tax =
            tax.saturating_add(total.saturating_sub(previous_limit).saturating_mul(rate) / 10_000);
    }

    tax.min(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::GlobalStorageConfig;

    #[test]
    fn progressive_tax_uses_configured_tiers() {
        let tiers = GlobalStorageConfig::default().tax_tiers;

        assert_eq!(progressive_tax(30_000, 100_000, &tiers), 0);
        assert_eq!(progressive_tax(60_000, 100_000, &tiers), 3);
        assert_eq!(progressive_tax(85_000, 100_000, &tiers), 15);
        assert_eq!(progressive_tax(100_000, 100_000, &tiers), 45);
    }
}
