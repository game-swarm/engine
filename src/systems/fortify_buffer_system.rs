use bevy::prelude::*;
use swarm_engine_plugin_sdk::buffers::{PendingSpecialAttack, SpecialAttackKind};

use crate::components::FortifyBuffer;

pub fn fortify_buffer_system(
    mut commands: Commands,
    pending: Res<PendingSpecialAttack>,
    mut buffers: Query<&mut FortifyBuffer>,
) {
    for intent in pending
        .intents
        .iter()
        .filter(|intent| intent.kind == SpecialAttackKind::Fortify)
    {
        if let Ok(mut buffer) = buffers.get_mut(intent.target) {
            buffer.active = true;
        } else {
            commands
                .entity(intent.target)
                .insert(FortifyBuffer { active: true });
        }
    }
}
