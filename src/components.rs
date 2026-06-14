use std::collections::BTreeMap;
use std::fmt;

use bevy::prelude::{Component, Resource as BevyResource};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

pub const DEFAULT_DRONE_LIFESPAN: u32 = 1500;

pub type PlayerId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RoomId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomNameError {
    Empty,
    InvalidFormat,
    CoordinateTooLarge,
}

impl RoomId {
    const COORD_BITS: u32 = 13;
    const COORD_MASK: u32 = (1 << Self::COORD_BITS) - 1;
    const SECTOR_SHIFT: u32 = Self::COORD_BITS * 2;
    const MAX_COORD_MAGNITUDE: i32 = (1 << (Self::COORD_BITS - 1)) - 1;

    pub fn from_room_name(name: &str) -> Result<Self, RoomNameError> {
        let mut chars = name.chars().peekable();
        let sector = chars.next().ok_or(RoomNameError::Empty)?;
        if !sector.is_ascii_uppercase() {
            return Err(RoomNameError::InvalidFormat);
        }

        let vertical = parse_digits(&mut chars)?;
        let ns = chars.next().ok_or(RoomNameError::InvalidFormat)?;
        if !matches!(ns, 'N' | 'S') {
            return Err(RoomNameError::InvalidFormat);
        }

        let horizontal = parse_digits(&mut chars)?;
        let ew = chars.next().ok_or(RoomNameError::InvalidFormat)?;
        if !matches!(ew, 'E' | 'W') || chars.next().is_some() {
            return Err(RoomNameError::InvalidFormat);
        }

        let y = signed_room_coordinate(vertical, ns == 'N')?;
        let x = signed_room_coordinate(horizontal, ew == 'E')?;
        Self::from_sector_coordinates(sector, x, y)
    }

    pub fn from_sector_coordinates(sector: char, x: i32, y: i32) -> Result<Self, RoomNameError> {
        if !sector.is_ascii_uppercase() {
            return Err(RoomNameError::InvalidFormat);
        }
        if x.abs() > Self::MAX_COORD_MAGNITUDE || y.abs() > Self::MAX_COORD_MAGNITUDE {
            return Err(RoomNameError::CoordinateTooLarge);
        }
        let sector_index = (sector as u32) - ('A' as u32);
        Ok(Self(
            (sector_index << Self::SECTOR_SHIFT)
                | (encode_signed(y) << Self::COORD_BITS)
                | encode_signed(x),
        ))
    }

    pub fn sector_coordinates(self) -> (char, i32, i32) {
        let sector_index = self.0 >> Self::SECTOR_SHIFT;
        let sector = char::from_u32(('A' as u32) + sector_index).unwrap_or('A');
        let y = decode_signed((self.0 >> Self::COORD_BITS) & Self::COORD_MASK);
        let x = decode_signed(self.0 & Self::COORD_MASK);
        (sector, x, y)
    }

    pub fn room_name(self) -> String {
        let (sector, x, y) = self.sector_coordinates();
        let ns = if y >= 0 { 'N' } else { 'S' };
        let ew = if x >= 0 { 'E' } else { 'W' };
        format!("{sector}{}{}{}{}", y.abs(), ns, x.abs(), ew)
    }

    pub fn adjacent(self, dx: i32, dy: i32) -> Option<Self> {
        let (sector, x, y) = self.sector_coordinates();
        Self::from_sector_coordinates(sector, x.checked_add(dx)?, y.checked_add(dy)?).ok()
    }

    pub fn is_same_or_adjacent(self, other: Self) -> bool {
        let (sector_a, x_a, y_a) = self.sector_coordinates();
        let (sector_b, x_b, y_b) = other.sector_coordinates();
        sector_a == sector_b && (x_a - x_b).abs() <= 1 && (y_a - y_b).abs() <= 1
    }
}

impl fmt::Display for RoomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.room_name())
    }
}

fn parse_digits<I>(chars: &mut std::iter::Peekable<I>) -> Result<i32, RoomNameError>
where
    I: Iterator<Item = char>,
{
    let mut value = String::new();
    while let Some(next) = chars.peek() {
        if next.is_ascii_digit() {
            value.push(*next);
            chars.next();
        } else {
            break;
        }
    }
    if value.is_empty() {
        return Err(RoomNameError::InvalidFormat);
    }
    value.parse().map_err(|_| RoomNameError::CoordinateTooLarge)
}

fn signed_room_coordinate(magnitude: i32, positive: bool) -> Result<i32, RoomNameError> {
    if magnitude > RoomId::MAX_COORD_MAGNITUDE {
        return Err(RoomNameError::CoordinateTooLarge);
    }
    Ok(if positive { magnitude } else { -magnitude })
}

fn encode_signed(value: i32) -> u32 {
    if value >= 0 {
        (value as u32) << 1
    } else {
        ((-value as u32) << 1) - 1
    }
}

fn decode_signed(value: u32) -> i32 {
    if value & 1 == 0 {
        (value >> 1) as i32
    } else {
        -(((value + 1) >> 1) as i32)
    }
}

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Position {
    pub x: i32,
    pub y: i32,
    pub room: RoomId,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Owner(pub PlayerId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(BevyResource, Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
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

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drone {
    pub owner: PlayerId,
    pub body: Vec<BodyPart>,
    pub carry: IndexMap<String, u32>,
    pub carry_capacity: u32,
    pub fatigue: u32,
    pub hits: u32,
    pub hits_max: u32,
    pub spawning: bool,
    pub age: u32,
    pub lifespan: u32,
}

impl Drone {
    pub fn new(owner: PlayerId, body: Vec<BodyPart>) -> Self {
        let carry_capacity = body
            .iter()
            .filter(|part| matches!(part, BodyPart::Carry))
            .count() as u32
            * 50;
        Self {
            owner,
            body,
            carry: IndexMap::new(),
            carry_capacity,
            fatigue: 0,
            hits: 100,
            hits_max: 100,
            spawning: false,
            age: 0,
            lifespan: DEFAULT_DRONE_LIFESPAN,
        }
    }
}

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Structure {
    pub structure_type: StructureType,
    pub owner: Option<PlayerId>,
    pub hits: u32,
    pub hits_max: u32,
    pub energy: Option<u32>,
    pub energy_capacity: Option<u32>,
    pub cooldown: u32,
}

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resource {
    pub amounts: IndexMap<String, u32>,
}

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub produces: IndexMap<String, u32>,
    pub capacity: u32,
    pub ticks_to_regeneration: u32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Terrain(pub TerrainType);

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
