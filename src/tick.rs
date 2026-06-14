use std::{collections::HashMap, thread};

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

pub trait PlayerExecutor: Send {
    fn collect(&mut self, snapshot: TickSnapshot) -> Result<Vec<CommandIntent>, ExecutorError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitError {
    Failed(String),
}

pub trait TickCommitter {
    fn commit(&mut self, trace: TickTrace) -> Result<(), CommitError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BroadcastError {
    Failed(String),
}

pub trait TickBroadcaster {
    fn broadcast(&mut self, event: TickBroadcast) -> Result<(), BroadcastError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickTrace {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub commands: Vec<RawCommand>,
    pub state: TickState,
    pub rejections: Vec<CommandRejection>,
    pub metrics: TickMetrics,
    pub state_checksum: u64,
}

impl TickTrace {
    pub fn accepted(&self) -> &[RawCommand] {
        &self.commands
    }
}

pub type TickCommitRecord = TickTrace;

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
pub enum ReplayError {
    MissingPreviousState {
        tick: Tick,
    },
    StateMismatch {
        tick: Tick,
        expected_checksum: u64,
        actual_checksum: u64,
    },
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct CollectedPlayerCommands {
    player_id: PlayerId,
    commands: Vec<RawCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerCollectResult {
    pub player_id: PlayerId,
    pub commands: Vec<RawCommand>,
    pub metrics: TickMetrics,
}

pub fn seeded_player_shuffle(
    mut players: Vec<PlayerId>,
    tick: Tick,
    state_checksum: u64,
) -> Vec<PlayerId> {
    let mut seed_input = Vec::with_capacity(16);
    seed_input.extend_from_slice(&tick.to_le_bytes());
    seed_input.extend_from_slice(&state_checksum.to_le_bytes());
    let mut hasher = blake3::Hasher::new();
    hasher.update(&seed_input);
    let mut reader = hasher.finalize_xof();

    for i in 0..players.len() {
        let remaining = players.len() - i;
        let mut bytes = [0_u8; 8];
        reader.fill(&mut bytes);
        let offset = (u64::from_le_bytes(bytes) as usize) % remaining;
        players.swap(i, i + offset);
    }

    players
}

fn collect_player_commands<E: PlayerExecutor + ?Sized>(
    tick: Tick,
    player_id: PlayerId,
    state_checksum: u64,
    executor: &mut E,
) -> PlayerCollectResult {
    let snapshot = TickSnapshot {
        tick,
        player_id,
        state_checksum,
    };
    let mut metrics = TickMetrics::default();
    let intents = match executor.collect(snapshot) {
        Ok(intents) => intents,
        Err(ExecutorError::Timeout) => {
            metrics.executor_timeouts += 1;
            Vec::new()
        }
        Err(ExecutorError::Error(_)) => {
            metrics.executor_errors += 1;
            Vec::new()
        }
    };
    let commands = intents
        .into_iter()
        .filter_map(|intent| source_gate(player_id, tick, CommandSource::Wasm, intent).ok())
        .collect::<Vec<_>>();

    PlayerCollectResult {
        player_id,
        commands,
        metrics,
    }
}

fn serial_execution_queue(
    collected: Vec<CollectedPlayerCommands>,
    tick: Tick,
    state_checksum: u64,
) -> Vec<RawCommand> {
    let mut by_player = collected
        .into_iter()
        .map(|mut collected| {
            collected.commands.sort_by_key(|command| command.sequence);
            (collected.player_id, collected.commands)
        })
        .collect::<HashMap<_, _>>();
    let player_order =
        seeded_player_shuffle(by_player.keys().copied().collect(), tick, state_checksum);
    let mut queue = Vec::new();
    for player_id in player_order {
        if let Some(commands) = by_player.remove(&player_id) {
            queue.extend(commands);
        }
    }
    queue
}

pub struct MultiPlayerTickScheduler<C, B> {
    pub world: SwarmWorld,
    pub executors: HashMap<PlayerId, Box<dyn PlayerExecutor>>,
    pub committer: C,
    pub broadcaster: B,
    pub tick_counter: Tick,
    pub metrics: TickMetrics,
}

impl<C, B> MultiPlayerTickScheduler<C, B>
where
    C: TickCommitter,
    B: TickBroadcaster,
{
    pub fn new(
        world: SwarmWorld,
        executors: HashMap<PlayerId, Box<dyn PlayerExecutor>>,
        committer: C,
        broadcaster: B,
    ) -> Self {
        Self {
            world,
            executors,
            committer,
            broadcaster,
            tick_counter: 0,
            metrics: TickMetrics::default(),
        }
    }

    pub fn tick(&mut self) -> TickReport {
        let tick = self.tick_counter;
        let state_checksum = self.world.state_checksum();
        let mut results = thread::scope(|scope| {
            self.executors
                .iter_mut()
                .map(|(&player_id, executor)| {
                    scope.spawn(move || {
                        collect_player_commands(tick, player_id, state_checksum, executor.as_mut())
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| handle.join().expect("player executor thread panicked"))
                .collect::<Vec<_>>()
        });
        results.sort_by_key(|result| result.player_id);

        for result in &results {
            self.metrics.executor_errors += result.metrics.executor_errors;
            self.metrics.executor_timeouts += result.metrics.executor_timeouts;
        }

        let collected = results
            .into_iter()
            .map(|result| CollectedPlayerCommands {
                player_id: result.player_id,
                commands: result.commands,
            })
            .collect::<Vec<_>>();
        let raw_commands = serial_execution_queue(collected, tick, state_checksum);

        let world_snapshot = WorldSnapshot::capture(self.world.app.world_mut());
        let execution = execute_deterministic(&mut self.world, raw_commands);
        let accepted = execution.commands;
        let rejections = execution.rejections;
        self.metrics.accepted_commands += accepted.len() as u64;
        self.metrics.rejected_commands += rejections.len() as u64;

        let checksum = self.world.state_checksum();
        let state = TickState::capture(self.world.app.world_mut());
        let trace = TickTrace {
            tick,
            player_id: 0,
            commands: accepted.clone(),
            state,
            rejections: rejections.clone(),
            metrics: self.metrics.clone(),
            state_checksum: checksum,
        };

        if self.committer.commit(trace).is_err() {
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
            player_id: 0,
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

        let execution = execute_deterministic(&mut self.world, raw_commands);
        let accepted = execution.commands;
        let rejections = execution.rejections;
        self.metrics.accepted_commands += accepted.len() as u64;
        self.metrics.rejected_commands += rejections.len() as u64;

        let checksum = self.world.state_checksum();
        let state = TickState::capture(self.world.app.world_mut());
        let trace = TickTrace {
            tick,
            player_id: self.player_id,
            commands: accepted.clone(),
            state,
            rejections: rejections.clone(),
            metrics: self.metrics.clone(),
            state_checksum: checksum,
        };

        if self.committer.commit(trace).is_err() {
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
    pub records: Vec<TickTrace>,
    pub fail_next: bool,
}

impl TickCommitter for InMemoryTickCommitter {
    fn commit(&mut self, trace: TickTrace) -> Result<(), CommitError> {
        if self.fail_next {
            self.fail_next = false;
            return Err(CommitError::Failed("in-memory commit failed".to_string()));
        }

        self.records.push(trace);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeterministicExecution {
    pub commands: Vec<RawCommand>,
    pub rejections: Vec<CommandRejection>,
    pub state: TickState,
    pub state_checksum: u64,
}

pub fn execute_deterministic(
    world: &mut SwarmWorld,
    commands: Vec<RawCommand>,
) -> DeterministicExecution {
    let mut accepted = Vec::new();
    let mut rejections = Vec::new();
    for raw in commands {
        match validate_command(world.app.world_mut(), raw.clone()) {
            Ok(validated) => match apply_command(world.app.world_mut(), validated) {
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

    world.run_tick();
    let state_checksum = world.state_checksum();
    let state = TickState::capture(world.app.world_mut());
    DeterministicExecution {
        commands: accepted,
        rejections,
        state,
        state_checksum,
    }
}

pub fn replay_tick(
    previous_state: &TickState,
    trace: &TickTrace,
) -> Result<TickState, ReplayError> {
    let mut world = crate::world::create_world();
    previous_state.clone().restore(world.app.world_mut());
    let replayed = execute_deterministic(&mut world, trace.commands.clone());
    if replayed.state != trace.state {
        return Err(ReplayError::StateMismatch {
            tick: trace.tick,
            expected_checksum: trace.state_checksum,
            actual_checksum: replayed.state_checksum,
        });
    }

    Ok(replayed.state)
}

pub fn replay(initial_state: &TickState, traces: &[TickTrace]) -> Result<TickState, ReplayError> {
    let mut state = initial_state.clone();
    for trace in traces {
        state = replay_tick(&state, trace)?;
    }

    Ok(state)
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

pub type TickState = WorldSnapshot;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldSnapshot {
    entities: HashMap<Entity, EntitySnapshot>,
    terrains: RoomTerrains,
    pending_spawns: PendingSpawnQueue,
    room_counts: RoomDroneCounts,
    pending_combat: PendingCombat,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    pub fn capture(world: &mut World) -> Self {
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

    pub fn restore(self, world: &mut World) {
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
    use std::sync::{Arc, Barrier, Mutex};

    use super::*;

    #[derive(Debug)]
    struct BarrierExecutor {
        player_id: PlayerId,
        sequence: u32,
        action: CommandAction,
        barrier: Arc<Barrier>,
        arrivals: Arc<Mutex<Vec<PlayerId>>>,
    }

    impl PlayerExecutor for BarrierExecutor {
        fn collect(&mut self, snapshot: TickSnapshot) -> Result<Vec<CommandIntent>, ExecutorError> {
            assert_eq!(snapshot.player_id, self.player_id);
            self.arrivals.lock().unwrap().push(self.player_id);
            let wait = self.barrier.wait();
            if wait.is_leader() {
                assert_eq!(self.arrivals.lock().unwrap().len(), 2);
            }
            Ok(vec![CommandIntent {
                sequence: self.sequence,
                action: self.action.clone(),
            }])
        }
    }

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
    fn multi_player_tick_collects_players_in_parallel_and_executes_serially() {
        let mut world = create_world();
        let first = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let second = world.spawn_drone(2, 10, 12, vec![BodyPart::Move]);
        let barrier = Arc::new(Barrier::new(2));
        let arrivals = Arc::new(Mutex::new(Vec::new()));
        let mut executors: HashMap<PlayerId, Box<dyn PlayerExecutor>> = HashMap::new();
        executors.insert(
            1,
            Box::new(BarrierExecutor {
                player_id: 1,
                sequence: 1,
                action: CommandAction::Move {
                    object_id: object_id(first),
                    direction: Direction::Top,
                },
                barrier: barrier.clone(),
                arrivals: arrivals.clone(),
            }),
        );
        executors.insert(
            2,
            Box::new(BarrierExecutor {
                player_id: 2,
                sequence: 1,
                action: CommandAction::Move {
                    object_id: object_id(second),
                    direction: Direction::Top,
                },
                barrier,
                arrivals,
            }),
        );
        let mut scheduler = MultiPlayerTickScheduler::new(
            world,
            executors,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();

        assert!(report.committed);
        assert!(report.broadcasted);
        assert_eq!(scheduler.tick_counter, 1);
        assert_eq!(scheduler.committer.records.len(), 1);
        assert_eq!(scheduler.broadcaster.broadcasts.len(), 1);
        assert_eq!(report.accepted.len(), 2);
        assert_eq!(report.rejections.len(), 0);
        assert_eq!(
            scheduler
                .world
                .app
                .world()
                .entity(first)
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
                .entity(second)
                .get::<Position>()
                .unwrap()
                .y,
            11
        );
    }

    #[test]
    fn blake3_xof_player_shuffle_is_deterministic_per_tick_and_checksum() {
        let players = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let first = seeded_player_shuffle(players.clone(), 7, 42);
        let second = seeded_player_shuffle(players.clone(), 7, 42);

        assert_eq!(first, second);
        assert_ne!(first, players);
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
            assert_eq!(scheduler.committer.records[0].commands.len(), 0);
        }
    }

    #[test]
    fn tick_trace_records_commands_state_rejections_and_metrics() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let executor = StaticExecutor {
            result: Ok(vec![
                CommandIntent {
                    sequence: 1,
                    action: CommandAction::Move {
                        object_id: object_id(drone),
                        direction: Direction::Top,
                    },
                },
                CommandIntent {
                    sequence: 2,
                    action: CommandAction::Harvest {
                        object_id: object_id(drone),
                        target_id: 0,
                        resource: None,
                    },
                },
            ]),
        };
        let mut scheduler = TickScheduler::new(
            world,
            1,
            executor,
            InMemoryTickCommitter::default(),
            InMemoryTickBroadcaster::default(),
        );

        let report = scheduler.tick();
        let trace = &scheduler.committer.records[0];

        assert!(report.committed);
        assert_eq!(trace.commands.len(), 1);
        assert_eq!(trace.rejections.len(), 1);
        assert_eq!(trace.metrics.accepted_commands, 1);
        assert_eq!(trace.metrics.rejected_commands, 1);
        assert_eq!(
            trace.state,
            TickState::capture(scheduler.world.app.world_mut())
        );
        assert_eq!(trace.state_checksum, scheduler.world.state_checksum());
    }

    #[test]
    fn replay_tick_succeeds_from_previous_state_and_recorded_commands() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let previous_state = TickState::capture(world.app.world_mut());
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
            InMemoryTickBroadcaster::default(),
        );

        scheduler.tick();
        let trace = scheduler.committer.records[0].clone();

        let replayed = replay_tick(&previous_state, &trace).expect("replay should match trace");

        assert_eq!(replayed, trace.state);
    }

    #[test]
    fn replay_tick_fails_when_recorded_state_does_not_match() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let previous_state = TickState::capture(world.app.world_mut());
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
            InMemoryTickBroadcaster::default(),
        );

        scheduler.tick();
        let mut trace = scheduler.committer.records[0].clone();
        trace.state = previous_state.clone();

        let error =
            replay_tick(&previous_state, &trace).expect_err("replay should detect mismatch");

        assert!(matches!(error, ReplayError::StateMismatch { tick: 0, .. }));
    }

    #[test]
    fn replay_replays_multiple_traces_in_order() {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let initial_state = TickState::capture(world.app.world_mut());
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
            InMemoryTickBroadcaster::default(),
        );

        scheduler.tick();
        scheduler.executor.result = Ok(vec![CommandIntent {
            sequence: 1,
            action: CommandAction::Move {
                object_id: object_id(drone),
                direction: Direction::Top,
            },
        }]);
        scheduler.tick();

        let replayed = replay(&initial_state, &scheduler.committer.records)
            .expect("trace sequence should replay");

        assert_eq!(replayed, scheduler.committer.records[1].state);
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
