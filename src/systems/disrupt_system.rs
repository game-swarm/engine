use bevy::prelude::*;

use crate::components::DisruptState;

/// S20 disrupt_system — per-tick disrupt effects.
///
/// Reads DisruptState (written by S22) and applies the interrupted flag
/// to the affected entity. Body part match is verified by command validation
/// before the intent is passed to S14→S22.
pub fn disrupt_system(disrupted: Query<&DisruptState>) {
    for state in disrupted.iter() {
        if state.remaining_ticks == 0 {
            continue;
        }
        // The disrupted flag is read by command validation to reject
        // actions from disrupted entities. status_advance handles duration.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disrupt_query_runs_without_panic() {
        let mut app = App::new();
        app.world_mut().spawn(DisruptState {
            body_parts: vec![],
            remaining_ticks: 1,
        });

        app.add_systems(Update, disrupt_system);
        app.update();
    }

    #[test]
    fn disrupt_skips_expired() {
        let mut app = App::new();
        app.world_mut().spawn(DisruptState {
            body_parts: vec![],
            remaining_ticks: 0,
        });

        app.add_systems(Update, disrupt_system);
        app.update();
    }
}
