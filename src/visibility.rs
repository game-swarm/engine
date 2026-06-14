use std::collections::BTreeSet;

use bevy::prelude::*;

use crate::command::{ObjectId, Tick, object_id};
use crate::components::*;

pub const VISIBILITY_RADIUS: i32 = 5;

pub type VisiblePositionKey = (RoomId, i32, i32);

pub fn is_visible_to(world: &mut World, entity: Entity, player_id: PlayerId, tick: Tick) -> bool {
    let _ = tick;

    if is_owned_by(world, entity, player_id) {
        return true;
    }

    let Some(position) = world.get::<Position>(entity).copied() else {
        return false;
    };

    visible_positions(world, player_id).contains(&(position.room, position.x, position.y))
}

pub fn is_position_visible_to(world: &mut World, player_id: PlayerId, position: Position) -> bool {
    visible_positions(world, player_id).contains(&(position.room, position.x, position.y))
}

pub fn visible_positions(world: &mut World, player_id: PlayerId) -> BTreeSet<VisiblePositionKey> {
    let mut anchors = world
        .query::<(
            &Position,
            Option<&Drone>,
            Option<&Structure>,
            Option<&Controller>,
        )>()
        .iter(world)
        .filter_map(|(position, drone, structure, controller)| {
            let owned_drone = drone.is_some_and(|drone| drone.owner == player_id);
            let owned_structure =
                structure.is_some_and(|structure| structure.owner == Some(player_id));
            let owned_controller =
                controller.is_some_and(|controller| controller.owner == Some(player_id));
            (owned_drone || owned_structure || owned_controller).then_some(*position)
        })
        .collect::<Vec<_>>();
    anchors.sort_by_key(|position| (position.room.0, position.x, position.y));

    let terrains = world.resource::<RoomTerrains>();
    let mut visible = BTreeSet::new();
    for anchor in anchors {
        if let Some(room) = terrains.0.get(&anchor.room) {
            for y in (anchor.y - VISIBILITY_RADIUS)..=(anchor.y + VISIBILITY_RADIUS) {
                for x in (anchor.x - VISIBILITY_RADIUS)..=(anchor.x + VISIBILITY_RADIUS) {
                    if room.contains(x, y) {
                        visible.insert((anchor.room, x, y));
                    }
                }
            }
        }
    }
    visible
}

pub fn visible_entity_ids(world: &mut World, player_id: PlayerId, tick: Tick) -> BTreeSet<Entity> {
    let all_entities = {
        let mut query = world.query::<Entity>();
        query.iter(world).collect::<Vec<_>>()
    };
    let mut entities = all_entities
        .into_iter()
        .filter(|entity| is_visible_to(world, *entity, player_id, tick))
        .collect::<Vec<_>>();
    entities.sort_by_key(|entity| entity.index());
    entities.into_iter().collect()
}

pub fn visible_object_ids(world: &mut World, player_id: PlayerId, tick: Tick) -> Vec<ObjectId> {
    visible_entity_ids(world, player_id, tick)
        .into_iter()
        .map(object_id)
        .collect()
}

fn is_owned_by(world: &World, entity: Entity, player_id: PlayerId) -> bool {
    world
        .get::<Drone>(entity)
        .is_some_and(|drone| drone.owner == player_id)
        || world
            .get::<Structure>(entity)
            .is_some_and(|structure| structure.owner == Some(player_id))
        || world
            .get::<Controller>(entity)
            .is_some_and(|controller| controller.owner == Some(player_id))
        || world
            .get::<Owner>(entity)
            .is_some_and(|owner| owner.0 == player_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::create_world;

    #[test]
    fn own_entities_always_visible() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 49, 49, vec![BodyPart::Move]);

        assert!(is_visible_to(world.app.world_mut(), drone, 1, 7));
    }

    #[test]
    fn enemy_outside_vision_hidden() {
        let mut world = create_world();
        world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let enemy = world.spawn_drone(2, 40, 40, vec![BodyPart::Move]);

        assert!(!is_visible_to(world.app.world_mut(), enemy, 1, 7));
    }

    #[test]
    fn multiple_vision_sources_union() {
        let mut world = create_world();
        world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        world.spawn_drone(1, 30, 30, vec![BodyPart::Move]);
        let first_enemy = world.spawn_drone(2, 15, 10, vec![BodyPart::Move]);
        let second_enemy = world.spawn_drone(2, 35, 30, vec![BodyPart::Move]);

        assert!(is_visible_to(world.app.world_mut(), first_enemy, 1, 7));
        assert!(is_visible_to(world.app.world_mut(), second_enemy, 1, 7));
    }

    #[test]
    fn vision_range_boundary() {
        let mut world = create_world();
        world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let boundary = world.spawn_drone(2, 15, 10, vec![BodyPart::Move]);
        let outside = world.spawn_drone(2, 16, 10, vec![BodyPart::Move]);

        assert!(is_visible_to(world.app.world_mut(), boundary, 1, 7));
        assert!(!is_visible_to(world.app.world_mut(), outside, 1, 7));
    }
}
