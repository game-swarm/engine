use bevy::prelude::*;

use crate::components::Source;

/// Phase 2b regeneration system.
///
/// Each tick the source counts down `ticks_to_regeneration`. When it reaches
/// zero the source is refilled to capacity and the countdown resets to
/// `regeneration_time`.  If the source is already full the countdown is held
/// at `regeneration_time` (no wasted ticking while full).
pub fn regeneration_system(mut sources: Query<&mut Source>) {
    for mut source in sources.iter_mut() {
        // Already full — reset timer and skip.
        if source.amount >= source.capacity {
            source.ticks_to_regeneration = source.regeneration_time;
            continue;
        }

        // Count down toward the next regeneration event.
        source.ticks_to_regeneration = source.ticks_to_regeneration.saturating_sub(1);

        if source.ticks_to_regeneration == 0 {
            // Refill to capacity and reset the countdown.
            source.amount = source.capacity;
            source.ticks_to_regeneration = source.regeneration_time;
        }
    }
}
