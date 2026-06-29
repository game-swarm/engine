use bevy::prelude::*;

use crate::components::{LeechBuffer, LeechState};

pub fn leech_buffer_system(
    mut commands: Commands,
    states: Query<(Entity, &LeechState)>,
    mut buffers: Query<&mut LeechBuffer>,
) {
    for (entity, state) in &states {
        let next = LeechBuffer {
            resource: state.resource.clone(),
            amount_per_tick: state.amount_per_tick,
            age_acceleration: state.age_acceleration,
        };
        if let Ok(mut buffer) = buffers.get_mut(entity) {
            *buffer = next;
        } else {
            commands.entity(entity).insert(next);
        }
    }
}
