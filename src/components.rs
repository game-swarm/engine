use std::collections::{BTreeMap, HashMap};
use std::fmt;

use bevy::prelude::{Component, Resource as BevyResource};
use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ts_rs::TS;

pub const DEFAULT_DRONE_LIFESPAN: u32 = 1500;
pub const MIN_LIFESPAN: u32 = 100;

pub const VANILLA_ACTION_NAMES: &[&str] = &[
    "Attack",
    "RangedAttack",
    "Heal",
    "Hack",
    "Drain",
    "Overload",
    "Debilitate",
    "Disrupt",
    "Fortify",
    "Leech",
    "Fabricate",
];

pub type PlayerId = u32;

pub const DEFAULT_TICK_INTERVAL_MS: u64 = 3_000;
pub const TUTORIAL_TICK_INTERVAL_MS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
pub enum WorldMode {
    Default,
    Tutorial,
    Novice,
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
                WorldMode::Default | WorldMode::Novice | WorldMode::Arena => {
                    DEFAULT_TICK_INTERVAL_MS
                }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
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
    pub weight: u32,
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
            weight: default_body_part_weight(BodyPart::Move),
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
                    weight: default_body_part_weight(part),
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
fn default_body_part_weight(part: BodyPart) -> u32 {
    match part {
        BodyPart::Move => 50,
        BodyPart::Work => 20,
        BodyPart::Carry => 50,
        BodyPart::Attack => 20,
        BodyPart::RangedAttack => 50,
        BodyPart::Heal => 250,
        BodyPart::Claim => 100,
        BodyPart::Tough => 10,
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
    pub fn weight(&self, part: BodyPart) -> u32 {
        self.parts
            .get(&part)
            .map(|d| d.weight)
            .unwrap_or_else(|| default_body_part_weight(part))
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
        self.damage_types.entry(damage_type.into()).or_default();
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpecialEffectDef {
    pub name: String,
    pub description: String,
    pub handler: String,
    pub target: String,
    pub duration: u32,
    pub resistance: Option<String>,
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
        let mut actions = vanilla_action_defs()
            .into_iter()
            .map(|def| (def.name.clone(), def))
            .collect::<IndexMap<_, _>>();
        for def in defs {
            if is_vanilla_action_name(&def.name) {
                continue;
            }
            actions.insert(def.name.clone(), def);
        }
        Self { actions }
    }

    pub fn get(&self, name: &str) -> Option<&CustomActionDef> {
        self.actions.get(name)
    }
}

pub fn is_vanilla_action_name(name: &str) -> bool {
    VANILLA_ACTION_NAMES.contains(&name)
}

fn custom_action_def(
    name: &str,
    description: &str,
    range: u32,
    special_effect: Option<&str>,
    cooldown: Option<u32>,
    cost: &[(&str, u32)],
) -> CustomActionDef {
    CustomActionDef {
        name: name.to_string(),
        description: description.to_string(),
        range,
        special_effect: special_effect.map(str::to_string),
        cooldown,
        cost: cost
            .iter()
            .map(|(resource, amount)| ((*resource).to_string(), *amount))
            .collect(),
        ..Default::default()
    }
}

pub fn vanilla_action_defs() -> Vec<CustomActionDef> {
    let attack = custom_action_def("Attack", "Melee attack target", 1, None, None, &[]);
    let ranged_attack =
        custom_action_def("RangedAttack", "Ranged attack target", 3, None, None, &[]);
    let heal = custom_action_def("Heal", "Repair or heal target", 1, None, None, &[]);

    let mut hack = custom_action_def(
        "Hack",
        "5-stage intrusion attack",
        1,
        Some("hack"),
        Some(200),
        &[("Energy", 1000)],
    );
    hack.damage_type = Some("Psionic".to_string());

    let mut drain = custom_action_def(
        "Drain",
        "Continuously drain resources from target",
        1,
        Some("drain"),
        Some(50),
        &[("Energy", 200)],
    );
    drain.damage_type = Some("EMP".to_string());

    let mut overload = custom_action_def(
        "Overload",
        "Reduce target player fuel budget",
        5,
        Some("overload"),
        Some(200),
        &[("Energy", 300)],
    );
    overload.damage_type = Some("EMP".to_string());
    overload.special_param = Some(500_000.0);

    let mut debilitate = custom_action_def(
        "Debilitate",
        "Apply vulnerability to a target damage type for 50 ticks",
        3,
        Some("debilitate"),
        Some(150),
        &[("Energy", 200)],
    );
    debilitate.damage_type = Some("Corrosive".to_string());
    debilitate.special_param = Some(2.0);

    let mut disrupt = custom_action_def(
        "Disrupt",
        "Interrupt target current continuous action",
        1,
        Some("disrupt"),
        Some(50),
        &[("Energy", 100)],
    );
    disrupt.damage_type = Some("Sonic".to_string());

    let mut fortify = custom_action_def(
        "Fortify",
        "Shield and cleanse self or an ally",
        1,
        Some("fortify"),
        Some(300),
        &[("Energy", 400)],
    );
    fortify.special_param = Some(0.5);

    let mut leech = custom_action_def(
        "Leech",
        "Kinetic attack that heals the attacker for 50% of dealt damage",
        1,
        Some("leech"),
        Some(100),
        &[("Energy", 300)],
    );
    leech.damage_type = Some("Kinetic".to_string());
    leech.base_damage = Some(15);
    leech.special_param = Some(0.5);

    let fabricate = custom_action_def(
        "Fabricate",
        "Convert enemy drone into an owned structure",
        1,
        Some("fabricate"),
        Some(500),
        &[("Energy", 5000)],
    );

    vec![
        attack,
        ranged_attack,
        heal,
        hack,
        drain,
        overload,
        debilitate,
        disrupt,
        fortify,
        leech,
        fabricate,
    ]
}

/// Phase 2b StatusState components — each special attack gets its own
/// Bevy component. S22 `status_advance_system` is the UNIQUE WRITER for
/// all StatusState components (R22 B3). S16-S21 are READERS that produce
/// per-tick effects based on current state.

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HackState {
    /// 0=just applied, 1=slow, 2=root, 3=neutralized
    pub stage: u32,
    pub remaining_ticks: u32,
}

impl Default for HackState {
    fn default() -> Self {
        Self {
            stage: 0,
            remaining_ticks: 5,
        }
    }
}

#[derive(Component, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainState {
    /// Resource being drained
    pub resource: String,
    pub amount_per_tick: u32,
    pub remaining_ticks: u32,
}

#[derive(Component, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverloadState {
    pub fuel_drain_per_tick: u32,
    pub fuel_floor: u32,
    pub remaining_ticks: u32,
}

#[derive(Component, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebilitateState {
    pub damage_type: String,
    pub remaining_ticks: u32,
}

#[derive(Component, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisruptState {
    /// Body parts being disrupted
    pub body_parts: Vec<BodyPart>,
    pub remaining_ticks: u32,
}

#[derive(Component, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FortifyState {
    pub remaining_ticks: u32,
}

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeechState {
    pub resource: String,
    pub amount_per_tick: u32,
    pub age_acceleration: u32,
    pub remaining_ticks: u32,
}

impl Default for LeechState {
    fn default() -> Self {
        Self {
            resource: "Energy".to_string(),
            amount_per_tick: 0,
            age_acceleration: 1,
            remaining_ticks: 0,
        }
    }
}

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FabricateState {
    pub structure_type: StructureType,
    pub remaining_ticks: u32,
}

impl Default for FabricateState {
    fn default() -> Self {
        Self {
            structure_type: StructureType::FACTORY,
            remaining_ticks: 0,
        }
    }
}

#[derive(Component, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HackBuffer {
    pub active: bool,
}

#[derive(Component, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainBuffer {
    pub resource: String,
    pub amount_per_tick: u32,
}

#[derive(Component, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverloadBuffer {
    pub fuel_drain_per_tick: u32,
    pub fuel_floor: u32,
}

#[derive(Component, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebilitateBuffer {
    pub damage_type: String,
}

#[derive(Component, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisruptBuffer {
    pub body_parts: Vec<BodyPart>,
}

#[derive(Component, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FortifyBuffer {
    pub active: bool,
}

#[derive(Component, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeechBuffer {
    pub resource: String,
    pub amount_per_tick: u32,
    pub age_acceleration: u32,
}

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FabricateBuffer {
    pub structure_type: StructureType,
}

impl Default for FabricateBuffer {
    fn default() -> Self {
        Self {
            structure_type: StructureType::FACTORY,
        }
    }
}

pub const DEFAULT_ROOM_SIZE: i32 = 50;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema, TS,
)]
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
    pub last_action_tick: crate::command::Tick,
    pub lifespan: u32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawningGrace {
    pub remaining: u32,
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
            last_action_tick: u64::MAX,
            lifespan,
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
pub struct Wreckage {
    pub former_owner: PlayerId,
    pub amounts: IndexMap<String, u32>,
    pub remaining_ticks: u32,
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

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct DeathMark;

#[derive(
    Component, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct StableEntityId(pub u64);

#[derive(BevyResource, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StableEntityIdAllocator {
    pub next: u64,
}

impl Default for StableEntityIdAllocator {
    fn default() -> Self {
        Self { next: 1 }
    }
}

impl StableEntityIdAllocator {
    pub fn allocate(&mut self) -> StableEntityId {
        let id = StableEntityId(self.next);
        self.next = self.next.saturating_add(1);
        id
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PendingEntityKind {
    Drone {
        owner: PlayerId,
        body: Vec<BodyPart>,
        position: Position,
        spawning_grace: u32,
    },
    Structure {
        position: Position,
        structure: Structure,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingEntityCreationEntry {
    pub stable_id: StableEntityId,
    pub kind: PendingEntityKind,
}

#[derive(BevyResource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingEntityCreation {
    pub entries: Vec<PendingEntityCreationEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    #[serde(rename = "type")]
    pub action_type: String,
    pub payload: serde_json::Value,
}

#[derive(BevyResource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRegistry {
    pub handlers: BTreeMap<String, String>,
}

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
    fn custom_action_registry_keeps_vanilla_actions_immutable() {
        let registry = CustomActionRegistry::from_defs(vec![CustomActionDef {
            name: "Overload".to_string(),
            description: "Attempted override".to_string(),
            range: 1,
            cost: [("Energy".to_string(), 1)].into_iter().collect(),
            ..Default::default()
        }]);

        let overload = registry.get("Overload").unwrap();
        assert_eq!(overload.range, 5);
        assert_eq!(overload.cost.get("Energy"), Some(&300));
        assert_eq!(overload.special_effect.as_deref(), Some("overload"));
    }

    #[test]
    fn custom_action_registry_merges_non_reserved_actions() {
        let registry = CustomActionRegistry::from_defs(vec![CustomActionDef {
            name: "ShieldPulse".to_string(),
            description: "Custom shield pulse".to_string(),
            range: 2,
            special_effect: Some("fortify".to_string()),
            cost: [("Energy".to_string(), 33)].into_iter().collect(),
            ..Default::default()
        }]);

        assert_eq!(registry.actions.len(), 12);
        assert!(registry.get("Attack").is_some());
        assert_eq!(registry.get("Overload").unwrap().range, 5);
        assert_eq!(registry.get("ShieldPulse").unwrap().range, 2);
    }

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

/// Per-tick event log for feedback loop (learn → decide → act → understand).
/// Systems write structured events; MCP tools (`swarm_explain_last_tick`,
/// `swarm_get_snapshot`) and WebSocket push consume them for real-time feedback.
#[derive(BevyResource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventLog {
    pub entries: Vec<EventLogEntry>,
    /// Maximum entries retained; oldest evicted when exceeded.
    pub max_entries: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventLogEntry {
    pub tick: crate::command::Tick,
    pub player_id: Option<PlayerId>,
    pub event_type: String,
    pub message: String,
}

impl EventLog {
    pub fn with_capacity(max_entries: usize) -> Self {
        Self {
            entries: Vec::with_capacity(max_entries.min(1024)),
            max_entries,
        }
    }

    pub fn push(
        &mut self,
        tick: crate::command::Tick,
        player_id: Option<PlayerId>,
        event_type: impl Into<String>,
        message: impl Into<String>,
    ) {
        if self.entries.len() >= self.max_entries {
            self.entries.remove(0);
        }
        self.entries.push(EventLogEntry {
            tick,
            player_id,
            event_type: event_type.into(),
            message: message.into(),
        });
    }

    /// Clear entries older than `horizon` ticks.
    pub fn retain_since(&mut self, horizon: crate::command::Tick) {
        self.entries.retain(|entry| entry.tick >= horizon);
    }
}
