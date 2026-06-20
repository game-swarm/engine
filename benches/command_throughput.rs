// P5-1: Command Validation Throughput Benchmark
// Measure: validate + apply 10k commands p99 < 50ms validate, < 100ms apply

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use swarm_engine::command::{CommandAction, CommandIntent, CommandSource, Direction, RawCommand, Tick, source_gate};
use swarm_engine::components::{BodyPart, DamageType};
use swarm_engine::{create_world, spawn_drone};

fn bench_command_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("command_throughput");
    group.sample_size(20);

    group.bench_function("validate_1k_move_commands", |b| {
        let mut world = create_world();
        let drone = spawn_drone(&mut world, 1, 10, 10, vec![BodyPart::Move]);
        let intents: Vec<CommandIntent> = (0..1000)
            .map(|i| CommandIntent {
                action: CommandAction::Move {
                    direction: Direction::Right,
                },
                drone_id: Some(drone.to_bits()),
                tick: 0,
                fuel_budget: 10_000_000,
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
        let attacker = spawn_drone(&mut world, 1, 10, 10, vec![BodyPart::Move, BodyPart::Attack]);
        let target = spawn_drone(&mut world, 2, 11, 10, vec![BodyPart::Move]);
        let intents: Vec<CommandIntent> = (0..1000)
            .map(|_| CommandIntent {
                action: CommandAction::Attack {
                    target: Some(target.to_bits()),
                    body_part: BodyPart::Attack,
                    damage_type: DamageType::Kinetic,
                    charge_bonus: 0,
                },
                drone_id: Some(attacker.to_bits()),
                tick: 0,
                fuel_budget: 10_000_000,
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
