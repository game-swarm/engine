use bevy::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use swarm_engine::components::{Controller, Drone, Owner, PlayerId, Position, Structure};

#[derive(Resource, Debug, Clone)]
pub struct VisibilityConfig {
    pub fog_of_war: bool,
}

impl Default for VisibilityConfig {
    fn default() -> Self {
        Self { fog_of_war: true }
    }
}

#[derive(Resource, Debug, Clone, Default)]
pub struct VisibilityMap {
    pub visible_entities: BTreeMap<PlayerId, BTreeSet<Entity>>,
    pub visible_positions: BTreeMap<PlayerId, BTreeSet<PositionKey>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PositionKey {
    pub room: u32,
    pub x: i32,
    pub y: i32,
}

impl From<Position> for PositionKey {
    fn from(position: Position) -> Self {
        Self {
            room: position.room.0,
            x: position.x,
            y: position.y,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FogOfWarModPlugin;

impl Plugin for FogOfWarModPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VisibilityConfig>()
            .init_resource::<VisibilityMap>()
            .add_systems(Update, visibility_snapshot_system);
    }
}

pub fn visibility_snapshot_system(
    config: Res<VisibilityConfig>,
    mut map: ResMut<VisibilityMap>,
    all_entities: Query<(Entity, Option<&Position>)>,
    drones: Query<(&Drone, &Position)>,
    structures: Query<(&Structure, &Position)>,
    controllers: Query<(&Controller, &Position)>,
    owners: Query<(&Owner, &Position)>,
) {
    map.visible_entities.clear();
    map.visible_positions.clear();

    let mut players = BTreeSet::new();
    for (drone, _) in &drones {
        players.insert(drone.owner);
    }
    for (structure, _) in &structures {
        if let Some(owner) = structure.owner {
            players.insert(owner);
        }
    }
    for (controller, _) in &controllers {
        if let Some(owner) = controller.owner {
            players.insert(owner);
        }
    }
    for (owner, _) in &owners {
        players.insert(owner.0);
    }

    let all_position_set: BTreeSet<_> = all_entities
        .iter()
        .filter_map(|(_, position)| position.copied().map(PositionKey::from))
        .collect();

    for player in players {
        let visible_positions = if config.fog_of_war {
            player_visible_positions(player, &drones, &structures, &controllers, &owners)
        } else {
            all_position_set.clone()
        };

        let visible_entities = all_entities
            .iter()
            .filter_map(|(entity, position)| {
                position
                    .is_some_and(|position| {
                        visible_positions.contains(&PositionKey::from(*position))
                    })
                    .then_some(entity)
            })
            .collect();
        map.visible_positions.insert(player, visible_positions);
        map.visible_entities.insert(player, visible_entities);
    }
}

pub fn is_visible_to(map: &VisibilityMap, player: PlayerId, entity: Entity) -> bool {
    map.visible_entities
        .get(&player)
        .is_some_and(|visible| visible.contains(&entity))
}

fn player_visible_positions(
    player: PlayerId,
    drones: &Query<(&Drone, &Position)>,
    structures: &Query<(&Structure, &Position)>,
    controllers: &Query<(&Controller, &Position)>,
    owners: &Query<(&Owner, &Position)>,
) -> BTreeSet<PositionKey> {
    let mut anchors = Vec::new();
    let mut room_radius = 1u32;

    for (drone, position) in drones {
        if drone.owner == player {
            anchors.push((*position, 1));
        }
    }
    for (structure, position) in structures {
        if structure.owner == Some(player) {
            let radius =
                if structure.structure_type == swarm_engine::components::StructureType::OBSERVER {
                    2
                } else {
                    1
                };
            anchors.push((*position, radius));
            room_radius = room_radius.max(radius);
        }
    }
    for (controller, position) in controllers {
        if controller.owner == Some(player) {
            let radius = if controller.level >= 5 {
                1 + (controller.level - 4) as u32
            } else {
                1
            };
            anchors.push((*position, radius));
            room_radius = room_radius.max(radius);
        }
    }
    for (owner, position) in owners {
        if owner.0 == player {
            anchors.push((*position, room_radius));
        }
    }

    let mut visible = BTreeSet::new();
    for (anchor, radius) in anchors {
        for dy in -(radius as i32)..=(radius as i32) {
            for dx in -(radius as i32)..=(radius as i32) {
                if let Some(room) = anchor.room.adjacent(dx, dy) {
                    visible.insert(PositionKey::from(Position {
                        x: anchor.x,
                        y: anchor.y,
                        room,
                    }));
                }
            }
        }
    }
    visible
}
