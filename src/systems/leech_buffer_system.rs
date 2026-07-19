use bevy::prelude::*;
use swarm_engine_plugin_sdk::buffers::{PendingSpecialAttack, SpecialAttackKind};

use crate::components::LeechBuffer;

pub fn leech_buffer_system(
    mut commands: Commands,
    pending: Res<PendingSpecialAttack>,
    mut buffers: Query<&mut LeechBuffer>,
) {
    for intent in pending
        .intents
        .iter()
        .filter(|intent| intent.kind == SpecialAttackKind::Leech)
    {
        let next = LeechBuffer {
            resource: "Energy".to_string(),
            amount_per_tick: intent.amount.max(1) / 3,
            age_acceleration: 1,
        };
        if let Ok(mut buffer) = buffers.get_mut(intent.target) {
            *buffer = next;
        } else {
            commands.entity(intent.target).insert(next);
        }
    }
}
