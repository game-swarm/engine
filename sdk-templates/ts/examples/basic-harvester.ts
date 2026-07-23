import {
  actions,
  command,
  type CommandIntent,
  type Direction,
  type DroneEntity,
  type HarvestAction,
  type StructureEntity,
  type WorldEntity,
  type WorldSnapshot
} from "@swarm/sdk-ts";

const ENERGY = "Energy";
const DIRECTIONS: Direction[] = ["North", "South", "East", "West"];

export function tick(snapshot: WorldSnapshot): CommandIntent[] {
  const sources = snapshot.entities.filter((entity) => entity.type === "source");
  const stores = snapshot.entities.filter(
    (entity): entity is StructureEntity =>
      entity.type === "structure" &&
      "structure_type" in entity &&
      entity.owner === snapshot.player_id &&
      (entity.structure_type === "Spawn" || entity.structure_type === "Extension")
  );
  const drones = snapshot.entities.filter(
    (entity): entity is DroneEntity => entity.type === "drone" && "fatigue" in entity && entity.owner === snapshot.player_id && !entity.spawning && entity.fatigue === 0
  );

  let sequence = 0;
  const commands: CommandIntent[] = [];

  for (const drone of drones) {
    const carried = drone.carry?.[ENERGY] ?? 0;
    const capacity = drone.carry_capacity ?? 0;

    if (capacity > 0 && carried >= capacity) {
      const store = nearest(drone, stores);
      commands.push(store ? command(sequence, `harvester-${snapshot.tick}-${sequence++}`, actions.transfer(drone.id, store.id, ENERGY, carried)) : randomMove(snapshot.tick, sequence++, drone));
      continue;
    }

    const source = nearest(drone, sources);
    commands.push(source ? command(sequence, `harvester-${snapshot.tick}-${sequence++}`, { type: "Harvest", object_id: drone.id, target_id: source.id, resource: ENERGY } satisfies HarvestAction) : randomMove(snapshot.tick, sequence++, drone));
  }

  return commands;
}

function nearest<T extends WorldEntity>(from: WorldEntity, entities: T[]): T | undefined {
  let best: T | undefined;
  let bestDistance = Infinity;

  for (const entity of entities) {
    if (entity.position.room !== from.position.room) continue;
    const distance = Math.abs(entity.position.x - from.position.x) + Math.abs(entity.position.y - from.position.y);
    if (distance < bestDistance) {
      best = entity;
      bestDistance = distance;
    }
  }

  return best;
}

function randomMove(tick: number | bigint, sequence: number, drone: DroneEntity): CommandIntent {
  const direction = DIRECTIONS[Math.floor(Math.random() * DIRECTIONS.length)] ?? "North";
  return command(sequence, `harvester-${tick}-${sequence}`, actions.move(drone.id, direction));
}
