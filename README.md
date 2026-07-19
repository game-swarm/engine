# Swarm Engine

Rust game engine component of Swarm.

## Local Development

Cargo resolves the engine API and plugin SDK from the versioned `v0.1.0` Git release. The engine keeps mod source checkouts as siblings for compile-time composition:

```text
swarm/
├── engine/
└── mods/
    ├── combat-core/
    └── ...
```

A normal engine checkout fetches every configured mod. Cargo fetches API/SDK dependencies itself:

```bash
cd engine
./scripts/fetch-mods.sh
```

`MOD_REV` overrides every mod revision for coordinated development. Non-SHA overrides require `ALLOW_MUTABLE_REFS=true`. Release and CI builds should leave it unset so `mods.toml` remains authoritative. API/SDK upgrades are made only by changing their Cargo version and Git tag together.

Then run the engine in development mode with required secrets and paths:

```bash
# Set required development secrets and persistence paths
export SWARM_ENGINE_MODE=development
export SWARM_NATS_AUTH_SECRET="dev-nats-secret"
export SWARM_PROXY_SIGNATURE_SECRET="dev-proxy-secret"
export REDB_PATH="/tmp/swarm.redb"
export KEYFRAME_BACKUP_PATH="/tmp/swarm-backups"
export SWARM_PROXY_NONCE_PATH="/tmp/swarm-engine-state/proxy-nonces.db"

mkdir -p "$KEYFRAME_BACKUP_PATH"
install -d -m 700 "$(dirname "$SWARM_PROXY_NONCE_PATH")"
cargo run --features vanilla_mods
```

## Container Build

The Docker build context must be the directory that contains the engine and sibling mods. Fetch mods first, then run Docker from that parent directory:

```bash
cd engine
./scripts/fetch-mods.sh
cd ..
docker build -f engine/Dockerfile -t swarm-engine:local .
```

Using `engine/` alone as the build context cannot work because Cargo resolves mod crates through sibling paths.

The engine starts with:

- the engine with an embedded `redb` database at `REDB_PATH`
- NATS at `NATS_URL` (defaults to `nats://127.0.0.1:4222`); startup retries until the connection is available

In development mode, the engine requires valid `REDB_PATH` and `KEYFRAME_BACKUP_PATH` to start. Failure to open or recover the database, or missing backup configuration, results in an immediate process exit (**fail-fast**). If NATS is unavailable, startup keeps retrying and ticks do not begin until NATS connects.


## Production Configuration

Production mode (`SWARM_ENGINE_MODE=production`, the default) requires strict security settings:

- **Issuer Key**: Exactly one of `SWARM_ENGINE_ISSUER_KEY_FILE` (absolute path to 32-byte seed file, no symlinks) or `SWARM_ENGINE_ISSUER_KEY_B64` (base64-encoded 32-byte seed) must be set.
- **NATS Security**: Production requires `NATS_TLS_REQUIRED=true` and `NATS_CREDENTIALS_FILE` (path to a valid NATS credentials file).
- **Message Authentication**: `SWARM_NATS_AUTH_SECRET` must match Sandbox, and `SWARM_PROXY_SIGNATURE_SECRET` must match Gateway.
- **Proxy Nonce Store**: `SWARM_PROXY_NONCE_PATH` should point to a stable file outside `/tmp`; its parent directory must be private, owned by the engine user, and must not be a symlink. If unset, production uses `/var/lib/swarm-engine/proxy-nonces.db`.
- **Backups**: `KEYFRAME_BACKUP_PATH` must be configured for production keyframe backups and should be isolated from the primary keyframe storage path.
- **Persistence**: `REDB_PATH` should point to a stable, writable persistent volume.
