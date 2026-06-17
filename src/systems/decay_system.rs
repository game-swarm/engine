use bevy::prelude::*;

use crate::components::{Drone, Structure};
use crate::resources::CurrentTick;

/// Decay system — handles cooldown/fatigue reduction, drone aging, and structure maintenance.
/// Active drones (that executed a command this tick) age at ceil(1.1) = 2.
pub fn decay_system(
    current_tick: Option<Res<CurrentTick>>,
    mut drones: Query<&mut Drone>,
    mut structures: Query<&mut Structure>,
) {
    let current_tick = current_tick.map(|tick| tick.0);
    for mut drone in drones.iter_mut() {
        drone.fatigue = drone.fatigue.saturating_sub(1);
        let age_inc = if current_tick == Some(drone.last_action_tick) {
            2
        } else {
            1
        };
        drone.age = drone.age.saturating_add(age_inc);
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

    fn spawn_test_drone(world: &mut crate::SwarmWorld, last_action_tick: u64) -> Entity {
        world
            .app
            .world_mut()
            .spawn(Drone {
                owner: 1,
                body: vec![],
                carry: IndexMap::new(),
                carry_capacity: 0,
                fatigue: 3,
                hits: 100,
                hits_max: 100,
                spawning: false,
                age: 10,
                last_action_tick,
                lifespan: DEFAULT_DRONE_LIFESPAN,
            })
            .id()
    }

    #[test]
    fn idle_drone_ages_by_one() {
        let mut world = create_world();
        let drone = spawn_test_drone(&mut world, u64::MAX);

        world.app.update();

        let drone = world.app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(drone.age, 11);
        assert_eq!(drone.fatigue, 2);
    }

    #[test]
    fn active_drone_ages_by_two_on_same_tick() {
        let mut world = create_world();
        world.app.world_mut().resource_mut::<CurrentTick>().0 = 7;
        let drone = spawn_test_drone(&mut world, 7);

        world.app.update();

        let drone = world.app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(drone.age, 12);
        assert_eq!(drone.fatigue, 2);
    }

    #[test]
    fn drone_constructor_starts_idle_for_tick_zero() {
        let registry = BodyPartRegistry::default();
        let drone = Drone::new(1, vec![], &registry);

        assert_ne!(drone.last_action_tick, 0);
    }
}
