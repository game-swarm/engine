FROM rust:1-trixie AS build

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libasound2-dev \
        libudev-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY mods.toml ./mods.toml
COPY scripts/ ./scripts/
RUN git config --global advice.detachedHead false \
    && ./scripts/fetch-mods.sh
COPY . .
RUN cargo build --release --locked --features vanilla_mods
RUN ./target/release/swarm-engine generate-sdk world.toml /app/sdk-output

FROM debian:trixie-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /app/target/release/swarm-engine /usr/local/bin/swarm-engine
COPY --from=build /app/world.toml /app/world.toml
COPY --from=build /app/mods/ /app/mods/
COPY --from=build /app/sdk-output /app/sdk-output
WORKDIR /app

EXPOSE 8080
HEALTHCHECK --interval=10s --timeout=2s --start-period=10s --retries=6 \
    CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1

CMD ["swarm-engine"]
