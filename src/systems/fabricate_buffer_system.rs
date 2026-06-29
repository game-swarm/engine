use bevy::prelude::*;

use crate::components::{FabricateBuffer, FabricateState};

pub fn fabricate_buffer_system(
    mut commands: Commands,
    states: Query<(Entity, &FabricateState)>,
    mut buffers: Query<&mut FabricateBuffer>,
) {
    for (entity, state) in &states {
        let next = FabricateBuffer {
            structure_type: state.structure_type,
        };
        if let Ok(mut buffer) = buffers.get_mut(entity) {
            *buffer = next;
        } else {
            commands.entity(entity).insert(next);
        }
    }
}
