use bevy::prelude::*;

use crate::components::{DebilitateBuffer, DebilitateState};

pub fn debilitate_buffer_system(
    mut commands: Commands,
    states: Query<(Entity, &DebilitateState)>,
    mut buffers: Query<&mut DebilitateBuffer>,
) {
    for (entity, state) in &states {
        let next = DebilitateBuffer {
            damage_type: state.damage_type.clone(),
        };
        if let Ok(mut buffer) = buffers.get_mut(entity) {
            *buffer = next;
        } else {
            commands.entity(entity).insert(next);
        }
    }
}
