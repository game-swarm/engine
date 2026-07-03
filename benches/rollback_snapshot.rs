//! Rollback snapshot/restore benchmark per §8.3 of 05-persistence-contract.md.
//!
//! Gate: 500 entities, all components, p99 < 50ms, entity ID allocator verified.

use std::collections::HashMap;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use bevy::prelude::*;
use swarm_engine::components::*;
use swarm_engine::resources::{PendingGlobalTransfers, PlayerGlobalStorage, PlayerLocalStorage};
use swarm_engine::systems::{PendingCombat, PendingSpawnQueue, Projectile, RoomDroneCounts};
use swarm_engine::tick::WorldSnapshot;

/// Build a world with `n` entities, each carrying a random subset of all tracked components.
fn build_populated_world(n: usize) -> World {
    let mut world = World::new();

    world.init_resource::<RoomTerrains>();
    world.init_resource::<PendingSpawnQueue>();
    world.init_resource::<RoomDroneCounts>();
    world.init_resource::<PendingCombat>();
    world.init_resource::<PlayerLocalStorage>();
    world.init_resource::<PlayerGlobalStorage>();
    world.init_resource::<PendingGlobalTransfers>();
    world.init_resource::<swarm_engine::systems::StartingResourcesGranted>();
    world.init_resource::<swarm_engine::systems::PlayerFirstSpawnTick>();

    for i in 0..n {
        let mut entity = world.spawn_empty();
        entity.insert(Position {
            room: RoomId(i as u32 / 10),
            x: i as i32 % 10,
            y: (i / 10) as i32 % 10,
        });
        entity.insert(Owner(i as PlayerId));
        entity.insert(Drone {
            owner: i as PlayerId,
            body: vec![BodyPart::Move, BodyPart::Attack],
            carry: Default::default(),
            carry_capacity: 100,
            fatigue: 0,
            hits: 100,
            hits_max: 100,
            spawning: false,
            age: 0,
            last_action_tick: 0,
            lifespan: 1500,
        });
        entity.insert(SpawningGrace { remaining: 0 });
        entity.insert(Attributes(vec!["Fortified".into()]));
        entity.insert(EntityFlags(HashMap::from([(
            "immune_Kinetic".into(),
            i % 3 == 0,
        )])));
        entity.insert(DroneEnv::default());
        entity.insert(CodeVersion(1));
        entity.insert(DeathMark);

        if i % 5 == 0 {
            entity.insert(Projectile {
                source: 0,
                target: i as u64,
                damage_type: "Kinetic".into(),
                damage: 30,
                speed: 3,
                ticks_remaining: i as u32 % 4,
            });
        }
    }

    world.flush();
    world
}

fn bench_snapshot_capture(c: &mut Criterion) {
    let mut group = c.benchmark_group("rollback_snapshot");
    group.sample_size(100);
    group.measurement_time(std::time::Duration::from_secs(10));

    group.bench_function("capture_500_entities", |b| {
        let mut world = build_populated_world(500);
        b.iter(|| black_box(WorldSnapshot::capture(black_box(&mut world))));
    });

    group.bench_function("restore_500_entities", |b| {
        b.iter_batched(
            || {
                let mut world = build_populated_world(500);
                let snapshot = WorldSnapshot::capture(&mut world);
                (world, snapshot)
            },
            |(mut world, snapshot)| {
                black_box(snapshot.restore(black_box(&mut world)));
            },
            criterion::BatchSize::LargeInput,
        );
    });

    group.finish();
}

fn bench_allocator_determinism(c: &mut Criterion) {
    let mut group = c.benchmark_group("rollback_allocator");
    group.sample_size(50);

    group.bench_function("verify_allocator_500", |b| {
        b.iter_batched(
            || {
                let mut world = build_populated_world(500);
                let snapshot = WorldSnapshot::capture(&mut world);
                let total_before = snapshot.entity_total_count;
                (world, snapshot, total_before)
            },
            |(mut world, snapshot, total_before)| {
                let _entity_map = snapshot.restore(&mut world);
                let next = world.spawn_empty().id();
                assert!(
                    next.index().index() <= total_before + 1,
                    "allocator drifted: expected index <= {}, got {}",
                    total_before + 1,
                    next.index().index()
                );
                world.entity_mut(next).despawn();
            },
            criterion::BatchSize::LargeInput,
        );
    });

    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default()
        .warm_up_time(std::time::Duration::from_secs(3))
        .measurement_time(std::time::Duration::from_secs(10))
        .sample_size(100);
    targets = bench_snapshot_capture, bench_allocator_determinism
);
criterion_main!(benches);
