import {
  actions,
  bodyCost,
  command,
  hasResources,
  type CommandIntent,
  type DroneEntity,
  type HarvestAction,
  type SourceEntity,
  type StructureEntity,
  type WorldEntity,
  type WorldSnapshot
} from "@swarm/sdk-ts";

const ENERGY = "Energy";
const WORKER_BODY = ["Work"] as const;
const WORKER_COST = bodyCost([...WORKER_BODY]);

export function tick(snapshot: WorldSnapshot): CommandIntent[] {
  const spawn = findMySpawn(snapshot);
  const source = findEnergySource(snapshot.entities);
  if (!spawn || !source) return [];

  const commands: CommandIntent[] = [];
  let sequence = 0;

  const spawnEnergy = spawn.store?.[ENERGY] ?? snapshot.resources[ENERGY] ?? 0;
  if (spawnEnergy >= (WORKER_COST[ENERGY] ?? 100) && !spawn.cooldown) {
    commands.push(command(sequence++, actions.spawn(spawn.id, [...WORKER_BODY])));
  }

  for (const drone of findMyWorkers(snapshot)) {
    if (drone.spawning || drone.fatigue > 0) continue;

    const carriedEnergy = drone.carry?.[ENERGY] ?? 0;
    if (carriedEnergy >= Math.min(100, drone.carry_capacity ?? 100)) {
      commands.push(
        isNear(drone, spawn)
          ? command(sequence++, actions.transfer(drone.id, spawn.id, ENERGY, carriedEnergy))
          : command(sequence++, actions.moveTo(drone.id, spawn.position.x, spawn.position.y))
      );
      continue;
    }

    commands.push(
      isNear(drone, source)
        ? command(sequence++, { type: "Harvest", object_id: drone.id, target_id: source.id, resource: ENERGY } satisfies HarvestAction)
        : command(sequence++, actions.moveTo(drone.id, source.position.x, source.position.y))
    );
  }

  return commands;
}

export function hasEnoughEnergyForWorker(snapshot: WorldSnapshot): boolean {
  const spawn = findMySpawn(snapshot);
  const available = spawn?.store ?? snapshot.resources;
  return hasResources(available, WORKER_COST);
}

function findMySpawn(snapshot: WorldSnapshot): StructureEntity | undefined {
  return snapshot.entities.find(
    (entity): entity is StructureEntity =>
      entity.type === "structure" && "structure_type" in entity && entity.owner === snapshot.player_id && entity.structure_type === "Spawn"
  );
}

function findMyWorkers(snapshot: WorldSnapshot): DroneEntity[] {
  return snapshot.entities.filter(
    (entity): entity is DroneEntity => entity.type === "drone" && "body" in entity && entity.owner === snapshot.player_id && entity.body.includes("Work")
  );
}

function findEnergySource(entities: WorldEntity[]): SourceEntity | undefined {
  return entities.find((entity): entity is SourceEntity => entity.type === "source" && "produces" in entity && (entity.produces[ENERGY] ?? 0) > 0);
}

function isNear(a: WorldEntity, b: WorldEntity): boolean {
  return a.position.room === b.position.room && Math.max(Math.abs(a.position.x - b.position.x), Math.abs(a.position.y - b.position.y)) <= 1;
}
