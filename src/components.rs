use std::collections::BTreeMap;

use bevy::prelude::{Component, Resource as BevyResource};
use indexmap::IndexMap;

pub const DEFAULT_DRONE_LIFESPAN: u32 = 1500;

pub type PlayerId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RoomId(pub u32);

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Position {
    pub x: i32,
    pub y: i32,
    pub room: RoomId,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Owner(pub PlayerId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BodyPart {
    Move,
    Work,
    Carry,
    Attack,
    RangedAttack,
    Heal,
    Claim,
    Tough,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StructureType {
    Spawn,
    Extension,
    Tower,
    Storage,
    Link,
    Extractor,
    Lab,
    Terminal,
    Nuker,
    Observer,
    PowerSpawn,
    Factory,
}

pub const DEFAULT_ROOM_SIZE: i32 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TerrainType {
    Plain,
    Swamp,
    Wall,
}

impl TerrainType {
    pub fn is_passable(self) -> bool {
        !matches!(self, TerrainType::Wall)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomTerrain {
    pub width: i32,
    pub height: i32,
    tiles: Vec<TerrainType>,
}

impl RoomTerrain {
    pub fn new(width: i32, height: i32, fill: TerrainType) -> Self {
        assert!(width > 0 && height > 0, "room dimensions must be positive");
        Self {
            width,
            height,
            tiles: vec![fill; (width * height) as usize],
        }
    }

    pub fn default_room() -> Self {
        Self::new(DEFAULT_ROOM_SIZE, DEFAULT_ROOM_SIZE, TerrainType::Plain)
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && x < self.width && y < self.height
    }

    pub fn get(&self, x: i32, y: i32) -> Option<TerrainType> {
        self.contains(x, y).then(|| self.tiles[self.index(x, y)])
    }

    pub fn set(&mut self, x: i32, y: i32, terrain: TerrainType) -> bool {
        if !self.contains(x, y) {
            return false;
        }
        let index = self.index(x, y);
        self.tiles[index] = terrain;
        true
    }

    pub fn is_passable(&self, x: i32, y: i32) -> bool {
        self.get(x, y).is_some_and(TerrainType::is_passable)
    }

    pub fn iter(&self) -> impl Iterator<Item = (i32, i32, TerrainType)> + '_ {
        self.tiles.iter().enumerate().map(|(index, terrain)| {
            let index = index as i32;
            (index % self.width, index / self.width, *terrain)
        })
    }

    fn index(&self, x: i32, y: i32) -> usize {
        (y * self.width + x) as usize
    }
}

#[derive(BevyResource, Debug, Clone, PartialEq, Eq, Default)]
pub struct RoomTerrains(pub BTreeMap<RoomId, RoomTerrain>);

impl RoomTerrains {
    pub fn get_terrain(&self, position: Position) -> Option<TerrainType> {
        self.0
            .get(&position.room)
            .and_then(|room| room.get(position.x, position.y))
    }

    pub fn set_terrain(&mut self, position: Position, terrain: TerrainType) -> bool {
        self.0
            .get_mut(&position.room)
            .is_some_and(|room| room.set(position.x, position.y, terrain))
    }

    pub fn is_passable(&self, position: Position) -> bool {
        self.0
            .get(&position.room)
            .is_some_and(|room| room.is_passable(position.x, position.y))
    }
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct Drone {
    pub owner: PlayerId,
    pub body: Vec<BodyPart>,
    pub fatigue: u32,
    pub hits: u32,
    pub hits_max: u32,
    pub spawning: bool,
    pub age: u32,
    pub lifespan: u32,
}

impl Drone {
    pub fn new(owner: PlayerId, body: Vec<BodyPart>) -> Self {
        Self {
            owner,
            body,
            fatigue: 0,
            hits: 100,
            hits_max: 100,
            spawning: false,
            age: 0,
            lifespan: DEFAULT_DRONE_LIFESPAN,
        }
    }
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct Structure {
    pub structure_type: StructureType,
    pub owner: Option<PlayerId>,
    pub hits: u32,
    pub hits_max: u32,
    pub energy: Option<u32>,
    pub energy_capacity: Option<u32>,
    pub cooldown: u32,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub amounts: IndexMap<String, u32>,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub produces: IndexMap<String, u32>,
    pub amount: u32,
    pub capacity: u32,
    pub ticks_to_regeneration: u32,
    pub regeneration_time: u32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Terrain(pub TerrainType);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct Controller {
    pub owner: Option<PlayerId>,
    pub level: u8,
    pub progress: u32,
    pub progress_total: u32,
    pub downgrade_timer: u32,
    pub safe_mode: u32,
    pub safe_mode_available: u32,
    pub safe_mode_cooldown: u32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MarkedForDeath;
