use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use swarm_engine::components::{BodyPart, BodyPartRegistry, Drone, Position, Resource, RoomId};

#[derive(Resource, Debug, Clone, Default, Serialize, Deserialize)]
pub struct Tick(pub u64);

#[derive(Resource, Debug, Clone, Serialize, Deserialize)]
pub struct WorldConfig {
    pub world_bosses_enabled: bool,
    pub arena_bosses_enabled: bool,
    pub boss_spawn_interval: u64,
}

impl Default for WorldConfig {
    fn default() -> Self {
        Self {
            world_bosses_enabled: true,
            arena_bosses_enabled: true,
            boss_spawn_interval: 5_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BossTemplate {
    pub name: String,
    pub mode: BossMode,
    pub hits: u32,
    pub phases: Vec<u32>,
    pub drops: BTreeMap<String, u32>,
    pub spawn_position: Position,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BossMode {
    #[default]
    World,
    Arena,
}

#[derive(Component, Debug, Clone, Serialize, Deserialize)]
pub struct BossAI {
    pub name: String,
    pub mode: BossMode,
    pub phase: BossPhase,
    pub phase_thresholds: Vec<u32>,
    pub drops: BTreeMap<String, u32>,
    pub spawn_position: Position,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BossPhase {
    #[default]
    Phase1,
    Phase2,
    Phase3,
}

#[derive(Resource, Debug, Clone)]
pub struct VanillaBossConfig {
    pub boss_templates: Vec<BossTemplate>,
    pub arena_bosses_enabled: bool,
    pub world_bosses_enabled: bool,
    pub boss_spawn_interval: u64,
}

#[derive(Debug, Clone)]
pub struct VanillaBossPlugin {
    pub boss_templates: Vec<BossTemplate>,
    pub arena_bosses_enabled: bool,
    pub world_bosses_enabled: bool,
    pub boss_spawn_interval: u64,
}

impl Default for VanillaBossPlugin {
    fn default() -> Self {
        Self {
            boss_templates: vec![
                BossTemplate {
                    name: "world-alpha".to_string(),
                    mode: BossMode::World,
                    hits: 100_000,
                    phases: vec![75, 50, 25],
                    drops: BTreeMap::from([
                        ("Energy".to_string(), 5_000),
                        ("Mineral".to_string(), 100),
                    ]),
                    spawn_position: Position {
                        x: 25,
                        y: 25,
                        room: RoomId(0),
                    },
                },
                BossTemplate {
                    name: "arena-champion".to_string(),
                    mode: BossMode::Arena,
                    hits: 50_000,
                    phases: vec![50, 20],
                    drops: BTreeMap::from([("ArenaToken".to_string(), 1)]),
                    spawn_position: Position {
                        x: 25,
                        y: 25,
                        room: RoomId(1),
                    },
                },
            ],
            arena_bosses_enabled: true,
            world_bosses_enabled: true,
            boss_spawn_interval: 5_000,
        }
    }
}

impl Plugin for VanillaBossPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(VanillaBossConfig {
            boss_templates: self.boss_templates.clone(),
            arena_bosses_enabled: self.arena_bosses_enabled,
            world_bosses_enabled: self.world_bosses_enabled,
            boss_spawn_interval: self.boss_spawn_interval,
        })
        .init_resource::<WorldConfig>()
        .init_resource::<Tick>()
        .add_systems(
            Update,
            (
                boss_spawn_system,
                boss_phase_trigger_system,
                boss_ai_system,
                boss_drop_system,
            )
                .chain(),
        );
    }
}

pub fn boss_spawn_system(
    mut commands: Commands,
    tick: Res<Tick>,
    config: Res<VanillaBossConfig>,
    world: Res<WorldConfig>,
    bosses: Query<&BossAI>,
) {
    let interval = world
        .boss_spawn_interval
        .max(config.boss_spawn_interval)
        .max(1);
    if tick.0 % interval != 0 {
        return;
    }
    for template in &config.boss_templates {
        let enabled = match template.mode {
            BossMode::World => config.world_bosses_enabled && world.world_bosses_enabled,
            BossMode::Arena => config.arena_bosses_enabled && world.arena_bosses_enabled,
        };
        if !enabled || bosses.iter().any(|boss| boss.name == template.name) {
            continue;
        }
        commands.spawn((
            boss_drone(template.hits),
            template.spawn_position,
            BossAI {
                name: template.name.clone(),
                mode: template.mode,
                phase: BossPhase::Phase1,
                phase_thresholds: template.phases.clone(),
                drops: template.drops.clone(),
                spawn_position: template.spawn_position,
            },
        ));
    }
}

pub fn boss_phase_trigger_system(mut bosses: Query<(&mut BossAI, &Drone)>) {
    for (mut boss, drone) in &mut bosses {
        let pct = if drone.hits_max == 0 {
            0
        } else {
            (drone.hits as u64 * 100 / drone.hits_max as u64) as u32
        };
        boss.phase = if pct <= *boss.phase_thresholds.get(2).unwrap_or(&25) {
            BossPhase::Phase3
        } else if pct <= *boss.phase_thresholds.get(1).unwrap_or(&50) {
            BossPhase::Phase2
        } else {
            BossPhase::Phase1
        };
    }
}

pub fn boss_ai_system(mut bosses: Query<(&BossAI, &mut Drone)>) {
    for (boss, mut drone) in &mut bosses {
        match boss.phase {
            BossPhase::Phase1 => {}
            BossPhase::Phase2 => {
                drone.hits = drone.hits.saturating_add(10).min(drone.hits_max);
            }
            BossPhase::Phase3 => {
                drone.hits_max = drone.hits_max.saturating_add(1);
            }
        }
    }
}

pub fn boss_drop_system(
    mut commands: Commands,
    bosses: Query<(Entity, &BossAI, &Drone, &Position)>,
) {
    for (entity, boss, drone, position) in &bosses {
        if drone.hits == 0 {
            commands.spawn((
                Resource {
                    amounts: boss.drops.clone().into_iter().collect(),
                },
                *position,
            ));
            commands.entity(entity).despawn();
        }
    }
}

fn boss_drone(hits: u32) -> Drone {
    let registry = BodyPartRegistry::default();
    let mut drone = Drone::new(0, vec![BodyPart::Tough, BodyPart::Attack], &registry);
    drone.hits = hits;
    drone.hits_max = hits;
    drone
}
