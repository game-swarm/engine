import { describe, expect, it } from "vitest";
import {
  actions,
  bodyCost,
  canPublicSpectate,
  command,
  createDefaultWorldConfig,
  decodeTickInput,
  encodeTickInput,
  encodeTickResult,
  makeSnapshotForAi,
  parseCommandsFromTickOutput,
  serializeTickOutput,
  validateCommandIntents,
  validateWorldConfig,
  visibleEntities
} from "../dist/index.js";
import type { Direction, WorldEntity, WorldSnapshot } from "../dist/index.js";

describe("command intents", () => {
  it("matches engine-api ABI v2 TickResult golden bytes", () => {
    const output = encodeTickResult({
      commands: [command(7, "move-42-t99", actions.move(1001, "North"), "trace-a")],
      messages: [{ channel: "Debug", text: "queued" }]
    });
    expect([...output]).toEqual([
      2, 0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 69, 0, 0, 0,
      1, 0, 0, 0, 7, 0, 0, 0, 11, 0, 0, 0, 109, 111, 118, 101, 45, 52, 50, 45, 116, 57, 57,
      1, 7, 0, 0, 0, 116, 114, 97, 99, 101, 45, 97, 1, 0, 0, 0, 233, 3, 0, 0, 0, 0, 0, 0,
      0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 6, 0, 0, 0, 113, 117, 101, 117, 101, 100
    ]);

    const parsed = parseCommandsFromTickOutput(output);
    expect(parsed.ok).toBe(true);
    expect(parsed.value?.[0]).toMatchObject({ sequence: 7, idempotency_key: "move-42-t99", client_trace_id: "trace-a" });
  });

  it("round-trips every canonical Direction without aliases", () => {
    const directions: Direction[] = ["North", "South", "East", "West"];
    for (const [sequence, direction] of directions.entries()) {
      const output = serializeTickOutput([command(sequence, `move-${direction}`, actions.move(1001, direction))]);
      const parsed = parseCommandsFromTickOutput(output);
      expect(parsed.ok, direction).toBe(true);
      expect(parsed.value?.[0]?.action, direction).toEqual({ type: "Move", object_id: 1001n, direction });
    }
  });

  it("round-trips canonical ABI v2 TickInput bindings", () => {
    const input = {
      tick: 99n,
      player_id: 42,
      world_id: 7n,
      visible_snapshot: new Uint8Array([1, 2, 3]),
      world_config_view: { config_hash: new Uint8Array(32).fill(7), payload: new Uint8Array([4, 5]) },
      fuel_budget_hints: { fuel_remaining: 12_345n, host_calls_remaining: 77, output_bytes_remaining: 262_144 },
      message_inbox_cursor: { next_message_id: 555n }
    };
    const encoded = encodeTickInput(input);
    expect([...encoded.slice(0, 20)]).toEqual([2, 0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 89, 0, 0, 0]);
    const decoded = decodeTickInput(encoded);
    expect(decoded.ok).toBe(true);
    expect(decoded.value).toEqual(input);
  });

  it("builds and serializes valid binary CommandIntent output", () => {
    const output = serializeTickOutput([
      command(0, "test-move", actions.move(1001, "East")),
      command(1, "test-spawn", actions.spawn(1001, 2001, ["Move", "Work", "Carry"])),
      command(2, "test-claim", actions.claimController(1001, 3001))
    ]);
    const parsed = parseCommandsFromTickOutput(output);
    expect(parsed.ok).toBe(true);
    expect(parsed.value?.[0]?.action.type).toBe("Move");
  });

  it("rejects legacy JSON tick output", () => {
    const parsed = parseCommandsFromTickOutput(new TextEncoder().encode("[]"));
    expect(parsed.ok).toBe(false);
    expect(parsed.issues[0]?.code).toBe("InvalidBinaryAbi");
  });

  it("round-trips exact BigInt u64 ObjectIds through binary output", () => {
    const aboveMaxSafeInteger = 9007199254740993n;
    const maxUint64 = 18446744073709551615n;

    const output = serializeTickOutput([
      command(0, "test-large-move", actions.move(aboveMaxSafeInteger, "North")),
      command(1, "test-large-attack", actions.attack(maxUint64, aboveMaxSafeInteger))
    ]);
    expect(output).toBeInstanceOf(Uint8Array);
    expect(new TextDecoder().decode(output)).not.toContain("object_id");
    const parsed = parseCommandsFromTickOutput(output);
    expect(parsed.ok).toBe(true);
    expect(parsed.value?.[0]?.action).toMatchObject({ object_id: aboveMaxSafeInteger });
    expect(parsed.value?.[1]?.action).toMatchObject({ object_id: maxUint64, target_id: aboveMaxSafeInteger });
  });

  it("rejects BigInt ObjectIds outside the u64 range", () => {
    for (const objectId of [-1n, 18446744073709551616n]) {
      const result = validateCommandIntents([command(0, "test-invalid-object", actions.move(objectId, "North"))]);
      expect(result.ok).toBe(false);
      expect(result.issues).toContainEqual(expect.objectContaining({ path: "$[0].action.object_id", code: "InvalidObjectId" }));
      expect(() => serializeTickOutput([command(0, "test-invalid-object", actions.move(objectId, "North"))])).toThrow("must be a u64 ObjectId");
    }
  });

  it("rejects server-injected fields in untrusted CommandIntent", () => {
    const result = validateCommandIntents([{ sequence: 0, idempotency_key: "test-injected", player_id: 42, action: actions.move(1, "North") }]);
    expect(result.ok).toBe(false);
    expect(result.issues.some((issue) => issue.code === "AdditionalProperty" && issue.path === "$[0].player_id")).toBe(true);
  });

  it("rejects unknown action fields and oversized spawn bodies", () => {
    const result = validateCommandIntents([{ sequence: 0, idempotency_key: "test-oversized-body", action: { ...actions.spawn(1, 2, Array(51).fill("Move")), extra: true } }]);
    expect(result.ok).toBe(false);
    expect(result.issues.map((issue) => issue.code)).toContain("AdditionalProperty");
    expect(result.issues.map((issue) => issue.code)).toContain("BodyTooLarge");
  });

  it("rejects empty spawn bodies and non-u32 allied target players", () => {
    const result = validateCommandIntents([
      command(0, "test-empty-body", actions.spawn(1, 2, [])),
      command(1, "test-invalid-player", actions.alliedTransfer(2 ** 40, "Energy", 10))
    ]);

    expect(result.ok).toBe(false);
    expect(result.issues.map((issue) => issue.code)).toContain("BodyEmpty");
    expect(result.issues.some((issue) => issue.path === "$[1].action.target_player" && issue.code === "InvalidU32")).toBe(true);
  });

  it("rejects legacy spawn and claim controller fields", () => {
    const result = validateCommandIntents([
      { sequence: 0, idempotency_key: "test-legacy-spawn", action: { type: "Spawn", spawn_id: 1, body: ["Move"] } },
      { sequence: 1, idempotency_key: "test-legacy-claim", action: { type: "ClaimController", object_id: 1, controller_id: 2 } }
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
      actions.hack(1, 2, { damage_type: "EMP", cooldown: 3 }),
      actions.drain(1, 2, { damage_type: "Corrosive", cooldown: 3 }),
      actions.overload(1, 2, { damage_type: "Thermal", cooldown: 3 }),
      actions.debilitate(1, 2, { damage_type: "Sonic", cooldown: 3 }),
      actions.disrupt(1, 2, { damage_type: "Psionic", cooldown: 3 }),
      actions.fortify(1, 2, { cooldown: 3 }),
      actions.leech(1, 2, { damage_type: "Kinetic", cooldown: 3 }),
      actions.fabricate(1, 2, { cooldown: 3 }),
      actions.alliedTransfer(7, "Energy", 10)
    ];

    const result = validateCommandIntents(concreteActions.map((action, sequence) => command(sequence, `test-concrete-${sequence}`, action)));
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
    expect(createDefaultWorldConfig("tutorial").world.tick_interval_ms).toBe(1000);
    expect(createDefaultWorldConfig("novice").world.tick_interval_ms).toBe(3000);
    const arena = createDefaultWorldConfig("arena");
    expect(arena.world.tick_interval_ms).toBe(3000);
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

  it("rejects unsafe public spectate in novice worlds", () => {
    const config = createDefaultWorldConfig("novice");
    config.visibility.public_spectate = true;
    config.visibility.replay_privacy = "world";

    expect(canPublicSpectate(config)).toBe(false);
    expect(validateWorldConfig(config).map((issue) => issue.code)).toContain("SpectateDelayTooLow");
  });
});
