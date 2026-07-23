import type {
  Action,
  BodyPart,
  CommandIntent,
  DamageType,
  Direction,
  PlayerMessage,
  StructureType,
  TickInput,
  TickResult,
} from "./commands.js";
import type { ValidationIssue, ValidationResult } from "./types_template.js";
import {
  ABI_VERSION,
  MAX_BODY_PARTS,
  MAX_COMMANDS_PER_PLAYER,
  MAX_JSON_DEPTH,
  MAX_TICK_OUTPUT_BYTES,
} from "./commands.js";
// Room bounds — may be world-config dependent; keep stable defaults
const MIN_ROOM_COORD = -127;
const MAX_ROOM_COORD = 127;
const MAX_STRING_LENGTH = 1024;
const MAX_UINT64 = (1n << 64n) - 1n;

const actionFields: Record<string, readonly string[]> = {
  Move: ["type", "object_id", "direction"],
  Harvest: ["type", "object_id", "target_id", "resource"],
  Transfer: ["type", "object_id", "target_id", "resource", "amount"],
  Withdraw: ["type", "object_id", "target_id", "resource", "amount"],
  Build: ["type", "object_id", "x", "y", "structure"],
  Repair: ["type", "object_id", "target_id"],
  Attack: ["type", "object_id", "target_id"],
  RangedAttack: ["type", "object_id", "target_id"],
  Heal: ["type", "object_id", "target_id"],
  ClaimController: ["type", "object_id", "target_id"],
  Spawn: ["type", "object_id", "spawn_id", "body_parts"],
  Recycle: ["type", "object_id"],
  Hack: ["type", "object_id", "target_id", "damage_type", "cooldown"],
  Drain: ["type", "object_id", "target_id", "damage_type", "cooldown"],
  Overload: ["type", "object_id", "target_id", "damage_type", "cooldown"],
  Debilitate: ["type", "object_id", "target_id", "damage_type", "cooldown"],
  Disrupt: ["type", "object_id", "target_id", "damage_type", "cooldown"],
  Fortify: ["type", "object_id", "target_id", "cooldown"],
  Leech: ["type", "object_id", "target_id", "damage_type", "cooldown"],
  Fabricate: ["type", "object_id", "target_id", "cooldown"],
  TransferToGlobal: ["type", "resource", "amount"],
  TransferFromGlobal: ["type", "resource", "amount"],
  AlliedTransfer: ["type", "target_player", "resource", "amount"]
};

const directions = new Set<Direction>(["North", "South", "East", "West"]);
const bodyParts = new Set<BodyPart>(["Move", "Work", "Carry", "Attack", "RangedAttack", "Heal", "Claim", "Tough"]);
const damageTypes = new Set<DamageType>(["Kinetic", "Thermal", "EMP", "Sonic", "Corrosive", "Psionic"]);
const structureTypes = new Set<StructureType>([
  "Spawn",
  "Extension",
  "Tower",
  "Storage",
  "Link",
  "Extractor",
  "Lab",
  "Terminal",
  "Nuker",
  "Observer",
  "PowerSpawn",
  "Factory",
  "Depot"
]);

export function parseTickOutput(input: Uint8Array): ValidationResult<CommandIntent[]> {
  const result = decodeTickResult(input);
  return result.ok
    ? { ok: true, value: result.value!.commands, issues: [] }
    : { ok: false, issues: result.issues };
}

export function serializeTickOutput(commands: CommandIntent[]): Uint8Array {
  const validation = validateCommandIntents(commands);
  if (!validation.ok) {
    throw new Error(`Invalid tick output: ${validation.issues.map((issue) => `${issue.path} ${issue.message}`).join("; ")}`);
  }
  return encodeTickResult({ commands: validation.value ?? [], messages: [] });
}

const GAME_API_SCHEMA_VERSION = 4;
const CANONICAL_CODEC_VERSION = 1;
const TICK_INPUT_TAG = 1;
const TICK_RESULT_TAG = 2;
const HEADER_BYTES = 20;

export function encodeTickInput(input: TickInput): Uint8Array {
  const payload = new BinaryWriter();
  payload.u64(input.tick);
  payload.u32(input.player_id);
  payload.u64(input.world_id);
  payload.bytes(input.visible_snapshot);
  if (!(input.world_config_view.config_hash instanceof Uint8Array) || input.world_config_view.config_hash.byteLength !== 32) {
    throw new Error("WorldConfigView config_hash must contain exactly 32 bytes");
  }
  payload.raw(input.world_config_view.config_hash);
  payload.bytes(input.world_config_view.payload);
  payload.u64(input.fuel_budget_hints.fuel_remaining);
  payload.u32(input.fuel_budget_hints.host_calls_remaining);
  payload.u32(input.fuel_budget_hints.output_bytes_remaining);
  payload.u64(input.message_inbox_cursor.next_message_id);
  return encodeEnvelope(TICK_INPUT_TAG, payload.finish(), "tick input");
}

export function decodeTickInput(input: Uint8Array): ValidationResult<TickInput> {
  if (!(input instanceof Uint8Array)) return fail("$", "tick input must be binary ABI v2 bytes", "BinaryRequired");
  if (input.byteLength > MAX_TICK_OUTPUT_BYTES) return fail("$", "tick input exceeds 256KB", "MaxBytes");
  try {
    const reader = decodeEnvelope(input, TICK_INPUT_TAG, "TickInput");
    const result: TickInput = {
      tick: reader.u64(),
      player_id: reader.u32(),
      world_id: reader.u64(),
      visible_snapshot: reader.bytes(),
      world_config_view: { config_hash: reader.raw(32), payload: reader.bytes() },
      fuel_budget_hints: {
        fuel_remaining: reader.u64(),
        host_calls_remaining: reader.u32(),
        output_bytes_remaining: reader.u32()
      },
      message_inbox_cursor: { next_message_id: reader.u64() }
    };
    reader.finish();
    if (!bytesEqual(encodeTickInput(result), input)) throw new Error("tick input is not canonical ABI v2");
    return { ok: true, value: result, issues: [] };
  } catch (error) {
    return fail("$", error instanceof Error ? error.message : "invalid binary tick input", "InvalidBinaryAbi");
  }
}

export function encodeTickResult(result: TickResult): Uint8Array {
  const validation = validateCommandIntents(result.commands);
  if (!validation.ok) {
    throw new Error(`Invalid tick output: ${validation.issues.map((item) => `${item.path} ${item.message}`).join("; ")}`);
  }
  const payload = new BinaryWriter();
  payload.vector(result.commands, (command) => encodeCommandIntent(payload, command));
  payload.vector(result.messages, (message) => encodePlayerMessage(payload, message));
  const payloadBytes = payload.finish();
  return encodeEnvelope(TICK_RESULT_TAG, payloadBytes, "tick output");
}

export function decodeTickResult(input: Uint8Array): ValidationResult<TickResult> {
  if (!(input instanceof Uint8Array)) return fail("$", "tick output must be binary ABI v2 bytes", "BinaryRequired");
  if (input.byteLength > MAX_TICK_OUTPUT_BYTES) return fail("$", "tick output exceeds 256KB", "MaxBytes");
  try {
    const reader = decodeEnvelope(input, TICK_RESULT_TAG, "TickResult");
    const commands = reader.vector(() => decodeCommandIntent(reader), MAX_COMMANDS_PER_PLAYER);
    const messages = reader.vector(() => decodePlayerMessage(reader));
    reader.finish();
    const result: TickResult = { commands, messages };
    const validation = validateCommandIntents(commands);
    if (!validation.ok) return { ok: false, issues: validation.issues };
    if (!bytesEqual(encodeTickResult(result), input)) throw new Error("tick output is not canonical ABI v2");
    return { ok: true, value: result, issues: [] };
  } catch (error) {
    return fail("$", error instanceof Error ? error.message : "invalid binary tick output", "InvalidBinaryAbi");
  }
}

export function stringifyJson(value: unknown): string {
  const serialized = stringifyJsonValue(value);
  if (serialized === undefined) throw new TypeError("value is not JSON serializable");
  return serialized;
}

function stringifyJsonValue(value: unknown): string | undefined {
  if (value === null) return "null";

  switch (typeof value) {
    case "bigint":
      if (value < 0n || value > MAX_UINT64) throw new RangeError("bigint value is outside the u64 range");
      return value.toString();
    case "boolean":
    case "number":
    case "string":
      return JSON.stringify(value);
    case "object":
      if (Array.isArray(value)) {
        return `[${Array.from(value, (item) => stringifyJsonValue(item) ?? "null").join(",")}]`;
      }
      return `{${Object.entries(value)
        .flatMap(([key, item]) => {
          const serialized = stringifyJsonValue(item);
          return serialized === undefined ? [] : [`${JSON.stringify(key)}:${serialized}`];
        })
        .join(",")}}`;
    default:
      return undefined;
  }
}

function encodeCommandIntent(writer: BinaryWriter, command: CommandIntent): void {
  writer.u32(command.sequence);
  writer.string(command.idempotency_key);
  writer.option(command.client_trace_id, (value) => writer.string(value));
  encodeAction(writer, command.action);
}

function decodeCommandIntent(reader: BinaryReader): CommandIntent {
  const sequence = reader.u32();
  const idempotency_key = reader.string();
  const client_trace_id = reader.option(() => reader.string());
  const action = decodeAction(reader);
  return client_trace_id === undefined
    ? { sequence, idempotency_key, action }
    : { sequence, idempotency_key, client_trace_id, action };
}

function encodePlayerMessage(writer: BinaryWriter, message: PlayerMessage): void {
  if (message.channel !== "Player" && message.channel !== "Debug") throw new Error("unknown PlayerMessage channel");
  if (typeof message.text !== "string") throw new Error("PlayerMessage text must be a string");
  writer.u32(message.channel === "Player" ? 0 : 1);
  writer.string(message.text);
}

function decodePlayerMessage(reader: BinaryReader): PlayerMessage {
  const channelTag = reader.u32();
  if (channelTag !== 0 && channelTag !== 1) throw new Error(`unknown MessageChannel discriminant ${channelTag}`);
  return { channel: channelTag === 0 ? "Player" : "Debug", text: reader.string() };
}

function encodeAction(writer: BinaryWriter, action: Action): void {
  switch (action.type) {
    case "Move":
      writer.u32(1); writer.u64(action.object_id); writer.u32(directionTag(action.direction)); return;
    case "Harvest":
      writer.u32(2); writer.u64(action.object_id); writer.u64(action.target_id); writer.option(action.resource, (value) => writer.string(value)); return;
    case "Transfer":
      writer.u32(3); writer.u64(action.object_id); writer.u64(action.target_id); writer.string(action.resource); writer.u32(action.amount); return;
    case "Withdraw":
      writer.u32(4); writer.u64(action.object_id); writer.u64(action.target_id); writer.string(action.resource); writer.u32(action.amount); return;
    case "ClaimController":
      writer.u32(6); writer.u64(action.object_id); writer.u64(action.target_id); return;
    case "Spawn":
      writer.u32(7); writer.u64(action.object_id); writer.u64(action.spawn_id); writer.vector(action.body_parts, (part) => writer.u32(bodyPartTag(part))); return;
    case "Recycle":
      writer.u32(8); writer.u64(action.object_id); return;
    case "Build":
      writer.u32(9); writer.u64(action.object_id); writer.string(action.structure); writer.i32(action.x); writer.i32(action.y); return;
    case "Repair":
      writer.u32(10); writer.u64(action.object_id); writer.u64(action.target_id); return;
    case "UpgradeController":
      writer.u32(11); writer.u64(action.object_id); writer.u64(action.target_id); return;
    case "TransferToGlobal":
      writer.u32(12); writer.string(action.resource); writer.u32(action.amount); return;
    case "TransferFromGlobal":
      writer.u32(13); writer.string(action.resource); writer.u32(action.amount); return;
    case "AlliedTransfer":
      writer.u32(14); writer.u32(action.target_player); writer.string(action.resource); writer.u32(action.amount); return;
    default:
      encodeGenericAction(writer, action);
  }
}

function encodeGenericAction(writer: BinaryWriter, action: Exclude<Action, { type: "Move" | "Harvest" | "Transfer" | "Withdraw" | "ClaimController" | "Spawn" | "Recycle" | "Build" | "Repair" | "UpgradeController" | "TransferToGlobal" | "TransferFromGlobal" | "AlliedTransfer" }>): void {
  const record = action as unknown as Record<string, unknown>;
  writer.u32(5);
  writer.string(action.type);
  writer.u64(record.object_id as number | bigint);
  writer.raw(new Uint8Array(32));
  const payload = Object.fromEntries(Object.entries(record).filter(([key, value]) => key !== "type" && key !== "object_id" && value !== undefined));
  writer.bytes(new TextEncoder().encode(stringifyJson(payload)));
}

function decodeAction(reader: BinaryReader): Action {
  const tag = reader.u32();
  switch (tag) {
    case 1: return { type: "Move", object_id: reader.u64(), direction: directionFromTag(reader.u32()) };
    case 2: {
      const object_id = reader.u64(); const target_id = reader.u64(); const resource = reader.option(() => reader.string());
      return resource === undefined ? { type: "Harvest", object_id, target_id } : { type: "Harvest", object_id, target_id, resource };
    }
    case 3: return { type: "Transfer", object_id: reader.u64(), target_id: reader.u64(), resource: reader.string(), amount: reader.u32() };
    case 4: return { type: "Withdraw", object_id: reader.u64(), target_id: reader.u64(), resource: reader.string(), amount: reader.u32() };
    case 5: return decodeGenericAction(reader);
    case 6: return { type: "ClaimController", object_id: reader.u64(), target_id: reader.u64() };
    case 7: return { type: "Spawn", object_id: reader.u64(), spawn_id: reader.u64(), body_parts: reader.vector(() => bodyPartFromTag(reader.u32())) };
    case 8: return { type: "Recycle", object_id: reader.u64() };
    case 9: return { type: "Build", object_id: reader.u64(), structure: reader.string() as StructureType, x: reader.i32(), y: reader.i32() };
    case 10: return { type: "Repair", object_id: reader.u64(), target_id: reader.u64() };
    case 11: return { type: "UpgradeController", object_id: reader.u64(), target_id: reader.u64() };
    case 12: return { type: "TransferToGlobal", resource: reader.string(), amount: reader.u32() };
    case 13: return { type: "TransferFromGlobal", resource: reader.string(), amount: reader.u32() };
    case 14: return { type: "AlliedTransfer", target_player: reader.u32(), resource: reader.string(), amount: reader.u32() };
    default: throw new Error(`unknown CommandAction discriminant ${tag}`);
  }
}

function decodeGenericAction(reader: BinaryReader): Action {
  const type = reader.string();
  const object_id = reader.u64();
  reader.raw(32);
  const payloadBytes = reader.bytes();
  let payload: unknown;
  try {
    payload = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(payloadBytes), (_key, value, context?: { source?: string }) => {
      if (typeof value === "number" && context?.source && /^\d+$/.test(context.source)) {
        const exact = BigInt(context.source);
        if (exact > BigInt(Number.MAX_SAFE_INTEGER) && exact <= MAX_UINT64) return exact;
      }
      return value;
    });
  } catch { throw new Error("invalid generic Action payload"); }
  if (!isRecord(payload)) throw new Error("generic Action payload must be an object");
  return { type, object_id, ...payload } as unknown as Action;
}

function directionTag(direction: Direction): number {
  if (direction === "North") return 0;
  if (direction === "South") return 1;
  if (direction === "East") return 2;
  if (direction === "West") return 3;
  throw new Error(`unknown Direction ${String(direction)}`);
}

function directionFromTag(tag: number): Direction {
  if (tag === 0) return "North";
  if (tag === 1) return "South";
  if (tag === 2) return "East";
  if (tag === 3) return "West";
  throw new Error(`unknown Direction discriminant ${tag}`);
}

const bodyPartTags: Record<BodyPart, number> = { Move: 1, Work: 2, Carry: 3, Attack: 4, RangedAttack: 5, Heal: 6, Claim: 7, Tough: 8 };
const bodyPartsByTag = ["Move", "Work", "Carry", "Attack", "RangedAttack", "Heal", "Claim", "Tough"] as const;
function bodyPartTag(part: BodyPart): number { return bodyPartTags[part]; }
function bodyPartFromTag(tag: number): BodyPart {
  const part = bodyPartsByTag[tag - 1];
  if (!part) throw new Error(`unknown BodyPart discriminant ${tag}`);
  return part;
}

class BinaryWriter {
  private readonly output: number[] = [];
  u32(value: number): void { this.number(value, 4, false); }
  i32(value: number): void { this.number(value, 4, true); }
  u64(value: number | bigint): void {
    const exact = typeof value === "bigint" ? value : BigInt(value);
    if (exact < 0n || exact > MAX_UINT64) throw new Error("u64 value is out of range");
    for (let shift = 0n; shift < 64n; shift += 8n) this.output.push(Number((exact >> shift) & 0xffn));
  }
  string(value: string): void { this.bytes(new TextEncoder().encode(value)); }
  bytes(value: Uint8Array): void { this.u32(value.byteLength); this.raw(value); }
  raw(value: Uint8Array): void { this.output.push(...value); }
  option<T>(value: T | undefined, encode: (value: T) => void): void { this.output.push(value === undefined ? 0 : 1); if (value !== undefined) encode(value); }
  vector<T>(values: readonly T[], encode: (value: T) => void): void { this.u32(values.length); values.forEach(encode); }
  finish(): Uint8Array { return Uint8Array.from(this.output); }
  private number(value: number, size: number, signed: boolean): void {
    if (!Number.isInteger(value)) throw new Error("integer value required");
    const bytes = new Uint8Array(size); new DataView(bytes.buffer).setInt32(0, value, true);
    if (!signed && value < 0) throw new Error("unsigned integer value required");
    this.raw(bytes);
  }
}

class BinaryReader {
  private offset = 0;
  constructor(private readonly input: Uint8Array) {}
  u32(): number { return this.number(false); }
  i32(): number { return this.number(true); }
  u64(): bigint {
    const bytes = this.raw(8); let value = 0n;
    for (let index = 0; index < 8; index++) value |= BigInt(bytes[index] ?? 0) << BigInt(index * 8);
    return value;
  }
  string(): string { return new TextDecoder("utf-8", { fatal: true }).decode(this.bytes()); }
  bytes(): Uint8Array { return this.raw(this.u32()); }
  raw(length: number): Uint8Array { if (length < 0 || this.offset + length > this.input.byteLength) throw new Error("unexpected end of ABI payload"); const value = this.input.slice(this.offset, this.offset + length); this.offset += length; return value; }
  option<T>(decode: () => T): T | undefined { const tag = this.raw(1)[0]; if (tag === 0) return undefined; if (tag !== 1) throw new Error(`invalid option tag ${tag}`); return decode(); }
  vector<T>(decode: () => T, max = 16_384): T[] { const length = this.u32(); if (length > max) throw new Error(`vector length ${length} exceeds ${max}`); return Array.from({ length }, decode); }
  expectU32(expected: number, field: string): void { const actual = this.u32(); if (actual !== expected) throw new Error(`${field} mismatch: expected ${expected}, got ${actual}`); }
  finish(): void { if (this.offset !== this.input.byteLength) throw new Error("trailing bytes in ABI payload"); }
  private number(signed: boolean): number { const bytes = this.raw(4); return signed ? new DataView(bytes.buffer, bytes.byteOffset, 4).getInt32(0, true) : new DataView(bytes.buffer, bytes.byteOffset, 4).getUint32(0, true); }
}

function bytesEqual(left: Uint8Array, right: Uint8Array): boolean {
  return left.byteLength === right.byteLength && left.every((byte, index) => byte === right[index]);
}

function encodeEnvelope(messageTag: number, payload: Uint8Array, field: string): Uint8Array {
  const writer = new BinaryWriter();
  writer.u32(ABI_VERSION);
  writer.u32(GAME_API_SCHEMA_VERSION);
  writer.u32(CANONICAL_CODEC_VERSION);
  writer.u32(messageTag);
  writer.u32(payload.byteLength);
  writer.raw(payload);
  const output = writer.finish();
  if (output.byteLength > MAX_TICK_OUTPUT_BYTES) throw new Error(`Invalid ${field}: exceeds 256KB`);
  return output;
}

function decodeEnvelope(input: Uint8Array, messageTag: number, messageName: string): BinaryReader {
  const reader = new BinaryReader(input);
  reader.expectU32(ABI_VERSION, "ABI version");
  reader.expectU32(GAME_API_SCHEMA_VERSION, "game API schema version");
  reader.expectU32(CANONICAL_CODEC_VERSION, "canonical codec version");
  reader.expectU32(messageTag, `${messageName} message tag`);
  const payloadLength = reader.u32();
  if (payloadLength !== input.byteLength - HEADER_BYTES) throw new Error(`${messageName} payload length mismatch`);
  return reader;
}

export function validateCommandIntents(value: unknown): ValidationResult<CommandIntent[]> {
  const issues: ValidationIssue[] = [];
  if (!Array.isArray(value)) return fail("$", "tick output must be an array", "ArrayRequired");
  if (value.length > MAX_COMMANDS_PER_PLAYER) issue(issues, "$", `more than ${MAX_COMMANDS_PER_PLAYER} commands`, "MaxItems");
  if (jsonDepth(value) > MAX_JSON_DEPTH) issue(issues, "$", `JSON depth exceeds ${MAX_JSON_DEPTH}`, "MaxDepth");
  value.forEach((item, index) => validateCommandIntent(item, `$[${index}]`, issues));
  if (issues.length > 0) return { ok: false, issues };
  return { ok: true, value: value as CommandIntent[], issues };
}

export function validateCommandIntent(value: unknown, path = "$", issues: ValidationIssue[] = []): ValidationIssue[] {
  if (!isRecord(value)) {
    issue(issues, path, "command must be an object", "ObjectRequired");
    return issues;
  }
  exactKeys(value, ["sequence", "idempotency_key", "client_trace_id", "action"], path, issues);
  u32(value.sequence, `${path}.sequence`, issues);
  boundedString(value.idempotency_key, `${path}.idempotency_key`, issues);
  if (value.client_trace_id !== undefined) {
    boundedString(value.client_trace_id, `${path}.client_trace_id`, issues);
  }
  validateAction(value.action, `${path}.action`, issues);
  return issues;
}

export function validateAction(value: unknown, path = "$", issues: ValidationIssue[] = []): ValidationIssue[] {
  if (!isRecord(value)) {
    issue(issues, path, "action must be an object", "ObjectRequired");
    return issues;
  }
  if (typeof value.type !== "string" || !(value.type in actionFields)) {
    issue(issues, `${path}.type`, "unknown action type", "InvalidActionType");
    return issues;
  }
  exactKeys(value, actionFields[value.type] ?? [], path, issues);
  switch (value.type as string) {
    case "Move":
      objectId(value.object_id, `${path}.object_id`, issues);
      enumValue(value.direction, directions, `${path}.direction`, issues);
      break;
    case "Harvest":
      objectId(value.object_id, `${path}.object_id`, issues);
      objectId(value.target_id, `${path}.target_id`, issues);
      if (value.resource !== undefined) boundedString(value.resource, `${path}.resource`, issues);
      break;
    case "Transfer":
    case "Withdraw":
      objectId(value.object_id, `${path}.object_id`, issues);
      objectId(value.target_id, `${path}.target_id`, issues);
      boundedString(value.resource, `${path}.resource`, issues);
      u32(value.amount, `${path}.amount`, issues);
      break;
    case "Build":
      objectId(value.object_id, `${path}.object_id`, issues);
      coord(value.x, `${path}.x`, issues);
      coord(value.y, `${path}.y`, issues);
      enumValue(value.structure, structureTypes, `${path}.structure`, issues);
      break;
    case "Repair":
    case "Attack":
    case "RangedAttack":
    case "Heal":
      objectId(value.object_id, `${path}.object_id`, issues);
      objectId(value.target_id, `${path}.target_id`, issues);
      break;
    case "Spawn":
      objectId(value.object_id, `${path}.object_id`, issues);
      objectId(value.spawn_id, `${path}.spawn_id`, issues);
      if (!Array.isArray(value.body_parts)) issue(issues, `${path}.body_parts`, "body_parts must be an array", "ArrayRequired");
      else {
        if (value.body_parts.length === 0) issue(issues, `${path}.body_parts`, "body_parts must not be empty", "BodyEmpty");
        if (value.body_parts.length > MAX_BODY_PARTS) issue(issues, `${path}.body_parts`, `body_parts exceeds ${MAX_BODY_PARTS} parts`, "BodyTooLarge");
        value.body_parts.forEach((part, index) => enumValue(part, bodyParts, `${path}.body_parts[${index}]`, issues));
      }
      break;
    case "ClaimController":
      objectId(value.object_id, `${path}.object_id`, issues);
      objectId(value.target_id, `${path}.target_id`, issues);
      break;
    case "Recycle":
      objectId(value.object_id, `${path}.object_id`, issues);
      break;
    case "Hack":
    case "Drain":
    case "Overload":
    case "Debilitate":
    case "Disrupt":
    case "Leech":
      objectId(value.object_id, `${path}.object_id`, issues);
      objectId(value.target_id, `${path}.target_id`, issues);
      if (value.damage_type !== undefined) enumValue(value.damage_type, damageTypes, `${path}.damage_type`, issues);
      if (value.cooldown !== undefined) u32(value.cooldown, `${path}.cooldown`, issues);
      break;
    case "Fortify":
    case "Fabricate":
      objectId(value.object_id, `${path}.object_id`, issues);
      objectId(value.target_id, `${path}.target_id`, issues);
      if (value.cooldown !== undefined) u32(value.cooldown, `${path}.cooldown`, issues);
      break;
    case "TransferToGlobal":
    case "TransferFromGlobal":
      boundedString(value.resource, `${path}.resource`, issues);
      u32(value.amount, `${path}.amount`, issues);
      break;
    case "AlliedTransfer":
      u32(value.target_player, `${path}.target_player`, issues);
      boundedString(value.resource, `${path}.resource`, issues);
      u32(value.amount, `${path}.amount`, issues);
      break;
  }
  return issues;
}

export function jsonDepth(value: unknown): number {
  if (value === null || typeof value !== "object") return 0;
  if (Array.isArray(value)) return 1 + Math.max(0, ...value.map(jsonDepth));
  return 1 + Math.max(0, ...Object.values(value).map(jsonDepth));
}

function exactKeys(value: Record<string, unknown>, allowed: readonly string[], path: string, issues: ValidationIssue[]): void {
  for (const key of Object.keys(value)) {
    if (!allowed.includes(key)) issue(issues, `${path}.${key}`, "field is not allowed", "AdditionalProperty");
  }
  for (const key of allowed) {
    if (key !== "resource" && key !== "target_id" && key !== "damage_type" && key !== "cooldown" && key !== "client_trace_id" && !(key in value)) issue(issues, `${path}.${key}`, "field is required", "Required");
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function u32(value: unknown, path: string, issues: ValidationIssue[]): void {
  if (!Number.isInteger(value) || (value as number) < 0 || (value as number) > 0xffffffff) issue(issues, path, "must be a u32", "InvalidU32");
}

function objectId(value: unknown, path: string, issues: ValidationIssue[]): void {
  const validNumber = Number.isSafeInteger(value) && (value as number) >= 0;
  const validBigint = typeof value === "bigint" && value >= 0n && value <= MAX_UINT64;
  if (!validNumber && !validBigint) issue(issues, path, "must be a u64 ObjectId", "InvalidObjectId");
}

function coord(value: unknown, path: string, issues: ValidationIssue[]): void {
  if (!Number.isInteger(value) || (value as number) < MIN_ROOM_COORD || (value as number) > MAX_ROOM_COORD) {
    issue(issues, path, `must be an i32 room coordinate in [${MIN_ROOM_COORD}, ${MAX_ROOM_COORD}]`, "InvalidCoordinate");
  }
}

function boundedString(value: unknown, path: string, issues: ValidationIssue[]): void {
  if (typeof value !== "string" || value.length === 0 || value.length > MAX_STRING_LENGTH) issue(issues, path, "must be a non-empty bounded string", "InvalidString");
}

function enumValue<T extends string>(value: unknown, allowed: ReadonlySet<T>, path: string, issues: ValidationIssue[]): void {
  if (typeof value !== "string" || !allowed.has(value as T)) issue(issues, path, "invalid enum value", "InvalidEnum");
}

function issue(issues: ValidationIssue[], path: string, message: string, code: string): void {
  issues.push({ path, message, code });
}

function fail<T>(path: string, message: string, code: string): ValidationResult<T> {
  return { ok: false, issues: [{ path, message, code }] };
}
