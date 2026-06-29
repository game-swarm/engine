# Swarm Engine

Rust game engine component of Swarm.

## Local Development

From the repository root, start the development services:

```bash
docker compose up --build
```

The compose stack starts:

- the engine with an embedded `redb` database at `REDB_PATH` (defaults to `swarm.redb`)

`redb` provides pure-Rust embedded ACID key-value storage with no external database service and no C dependencies.

Missing redb access or NATS does not crash the process. The engine logs the dependency as `status=degraded`, keeps ticking without persistence or broadcast, and returns `503` from `/healthz` until required dependencies are reachable.
