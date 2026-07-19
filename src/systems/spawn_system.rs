use std::collections::BTreeMap;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use swarm_engine_api::ids::{BodyPart, PlayerId, RoomId};
use swarm_engine_plugin_sdk::components::{
    BodyPartRegistry, Drone, Owner, Position, SpawningGrace,
};

use crate::components::{
    PendingEntityCreation, PendingEntityCreationEntry, PendingEntityKind, RoomTerrains,
    StableEntityIdAllocator,
};
use crate::onboarding::OnboardingEvent;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSpawn {
    pub owner: PlayerId,
    pub body: Vec<BodyPart>,
    pub position: Position,
}

#[derive(Resource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSpawnQueue(pub Vec<PendingSpawn>);

#[derive(Resource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomDroneCounts(pub BTreeMap<(RoomId, PlayerId), u32>);

pub fn spawn_system(
    mut queue: ResMut<PendingSpawnQueue>,
    mut room_counts: ResMut<RoomDroneCounts>,
    mut pending_entities: ResMut<PendingEntityCreation>,
    mut stable_ids: ResMut<StableEntityIdAllocator>,
    terrains: Res<RoomTerrains>,
    mut onboarding_events: MessageWriter<OnboardingEvent>,
) {
    let pending = std::mem::take(&mut queue.0);
    for spawn in pending {
        if !terrains.is_passable(spawn.position) {
            continue;
        }

        let stable_id = stable_ids.allocate();
        pending_entities.entries.push(PendingEntityCreationEntry {
            stable_id,
            kind: PendingEntityKind::Drone {
                owner: spawn.owner,
                body: spawn.body,
                position: spawn.position,
                spawning_grace: 1,
            },
        });
        *room_counts
            .0
            .entry((spawn.position.room, spawn.owner))
            .or_default() += 1;
        onboarding_events.write(OnboardingEvent::DroneSpawned);
    }
}

pub fn flush_pending_entity_creation_system(
    mut commands: Commands,
    mut pending_entities: ResMut<PendingEntityCreation>,
    body_registry: Res<BodyPartRegistry>,
) {
    let mut entries = std::mem::take(&mut pending_entities.entries);
    entries.sort_by_key(|entry| entry.stable_id);
    for entry in entries {
        match entry.kind {
            PendingEntityKind::Drone {
                owner,
                body,
                position,
                spawning_grace,
            } => {
                let mut entity = commands.spawn((
                    entry.stable_id,
                    position,
                    Owner(owner),
                    Drone::new(owner, body, &body_registry),
                ));
                if spawning_grace > 0 {
                    entity.insert(SpawningGrace {
                        remaining: spawning_grace,
                    });
                }
            }
            PendingEntityKind::Structure {
                position,
                structure,
            } => {
                commands.spawn((entry.stable_id, position, structure));
            }
        }
    }
}
