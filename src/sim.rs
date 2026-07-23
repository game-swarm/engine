use std::time::Instant;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use swarm_engine_api::abi::{
    SnapshotActorContext as AbiSnapshotActorContext, SnapshotEntity as AbiSnapshotEntity,
    SnapshotMessage as AbiSnapshotMessage, SnapshotOmittedBucket as AbiSnapshotOmittedBucket,
    SnapshotOmittedCategories as AbiSnapshotOmittedCategories,
    SnapshotPosition as AbiSnapshotPosition, SnapshotTerrain as AbiSnapshotTerrain,
    SnapshotTerrainTile as AbiSnapshotTerrainTile, VisibleSnapshot,
};
use swarm_engine_api::ids::{BodyPart, PlayerId};
use swarm_engine_plugin_sdk::components::{Controller, Drone, Owner, Position, Structure};

use crate::command::{Tick, object_id};
use crate::components::{RoomTerrains, Source, TerrainType};
use crate::visibility::{VisibilitySet, is_visible_to, visible_positions};
use crate::world::{SwarmWorld, create_world};

// ═══════════════════════════════════════════════════════════════════
// Snapshot data types (P0-6)
// ═══════════════════════════════════════════════════════════════════

/// Distance bucket for deterministic truncation ordering (§1.3)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DistanceBucket {
    /// Bucket 0: self (own drone)
    Self_ = 0,
    /// Bucket 1: adjacent tile (0, 1]
    Adjacent = 1,
    /// Bucket 2: close range (1, 4]
    Close = 2,
    /// Bucket 3: medium range (4, 8]
    Medium = 3,
    /// Bucket 4: far range (8, 16]
    Far = 4,
    /// Bucket 5: very far (16, 32]
    VeryFar = 5,
    /// Bucket 6: out of sight (32, ∞)
    OutOfSight = 6,
}

impl DistanceBucket {
    pub fn from_distance(distance: u64) -> Self {
        if distance == 0 {
            Self::Self_
        } else if distance <= 1 {
            Self::Adjacent
        } else if distance <= 4 {
            Self::Close
        } else if distance <= 8 {
            Self::Medium
        } else if distance <= 16 {
            Self::Far
        } else if distance <= 32 {
            Self::VeryFar
        } else {
            Self::OutOfSight
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OmittedBucket {
    #[serde(rename = "0")]
    Zero,
    Few,
    Some,
    Many,
    Extreme,
}

impl OmittedBucket {
    pub fn from_count(count: usize) -> Self {
        match count {
            0 => Self::Zero,
            1..=10 => Self::Few,
            11..=50 => Self::Some,
            51..=200 => Self::Many,
            _ => Self::Extreme,
        }
    }
}

impl Default for OmittedBucket {
    fn default() -> Self {
        Self::Zero
    }
}

/// Bucketed omitted categories in a truncated snapshot (§1.2)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OmittedCategories {
    pub entities: OmittedBucket,
    pub resources: OmittedBucket,
    pub events: OmittedBucket,
    #[serde(default)]
    pub terrain: OmittedBucket,
    #[serde(default)]
    pub messages: OmittedBucket,
}

impl OmittedCategories {
    pub fn all_zero() -> Self {
        Self {
            entities: OmittedBucket::Zero,
            resources: OmittedBucket::Zero,
            events: OmittedBucket::Zero,
            terrain: OmittedBucket::Zero,
            messages: OmittedBucket::Zero,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotTerrainTile {
    pub room_id: u32,
    pub x: i32,
    pub y: i32,
    pub terrain: TerrainType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMessage {
    pub message_id: u64,
    pub sender_id: String,
    pub recipient_id: String,
    pub payload: Vec<u8>,
}

/// Lightweight entity representation for snapshots
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotEntity {
    #[serde(rename = "e")]
    pub entity_id: String,
    #[serde(rename = "t")]
    pub entity_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "p")]
    pub position: Option<(u32, i32, i32)>, // (room_id, x, y)
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "o")]
    pub owner: Option<PlayerId>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "h")]
    pub hits: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "hm")]
    pub hits_max: Option<u32>,
}

/// Snapshot configuration for a drone
#[derive(Debug, Clone)]
pub struct SnapshotConfig {
    pub max_size_bytes: usize,
    pub fog_of_war: bool,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            max_size_bytes: 256 * 1024,
            fog_of_war: true,
        }
    }
}

/// Key for deterministic sort: (distance_bucket, entity_id)
type SortKey = (DistanceBucket, String);

/// Per-drone perception snapshot (§1)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerDroneSnapshot {
    pub tick: Tick,
    #[serde(rename = "drone_id")]
    pub drone_entity_id: String,
    pub truncated: bool,
    #[serde(default)]
    pub degraded: bool,
    pub omitted_categories: OmittedCategories,
    #[serde(default)]
    pub terrain: Vec<SnapshotTerrainTile>,
    pub entities: Vec<SnapshotEntity>,
    pub resources: Vec<SnapshotEntity>,
    #[serde(default)]
    pub events: Vec<SnapshotEntity>,
    #[serde(default)]
    pub messages: Vec<SnapshotMessage>,
    #[serde(default)]
    pub omitted_messages: OmittedBucket,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotActorContext {
    pub active_drones: Vec<String>,
    pub primary_drone: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerPlayerSnapshot {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub actor_context: SnapshotActorContext,
    pub truncated: bool,
    #[serde(default)]
    pub degraded: bool,
    #[serde(default)]
    pub over_budget: bool,
    pub omitted_categories: OmittedCategories,
    #[serde(default)]
    pub terrain: Vec<SnapshotTerrainTile>,
    pub entities: Vec<SnapshotEntity>,
    pub resources: Vec<SnapshotEntity>,
    #[serde(default)]
    pub events: Vec<SnapshotEntity>,
    #[serde(default)]
    pub messages: Vec<SnapshotMessage>,
    #[serde(default)]
    pub omitted_messages: OmittedBucket,
}

// ═══════════════════════════════════════════════════════════════════
// Snapshot building
// ═══════════════════════════════════════════════════════════════════

/// Fog-of-war filter: delegates to visibility module (§1.4 critical entities always visible)
pub fn fog_of_war_filter(
    world: &mut World,
    _drone_entity: Entity,
    target_entity: Entity,
    player_id: PlayerId,
    tick: Tick,
) -> bool {
    // Critical: own entities always visible
    if let Some(owner) = world.get::<Owner>(target_entity)
        && owner.0 == player_id
    {
        return true;
    }
    // Critical: controllers always visible (room contention)
    if world.get::<Controller>(target_entity).is_some() {
        return true;
    }
    // Delegate to visibility module
    is_visible_to(world, target_entity, player_id, tick)
}

/// Compute hex distance between two positions
fn hex_distance(a: &Position, b: &Position) -> u64 {
    let dx = a.x as i64 - b.x as i64;
    let dy = a.y as i64 - b.y as i64;
    let dz = dx + dy;
    (dx.unsigned_abs() + dy.unsigned_abs() + dz.unsigned_abs()) / 2
}

/// Classify an entity into a SnapshotEntity
fn classify_entity(world: &World, entity: Entity) -> Option<SnapshotEntity> {
    let entity_id = object_id(entity).to_string();
    let position = world.get::<Position>(entity).map(|p| (p.room.0, p.x, p.y));

    if let Some(drone) = world.get::<Drone>(entity) {
        Some(SnapshotEntity {
            entity_id,
            entity_type: "drone".to_string(),
            position,
            owner: Some(drone.owner),
            hits: Some(drone.hits),
            hits_max: Some(drone.hits_max),
        })
    } else if let Some(structure) = world.get::<Structure>(entity) {
        Some(SnapshotEntity {
            entity_id,
            entity_type: format!("structure:{:?}", structure.structure_type),
            position,
            owner: structure.owner,
            hits: Some(structure.hits),
            hits_max: Some(structure.hits_max),
        })
    } else if let Some(controller) = world.get::<Controller>(entity) {
        Some(SnapshotEntity {
            entity_id,
            entity_type: "controller".to_string(),
            position,
            owner: controller.owner,
            hits: None,
            hits_max: None,
        })
    } else if let Some(source) = world.get::<Source>(entity) {
        Some(SnapshotEntity {
            entity_id,
            entity_type: "source".to_string(),
            position,
            owner: None,
            hits: Some(source.capacity),
            hits_max: Some(source.capacity), // capacity ≈ "health" for sources
        })
    } else {
        // Unknown entity type — still include in snapshot if it has a position
        position.map(|pos| SnapshotEntity {
            entity_id,
            entity_type: "unknown".to_string(),
            position: Some(pos),
            owner: None,
            hits: None,
            hits_max: None,
        })
    }
}

/// Build a perception snapshot for a single drone (§1)
pub fn build_snapshot(
    world: &mut World,
    drone_entity: Entity,
    player_id: PlayerId,
    tick: Tick,
    config: &SnapshotConfig,
) -> PerDroneSnapshot {
    let drone_pos = world.get::<Position>(drone_entity).copied();

    // Collect all visible entities with distance bucket + entity_id sort key
    let mut sortable_entities: Vec<(SortKey, SnapshotEntity)> = Vec::new();
    let all_entities = world.query::<Entity>().iter(world).collect::<Vec<_>>();

    for &entity in &all_entities {
        // Apply fog-of-war filter
        if config.fog_of_war && !fog_of_war_filter(world, drone_entity, entity, player_id, tick) {
            continue;
        }

        if let Some(snapshot_entity) = classify_entity(world, entity) {
            // Compute distance bucket
            let bucket = if entity == drone_entity {
                DistanceBucket::Self_
            } else if let Some(ref pos) = drone_pos {
                if let Some(ep) = world.get::<Position>(entity) {
                    DistanceBucket::from_distance(hex_distance(pos, ep))
                } else {
                    DistanceBucket::OutOfSight
                }
            } else {
                DistanceBucket::OutOfSight
            };

            let key = (bucket, snapshot_entity.entity_id.clone());
            sortable_entities.push((key, snapshot_entity));
        }
    }

    // Deterministic sort: distance bucket asc, then entity_id lexicographic (§1.3)
    sortable_entities.sort_by(|a, b| a.0.0.cmp(&b.0.0).then_with(|| a.0.1.cmp(&b.0.1)));

    // Separate critical entities (never truncated) from non-critical (§1.4)
    let drone_eid = object_id(drone_entity).to_string();
    let (critical, truncatable): (Vec<_>, Vec<_>) =
        sortable_entities.into_iter().partition(|(_, e)| {
            e.entity_id == drone_eid // own drone
                || e.entity_type == "controller" // room controllers
                || e.entity_type.starts_with("structure") // structures
                || (e.entity_type == "drone" && e.owner == Some(player_id)) // own drones
        });

    // Serialize and truncate if needed
    let drone_eid = object_id(drone_entity).to_string();
    let serialize_to_size = |entities: &[SnapshotEntity]| -> usize {
        encode_drone_snapshot_v2(&PerDroneSnapshot {
            tick,
            drone_entity_id: drone_eid.clone(),
            truncated: false,
            degraded: false,
            omitted_categories: OmittedCategories::all_zero(),
            terrain: Vec::new(),
            entities: entities.to_vec(),
            resources: Vec::new(),
            events: Vec::new(),
            messages: Vec::new(),
            omitted_messages: OmittedBucket::Zero,
        })
        .len()
    };

    let mut kept_entities: Vec<SnapshotEntity> = critical.iter().map(|(_, e)| e.clone()).collect();
    let mut omitted_count = 0usize;
    let mut degraded = false;

    // Add truncatable entities from closest to farthest until size limit
    for (_, entity) in &truncatable {
        kept_entities.push(entity.clone());
        if serialize_to_size(&kept_entities) > config.max_size_bytes {
            kept_entities.pop();
            omitted_count += 1;
        }
    }

    let total_truncatable = truncatable.len();
    if omitted_count > 0 {
        // Check degradation: if action_range entities were removed (§1.5)
        // Entities in buckets 0-3 (range ≤8) being removed = degraded
        let removed_has_nearby = truncatable
            .iter()
            .skip(total_truncatable - omitted_count)
            .any(|((bucket, _), _)| *bucket <= DistanceBucket::Medium);
        degraded = removed_has_nearby;
    }

    PerDroneSnapshot {
        tick,
        drone_entity_id: drone_eid,
        truncated: omitted_count > 0,
        degraded,
        omitted_categories: OmittedCategories {
            entities: OmittedBucket::from_count(omitted_count),
            resources: OmittedBucket::Zero, // resources tracked separately in ledger
            events: OmittedBucket::Zero,    // events not yet in snapshot scope
            terrain: OmittedBucket::Zero,
            messages: OmittedBucket::Zero,
        },
        terrain: Vec::new(),
        entities: kept_entities,
        resources: Vec::new(),
        events: Vec::new(),
        messages: Vec::new(),
        omitted_messages: OmittedBucket::Zero,
    }
}

/// Collect snapshots for all active drones of a player (§1 integration)
pub fn collect_snapshots(
    world: &mut World,
    player_ids: &[PlayerId],
    tick: Tick,
    config: &SnapshotConfig,
) -> Vec<PerDroneSnapshot> {
    let mut snapshots = Vec::new();
    let all_entities: Vec<Entity> = world.query::<Entity>().iter(world).collect();

    for &player_id in player_ids {
        // Find all drones owned by this player
        for &entity in &all_entities {
            if let Some(drone) = world.get::<Drone>(entity)
                && drone.owner == player_id
            {
                let snapshot = build_snapshot(world, entity, player_id, tick, config);
                snapshots.push(snapshot);
            }
        }
    }

    snapshots
}

pub fn build_player_snapshot(
    world: &mut World,
    player_id: PlayerId,
    tick: Tick,
    config: &SnapshotConfig,
) -> Option<PerPlayerSnapshot> {
    let all_entities: Vec<Entity> = world.query::<Entity>().iter(world).collect();
    let mut owned_drones = all_entities
        .iter()
        .copied()
        .filter(|entity| {
            world
                .get::<Drone>(*entity)
                .is_some_and(|drone| drone.owner == player_id)
        })
        .collect::<Vec<_>>();
    owned_drones.sort_by_key(|entity| object_id(*entity));

    let active_drones = owned_drones
        .iter()
        .map(|entity| object_id(*entity).to_string())
        .collect::<Vec<_>>();
    let actor_context = SnapshotActorContext {
        primary_drone: active_drones.first().cloned(),
        active_drones,
    };
    let drone_positions = owned_drones
        .iter()
        .filter_map(|entity| {
            world
                .get::<Position>(*entity)
                .copied()
                .map(|position| (*entity, position))
        })
        .collect::<Vec<_>>();
    let visible = config
        .fog_of_war
        .then(|| visible_positions(world, player_id));
    let terrain = snapshot_terrain(world, visible.as_ref());

    let mut sortable_entities: Vec<(SortKey, SnapshotEntity)> = Vec::new();
    for &entity in &all_entities {
        if visible.as_ref().is_some_and(|visible| {
            !is_visible_with_precomputed_positions(world, entity, player_id, visible)
        }) {
            continue;
        }

        if let Some(snapshot_entity) = classify_entity(world, entity) {
            let bucket = if owned_drones.contains(&entity) {
                DistanceBucket::Self_
            } else if let Some(entity_position) = world.get::<Position>(entity) {
                drone_positions
                    .iter()
                    .map(|(_, drone_position)| {
                        DistanceBucket::from_distance(hex_distance(drone_position, entity_position))
                    })
                    .min()
                    .unwrap_or(DistanceBucket::OutOfSight)
            } else {
                DistanceBucket::OutOfSight
            };

            sortable_entities.push(((bucket, snapshot_entity.entity_id.clone()), snapshot_entity));
        }
    }

    sortable_entities.sort_by(|a, b| a.0.0.cmp(&b.0.0).then_with(|| a.0.1.cmp(&b.0.1)));

    let (critical, truncatable): (Vec<_>, Vec<_>) =
        sortable_entities.into_iter().partition(|(_, entity)| {
            (entity.entity_type == "drone" && entity.owner == Some(player_id))
                || entity.entity_type == "controller"
                || entity.entity_type.starts_with("structure")
        });

    let size_of = |entities: &[SnapshotEntity], omitted_count: usize, over_budget: bool| -> usize {
        encode_player_snapshot_v2(&PerPlayerSnapshot {
            tick,
            player_id,
            actor_context: actor_context.clone(),
            truncated: omitted_count > 0 || over_budget,
            degraded: false,
            over_budget,
            omitted_categories: OmittedCategories {
                entities: OmittedBucket::from_count(omitted_count),
                resources: OmittedBucket::Zero,
                events: OmittedBucket::Zero,
                terrain: OmittedBucket::Zero,
                messages: OmittedBucket::Zero,
            },
            terrain: terrain.clone(),
            entities: entities.to_vec(),
            resources: Vec::new(),
            events: Vec::new(),
            messages: Vec::new(),
            omitted_messages: OmittedBucket::Zero,
        })
        .len()
    };

    let mut kept_entities = critical
        .iter()
        .map(|(_, entity)| entity.clone())
        .collect::<Vec<_>>();
    let mut omitted_count = 0usize;
    let mut degraded = false;
    let critical_over_budget = size_of(&kept_entities, 0, false) > config.max_size_bytes;
    let critical_count = kept_entities.len();
    let all_entities_fit = if critical_over_budget {
        false
    } else {
        kept_entities.extend(truncatable.iter().map(|(_, entity)| entity.clone()));
        if size_of(&kept_entities, 0, false) <= config.max_size_bytes {
            true
        } else {
            kept_entities.truncate(critical_count);
            false
        }
    };

    if !all_entities_fit {
        for ((bucket, _), entity) in &truncatable {
            kept_entities.push(entity.clone());
            if size_of(&kept_entities, omitted_count, critical_over_budget) > config.max_size_bytes
            {
                kept_entities.pop();
                omitted_count += 1;
                degraded |= *bucket <= DistanceBucket::Medium;
            }
        }
    }

    Some(PerPlayerSnapshot {
        tick,
        player_id,
        actor_context,
        truncated: omitted_count > 0 || critical_over_budget,
        degraded,
        over_budget: critical_over_budget,
        omitted_categories: OmittedCategories {
            entities: OmittedBucket::from_count(omitted_count),
            resources: OmittedBucket::Zero,
            events: OmittedBucket::Zero,
            terrain: OmittedBucket::Zero,
            messages: OmittedBucket::Zero,
        },
        terrain,
        entities: kept_entities,
        resources: Vec::new(),
        events: Vec::new(),
        messages: Vec::new(),
        omitted_messages: OmittedBucket::Zero,
    })
}

fn snapshot_terrain(world: &World, visible: Option<&VisibilitySet>) -> Vec<SnapshotTerrainTile> {
    let Some(terrains) = world.get_resource::<RoomTerrains>() else {
        return Vec::new();
    };
    let mut tiles = terrains
        .0
        .iter()
        .flat_map(|(room_id, room)| {
            room.iter().filter_map(move |(x, y, terrain)| {
                visible
                    .is_none_or(|visible| visible.contains(&(*room_id, x, y)))
                    .then_some(SnapshotTerrainTile {
                        room_id: room_id.0,
                        x,
                        y,
                        terrain,
                    })
            })
        })
        .collect::<Vec<_>>();
    tiles.sort_by_key(|tile| (tile.room_id, tile.y, tile.x));
    tiles
}

fn is_visible_with_precomputed_positions(
    world: &World,
    entity: Entity,
    player_id: PlayerId,
    visible: &VisibilitySet,
) -> bool {
    if world
        .get::<Owner>(entity)
        .is_some_and(|owner| owner.0 == player_id)
        || world.get::<Controller>(entity).is_some()
    {
        return true;
    }

    world
        .get::<Position>(entity)
        .is_some_and(|position| visible.contains(&(position.room, position.x, position.y)))
}

pub fn collect_player_snapshots(
    world: &mut World,
    player_ids: &[PlayerId],
    tick: Tick,
    config: &SnapshotConfig,
) -> Vec<PerPlayerSnapshot> {
    let mut player_ids = player_ids.to_vec();
    player_ids.sort_unstable();
    player_ids.dedup();
    player_ids
        .into_iter()
        .filter_map(|player_id| build_player_snapshot(world, player_id, tick, config))
        .collect()
}

pub fn snapshot_hash(snapshot: &PerPlayerSnapshot) -> [u8; 32] {
    *blake3::hash(&encode_player_snapshot_v2(snapshot)).as_bytes()
}

pub fn encode_player_snapshot_v2(snapshot: &PerPlayerSnapshot) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"swarm.snapshot.player.v2");
    write_u64(&mut bytes, snapshot.tick);
    write_u32(&mut bytes, snapshot.player_id);
    encode_actor_context(&mut bytes, &snapshot.actor_context);
    write_bool(&mut bytes, snapshot.truncated);
    write_bool(&mut bytes, snapshot.degraded);
    write_bool(&mut bytes, snapshot.over_budget);
    encode_omitted_categories(&mut bytes, snapshot.omitted_categories);
    encode_terrain_tiles(&mut bytes, &snapshot.terrain);
    encode_entities(&mut bytes, &snapshot.entities);
    encode_entities(&mut bytes, &snapshot.resources);
    encode_entities(&mut bytes, &snapshot.events);
    encode_messages(&mut bytes, &snapshot.messages);
    encode_omitted_bucket(&mut bytes, snapshot.omitted_messages);
    bytes
}

pub fn to_abi_visible_snapshot(
    snapshot: &PerPlayerSnapshot,
    world_seed: u64,
    actor_id: u64,
) -> VisibleSnapshot {
    VisibleSnapshot {
        tick: snapshot.tick,
        player_id: snapshot.player_id,
        world_seed,
        actor_id,
        actor_context: AbiSnapshotActorContext {
            active_drones: snapshot.actor_context.active_drones.clone(),
            primary_drone: snapshot.actor_context.primary_drone.clone(),
        },
        truncated: snapshot.truncated,
        degraded: snapshot.degraded,
        over_budget: snapshot.over_budget,
        omitted_categories: to_abi_omitted_categories(snapshot.omitted_categories),
        terrain: snapshot
            .terrain
            .iter()
            .map(|tile| AbiSnapshotTerrainTile {
                room_id: tile.room_id,
                x: tile.x,
                y: tile.y,
                terrain: match tile.terrain {
                    TerrainType::Plain => AbiSnapshotTerrain::Plain,
                    TerrainType::Swamp => AbiSnapshotTerrain::Swamp,
                    TerrainType::Wall => AbiSnapshotTerrain::Wall,
                },
            })
            .collect(),
        entities: snapshot.entities.iter().map(to_abi_entity).collect(),
        resources: snapshot.resources.iter().map(to_abi_entity).collect(),
        events: snapshot.events.iter().map(to_abi_entity).collect(),
        messages: snapshot
            .messages
            .iter()
            .map(|message| AbiSnapshotMessage {
                message_id: message.message_id,
                sender_id: message.sender_id.clone(),
                recipient_id: message.recipient_id.clone(),
                payload: message.payload.clone(),
            })
            .collect(),
        omitted_messages: to_abi_omitted_bucket(snapshot.omitted_messages),
    }
}

fn to_abi_entity(entity: &SnapshotEntity) -> AbiSnapshotEntity {
    AbiSnapshotEntity {
        entity_id: entity.entity_id.clone(),
        entity_type: entity.entity_type.clone(),
        position: entity
            .position
            .map(|(room_id, x, y)| AbiSnapshotPosition { room_id, x, y }),
        owner: entity.owner,
        hits: entity.hits,
        hits_max: entity.hits_max,
    }
}

fn to_abi_omitted_categories(categories: OmittedCategories) -> AbiSnapshotOmittedCategories {
    AbiSnapshotOmittedCategories {
        entities: to_abi_omitted_bucket(categories.entities),
        resources: to_abi_omitted_bucket(categories.resources),
        events: to_abi_omitted_bucket(categories.events),
        terrain: to_abi_omitted_bucket(categories.terrain),
        messages: to_abi_omitted_bucket(categories.messages),
    }
}

fn to_abi_omitted_bucket(bucket: OmittedBucket) -> AbiSnapshotOmittedBucket {
    match bucket {
        OmittedBucket::Zero => AbiSnapshotOmittedBucket::Zero,
        OmittedBucket::Few => AbiSnapshotOmittedBucket::Few,
        OmittedBucket::Some => AbiSnapshotOmittedBucket::Some,
        OmittedBucket::Many => AbiSnapshotOmittedBucket::Many,
        OmittedBucket::Extreme => AbiSnapshotOmittedBucket::Extreme,
    }
}

fn encode_drone_snapshot_v2(snapshot: &PerDroneSnapshot) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"swarm.snapshot.drone.v2");
    write_u64(&mut bytes, snapshot.tick);
    write_string(&mut bytes, &snapshot.drone_entity_id);
    write_bool(&mut bytes, snapshot.truncated);
    write_bool(&mut bytes, snapshot.degraded);
    encode_omitted_categories(&mut bytes, snapshot.omitted_categories);
    encode_terrain_tiles(&mut bytes, &snapshot.terrain);
    encode_entities(&mut bytes, &snapshot.entities);
    encode_entities(&mut bytes, &snapshot.resources);
    encode_entities(&mut bytes, &snapshot.events);
    encode_messages(&mut bytes, &snapshot.messages);
    encode_omitted_bucket(&mut bytes, snapshot.omitted_messages);
    bytes
}

fn encode_actor_context(bytes: &mut Vec<u8>, actor_context: &SnapshotActorContext) {
    write_len(bytes, actor_context.active_drones.len());
    for drone_id in &actor_context.active_drones {
        write_string(bytes, drone_id);
    }
    encode_option_string(bytes, actor_context.primary_drone.as_deref());
}

fn encode_omitted_categories(bytes: &mut Vec<u8>, categories: OmittedCategories) {
    encode_omitted_bucket(bytes, categories.entities);
    encode_omitted_bucket(bytes, categories.resources);
    encode_omitted_bucket(bytes, categories.events);
    encode_omitted_bucket(bytes, categories.terrain);
    encode_omitted_bucket(bytes, categories.messages);
}

fn encode_omitted_bucket(bytes: &mut Vec<u8>, bucket: OmittedBucket) {
    write_u8(
        bytes,
        match bucket {
            OmittedBucket::Zero => 0,
            OmittedBucket::Few => 1,
            OmittedBucket::Some => 2,
            OmittedBucket::Many => 3,
            OmittedBucket::Extreme => 4,
        },
    );
}

fn encode_terrain_tiles(bytes: &mut Vec<u8>, terrain: &[SnapshotTerrainTile]) {
    write_len(bytes, terrain.len());
    for tile in terrain {
        write_u32(bytes, tile.room_id);
        write_i32(bytes, tile.x);
        write_i32(bytes, tile.y);
        write_u8(
            bytes,
            match tile.terrain {
                TerrainType::Plain => 0,
                TerrainType::Swamp => 1,
                TerrainType::Wall => 2,
            },
        );
    }
}

fn encode_entities(bytes: &mut Vec<u8>, entities: &[SnapshotEntity]) {
    write_len(bytes, entities.len());
    for entity in entities {
        write_string(bytes, &entity.entity_id);
        write_string(bytes, &entity.entity_type);
        match entity.position {
            Some((room_id, x, y)) => {
                write_bool(bytes, true);
                write_u32(bytes, room_id);
                write_i32(bytes, x);
                write_i32(bytes, y);
            }
            None => write_bool(bytes, false),
        }
        encode_option_u32(bytes, entity.owner);
        encode_option_u32(bytes, entity.hits);
        encode_option_u32(bytes, entity.hits_max);
    }
}

fn encode_messages(bytes: &mut Vec<u8>, messages: &[SnapshotMessage]) {
    write_len(bytes, messages.len());
    for message in messages {
        write_u64(bytes, message.message_id);
        write_string(bytes, &message.sender_id);
        write_string(bytes, &message.recipient_id);
        write_len(bytes, message.payload.len());
        bytes.extend_from_slice(&message.payload);
    }
}

fn encode_option_string(bytes: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(value) => {
            write_bool(bytes, true);
            write_string(bytes, value);
        }
        None => write_bool(bytes, false),
    }
}

fn encode_option_u32(bytes: &mut Vec<u8>, value: Option<u32>) {
    match value {
        Some(value) => {
            write_bool(bytes, true);
            write_u32(bytes, value);
        }
        None => write_bool(bytes, false),
    }
}

fn write_u8(bytes: &mut Vec<u8>, value: u8) {
    bytes.push(value);
}

fn write_bool(bytes: &mut Vec<u8>, value: bool) {
    write_u8(bytes, value as u8);
}

fn write_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn write_i32(bytes: &mut Vec<u8>, value: i32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn write_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn write_len(bytes: &mut Vec<u8>, len: usize) {
    write_u64(bytes, len as u64);
}

fn write_string(bytes: &mut Vec<u8>, value: &str) {
    write_len(bytes, value.len());
    bytes.extend_from_slice(value.as_bytes());
}

// ═══════════════════════════════════════════════════════════════════
// Local simulation (existing functionality retained)
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalSimulationSummary {
    pub ticks: Tick,
    pub final_state_checksum: u64,
    pub elapsed_ms: u128,
    pub drones: usize,
    pub sources: usize,
    pub structures: usize,
    pub controllers: usize,
}

pub fn create_local_simulation_world() -> SwarmWorld {
    let mut world = create_world();
    world.spawn_drone(
        1,
        10,
        10,
        vec![BodyPart::Move, BodyPart::Work, BodyPart::Carry],
    );
    world
}

pub fn run_local_simulation(ticks: Tick) -> LocalSimulationSummary {
    let started_at = Instant::now();
    let mut world = create_local_simulation_world();
    for _ in 0..ticks {
        world.run_tick();
    }
    summarize_local_simulation(&mut world, ticks, started_at.elapsed().as_millis())
}

pub fn summarize_local_simulation(
    world: &mut SwarmWorld,
    ticks: Tick,
    elapsed_ms: u128,
) -> LocalSimulationSummary {
    let final_state_checksum = world.state_checksum();
    let ecs = world.app.world_mut();
    LocalSimulationSummary {
        ticks,
        final_state_checksum,
        elapsed_ms,
        drones: ecs.query::<&Drone>().iter(ecs).count(),
        sources: ecs.query::<&Source>().iter(ecs).count(),
        structures: ecs.query::<&Structure>().iter(ecs).count(),
        controllers: ecs.query::<&Controller>().iter(ecs).count(),
    }
}

// ═══════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_engine_api::ids::{BodyPart, RoomId};
    use swarm_engine_plugin_sdk::components::Drone;

    #[test]
    fn local_simulation_runs_ticks_and_reports_summary() {
        let summary = run_local_simulation(3);

        assert_eq!(summary.ticks, 3);
        assert_eq!(summary.drones, 1);
        assert_eq!(summary.sources, 1);
        assert!(summary.final_state_checksum > 0);
    }

    // ── Snapshot tests ──

    fn create_test_world() -> SwarmWorld {
        let mut world = create_world();
        // Spawn a drone for player 1
        world.spawn_drone(
            1,
            5,
            5,
            vec![BodyPart::Move, BodyPart::Work, BodyPart::Carry],
        );
        // Spawn a drone for player 2 (far away)
        world.spawn_drone(2, 50, 50, vec![BodyPart::Move]);
        world
    }

    #[test]
    fn snapshot_builder_produces_valid_snapshot() {
        let mut world = create_test_world();
        let tick = 1;
        let w = world.app.world_mut();
        let entities: Vec<Entity> = w
            .query::<(Entity, &Drone)>()
            .iter(w)
            .map(|(e, _)| e)
            .collect();
        assert!(!entities.is_empty(), "test world must have drones");

        let drone = entities[0];
        let player_id = 1u32;
        let config = SnapshotConfig::default();

        let snapshot = build_snapshot(w, drone, player_id, tick, &config);

        assert_eq!(snapshot.tick, tick);
        assert!(!snapshot.drone_entity_id.is_empty());
        // Own drone should be in entities
        assert!(
            !snapshot.entities.is_empty(),
            "snapshot should contain entities"
        );
        assert!(
            snapshot.entities.iter().any(|e| e.entity_type == "drone"),
            "snapshot should contain drone entities"
        );
        // No truncation expected for small world
        assert!(!snapshot.truncated);
        assert_eq!(snapshot.omitted_categories, OmittedCategories::all_zero());
    }

    #[test]
    fn snapshot_fog_of_war_filters_distant_entities() {
        let mut world = create_test_world();
        // Collect owned entity IDs to avoid borrow issues
        let entities: Vec<(Entity, PlayerId)> = {
            let w = world.app.world_mut();
            w.query::<(Entity, &Drone)>()
                .iter(w)
                .map(|(e, d)| (e, d.owner))
                .collect()
        };

        // Find player 1's drone (at 5,5)
        let drone1 = entities.iter().find(|(_, owner)| *owner == 1).unwrap();
        // Player 2's drone at (50,50) should be out of fog range
        let drone2 = entities.iter().find(|(_, owner)| *owner == 2).unwrap();

        let config = SnapshotConfig {
            fog_of_war: true,
            ..Default::default()
        };
        let w2 = world.app.world_mut();
        let snapshot = build_snapshot(w2, drone1.0, 1, 1, &config);

        // Player 2's drone at distance ~45 should NOT be visible
        let drone2_id = format!("{:?}", drone2.0);
        let has_drone2 = snapshot.entities.iter().any(|e| e.entity_id == drone2_id);
        assert!(
            !has_drone2,
            "distant enemy drone should be filtered by fog_of_war"
        );
    }

    #[test]
    fn snapshot_fog_of_war_disabled_shows_all() {
        let mut world = create_test_world();
        let w = world.app.world_mut();
        let entities: Vec<(Entity, &Drone)> = w.query::<(Entity, &Drone)>().iter(w).collect();
        let drone1 = entities.iter().find(|(_, d)| d.owner == 1).unwrap().0;

        let config = SnapshotConfig {
            fog_of_war: false,
            ..Default::default()
        };
        let snapshot = build_snapshot(w, drone1, 1, 1, &config);

        // With fog disabled, should see enemy drone too
        let drone_count = snapshot
            .entities
            .iter()
            .filter(|e| e.entity_type == "drone")
            .count();
        assert!(
            drone_count >= 2,
            "fog disabled should show all drones, got {}",
            drone_count
        );
    }

    #[test]
    fn snapshot_deterministic_truncation_order() {
        let mut world = create_test_world();
        let w = world.app.world_mut();
        let entities: Vec<(Entity, &Drone)> = w.query::<(Entity, &Drone)>().iter(w).collect();
        let drone1 = entities.iter().find(|(_, d)| d.owner == 1).unwrap().0;

        // Build two snapshots from same state
        let config = SnapshotConfig::default();
        let s1 = build_snapshot(w, drone1, 1, 1, &config);
        let s2 = build_snapshot(w, drone1, 1, 1, &config);

        // Same state → same snapshot
        assert_eq!(s1.truncated, s2.truncated);
        assert_eq!(s1.omitted_categories, s2.omitted_categories);
        assert_eq!(s1.degraded, s2.degraded);
        assert_eq!(s1.entities.len(), s2.entities.len());
        // Entity order must be identical
        for (a, b) in s1.entities.iter().zip(s2.entities.iter()) {
            assert_eq!(
                a.entity_id, b.entity_id,
                "entity order must be deterministic"
            );
        }
    }

    #[test]
    fn snapshot_critical_entities_never_truncated() {
        let mut world = create_test_world();
        let w = world.app.world_mut();
        let entities: Vec<(Entity, &Drone)> = w.query::<(Entity, &Drone)>().iter(w).collect();
        let drone1 = entities.iter().find(|(_, d)| d.owner == 1).unwrap().0;
        let drone1_eid = object_id(drone1).to_string();

        // Tiny max_size to force truncation
        let config = SnapshotConfig {
            max_size_bytes: 50,
            fog_of_war: false,
        };
        let snapshot = build_snapshot(w, drone1, 1, 1, &config);

        // Own drone must always be present
        assert!(
            snapshot.entities.iter().any(|e| e.entity_id == drone1_eid),
            "own drone must never be truncated"
        );
    }

    #[test]
    fn snapshot_truncation_sets_omitted_count() {
        let mut world = create_test_world();
        // Add many entities to force truncation
        for i in 0..20 {
            world.spawn_drone(2, 50 + i, 50 + i, vec![BodyPart::Move]);
        }
        let w = world.app.world_mut();
        let drone1 = w
            .query::<(Entity, &Drone)>()
            .iter(w)
            .find(|(_, d)| d.owner == 1)
            .unwrap()
            .0;

        // Small max_size forces truncation
        let config = SnapshotConfig {
            max_size_bytes: 200,
            fog_of_war: false,
        };
        let snapshot = build_snapshot(w, drone1, 1, 1, &config);

        assert!(
            snapshot.truncated,
            "snapshot should be truncated with tiny max_size"
        );
        assert_ne!(
            snapshot.omitted_categories.entities,
            OmittedBucket::Zero,
            "omitted_count bucket should be non-zero"
        );
        // Schema stability: all categories present even if zero
        let _ = snapshot.omitted_categories.resources;
        let _ = snapshot.omitted_categories.events;
    }

    #[test]
    fn snapshot_degraded_when_nearby_entities_removed() {
        let mut world = create_test_world();
        // Add entities close to drone1
        world.spawn_drone(2, 6, 5, vec![BodyPart::Move]); // adjacent to drone1
        world.spawn_drone(2, 4, 5, vec![BodyPart::Move]); // adjacent

        let w = world.app.world_mut();
        let drone1 = w
            .query::<(Entity, &Drone)>()
            .iter(w)
            .find(|(_, d)| d.owner == 1)
            .unwrap()
            .0;

        // Tiny max_size will remove nearby entities
        let config = SnapshotConfig {
            max_size_bytes: 100,
            fog_of_war: false,
        };
        let snapshot = build_snapshot(w, drone1, 1, 1, &config);

        if snapshot.truncated {
            assert!(
                snapshot.degraded,
                "removing nearby (≤8) entities should mark tick as degraded"
            );
        }
    }

    #[test]
    fn distance_bucket_assignment() {
        assert_eq!(DistanceBucket::from_distance(0), DistanceBucket::Self_);
        assert_eq!(DistanceBucket::from_distance(1), DistanceBucket::Adjacent);
        assert_eq!(DistanceBucket::from_distance(2), DistanceBucket::Close);
        assert_eq!(DistanceBucket::from_distance(4), DistanceBucket::Close);
        assert_eq!(DistanceBucket::from_distance(5), DistanceBucket::Medium);
        assert_eq!(DistanceBucket::from_distance(8), DistanceBucket::Medium);
        assert_eq!(DistanceBucket::from_distance(10), DistanceBucket::Far);
        assert_eq!(DistanceBucket::from_distance(16), DistanceBucket::Far);
        assert_eq!(DistanceBucket::from_distance(20), DistanceBucket::VeryFar);
        assert_eq!(DistanceBucket::from_distance(32), DistanceBucket::VeryFar);
        assert_eq!(
            DistanceBucket::from_distance(100),
            DistanceBucket::OutOfSight
        );
    }

    #[test]
    fn hex_distance_is_exact_for_extreme_coordinates() {
        let a = Position {
            x: i32::MIN,
            y: i32::MAX,
            room: RoomId(0),
        };
        let b = Position {
            x: i32::MAX,
            y: i32::MIN,
            room: RoomId(0),
        };

        assert_eq!(hex_distance(&a, &b), u32::MAX as u64);
    }

    #[test]
    fn snapshot_omitted_categories_all_zero_when_no_truncation() {
        let mut world = create_test_world();
        let w = world.app.world_mut();
        let drone1 = w
            .query::<(Entity, &Drone)>()
            .iter(w)
            .find(|(_, d)| d.owner == 1)
            .unwrap()
            .0;

        let config = SnapshotConfig::default(); // 256KB
        let snapshot = build_snapshot(w, drone1, 1, 1, &config);

        assert!(!snapshot.truncated);
        assert_eq!(snapshot.omitted_categories, OmittedCategories::all_zero());
    }

    #[test]
    fn player_snapshot_for_no_drone_player_has_null_primary_drone() {
        let mut world = create_test_world();
        let w = world.app.world_mut();

        let snapshot = build_player_snapshot(w, 99, 7, &SnapshotConfig::default()).unwrap();

        assert_eq!(snapshot.player_id, 99);
        assert_eq!(snapshot.actor_context.active_drones, Vec::<String>::new());
        assert_eq!(snapshot.actor_context.primary_drone, None);
        assert_eq!(snapshot.omitted_categories, OmittedCategories::all_zero());
        assert_eq!(snapshot.omitted_messages, OmittedBucket::Zero);
    }

    #[test]
    fn player_snapshot_v2_carries_terrain_and_message_omission_buckets() {
        let mut world = create_test_world();
        let w = world.app.world_mut();

        let snapshot = build_player_snapshot(w, 1, 7, &SnapshotConfig::default()).unwrap();

        assert!(!snapshot.terrain.is_empty());
        assert_eq!(snapshot.messages, Vec::<SnapshotMessage>::new());
        assert_eq!(snapshot.omitted_categories.terrain, OmittedBucket::Zero);
        assert_eq!(snapshot.omitted_categories.messages, OmittedBucket::Zero);
        assert_eq!(snapshot.omitted_messages, OmittedBucket::Zero);
    }

    #[test]
    fn abi_visible_snapshot_maps_every_player_snapshot_field() {
        let entity = SnapshotEntity {
            entity_id: "entity-1".to_string(),
            entity_type: "Drone".to_string(),
            position: Some((3, 4, 5)),
            owner: Some(7),
            hits: Some(8),
            hits_max: Some(9),
        };
        let snapshot = PerPlayerSnapshot {
            tick: 11,
            player_id: 7,
            actor_context: SnapshotActorContext {
                active_drones: vec!["drone-1".to_string()],
                primary_drone: Some("drone-1".to_string()),
            },
            truncated: true,
            degraded: true,
            over_budget: true,
            omitted_categories: OmittedCategories {
                entities: OmittedBucket::Few,
                resources: OmittedBucket::Some,
                events: OmittedBucket::Many,
                terrain: OmittedBucket::Extreme,
                messages: OmittedBucket::Zero,
            },
            terrain: vec![SnapshotTerrainTile {
                room_id: 3,
                x: 4,
                y: 5,
                terrain: TerrainType::Swamp,
            }],
            entities: vec![entity.clone()],
            resources: vec![entity.clone()],
            events: vec![entity],
            messages: vec![SnapshotMessage {
                message_id: 12,
                sender_id: "sender".to_string(),
                recipient_id: "recipient".to_string(),
                payload: vec![13, 14],
            }],
            omitted_messages: OmittedBucket::Few,
        };

        let visible = to_abi_visible_snapshot(&snapshot, 15, 16);

        assert_eq!(visible.tick, snapshot.tick);
        assert_eq!(visible.player_id, snapshot.player_id);
        assert_eq!(visible.world_seed, 15);
        assert_eq!(visible.actor_id, 16);
        assert_eq!(visible.actor_context.active_drones, vec!["drone-1"]);
        assert_eq!(
            visible.actor_context.primary_drone.as_deref(),
            Some("drone-1")
        );
        assert!(visible.truncated && visible.degraded && visible.over_budget);
        assert_eq!(
            visible.omitted_categories.entities,
            AbiSnapshotOmittedBucket::Few
        );
        assert_eq!(
            visible.omitted_categories.resources,
            AbiSnapshotOmittedBucket::Some
        );
        assert_eq!(
            visible.omitted_categories.events,
            AbiSnapshotOmittedBucket::Many
        );
        assert_eq!(
            visible.omitted_categories.terrain,
            AbiSnapshotOmittedBucket::Extreme
        );
        assert_eq!(
            visible.omitted_categories.messages,
            AbiSnapshotOmittedBucket::Zero
        );
        assert_eq!(visible.terrain[0].terrain, AbiSnapshotTerrain::Swamp);
        for mapped in [
            &visible.entities[0],
            &visible.resources[0],
            &visible.events[0],
        ] {
            assert_eq!(mapped.entity_id, "entity-1");
            assert_eq!(mapped.entity_type, "Drone");
            assert_eq!(
                mapped.position,
                Some(AbiSnapshotPosition {
                    room_id: 3,
                    x: 4,
                    y: 5
                })
            );
            assert_eq!(mapped.owner, Some(7));
            assert_eq!(mapped.hits, Some(8));
            assert_eq!(mapped.hits_max, Some(9));
        }
        assert_eq!(visible.messages[0].message_id, 12);
        assert_eq!(visible.messages[0].sender_id, "sender");
        assert_eq!(visible.messages[0].recipient_id, "recipient");
        assert_eq!(visible.messages[0].payload, vec![13, 14]);
        assert_eq!(visible.omitted_messages, AbiSnapshotOmittedBucket::Few);
    }

    #[test]
    fn player_snapshot_hash_uses_v2_binary_encoding_not_json() {
        let mut world = create_test_world();
        let w = world.app.world_mut();

        let snapshot = build_player_snapshot(w, 1, 7, &SnapshotConfig::default()).unwrap();

        assert_eq!(
            snapshot_hash(&snapshot),
            *blake3::hash(&encode_player_snapshot_v2(&snapshot)).as_bytes()
        );
        assert_ne!(
            snapshot_hash(&snapshot),
            *blake3::hash(&serde_json::to_vec(&snapshot).unwrap()).as_bytes()
        );
    }

    #[test]
    fn collect_snapshots_builds_for_all_drones() {
        let mut world = create_test_world();
        let w = world.app.world_mut();
        let config = SnapshotConfig::default();

        let snapshots = collect_snapshots(w, &[1, 2], 1, &config);

        assert!(
            !snapshots.is_empty(),
            "should build snapshots for players with drones"
        );
        // Player 1 has 1 drone
        let p1_count = snapshots
            .iter()
            .filter(|s| s.entities.iter().any(|e| e.owner == Some(1)))
            .count();
        assert!(p1_count >= 1, "player 1's drone should have a snapshot");
    }

    #[test]
    fn player_collect_snapshot_has_actor_context_and_collective_visibility() {
        let mut world = create_test_world();
        let second = world.spawn_drone(1, 8, 5, vec![BodyPart::Move]);
        let w = world.app.world_mut();
        let config = SnapshotConfig::default();

        let snapshot = build_player_snapshot(w, 1, 7, &config).expect("player 1 snapshot");

        assert_eq!(snapshot.tick, 7);
        assert_eq!(snapshot.player_id, 1);
        assert_eq!(snapshot.actor_context.active_drones.len(), 2);
        assert!(
            snapshot
                .actor_context
                .active_drones
                .contains(&object_id(second).to_string())
        );
        assert!(snapshot.actor_context.primary_drone.is_some());
        assert_eq!(snapshot.omitted_categories.resources, OmittedBucket::Zero);
        assert_eq!(snapshot.omitted_categories.events, OmittedBucket::Zero);
        assert_eq!(snapshot_hash(&snapshot), snapshot_hash(&snapshot.clone()));

        let own_drone_count = snapshot
            .entities
            .iter()
            .filter(|entity| entity.entity_type == "drone" && entity.owner == Some(1))
            .count();
        assert_eq!(own_drone_count, 2, "all own drones remain visible");
    }

    #[test]
    fn omitted_zero_serializes_as_numeric_zero_bucket() {
        assert_eq!(serde_json::to_value(OmittedBucket::Zero).unwrap(), "0");
    }

    #[test]
    fn collect_player_snapshots_returns_one_snapshot_per_player() {
        let mut world = create_test_world();
        world.spawn_drone(1, 6, 5, vec![BodyPart::Move]);
        let w = world.app.world_mut();
        let config = SnapshotConfig::default();

        let snapshots = collect_player_snapshots(w, &[1, 2, 99], 11, &config);

        assert_eq!(snapshots.len(), 3);
        assert_eq!(snapshots[0].player_id, 1);
        assert_eq!(snapshots[1].player_id, 2);
        assert_eq!(snapshots[2].player_id, 99);
        assert_eq!(snapshots[0].actor_context.active_drones.len(), 2);
        assert_eq!(snapshots[1].actor_context.active_drones.len(), 1);
        assert_eq!(snapshots[2].actor_context.active_drones.len(), 0);
        assert_eq!(snapshots[2].actor_context.primary_drone, None);
    }

    #[test]
    fn player_snapshot_truncation_is_deterministic_and_marks_omissions() {
        let mut world = create_test_world();
        for index in 0..24 {
            world.spawn_drone(2, 6 + index, 5, vec![BodyPart::Move]);
        }
        let w = world.app.world_mut();
        let config = SnapshotConfig {
            max_size_bytes: 512,
            fog_of_war: false,
        };

        let first = build_player_snapshot(w, 1, 13, &config).expect("first snapshot");
        let second = build_player_snapshot(w, 1, 13, &config).expect("second snapshot");

        assert_eq!(first, second);
        assert!(first.truncated);
        assert!(
            encode_player_snapshot_v2(&first).len() <= config.max_size_bytes || first.over_budget
        );
        assert_ne!(first.omitted_categories.entities, OmittedBucket::Zero);
        assert!(
            first
                .entities
                .iter()
                .any(|entity| entity.entity_type == "drone" && entity.owner == Some(1)),
            "own drone is part of the critical retention set"
        );
    }
}
