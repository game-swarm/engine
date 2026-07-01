use bevy::prelude::*;

use crate::components::DisruptBuffer;
use crate::systems::{PendingSpecialAttack, SpecialAttackKind};

pub fn disrupt_buffer_system(
    mut commands: Commands,
    pending: Res<PendingSpecialAttack>,
    mut buffers: Query<&mut DisruptBuffer>,
) {
    for intent in pending
        .intents
        .iter()
        .filter(|intent| intent.kind == SpecialAttackKind::Disrupt)
    {
        let next = DisruptBuffer::default();
        if let Ok(mut buffer) = buffers.get_mut(intent.target) {
            *buffer = next;
        } else {
            commands.entity(intent.target).insert(next);
        }
    }
}
