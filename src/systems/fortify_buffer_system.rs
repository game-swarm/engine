use bevy::prelude::*;

use crate::components::{FortifyBuffer, FortifyState};

pub fn fortify_buffer_system(
    mut commands: Commands,
    states: Query<(Entity, &FortifyState)>,
    mut buffers: Query<&mut FortifyBuffer>,
) {
    for (entity, state) in &states {
        if let Ok(mut buffer) = buffers.get_mut(entity) {
            buffer.active = state.remaining_ticks > 0;
        } else {
            commands.entity(entity).insert(FortifyBuffer {
                active: state.remaining_ticks > 0,
            });
        }
    }
}
