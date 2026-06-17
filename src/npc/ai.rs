use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::components::{BodyPart, BodyPartRegistry, DamageType, Drone, Position, RoomTerrains};
use crate::resources::CurrentTick;
use crate::systems::{CombatRules, PendingCombat};

pub const DEFAULT_NPC_AGGRO_RANGE: u32 = 5;
pub const DEFAULT_NPC_ATTACK_RANGE: u32 = 1;
pub const DEFAULT_NPC_DAMAGE: u32 = 30;

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Npc {
    pub npc_type: NpcType,
    pub hits: u32,
    pub hits_max: u32,
    pub damage: u32,
    pub damage_type: String,
}

impl Npc {
    pub fn new(npc_type: NpcType) -> Self {
        Self {
            npc_type,
            hits: 100,
            hits_max: 100,
            damage: DEFAULT_NPC_DAMAGE,
            damage_type: DamageType::Kinetic.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NpcType {
    Neutral,
    Guardian,
    Raider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NpcMode {
    Patrol,
    Guard,
    Wander,
    Flee,
    Attack,
}

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NpcBehavior {
    pub mode: NpcMode,
    pub idle_mode: NpcMode,
    pub home: Position,
    pub patrol_points: Vec<Position>,
    pub patrol_index: usize,
    pub aggro_range: u32,
    pub attack_range: u32,
    pub flee_below_hits: u32,
    pub target: Option<u64>,
    pub seed: u64,
}

impl NpcBehavior {
    pub fn guard(home: Position) -> Self {
        Self::new(NpcMode::Guard, home)
    }

    pub fn patrol(points: Vec<Position>) -> Self {
        let home = points.first().copied().unwrap_or(Position {
            x: 0,
            y: 0,
            room: crate::components::RoomId(0),
        });
        Self {
            mode: NpcMode::Patrol,
            idle_mode: NpcMode::Patrol,
            home,
            patrol_points: points,
            patrol_index: 0,
            aggro_range: DEFAULT_NPC_AGGRO_RANGE,
            attack_range: DEFAULT_NPC_ATTACK_RANGE,
            flee_below_hits: 0,
            target: None,
            seed: 0,
        }
    }

    pub fn wander(home: Position, seed: u64) -> Self {
        let mut behavior = Self::new(NpcMode::Wander, home);
        behavior.seed = seed;
        behavior
    }

    pub fn flee(home: Position) -> Self {
        Self::new(NpcMode::Flee, home)
    }

    fn new(mode: NpcMode, home: Position) -> Self {
        Self {
            mode,
            idle_mode: mode,
            home,
            patrol_points: Vec::new(),
            patrol_index: 0,
            aggro_range: DEFAULT_NPC_AGGRO_RANGE,
            attack_range: DEFAULT_NPC_ATTACK_RANGE,
            flee_below_hits: 0,
            target: None,
            seed: 0,
        }
    }
}

pub fn npc_ai_system(
    current_tick: Option<Res<CurrentTick>>,
    terrains: Res<RoomTerrains>,
    drones: Query<(Entity, &Position, &Drone), Without<Npc>>,
    mut npcs: Query<(Entity, &mut Position, &mut NpcBehavior, &Npc), Without<Drone>>,
) {
    let tick = current_tick.map(|tick| tick.0).unwrap_or_default();
    let drone_positions = drones
        .iter()
        .filter(|(_, _, drone)| drone.hits > 0)
        .map(|(entity, position, _)| (entity, *position))
        .collect::<Vec<_>>();

    for (entity, mut position, mut behavior, npc) in npcs.iter_mut() {
        if npc.hits == 0 {
            continue;
        }

        let nearest = nearest_drone(*position, &drone_positions);
        let aggro_target = nearest
            .filter(|(_, target_position, distance)| {
                position.room == target_position.room && *distance <= behavior.aggro_range
            })
            .map(|(target, target_position, distance)| (target, target_position, distance));

        let should_flee = behavior.idle_mode == NpcMode::Flee
            || (behavior.flee_below_hits > 0 && npc.hits <= behavior.flee_below_hits);

        if should_flee {
            behavior.mode = NpcMode::Flee;
            behavior.target = aggro_target.map(|(target, _, _)| target.to_bits());
            if let Some((_, threat_position, _)) = aggro_target {
                if let Some(next) = flee_step(*position, threat_position, &terrains) {
                    *position = next;
                }
            }
            continue;
        }

        if let Some((target, _, _)) = aggro_target {
            behavior.mode = NpcMode::Attack;
            behavior.target = Some(target.to_bits());
            continue;
        }

        if behavior.mode == NpcMode::Attack {
            behavior.mode = behavior.idle_mode;
            behavior.target = None;
        }

        match behavior.mode {
            NpcMode::Patrol => patrol_step(&mut position, &mut behavior, &terrains),
            NpcMode::Guard => {}
            NpcMode::Wander => {
                if let Some(next) = wander_step(entity, *position, behavior.seed, tick, &terrains) {
                    *position = next;
                }
            }
            NpcMode::Flee => {
                if let Some((_, threat_position, _)) = nearest {
                    if let Some(next) = flee_step(*position, threat_position, &terrains) {
                        *position = next;
                    }
                }
            }
            NpcMode::Attack => {}
        }
    }
}

pub fn npc_combat_system(
    body_registry: Res<BodyPartRegistry>,
    combat_rules: Res<CombatRules>,
    mut combat: ResMut<PendingCombat>,
    drones: Query<(Entity, &Position, &Drone), Without<Npc>>,
    npcs: Query<(&Position, &NpcBehavior, &Npc), Without<Drone>>,
) {
    let drones = drones
        .iter()
        .filter(|(_, _, drone)| drone.hits > 0)
        .map(|(entity, position, _)| (entity, *position))
        .collect::<Vec<_>>();

    for (position, behavior, npc) in npcs.iter() {
        if npc.hits == 0 || behavior.mode != NpcMode::Attack {
            continue;
        }
        let Some(target_bits) = behavior.target else {
            continue;
        };
        let target = Entity::from_bits(target_bits);
        let Some((_, target_position)) = drones.iter().find(|(entity, _)| *entity == target) else {
            continue;
        };
        if position.room != target_position.room
            || hex_distance(*position, *target_position) > behavior.attack_range
        {
            continue;
        }

        let registry_damage = body_registry.base_damage(BodyPart::Attack);
        let damage = combat_rules.scale_damage(npc.damage.max(registry_damage));
        combat.queue_typed_damage(target, npc.damage_type.clone(), damage);
    }
}

fn patrol_step(position: &mut Position, behavior: &mut NpcBehavior, terrains: &RoomTerrains) {
    if behavior.patrol_points.is_empty() {
        return;
    }
    if behavior.patrol_index >= behavior.patrol_points.len() {
        behavior.patrol_index = 0;
    }
    if *position == behavior.patrol_points[behavior.patrol_index] {
        behavior.patrol_index = (behavior.patrol_index + 1) % behavior.patrol_points.len();
    }
    if let Some(next) = step_toward(
        *position,
        behavior.patrol_points[behavior.patrol_index],
        terrains,
    ) {
        *position = next;
    }
}

fn nearest_drone(
    position: Position,
    drones: &[(Entity, Position)],
) -> Option<(Entity, Position, u32)> {
    drones
        .iter()
        .filter(|(_, drone_position)| drone_position.room == position.room)
        .map(|(entity, drone_position)| {
            (
                *entity,
                *drone_position,
                hex_distance(position, *drone_position),
            )
        })
        .min_by_key(|(entity, _, distance)| (*distance, entity.to_bits()))
}

fn step_toward(from: Position, to: Position, terrains: &RoomTerrains) -> Option<Position> {
    candidate_steps(from)
        .into_iter()
        .filter(|candidate| terrains.is_passable(*candidate))
        .min_by_key(|candidate| (hex_distance(*candidate, to), candidate.x, candidate.y))
}

fn flee_step(from: Position, threat: Position, terrains: &RoomTerrains) -> Option<Position> {
    candidate_steps(from)
        .into_iter()
        .filter(|candidate| terrains.is_passable(*candidate))
        .max_by_key(|candidate| (hex_distance(*candidate, threat), -candidate.x, -candidate.y))
}

fn wander_step(
    entity: Entity,
    from: Position,
    seed: u64,
    tick: u64,
    terrains: &RoomTerrains,
) -> Option<Position> {
    let candidates = candidate_steps(from)
        .into_iter()
        .filter(|candidate| terrains.is_passable(*candidate))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return None;
    }
    let index = deterministic_index(seed ^ entity.to_bits() ^ tick, candidates.len());
    candidates.get(index).copied()
}

fn candidate_steps(position: Position) -> [Position; 6] {
    [
        Position {
            x: position.x,
            y: position.y - 1,
            room: position.room,
        },
        Position {
            x: position.x + 1,
            y: position.y - 1,
            room: position.room,
        },
        Position {
            x: position.x + 1,
            y: position.y,
            room: position.room,
        },
        Position {
            x: position.x,
            y: position.y + 1,
            room: position.room,
        },
        Position {
            x: position.x - 1,
            y: position.y + 1,
            room: position.room,
        },
        Position {
            x: position.x - 1,
            y: position.y,
            room: position.room,
        },
    ]
}

fn deterministic_index(mut value: u64, len: usize) -> usize {
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51afd7ed558ccd);
    value ^= value >> 33;
    value = value.wrapping_mul(0xc4ceb9fe1a85ec53);
    value ^= value >> 33;
    (value as usize) % len
}

fn hex_distance(a: Position, b: Position) -> u32 {
    let dx = (a.x - b.x).unsigned_abs();
    let dy = (a.y - b.y).unsigned_abs();
    let dz = ((a.x + a.y) - (b.x + b.y)).unsigned_abs();
    (dx + dy + dz) / 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{BodyPart, RoomId};
    use crate::create_world;

    fn position(x: i32, y: i32) -> Position {
        Position {
            x,
            y,
            room: RoomId(0),
        }
    }

    #[test]
    fn patrol_moves_toward_next_waypoint() {
        let mut world = create_world();
        let npc = world
            .app
            .world_mut()
            .spawn((
                position(10, 10),
                Npc::new(NpcType::Neutral),
                NpcBehavior::patrol(vec![position(10, 10), position(12, 10)]),
            ))
            .id();

        world.run_tick();

        let npc_position = world.app.world().entity(npc).get::<Position>().unwrap();
        assert_eq!(*npc_position, position(11, 10));
    }

    #[test]
    fn guard_switches_to_attack_when_drone_enters_aggro_range() {
        let mut world = create_world();
        let npc = world
            .app
            .world_mut()
            .spawn((
                position(10, 10),
                Npc::new(NpcType::Guardian),
                NpcBehavior::guard(position(10, 10)),
            ))
            .id();
        let drone = world.spawn_drone(1, 12, 10, vec![BodyPart::Move]);
        world
            .app
            .world_mut()
            .entity_mut(drone)
            .remove::<crate::components::SpawningGrace>();

        world.run_tick();

        let behavior = world.app.world().entity(npc).get::<NpcBehavior>().unwrap();
        assert_eq!(behavior.mode, NpcMode::Attack);
        assert_eq!(behavior.target, Some(drone.to_bits()));
    }

    #[test]
    fn npc_combat_damages_drone_through_pending_combat() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 11, 10, vec![BodyPart::Move]);
        world
            .app
            .world_mut()
            .entity_mut(drone)
            .remove::<crate::components::SpawningGrace>();
        world.app.world_mut().spawn((
            position(10, 10),
            Npc::new(NpcType::Guardian),
            NpcBehavior::guard(position(10, 10)),
        ));

        world.run_tick();

        let drone = world.app.world().entity(drone).get::<Drone>().unwrap();
        assert_eq!(drone.hits, 70);
    }
}
