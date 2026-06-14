use std::collections::HashMap;

use bevy::prelude::*;

use crate::components::{BodyPart, Drone, Owner, PlayerId, Position, RoomId, RoomTerrains};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSpawn {
    pub owner: PlayerId,
    pub body: Vec<BodyPart>,
    pub position: Position,
}

#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingSpawnQueue(pub Vec<PendingSpawn>);

#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct RoomDroneCounts(pub HashMap<(RoomId, PlayerId), u32>);

pub fn spawn_system(
    mut commands: Commands,
    mut queue: ResMut<PendingSpawnQueue>,
    mut room_counts: ResMut<RoomDroneCounts>,
    terrains: Res<RoomTerrains>,
) {
    let pending = std::mem::take(&mut queue.0);
    for spawn in pending {
        if !terrains.is_passable(spawn.position) {
            continue;
        }

        commands.spawn((
            spawn.position,
            Owner(spawn.owner),
            Drone::new(spawn.owner, spawn.body),
        ));
        *room_counts
            .0
            .entry((spawn.position.room, spawn.owner))
            .or_default() += 1;
    }
}
