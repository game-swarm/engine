import { describe, expect, it } from "vitest";
import { parseCommandsFromTickOutput, serializeTickOutput, type WorldSnapshot } from "../dist/index.js";
import { hasEnoughEnergyForWorker, tick } from "../examples/starter-bot/src/bot.js";

const baseSnapshot: WorldSnapshot = {
  tick: 1,
  player_id: 42,
  terrain: [],
  resources: {},
  entities: [
    {
      id: 100,
      type: "structure",
      owner: 42,
      structure_type: "Spawn",
      position: { x: 0, y: 0, room: 1 },
      store: { Energy: 100 }
    },
    {
      id: 200,
      type: "source",
      position: { x: 1, y: 0, room: 1 },
      produces: { Energy: 10 },
      capacity: 1000,
      ticks_to_regeneration: 0
    }
  ]
};

describe("starter bot", () => {
  it("spawns a worker once the spawn has 100 Energy", () => {
    const snapshot: WorldSnapshot = {
      ...baseSnapshot,
      entities: [
        ...baseSnapshot.entities,
        {
          id: 150,
          type: "drone",
          owner: 42,
          position: { x: 0, y: 1, room: 1 },
          body: ["Work"],
          fatigue: 0,
          carry: { Energy: 0 },
          carry_capacity: 100
        }
      ]
    };
    const commands = tick(snapshot);
    expect(hasEnoughEnergyForWorker(baseSnapshot)).toBe(true);
    expect(commands[0]).toEqual({ sequence: 0, idempotency_key: "starter-1-0", action: { type: "Spawn", object_id: 150, spawn_id: 100, body_parts: ["Work"] } });
  });

  it("harvests, returns, and serializes valid SDK command output", () => {
    const snapshot: WorldSnapshot = {
      ...baseSnapshot,
      entities: [
        ...baseSnapshot.entities,
        {
          id: 300,
          type: "drone",
          owner: 42,
          position: { x: 1, y: 1, room: 1 },
          body: ["Work"],
          fatigue: 0,
          carry: { Energy: 0 },
          carry_capacity: 100
        },
        {
          id: 301,
          type: "drone",
          owner: 42,
          position: { x: 0, y: 1, room: 1 },
          body: ["Work"],
          fatigue: 0,
          carry: { Energy: 100 },
          carry_capacity: 100
        }
      ]
    };

    const output = serializeTickOutput(tick(snapshot));
    const parsed = parseCommandsFromTickOutput(output);

    expect(parsed.ok).toBe(true);
    expect(parsed.value?.map((intent) => intent.action.type)).toEqual(["Spawn", "Harvest", "Transfer"]);
  });
});
