use crate::commands::{BodyPart, CommandAction, CommandIntent, Direction, StructureType};
use crate::types_template::{
    ObjectKind, ObjectSnapshot, Position, ResourceMap, Snapshot, TickResult,
};

const ENERGY: &str = "Energy";
const WORKER_BODY: [BodyPart; 1] = [BodyPart::Work];
const WORKER_COST: u32 = 100;
const DEFAULT_CARRY_CAPACITY: u32 = 100;

pub fn tick(snapshot: Snapshot) -> TickResult {
    let Some(spawn) = find_my_spawn(&snapshot) else {
        return TickResult { commands: vec![] };
    };
    let Some(source) = find_energy_source(&snapshot.objects) else {
        return TickResult { commands: vec![] };
    };

    let mut commands = Vec::new();
    let mut sequence = 0;

    if spawn_energy(spawn, &snapshot) >= WORKER_COST
        && structure_cooldown(spawn) == 0
        && let Some(actor) = find_my_workers(&snapshot).next()
    {
        commands.push(command(
            sequence,
            CommandAction::Spawn {
                object_id: actor.id,
                spawn_id: spawn.id,
                body_parts: WORKER_BODY.to_vec(),
            },
        ));
        sequence += 1;
    }

    for drone in find_my_workers(&snapshot) {
        if drone_spawning(drone) || drone_fatigue(drone) > 0 {
            continue;
        }

        let carried_energy = carried_energy(drone);
        let carry_capacity = drone_carry_capacity(drone).unwrap_or(DEFAULT_CARRY_CAPACITY);
        if carried_energy >= DEFAULT_CARRY_CAPACITY.min(carry_capacity) {
            commands.push(if is_near(drone, spawn) {
                command(
                    sequence,
                    CommandAction::Transfer {
                        object_id: drone.id,
                        target_id: spawn.id,
                        resource: ENERGY.to_string(),
                        amount: carried_energy,
                    },
                )
            } else {
                move_toward(sequence, drone, &spawn.position)
            });
            sequence += 1;
            continue;
        }

        commands.push(if is_near(drone, source) {
            command(
                sequence,
                CommandAction::Harvest {
                    object_id: drone.id,
                    target_id: source.id,
                    resource: Some(ENERGY.to_string()),
                },
            )
        } else {
            move_toward(sequence, drone, &source.position)
        });
        sequence += 1;
    }

    TickResult { commands }
}

pub fn has_enough_energy_for_worker(snapshot: &Snapshot) -> bool {
    let available = find_my_spawn(snapshot)
        .and_then(structure_store)
        .or_else(|| snapshot.player.as_ref().map(|player| &player.resources));

    available
        .and_then(|resources| resources.get(ENERGY))
        .copied()
        .unwrap_or_default()
        >= WORKER_COST
}

fn command(sequence: u32, action: CommandAction) -> CommandIntent {
    CommandIntent { sequence, action }
}

fn find_my_spawn(snapshot: &Snapshot) -> Option<&ObjectSnapshot> {
    snapshot.objects.iter().find(|object| match &object.kind {
        ObjectKind::Structure {
            structure, owner, ..
        } => owner == &Some(snapshot.player_id) && *structure == StructureType::Spawn,
        _ => false,
    })
}

fn find_my_workers(snapshot: &Snapshot) -> impl Iterator<Item = &ObjectSnapshot> {
    snapshot.objects.iter().filter(|object| match &object.kind {
        ObjectKind::Drone { owner, body, .. } => {
            *owner == snapshot.player_id && body.contains(&BodyPart::Work)
        }
        _ => false,
    })
}

fn find_energy_source(objects: &[ObjectSnapshot]) -> Option<&ObjectSnapshot> {
    objects.iter().find(|object| match &object.kind {
        ObjectKind::Source { produces, .. } => {
            produces.get(ENERGY).copied().unwrap_or_default() > 0
        }
        _ => false,
    })
}

fn spawn_energy(spawn: &ObjectSnapshot, snapshot: &Snapshot) -> u32 {
    structure_store(spawn)
        .and_then(|store| store.get(ENERGY).copied())
        .or_else(|| {
            snapshot
                .player
                .as_ref()
                .and_then(|player| player.resources.get(ENERGY).copied())
        })
        .unwrap_or_default()
}

fn structure_store(object: &ObjectSnapshot) -> Option<&ResourceMap> {
    match &object.kind {
        ObjectKind::Structure { store, .. } => Some(store),
        _ => None,
    }
}

fn structure_cooldown(object: &ObjectSnapshot) -> u32 {
    match &object.kind {
        ObjectKind::Structure { cooldown, .. } => *cooldown,
        _ => 0,
    }
}

fn drone_spawning(object: &ObjectSnapshot) -> bool {
    match &object.kind {
        ObjectKind::Drone { spawning, .. } => *spawning,
        _ => false,
    }
}

fn drone_fatigue(object: &ObjectSnapshot) -> u32 {
    match &object.kind {
        ObjectKind::Drone { fatigue, .. } => *fatigue,
        _ => 0,
    }
}

fn carried_energy(object: &ObjectSnapshot) -> u32 {
    match &object.kind {
        ObjectKind::Drone { carry, .. } => carry.get(ENERGY).copied().unwrap_or_default(),
        _ => 0,
    }
}

fn drone_carry_capacity(object: &ObjectSnapshot) -> Option<u32> {
    match &object.kind {
        ObjectKind::Drone { body, .. } => {
            let carry_parts = body.iter().filter(|part| **part == BodyPart::Carry).count() as u32;
            (carry_parts > 0).then_some(carry_parts * 50)
        }
        _ => None,
    }
}

fn move_toward(sequence: u32, actor: &ObjectSnapshot, target: &Position) -> CommandIntent {
    let direction = direction_toward(&actor.position, target).unwrap_or(Direction::BottomRight);
    command(
        sequence,
        CommandAction::Move {
            object_id: actor.id,
            direction,
        },
    )
}

fn direction_toward(from: &Position, to: &Position) -> Option<Direction> {
    if from.room != to.room || (from.x == to.x && from.y == to.y) {
        return None;
    }

    const DIRECTIONS: [Direction; 6] = [
        Direction::Top,
        Direction::TopRight,
        Direction::BottomRight,
        Direction::Bottom,
        Direction::BottomLeft,
        Direction::TopLeft,
    ];

    DIRECTIONS.into_iter().min_by_key(|direction| {
        let (dx, dy) = direction.offset();
        let next = Position {
            x: from.x + dx,
            y: from.y + dy,
            room: from.room,
        };
        hex_distance(&next, to)
    })
}

fn is_near(a: &ObjectSnapshot, b: &ObjectSnapshot) -> bool {
    a.position.room == b.position.room
        && (a.position.x - b.position.x)
            .abs()
            .max((a.position.y - b.position.y).abs())
            <= 1
}

fn hex_distance(from: &Position, to: &Position) -> u32 {
    if from.room != to.room {
        return u32::MAX;
    }
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    dx.abs().max(dy.abs()).max((dx + dy).abs()) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::StructureType;
    use crate::types_template::PlayerSnapshot;
    use std::collections::BTreeMap;

    #[test]
    fn starter_bot_spawns_and_harvests_with_worker() {
        let snapshot = Snapshot {
            tick: 1,
            player_id: 7,
            rooms: vec![],
            objects: vec![
                structure(10, 0, 0, StructureType::Spawn, Some(7), [(ENERGY, 100)], 0),
                source(20, 2, 0, [(ENERGY, 300)]),
                drone(30, 1, 0, 7, vec![BodyPart::Work], 0, false, []),
            ],
            player: None,
        };

        let result = tick(snapshot);

        assert_eq!(result.commands.len(), 2);
        assert!(matches!(
            result.commands[0].action,
            CommandAction::Spawn {
                object_id: 30,
                spawn_id: 10,
                ..
            }
        ));
        assert_eq!(
            result.commands[1].action,
            CommandAction::Harvest {
                object_id: 30,
                target_id: 20,
                resource: Some(ENERGY.to_string())
            }
        );
    }

    #[test]
    fn starter_bot_transfers_full_worker_to_spawn() {
        let snapshot = Snapshot {
            tick: 1,
            player_id: 7,
            rooms: vec![],
            objects: vec![
                structure(10, 0, 0, StructureType::Spawn, Some(7), [], 1),
                source(20, 5, 5, [(ENERGY, 300)]),
                drone(
                    30,
                    1,
                    0,
                    7,
                    vec![BodyPart::Work, BodyPart::Carry],
                    0,
                    false,
                    [(ENERGY, 50)],
                ),
            ],
            player: Some(PlayerSnapshot {
                id: 7,
                resources: resource_map([]),
                global_storage: resource_map([]),
            }),
        };

        let result = tick(snapshot);

        assert_eq!(
            result.commands[0].action,
            CommandAction::Transfer {
                object_id: 30,
                target_id: 10,
                resource: ENERGY.to_string(),
                amount: 50,
            }
        );
    }

    #[test]
    fn direction_toward_steps_closer() {
        let from = Position {
            x: 0,
            y: 0,
            room: 1,
        };
        let to = Position {
            x: 3,
            y: 0,
            room: 1,
        };

        assert_eq!(direction_toward(&from, &to), Some(Direction::BottomRight));
    }

    #[allow(clippy::too_many_arguments)]
    fn drone<const N: usize>(
        id: u64,
        x: i32,
        y: i32,
        owner: u32,
        body: Vec<BodyPart>,
        fatigue: u32,
        spawning: bool,
        carry: [(&str, u32); N],
    ) -> ObjectSnapshot {
        ObjectSnapshot {
            id,
            position: Position { x, y, room: 1 },
            kind: ObjectKind::Drone {
                owner,
                body,
                fatigue,
                hits: 100,
                hits_max: 100,
                spawning,
                age: 1,
                carry: resource_map(carry),
            },
        }
    }

    fn structure<const N: usize>(
        id: u64,
        x: i32,
        y: i32,
        structure: StructureType,
        owner: Option<u32>,
        store: [(&str, u32); N],
        cooldown: u32,
    ) -> ObjectSnapshot {
        ObjectSnapshot {
            id,
            position: Position { x, y, room: 1 },
            kind: ObjectKind::Structure {
                structure,
                owner,
                hits: 1000,
                hits_max: 1000,
                store: resource_map(store),
                cooldown,
            },
        }
    }

    fn source<const N: usize>(
        id: u64,
        x: i32,
        y: i32,
        produces: [(&str, u32); N],
    ) -> ObjectSnapshot {
        ObjectSnapshot {
            id,
            position: Position { x, y, room: 1 },
            kind: ObjectKind::Source {
                produces: resource_map(produces),
                capacity: 300,
                ticks_to_regeneration: 0,
            },
        }
    }

    fn resource_map<const N: usize>(items: [(&str, u32); N]) -> BTreeMap<String, u32> {
        items
            .into_iter()
            .map(|(resource, amount)| (resource.to_string(), amount))
            .collect()
    }
}
