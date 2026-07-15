import type {
  Action,
  BodyPart,
  CommandIntent,
  DamageType,
  Direction,
  StructureType,
} from "./commands.js";
import type { ValidationIssue, ValidationResult } from "./types_template.js";
import {
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

const directions = new Set<Direction>(["Top", "TopRight", "BottomRight", "Bottom", "BottomLeft", "TopLeft"]);
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

export function parseTickOutput(input: string | Uint8Array): ValidationResult<CommandIntent[]> {
  const text = typeof input === "string" ? input : new TextDecoder("utf-8", { fatal: true }).decode(input);
  const bytes = new TextEncoder().encode(text).byteLength;
  if (bytes > MAX_TICK_OUTPUT_BYTES) return fail("$", "tick output exceeds 256KB", "MaxBytes");

  let value: unknown;
  try {
    value = JSON.parse(text, (_key, parsedValue, context?: { source?: string }) => {
      const source = context?.source;
      if (typeof parsedValue === "number" && source && /^\d+$/.test(source)) {
        const exact = BigInt(source);
        if (exact > BigInt(Number.MAX_SAFE_INTEGER) && exact <= MAX_UINT64) return exact;
      }
      return parsedValue;
    });
  } catch {
    return fail("$", "tick output is not valid JSON", "InvalidJson");
  }
  return validateCommandIntents(value);
}

export function serializeTickOutput(commands: CommandIntent[]): string {
  const validation = validateCommandIntents(commands);
  if (!validation.ok) {
    throw new Error(`Invalid tick output: ${validation.issues.map((issue) => `${issue.path} ${issue.message}`).join("; ")}`);
  }
  const text = stringifyJson(validation.value);
  if (new TextEncoder().encode(text).byteLength > MAX_TICK_OUTPUT_BYTES) throw new Error("Invalid tick output: exceeds 256KB");
  return text;
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
  exactKeys(value, ["sequence", "action"], path, issues);
  u32(value.sequence, `${path}.sequence`, issues);
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
    if (key !== "resource" && key !== "target_id" && key !== "damage_type" && key !== "cooldown" && !(key in value)) issue(issues, `${path}.${key}`, "field is required", "Required");
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
