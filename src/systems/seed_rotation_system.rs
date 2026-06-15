use bevy::prelude::*;

use crate::world::WorldConfig;

/// Tracks ticks since last seed rotation. Registered as a Bevy resource.
#[derive(Resource, Debug, Clone)]
pub struct SeedRotationState {
    pub ticks_since_rotation: u64,
    pub next_rotation_at: u64,
}

impl Default for SeedRotationState {
    fn default() -> Self {
        Self {
            ticks_since_rotation: 0,
            next_rotation_at: 0,
        }
    }
}

/// System that rotates the world seed at the configured interval.
/// Registers in the ECS Update schedule.
pub fn seed_rotation_system(config: Res<WorldConfig>, mut state: ResMut<SeedRotationState>) {
    let interval = config.world.seed_rotation_interval;
    if interval == 0 {
        return; // disabled
    }
    state.ticks_since_rotation += 1;
    if state.ticks_since_rotation >= interval {
        state.ticks_since_rotation = 0;
        state.next_rotation_at = state.next_rotation_at.wrapping_add(1);
        // Rotation event: future systems read next_rotation_at to derive new seed.
        // For now, the rotation counter serves as a seed rotation signal.
    }
}
