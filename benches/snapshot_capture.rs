// P5-2: Snapshot Capture/Restore Benchmark
// Measure: 50 entity world snapshot < 20ms capture, < 30ms restore

use criterion::{Criterion, criterion_group, criterion_main};
use swarm_engine::components::BodyPart;
use swarm_engine::tick::WorldSnapshot;
use swarm_engine::{create_world, spawn_drone};

fn bench_snapshot_capture(c: &mut Criterion) {
    let mut group = c.benchmark_group("snapshot");
    group.sample_size(50);

    // Build a world with 50 drones
    let mut world = create_world();
    for i in 0..50 {
        spawn_drone(
            &mut world,
            (i % 5) + 1, // 5 players
            (i % 40) as i32,
            (i / 40) as i32,
            vec![BodyPart::Move, BodyPart::Attack, BodyPart::Heal],
        );
    }

    group.bench_function("capture_50_drones", |b| {
        b.iter(|| WorldSnapshot::capture(world.app.world_mut()));
    });

    let snapshot = WorldSnapshot::capture(world.app.world_mut());

    group.bench_function("restore_50_drones", |b| {
        b.iter(|| {
            // Restore to a fresh world
            let mut fresh = create_world();
            snapshot.clone().restore(fresh.app.world_mut());
        });
    });

    group.finish();
}

criterion_group!(benches, bench_snapshot_capture);
criterion_main!(benches);
