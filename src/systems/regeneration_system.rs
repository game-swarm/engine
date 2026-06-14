use bevy::prelude::*;

use crate::components::Source;
use crate::resources::ResourceRegistry;

/// Phase 2b regeneration system.
///
/// Each tick the source counts down `ticks_to_regeneration`. When it reaches
/// zero the source capacity is incremented by the amount defined in `produces`
/// and the countdown resets to the baseline (from ResourceRegistry).
/// If the source already has max capacity the countdown is held steady
/// (no wasted ticking while full).
pub fn regeneration_system(mut sources: Query<&mut Source>, registry: Res<ResourceRegistry>) {
    // Determine the baseline regeneration interval from the first source definition.
    // All sources share the same baseline; this avoids per-entity lookups.
    let baseline = registry
        .source("EnergyField")
        .map(|def| def.regeneration)
        .unwrap_or(300);

    for mut source in sources.iter_mut() {
        // Calculate max capacity from produces sums.
        let max_capacity = source.produces.values().sum::<u32>();

        // Already at or above max — reset timer and skip.
        if source.capacity >= max_capacity {
            source.ticks_to_regeneration = baseline;
            continue;
        }

        // Count down toward the next regeneration event.
        source.ticks_to_regeneration = source.ticks_to_regeneration.saturating_sub(1);

        if source.ticks_to_regeneration == 0 {
            // Increment capacity by the resource amounts defined in produces.
            // Each resource regenerates its contribution.
            let produced_total: u32 = source.produces.values().sum();
            let new_capacity = (source.capacity + produced_total).min(max_capacity);
            source.capacity = new_capacity;
            source.ticks_to_regeneration = baseline;
        }
    }
}
