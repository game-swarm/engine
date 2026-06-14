use bevy::prelude::*;

use crate::components::MarkedForDeath;

pub fn death_cleanup_system(mut commands: Commands, marked: Query<Entity, With<MarkedForDeath>>) {
    for entity in marked.iter() {
        commands.entity(entity).despawn_recursive();
    }
}
