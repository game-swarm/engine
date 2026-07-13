use bevy::prelude::Component;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::command::Tick;
use crate::components::Position;
use crate::resources::ResourceCost;

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NpcType {
    Creep,
    Guardian,
    Merchant,
    Swarmling,
}

impl NpcType {
    pub fn spawn_cycle(self) -> Option<Tick> {
        match self {
            Self::Creep => Some(50),
            Self::Guardian => Some(300),
            Self::Merchant => Some(500),
            Self::Swarmling => None,
        }
    }

    pub fn spawn_cycle_types(tick: Tick) -> Vec<Self> {
        let mut types = Vec::new();
        for npc_type in [Self::Creep, Self::Guardian, Self::Merchant] {
            if npc_type
                .spawn_cycle()
                .is_some_and(|cycle| tick > 0 && tick.is_multiple_of(cycle))
            {
                types.push(npc_type);
            }
        }
        if tick > 0 && tick.is_multiple_of(1_000) {
            types.push(Self::Swarmling);
        }
        types
    }

    pub fn spawn_count(self, tick: Tick) -> u32 {
        match self {
            Self::Swarmling => 10 + (tick % 21) as u32,
            _ => 1,
        }
    }
}

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NpcHp {
    pub current: u32,
    pub max: u32,
}

impl NpcHp {
    pub fn for_type(npc_type: NpcType) -> Self {
        let max = match npc_type {
            NpcType::Creep => 50,
            NpcType::Guardian => 300,
            NpcType::Merchant => 200,
            NpcType::Swarmling => 500,
        };
        Self { current: max, max }
    }
}

impl Default for NpcHp {
    fn default() -> Self {
        Self::for_type(NpcType::Creep)
    }
}

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NpcDamage {
    pub per_tick: u32,
}

impl NpcDamage {
    pub fn for_type(npc_type: NpcType) -> Self {
        Self {
            per_tick: match npc_type {
                NpcType::Creep => 10,
                NpcType::Guardian => 30,
                NpcType::Merchant => 0,
                NpcType::Swarmling => 5,
            },
        }
    }
}

#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NpcBehavior {
    #[default]
    Patrol,
    Guard,
    TradeRoute,
    GroupAttack,
}

impl NpcBehavior {
    pub fn for_type(npc_type: NpcType) -> Self {
        match npc_type {
            NpcType::Creep => Self::Patrol,
            NpcType::Guardian => Self::Guard,
            NpcType::Merchant => Self::TradeRoute,
            NpcType::Swarmling => Self::GroupAttack,
        }
    }
}

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NpcZone {
    pub level: u8,
}

impl NpcZone {
    pub fn for_position(position: Position) -> Self {
        let distance = position.x.abs().max(position.y.abs());
        Self {
            level: match distance {
                0..=12 => 1,
                13..=24 => 2,
                25..=36 => 3,
                _ => 4,
            },
        }
    }
}

impl Default for NpcZone {
    fn default() -> Self {
        Self { level: 1 }
    }
}

#[derive(Component, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NpcDrop {
    pub resources: ResourceCost,
    pub blueprint_chance_per_10_000: u32,
    pub wreckage: bool,
}

impl NpcDrop {
    pub fn for_type(npc_type: NpcType) -> Self {
        let resources = match npc_type {
            NpcType::Creep => resource_cost(&[("Energy", 20)]),
            NpcType::Guardian => resource_cost(&[("Crystal", 10)]),
            NpcType::Merchant => resource_cost(&[("Blueprint", 1)]),
            NpcType::Swarmling => resource_cost(&[("Wreckage", 1)]),
        };
        Self {
            resources,
            blueprint_chance_per_10_000: if npc_type == NpcType::Guardian {
                500
            } else {
                0
            },
            wreckage: matches!(npc_type, NpcType::Guardian | NpcType::Swarmling),
        }
    }
}

impl Default for NpcDrop {
    fn default() -> Self {
        Self::for_type(NpcType::Creep)
    }
}

fn resource_cost(entries: &[(&str, u32)]) -> ResourceCost {
    let mut cost = IndexMap::new();
    for (resource, amount) in entries {
        cost.insert((*resource).to_string(), *amount);
    }
    cost
}
