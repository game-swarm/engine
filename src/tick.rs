use std::collections::HashMap;

use bevy::prelude::*;

use crate::command::{
    CommandIntent, CommandRejection, CommandSource, RawCommand, Tick, apply_command, source_gate,
    validate_command,
};
use crate::components::*;
use crate::systems::{PendingCombat, PendingSpawnQueue, RoomDroneCounts};
use crate::world::SwarmWorld;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickSnapshot {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub state_checksum: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutorError {
    Error(String),
    Timeout,
}

pub trait PlayerExecutor {
    fn collect(&mut self, snapshot: TickSnapshot) -> Result<Vec<CommandIntent>, ExecutorError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitError {
    Failed(String),
}

pub trait TickCommitter {
    fn commit(&mut self, record: TickCommitRecord) -> Result<(), CommitError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BroadcastError {
    Failed(String),
}

pub trait TickBroadcaster {
    fn broadcast(&mut self, event: TickBroadcast) -> Result<(), BroadcastError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickCommitRecord {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub accepted: Vec<RawCommand>,
    pub rejections: Vec<CommandRejection>,
    pub state_checksum: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickBroadcast {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub accepted: Vec<RawCommand>,
    pub rejections: Vec<CommandRejection>,
    pub state_checksum: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickMetrics {
    pub executor_errors: u64,
    pub executor_timeouts: u64,
    pub accepted_commands: u64,
    pub rejected_commands: u64,
    pub commit_failures: u64,
    pub broadcast_failures: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickReport {
    pub tick: Tick,
    pub committed: bool,
    pub broadcasted: bool,
    pub accepted: Vec<RawCommand>,
    pub rejections: Vec<CommandRejection>,
    pub metrics: TickMetrics,
}

pub struct TickScheduler<E, C, B> {
    pub world: SwarmWorld,
    pub player_id: PlayerId,
    pub executor: E,
    pub committer: C,
    pub broadcaster: B,
    pub tick_counter: Tick,
    pub metrics: TickMetrics,
}

impl<E, C, B> TickScheduler<E, C, B>
where
    E: PlayerExecutor,
    C: TickCommitter,
    B: TickBroadcaster,
{
    pub fn new(
        world: SwarmWorld,
        player_id: PlayerId,
        executor: E,
        committer: C,
        broadcaster: B,
    ) -> Self {
        Self {
            world,
            player_id,
            executor,
            committer,
            broadcaster,
            tick_counter: 0,
            metrics: TickMetrics::default(),
        }
    }

    pub fn tick(&mut self) -> TickReport {
        let tick = self.tick_counter;
        let snapshot = TickSnapshot {
            tick,
            player_id: self.player_id,
            state_checksum: self.world.state_checksum(),
        };
        let intents = match self.executor.collect(snapshot) {
            Ok(intents) => intents,
            Err(ExecutorError::Timeout) => {
                self.metrics.executor_timeouts += 1;
                Vec::new()
            }
            Err(ExecutorError::Error(_)) => {
                self.metrics.executor_errors += 1;
                Vec::new()
            }
        };

        let world_snapshot = WorldSnapshot::capture(self.world.app.world_mut());
        let mut raw_commands = intents
            .into_iter()
            .filter_map(|intent| {
                source_gate(self.player_id, tick, CommandSource::Wasm, intent).ok()
            })
            .collect::<Vec<_>>();
        raw_commands.sort_by_key(|command| command.sequence);

        let mut accepted = Vec::new();
        let mut rejections = Vec::new();
        for raw in raw_commands {
            match validate_command(self.world.app.world_mut(), raw.clone()) {
                Ok(validated) => match apply_command(self.world.app.world_mut(), validated) {
                    Ok(()) => accepted.push(raw),
                    Err(rejection) => rejections.push(CommandRejection::new(
                        raw,
                        rejection,
                        serde_json::Value::Null,
                    )),
                },
                Err(rejection) => rejections.push(CommandRejection::new(
                    raw,
                    rejection,
                    serde_json::Value::Null,
                )),
            }
        }
        self.metrics.accepted_commands += accepted.len() as u64;
        self.metrics.rejected_commands += rejections.len() as u64;

        self.world.run_tick();
        let checksum = self.world.state_checksum();
        let commit = TickCommitRecord {
            tick,
            player_id: self.player_id,
            accepted: accepted.clone(),
            rejections: rejections.clone(),
            state_checksum: checksum,
        };

        if self.committer.commit(commit).is_err() {
            world_snapshot.restore(self.world.app.world_mut());
            self.metrics.commit_failures += 1;
            return TickReport {
                tick,
                committed: false,
                broadcasted: false,
                accepted,
                rejections,
                metrics: self.metrics.clone(),
            };
        }

        self.tick_counter += 1;
        let broadcast = TickBroadcast {
            tick,
            player_id: self.player_id,
            accepted: accepted.clone(),
            rejections: rejections.clone(),
            state_checksum: checksum,
        };
        let broadcasted = if self.broadcaster.broadcast(broadcast).is_ok() {
            true
        } else {
            self.metrics.broadcast_failures += 1;
            false
        };

        TickReport {
            tick,
            committed: true,
            broadcasted,
            accepted,
            rejections,
            metrics: self.metrics.clone(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryTickCommitter {
    pub records: Vec<TickCommitRecord>,
    pub fail_next: bool,
}

impl TickCommitter for InMemoryTickCommitter {
    fn commit(&mut self, record: TickCommitRecord) -> Result<(), CommitError> {
        if self.fail_next {
            self.fail_next = false;
            return Err(CommitError::Failed("in-memory commit failed".to_string()));
        }

        self.records.push(record);
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryTickBroadcaster {
    pub broadcasts: Vec<TickBroadcast>,
    pub fail_next: bool,
}

impl TickBroadcaster for InMemoryTickBroadcaster {
    fn broadcast(&mut self, event: TickBroadcast) -> Result<(), BroadcastError> {
        if self.fail_next {
            self.fail_next = false;
            return Err(BroadcastError::Failed(
                "in-memory broadcast failed".to_string(),
            ));
        }

        self.broadcasts.push(event);
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct WorldSnapshot {
    entities: HashMap<Entity, EntitySnapshot>,
    terrains: RoomTerrains,
    pending_spawns: PendingSpawnQueue,
    room_counts: RoomDroneCounts,
    pending_combat: PendingCombat,
}

#[derive(Debug, Clone, Default)]
struct EntitySnapshot {
    position: Option<Position>,
    owner: Option<Owner>,
    drone: Option<Drone>,
    structure: Option<Structure>,
    resource: Option<crate::components::Resource>,
    source: Option<Source>,
    terrain: Option<Terrain>,
    controller: Option<Controller>,
    marked_for_death: bool,
}

impl WorldSnapshot {
    fn capture(world: &mut World) -> Self {
        let mut query = world.query::<(
            Entity,
            Option<&Position>,
            Option<&Owner>,
            Option<&Drone>,
            Option<&Structure>,
            Option<&crate::components::Resource>,
            Option<&Source>,
            Option<&Terrain>,
            Option<&Controller>,
            Option<&MarkedForDeath>,
        )>();
        let entities = query
            .iter(world)
            .filter_map(
                |(
                    entity,
                    position,
                    owner,
                    drone,
                    structure,
                    resource,
                    source,
                    terrain,
                    controller,
                    marked_for_death,
                )| {
                    let snapshot = EntitySnapshot {
                        position: position.copied(),
                        owner: owner.copied(),
                        drone: drone.cloned(),
                        structure: structure.cloned(),
                        resource: resource.cloned(),
                        source: source.cloned(),
                        terrain: terrain.copied(),
                        controller: controller.cloned(),
                        marked_for_death: marked_for_death.is_some(),
                    };
                    snapshot.has_any().then_some((entity, snapshot))
                },
            )
            .collect();

        Self {
            entities,
            terrains: world.resource::<RoomTerrains>().clone(),
            pending_spawns: world.resource::<PendingSpawnQueue>().clone(),
            room_counts: world.resource::<RoomDroneCounts>().clone(),
            pending_combat: world.resource::<PendingCombat>().clone(),
        }
    }

    fn restore(self, world: &mut World) {
        let current_entities = Self::tracked_entities(world);
        for entity in current_entities {
            if !self.entities.contains_key(&entity) {
                let _ = world.despawn(entity);
            }
        }

        for (entity, snapshot) in self.entities {
            #[allow(deprecated)]
            let mut entity_mut = world
                .get_or_spawn(entity)
                .expect("snapshot entity should be spawnable during restore");
            restore_component(&mut entity_mut, snapshot.position);
            restore_component(&mut entity_mut, snapshot.owner);
            restore_component(&mut entity_mut, snapshot.drone);
            restore_component(&mut entity_mut, snapshot.structure);
            restore_component(&mut entity_mut, snapshot.resource);
            restore_component(&mut entity_mut, snapshot.source);
            restore_component(&mut entity_mut, snapshot.terrain);
            restore_component(&mut entity_mut, snapshot.controller);
            if snapshot.marked_for_death {
                entity_mut.insert(MarkedForDeath);
            } else {
                entity_mut.remove::<MarkedForDeath>();
            }
        }

        *world.resource_mut::<RoomTerrains>() = self.terrains;
        *world.resource_mut::<PendingSpawnQueue>() = self.pending_spawns;
        *world.resource_mut::<RoomDroneCounts>() = self.room_counts;
        *world.resource_mut::<PendingCombat>() = self.pending_combat;
    }

    fn tracked_entities(world: &mut World) -> Vec<Entity> {
        let mut query = world.query::<(
            Entity,
            Option<&Position>,
            Option<&Owner>,
            Option<&Drone>,
            Option<&Structure>,
            Option<&crate::components::Resource>,
            Option<&Source>,
            Option<&Terrain>,
            Option<&Controller>,
            Option<&MarkedForDeath>,
        )>();
        query
            .iter(world)
            .filter_map(
                |(
                    entity,
                    position,
                    owner,
                    drone,
                    structure,
                    resource,
                    source,
                    terrain,
                    controller,
                    marked_for_death,
                )| {
                    let has_any = position.is_some()
                        || owner.is_some()
                        || drone.is_some()
                        || structure.is_some()
                        || resource.is_some()
                        || source.is_some()
                        || terrain.is_some()
                        || controller.is_some()
                        || marked_for_death.is_some();
                    has_any.then_some(entity)
                },
            )
            .collect()
    }
}

impl EntitySnapshot {
    fn has_any(&self) -> bool {
        self.position.is_some()
            || self.owner.is_some()
            || self.drone.is_some()
            || self.structure.is_some()
            || self.resource.is_some()
            || self.source.is_some()
            || self.terrain.is_some()
            || self.controller.is_some()
            || self.marked_for_death
    }
}

fn restore_component<T: Component>(entity: &mut EntityWorldMut<'_>, component: Option<T>) {
    if let Some(component) = component {
        entity.insert(component);
    } else {
        entity.remove::<T>();
    }
}

#[cfg(test)]
mod tests {
    use crate::command::{CommandAction, Direction, object_id};
    use crate::systems::PendingSpawnQueue;
    use crate::{BodyPart, CommandIntent, Structure, StructureType, create_world};

    use super::*;

    #[derive(Debug, Clone)]
    struct StaticExecutor {
        result: Result<Vec<CommandIntent>, ExecutorError>,
    }

    impl PlayerExecutor for StaticExecutor {
        fn collect(
            &mut self,
            _snapshot: TickSnapshot,
        ) -> Result<Vec<CommandIntent>, ExecutorError> {
            self.result.clone()
        }
    }

    fn drone_count(world: &mut SwarmWorld) -> usize {
        world
            .app
            .world_mut()
            .query::<&Drone>()
            .iter(world.app.world())
            .count()
    }

    fn spawn_structure(world: &mut SwarmWorld, owner: PlayerId, x: i32, y: i32) -> Entity {
        world
            .app
            .world_mut()
            .spawn((
                Position {
                    x,
                    y,
                    room: RoomId(0),
                },
                Structure {
                    structure_type: StructureType::Spawn,
                    owner: Some(owner),
                    hits: 5_000,
                    hits_max: 5_000,
                    energy: Some(300),
                    energy_capacity: Some(300),
                    cooldown: 0,
                },
            ))
            .id()
    }

    #[test]
    fn normal_tick_collects_executes_commits_broadcasts_and_increments() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let executor = StaticExecutor {
            result: Ok(vec![CommandIntent {
                sequence: 2,
                action: CommandAction::Move {
                    object_id: object_id(drone),
                    direction: Direction::Top,
                },
            }]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();

        assert!(report.committed);
        assert!(report.broadcasted);
        assert_eq!(scheduler.tick_counter, 1);
        assert_eq!(scheduler.committer.records.len(), 1);
        assert_eq!(scheduler.broadcaster.broadcasts.len(), 1);
        assert_eq!(report.accepted.len(), 1);
        assert_eq!(report.rejections.len(), 0);
        assert_eq!(
            scheduler
                .world
                .app
                .world()
                .entity(drone)
                .get::<Position>()
                .unwrap()
                .y,
            9
        );
        assert_eq!(
            scheduler
                .world
                .app
                .world()
                .entity(drone)
                .get::<Drone>()
                .unwrap()
                .age,
            1
        );
    }

    #[test]
    fn executor_error_and_timeout_record_metrics_and_emit_empty_commands() {
        for result in [
            Err(ExecutorError::Error("boom".to_string())),
            Err(ExecutorError::Timeout),
        ] {
            let executor = StaticExecutor { result };
            let mut scheduler = TickScheduler::new(
                create_world(),
                1,
                executor,
                InMemoryTickCommitter::default(),
                InMemoryTickBroadcaster::default(),
            );

            let report = scheduler.tick();

            assert!(report.committed);
            assert!(report.accepted.is_empty());
            assert!(report.rejections.is_empty());
            assert_eq!(scheduler.committer.records[0].accepted.len(), 0);
        }
    }

    #[test]
    fn commit_failure_rolls_back_world_and_does_not_increment_or_broadcast() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let before = world.state_checksum();
        let executor = StaticExecutor {
            result: Ok(vec![CommandIntent {
                sequence: 1,
                action: CommandAction::Move {
                    object_id: object_id(drone),
                    direction: Direction::Top,
                },
            }]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter {
                fail_next: true,
                ..Default::default()
            },
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();

        assert!(!report.committed);
        assert!(!report.broadcasted);
        assert_eq!(scheduler.tick_counter, 0);
        assert_eq!(scheduler.broadcaster.broadcasts.len(), 0);
        assert_eq!(before, scheduler.world.state_checksum());
        assert_eq!(
            scheduler
                .world
                .app
                .world()
                .entity(drone)
                .get::<Position>()
                .unwrap()
                .y,
            10
        );
    }

    #[test]
    fn broadcast_failure_does_not_rollback_commit_or_tick_increment() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let executor = StaticExecutor {
            result: Ok(vec![CommandIntent {
                sequence: 1,
                action: CommandAction::Move {
                    object_id: object_id(drone),
                    direction: Direction::Top,
                },
            }]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster {
                fail_next: true,
                ..Default::default()
            },
        );

        let report = scheduler.tick();

        assert!(report.committed);
        assert!(!report.broadcasted);
        assert_eq!(scheduler.tick_counter, 1);
        assert_eq!(scheduler.committer.records.len(), 1);
        assert_eq!(scheduler.broadcaster.broadcasts.len(), 0);
        assert_eq!(
            scheduler
                .world
                .app
                .world()
                .entity(drone)
                .get::<Position>()
                .unwrap()
                .y,
            9
        );
    }

    #[test]
    fn spawn_drone_command_materializes_after_phase_2b() {
        let mut world = create_world();
        let spawn = spawn_structure(&mut world, 1, 10, 10);
        let executor = StaticExecutor {
            result: Ok(vec![CommandIntent {
                sequence: 1,
                action: CommandAction::SpawnDrone {
                    spawn_id: object_id(spawn),
                    body: vec![BodyPart::Move],
                },
            }]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();

        assert!(report.committed);
        assert_eq!(report.accepted.len(), 1);
        assert_eq!(
            scheduler
                .world
                .app
                .world()
                .resource::<PendingSpawnQueue>()
                .0
                .len(),
            0
        );
        assert_eq!(drone_count(&mut scheduler.world), 1);
    }
}
