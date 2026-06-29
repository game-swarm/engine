// P5-4: Tick Loop Benchmark
// Measure: 100-tick loop with 10 drones + 2 NPCs

use criterion::{Criterion, criterion_group, criterion_main};
use swarm_engine::components::{BodyPart, Position, RoomId};
use swarm_engine::npc::ai::{Npc, NpcBehavior, NpcSpecialAttack, NpcType};
use swarm_engine::{create_world, spawn_drone};

fn bench_tick_loop(c: &mut Criterion) {
    let mut group = c.benchmark_group("tick_loop");
    group.sample_size(20);

    group.bench_function("100_ticks_10_drones_2_npcs", |b| {
        b.iter(|| {
            let mut world = create_world();

            for i in 0..10 {
                spawn_drone(
                    &mut world,
                    (i % 5) + 1,
                    (i * 3) as i32,
                    5,
                    vec![BodyPart::Move, BodyPart::Attack, BodyPart::Heal],
                );
            }

            world.app.world_mut().spawn((
                Position {
                    x: 25,
                    y: 5,
                    room: RoomId(0),
                },
                Npc::new(NpcType::Guardian).with_special(NpcSpecialAttack::Hack, 3),
                NpcBehavior::guard(Position {
                    x: 25,
                    y: 5,
                    room: RoomId(0),
                }),
            ));

            for _ in 0..100 {
                world.run_tick();
            }
        });
    });

    group.finish();
}

criterion_group!(benches, bench_tick_loop);
criterion_main!(benches);
