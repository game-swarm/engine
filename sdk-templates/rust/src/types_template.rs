// ── Stable SDK types (template) ──
// Generated enums (Direction, BodyPart, StructureType, DamageType)
// and command types come from commands.rs which is produced by
// swarm-engine's IDL codegen.

use crate::commands::{BodyPart, Command, Direction, StructureType};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type ObjectId = u64;
pub type ResourceAmount = u32;
pub type ResourceName = String;
pub type PlayerId = u32;
pub type RoomId = u32;
pub type Tick = u64;
pub type ResourceMap = BTreeMap<ResourceName, ResourceAmount>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Position {
    pub x: i32,
    pub y: i32,
    pub room: RoomId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub rooms: Vec<RoomSnapshot>,
    pub objects: Vec<ObjectSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub player: Option<PlayerSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TickResult {
    pub commands: Vec<Command>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerSnapshot {
    pub id: PlayerId,
    #[serde(default)]
    pub resources: ResourceMap,
    #[serde(default)]
    pub global_storage: ResourceMap,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomSnapshot {
    pub id: RoomId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectSnapshot {
    pub id: ObjectId,
    pub position: Position,
    #[serde(flatten)]
    pub kind: ObjectKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum ObjectKind {
    Drone {
        owner: PlayerId,
        body: Vec<BodyPart>,
        fatigue: u32,
        hits: u32,
        hits_max: u32,
        spawning: bool,
        age: u32,
        #[serde(default)]
        carry: ResourceMap,
    },
    Structure {
        structure: StructureType,
        owner: Option<PlayerId>,
        hits: u32,
        hits_max: u32,
        #[serde(default)]
        store: ResourceMap,
        #[serde(default)]
        cooldown: u32,
    },
    Resource {
        amounts: ResourceMap,
    },
    Source {
        produces: ResourceMap,
        capacity: ResourceAmount,
        ticks_to_regeneration: u32,
    },
    Controller {
        owner: Option<PlayerId>,
        level: u8,
        progress: u32,
        progress_total: u32,
        downgrade_timer: u32,
        safe_mode: u32,
        safe_mode_available: u32,
        safe_mode_cooldown: u32,
    },
}
