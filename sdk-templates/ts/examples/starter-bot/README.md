# Starter Bot

This is the Phase 1.10 TypeScript starter bot: a 5-minute tutorial bot that keeps the first Swarm loop alive.

Goal: harvest `100 Energy` -> spawn one worker -> keep harvesting and spawning automatically.

## 1. Install

From the repository root:

```bash
npm install
```

## 2. Read The Bot

The tutorial logic lives in `src/bot.ts` and uses the SDK types and command builders:

```ts
import { actions, command, type WorldSnapshot } from "@swarm/sdk-ts";
```

Every tick, the bot:

1. Finds your `Spawn` and the nearest visible `source`.
2. If the spawn has at least `100 Energy`, submits `Spawn(["Work"])`.
3. Each worker harvests `Energy` until it carries `100`.
4. Full workers return to the spawn and transfer energy.
5. The loop repeats, so the colony keeps adding workers as energy arrives.

The tutorial worker body is intentionally tiny (`["Work"]`) so the first spawn happens exactly at the 100 Energy milestone from the Phase 1.10 acceptance criteria. Real worlds can change `WORKER_BODY` to `['Move', 'Work', 'Carry']` or a larger body once enough energy is available.

## 3. Build The WASM

```bash
npm run example:starter-bot:build
```

This runs TypeScript type-checking and compiles `assembly/tick.ts` to:

```text
examples/starter-bot/build/starter-bot.wasm
```

The AssemblyScript file is a standalone WASM smoke-test entrypoint that mirrors the tutorial strategy and exports:

```text
alloc(len) -> ptr
free(ptr)
tick(snapshot_ptr, snapshot_len) -> result_ptr
result_len() -> len
```

The engine ABI is still being finalized in the P0 notes, so `src/bot.ts` is the reference SDK bot and `assembly/tick.ts` proves the example can compile to deployable WASM today.

## 4. Deploy

When the Phase 1 CLI is available, deploy the generated module:

```bash
swarm deploy ./examples/starter-bot
```

Until then, use the unit tests as the local contract check.

## 5. Experiment

Try changing the worker body in `src/bot.ts` after the first loop works:

```ts
const WORKER_BODY = ["Move", "Work", "Carry"] as const;
```

Then rebuild and compare command output in the tick explanation or replay viewer.
