import { describe, expect, it } from "vitest";
import {
  actions,
  bodyCost,
  canPublicSpectate,
  command,
  createDefaultWorldConfig,
  makeSnapshotForAi,
  parseCommandsFromTickOutput,
  serializeTickOutput,
  validateCommandIntents,
  validateWorldConfig,
  visibleEntities
} from "../src/index.js";
import type { WorldEntity, WorldSnapshot } from "../src/index.js";

describe("command intents", () => {
  it("builds and serializes valid CommandIntent JSON", () => {
    const output = serializeTickOutput([
      command(0, actions.move(1001, "TopRight")),
      command(1, actions.spawn(2001, ["Move", "Work", "Carry"]))
    ]);
    const parsed = parseCommandsFromTickOutput(output);
    expect(parsed.ok).toBe(true);
    expect(parsed.value?.[0]?.action.type).toBe("Move");
  });

  it("rejects server-injected fields in untrusted CommandIntent", () => {
    const result = validateCommandIntents([{ sequence: 0, player_id: 42, action: actions.move(1, "Top") }]);
    expect(result.ok).toBe(false);
    expect(result.issues.some((issue) => issue.code === "AdditionalProperty" && issue.path === "$[0].player_id")).toBe(true);
  });

  it("rejects unknown action fields and oversized spawn bodies", () => {
    const result = validateCommandIntents([{ sequence: 0, action: { ...actions.spawn(1, Array(51).fill("Move")), extra: true } }]);
    expect(result.ok).toBe(false);
    expect(result.issues.map((issue) => issue.code)).toContain("AdditionalProperty");
    expect(result.issues.map((issue) => issue.code)).toContain("BodyTooLarge");
  });

  it("computes default body costs from the frozen IDL table", () => {
    expect(bodyCost(["Move", "Work", "Carry", "Tough"])).toEqual({ Energy: 210 });
  });
});

describe("tick and AI safety helpers", () => {
  it("wraps snapshots with explicit untrusted game data delimiters", () => {
    const snapshot: WorldSnapshot = { tick: 1, player_id: 42, entities: [], terrain: [], resources: {} };
    const text = makeSnapshotForAi(snapshot);
    expect(text).toContain("|||GAME_DATA|||");
    expect(text).toContain("|||END_GAME_DATA|||");
    expect(text).toContain('"_untrusted_game_data":true');
  });
});

describe("visibility", () => {
  const entities: WorldEntity[] = [
    { id: 1, type: "drone", owner: 1, position: { x: 0, y: 0, room: 1 }, body: ["Move"], fatigue: 0 },
    { id: 2, type: "drone", owner: 2, position: { x: 3, y: 0, room: 1 }, body: ["Move"], fatigue: 0 },
    { id: 3, type: "drone", owner: 2, position: { x: 8, y: 0, room: 1 }, body: ["Move"], fatigue: 0 }
  ];

  it("keeps owned entities visible and filters enemies by vision range", () => {
    expect(visibleEntities(entities, 1, 10).map((entity) => entity.id)).toEqual([1, 2]);
  });

  it("allows full-information mode without changing snapshot fog contract helpers", () => {
    expect(visibleEntities(entities, 1, 10, false).map((entity) => entity.id)).toEqual([1, 2, 3]);
  });
});

describe("world rules", () => {
  it("creates valid default world and arena configs", () => {
    expect(validateWorldConfig(createDefaultWorldConfig("persistent"))).toEqual([]);
    const arena = createDefaultWorldConfig("arena");
    expect(arena.visibility.fog_of_war).toBe(false);
    expect(canPublicSpectate(arena)).toBe(true);
  });

  it("rejects unsafe public spectate in persistent worlds", () => {
    const config = createDefaultWorldConfig("persistent");
    config.visibility.public_spectate = true;
    config.visibility.replay_privacy = "world";
    config.visibility.spectate_delay = 0;
    expect(canPublicSpectate(config)).toBe(false);
    expect(validateWorldConfig(config).map((issue) => issue.code)).toContain("SpectateDelayTooLow");
  });
});
