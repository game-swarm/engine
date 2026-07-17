# Starter Bot

This is the Phase 1.10 TypeScript starter bot: a 5-minute tutorial bot that keeps the first Swarm loop alive.

Goal: harvest `100 Energy` -> spawn one worker -> keep harvesting and spawning automatically.

## 1. Install

From the TypeScript SDK repository root:

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

The generated SDK and `assembly/tick.ts` use the current Engine ABI. The deploy CLI appends the signed target-manifest section required by Engine validation before upload.

## 4. Deploy

Create an owner-only deploy auth file containing the certificate bundle, its 32-byte Ed25519 private key, and the target defaults. On Unix, the CLI rejects auth files with group/world permissions.

```json
{
  "version": 1,
  "private_key_hex": "<64 hex characters>",
  "certificate_bundle": {
    "cert_id": "<client cert>",
    "player_id": 1,
    "client_auth_cert": "<serialized issued client certificate>",
    "code_signing_cert": "<serialized issued code-signing certificate>"
  },
  "gateway_url": "https://gateway.example.com",
  "world_id": "tutorial",
  "room_id": 0,
  "drone_id": 1,
  "target_manifest_hash": "blake3:<manifest hash>",
  "engine_abi_version": 1,
  "language": "typescript"
}
```

Deploy the project:

```bash
swarm deploy ./examples/starter-bot --auth-file ~/.config/swarm/deploy-auth.json
```

For a project directory, the CLI runs `npm run build`, selects the sole regular `build/*.wasm` artifact, signs both the deploy payload and Gateway request, and reports the accepted module ID and activation status. A prebuilt `.wasm` path can be deployed directly; use `--artifact` when a project emits more than one WASM file.

Copy the issued certificate bundle without altering its serialized certificate fields. Production gateways require HTTPS. Local loopback HTTP is rejected unless the command includes the explicit `--allow-insecure-loopback` test flag.

## 5. Experiment

Try changing the worker body in `src/bot.ts` after the first loop works:

```ts
const WORKER_BODY = ["Move", "Work", "Carry"] as const;
```

Then rebuild and compare command output in the tick explanation or replay viewer.
