use bevy::prelude::*;

use crate::components::{Drone, Structure};

/// Decay system — handles fatigue/cooldown reduction and structure maintenance.
/// Drone aging has been moved to aging_system (W13).
pub fn decay_system(
    mut drones: Query<&mut Drone>,
    mut structures: Query<&mut Structure>,
) {
    for mut drone in drones.iter_mut() {
        drone.fatigue = drone.fatigue.saturating_sub(1);
    }

    for mut structure in structures.iter_mut() {
        structure.cooldown = structure.cooldown.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{BodyPartRegistry, DEFAULT_DRONE_LIFESPAN};
    use crate::world::create_world;
    use indexmap::IndexMap;

    fn spawn_test_drone(world: &mut crate::SwarmWorld, fatigue: u32) -> Entity {
        world
            .app
            .world_mut()
            .spawn(Drone {
                owner: 1,
                body: vec![],
                carry: IndexMap::new(),
                carry_capacity: 0,
                fatigue,
                hits: 100,
                hits_max: 100,
                spawning: false,
                age: 10,
                last_action_tick: u64::MAX,
                lifespan: DEFAULT_DRONE_LIFESPAN,
            })
            .id()
    }

    #[test]
    fn decay_reduces_fatigue() {
        let mut world = create_world();
        let drone = spawn_test_drone(&mut world, 3);

        world.app.update();

        let d = world.app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(d.fatigue, 2, "fatigue should decrement by 1");
    }

    #[test]
    fn decay_does_not_change_age() {
        // Aging is now handled by aging_system (W13), not decay.
        // The aging_system increments age by 1 per tick, but decay itself
        // does NOT touch age — verify age only changes by the expected +1 from aging.
        let mut world = create_world();
        let drone = spawn_test_drone(&mut world, 0);

        let age_before = world.app.world().entity(drone).get::<Drone>().unwrap().age;

        world.app.update();

        let age_after = world.app.world().entity(drone).get::<Drone>().unwrap().age;
        assert_eq!(
            age_after,
            age_before + 1,
            "aging system increments age by exactly 1; decay does NOT modify it"
        );
    }
}
