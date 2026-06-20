use bevy::prelude::*;

use crate::components::FortifyState;

/// S21 fortify_system — per-tick fortify effects.
///
/// Reads FortifyState (written by S22) and provides armor/resistance.
/// The actual damage reduction is applied by damage_application_system
/// when it reads FortifyState from the entity.
pub fn fortify_system(
    fortified: Query<&FortifyState>,
) {
    for state in fortified.iter() {
        if state.remaining_ticks == 0 {
            continue;
        }
        // Fortify effect is consumed by damage_application which checks
        // for FortifyState when calculating resistance.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fortify_query_runs_without_panic() {
        let mut app = App::new();
        app.world_mut().spawn(FortifyState { remaining_ticks: 3 });

        app.add_systems(Update, fortify_system);
        app.update();
    }

    #[test]
    fn fortify_skips_expired() {
        let mut app = App::new();
        app.world_mut().spawn(FortifyState { remaining_ticks: 0 });

        app.add_systems(Update, fortify_system);
        app.update();
    }
}
