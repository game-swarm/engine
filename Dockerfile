FROM rust:1-trixie AS build

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libasound2-dev \
        libudev-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app/engine
COPY mods/ /app/mods/
COPY engine/ /app/engine/
RUN cargo build --release --locked --features vanilla_mods
RUN cargo test --locked --manifest-path sdk-templates/rust/Cargo.toml
RUN ./target/release/swarm-engine generate-sdk world.toml /app/engine/sdk-output
RUN set -eu; \
    found=0; \
    for manifest in /app/engine/sdk-output/*/sdk-rust/Cargo.toml; do \
        test -f "$manifest"; \
        cargo test --manifest-path "$manifest"; \
        found=1; \
    done; \
    test "$found" = 1

FROM debian:trixie-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /app/engine/target/release/swarm-engine /usr/local/bin/swarm-engine
COPY --from=build /app/engine/world.toml /app/world.toml
COPY --from=build /app/mods/ /app/mods/
COPY --from=build /app/engine/sdk-output /app/sdk-output
WORKDIR /app

EXPOSE 8080
HEALTHCHECK --interval=10s --timeout=2s --start-period=10s --retries=6 \
    CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1

CMD ["swarm-engine"]
