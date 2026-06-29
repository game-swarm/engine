use bevy::prelude::*;

use crate::components::{DrainBuffer, DrainState};

pub fn drain_buffer_system(
    mut commands: Commands,
    states: Query<(Entity, &DrainState)>,
    mut buffers: Query<&mut DrainBuffer>,
) {
    for (entity, state) in &states {
        let next = DrainBuffer {
            resource: state.resource.clone(),
            amount_per_tick: state.amount_per_tick,
        };
        if let Ok(mut buffer) = buffers.get_mut(entity) {
            *buffer = next;
        } else {
            commands.entity(entity).insert(next);
        }
    }
}
