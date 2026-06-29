# Swarm Engine

Rust game engine component of Swarm.

## Local Development

From the repository root, start the development services:

```bash
docker compose up --build
```

The compose stack starts:

- `tikv`: TiKV distributed KV store, exposed on `localhost:2379` (PD) and `localhost:20160` (KV)

The TiKV cluster provides distributed, transactional key-value storage accessed via the pure-Rust `tikv-client` crate — no C dependencies or system libraries required.

Missing TiKV or NATS does not crash the process. The engine logs the dependency as `status=degraded`, keeps ticking without persistence or broadcast, and returns `503` from `/healthz` until both services are reachable.
