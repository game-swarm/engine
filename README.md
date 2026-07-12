# Swarm Engine

Rust game engine component of Swarm.

## Local Development

From the engine repository, fetch the optional mod sources and run the engine:

```bash
./scripts/fetch-mods.sh
cargo run
```

The engine starts with:

- the engine with an embedded `redb` database at `REDB_PATH` (defaults to `swarm.redb`)
- NATS at `NATS_URL` when available (defaults to `nats://127.0.0.1:4222`)

`redb` provides pure-Rust embedded ACID key-value storage with no external database service and no C dependencies.

Missing redb access or NATS does not crash the process. The engine logs the dependency as `status=degraded`, keeps ticking without persistence or broadcast, and returns `503` from `/healthz` until required dependencies are reachable.
