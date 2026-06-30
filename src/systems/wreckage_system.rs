use bevy::prelude::*;
use indexmap::IndexMap;

use crate::components::{DeathMark, Drone, Position, Wreckage};
use crate::resources::ResourceRegistry;

pub const WRECKAGE_RATE_BP: u32 = 800;
pub const WRECKAGE_LIFESPAN_TICKS: u32 = 50;

pub fn spawn_wreckage_system(
    mut commands: Commands,
    registry: Res<ResourceRegistry>,
    destroyed: Query<(&Drone, &Position), With<DeathMark>>,
) {
    for (drone, position) in destroyed.iter() {
        let body_value = registry.body_energy_cost(&drone.body);
        let amount = (u64::from(body_value).saturating_mul(u64::from(WRECKAGE_RATE_BP)) / 10_000)
            .min(u64::from(u32::MAX)) as u32;
        if amount == 0 {
            continue;
        }

        let mut amounts = IndexMap::new();
        amounts.insert("Energy".to_string(), amount);
        commands.spawn((
            *position,
            Wreckage {
                former_owner: drone.owner,
                amounts,
                remaining_ticks: WRECKAGE_LIFESPAN_TICKS,
            },
        ));
    }
}

pub fn wreckage_decay_system(mut commands: Commands, mut wreckage: Query<(Entity, &mut Wreckage)>) {
    for (entity, mut wreckage) in wreckage.iter_mut() {
        wreckage.remaining_ticks = wreckage.remaining_ticks.saturating_sub(1);
        if wreckage.remaining_ticks == 0 {
            commands.entity(entity).despawn_recursive();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{BodyPart, DEFAULT_DRONE_LIFESPAN, RoomId};

    #[test]
    fn destroyed_drone_spawns_low_value_wreckage() {
        let mut app = App::new();
        app.init_resource::<ResourceRegistry>();
        app.add_systems(Update, spawn_wreckage_system);
        app.world_mut().spawn((
            Drone {
                owner: 1,
                body: vec![BodyPart::Move, BodyPart::Work],
                carry: IndexMap::new(),
                carry_capacity: 0,
                fatigue: 0,
                hits: 0,
                hits_max: 100,
                spawning: false,
                age: 0,
                last_action_tick: u64::MAX,
                lifespan: DEFAULT_DRONE_LIFESPAN,
            },
            Position {
                x: 4,
                y: 5,
                room: RoomId(0),
            },
            DeathMark,
        ));

        app.update();

        let wreckage = app
            .world_mut()
            .query::<&Wreckage>()
            .single(app.world())
            .unwrap();
        assert_eq!(wreckage.amounts.get("Energy"), Some(&12));
        assert_eq!(wreckage.remaining_ticks, WRECKAGE_LIFESPAN_TICKS);
    }
}
