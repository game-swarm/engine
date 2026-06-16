use std::time::Instant;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::command::Tick;
use crate::components::{BodyPart, Controller, Drone, Source, Structure};
use crate::world::{SwarmWorld, create_world};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalSimulationSummary {
    pub ticks: Tick,
    pub final_state_checksum: u64,
    pub elapsed_ms: u128,
    pub drones: usize,
    pub sources: usize,
    pub structures: usize,
    pub controllers: usize,
}

pub fn create_local_simulation_world() -> SwarmWorld {
    let mut world = create_world();
    world.spawn_drone(
        1,
        10,
        10,
        vec![BodyPart::Move, BodyPart::Work, BodyPart::Carry],
    );
    world
}

pub fn run_local_simulation(ticks: Tick) -> LocalSimulationSummary {
    let started_at = Instant::now();
    let mut world = create_local_simulation_world();
    for _ in 0..ticks {
        world.run_tick();
    }
    summarize_local_simulation(&mut world, ticks, started_at.elapsed().as_millis())
}

pub fn summarize_local_simulation(
    world: &mut SwarmWorld,
    ticks: Tick,
    elapsed_ms: u128,
) -> LocalSimulationSummary {
    let final_state_checksum = world.state_checksum();
    let ecs = world.app.world_mut();
    LocalSimulationSummary {
        ticks,
        final_state_checksum,
        elapsed_ms,
        drones: ecs.query::<&Drone>().iter(ecs).count(),
        sources: ecs.query::<&Source>().iter(ecs).count(),
        structures: ecs.query::<&Structure>().iter(ecs).count(),
        controllers: ecs.query::<&Controller>().iter(ecs).count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_simulation_runs_ticks_and_reports_summary() {
        let summary = run_local_simulation(3);

        assert_eq!(summary.ticks, 3);
        assert_eq!(summary.drones, 1);
        assert_eq!(summary.sources, 1);
        assert!(summary.final_state_checksum > 0);
    }
}
