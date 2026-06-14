use bevy::prelude::*;
use indexmap::IndexMap;

use crate::components::*;
use crate::systems::{PendingSpawn, PendingSpawnQueue, RoomDroneCounts};

pub type ObjectId = u64;
pub type Tick = u64;

pub const MAX_BODY_PARTS: usize = 50;
pub const MAX_DRONES_PER_PLAYER: u32 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandSource {
    Wasm,
    Mcp,
    Rest,
    AdminCli,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Top,
    TopRight,
    BottomRight,
    Bottom,
    BottomLeft,
    TopLeft,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandAction {
    Move {
        object_id: ObjectId,
        direction: Direction,
    },
    Harvest {
        object_id: ObjectId,
        target_id: ObjectId,
        resource: Option<String>,
    },
    Transfer {
        object_id: ObjectId,
        target_id: ObjectId,
        resource: String,
        amount: u32,
    },
    Withdraw {
        object_id: ObjectId,
        target_id: ObjectId,
        resource: String,
        amount: u32,
    },
    Attack {
        object_id: ObjectId,
        target_id: ObjectId,
    },
    Heal {
        object_id: ObjectId,
        target_id: ObjectId,
    },
    SpawnDrone {
        spawn_id: ObjectId,
        body: Vec<BodyPart>,
    },
}

/// Untrusted command shape emitted by a player module. Envelope fields are not
/// representable here; Source Gate is the only path to `RawCommand`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandIntent {
    pub sequence: u32,
    pub action: CommandAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawCommand {
    pub player_id: PlayerId,
    pub tick: Tick,
    pub source: CommandSource,
    pub sequence: u32,
    pub action: CommandAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCommand {
    pub raw: RawCommand,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectionReason {
    ObjectNotFound,
    NotOwner,
    NotMovable,
    Fatigued,
    MissingBodyPart {
        part: BodyPart,
    },
    TileBlocked,
    InvalidDirection,
    StillSpawning,
    OutOfRoom,
    InsufficientResource {
        resource: String,
        required: u32,
        available: u32,
    },
    CarryFull,
    NotSource,
    SourceEmpty,
    OutOfRange {
        distance: u32,
        max: u32,
    },
    TargetFull,
    TargetEmpty,
    TileOccupied,
    InvalidTerrain,
    AlreadyFullHealth,
    FriendlyTarget,
    NotYourSpawn,
    SpawnOnCooldown,
    BodyTooLarge,
    ExceedsRoomCapacity,
    RoomDroneCapReached,
    NotFriendly,
}

pub type CommandResult = Result<(), RejectionReason>;

pub fn source_gate(
    player_id: PlayerId,
    tick: Tick,
    source: CommandSource,
    intent: CommandIntent,
) -> RawCommand {
    RawCommand {
        player_id,
        tick,
        source,
        sequence: intent.sequence,
        action: intent.action,
    }
}

pub fn object_id(entity: Entity) -> ObjectId {
    entity.to_bits()
}

pub fn validate_command(
    world: &mut World,
    raw: RawCommand,
) -> Result<ValidatedCommand, RejectionReason> {
    match &raw.action {
        CommandAction::Move {
            object_id,
            direction,
        } => validate_move(world, raw.player_id, *object_id, *direction),
        CommandAction::Harvest {
            object_id,
            target_id,
            resource: _,
        } => validate_harvest(world, raw.player_id, *object_id, *target_id),
        CommandAction::Transfer {
            object_id,
            target_id,
            resource,
            amount,
        } => validate_transfer(
            world,
            raw.player_id,
            *object_id,
            *target_id,
            resource,
            *amount,
        ),
        CommandAction::Withdraw {
            object_id,
            target_id,
            resource,
            amount,
        } => validate_withdraw(
            world,
            raw.player_id,
            *object_id,
            *target_id,
            resource,
            *amount,
        ),
        CommandAction::Attack {
            object_id,
            target_id,
        } => validate_attack(world, raw.player_id, *object_id, *target_id),
        CommandAction::Heal {
            object_id,
            target_id,
        } => validate_heal(world, raw.player_id, *object_id, *target_id),
        CommandAction::SpawnDrone { spawn_id, body } => {
            validate_spawn_drone(world, raw.player_id, *spawn_id, body)
        }
    }?;

    Ok(ValidatedCommand { raw })
}

pub fn apply_command(world: &mut World, command: ValidatedCommand) -> CommandResult {
    match command.raw.action {
        CommandAction::Move {
            object_id,
            direction,
        } => apply_move(world, object_id, direction),
        CommandAction::Harvest {
            object_id,
            target_id,
            resource,
        } => apply_harvest(world, object_id, target_id, resource),
        CommandAction::Transfer {
            object_id,
            target_id,
            resource,
            amount,
        } => apply_transfer(world, object_id, target_id, &resource, amount),
        CommandAction::Withdraw {
            object_id,
            target_id,
            resource,
            amount,
        } => apply_withdraw(world, object_id, target_id, &resource, amount),
        CommandAction::Attack {
            object_id,
            target_id,
        } => apply_attack(world, object_id, target_id),
        CommandAction::Heal {
            object_id,
            target_id,
        } => apply_heal(world, object_id, target_id),
        CommandAction::SpawnDrone { spawn_id, body } => {
            apply_spawn_drone(world, command.raw.player_id, spawn_id, body)
        }
    }
}

fn validate_move(
    world: &mut World,
    player_id: PlayerId,
    object_id: ObjectId,
    direction: Direction,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_act(&drone, BodyPart::Move, true)?;
    let target = step(position, direction).ok_or(RejectionReason::InvalidDirection)?;

    if !world.resource::<RoomTerrains>().is_passable(target) {
        return Err(RejectionReason::TileBlocked);
    }
    if tile_has_blocking_enemy(world, target, player_id) {
        return Err(RejectionReason::TileBlocked);
    }
    Ok(())
}

fn validate_harvest(
    world: &mut World,
    player_id: PlayerId,
    object_id: ObjectId,
    target_id: ObjectId,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_act(&drone, BodyPart::Work, true)?;
    require_body(&drone, BodyPart::Carry)?;
    if carry_used(&drone.carry) >= drone.carry_capacity {
        return Err(RejectionReason::CarryFull);
    }

    let (target_position, source) = source_snapshot(world, target_id)?;
    if source.amount == 0 {
        return Err(RejectionReason::SourceEmpty);
    }
    ensure_range(position, target_position, 1)
}

fn validate_transfer(
    world: &mut World,
    player_id: PlayerId,
    object_id: ObjectId,
    target_id: ObjectId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    require_body(&drone, BodyPart::Carry)?;
    let available = *drone.carry.get(resource).unwrap_or(&0);
    if available < amount {
        return Err(RejectionReason::InsufficientResource {
            resource: resource.to_string(),
            required: amount,
            available,
        });
    }

    let (target_position, space) = target_resource_space(world, target_id, resource)?;
    if space < amount {
        return Err(RejectionReason::TargetFull);
    }
    ensure_range(position, target_position, 1)
}

fn validate_withdraw(
    world: &mut World,
    player_id: PlayerId,
    object_id: ObjectId,
    target_id: ObjectId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    require_body(&drone, BodyPart::Carry)?;
    let space = drone
        .carry_capacity
        .saturating_sub(carry_used(&drone.carry));
    if space < amount {
        return Err(RejectionReason::TargetFull);
    }

    let (target_position, available) = target_resource_amount(world, target_id, resource)?;
    if available == 0 {
        return Err(RejectionReason::TargetEmpty);
    }
    if available < amount {
        return Err(RejectionReason::InsufficientResource {
            resource: resource.to_string(),
            required: amount,
            available,
        });
    }
    ensure_range(position, target_position, 1)
}

fn validate_attack(
    world: &mut World,
    player_id: PlayerId,
    object_id: ObjectId,
    target_id: ObjectId,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    ensure_drone_can_act(&drone, BodyPart::Attack, true)?;
    let (target_position, target_owner) = attackable_snapshot(world, target_id)?;
    if target_owner == Some(player_id) {
        return Err(RejectionReason::FriendlyTarget);
    }
    ensure_range(position, target_position, 1)
}

fn validate_heal(
    world: &mut World,
    player_id: PlayerId,
    object_id: ObjectId,
    target_id: ObjectId,
) -> CommandResult {
    let (position, drone) = drone_snapshot(world, object_id)?;
    ensure_owner(&drone, player_id)?;
    require_body(&drone, BodyPart::Heal)?;
    let (target_position, target) = drone_snapshot(world, target_id)?;
    if target.owner != player_id {
        return Err(RejectionReason::NotFriendly);
    }
    if target.hits >= target.hits_max {
        return Err(RejectionReason::AlreadyFullHealth);
    }
    ensure_range(position, target_position, 3)
}

fn validate_spawn_drone(
    world: &mut World,
    player_id: PlayerId,
    spawn_id: ObjectId,
    body: &[BodyPart],
) -> CommandResult {
    let (position, structure) = structure_snapshot(world, spawn_id)?;
    if structure.structure_type != StructureType::Spawn || structure.owner != Some(player_id) {
        return Err(RejectionReason::NotYourSpawn);
    }
    if structure.cooldown > 0 {
        return Err(RejectionReason::SpawnOnCooldown);
    }
    if body.len() > MAX_BODY_PARTS {
        return Err(RejectionReason::BodyTooLarge);
    }
    let cost = body_cost(body);
    let energy = structure.energy.unwrap_or(0);
    if cost > structure.energy_capacity.unwrap_or(0) {
        return Err(RejectionReason::ExceedsRoomCapacity);
    }
    if cost > energy {
        return Err(RejectionReason::InsufficientResource {
            resource: "Energy".to_string(),
            required: cost,
            available: energy,
        });
    }
    if world
        .resource::<RoomDroneCounts>()
        .0
        .get(&(position.room, player_id))
        .copied()
        .unwrap_or_default()
        >= MAX_DRONES_PER_PLAYER
    {
        return Err(RejectionReason::RoomDroneCapReached);
    }
    let spawn_position = spawn_output_position(position);
    if !world.resource::<RoomTerrains>().is_passable(spawn_position) {
        return Err(RejectionReason::InvalidTerrain);
    }
    if tile_has_any_drone(world, spawn_position) {
        return Err(RejectionReason::TileOccupied);
    }
    Ok(())
}

fn apply_move(world: &mut World, object_id: ObjectId, direction: Direction) -> CommandResult {
    let entity = entity(object_id)?;
    let mut entity = world.entity_mut(entity);
    let mut position = entity
        .get_mut::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    *position = step(*position, direction).ok_or(RejectionReason::InvalidDirection)?;
    Ok(())
}

fn apply_harvest(
    world: &mut World,
    object_id: ObjectId,
    target_id: ObjectId,
    resource: Option<String>,
) -> CommandResult {
    let resource = resource.unwrap_or_else(|| "Energy".to_string());
    let object = entity(object_id)?;
    let target = entity(target_id)?;
    let (_, drone) = drone_snapshot(world, object_id)?;
    let work_parts = drone
        .body
        .iter()
        .filter(|part| **part == BodyPart::Work)
        .count() as u32;
    let free_capacity = drone
        .carry_capacity
        .saturating_sub(carry_used(&drone.carry));
    let amount = world
        .entity(target)
        .get::<crate::components::Source>()
        .ok_or(RejectionReason::NotSource)?
        .amount
        .min(free_capacity)
        .min(work_parts.max(1) * 2);

    world
        .entity_mut(target)
        .get_mut::<crate::components::Source>()
        .unwrap()
        .amount -= amount;
    *world
        .entity_mut(object)
        .get_mut::<Drone>()
        .unwrap()
        .carry
        .entry(resource)
        .or_default() += amount;
    Ok(())
}

fn apply_transfer(
    world: &mut World,
    object_id: ObjectId,
    target_id: ObjectId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    let object = entity(object_id)?;
    let target = entity(target_id)?;
    take_from_drone(world, object, resource, amount);
    add_to_target(world, target, resource, amount)
}

fn apply_withdraw(
    world: &mut World,
    object_id: ObjectId,
    target_id: ObjectId,
    resource: &str,
    amount: u32,
) -> CommandResult {
    let object = entity(object_id)?;
    let target = entity(target_id)?;
    take_from_target(world, target, resource, amount)?;
    *world
        .entity_mut(object)
        .get_mut::<Drone>()
        .unwrap()
        .carry
        .entry(resource.to_string())
        .or_default() += amount;
    Ok(())
}

fn apply_attack(world: &mut World, object_id: ObjectId, target_id: ObjectId) -> CommandResult {
    let (_, drone) = drone_snapshot(world, object_id)?;
    let damage = drone
        .body
        .iter()
        .filter(|part| **part == BodyPart::Attack)
        .count() as u32
        * 30;
    let target = entity(target_id)?;
    if let Some(mut target_drone) = world.entity_mut(target).get_mut::<Drone>() {
        target_drone.hits = target_drone.hits.saturating_sub(damage);
    } else if let Some(mut structure) = world.entity_mut(target).get_mut::<Structure>() {
        structure.hits = structure.hits.saturating_sub(damage);
    }
    Ok(())
}

fn apply_heal(world: &mut World, object_id: ObjectId, target_id: ObjectId) -> CommandResult {
    let (_, healer) = drone_snapshot(world, object_id)?;
    let heal = healer
        .body
        .iter()
        .filter(|part| **part == BodyPart::Heal)
        .count() as u32
        * 12;
    let target = entity(target_id)?;
    let mut entity_mut = world.entity_mut(target);
    let mut drone = entity_mut
        .get_mut::<Drone>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    drone.hits = (drone.hits + heal).min(drone.hits_max);
    Ok(())
}

fn apply_spawn_drone(
    world: &mut World,
    player_id: PlayerId,
    spawn_id: ObjectId,
    body: Vec<BodyPart>,
) -> CommandResult {
    let spawn = entity(spawn_id)?;
    let position = *world
        .entity(spawn)
        .get::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    let cost = body_cost(&body);
    {
        let mut entity_mut = world.entity_mut(spawn);
        let mut structure = entity_mut
            .get_mut::<Structure>()
            .ok_or(RejectionReason::ObjectNotFound)?;
        if let Some(energy) = &mut structure.energy {
            *energy = energy.saturating_sub(cost);
        }
        structure.cooldown = 1;
    }
    world
        .resource_mut::<PendingSpawnQueue>()
        .0
        .push(PendingSpawn {
            owner: player_id,
            body,
            position: spawn_output_position(position),
        });
    Ok(())
}

fn drone_snapshot(
    world: &mut World,
    object_id: ObjectId,
) -> Result<(Position, Drone), RejectionReason> {
    let entity = entity(object_id)?;
    let entity_ref = world
        .get_entity(entity)
        .map_err(|_| RejectionReason::ObjectNotFound)?;
    let position = *entity_ref
        .get::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    let drone = entity_ref
        .get::<Drone>()
        .ok_or(RejectionReason::NotMovable)?
        .clone();
    Ok((position, drone))
}

fn source_snapshot(
    world: &mut World,
    object_id: ObjectId,
) -> Result<(Position, crate::components::Source), RejectionReason> {
    let entity = entity(object_id)?;
    let entity_ref = world
        .get_entity(entity)
        .map_err(|_| RejectionReason::ObjectNotFound)?;
    let position = *entity_ref
        .get::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    let source = entity_ref
        .get::<crate::components::Source>()
        .ok_or(RejectionReason::NotSource)?
        .clone();
    Ok((position, source))
}

fn structure_snapshot(
    world: &mut World,
    object_id: ObjectId,
) -> Result<(Position, Structure), RejectionReason> {
    let entity = entity(object_id)?;
    let entity_ref = world
        .get_entity(entity)
        .map_err(|_| RejectionReason::ObjectNotFound)?;
    let position = *entity_ref
        .get::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    let structure = entity_ref
        .get::<Structure>()
        .ok_or(RejectionReason::ObjectNotFound)?
        .clone();
    Ok((position, structure))
}

fn attackable_snapshot(
    world: &mut World,
    object_id: ObjectId,
) -> Result<(Position, Option<PlayerId>), RejectionReason> {
    let entity = entity(object_id)?;
    let entity_ref = world
        .get_entity(entity)
        .map_err(|_| RejectionReason::ObjectNotFound)?;
    let position = *entity_ref
        .get::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    if let Some(drone) = entity_ref.get::<Drone>() {
        Ok((position, Some(drone.owner)))
    } else if let Some(structure) = entity_ref.get::<Structure>() {
        Ok((position, structure.owner))
    } else {
        Err(RejectionReason::ObjectNotFound)
    }
}

fn target_resource_amount(
    world: &mut World,
    target_id: ObjectId,
    resource: &str,
) -> Result<(Position, u32), RejectionReason> {
    let entity = entity(target_id)?;
    let entity_ref = world
        .get_entity(entity)
        .map_err(|_| RejectionReason::ObjectNotFound)?;
    let position = *entity_ref
        .get::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    if let Some(drone) = entity_ref.get::<Drone>() {
        return Ok((position, *drone.carry.get(resource).unwrap_or(&0)));
    }
    if let Some(structure) = entity_ref.get::<Structure>() {
        return Ok((position, structure_energy(resource, structure.energy)));
    }
    if let Some(resource_store) = entity_ref.get::<crate::components::Resource>() {
        return Ok((
            position,
            *resource_store.amounts.get(resource).unwrap_or(&0),
        ));
    }
    Err(RejectionReason::ObjectNotFound)
}

fn target_resource_space(
    world: &mut World,
    target_id: ObjectId,
    resource: &str,
) -> Result<(Position, u32), RejectionReason> {
    let entity = entity(target_id)?;
    let entity_ref = world
        .get_entity(entity)
        .map_err(|_| RejectionReason::ObjectNotFound)?;
    let position = *entity_ref
        .get::<Position>()
        .ok_or(RejectionReason::ObjectNotFound)?;
    if let Some(drone) = entity_ref.get::<Drone>() {
        return Ok((
            position,
            drone
                .carry_capacity
                .saturating_sub(carry_used(&drone.carry)),
        ));
    }
    if let Some(structure) = entity_ref.get::<Structure>() {
        if resource != "Energy" || structure.energy_capacity.is_none() {
            return Err(RejectionReason::TargetFull);
        }
        return Ok((
            position,
            structure
                .energy_capacity
                .unwrap_or(0)
                .saturating_sub(structure.energy.unwrap_or(0)),
        ));
    }
    Err(RejectionReason::ObjectNotFound)
}

fn ensure_owner(drone: &Drone, player_id: PlayerId) -> CommandResult {
    if drone.owner != player_id {
        return Err(RejectionReason::NotOwner);
    }
    Ok(())
}

fn ensure_drone_can_act(drone: &Drone, part: BodyPart, check_fatigue: bool) -> CommandResult {
    if drone.spawning {
        return Err(RejectionReason::StillSpawning);
    }
    if check_fatigue && drone.fatigue > 0 {
        return Err(RejectionReason::Fatigued);
    }
    require_body(drone, part)
}

fn require_body(drone: &Drone, part: BodyPart) -> CommandResult {
    if !drone.body.contains(&part) {
        return Err(RejectionReason::MissingBodyPart { part });
    }
    Ok(())
}

fn ensure_range(from: Position, to: Position, max: u32) -> CommandResult {
    let distance = hex_distance(from, to);
    if distance > max {
        return Err(RejectionReason::OutOfRange { distance, max });
    }
    Ok(())
}

fn hex_distance(from: Position, to: Position) -> u32 {
    if from.room != to.room {
        return u32::MAX;
    }
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    dx.abs().max(dy.abs()).max((dx + dy).abs()) as u32
}

fn step(position: Position, direction: Direction) -> Option<Position> {
    let (dx, dy) = match direction {
        Direction::Top => (0, -1),
        Direction::TopRight => (1, -1),
        Direction::BottomRight => (1, 0),
        Direction::Bottom => (0, 1),
        Direction::BottomLeft => (-1, 1),
        Direction::TopLeft => (-1, 0),
    };
    Some(Position {
        x: position.x + dx,
        y: position.y + dy,
        room: position.room,
    })
}

fn spawn_output_position(position: Position) -> Position {
    Position {
        x: position.x + 1,
        y: position.y,
        room: position.room,
    }
}

fn tile_has_blocking_enemy(world: &mut World, position: Position, player_id: PlayerId) -> bool {
    world
        .query::<(&Position, &Drone)>()
        .iter(world)
        .any(|(drone_position, drone)| *drone_position == position && drone.owner != player_id)
}

fn tile_has_any_drone(world: &mut World, position: Position) -> bool {
    world
        .query::<(&Position, &Drone)>()
        .iter(world)
        .any(|(drone_position, _)| *drone_position == position)
}

fn carry_used(carry: &IndexMap<String, u32>) -> u32 {
    carry.values().sum()
}

fn structure_energy(resource: &str, energy: Option<u32>) -> u32 {
    if resource == "Energy" {
        energy.unwrap_or(0)
    } else {
        0
    }
}

fn take_from_drone(world: &mut World, entity: Entity, resource: &str, amount: u32) {
    let mut entity_mut = world.entity_mut(entity);
    let mut drone = entity_mut.get_mut::<Drone>().unwrap();
    let value = drone.carry.entry(resource.to_string()).or_default();
    *value -= amount;
}

fn add_to_target(world: &mut World, entity: Entity, resource: &str, amount: u32) -> CommandResult {
    if let Some(mut drone) = world.entity_mut(entity).get_mut::<Drone>() {
        *drone.carry.entry(resource.to_string()).or_default() += amount;
        return Ok(());
    }
    if let Some(mut structure) = world.entity_mut(entity).get_mut::<Structure>() {
        if resource == "Energy" {
            if let Some(energy) = &mut structure.energy {
                *energy += amount;
                return Ok(());
            }
        }
    }
    Err(RejectionReason::ObjectNotFound)
}

fn take_from_target(
    world: &mut World,
    entity: Entity,
    resource: &str,
    amount: u32,
) -> CommandResult {
    if let Some(mut drone) = world.entity_mut(entity).get_mut::<Drone>() {
        let value = drone.carry.entry(resource.to_string()).or_default();
        *value -= amount;
        return Ok(());
    }
    if let Some(mut structure) = world.entity_mut(entity).get_mut::<Structure>() {
        if resource == "Energy" {
            if let Some(energy) = &mut structure.energy {
                *energy -= amount;
                return Ok(());
            }
        }
    }
    if let Some(mut resource_store) = world
        .entity_mut(entity)
        .get_mut::<crate::components::Resource>()
    {
        let value = resource_store
            .amounts
            .entry(resource.to_string())
            .or_default();
        *value -= amount;
        return Ok(());
    }
    Err(RejectionReason::ObjectNotFound)
}

pub fn body_cost(body: &[BodyPart]) -> u32 {
    body.iter()
        .map(|part| match part {
            BodyPart::Move => 50,
            BodyPart::Work => 100,
            BodyPart::Carry => 50,
            BodyPart::Attack => 80,
            BodyPart::RangedAttack => 100,
            BodyPart::Heal => 250,
            BodyPart::Claim => 600,
            BodyPart::Tough => 10,
        })
        .sum()
}

fn entity(object_id: ObjectId) -> Result<Entity, RejectionReason> {
    Ok(Entity::from_bits(object_id))
}
