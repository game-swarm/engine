# Swarm Engine

Rust game engine component of Swarm.

## Local Development

From the repository root, start the development services:

```bash
docker compose up --build
```

The compose stack starts:

- `fdb`: FoundationDB 7.3.59, exposed on `localhost:4500`
- `nats`: NATS with JetStream enabled, exposed on `localhost:4222` and monitoring on `localhost:8222`
- `engine`: the Rust engine container

The FoundationDB server writes `/etc/foundationdb/fdb.cluster` into the `fdb_config` named volume. The engine mounts the same volume read-only and receives `FDB_CLUSTER_FILE=/etc/foundationdb/fdb.cluster`, so future FDB client code can use the standard cluster file path. The engine image also installs the matching FoundationDB 7.3.59 client package for `libfdb_c` compatibility.

The current engine binary runs a deterministic tick loop and prints `tick`, `state_checksum`, and dependency status every tick. It also exposes `GET /healthz` on port `8080`.

Missing FDB or NATS does not crash the process. The engine logs the dependency as `status=degraded`, keeps ticking without persistence or broadcast, and returns `503` from `/healthz` until both services are reachable.
