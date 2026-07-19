// P5-1: Command Validation Throughput Benchmark
// Measure: parse and scheduler validate/apply for one capped 100-command player batch.
// 10k aggregate throughput belongs in multiplayer load tests (100 players x100 commands).

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use swarm_engine::command::{
    CommandAction, CommandIntent, Direction, object_id, parse_tick_output,
};
use swarm_engine::{
    BroadcastError, CommitError, MultiPlayerTickScheduler, PlayerExecutor, TickBroadcast,
    TickBroadcaster, TickCommitter, TickReport, TickSnapshot, TickTrace, create_world,
};
use swarm_engine_api::ids::BodyPart;

const COMMAND_BATCH_SIZE: usize = 100;

#[derive(Default)]
struct NoopCommitter;

impl TickCommitter for NoopCommitter {
    fn commit(&mut self, _trace: TickTrace) -> Result<(), CommitError> {
        Ok(())
    }
}

#[derive(Default)]
struct NoopBroadcaster;

impl TickBroadcaster for NoopBroadcaster {
    fn broadcast(&mut self, _event: TickBroadcast) -> Result<(), BroadcastError> {
        Ok(())
    }
}

struct StaticExecutor {
    intents: Vec<CommandIntent>,
}

impl PlayerExecutor for StaticExecutor {
    fn collect(
        &mut self,
        _snapshot: TickSnapshot,
    ) -> Result<Vec<CommandIntent>, swarm_engine::ExecutorError> {
        Ok(self.intents.clone())
    }
}

fn bench_command_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("command_throughput");
    group.sample_size(20);

    group.bench_function("parse_100_move_commands", |b| {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let intents: Vec<CommandIntent> = (0..COMMAND_BATCH_SIZE)
            .map(|i| CommandIntent {
                sequence: i as u32,
                action: CommandAction::Move {
                    object_id: object_id(drone),
                    direction: Direction::TopRight,
                },
            })
            .collect();
        let output = serde_json::to_vec(&intents).expect("move intents should serialize");

        b.iter(|| {
            let _ = parse_tick_output(1, 0, &output);
        });
    });

    group.bench_function("parse_100_attack_commands", |b| {
        let mut world = create_world();
        let attacker = world.spawn_drone(1, 10, 10, vec![BodyPart::Move, BodyPart::Attack]);
        let target = world.spawn_drone(2, 11, 10, vec![BodyPart::Move]);
        let intents: Vec<CommandIntent> = (0..COMMAND_BATCH_SIZE)
            .map(|i| CommandIntent {
                sequence: i as u32,
                action: CommandAction::Attack {
                    object_id: object_id(attacker),
                    target_id: object_id(target),
                    resource: None,
                    amount: None,
                    range: None,
                    structure: None,
                    damage_type: None,
                    cooldown: None,
                },
            })
            .collect();
        let output = serde_json::to_vec(&intents).expect("attack intents should serialize");

        b.iter(|| {
            let _ = parse_tick_output(1, 0, &output);
        });
    });

    group.bench_function("validate_apply_100_move_commands", |b| {
        b.iter_batched(
            scheduler_with_100_move_commands,
            |mut scheduler| {
                let report = scheduler.tick();
                assert_tick_report_applied_commands(&report);
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn scheduler_with_100_move_commands() -> MultiPlayerTickScheduler<NoopCommitter, NoopBroadcaster> {
    let mut world = create_world();
    let intents = (0..COMMAND_BATCH_SIZE)
        .map(|index| {
            let drone = world.spawn_drone(
                1,
                5 + (index as i32 % 10) * 3,
                5 + (index as i32 / 10) * 3,
                vec![BodyPart::Move],
            );
            CommandIntent {
                sequence: index as u32,
                action: CommandAction::Move {
                    object_id: object_id(drone),
                    direction: Direction::TopRight,
                },
            }
        })
        .collect::<Vec<_>>();
    let mut executors: std::collections::HashMap<_, Box<dyn PlayerExecutor>> =
        std::collections::HashMap::new();
    executors.insert(1, Box::new(StaticExecutor { intents }));
    MultiPlayerTickScheduler::new(world, executors, NoopCommitter, NoopBroadcaster)
}

fn assert_tick_report_applied_commands(report: &TickReport) {
    assert!(!report.accepted.is_empty());
    assert!(
        report.rejections.is_empty(),
        "validate/apply benchmark must not reject commands: {:?}",
        report.rejections
    );
}

criterion_group!(benches, bench_command_throughput);
criterion_main!(benches);
