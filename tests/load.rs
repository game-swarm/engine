use std::collections::HashMap;
use std::time::{Duration, Instant};

use swarm_engine::{
    create_world, BodyPart, CommandIntent, Drone, ExecutorError, InMemoryTickBroadcaster,
    InMemoryTickCommitter, MarketOrders, MultiPlayerTickScheduler, PendingGlobalTransfers,
    PlayerExecutor, PlayerGlobalStorage, PlayerId, PlayerLocalStorage, Position, Structure,
    TickSnapshot,
};

const PLAYERS: PlayerId = 100;
const TICKS: usize = 500;
const P99_LIMIT: Duration = Duration::from_secs(3);

#[derive(Default)]
struct NoopExecutor;

impl PlayerExecutor for NoopExecutor {
    fn collect(&mut self, _snapshot: TickSnapshot) -> Result<Vec<CommandIntent>, ExecutorError> {
        Ok(Vec::new())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResourceFootprint {
    entities: usize,
    positioned: usize,
    drones: usize,
    structures: usize,
    local_storage_players: usize,
    global_storage_players: usize,
    pending_global_transfers: usize,
    market_orders: usize,
}

#[derive(Debug)]
struct LoadRun {
    checksums: Vec<u64>,
    final_checksum: u64,
    p99: Duration,
    before: ResourceFootprint,
    after: ResourceFootprint,
}

#[test]
fn multiplayer_tick_scheduler_handles_load_deterministically_without_leaks() {
    let first = run_load();
    let second = run_load();

    assert!(
        first.p99 < P99_LIMIT,
        "first load-test p99 {:?} exceeded {:?}",
        first.p99,
        P99_LIMIT
    );
    assert!(
        second.p99 < P99_LIMIT,
        "second load-test p99 {:?} exceeded {:?}",
        second.p99,
        P99_LIMIT
    );

    assert_eq!(first.checksums.len(), TICKS);
    assert_eq!(first.checksums, second.checksums);
    assert_eq!(first.final_checksum, second.final_checksum);

    assert_eq!(
        first.before, first.after,
        "first run leaked world resources"
    );
    assert_eq!(
        second.before, second.after,
        "second run leaked world resources"
    );
}

fn run_load() -> LoadRun {
    let mut world = create_world();
    for player_id in 1..=PLAYERS {
        let index = player_id as i32 - 1;
        world.spawn_drone(
            player_id,
            index % 50,
            index / 50,
            vec![BodyPart::Move, BodyPart::Carry],
        );
    }

    let before = resource_footprint(&mut world);
    let mut scheduler = MultiPlayerTickScheduler::new(
        world,
        executors(),
        InMemoryTickCommitter::default(),
        InMemoryTickBroadcaster::default(),
    );
    let mut durations = Vec::with_capacity(TICKS);

    for expected_tick in 0..TICKS {
        let started_at = Instant::now();
        let report = scheduler.tick();
        durations.push(started_at.elapsed());

        assert_eq!(report.tick as usize, expected_tick);
        assert!(report.committed, "tick {expected_tick} did not commit");
        assert!(report.broadcasted, "tick {expected_tick} did not broadcast");
        assert_eq!(report.metrics.executor_errors, 0);
        assert_eq!(report.metrics.executor_timeouts, 0);
        assert!(report.accepted.is_empty());
        assert!(report.rejections.is_empty());
    }

    let checksums = scheduler
        .committer
        .records
        .iter()
        .map(|record| record.state_checksum)
        .collect::<Vec<_>>();
    assert_eq!(scheduler.committer.records.len(), TICKS);
    for (expected_tick, record) in scheduler.committer.records.iter().enumerate() {
        assert_eq!(record.tick as usize, expected_tick);
        assert_eq!(record.metrics.executor_errors, 0);
        assert_eq!(record.metrics.executor_timeouts, 0);
    }

    let final_checksum = scheduler.world.state_checksum();
    assert_eq!(checksums.last().copied(), Some(final_checksum));
    let after = resource_footprint(&mut scheduler.world);

    LoadRun {
        checksums,
        final_checksum,
        p99: percentile_nearest_rank(durations, 99),
        before,
        after,
    }
}

fn executors() -> HashMap<PlayerId, Box<dyn PlayerExecutor>> {
    (1..=PLAYERS)
        .map(|player_id| {
            (
                player_id,
                Box::<NoopExecutor>::default() as Box<dyn PlayerExecutor>,
            )
        })
        .collect()
}

fn resource_footprint(world: &mut swarm_engine::SwarmWorld) -> ResourceFootprint {
    let entities = world.app.world().iter_entities().count();
    let positioned = world
        .app
        .world_mut()
        .query::<&Position>()
        .iter(world.app.world())
        .count();
    let drones = world
        .app
        .world_mut()
        .query::<&Drone>()
        .iter(world.app.world())
        .count();
    let structures = world
        .app
        .world_mut()
        .query::<&Structure>()
        .iter(world.app.world())
        .count();

    ResourceFootprint {
        entities,
        positioned,
        drones,
        structures,
        local_storage_players: world.app.world().resource::<PlayerLocalStorage>().0.len(),
        global_storage_players: world.app.world().resource::<PlayerGlobalStorage>().0.len(),
        pending_global_transfers: world
            .app
            .world()
            .resource::<PendingGlobalTransfers>()
            .0
            .len(),
        market_orders: world.app.world().resource::<MarketOrders>().orders.len(),
    }
}

fn percentile_nearest_rank(mut values: Vec<Duration>, percentile: usize) -> Duration {
    assert!(!values.is_empty());
    values.sort_unstable();
    let rank = (percentile * values.len()).div_ceil(100).max(1);
    values[rank - 1]
}
