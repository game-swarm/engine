use std::time::Instant;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::command::{Tick, object_id};
use crate::components::{BodyPart, Controller, Drone, PlayerId, Source, Structure};
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
    pub fn from_distance(distance: f64) -> Self {
        if distance == 0.0 {
            Self::Self_
        } else if distance <= 1.0 {
            Self::Adjacent
        } else if distance <= 4.0 {
            Self::Close
        } else if distance <= 8.0 {
            Self::Medium
        } else if distance <= 16.0 {
            Self::Far
        } else if distance <= 32.0 {
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

/// Bucketed omitted categories in a truncated snapshot (§1.2)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OmittedCategories {
    pub entities: OmittedBucket,
    pub resources: OmittedBucket,
    pub events: OmittedBucket,
}

impl OmittedCategories {
    pub fn all_zero() -> Self {
        Self {
            entities: OmittedBucket::Zero,
            resources: OmittedBucket::Zero,
            events: OmittedBucket::Zero,
        }
    }
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
    pub entities: Vec<SnapshotEntity>,
    pub resources: Vec<SnapshotEntity>,
    #[serde(default)]
    pub events: Vec<SnapshotEntity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotActorContext {
    pub active_drones: Vec<String>,
    pub primary_drone: String,
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
    pub entities: Vec<SnapshotEntity>,
    pub resources: Vec<SnapshotEntity>,
    #[serde(default)]
    pub events: Vec<SnapshotEntity>,
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
    if let Some(owner) = world.get::<crate::components::Owner>(target_entity)
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
fn hex_distance(a: &crate::components::Position, b: &crate::components::Position) -> f64 {
    let dx = (a.x - b.x).unsigned_abs() as f64;
    let dy = (a.y - b.y).unsigned_abs() as f64;
    let dz = ((a.x + a.y) - (b.x + b.y)).unsigned_abs() as f64;
    (dx + dy + dz) / 2.0
}

/// Classify an entity into a SnapshotEntity
fn classify_entity(world: &World, entity: Entity) -> Option<SnapshotEntity> {
    let entity_id = object_id(entity).to_string();
    let position = world
        .get::<crate::components::Position>(entity)
        .map(|p| (p.room.0, p.x, p.y));

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
    let drone_pos = world
        .get::<crate::components::Position>(drone_entity)
        .copied();

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
                if let Some(ep) = world.get::<crate::components::Position>(entity) {
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
        serde_json::to_string(entities).unwrap_or_default().len()
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
        },
        entities: kept_entities,
        resources: Vec::new(),
        events: Vec::new(),
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

    if owned_drones.is_empty() {
        return None;
    }

    let active_drones = owned_drones
        .iter()
        .map(|entity| object_id(*entity).to_string())
        .collect::<Vec<_>>();
    let actor_context = SnapshotActorContext {
        primary_drone: active_drones[0].clone(),
        active_drones,
    };
    let drone_positions = owned_drones
        .iter()
        .filter_map(|entity| {
            world
                .get::<crate::components::Position>(*entity)
                .copied()
                .map(|position| (*entity, position))
        })
        .collect::<Vec<_>>();
    let visible = config
        .fog_of_war
        .then(|| visible_positions(world, player_id));

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
            } else if let Some(entity_position) = world.get::<crate::components::Position>(entity) {
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
        serde_json::to_vec(&PerPlayerSnapshot {
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
            },
            entities: entities.to_vec(),
            resources: Vec::new(),
            events: Vec::new(),
        })
        .unwrap_or_default()
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
        },
        entities: kept_entities,
        resources: Vec::new(),
        events: Vec::new(),
    })
}

fn is_visible_with_precomputed_positions(
    world: &World,
    entity: Entity,
    player_id: PlayerId,
    visible: &VisibilitySet,
) -> bool {
    if world
        .get::<crate::components::Owner>(entity)
        .is_some_and(|owner| owner.0 == player_id)
        || world.get::<Controller>(entity).is_some()
    {
        return true;
    }

    world
        .get::<crate::components::Position>(entity)
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
    let bytes = serde_json::to_vec(snapshot).unwrap_or_default();
    *blake3::hash(&bytes).as_bytes()
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
    use crate::components::{BodyPart, Drone};

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
        assert_eq!(DistanceBucket::from_distance(0.0), DistanceBucket::Self_);
        assert_eq!(DistanceBucket::from_distance(0.5), DistanceBucket::Adjacent);
        assert_eq!(DistanceBucket::from_distance(1.0), DistanceBucket::Adjacent);
        assert_eq!(DistanceBucket::from_distance(2.0), DistanceBucket::Close);
        assert_eq!(DistanceBucket::from_distance(4.0), DistanceBucket::Close);
        assert_eq!(DistanceBucket::from_distance(5.0), DistanceBucket::Medium);
        assert_eq!(DistanceBucket::from_distance(8.0), DistanceBucket::Medium);
        assert_eq!(DistanceBucket::from_distance(10.0), DistanceBucket::Far);
        assert_eq!(DistanceBucket::from_distance(16.0), DistanceBucket::Far);
        assert_eq!(DistanceBucket::from_distance(20.0), DistanceBucket::VeryFar);
        assert_eq!(DistanceBucket::from_distance(32.0), DistanceBucket::VeryFar);
        assert_eq!(
            DistanceBucket::from_distance(100.0),
            DistanceBucket::OutOfSight
        );
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
        assert!(!snapshot.actor_context.primary_drone.is_empty());
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

        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].player_id, 1);
        assert_eq!(snapshots[1].player_id, 2);
        assert_eq!(snapshots[0].actor_context.active_drones.len(), 2);
        assert_eq!(snapshots[1].actor_context.active_drones.len(), 1);
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
