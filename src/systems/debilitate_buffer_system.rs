use bevy::prelude::*;
use swarm_engine_plugin_sdk::buffers::{PendingSpecialAttack, SpecialAttackKind};

use crate::components::DebilitateBuffer;

pub fn debilitate_buffer_system(
    mut commands: Commands,
    pending: Res<PendingSpecialAttack>,
    mut buffers: Query<&mut DebilitateBuffer>,
) {
    for intent in pending
        .intents
        .iter()
        .filter(|intent| intent.kind == SpecialAttackKind::Debilitate)
    {
        let next = DebilitateBuffer {
            damage_type: "Corrosive".to_string(),
        };
        if let Ok(mut buffer) = buffers.get_mut(intent.target) {
            *buffer = next;
        } else {
            commands.entity(intent.target).insert(next);
        }
    }
}
