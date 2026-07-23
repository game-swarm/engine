use bevy::prelude::*;
use swarm_engine_plugin_sdk::components::{DeathMark, Drone, Owner, Position, SpawningGrace};

use crate::components::{FabricateBuffer, FabricateState};

pub fn fabricate_buffer_system(
    mut commands: Commands,
    states: Query<(Entity, &FabricateState)>,
    entities: Query<(
        Option<&Position>,
        Option<&Owner>,
        Option<&Drone>,
        Has<DeathMark>,
        Has<SpawningGrace>,
    )>,
    mut buffers: Query<&mut FabricateBuffer>,
) {
    let mut channels = states.iter().collect::<Vec<_>>();
    channels.sort_by_key(|(source, _)| source.to_bits());
    for (source, state) in channels {
        // These reads are part of S22b's declared validation input. S22 remains
        // the authority that expires or completes the channel.
        let _ = entities.get(source);
        let _ = entities.get(state.target);
        let channel_delta = 1;
        let next = FabricateBuffer {
            source: state.source,
            target: state.target,
            resolved_structure_type: state.resolved_structure_type,
            channel_delta,
            complete: state.channel_remaining <= channel_delta,
        };
        if let Ok(mut buffer) = buffers.get_mut(source) {
            *buffer = next;
        } else {
            commands.entity(source).insert(next);
        }
    }
}
