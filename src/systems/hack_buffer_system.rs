use bevy::prelude::*;

use crate::components::HackBuffer;
use crate::systems::{PendingSpecialAttack, SpecialAttackKind};

pub fn hack_buffer_system(
    mut commands: Commands,
    pending: Res<PendingSpecialAttack>,
    mut buffers: Query<&mut HackBuffer>,
) {
    for intent in pending
        .intents
        .iter()
        .filter(|intent| intent.kind == SpecialAttackKind::Hack)
    {
        if let Ok(mut buffer) = buffers.get_mut(intent.target) {
            buffer.active = true;
        } else {
            commands
                .entity(intent.target)
                .insert(HackBuffer { active: true });
        }
    }
}
