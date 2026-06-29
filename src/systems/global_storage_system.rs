use bevy::prelude::*;

use crate::components::{Drone, Owner, Position};
use crate::resources::{
    GlobalStorageConfig, GlobalTransferDirection, PendingAlliedTransfers, PendingGlobalTransfers,
    PlayerGlobalStorage, PlayerLocalStorage, ResourceCost, ResourceName,
};

pub fn global_storage_system(
    config: Res<GlobalStorageConfig>,
    mut global_storage: ResMut<PlayerGlobalStorage>,
    mut local_storage: ResMut<PlayerLocalStorage>,
    mut pending_transfers: ResMut<PendingGlobalTransfers>,
    drones: Query<(&Position, &Owner), With<Drone>>,
) {
    let mut delivered = Vec::new();
    let mut remaining = Vec::new();

    for mut transfer in std::mem::take(&mut pending_transfers.0) {
        if config.intercept_enabled
            && transfer_intercepted(
                transfer.player_id,
                transfer.start,
                transfer.end,
                config.intercept_range,
                &drones,
            )
        {
            continue;
        }

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

fn transfer_intercepted(
    player_id: crate::components::PlayerId,
    start: Position,
    end: Position,
    range: u32,
    drones: &Query<(&Position, &Owner), With<Drone>>,
) -> bool {
    drones.iter().any(|(position, owner)| {
        owner.0 != player_id && position_in_transfer_range(*position, start, end, range)
    })
}

fn position_in_transfer_range(
    position: Position,
    start: Position,
    end: Position,
    range: u32,
) -> bool {
    if position.room != start.room || start.room != end.room {
        return false;
    }

    let px = position.x as i64;
    let py = position.y as i64;
    let sx = start.x as i64;
    let sy = start.y as i64;
    let ex = end.x as i64;
    let ey = end.y as i64;
    let dx = ex - sx;
    let dy = ey - sy;
    let segment_len_sq = dx * dx + dy * dy;
    let range_sq = range as i64 * range as i64;

    if segment_len_sq == 0 {
        return squared_distance(px, py, sx, sy) <= range_sq;
    }

    let to_point_x = px - sx;
    let to_point_y = py - sy;
    let dot = to_point_x * dx + to_point_y * dy;
    if dot <= 0 {
        return squared_distance(px, py, sx, sy) <= range_sq;
    }
    if dot >= segment_len_sq {
        return squared_distance(px, py, ex, ey) <= range_sq;
    }

    let cross = to_point_x * dy - to_point_y * dx;
    cross * cross <= range_sq * segment_len_sq
}

fn squared_distance(ax: i64, ay: i64, bx: i64, by: i64) -> i64 {
    let dx = ax - bx;
    let dy = ay - by;
    dx * dx + dy * dy
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

// ── Associated Functions ──

/// Process pending allied transfers — decrement timers and deliver when ready.
pub fn allied_transfer_system(
    mut pending: ResMut<PendingAlliedTransfers>,
    mut global_storage: ResMut<PlayerGlobalStorage>,
) {
    let mut delivered = Vec::new();
    let mut remaining = Vec::new();

    for mut transfer in std::mem::take(&mut pending.0) {
        transfer.remaining_ticks = transfer.remaining_ticks.saturating_sub(1);
        if transfer.remaining_ticks == 0 {
            delivered.push(transfer);
        } else {
            remaining.push(transfer);
        }
    }
    pending.0 = remaining;

    for transfer in delivered {
        let target = global_storage.0.entry(transfer.to_player).or_default();
        add_resource(target, transfer.resource, transfer.deliver_amount);
    }
}
