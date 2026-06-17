use serde::{Deserialize, Serialize};

use crate::components::RoomId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NpcType {
    Creep,
    Guardian,
    Merchant,
    Swarmling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DifficultyZone {
    Zone1,
    Zone2,
    Zone3,
    Zone4,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZoneDefinition {
    pub zone: DifficultyZone,
    pub max_distance: u32,
    pub npc_spawn_rate: f64,
    pub hp_multiplier: f64,
    pub drop_bonus: f64,
    pub npc_types: Vec<NpcType>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorldPveConfig {
    pub center_x: i32,
    pub center_y: i32,
    pub zones: Vec<ZoneDefinition>,
}

impl Default for DifficultyZone {
    fn default() -> Self {
        Self::Zone1
    }
}

impl Default for ZoneDefinition {
    fn default() -> Self {
        Self {
            zone: DifficultyZone::Zone1,
            max_distance: 10,
            npc_spawn_rate: 1.0,
            hp_multiplier: 1.0,
            drop_bonus: 0.0,
            npc_types: vec![NpcType::Creep],
        }
    }
}

impl Default for WorldPveConfig {
    fn default() -> Self {
        Self {
            center_x: 0,
            center_y: 0,
            zones: vec![
                ZoneDefinition {
                    zone: DifficultyZone::Zone1,
                    max_distance: 10,
                    npc_spawn_rate: 1.0,
                    hp_multiplier: 1.0,
                    drop_bonus: 0.0,
                    npc_types: vec![NpcType::Creep],
                },
                ZoneDefinition {
                    zone: DifficultyZone::Zone2,
                    max_distance: 25,
                    npc_spawn_rate: 2.5,
                    hp_multiplier: 1.5,
                    drop_bonus: 0.25,
                    npc_types: vec![NpcType::Creep, NpcType::Guardian],
                },
                ZoneDefinition {
                    zone: DifficultyZone::Zone3,
                    max_distance: 50,
                    npc_spawn_rate: 4.0,
                    hp_multiplier: 2.0,
                    drop_bonus: 0.5,
                    npc_types: vec![NpcType::Creep, NpcType::Guardian],
                },
                ZoneDefinition {
                    zone: DifficultyZone::Zone4,
                    max_distance: u32::MAX,
                    npc_spawn_rate: 6.0,
                    hp_multiplier: 3.0,
                    drop_bonus: 1.0,
                    npc_types: vec![NpcType::Creep, NpcType::Guardian, NpcType::Swarmling],
                },
            ],
        }
    }
}

pub fn room_distance_from_world_center(room: RoomId, config: &WorldPveConfig) -> u32 {
    let (_, x, y) = room.sector_coordinates();
    (x - config.center_x)
        .unsigned_abs()
        .max((y - config.center_y).unsigned_abs())
}

pub fn zone_for_room(room: RoomId, config: &WorldPveConfig) -> DifficultyZone {
    zone_definition_for_room(room, config).zone
}

pub fn zone_definition_for_room<'a>(
    room: RoomId,
    config: &'a WorldPveConfig,
) -> &'a ZoneDefinition {
    let distance = room_distance_from_world_center(room, config);
    config
        .zones
        .iter()
        .filter(|zone| distance <= zone.max_distance)
        .min_by_key(|zone| zone.max_distance)
        .or_else(|| config.zones.iter().max_by_key(|zone| zone.max_distance))
        .expect("pve zone config must contain at least one zone")
}

pub fn zone_definition(zone: DifficultyZone, config: &WorldPveConfig) -> Option<&ZoneDefinition> {
    config
        .zones
        .iter()
        .find(|definition| definition.zone == zone)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn determines_zone_by_room_distance_from_center() {
        let config = WorldPveConfig::default();

        assert_eq!(
            zone_for_room(RoomId::from_room_name("A0N0E").unwrap(), &config),
            DifficultyZone::Zone1
        );
        assert_eq!(
            zone_for_room(RoomId::from_room_name("A0N11E").unwrap(), &config),
            DifficultyZone::Zone2
        );
        assert_eq!(
            zone_for_room(RoomId::from_room_name("A26N0E").unwrap(), &config),
            DifficultyZone::Zone3
        );
        assert_eq!(
            zone_for_room(RoomId::from_room_name("A51S0E").unwrap(), &config),
            DifficultyZone::Zone4
        );
    }

    #[test]
    fn parses_zone_multipliers_from_config() {
        let config: WorldPveConfig = toml::from_str(
            r#"
center_x = 5
center_y = -5

[[zones]]
zone = "Zone1"
max_distance = 3
npc_spawn_rate = 0.5
hp_multiplier = 1.25
drop_bonus = 0.1
npc_types = ["Creep", "Merchant"]

[[zones]]
zone = "Zone4"
max_distance = 4294967295
npc_spawn_rate = 5.0
hp_multiplier = 4.0
drop_bonus = 2.0
npc_types = ["Guardian", "Swarmling"]
"#,
        )
        .unwrap();

        let zone = zone_definition_for_room(RoomId::from_room_name("A5S8E").unwrap(), &config);
        assert_eq!(zone.zone, DifficultyZone::Zone1);
        assert_eq!(zone.npc_spawn_rate, 0.5);
        assert_eq!(zone.hp_multiplier, 1.25);
        assert_eq!(zone.drop_bonus, 0.1);
        assert_eq!(zone.npc_types, vec![NpcType::Creep, NpcType::Merchant]);
    }
}
