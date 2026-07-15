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
      command(1, actions.spawn(1001, 2001, ["Move", "Work", "Carry"])),
      command(2, actions.claimController(1001, 3001))
    ]);
    const parsed = parseCommandsFromTickOutput(output);
    expect(parsed.ok).toBe(true);
    expect(parsed.value?.[0]?.action.type).toBe("Move");
  });

  it("serializes BigInt ObjectIds as exact unquoted u64 JSON numbers", () => {
    const aboveMaxSafeInteger = 9007199254740993n;
    const maxUint64 = 18446744073709551615n;

    const output = serializeTickOutput([
      command(0, actions.move(aboveMaxSafeInteger, "Top")),
      command(1, actions.attack(maxUint64, aboveMaxSafeInteger))
    ]);
    expect(output).toBe(
      '[{"sequence":0,"action":{"type":"Move","object_id":9007199254740993,"direction":"Top"}},{"sequence":1,"action":{"type":"Attack","object_id":18446744073709551615,"target_id":9007199254740993}}]'
    );
    const parsed = parseCommandsFromTickOutput(output);
    expect(parsed.ok).toBe(true);
    expect(parsed.value?.[0]?.action).toMatchObject({ object_id: aboveMaxSafeInteger });
    expect(parsed.value?.[1]?.action).toMatchObject({ object_id: maxUint64, target_id: aboveMaxSafeInteger });
  });

  it("rejects BigInt ObjectIds outside the u64 range", () => {
    for (const objectId of [-1n, 18446744073709551616n]) {
      const result = validateCommandIntents([command(0, actions.move(objectId, "Top"))]);
      expect(result.ok).toBe(false);
      expect(result.issues).toContainEqual(expect.objectContaining({ path: "$[0].action.object_id", code: "InvalidObjectId" }));
      expect(() => serializeTickOutput([command(0, actions.move(objectId, "Top"))])).toThrow("must be a u64 ObjectId");
    }
  });

  it("rejects server-injected fields in untrusted CommandIntent", () => {
    const result = validateCommandIntents([{ sequence: 0, player_id: 42, action: actions.move(1, "Top") }]);
    expect(result.ok).toBe(false);
    expect(result.issues.some((issue) => issue.code === "AdditionalProperty" && issue.path === "$[0].player_id")).toBe(true);
  });

  it("rejects unknown action fields and oversized spawn bodies", () => {
    const result = validateCommandIntents([{ sequence: 0, action: { ...actions.spawn(1, 2, Array(51).fill("Move")), extra: true } }]);
    expect(result.ok).toBe(false);
    expect(result.issues.map((issue) => issue.code)).toContain("AdditionalProperty");
    expect(result.issues.map((issue) => issue.code)).toContain("BodyTooLarge");
  });

  it("rejects empty spawn bodies and non-u32 allied target players", () => {
    const result = validateCommandIntents([
      command(0, actions.spawn(1, 2, [])),
      command(1, actions.alliedTransfer(2 ** 40, "Energy", 10))
    ]);

    expect(result.ok).toBe(false);
    expect(result.issues.map((issue) => issue.code)).toContain("BodyEmpty");
    expect(result.issues.some((issue) => issue.path === "$[1].action.target_player" && issue.code === "InvalidU32")).toBe(true);
  });

  it("rejects legacy spawn and claim controller fields", () => {
    const result = validateCommandIntents([
      { sequence: 0, action: { type: "Spawn", spawn_id: 1, body: ["Move"] } },
      { sequence: 1, action: { type: "ClaimController", object_id: 1, controller_id: 2 } }
    ]);
    expect(result.ok).toBe(false);
    expect(result.issues.some((issue) => issue.path === "$[0].action.object_id" && issue.code === "Required")).toBe(true);
    expect(result.issues.some((issue) => issue.path === "$[0].action.body" && issue.code === "AdditionalProperty")).toBe(true);
    expect(result.issues.some((issue) => issue.path === "$[1].action.controller_id" && issue.code === "AdditionalProperty")).toBe(true);
  });

  it("validates the concrete ActionRegistry and economy wire actions", () => {
    const concreteActions = [
      actions.recycle(1),
      actions.attack(1, 2),
      actions.rangedAttack(1, 2),
      actions.heal(1, 2),
      actions.hack(1, 2, "EMP", 3),
      actions.drain(1, 2, "Corrosive", 3),
      actions.overload(1, 2, "Thermal", 3),
      actions.debilitate(1, 2, "Sonic", 3),
      actions.disrupt(1, 2, "Psionic", 3),
      actions.fortify(1, 2, 3),
      actions.leech(1, 2, "Kinetic", 3),
      actions.fabricate(1, 2, 3),
      actions.alliedTransfer(7, "Energy", 10)
    ];

    const result = validateCommandIntents(concreteActions.map((action, sequence) => command(sequence, action)));
    expect(result.ok).toBe(true);
  });

  it("computes default body costs from the frozen IDL table", () => {
    expect(bodyCost(["Move", "Work", "Carry", "Tough"])).toEqual({ Energy: 210 });
  });
});

describe("tick and AI safety helpers", () => {
  it("wraps snapshots with explicit untrusted game data delimiters", () => {
    const snapshot: WorldSnapshot = { tick: 1, player_id: 42, entities: [], terrain: [], resources: {} };
    const text = makeSnapshotForAi(snapshot);
    expect(text).toContain("___AI_GAME_DATA_START___");
    expect(text).toContain("___AI_GAME_DATA_END___");
    expect(text).toContain('"_untrusted_game_data":true');
  });

  it("preserves BigInt u64 values in AI snapshot JSON", () => {
    const snapshot: WorldSnapshot = {
      tick: 9007199254740993n,
      player_id: 42,
      entities: [{ id: 18446744073709551615n, type: "source", position: { x: 0, y: 0, room: 1 } }],
      terrain: [],
      resources: {}
    };
    const text = makeSnapshotForAi(snapshot);
    expect(text).toContain('"tick":9007199254740993');
    expect(text).toContain('"id":18446744073709551615');
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
