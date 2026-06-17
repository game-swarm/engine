const ENERGY = "Energy";
let output = "[]";

export function alloc(len: i32): i32 {
  return changetype<i32>(heap.alloc(len));
}

export function free(ptr: i32): void {
  heap.free(changetype<usize>(ptr));
}

export function result_len(): i32 {
  return String.UTF8.byteLength(output);
}

export function tick(snapshot_ptr: i32, snapshot_len: i32): i32 {
  const snapshot = String.UTF8.decodeUnsafe(snapshot_ptr, snapshot_len);
  output = buildCommands(snapshot);
  const bytes = String.UTF8.encode(output, false);
  const len = bytes.byteLength;
  const ptr = heap.alloc(len);
  memory.copy(changetype<usize>(ptr), changetype<usize>(bytes), len);
  return changetype<i32>(ptr);
}

function buildCommands(snapshot: string): string {
  const playerId = numberField(snapshot, "player_id");
  const spawn = findOwnedEntity(snapshot, playerId, "structure", "Spawn");
  const source = findEntity(snapshot, "source");
  if (spawn.length == 0 || source.length == 0) return "[]";

  const commands = new Array<string>();
  let sequence = 0;
  const spawnEnergy = storeEnergy(spawn);
  const spawnCooldown = numberField(spawn, "cooldown");
  if (spawnEnergy >= 100 && spawnCooldown == 0) {
    commands.push('{"sequence":' + sequence.toString() + ',"action":{"type":"Spawn","spawn_id":' + idField(spawn).toString() + ',"body":["Work"]}}');
    sequence += 1;
  }

  let cursor = 0;
  while (true) {
    const droneStart = snapshot.indexOf('{"id":', cursor);
    if (droneStart < 0) break;
    const entity = objectAt(snapshot, droneStart);
    cursor = droneStart + entity.length;
    if (entity.indexOf('"type":"drone"') < 0 || numberField(entity, "owner") != playerId || entity.indexOf('"Work"') < 0) continue;
    if (numberField(entity, "spawning") > 0 || numberField(entity, "fatigue") > 0) continue;

    const carried = carryEnergy(entity);
    if (carried >= 100) {
      commands.push(actionForTarget(sequence, entity, spawn, true, carried));
    } else {
      commands.push(actionForTarget(sequence, entity, source, false, 0));
    }
    sequence += 1;
  }

  return "[" + commands.join(",") + "]";
}

function actionForTarget(sequence: i32, actor: string, target: string, transfer: bool, amount: i32): string {
  const actorId = idField(actor);
  const targetId = idField(target);
  const tx = numberField(target, "x");
  const ty = numberField(target, "y");
  if (!isNear(actor, target)) {
    return '{"sequence":' + sequence.toString() + ',"action":{"type":"MoveTo","object_id":' + actorId.toString() + ',"x":' + tx.toString() + ',"y":' + ty.toString() + "}}";
  }
  if (transfer) {
    return '{"sequence":' + sequence.toString() + ',"action":{"type":"Transfer","object_id":' + actorId.toString() + ',"target_id":' + targetId.toString() + ',"resource":"' + ENERGY + '","amount":' + amount.toString() + "}}";
  }
  return '{"sequence":' + sequence.toString() + ',"action":{"type":"Harvest","object_id":' + actorId.toString() + ',"target_id":' + targetId.toString() + ',"resource":"' + ENERGY + '"}}';
}

function findOwnedEntity(snapshot: string, owner: i32, typeName: string, structureType: string): string {
  let cursor = 0;
  while (true) {
    const start = snapshot.indexOf('{"id":', cursor);
    if (start < 0) return "";
    const entity = objectAt(snapshot, start);
    cursor = start + entity.length;
    if (entity.indexOf('"type":"' + typeName + '"') >= 0 && entity.indexOf('"structure_type":"' + structureType + '"') >= 0 && numberField(entity, "owner") == owner) {
      return entity;
    }
  }
}

function findEntity(snapshot: string, typeName: string): string {
  let cursor = 0;
  while (true) {
    const start = snapshot.indexOf('{"id":', cursor);
    if (start < 0) return "";
    const entity = objectAt(snapshot, start);
    cursor = start + entity.length;
    if (entity.indexOf('"type":"' + typeName + '"') >= 0) return entity;
  }
}

function objectAt(text: string, start: i32): string {
  let depth = 0;
  for (let i = start; i < text.length; i++) {
    const c = text.charCodeAt(i);
    if (c == 123) depth += 1;
    if (c == 125) {
      depth -= 1;
      if (depth == 0) return text.substring(start, i + 1);
    }
  }
  return "";
}

function idField(text: string): i32 {
  return numberField(text, "id");
}

function storeEnergy(text: string): i32 {
  const storeStart = text.indexOf('"store":');
  if (storeStart < 0) return 0;
  return numberField(text.substring(storeStart), ENERGY);
}

function carryEnergy(text: string): i32 {
  const carryStart = text.indexOf('"carry":');
  if (carryStart < 0) return 0;
  return numberField(text.substring(carryStart), ENERGY);
}

function numberField(text: string, field: string): i32 {
  const key = '"' + field + '":';
  const start = text.indexOf(key);
  if (start < 0) return 0;
  let i = start + key.length;
  while (i < text.length && text.charCodeAt(i) == 32) i += 1;
  let value = 0;
  while (i < text.length) {
    const c = text.charCodeAt(i);
    if (c < 48 || c > 57) break;
    value = value * 10 + (c - 48);
    i += 1;
  }
  return value;
}

function isNear(a: string, b: string): bool {
  const ax = numberField(a, "x");
  const ay = numberField(a, "y");
  const bx = numberField(b, "x");
  const by = numberField(b, "y");
  const dx = ax > bx ? ax - bx : bx - ax;
  const dy = ay > by ? ay - by : by - ay;
  return dx <= 1 && dy <= 1;
}
