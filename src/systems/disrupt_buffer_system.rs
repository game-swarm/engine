use bevy::prelude::*;

use crate::components::{DisruptBuffer, DisruptState};

pub fn disrupt_buffer_system(
    mut commands: Commands,
    states: Query<(Entity, &DisruptState)>,
    mut buffers: Query<&mut DisruptBuffer>,
) {
    for (entity, state) in &states {
        let next = DisruptBuffer {
            body_parts: state.body_parts.clone(),
        };
        if let Ok(mut buffer) = buffers.get_mut(entity) {
            *buffer = next;
        } else {
            commands.entity(entity).insert(next);
        }
    }
}
