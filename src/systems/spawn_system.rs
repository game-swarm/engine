use std::collections::BTreeMap;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::components::{
    BodyPart, BodyPartRegistry, Drone, Owner, PlayerId, Position, RoomId, RoomTerrains,
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
    mut commands: Commands,
    mut queue: ResMut<PendingSpawnQueue>,
    mut room_counts: ResMut<RoomDroneCounts>,
    terrains: Res<RoomTerrains>,
    body_registry: Res<BodyPartRegistry>,
    mut onboarding_events: EventWriter<OnboardingEvent>,
) {
    let pending = std::mem::take(&mut queue.0);
    for spawn in pending {
        if !terrains.is_passable(spawn.position) {
            continue;
        }

        commands.spawn((
            spawn.position,
            Owner(spawn.owner),
            Drone::new(spawn.owner, spawn.body, &body_registry),
        ));
        *room_counts
            .0
            .entry((spawn.position.room, spawn.owner))
            .or_default() += 1;
        onboarding_events.send(OnboardingEvent::DroneSpawned);
    }
}
