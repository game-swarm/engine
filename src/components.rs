use std::collections::{BTreeMap, HashMap};
use std::fmt;

use bevy::prelude::{Component, Resource as BevyResource};
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const DEFAULT_DRONE_LIFESPAN: u32 = 1500;

pub type PlayerId = u32;

pub const DEFAULT_TICK_INTERVAL_MS: u64 = 3_000;
pub const TUTORIAL_TICK_INTERVAL_MS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorldMode {
    Default,
    Tutorial,
    Arena,
}

#[derive(BevyResource, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldSettings {
    pub mode: WorldMode,
    pub tick_interval_ms: u64,
    pub namespace: String,
}

impl WorldSettings {
    pub fn new(mode: WorldMode, namespace: String) -> Self {
        Self {
            mode,
            tick_interval_ms: match mode {
                WorldMode::Tutorial => TUTORIAL_TICK_INTERVAL_MS,
                WorldMode::Default | WorldMode::Arena => DEFAULT_TICK_INTERVAL_MS,
            },
            namespace,
        }
    }
}

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

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

impl fmt::Display for BodyPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Move => "Move",
            Self::Work => "Work",
            Self::Carry => "Carry",
            Self::Attack => "Attack",
            Self::RangedAttack => "RangedAttack",
            Self::Heal => "Heal",
            Self::Claim => "Claim",
            Self::Tough => "Tough",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DamageType {
    Kinetic,
    Thermal,
    EMP,
    Sonic,
    Corrosive,
    Psionic,
}
impl DamageType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kinetic => "Kinetic",
            Self::Thermal => "Thermal",
            Self::EMP => "EMP",
            Self::Sonic => "Sonic",
            Self::Corrosive => "Corrosive",
            Self::Psionic => "Psionic",
        }
    }
}
impl fmt::Display for DamageType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DamageTypeDef {
    pub name: String,
    pub component_multipliers: IndexMap<String, f64>,
    pub attribute_multipliers: IndexMap<String, f64>,
}
impl Default for DamageTypeDef {
    fn default() -> Self {
        Self {
            name: DamageType::Kinetic.to_string(),
            component_multipliers: IndexMap::new(),
            attribute_multipliers: IndexMap::new(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BodyPartTypeDef {
    pub name: BodyPart,
    pub damage_type: Option<String>,
    pub base_damage: Option<u32>,
    pub heal_amount: Option<u32>,
    pub resistances: IndexMap<String, f64>,
    pub age_modifier: i32,
}
impl Default for BodyPartTypeDef {
    fn default() -> Self {
        Self {
            name: BodyPart::Move,
            damage_type: None,
            base_damage: None,
            heal_amount: None,
            resistances: IndexMap::new(),
            age_modifier: 0,
        }
    }
}
#[derive(BevyResource, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BodyPartRegistry {
    pub parts: IndexMap<BodyPart, BodyPartTypeDef>,
}
impl Default for BodyPartRegistry {
    fn default() -> Self {
        let mut parts = IndexMap::new();
        for part in [
            BodyPart::Move,
            BodyPart::Work,
            BodyPart::Carry,
            BodyPart::Attack,
            BodyPart::RangedAttack,
            BodyPart::Heal,
            BodyPart::Claim,
            BodyPart::Tough,
        ] {
            parts.insert(
                part,
                BodyPartTypeDef {
                    name: part,
                    ..Default::default()
                },
            );
        }
        parts.get_mut(&BodyPart::Attack).unwrap().damage_type =
            Some(DamageType::Kinetic.to_string());
        parts.get_mut(&BodyPart::Attack).unwrap().base_damage = Some(30);
        parts.get_mut(&BodyPart::RangedAttack).unwrap().damage_type =
            Some(DamageType::Kinetic.to_string());
        parts.get_mut(&BodyPart::RangedAttack).unwrap().base_damage = Some(25);
        parts.get_mut(&BodyPart::Heal).unwrap().heal_amount = Some(12);
        // age_modifier: Tough lives longer, combat/utility parts reduce lifespan
        parts.get_mut(&BodyPart::Tough).unwrap().age_modifier = 100;
        parts.get_mut(&BodyPart::Attack).unwrap().age_modifier = -80;
        parts.get_mut(&BodyPart::RangedAttack).unwrap().age_modifier = -50;
        parts.get_mut(&BodyPart::Heal).unwrap().age_modifier = -30;
        parts.get_mut(&BodyPart::Claim).unwrap().age_modifier = -50;
        parts
            .get_mut(&BodyPart::Tough)
            .unwrap()
            .resistances
            .insert(DamageType::Kinetic.to_string(), 0.5);
        Self { parts }
    }
}
impl BodyPartRegistry {
    pub fn from_defs(defs: Vec<BodyPartTypeDef>) -> Self {
        let mut r = Self::default();
        for d in defs {
            r.parts.insert(d.name, d);
        }
        r
    }
    pub fn damage_type(&self, part: BodyPart) -> String {
        self.parts
            .get(&part)
            .and_then(|d| d.damage_type.clone())
            .unwrap_or_else(|| DamageType::Kinetic.to_string())
    }
    pub fn base_damage(&self, part: BodyPart) -> u32 {
        self.parts
            .get(&part)
            .and_then(|d| d.base_damage)
            .unwrap_or(0)
    }
    pub fn heal_amount(&self, part: BodyPart) -> u32 {
        self.parts
            .get(&part)
            .and_then(|d| d.heal_amount)
            .unwrap_or(0)
    }
    pub fn resistance(&self, part: BodyPart, dt: &str) -> f64 {
        self.parts
            .get(&part)
            .and_then(|d| d.resistances.get(dt).copied())
            .unwrap_or(0.0)
            .clamp(0.0, 1.0)
    }
}
#[derive(BevyResource, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DamageTypeRegistry {
    pub damage_types: IndexMap<String, DamageTypeDef>,
}
impl Default for DamageTypeRegistry {
    fn default() -> Self {
        let mut damage_types = IndexMap::new();
        for dt in [
            DamageType::Kinetic,
            DamageType::Thermal,
            DamageType::EMP,
            DamageType::Sonic,
            DamageType::Corrosive,
            DamageType::Psionic,
        ] {
            damage_types.insert(
                dt.to_string(),
                DamageTypeDef {
                    name: dt.to_string(),
                    ..Default::default()
                },
            );
        }
        damage_types
            .get_mut(DamageType::Kinetic.as_str())
            .unwrap()
            .attribute_multipliers
            .insert("Shielded".to_string(), 0.7);
        Self { damage_types }
    }
}
impl DamageTypeRegistry {
    pub fn from_defs(defs: Vec<DamageTypeDef>) -> Self {
        let mut r = Self::default();
        for d in defs {
            r.damage_types.insert(d.name.clone(), d);
        }
        r
    }
    pub fn component_multiplier(&self, dt: &str, body: Option<&[BodyPart]>) -> f64 {
        let Some(def) = self.damage_types.get(dt) else {
            return 1.0;
        };
        body.unwrap_or(&[])
            .iter()
            .filter_map(|part| def.component_multipliers.get(&part.to_string()))
            .fold(1.0, |acc, multiplier| {
                acc * damage_type_multiplier(*multiplier)
            })
    }
    pub fn attribute_multiplier(&self, dt: &str, attrs: Option<&Attributes>) -> f64 {
        let Some(attrs) = attrs else {
            return 1.0;
        };
        let Some(def) = self.damage_types.get(dt) else {
            return 1.0;
        };
        attrs
            .0
            .iter()
            .filter_map(|a| def.attribute_multipliers.get(a))
            .fold(1.0, |acc, multiplier| {
                acc * damage_type_multiplier(*multiplier)
            })
    }
}

fn damage_type_multiplier(multiplier: f64) -> f64 {
    if multiplier.is_finite() {
        multiplier.max(0.0)
    } else {
        1.0
    }
}

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Attributes(pub Vec<String>);

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EntityFlags(pub HashMap<String, bool>);

#[derive(BevyResource, Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ResistanceRegistry {
    pub damage_types: IndexMap<String, ResistanceDamageTypeDef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ResistanceDamageTypeDef {
    pub component_multipliers: IndexMap<String, f64>,
    pub attribute_multipliers: IndexMap<String, f64>,
}

impl ResistanceRegistry {
    pub fn from_registries(
        body_registry: &BodyPartRegistry,
        damage_registry: &DamageTypeRegistry,
    ) -> Self {
        let mut registry = Self::default();
        for damage_type in damage_registry.damage_types.keys() {
            registry.add_damage_type(damage_type);
        }
        for damage_type in damage_registry.damage_types.keys() {
            registry.add_damage_type(damage_type);
        }
        for (part, def) in &body_registry.parts {
            for (damage_type, resistance) in &def.resistances {
                registry.set_component_multiplier(
                    damage_type,
                    &part.to_string(),
                    1.0 - resistance.clamp(0.0, 1.0),
                );
            }
        }
        registry
    }

    pub fn add_damage_type(&mut self, damage_type: impl Into<String>) {
        self.damage_types
            .entry(damage_type.into())
            .or_insert_with(ResistanceDamageTypeDef::default);
    }

    pub fn set_resistance(&mut self, damage_type: &str, layer: &str, key: &str, multiplier: f64) {
        match layer {
            "component" | "components" | "body" | "body_part" => {
                self.set_component_multiplier(damage_type, key, multiplier);
            }
            "attribute" | "attributes" => {
                self.set_attribute_multiplier(damage_type, key, multiplier);
            }
            _ => {}
        }
    }

    pub fn set_component_multiplier(
        &mut self,
        damage_type: &str,
        component: &str,
        multiplier: f64,
    ) {
        self.add_damage_type(damage_type);
        if let Some(def) = self.damage_types.get_mut(damage_type) {
            def.component_multipliers
                .insert(component.to_string(), clamp_multiplier(multiplier));
        }
    }

    pub fn set_attribute_multiplier(
        &mut self,
        damage_type: &str,
        attribute: &str,
        multiplier: f64,
    ) {
        self.add_damage_type(damage_type);
        if let Some(def) = self.damage_types.get_mut(damage_type) {
            def.attribute_multipliers
                .insert(attribute.to_string(), clamp_multiplier(multiplier));
        }
    }

    pub fn component_multiplier(&self, damage_type: &str, body: Option<&[BodyPart]>) -> f64 {
        let Some(def) = self.damage_types.get(damage_type) else {
            return 1.0;
        };
        body.unwrap_or(&[])
            .iter()
            .filter_map(|part| def.component_multipliers.get(&part.to_string()))
            .fold(1.0, |acc, multiplier| acc * multiplier.clamp(0.0, 1.0))
    }

    pub fn attribute_multiplier(&self, damage_type: &str, attrs: Option<&Attributes>) -> f64 {
        let Some(attrs) = attrs else {
            return 1.0;
        };
        let Some(def) = self.damage_types.get(damage_type) else {
            return 1.0;
        };
        attrs
            .0
            .iter()
            .filter_map(|attribute| def.attribute_multipliers.get(attribute))
            .fold(1.0, |acc, multiplier| acc * multiplier.clamp(0.0, 1.0))
    }
}

fn clamp_multiplier(multiplier: f64) -> f64 {
    if multiplier.is_finite() {
        multiplier.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StructureType(pub &'static str);

impl StructureType {
    pub const SPAWN: Self = Self("Spawn");
    pub const EXTENSION: Self = Self("Extension");
    pub const TOWER: Self = Self("Tower");
    pub const STORAGE: Self = Self("Storage");
    pub const LINK: Self = Self("Link");
    pub const EXTRACTOR: Self = Self("Extractor");
    pub const LAB: Self = Self("Lab");
    pub const TERMINAL: Self = Self("Terminal");
    pub const NUKER: Self = Self("Nuker");
    pub const OBSERVER: Self = Self("Observer");
    pub const POWER_SPAWN: Self = Self("PowerSpawn");
    pub const FACTORY: Self = Self("Factory");
    pub const DEPOT: Self = Self("Depot");

    #[allow(non_upper_case_globals)]
    pub const Spawn: Self = Self::SPAWN;
    #[allow(non_upper_case_globals)]
    pub const Extension: Self = Self::EXTENSION;
    #[allow(non_upper_case_globals)]
    pub const Tower: Self = Self::TOWER;
    #[allow(non_upper_case_globals)]
    pub const Storage: Self = Self::STORAGE;
    #[allow(non_upper_case_globals)]
    pub const Link: Self = Self::LINK;
    #[allow(non_upper_case_globals)]
    pub const Extractor: Self = Self::EXTRACTOR;
    #[allow(non_upper_case_globals)]
    pub const Lab: Self = Self::LAB;
    #[allow(non_upper_case_globals)]
    pub const Terminal: Self = Self::TERMINAL;
    #[allow(non_upper_case_globals)]
    pub const Nuker: Self = Self::NUKER;
    #[allow(non_upper_case_globals)]
    pub const Observer: Self = Self::OBSERVER;
    #[allow(non_upper_case_globals)]
    pub const PowerSpawn: Self = Self::POWER_SPAWN;
    #[allow(non_upper_case_globals)]
    pub const Factory: Self = Self::FACTORY;
    #[allow(non_upper_case_globals)]
    pub const Depot: Self = Self::DEPOT;

    pub fn new(name: impl Into<String>) -> Self {
        Self(Box::leak(name.into().into_boxed_str()))
    }

    pub fn as_str(self) -> &'static str {
        self.0
    }
}

impl Default for StructureType {
    fn default() -> Self {
        Self::SPAWN
    }
}

impl fmt::Display for StructureType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl Serialize for StructureType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.0)
    }
}

impl<'de> Deserialize<'de> for StructureType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::new)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StructureAttackDef {
    pub damage: u32,
    pub damage_type: String,
    pub range: u32,
    pub cooldown: u32,
}

impl Default for StructureAttackDef {
    fn default() -> Self {
        Self {
            damage: 0,
            damage_type: DamageType::Kinetic.to_string(),
            range: 0,
            cooldown: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StructureTypeDef {
    pub name: StructureType,
    pub description: String,
    pub category: String,
    pub hits: u32,
    pub rcl_required: u8,
    pub max_per_room: Option<u32>,
    pub capacity: Option<u32>,
    pub attack: Option<StructureAttackDef>,
    pub sight_range: Option<u32>,
    pub cost: IndexMap<String, u32>,
    pub repair_capacity: Option<u32>,
    pub repair_range: Option<u32>,
    pub repair_aging: Option<u32>,
    pub maintenance: IndexMap<String, u32>,
}

impl Default for StructureTypeDef {
    fn default() -> Self {
        Self {
            name: StructureType::SPAWN,
            description: String::new(),
            category: "core".to_string(),
            hits: 1,
            rcl_required: 1,
            max_per_room: None,
            capacity: None,
            attack: None,
            sight_range: None,
            cost: IndexMap::new(),
            repair_capacity: None,
            repair_range: None,
            repair_aging: None,
            maintenance: IndexMap::new(),
        }
    }
}

#[derive(BevyResource, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructureTypeRegistry {
    pub structure_types: IndexMap<StructureType, StructureTypeDef>,
}

impl Default for StructureTypeRegistry {
    fn default() -> Self {
        let mut registry = Self {
            structure_types: IndexMap::new(),
        };
        registry.insert(
            "Spawn",
            "Spawn point for creating drones",
            "core",
            5000,
            1,
            None,
            Some(300),
            None,
            None,
            200,
        );
        registry.insert(
            "Extension",
            "Energy extension",
            "storage",
            1000,
            2,
            Some(60),
            Some(50),
            None,
            None,
            50,
        );
        registry.insert(
            "Tower",
            "Defensive tower",
            "defense",
            3000,
            3,
            None,
            Some(1000),
            Some(StructureAttackDef {
                damage: 50,
                damage_type: DamageType::Kinetic.to_string(),
                range: 5,
                cooldown: 10,
            }),
            None,
            200,
        );
        registry.insert(
            "Storage",
            "Large local resource storage",
            "storage",
            10000,
            3,
            None,
            Some(1_000_000),
            None,
            None,
            500,
        );
        registry.insert(
            "Link",
            "Short range energy link",
            "logistics",
            1000,
            4,
            None,
            None,
            None,
            None,
            300,
        );
        registry.insert(
            "Extractor",
            "Mineral extractor",
            "production",
            5000,
            6,
            None,
            None,
            None,
            None,
            800,
        );
        registry.insert(
            "Lab",
            "Resource reaction lab",
            "production",
            5000,
            6,
            None,
            None,
            None,
            None,
            1000,
        );
        registry.insert(
            "Terminal",
            "Market terminal",
            "logistics",
            3000,
            5,
            None,
            None,
            None,
            None,
            500,
        );
        registry.insert(
            "Observer",
            "Remote observer",
            "intel",
            500,
            5,
            None,
            None,
            None,
            Some(10),
            300,
        );
        registry.insert(
            "PowerSpawn",
            "Advanced spawn",
            "core",
            5000,
            7,
            None,
            None,
            None,
            None,
            5000,
        );
        registry.insert(
            "Factory",
            "Commodity factory",
            "production",
            5000,
            6,
            None,
            None,
            None,
            None,
            1500,
        );
        registry.insert(
            "Nuker",
            "Nuclear launcher",
            "defense",
            10000,
            8,
            None,
            None,
            None,
            None,
            100000,
        );
        // Depot: forward maintenance node
        {
            let structure_type = StructureType::DEPOT;
            let mut cost = IndexMap::new();
            cost.insert("Energy".to_string(), 700);
            let mut maintenance = IndexMap::new();
            maintenance.insert("Energy".to_string(), 1);
            registry.structure_types.insert(
                structure_type,
                StructureTypeDef {
                    name: structure_type,
                    description:
                        "Forward maintenance depot — consumes resources to reduce drone age"
                            .to_string(),
                    category: "logistics".to_string(),
                    hits: 3000,
                    rcl_required: 4,
                    max_per_room: None,
                    capacity: Some(500),
                    attack: None,
                    sight_range: None,
                    cost,
                    repair_capacity: Some(10),
                    repair_range: Some(3),
                    repair_aging: Some(2),
                    maintenance,
                },
            );
        }
        registry
    }
}

impl StructureTypeRegistry {
    #[allow(clippy::too_many_arguments)]
    fn insert(
        &mut self,
        name: &'static str,
        description: &str,
        category: &str,
        hits: u32,
        rcl_required: u8,
        max_per_room: Option<u32>,
        capacity: Option<u32>,
        attack: Option<StructureAttackDef>,
        sight_range: Option<u32>,
        energy_cost: u32,
    ) {
        let structure_type = StructureType(name);
        let mut cost = IndexMap::new();
        cost.insert("Energy".to_string(), energy_cost);
        self.structure_types.insert(
            structure_type,
            StructureTypeDef {
                name: structure_type,
                description: description.to_string(),
                category: category.to_string(),
                hits,
                rcl_required,
                max_per_room,
                capacity,
                attack,
                sight_range,
                cost,
                repair_capacity: None,
                repair_range: None,
                repair_aging: None,
                maintenance: IndexMap::new(),
            },
        );
    }

    pub fn from_defs(defs: Vec<StructureTypeDef>) -> Self {
        let mut registry = Self::default();
        for def in defs {
            registry.structure_types.insert(def.name, def);
        }
        registry
    }

    pub fn get(&self, structure_type: StructureType) -> Option<&StructureTypeDef> {
        self.structure_types.get(&structure_type)
    }

    pub fn contains(&self, structure_type: StructureType) -> bool {
        self.structure_types.contains_key(&structure_type)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpecialEffectDef {
    pub name: String,
    pub description: String,
    pub handler: String,
    pub target: String,
    pub duration: u32,
    pub resistance: Option<String>,
}

impl Default for SpecialEffectDef {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            handler: String::new(),
            target: String::new(),
            duration: 0,
            resistance: None,
        }
    }
}

#[derive(BevyResource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecialEffectRegistry {
    pub effects: IndexMap<String, SpecialEffectDef>,
}

impl SpecialEffectRegistry {
    pub fn from_defs(defs: Vec<SpecialEffectDef>) -> Self {
        let mut effects = IndexMap::new();
        for mut def in defs {
            if def.handler.is_empty() {
                def.handler = def.name.clone();
            }
            effects.insert(def.name.clone(), def);
        }
        Self { effects }
    }

    pub fn get(&self, name: &str) -> Option<&SpecialEffectDef> {
        self.effects.get(name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomActionDef {
    pub name: String,
    pub description: String,
    pub damage_type: Option<String>,
    pub base_damage: Option<u32>,
    pub range: u32,
    pub special_effect: Option<String>,
    pub special_param: Option<f64>,
    pub cooldown: Option<u32>,
    pub cost: IndexMap<String, u32>,
}

impl Default for CustomActionDef {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            damage_type: None,
            base_damage: None,
            range: 1,
            special_effect: None,
            special_param: None,
            cooldown: None,
            cost: IndexMap::new(),
        }
    }
}

#[derive(BevyResource, Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CustomActionRegistry {
    pub actions: IndexMap<String, CustomActionDef>,
}

impl CustomActionRegistry {
    pub fn from_defs(defs: Vec<CustomActionDef>) -> Self {
        let mut actions = IndexMap::new();
        for def in defs {
            actions.insert(def.name.clone(), def);
        }
        Self { actions }
    }

    pub fn get(&self, name: &str) -> Option<&CustomActionDef> {
        self.actions.get(name)
    }
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
    pub aging_remainder: u8,
    pub lifespan: u32,
    pub executed_command_this_tick: bool,
}

impl Drone {
    pub fn new(owner: PlayerId, body: Vec<BodyPart>, registry: &BodyPartRegistry) -> Self {
        Self::new_with_lifespan(owner, body, registry, DEFAULT_DRONE_LIFESPAN)
    }

    pub fn new_with_lifespan(
        owner: PlayerId,
        body: Vec<BodyPart>,
        registry: &BodyPartRegistry,
        base_lifespan: u32,
    ) -> Self {
        let carry_capacity = body
            .iter()
            .filter(|part| matches!(part, BodyPart::Carry))
            .count() as u32
            * 50;
        let lifespan_mod: i32 = body
            .iter()
            .filter_map(|part| registry.parts.get(part))
            .map(|def| def.age_modifier)
            .sum();
        let lifespan = base_lifespan.saturating_add_signed(lifespan_mod);
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
            aging_remainder: 0,
            lifespan,
            executed_command_this_tick: false,
        }
    }
}

/// Per-drone environment variables accessible from WASM modules.
/// Managed by drone_env_var_system according to DroneConfig.env_vars.
#[derive(Component, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroneEnv {
    pub vars: indexmap::IndexMap<String, String>,
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
    pub repair_capacity: u32,
    pub repair_range: u32,
    pub repair_per_drone: u32,
}

/// Tracks the deployed code version for a drone. Updated by
/// code_propagation_system when a new code version propagates to this drone.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct CodeVersion(pub u64);

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MarkedForDeath;

/// Shared resource tracking per-player age repair totals across Controller and Depot systems.
/// Combined repair cannot exceed 50% of natural growth per tick per drone.
/// Per-player latest deployed code version. Set by MCP deploy; read by
/// code_propagation_system to determine which drones are outdated.
#[derive(BevyResource, Debug, Clone, Default)]
pub struct LatestCodeVersions(pub indexmap::IndexMap<PlayerId, u64>);

#[derive(BevyResource, Debug, Clone, Default)]
pub struct RepairTracker {
    pub per_player: IndexMap<PlayerId, u32>,
    pub hard_cap: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drone_lifespan_includes_age_modifiers() {
        let registry = BodyPartRegistry::default();
        // Tough (+100) + Attack (-80) → net +20
        let drone = Drone::new(1, vec![BodyPart::Tough, BodyPart::Attack], &registry);
        assert_eq!(drone.lifespan, 1520); // 1500 + 100 - 80

        // Heal (-30) + Claim (-50) → net -80
        let drone = Drone::new(2, vec![BodyPart::Heal, BodyPart::Claim], &registry);
        assert_eq!(drone.lifespan, 1420); // 1500 - 30 - 50

        // Move + Work + Carry → net 0
        let drone = Drone::new(
            3,
            vec![BodyPart::Move, BodyPart::Work, BodyPart::Carry],
            &registry,
        );
        assert_eq!(drone.lifespan, 1500);

        // Multiple Tough (+100 each)
        let drone = Drone::new(4, vec![BodyPart::Tough, BodyPart::Tough], &registry);
        assert_eq!(drone.lifespan, 1700); // 1500 + 100 + 100
    }

    #[test]
    fn drone_lifespan_uses_configurable_base() {
        let registry = BodyPartRegistry::default();
        let drone = Drone::new_with_lifespan(1, vec![BodyPart::Tough], &registry, 2_000);

        assert_eq!(drone.lifespan, 2_100);
    }
}
