use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::command::{ObjectId, Tick, object_id};
use crate::components::*;
use crate::world::SwarmWorld;

const VISIBILITY_RADIUS: i32 = 5;
const MAX_WASM_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpContext {
    pub player_id: PlayerId,
    pub tick: Tick,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpError {
    pub code: i32,
    pub message: String,
}

impl McpError {
    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }

    fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("unknown MCP tool: {method}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<McpError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleWorldSnapshot {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub room_id: u32,
    pub visibility_radius: i32,
    pub visible_tiles: Vec<VisibleTile>,
    pub entities: Vec<VisibleEntity>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VisibleTile {
    pub x: i32,
    pub y: i32,
    pub terrain: TerrainType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum VisibleEntity {
    Drone(VisibleDrone),
    Structure(VisibleStructure),
    Source(VisibleSource),
    Resource(VisibleResource),
    Controller(VisibleController),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisiblePosition {
    pub x: i32,
    pub y: i32,
    pub room_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleDrone {
    pub id: ObjectId,
    pub owner: PlayerId,
    pub position: VisiblePosition,
    pub body: Vec<BodyPart>,
    pub carry: BTreeMap<String, u32>,
    pub carry_capacity: u32,
    pub fatigue: u32,
    pub hits: u32,
    pub hits_max: u32,
    pub spawning: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleStructure {
    pub id: ObjectId,
    pub structure_type: StructureType,
    pub owner: Option<PlayerId>,
    pub position: VisiblePosition,
    pub hits: u32,
    pub hits_max: u32,
    pub energy: Option<u32>,
    pub energy_capacity: Option<u32>,
    pub cooldown: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleSource {
    pub id: ObjectId,
    pub position: VisiblePosition,
    pub produces: BTreeMap<String, u32>,
    pub capacity: u32,
    pub ticks_to_regeneration: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleResource {
    pub id: ObjectId,
    pub position: VisiblePosition,
    pub amounts: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleController {
    pub id: ObjectId,
    pub owner: Option<PlayerId>,
    pub position: VisiblePosition,
    pub level: u8,
    pub progress: u32,
    pub progress_total: u32,
    pub safe_mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldRules {
    pub ruleset: String,
    pub room_size: i32,
    pub visibility_radius: i32,
    pub max_wasm_bytes: usize,
    pub active_mods: Vec<WorldRuleMod>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldRuleMod {
    pub id: String,
    pub version: String,
    pub description: String,
    pub config: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployParams {
    pub wasm_bytes: String,
    pub language: String,
    pub version_tag: String,
    pub room_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployResult {
    pub module_id: String,
    pub status: String,
    pub deployed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredModule {
    pub module_id: String,
    pub player_id: PlayerId,
    pub room_id: RoomId,
    pub wasm_bytes: Vec<u8>,
    pub language: String,
    pub version_tag: String,
    pub deployed_at: String,
    pub load_after_tick: Tick,
}

#[derive(Debug, Default)]
pub struct McpServer {
    modules: Vec<StoredModule>,
}

impl McpServer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle_json_rpc(
        &mut self,
        world: &mut SwarmWorld,
        context: McpContext,
        request: JsonRpcRequest,
    ) -> JsonRpcResponse {
        let id = request.id.clone();
        if request.jsonrpc != "2.0" {
            return error_response(id, McpError::invalid_params("jsonrpc must be 2.0"));
        }

        match self.call_tool(world, context, &request.method, request.params) {
            Ok(result) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(result),
                error: None,
            },
            Err(error) => error_response(id, error),
        }
    }

    pub fn call_tool(
        &mut self,
        world: &mut SwarmWorld,
        context: McpContext,
        tool: &str,
        params: Value,
    ) -> Result<Value, McpError> {
        match tool {
            "swarm_get_snapshot" => serde_json::to_value(swarm_get_snapshot(world, context))
                .map_err(|error| McpError::invalid_params(error.to_string())),
            "swarm_get_world_rules" => serde_json::to_value(swarm_get_world_rules())
                .map_err(|error| McpError::invalid_params(error.to_string())),
            "swarm_deploy" => {
                let params: DeployParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(self.swarm_deploy(world, context, params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            method => Err(McpError::method_not_found(method)),
        }
    }

    pub fn swarm_deploy(
        &mut self,
        world: &SwarmWorld,
        context: McpContext,
        params: DeployParams,
    ) -> Result<DeployResult, McpError> {
        if params.language.trim().is_empty() {
            return Err(McpError::invalid_params("language is required"));
        }
        if params.version_tag.trim().is_empty() {
            return Err(McpError::invalid_params("version_tag is required"));
        }

        let room_id = RoomId(params.room_id);
        if !world
            .app
            .world()
            .resource::<RoomTerrains>()
            .0
            .contains_key(&room_id)
        {
            return Err(McpError::invalid_params("room_id does not exist"));
        }

        let wasm_bytes = decode_base64(&params.wasm_bytes)?;
        if wasm_bytes.is_empty() {
            return Err(McpError::invalid_params("wasm_bytes is empty"));
        }
        if wasm_bytes.len() > MAX_WASM_BYTES {
            return Err(McpError::invalid_params("wasm module exceeds max size"));
        }
        if !wasm_bytes.starts_with(b"\0asm") {
            return Err(McpError::invalid_params("wasm_bytes must be a wasm module"));
        }

        let module_id = format!(
            "mod_{}_{}_{}",
            context.player_id,
            params.room_id,
            self.modules.len() + 1
        );
        let deployed_at = unix_timestamp_string();
        self.modules.push(StoredModule {
            module_id: module_id.clone(),
            player_id: context.player_id,
            room_id,
            wasm_bytes,
            language: params.language,
            version_tag: params.version_tag,
            deployed_at: deployed_at.clone(),
            load_after_tick: context.tick + 1,
        });

        Ok(DeployResult {
            module_id,
            status: "pending_next_tick".to_string(),
            deployed_at,
        })
    }

    pub fn modules(&self) -> &[StoredModule] {
        &self.modules
    }
}

pub fn swarm_get_snapshot(world: &mut SwarmWorld, context: McpContext) -> VisibleWorldSnapshot {
    let room_id = RoomId(0);
    let visible_positions = visible_positions(world.app.world_mut(), context.player_id);
    let terrains = world.app.world().resource::<RoomTerrains>();
    let mut visible_tiles = terrains
        .0
        .get(&room_id)
        .into_iter()
        .flat_map(|room| room.iter())
        .filter(|(x, y, _)| visible_positions.contains(&(room_id, *x, *y)))
        .map(|(x, y, terrain)| VisibleTile { x, y, terrain })
        .collect::<Vec<_>>();
    visible_tiles.sort();

    let mut entities =
        visible_entities(world.app.world_mut(), context.player_id, &visible_positions);
    entities.sort_by_key(entity_sort_key);

    VisibleWorldSnapshot {
        tick: context.tick,
        player_id: context.player_id,
        room_id: room_id.0,
        visibility_radius: VISIBILITY_RADIUS,
        visible_tiles,
        entities,
    }
}

pub fn swarm_get_world_rules() -> WorldRules {
    let mut engine_config = BTreeMap::new();
    engine_config.insert("mcp_direct_gameplay_actions".to_string(), json!(false));
    engine_config.insert(
        "snapshot_visibility".to_string(),
        json!("player_visible_tiles_only"),
    );

    let mut base_config = BTreeMap::new();
    base_config.insert(
        "max_body_parts".to_string(),
        json!(crate::command::MAX_BODY_PARTS),
    );
    base_config.insert(
        "max_commands_per_player".to_string(),
        json!(crate::command::MAX_COMMANDS_PER_PLAYER),
    );
    base_config.insert(
        "max_drones_per_player".to_string(),
        json!(crate::command::MAX_DRONES_PER_PLAYER),
    );

    WorldRules {
        ruleset: "phase1".to_string(),
        room_size: DEFAULT_ROOM_SIZE,
        visibility_radius: VISIBILITY_RADIUS,
        max_wasm_bytes: MAX_WASM_BYTES,
        active_mods: vec![
            WorldRuleMod {
                id: "mcp_security_contract".to_string(),
                version: "phase1".to_string(),
                description: "MCP exposes deploy and safe read-only tools only".to_string(),
                config: engine_config,
            },
            WorldRuleMod {
                id: "base_world".to_string(),
                version: "phase1".to_string(),
                description: "Core room, command, and sandbox limits".to_string(),
                config: base_config,
            },
        ],
    }
}

pub fn is_visible_to(world: &mut World, player_id: PlayerId, position: Position) -> bool {
    visible_positions(world, player_id).contains(&(position.room, position.x, position.y))
}

fn visible_positions(world: &mut World, player_id: PlayerId) -> BTreeSet<(RoomId, i32, i32)> {
    let mut anchors = world
        .query::<(&Position, Option<&Drone>, Option<&Structure>)>()
        .iter(world)
        .filter_map(|(position, drone, structure)| {
            let owned_drone = drone.is_some_and(|drone| drone.owner == player_id);
            let owned_structure =
                structure.is_some_and(|structure| structure.owner == Some(player_id));
            (owned_drone || owned_structure).then_some(*position)
        })
        .collect::<Vec<_>>();
    anchors.sort_by_key(|position| (position.room.0, position.x, position.y));

    let terrains = world.resource::<RoomTerrains>();
    let mut visible = BTreeSet::new();
    for anchor in anchors {
        if let Some(room) = terrains.0.get(&anchor.room) {
            for y in (anchor.y - VISIBILITY_RADIUS)..=(anchor.y + VISIBILITY_RADIUS) {
                for x in (anchor.x - VISIBILITY_RADIUS)..=(anchor.x + VISIBILITY_RADIUS) {
                    if room.contains(x, y) {
                        visible.insert((anchor.room, x, y));
                    }
                }
            }
        }
    }
    visible
}

fn visible_entities(
    world: &mut World,
    player_id: PlayerId,
    visible_positions: &BTreeSet<(RoomId, i32, i32)>,
) -> Vec<VisibleEntity> {
    let mut entities = Vec::new();

    for (entity, position, drone) in world.query::<(Entity, &Position, &Drone)>().iter(world) {
        let owned = drone.owner == player_id;
        if owned || visible_positions.contains(&(position.room, position.x, position.y)) {
            entities.push(VisibleEntity::Drone(VisibleDrone {
                id: object_id(entity),
                owner: drone.owner,
                position: visible_position(*position),
                body: drone.body.clone(),
                carry: drone.carry.iter().map(|(k, v)| (k.clone(), *v)).collect(),
                carry_capacity: drone.carry_capacity,
                fatigue: drone.fatigue,
                hits: drone.hits,
                hits_max: drone.hits_max,
                spawning: drone.spawning,
            }));
        }
    }

    for (entity, position, structure) in
        world.query::<(Entity, &Position, &Structure)>().iter(world)
    {
        let owned = structure.owner == Some(player_id);
        if owned || visible_positions.contains(&(position.room, position.x, position.y)) {
            entities.push(VisibleEntity::Structure(VisibleStructure {
                id: object_id(entity),
                structure_type: structure.structure_type,
                owner: structure.owner,
                position: visible_position(*position),
                hits: structure.hits,
                hits_max: structure.hits_max,
                energy: structure.energy,
                energy_capacity: structure.energy_capacity,
                cooldown: structure.cooldown,
            }));
        }
    }

    for (entity, position, source) in world.query::<(Entity, &Position, &Source)>().iter(world) {
        if visible_positions.contains(&(position.room, position.x, position.y)) {
            entities.push(VisibleEntity::Source(VisibleSource {
                id: object_id(entity),
                position: visible_position(*position),
                produces: source
                    .produces
                    .iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .collect(),
                capacity: source.capacity,
                ticks_to_regeneration: source.ticks_to_regeneration,
            }));
        }
    }

    for (entity, position, resource) in world
        .query::<(Entity, &Position, &crate::components::Resource)>()
        .iter(world)
    {
        if visible_positions.contains(&(position.room, position.x, position.y)) {
            entities.push(VisibleEntity::Resource(VisibleResource {
                id: object_id(entity),
                position: visible_position(*position),
                amounts: resource
                    .amounts
                    .iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .collect(),
            }));
        }
    }

    for (entity, position, controller) in world
        .query::<(Entity, &Position, &Controller)>()
        .iter(world)
    {
        if visible_positions.contains(&(position.room, position.x, position.y)) {
            entities.push(VisibleEntity::Controller(VisibleController {
                id: object_id(entity),
                owner: controller.owner,
                position: visible_position(*position),
                level: controller.level,
                progress: controller.progress,
                progress_total: controller.progress_total,
                safe_mode: controller.safe_mode,
            }));
        }
    }

    entities
}

fn visible_position(position: Position) -> VisiblePosition {
    VisiblePosition {
        x: position.x,
        y: position.y,
        room_id: position.room.0,
    }
}

fn entity_sort_key(entity: &VisibleEntity) -> (u8, ObjectId) {
    match entity {
        VisibleEntity::Drone(entity) => (0, entity.id),
        VisibleEntity::Structure(entity) => (1, entity.id),
        VisibleEntity::Source(entity) => (2, entity.id),
        VisibleEntity::Resource(entity) => (3, entity.id),
        VisibleEntity::Controller(entity) => (4, entity.id),
    }
}

fn error_response(id: Value, error: McpError) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: None,
        error: Some(error),
    }
}

fn unix_timestamp_string() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    seconds.to_string()
}

fn decode_base64(input: &str) -> Result<Vec<u8>, McpError> {
    let bytes = input.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err(McpError::invalid_params("wasm_bytes is not valid base64"));
    }

    let mut output = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut chunks = bytes.chunks_exact(4).peekable();
    while let Some(chunk) = chunks.next() {
        let last = chunks.peek().is_none();
        let a = base64_value(chunk[0])?;
        let b = base64_value(chunk[1])?;
        let c = if chunk[2] == b'=' {
            64
        } else {
            base64_value(chunk[2])?
        };
        let d = if chunk[3] == b'=' {
            64
        } else {
            base64_value(chunk[3])?
        };
        if (chunk[2] == b'=' && chunk[3] != b'=')
            || (!last && (chunk[2] == b'=' || chunk[3] == b'='))
        {
            return Err(McpError::invalid_params("wasm_bytes is not valid base64"));
        }

        output.push((a << 2) | (b >> 4));
        if c != 64 {
            output.push(((b & 0x0f) << 4) | (c >> 2));
        }
        if d != 64 {
            output.push(((c & 0x03) << 6) | d);
        }
    }

    Ok(output)
}

fn base64_value(byte: u8) -> Result<u8, McpError> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(McpError::invalid_params("wasm_bytes is not valid base64")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Structure, StructureType, create_world};

    fn spawn_structure(world: &mut SwarmWorld, owner: Option<PlayerId>, x: i32, y: i32) {
        world.app.world_mut().spawn((
            Position {
                x,
                y,
                room: RoomId(0),
            },
            Structure {
                structure_type: StructureType::Spawn,
                owner,
                hits: 5_000,
                hits_max: 5_000,
                energy: Some(300),
                energy_capacity: Some(300),
                cooldown: 0,
            },
        ));
    }

    #[test]
    fn snapshot_filters_entities_and_terrain_by_player_visibility() {
        let mut world = create_world();
        world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        world.spawn_drone(2, 12, 10, vec![BodyPart::Move]);
        world.spawn_drone(2, 40, 40, vec![BodyPart::Move]);

        let snapshot = swarm_get_snapshot(
            &mut world,
            McpContext {
                player_id: 1,
                tick: 7,
            },
        );

        assert_eq!(snapshot.tick, 7);
        assert!(
            snapshot
                .visible_tiles
                .iter()
                .any(|tile| tile.x == 10 && tile.y == 10)
        );
        assert!(
            !snapshot
                .visible_tiles
                .iter()
                .any(|tile| tile.x == 40 && tile.y == 40)
        );
        assert!(snapshot.visible_tiles.len() < (DEFAULT_ROOM_SIZE * DEFAULT_ROOM_SIZE) as usize);

        let drone_positions = snapshot
            .entities
            .iter()
            .filter_map(|entity| match entity {
                VisibleEntity::Drone(drone) => {
                    Some((drone.owner, drone.position.x, drone.position.y))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(drone_positions.contains(&(1, 10, 10)));
        assert!(drone_positions.contains(&(2, 12, 10)));
        assert!(!drone_positions.contains(&(2, 40, 40)));
    }

    #[test]
    fn owned_structure_extends_visibility_without_leaking_full_map() {
        let mut world = create_world();
        spawn_structure(&mut world, Some(1), 35, 35);

        let snapshot = swarm_get_snapshot(
            &mut world,
            McpContext {
                player_id: 1,
                tick: 1,
            },
        );

        assert!(is_visible_to(
            world.app.world_mut(),
            1,
            Position {
                x: 35,
                y: 35,
                room: RoomId(0)
            }
        ));
        assert!(
            snapshot
                .visible_tiles
                .iter()
                .any(|tile| tile.x == 35 && tile.y == 35)
        );
        assert!(
            !snapshot
                .visible_tiles
                .iter()
                .any(|tile| tile.x == 0 && tile.y == 0)
        );
    }

    #[test]
    fn deploy_validates_and_stores_wasm_for_next_tick_loading() {
        let world = create_world();
        let mut server = McpServer::new();
        let result = server
            .swarm_deploy(
                &world,
                McpContext {
                    player_id: 42,
                    tick: 11,
                },
                DeployParams {
                    wasm_bytes: "AGFzbQEAAAA=".to_string(),
                    language: "rust".to_string(),
                    version_tag: "v1".to_string(),
                    room_id: 0,
                },
            )
            .expect("deploy should succeed");

        assert_eq!(result.status, "pending_next_tick");
        assert_eq!(server.modules().len(), 1);
        assert_eq!(server.modules()[0].module_id, result.module_id);
        assert_eq!(server.modules()[0].load_after_tick, 12);
        assert_eq!(server.modules()[0].wasm_bytes, b"\0asm\x01\0\0\0");
    }

    #[test]
    fn deploy_rejects_invalid_base64_and_non_wasm() {
        let world = create_world();
        let mut server = McpServer::new();
        let context = McpContext {
            player_id: 1,
            tick: 0,
        };

        assert!(
            server
                .swarm_deploy(
                    &world,
                    context.clone(),
                    DeployParams {
                        wasm_bytes: "not base64".to_string(),
                        language: "rust".to_string(),
                        version_tag: "v1".to_string(),
                        room_id: 0,
                    },
                )
                .is_err()
        );
        assert!(
            server
                .swarm_deploy(
                    &world,
                    context,
                    DeployParams {
                        wasm_bytes: "YWJj".to_string(),
                        language: "rust".to_string(),
                        version_tag: "v1".to_string(),
                        room_id: 0,
                    },
                )
                .is_err()
        );
    }

    #[test]
    fn world_rules_expose_safe_readable_configuration() {
        let rules = swarm_get_world_rules();

        assert_eq!(rules.ruleset, "phase1");
        assert_eq!(rules.room_size, DEFAULT_ROOM_SIZE);
        assert!(rules.active_mods.iter().any(|module| {
            module.id == "mcp_security_contract"
                && module.config.get("mcp_direct_gameplay_actions") == Some(&json!(false))
        }));
    }

    #[test]
    fn json_rpc_dispatches_only_phase1_mcp_tools() {
        let mut world = create_world();
        let mut server = McpServer::new();
        let context = McpContext {
            player_id: 1,
            tick: 0,
        };

        let ok = server.handle_json_rpc(
            &mut world,
            context.clone(),
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: json!(1),
                method: "swarm_get_world_rules".to_string(),
                params: Value::Null,
            },
        );
        assert!(ok.result.is_some());
        assert!(ok.error.is_none());

        let denied = server.handle_json_rpc(
            &mut world,
            context,
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: json!(2),
                method: "swarm_move".to_string(),
                params: Value::Null,
            },
        );
        assert!(denied.result.is_none());
        assert_eq!(denied.error.unwrap().code, -32601);
    }
}
