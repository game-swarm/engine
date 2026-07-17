use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{AuthMode, McpContext, ToolInfo};
use crate::components::{Controller, Drone, Position, RoomId};
use crate::world::SwarmWorld;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldStatsParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldStatsResult {
    pub tick: u64,
    pub scope: String,
    pub gcl_total: u32,
    pub room_count: usize,
    pub drone_count: usize,
}

pub fn swarm_get_world_stats(
    world: &mut SwarmWorld,
    context: McpContext,
    _params: WorldStatsParams,
) -> WorldStatsResult {
    let mut owned_rooms = BTreeSet::<RoomId>::new();
    let mut gcl_total = 0_u32;
    {
        let mut query = world.app.world_mut().query::<(&Position, &Controller)>();
        for (position, controller) in query.iter(world.app.world()) {
            if controller.owner.is_some() {
                owned_rooms.insert(position.room);
                gcl_total = gcl_total.saturating_add(u32::from(controller.level));
            }
        }
    }

    let drone_count = {
        let mut query = world.app.world_mut().query::<&Drone>();
        query.iter(world.app.world()).count()
    };

    WorldStatsResult {
        tick: context.tick,
        scope: "public_world".to_string(),
        gcl_total,
        room_count: owned_rooms.len(),
        drone_count,
    }
}

pub fn world_stats_tool_info() -> ToolInfo {
    ToolInfo {
        name: "swarm_get_world_statistics".to_string(),
        description: "Get public aggregate world statistics without rankings or entity identifiers"
            .to_string(),
        auth_mode: AuthMode::WebSessionOk,
        input_schema: empty_object_schema(),
        output_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["tick", "scope", "gcl_total", "room_count", "drone_count"],
            "properties": {
                "tick": {"type": "integer", "minimum": 0},
                "scope": {"const": "public_world"},
                "gcl_total": {"type": "integer", "minimum": 0},
                "room_count": {"type": "integer", "minimum": 0},
                "drone_count": {"type": "integer", "minimum": 0}
            }
        }),
    }
}

fn empty_object_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {}
    })
}
