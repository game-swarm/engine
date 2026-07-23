# Swarm TypeScript SDK

TypeScript SDK for the Swarm programmable MMO RTS API. This package implements the Phase 0 client-facing contract: IDL types, `CommandIntent` builders, strict tick-output validation and serialization, visibility helpers, world-rules helpers, and debug-safe snapshot formatting.

It does not implement the server engine, ECS simulation, WASM sandbox, MCP server, or authoritative command application. Those remain engine/gateway responsibilities.

## Install

```bash
npm install @swarm/sdk-ts
```

## Usage

```ts
import { actions, command, runTick, type TickHandler } from "@swarm/sdk-ts";

const tick: TickHandler = (snapshot) => {
  const spawn = snapshot.entities.find(
    (entity) => entity.type === "structure" && entity.owner === snapshot.player_id
  );
  const actor = snapshot.entities.find(
    (entity) => entity.type === "drone" && entity.owner === snapshot.player_id
  );

  if (!spawn || !actor) return [];

  return [
    command(0, `bot-${snapshot.tick}-0`, actions.spawn(actor.id, spawn.id, ["Move", "Work", "Carry"]))
  ];
};

const commandJson = await runTick(tick, snapshot);
```

## Command Contract

WASM `tick()` output is `CommandIntent[]`. Each intent contains only:

- `sequence`: per-tick monotonic `u32` chosen by player code
- `idempotency_key`: a non-empty key unique to the intended command
- `action`: one IDL action such as `Move`, `Harvest`, `Spawn`, `Attack`, `Fortify`

Fields such as `player_id`, `tick`, `source`, and `auth` are server-injected and rejected if present in untrusted tick output.

```ts
import { actions, command, serializeTickOutput } from "@swarm/sdk-ts";

const output = serializeTickOutput([
command(0, "tick-42-move", actions.move(1001, "East")),
  command(1, "tick-42-harvest", actions.harvest(1001, 4001, "Energy"))
]);
```

Validation enforces the P0 limits: 100 commands per tick, 256KB JSON output, depth at most 10, no extra fields, bounded strings, valid enums, room coordinate bounds, and spawn body size at most 50.

## Visibility Helpers

```ts
import { visibleEntities } from "@swarm/sdk-ts";

const visible = visibleEntities(allEntities, playerId, tick);
```

The helper mirrors the P0 unified visibility policy for SDK-side filtering and tests: owned entities are always visible; hostile/neutral entities are visible when within a friendly vision source; full-information mode can be enabled for arena or tutorial views. Server outputs must still use the authoritative engine visibility cache.

## World Rules Helpers

```ts
import { createDefaultWorldConfig, validateWorldConfig } from "@swarm/sdk-ts";

const config = createDefaultWorldConfig("persistent");
const issues = validateWorldConfig(config);
```

The SDK includes defaults for World and Arena modes, validation for dangerous rule combinations, and helpers for action/body costs using runtime resource names.

## AI Snapshot Safety

```ts
import { makeSnapshotForAi } from "@swarm/sdk-ts";

const promptSafePayload = makeSnapshotForAi(snapshot);
```

Snapshots are wrapped in explicit untrusted-data delimiters so AI agents can distinguish game data from system instructions.

## Development

```bash
npm install
npm test
npm run build
```

Build a minimal TypeScript starter bot example with:

```bash
npm run build:example basic-harvester
```

The example source is `examples/basic-harvester.ts`.

The package has no runtime dependencies.
