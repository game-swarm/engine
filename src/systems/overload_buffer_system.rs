use bevy::prelude::*;

use crate::components::OverloadBuffer;
use crate::systems::{PendingSpecialAttack, SpecialAttackKind};

pub fn overload_buffer_system(
    mut commands: Commands,
    pending: Res<PendingSpecialAttack>,
    mut buffers: Query<&mut OverloadBuffer>,
) {
    for intent in pending
        .intents
        .iter()
        .filter(|intent| intent.kind == SpecialAttackKind::Overload)
    {
        let next = OverloadBuffer {
            fuel_drain_per_tick: intent.amount.max(100),
            fuel_floor: 200,
        };
        if let Ok(mut buffer) = buffers.get_mut(intent.target) {
            *buffer = next;
        } else {
            commands.entity(intent.target).insert(next);
        }
    }
}
