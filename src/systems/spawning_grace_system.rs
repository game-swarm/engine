use bevy::prelude::*;

use crate::components::SpawningGrace;

pub fn spawning_grace_system(grace_query: Query<&SpawningGrace>) {
    for grace in &grace_query {
        debug_assert!(grace.remaining > 0);
    }
}

pub fn spawning_grace_expiry_system(
    mut commands: Commands,
    mut grace_query: Query<(Entity, &mut SpawningGrace)>,
) {
    for (entity, mut grace) in grace_query.iter_mut() {
        grace.remaining = grace.remaining.saturating_sub(1);
        if grace.remaining == 0 {
            commands.entity(entity).remove::<SpawningGrace>();
        }
    }
}
