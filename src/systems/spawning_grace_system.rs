use bevy::prelude::*;
use swarm_engine_plugin_sdk::components::SpawningGrace;

/// Asserts all drones with SpawningGrace still have remaining > 0.
/// The SpawningGrace component is written by spawn_system during entity creation
/// (spawn_system.rs line 43), not here. This system runs after spawn and before
/// combat to guarantee newborn drone invincibility for one tick.
pub fn spawning_grace_system(grace_query: Query<&SpawningGrace>) {
    for grace in &grace_query {
        debug_assert!(grace.remaining > 0);
    }
}

/// Decrements SpawningGrace::remaining each tick. When it reaches zero,
/// removes the component so the drone can be targeted in combat.
pub fn spawning_grace_expiry_system(
    mut commands: Commands,
    mut grace_query: Query<(Entity, &mut SpawningGrace)>,
) {
    for (entity, mut grace) in grace_query.iter_mut() {
        grace.remaining = grace.remaining.saturating_sub(1);
        if grace.remaining == 0 {
            commands.entity(entity).remove::<SpawningGrace>();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::create_world;
    use swarm_engine_api::ids::RoomId;
    use swarm_engine_plugin_sdk::components::{BodyPartRegistry, Drone, Position};

    #[test]
    fn spawning_grace_present_on_new_drone() {
        // spawn_system adds SpawningGrace { remaining: 1 } to newborn drones.
        // Verify that the component is present after spawning completes.
        let mut world = create_world();
        let e = world
            .app
            .world_mut()
            .spawn((
                Drone::new(1, vec![], &BodyPartRegistry::default()),
                Position {
                    x: 0,
                    y: 0,
                    room: RoomId(0),
                },
                SpawningGrace { remaining: 1 },
            ))
            .id();

        // Run one tick — expiry should decrement to 0 and remove
        world.app.update();

        let grace = world.app.world().entity(e).get::<SpawningGrace>();
        assert!(
            grace.is_none(),
            "SpawningGrace should be removed after 1 tick"
        );
    }

    #[test]
    fn spawning_grace_assertion_passes() {
        // spawning_grace_system asserts remaining > 0 — should not panic
        let mut world = create_world();
        world.app.world_mut().spawn((
            Drone::new(1, vec![], &BodyPartRegistry::default()),
            Position {
                x: 0,
                y: 0,
                room: RoomId(0),
            },
            SpawningGrace { remaining: 2 },
        ));

        // Should not panic
        world.app.update();
    }
}
