use bevy::prelude::*;

use crate::components::DrainBuffer;
use crate::systems::{PendingSpecialAttack, SpecialAttackKind};

pub fn drain_buffer_system(
    mut commands: Commands,
    pending: Res<PendingSpecialAttack>,
    mut buffers: Query<&mut DrainBuffer>,
) {
    for intent in pending
        .intents
        .iter()
        .filter(|intent| intent.kind == SpecialAttackKind::Drain)
    {
        let next = DrainBuffer {
            resource: "energy".to_string(),
            amount_per_tick: intent.amount.max(1) / 3,
        };
        if let Ok(mut buffer) = buffers.get_mut(intent.target) {
            *buffer = next;
        } else {
            commands.entity(intent.target).insert(next);
        }
    }
}
