use bevy::prelude::*;

use crate::components::{Drone, Owner, Position};
use crate::resource_ledger::compute_continuous_storage_tax;
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
        apply_continuous_tax(storage, &config);
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

fn apply_continuous_tax(storage: &mut ResourceCost, config: &GlobalStorageConfig) {
    if config.capacity == 0 {
        return;
    }

    let total: u32 = storage.values().sum();
    let tax = compute_continuous_storage_tax(total, config.capacity, config);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::GlobalStorageConfig;

    #[test]
    fn continuous_tax_uses_configured_anchors() {
        let config = GlobalStorageConfig::default();

        assert_eq!(compute_continuous_storage_tax(30_000, 100_000, &config), 0);
        assert_eq!(
            compute_continuous_storage_tax(100_000, 100_000, &config),
            24
        );
    }
}
