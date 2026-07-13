// P5-1: Command Validation Throughput Benchmark
// Measure: validate + apply 10k commands p99 < 50ms validate, < 100ms apply

use criterion::{Criterion, criterion_group, criterion_main};
use swarm_engine::command::{
    CommandAction, CommandIntent, CommandSource, Direction, object_id, source_gate,
};
use swarm_engine::components::BodyPart;
use swarm_engine::create_world;

fn bench_command_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("command_throughput");
    group.sample_size(20);

    group.bench_function("validate_1k_move_commands", |b| {
        let mut world = create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let intents: Vec<CommandIntent> = (0..1000)
            .map(|i| CommandIntent {
                sequence: i,
                action: CommandAction::Move {
                    object_id: object_id(drone),
                    direction: Direction::TopRight,
                },
            })
            .collect();

        b.iter(|| {
            for intent in &intents {
                let _ = source_gate(1, 0, CommandSource::Wasm, intent.clone());
            }
        });
    });

    group.bench_function("validate_1k_attack_commands", |b| {
        let mut world = create_world();
        let attacker = world.spawn_drone(1, 10, 10, vec![BodyPart::Move, BodyPart::Attack]);
        let target = world.spawn_drone(2, 11, 10, vec![BodyPart::Move]);
        let intents: Vec<CommandIntent> = (0..1000)
            .map(|i| CommandIntent {
                sequence: i,
                action: CommandAction::Action {
                    action_type: "Attack".to_string(),
                    object_id: object_id(attacker),
                    target_id: Some(object_id(target)),
                    payload: serde_json::Value::Object(serde_json::Map::new()),
                },
            })
            .collect();

        b.iter(|| {
            for intent in &intents {
                let _ = source_gate(1, 0, CommandSource::Wasm, intent.clone());
            }
        });
    });

    group.finish();
}

criterion_group!(benches, bench_command_throughput);
criterion_main!(benches);
