// ── Stable SDK types (template) ──
// The generated types (Direction, BodyPart, Action, CommandIntent, etc.)
// are in commands.ts which is produced by swarm-engine's IDL codegen.

import type { Action, BodyPart, CommandIntent, ResourceCost, ResourceName, StructureType } from "./commands.js";

type UInt32 = number;
type UInt64 = number | bigint;
type PlayerId = UInt32;
type RoomId = UInt32;
type ObjectId = UInt64;
type Tick = UInt64;

interface Position {
  x: number;
  y: number;
  room: RoomId;
}

export type CommandSource = "WASM" | "MCP_Deploy" | "REST" | "AdminCLI";
export interface RawCommand<A extends Action = Action> extends CommandIntent<A> {
  player_id: PlayerId;
  tick: Tick;
  source: CommandSource;
}

export interface TickTraceRejection<A extends Action = Action> {
  command?: RawCommand<A> | CommandIntent<A>;
  rejection: string;
  detail?: JsonObject;
  tick: Tick;
}

export interface UntrustedString {
  value: string;
  untrusted: true;
  source_player: PlayerId;
}

export type EntityKind = "drone" | "structure" | "resource" | "source" | "controller" | "construction_site";

export interface BaseEntity {
  id: ObjectId;
  type: EntityKind;
  owner?: PlayerId | 0 | null;
  position: Position;
  name?: UntrustedString;
  vision_range?: number;
  hits?: number;
  hits_max?: number;
}

export interface DroneEntity extends BaseEntity {
  type: "drone";
  owner: PlayerId | 0;
  body: BodyPart[];
  fatigue: number;
  spawning?: boolean;
  carry?: ResourceCost;
  carry_capacity?: number;
}

export interface StructureEntity extends BaseEntity {
  type: "structure";
  structure_type: StructureType;
  cooldown?: number;
  store?: ResourceCost;
  store_capacity?: ResourceCost;
}

export interface SourceEntity extends BaseEntity {
  type: "source";
  produces: ResourceCost;
  capacity: number;
  ticks_to_regeneration: number;
}

export type WorldEntity = BaseEntity | DroneEntity | StructureEntity | SourceEntity;

export interface TerrainTile {
  position: Position;
  terrain: TerrainType;
}
export type TerrainType = "Plain" | "Wall" | "Swamp";

export interface LeaderboardSnapshot {
  rank: number;
  gcl: number;
  rooms?: number;
  drones?: number;
}

export interface WorldSnapshot {
  tick: Tick;
  player_id: PlayerId;
  _untrusted_game_data?: true;
  entities: WorldEntity[];
  terrain: TerrainTile[];
  resources: ResourceCost;
  controller?: JsonObject;
  leaderboard_snapshot?: LeaderboardSnapshot;
  world_rules?: WorldConfig;
}

export interface TickMetrics {
  tick: Tick;
  player_id?: PlayerId;
  cpu_fuel?: number;
  cmd_count?: number;
  cmd_success?: number;
  latency_ms?: number;
  collect_timeouts?: number;
}

export interface TickExplanation {
  tick: Tick;
  commands_submitted: number;
  commands_accepted: number;
  commands_rejected: Array<{ command: string; reason: string; detail?: string | JsonObject; suggestion?: string }>;
  state_changes: string[];
  notable_events: string[];
}

export type WorldMode = "persistent" | "tutorial" | "novice" | "arena";
export type SpawnPolicy = "RandomRoom" | "ManualSelect" | "FixedSpawn" | "Inherit";
export type RespawnPolicy = "NewRoom" | "SameRoom" | "Spectate" | "Ban";
export type CodePropagationSource = "Spawn" | "Controller" | "AnyDrone";
export type PlayerView = "drone" | "full" | "allied";
export type ReplayPrivacy = "private" | "allies" | "world" | "public";

export interface ResourceDef {
  name: ResourceName;
  display_name?: string;
  category?: string;
  starting_amount?: number;
  max_storage?: number;
  decay_rate?: number;
  tradeable?: boolean;
}

export interface SourceDef {
  name: string;
  produces: ResourceCost;
  capacity: number;
  regeneration: number;
}

export interface ModConfig {
  name: string;
  version: string;
  config?: JsonObject;
}

export interface WorldConfig {
  world: { name: string; mode: WorldMode; tick_interval_ms: number };
  spawn: { policy: SpawnPolicy; respawn: RespawnPolicy; cooldown: number };
  code: {
    update_cost: ResourceCost;
    update_cooldown: number;
    update_window: { every: number; duration: number };
    propagation_speed: number;
    propagation_source: CodePropagationSource;
  };
  drone: {
    env_vars: boolean;
    memory_size: number;
    memory_spawn_cost: ResourceCost;
    memory_upkeep_cost: ResourceCost;
    max_body_parts: number;
    max_drones_per_player: number;
    lifespan: number;
  };
  resources: {
    source_regeneration_rate: number;
    build_cost_multiplier: number;
    drone_decay_rate: number;
    global_storage_enabled: boolean;
    global_storage_capacity: number;
    transfer_to_global_cost: Record<string, number>;
    transfer_from_global_cost: Record<string, number>;
    transfer_to_global_time: number;
    transfer_from_global_time: number;
  };
  combat: { pvp_enabled: boolean; friendly_fire: boolean; damage_multiplier: number };
  visibility: { fog_of_war: boolean; player_view: PlayerView; public_spectate: boolean; spectate_delay: number; replay_privacy: ReplayPrivacy };
  resource_types: ResourceDef[];
  source_types: SourceDef[];
  actions: { costs: Record<string, ResourceCost> };
  mods: ModConfig[];
}

export type JsonPrimitive = string | number | boolean | null;
export type JsonValue = JsonPrimitive | JsonValue[] | JsonObject;
export interface JsonObject {
  [key: string]: JsonValue;
}

export interface ValidationIssue {
  path: string;
  message: string;
  code: string;
}

export interface ValidationResult<T> {
  ok: boolean;
  value?: T;
  issues: ValidationIssue[];
}
