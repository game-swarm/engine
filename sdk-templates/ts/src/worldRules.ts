import { MAX_BODY_PARTS, MAX_DRONES_PER_PLAYER } from "./commands.js";
import { addCost } from "./commands.js";
import type { BodyPart, ResourceCost, ValidationIssue, WorldConfig, WorldMode } from "./commands.js";

export function createDefaultWorldConfig(mode: WorldMode = "persistent"): WorldConfig {
  const arena = mode === "arena";
  return {
    world: { name: arena ? "Swarm Arena" : "World of Swarm", mode, tick_interval_ms: 1000 },
    spawn: { policy: arena ? "FixedSpawn" : "RandomRoom", respawn: "NewRoom", cooldown: 0 },
    code: { update_cost: {}, update_cooldown: arena ? 0 : 5, update_window: { every: 0, duration: 0 }, propagation_speed: 0, propagation_source: "Spawn" },
    drone: {
      env_vars: true,
      memory_size: 1024,
      memory_spawn_cost: {},
      memory_upkeep_cost: {},
      max_body_parts: MAX_BODY_PARTS,
      max_drones_per_player: MAX_DRONES_PER_PLAYER,
      lifespan: 1500
    },
    resources: {
      source_regeneration_rate: 10000,
      build_cost_multiplier: 10000,
      drone_decay_rate: 10000,
      global_storage_enabled: true,
      global_storage_capacity: 100000,
      transfer_to_global_cost: { Energy: 0.01 },
      transfer_from_global_cost: { Energy: 0.05 },
      transfer_to_global_time: 10,
      transfer_from_global_time: 5,
    },
    combat: { pvp_enabled: true, friendly_fire: false, damage_multiplier: 1 },
    visibility: { fog_of_war: !arena, player_view: "drone", public_spectate: arena, spectate_delay: arena ? 100 : 0, replay_privacy: arena ? "public" : "private" },
    resource_types: [{ name: "Energy", display_name: "Energy", category: "energy", starting_amount: 1000, max_storage: 100000, decay_rate: 0, tradeable: true }],
    source_types: [{ name: "EnergyField", produces: { Energy: 1 }, capacity: 3000, regeneration: 300 }],
    actions: { costs: {} },
    mods: []
  };
}

export function validateWorldConfig(config: WorldConfig): ValidationIssue[] {
  const issues: ValidationIssue[] = [];
  if (config.world.tick_interval_ms < 1000) push(issues, "world.tick_interval_ms", "tick interval must be at least 1000ms", "TickIntervalTooShort");
  if (config.code.propagation_speed < 0 || config.code.propagation_speed > 100) push(issues, "code.propagation_speed", "propagation speed must be 0..100", "InvalidPropagationSpeed");
  if (config.drone.memory_size > 65536) push(issues, "drone.memory_size", "memory size must not exceed 64KB", "MemoryTooLarge");
  if (config.drone.max_body_parts > MAX_BODY_PARTS) push(issues, "drone.max_body_parts", "max body parts must not exceed 50", "BodyTooLarge");
  if (config.combat.damage_multiplier <= 0) push(issues, "combat.damage_multiplier", "damage multiplier must be positive", "InvalidDamageMultiplier");
  if (config.world.mode === "persistent" && config.visibility.public_spectate && config.visibility.spectate_delay < 50) {
    push(issues, "visibility.spectate_delay", "persistent public spectate requires at least 50 tick delay", "SpectateDelayTooLow");
  }
  if (config.resources.global_storage_enabled && (config.resources.transfer_to_global_time <= 0 || config.resources.transfer_from_global_time <= 0)) {
    push(issues, "resources.transfer_time", "global/local transfer times must be positive", "InvalidTransferTime");
  }
  const resourceNames = new Set(config.resource_types.map((resource) => resource.name));
  for (const [path, cost] of Object.entries(config.actions.costs)) {
    for (const resource of Object.keys(cost)) {
      if (!resourceNames.has(resource)) push(issues, `actions.costs.${path}.${resource}`, "resource is not declared", "UnknownResource");
    }
  }
  return issues;
}

export function actionCost(config: WorldConfig, action: string, detail?: string): ResourceCost {
  const key = detail ? `${action}.${detail}` : action;
  return { ...(config.actions.costs[key] ?? {}) };
}

export function bodyPartCost(config: WorldConfig, part: BodyPart): ResourceCost {
  return actionCost(config, "body_part", part);
}

export function bodyCostFromRules(config: WorldConfig, body: BodyPart[]): ResourceCost {
  const total: ResourceCost = {};
  for (const part of body) addCost(total, bodyPartCost(config, part));
  return total;
}

function push(issues: ValidationIssue[], path: string, message: string, code: string): void {
  issues.push({ path, message, code });
}
