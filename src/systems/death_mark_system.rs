use bevy::prelude::*;

use crate::components::{DeathMark, Drone, MarkedForDeath, Position, Structure};
use crate::systems::RoomDroneCounts;

pub fn death_mark_system(
    mut commands: Commands,
    drones: Query<(Entity, &Drone, Option<&Position>), Without<MarkedForDeath>>,
    structures: Query<(Entity, &Structure), Without<MarkedForDeath>>,
    mut room_counts: ResMut<RoomDroneCounts>,
) {
    for (entity, drone, position) in drones.iter() {
        if drone.hits == 0 || drone.age >= drone.lifespan {
            commands.entity(entity).insert(DeathMark);
            if let Some(position) = position {
                if let Some(count) = room_counts.0.get_mut(&(position.room, drone.owner)) {
                    *count = count.saturating_sub(1);
                }
            }
        }
    }

    for (entity, structure) in structures.iter() {
        if structure.hits == 0 {
            commands.entity(entity).insert(DeathMark);
        }
    }
}
