FROM rust:1.85-slim AS build

ARG FDB_VERSION=7.3.59

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        libssl-dev \
        pkg-config \
    && arch="$(dpkg --print-architecture)" \
    && curl -fsSL -o /tmp/foundationdb-clients.deb \
        "https://github.com/apple/foundationdb/releases/download/${FDB_VERSION}/foundationdb-clients_${FDB_VERSION}-1_${arch}.deb" \
    && apt-get install -y --no-install-recommends /tmp/foundationdb-clients.deb \
    && rm -f /tmp/foundationdb-clients.deb \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim AS runtime

ARG FDB_VERSION=7.3.59

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
    && arch="$(dpkg --print-architecture)" \
    && curl -fsSL -o /tmp/foundationdb-clients.deb \
        "https://github.com/apple/foundationdb/releases/download/${FDB_VERSION}/foundationdb-clients_${FDB_VERSION}-1_${arch}.deb" \
    && apt-get install -y --no-install-recommends /tmp/foundationdb-clients.deb \
    && rm -f /tmp/foundationdb-clients.deb \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /app/target/release/swarm-engine /usr/local/bin/swarm-engine
COPY --from=build /app/world.toml /app/world.toml
COPY --from=build /app/mods/ /app/mods/
WORKDIR /app

EXPOSE 8080
HEALTHCHECK --interval=10s --timeout=2s --start-period=10s --retries=6 \
    CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1

CMD ["swarm-engine"]