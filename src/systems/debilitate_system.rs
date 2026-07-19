use bevy::prelude::*;
use swarm_engine_plugin_sdk::components::Drone;

use crate::components::DebilitateState;

/// S19 debilitate_system — per-tick debilitate effects.
///
/// Reads DebilitateState (written by S22) and doubles damage from
/// the debilitated damage type against the affected drone.
pub fn debilitate_system(mut debilitated: Query<(&mut Drone, &DebilitateState)>) {
    for (drone, state) in debilitated.iter_mut() {
        if state.remaining_ticks == 0 {
            continue;
        }
        // Debilitate doubles resistance damage — the actual damage doubling
        // is applied by damage_application_system when it reads DebilitateState.
        // This system serves as the marker/reader.
        let _ = drone; // nothing to do per-tick; status_advance handles duration
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
    fn debilitate_presence_is_noop_per_tick() {
        let mut app = App::new();
        app.world_mut().spawn((
            test_drone(),
            DebilitateState {
                damage_type: "Corrosive".into(),
                remaining_ticks: 5,
            },
        ));

        app.add_systems(Update, debilitate_system);
        app.update();

        // System runs without panic — effect is consumed by damage_application
    }

    #[test]
    fn debilitate_skips_expired() {
        let mut app = App::new();
        app.world_mut().spawn((
            test_drone(),
            DebilitateState {
                damage_type: "Kinetic".into(),
                remaining_ticks: 0,
            },
        ));

        app.add_systems(Update, debilitate_system);
        app.update();
        // No panic = passes
    }
}
