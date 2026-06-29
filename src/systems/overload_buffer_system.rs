use bevy::prelude::*;

use crate::components::{OverloadBuffer, OverloadState};

pub fn overload_buffer_system(
    mut commands: Commands,
    states: Query<(Entity, &OverloadState)>,
    mut buffers: Query<&mut OverloadBuffer>,
) {
    for (entity, state) in &states {
        let next = OverloadBuffer {
            fuel_drain_per_tick: state.fuel_drain_per_tick,
            fuel_floor: state.fuel_floor,
        };
        if let Ok(mut buffer) = buffers.get_mut(entity) {
            *buffer = next;
        } else {
            commands.entity(entity).insert(next);
        }
    }
}
