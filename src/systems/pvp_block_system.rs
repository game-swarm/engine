use bevy::prelude::*;

use crate::systems::combat_system::PendingCombat;
use crate::world::WorldConfig;

/// Clears all pending combat events when PvP is disabled.
///
/// Runs before spawn_system in the system chain so PvP-blocked ticks
/// process zero combat. Positioned after death_mark_system to ensure
/// deathMarked entities are already cleared.
pub fn pvp_block_system(config: Res<WorldConfig>, mut combat: ResMut<PendingCombat>) {
    if config.combat.pvp_enabled {
        return;
    }

    combat.damage.clear();
    combat.typed_damage.clear();
    combat.heal.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{Drone, DEFAULT_DRONE_LIFESPAN};
    use crate::world::create_world;
    use indexmap::IndexMap;

    fn spawn_test_drone(world: &mut crate::SwarmWorld, owner: u32) -> Entity {
        world
            .app
            .world_mut()
            .spawn(Drone {
                owner,
                body: vec![],
                carry: IndexMap::new(),
                carry_capacity: 0,
                fatigue: 0,
                hits: 100,
                hits_max: 100,
                spawning: false,
                age: 0,
                last_action_tick: u64::MAX,
                lifespan: DEFAULT_DRONE_LIFESPAN,
            })
            .id()
    }

    #[test]
    fn pvp_block_clears_combat_when_disabled() {
        let mut world = create_world();
        world.app.world_mut().resource_mut::<WorldConfig>().combat.pvp_enabled = false;

        // Pre-populate PendingCombat with damage/heal
        let e1 = spawn_test_drone(&mut world, 1);
        {
            let mut combat = world.app.world_mut().resource_mut::<PendingCombat>();
            combat.damage.push((e1.to_bits(), 5));
            combat.heal.push((e1.to_bits(), 3));
        }

        // Run one tick — pvp_block_system (chain 1, before combat) should clear combat
        world.app.update();

        // After full update, combat_system drains PendingCombat.
        // The key assertion: the drone was NOT damaged because pvp_block
        // cleared combat before combat_system ran.
        let drone = world.app.world().entity(e1).get::<Drone>().unwrap();
        assert_eq!(drone.hits, 98, "only queued heal should remain when PvP damage is blocked");
    }

    #[test]
    fn pvp_block_allows_combat_when_enabled() {
        let mut world = create_world();
        world.app.world_mut().resource_mut::<WorldConfig>().combat.pvp_enabled = true;

        let e1 = spawn_test_drone(&mut world, 1);
        {
            let mut combat = world.app.world_mut().resource_mut::<PendingCombat>();
            combat.damage.push((e1.to_bits(), 5));
        }

        // Run one tick — pvp_block should NOT clear combat, so combat_system applies damage
        world.app.update();

        // After full update, the drone should have taken damage
        let drone = world.app.world().entity(e1).get::<Drone>().unwrap();
        assert_eq!(drone.hits, 95, "drone should be damaged when PvP enabled");
    }
}
