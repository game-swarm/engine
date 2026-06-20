use bevy::prelude::*;

use crate::components::{Drone, MarkedForDeath};

/// Aging system (S23) — increments drone age each tick.
/// On the NEXT tick, S07 death_marker catches `age >= lifespan` and inserts
/// MarkedForDeath with proper RoomCap release.
/// Filter: `Without<MarkedForDeath>` — dead drones do not continue aging.
pub fn aging_system(mut drones: Query<&mut Drone, Without<MarkedForDeath>>) {
    for mut drone in drones.iter_mut() {
        drone.age = drone.age.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::DEFAULT_DRONE_LIFESPAN;
    use indexmap::IndexMap;

    fn test_drone(age: u32, lifespan: u32) -> Drone {
        Drone {
            owner: 1,
            body: vec![],
            carry: IndexMap::new(),
            carry_capacity: 0,
            fatigue: 0,
            hits: 100,
            hits_max: 100,
            spawning: false,
            age,
            last_action_tick: u64::MAX,
            lifespan,
        }
    }

    #[test]
    fn aging_increments_drone_age() {
        let mut app = App::new();
        app.add_systems(Update, aging_system);
        let drone = app.world_mut().spawn(test_drone(5, DEFAULT_DRONE_LIFESPAN)).id();

        app.update();

        let d = app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(d.age, 6, "age should increment by 1 each tick");
    }

    #[test]
    fn aging_skips_marked_for_death() {
        let mut app = App::new();
        app.add_systems(Update, aging_system);
        let drone = app.world_mut().spawn(test_drone(1499, DEFAULT_DRONE_LIFESPAN)).id();
        app.world_mut().entity_mut(drone).insert(MarkedForDeath);

        let age_before = app.world().entity(drone).get::<Drone>().unwrap().age;

        app.update();

        let age_after = app.world().entity(drone).get::<Drone>().unwrap().age;
        assert_eq!(
            age_after, age_before,
            "MarkedForDeath drones should not continue aging"
        );
    }

    #[test]
    fn aging_saturates_at_u32_max() {
        let mut app = App::new();
        app.add_systems(Update, aging_system);
        let drone = app.world_mut().spawn(test_drone(u32::MAX, DEFAULT_DRONE_LIFESPAN)).id();

        app.update();

        let d = app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(d.age, u32::MAX, "age should saturate at u32::MAX");
    }
}
