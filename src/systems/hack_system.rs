use bevy::prelude::*;
use swarm_engine_plugin_sdk::components::{Drone, Owner, Position};

use crate::components::HackState;

/// S16 hack_system — per-tick effects of Hack status on drones.
///
/// Reads HackState (written by S22 status_advance_system) and produces
/// control effects: owner changed to 0 (neutral), fatigue set, slow/root.
/// Runs in Status Effects Parallel Set B (disjoint from other status systems).
pub fn hack_system(mut drones: Query<(Entity, &mut Drone, &mut HackState, &Owner, &Position)>) {
    for (_entity, mut drone, mut state, _owner, _pos) in drones.iter_mut() {
        if state.remaining_ticks == 0 {
            continue;
        }

        match state.stage {
            0 => {
                // Just applied — set initial hack effects
                drone.owner = 0;
                drone.fatigue = drone.fatigue.max(4);
                state.stage = 1;
            }
            1..=2 => {
                // Slow ticks
                drone.fatigue = drone.fatigue.saturating_add(2);
                state.stage += 1;
            }
            3..=4 => {
                // Root ticks
                drone.fatigue = drone.fatigue.saturating_add(4);
                state.stage += 1;
            }
            5 => {
                // Neutralized — full control
                drone.owner = 0;
                // expire on next tick via status_advance
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::DEFAULT_DRONE_LIFESPAN;
    use indexmap::IndexMap;

    fn test_drone() -> Drone {
        Drone {
            owner: 1,
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
        }
    }

    #[test]
    fn hack_stage_0_sets_owner_to_neutral_and_fatigue() {
        let mut app = App::new();
        let drone = app
            .world_mut()
            .spawn((
                test_drone(),
                HackState {
                    stage: 0,
                    remaining_ticks: 5,
                },
                Owner(1),
                Position {
                    x: 0,
                    y: 0,
                    room: swarm_engine_api::ids::RoomId(0),
                },
            ))
            .id();

        app.add_systems(Update, hack_system);
        app.update();

        let d = app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(d.owner, 0, "Hack stage 0 should set owner to neutral");
        assert!(d.fatigue >= 4, "Hack should set fatigue >= 4");
        let state = app.world().entity(drone).get::<HackState>().unwrap();
        assert_eq!(state.stage, 1, "Hack should advance to stage 1");
    }

    #[test]
    fn hack_slow_ticks_add_fatigue() {
        let mut app = App::new();
        let drone = app
            .world_mut()
            .spawn((
                Drone {
                    fatigue: 0,
                    ..test_drone()
                },
                HackState {
                    stage: 1,
                    remaining_ticks: 3,
                },
                Owner(0),
                Position {
                    x: 0,
                    y: 0,
                    room: swarm_engine_api::ids::RoomId(0),
                },
            ))
            .id();

        app.add_systems(Update, hack_system);
        app.update();

        let d = app.world().entity(drone).get::<Drone>().unwrap();
        assert!(d.fatigue >= 2, "Slow tick should add fatigue");
        let state = app.world().entity(drone).get::<HackState>().unwrap();
        assert_eq!(state.stage, 2, "Should advance to stage 2");
    }
}
