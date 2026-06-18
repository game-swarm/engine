use std::collections::HashMap;
use std::time::{Duration, Instant};

use swarm_engine::{
    BodyPart, CommandIntent, Drone, ExecutorError, InMemoryTickBroadcaster, InMemoryTickCommitter,
    MultiPlayerTickScheduler, PendingGlobalTransfers, PlayerExecutor, PlayerGlobalStorage,
    PlayerId, PlayerLocalStorage, Position, Structure, TickSnapshot, create_world,
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
    npcs: usize,
    structures: usize,
    local_storage_players: usize,
    global_storage_players: usize,
    pending_global_transfers: usize,
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

    let expected_npcs = expected_npc_spawns(TICKS);
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

    // Drones must not leak
    assert_eq!(
        first.before.drones, first.after.drones,
        "first run leaked drones"
    );
    assert_eq!(
        second.before.drones, second.after.drones,
        "second run leaked drones"
    );
    assert_eq!(first.before.drones, PLAYERS as usize);

    // Non-drone resources must not leak (structures, storage, transfers)
    assert_eq!(first.before.structures, first.after.structures);
    assert_eq!(
        first.before.local_storage_players,
        first.after.local_storage_players
    );
    assert_eq!(
        first.before.global_storage_players,
        first.after.global_storage_players
    );
    assert_eq!(
        first.before.pending_global_transfers,
        first.after.pending_global_transfers
    );

    // NPC spawning is deterministic — verify spawned count matches expectation
    let actual_npcs_first = first.after.npcs;
    let actual_npcs_second = second.after.npcs;
    assert_eq!(
        actual_npcs_first, actual_npcs_second,
        "NPC spawn count differs between runs: {} vs {}",
        actual_npcs_first, actual_npcs_second
    );
    assert_eq!(
        actual_npcs_first, expected_npcs,
        "NPC spawn count mismatch: expected {expected_npcs}, got {actual_npcs_first}"
    );
    assert_eq!(first.before.npcs, 0, "NPCs present before tick loop");

    // Total entity drift should equal NPC count only
    let non_npc_growth_first = (first.after.entities - first.after.npcs)
        .saturating_sub(first.before.entities - first.before.npcs);
    let non_npc_growth_second = (second.after.entities - second.after.npcs)
        .saturating_sub(second.before.entities - second.before.npcs);
    assert_eq!(
        non_npc_growth_first, 0,
        "first run: {} unexpected non-NPC entities leaked",
        non_npc_growth_first
    );
    assert_eq!(
        non_npc_growth_second, 0,
        "second run: {} unexpected non-NPC entities leaked",
        non_npc_growth_second
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

/// Compute expected NPC spawn count for N iterations of scheduler.tick().
///
/// The NPC spawn system (npc_spawn_system) spawns:
/// - Creep:    every 50 ticks  (tick 50, 100, ..., floor((N-1)/50) * 50)
/// - Guardian: at tick 300
/// - Merchant: at tick 500
/// - Swarmling: at tick 1000 (with variable count)
///
/// Each spawns 1 entity except Swarmling.
fn expected_npc_spawns(ticks: usize) -> usize {
    if ticks == 0 {
        return 0;
    }
    let max_tick = ticks.saturating_sub(1);
    let mut count = max_tick / 50; // Creep: one at every 50-tick boundary
    if max_tick >= 300 {
        count += 1; // Guardian
    }
    if max_tick >= 500 {
        count += 1; // Merchant
    }
    if max_tick >= 1000 {
        // Swarmling: 10 + (1000 % 21) = 10 + 13 = 23 at first spawn
        count += 23;
        // Additional every 1000 ticks after
        for extra in (2000..=max_tick).step_by(1000) {
            count += 10 + ((extra as u32) % 21) as usize;
        }
    }
    count
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

    let npcs = world
        .app
        .world_mut()
        .query::<&swarm_engine::npc::components::NpcType>()
        .iter(world.app.world())
        .count();

    ResourceFootprint {
        entities,
        positioned,
        drones,
        npcs,
        structures,
        local_storage_players: world.app.world().resource::<PlayerLocalStorage>().0.len(),
        global_storage_players: world.app.world().resource::<PlayerGlobalStorage>().0.len(),
        pending_global_transfers: world
            .app
            .world()
            .resource::<PendingGlobalTransfers>()
            .0
            .len(),
    }
}

fn percentile_nearest_rank(mut values: Vec<Duration>, percentile: usize) -> Duration {
    assert!(!values.is_empty());
    values.sort_unstable();
    let rank = (percentile * values.len()).div_ceil(100).max(1);
    values[rank - 1]
}
