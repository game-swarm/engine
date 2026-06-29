use bevy::prelude::*;

use crate::components::{HackBuffer, HackState};

pub fn hack_buffer_system(
    mut commands: Commands,
    states: Query<(Entity, &HackState)>,
    mut buffers: Query<&mut HackBuffer>,
) {
    for (entity, state) in &states {
        if let Ok(mut buffer) = buffers.get_mut(entity) {
            buffer.active = state.remaining_ticks > 0;
        } else {
            commands.entity(entity).insert(HackBuffer {
                active: state.remaining_ticks > 0,
            });
        }
    }
}
