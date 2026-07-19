use bevy::prelude::*;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use swarm_engine_api::ids::RoomId;
use swarm_engine_plugin_sdk::components::Position;

use crate::components::RoomTerrains;
use crate::resources::{ResourceAmount, ResourceCost, ResourceName};

pub const STRONGHOLD_PROBABILITY_SCALE: u32 = 10_000;

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stronghold {
    pub stronghold_type: StrongholdType,
    pub production: ResourceCost,
    pub stored: ResourceCost,
    pub guard_count: u32,
    pub guard_type: StrongholdGuardType,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StrongholdGuard {
    pub stronghold: Entity,
    pub guard_type: StrongholdGuardType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StrongholdType {
    RichVein,
    AncientRuins,
    EnergySpring,
}

impl StrongholdType {
    pub fn production(self) -> ResourceCost {
        match self {
            Self::RichVein => resource_cost("Energy", 5),
            Self::AncientRuins => resource_cost("Crystal", 2),
            Self::EnergySpring => resource_cost("Energy", 12),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StrongholdGuardType {
    Miner,
    Sentinel,
    Warden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StrongholdZone {
    Core,
    Frontier,
    Remote,
}

impl StrongholdZone {
    pub fn for_room(room: RoomId) -> Self {
        let (_, x, y) = room.sector_coordinates();
        let distance = x.unsigned_abs() + y.unsigned_abs();
        if distance <= 1 {
            Self::Core
        } else if distance <= 4 {
            Self::Frontier
        } else {
            Self::Remote
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrongholdZoneConfig {
    pub zone: StrongholdZone,
    pub probability_per_10_000: u32,
    pub stronghold_type: StrongholdType,
    pub guard_count: u32,
    pub guard_type: StrongholdGuardType,
}

#[derive(Resource, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrongholdSpawnConfig {
    pub zones: Vec<StrongholdZoneConfig>,
}

#[derive(Resource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnedStrongholdRooms(pub IndexMap<RoomId, Entity>);

impl Default for StrongholdSpawnConfig {
    fn default() -> Self {
        Self {
            zones: vec![
                StrongholdZoneConfig {
                    zone: StrongholdZone::Core,
                    probability_per_10_000: 0,
                    stronghold_type: StrongholdType::RichVein,
                    guard_count: 2,
                    guard_type: StrongholdGuardType::Miner,
                },
                StrongholdZoneConfig {
                    zone: StrongholdZone::Frontier,
                    probability_per_10_000: 0,
                    stronghold_type: StrongholdType::AncientRuins,
                    guard_count: 3,
                    guard_type: StrongholdGuardType::Sentinel,
                },
                StrongholdZoneConfig {
                    zone: StrongholdZone::Remote,
                    probability_per_10_000: 0,
                    stronghold_type: StrongholdType::EnergySpring,
                    guard_count: 4,
                    guard_type: StrongholdGuardType::Warden,
                },
            ],
        }
    }
}

impl StrongholdSpawnConfig {
    pub fn enabled() -> Self {
        Self {
            zones: vec![
                StrongholdZoneConfig {
                    zone: StrongholdZone::Core,
                    probability_per_10_000: 1_000,
                    stronghold_type: StrongholdType::RichVein,
                    guard_count: 2,
                    guard_type: StrongholdGuardType::Miner,
                },
                StrongholdZoneConfig {
                    zone: StrongholdZone::Frontier,
                    probability_per_10_000: 1_500,
                    stronghold_type: StrongholdType::AncientRuins,
                    guard_count: 3,
                    guard_type: StrongholdGuardType::Sentinel,
                },
                StrongholdZoneConfig {
                    zone: StrongholdZone::Remote,
                    probability_per_10_000: 2_000,
                    stronghold_type: StrongholdType::EnergySpring,
                    guard_count: 4,
                    guard_type: StrongholdGuardType::Warden,
                },
            ],
        }
    }

    pub fn for_zone(&self, zone: StrongholdZone) -> Option<&StrongholdZoneConfig> {
        self.zones.iter().find(|config| config.zone == zone)
    }
}

pub fn stronghold_spawn_system(
    mut commands: Commands,
    config: Res<StrongholdSpawnConfig>,
    terrains: Res<RoomTerrains>,
    mut spawned_rooms: ResMut<SpawnedStrongholdRooms>,
) {
    for (&room, terrain) in terrains.0.iter() {
        if spawned_rooms.0.contains_key(&room) {
            continue;
        }

        let Some(zone_config) = config.for_zone(StrongholdZone::for_room(room)) else {
            continue;
        };

        if !stronghold_should_spawn(room, zone_config) {
            continue;
        }

        let position = Position {
            x: terrain.width / 2,
            y: terrain.height / 2,
            room,
        };
        let stronghold = commands
            .spawn((
                position,
                Stronghold {
                    stronghold_type: zone_config.stronghold_type,
                    production: zone_config.stronghold_type.production(),
                    stored: ResourceCost::new(),
                    guard_count: zone_config.guard_count,
                    guard_type: zone_config.guard_type,
                },
            ))
            .id();

        spawned_rooms.0.insert(room, stronghold);
        for index in 0..zone_config.guard_count {
            commands.spawn((
                guard_position(position, index),
                StrongholdGuard {
                    stronghold,
                    guard_type: zone_config.guard_type,
                },
            ));
        }
    }
}

pub fn stronghold_production_system(mut strongholds: Query<&mut Stronghold>) {
    for mut stronghold in strongholds.iter_mut() {
        let production = stronghold.production.clone();
        for (resource, amount) in production {
            *stronghold.stored.entry(resource).or_default() += amount;
        }
    }
}

fn stronghold_should_spawn(room: RoomId, config: &StrongholdZoneConfig) -> bool {
    if config.probability_per_10_000 >= STRONGHOLD_PROBABILITY_SCALE {
        return true;
    }

    let roll = room_roll(room, config.zone) % STRONGHOLD_PROBABILITY_SCALE;
    roll < config.probability_per_10_000
}

fn room_roll(room: RoomId, zone: StrongholdZone) -> u32 {
    let mut value = room.0 ^ ((zone as u32) << 27);
    value ^= value >> 16;
    value = value.wrapping_mul(0x7feb_352d);
    value ^= value >> 15;
    value = value.wrapping_mul(0x846c_a68b);
    value ^ (value >> 16)
}

fn guard_position(origin: Position, index: u32) -> Position {
    const OFFSETS: [(i32, i32); 6] = [(1, 0), (0, 1), (-1, 1), (-1, 0), (0, -1), (1, -1)];
    let (dx, dy) = OFFSETS[index as usize % OFFSETS.len()];
    Position {
        x: origin.x + dx,
        y: origin.y + dy,
        room: origin.room,
    }
}

fn resource_cost(resource: impl Into<ResourceName>, amount: ResourceAmount) -> ResourceCost {
    let mut cost = ResourceCost::new();
    cost.insert(resource.into(), amount);
    cost
}

#[cfg(test)]
mod tests {
    use bevy::prelude::*;

    use super::*;
    use crate::components::{RoomTerrain, TerrainType};
    use std::collections::BTreeMap;

    #[test]
    fn spawn_system_uses_zone_guard_configuration() {
        let mut app = App::new();
        app.init_resource::<SpawnedStrongholdRooms>();
        app.insert_resource(RoomTerrains(BTreeMap::from([(
            RoomId(0),
            RoomTerrain::new(10, 10, TerrainType::Plain),
        )])));
        app.insert_resource(StrongholdSpawnConfig {
            zones: vec![StrongholdZoneConfig {
                zone: StrongholdZone::Core,
                probability_per_10_000: STRONGHOLD_PROBABILITY_SCALE,
                stronghold_type: StrongholdType::EnergySpring,
                guard_count: 3,
                guard_type: StrongholdGuardType::Warden,
            }],
        });
        app.add_systems(Update, stronghold_spawn_system);

        app.update();

        let world = app.world_mut();
        let strongholds = world
            .query::<(&Stronghold, &Position)>()
            .iter(world)
            .collect::<Vec<_>>();
        assert_eq!(strongholds.len(), 1);
        assert_eq!(
            strongholds[0].0.stronghold_type,
            StrongholdType::EnergySpring
        );
        assert_eq!(strongholds[0].0.guard_count, 3);
        assert_eq!(strongholds[0].0.guard_type, StrongholdGuardType::Warden);
        assert_eq!(
            *strongholds[0].1,
            Position {
                x: 5,
                y: 5,
                room: RoomId(0)
            }
        );

        let guards = world
            .query::<&StrongholdGuard>()
            .iter(world)
            .collect::<Vec<_>>();
        assert_eq!(guards.len(), 3);
        assert!(
            guards
                .iter()
                .all(|guard| guard.guard_type == StrongholdGuardType::Warden)
        );
    }

    #[test]
    fn production_system_adds_resources_each_tick() {
        let mut app = App::new();
        app.add_systems(Update, stronghold_production_system);
        app.world_mut().spawn(Stronghold {
            stronghold_type: StrongholdType::AncientRuins,
            production: StrongholdType::AncientRuins.production(),
            stored: ResourceCost::new(),
            guard_count: 0,
            guard_type: StrongholdGuardType::Sentinel,
        });

        app.update();
        app.update();

        let world = app.world_mut();
        let stronghold = world
            .query::<&Stronghold>()
            .single(world)
            .expect("expected one stronghold");
        assert_eq!(stronghold.stored.get("Crystal"), Some(&4));
    }
}
