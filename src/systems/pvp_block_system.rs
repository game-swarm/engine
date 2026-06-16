use bevy::prelude::*;

use crate::systems::combat_system::PendingCombat;
use crate::world::WorldConfig;

/// When `pvp_enabled` is false, clears all pending combat events before
/// `combat_system` runs, preventing any hostile actions from dealing damage.
///
/// Runs before `spawn_system` in the system chain so PvP-blocked tick
/// processes zero combat.
pub fn pvp_block_system(
    config: Res<WorldConfig>,
    mut combat: ResMut<PendingCombat>,
) {
    if config.combat.pvp_enabled {
        return;
    }

    combat.damage.clear();
    combat.typed_damage.clear();
    combat.heal.clear();
}
