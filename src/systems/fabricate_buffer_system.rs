use bevy::prelude::*;
use swarm_engine_plugin_sdk::buffers::{PendingSpecialAttack, SpecialAttackKind};

use crate::components::FabricateBuffer;

pub fn fabricate_buffer_system(
    mut commands: Commands,
    pending: Res<PendingSpecialAttack>,
    mut buffers: Query<&mut FabricateBuffer>,
) {
    for intent in pending
        .intents
        .iter()
        .filter(|intent| intent.kind == SpecialAttackKind::Fabricate)
    {
        let next = FabricateBuffer::default();
        if let Ok(mut buffer) = buffers.get_mut(intent.target) {
            *buffer = next;
        } else {
            commands.entity(intent.target).insert(next);
        }
    }
}
