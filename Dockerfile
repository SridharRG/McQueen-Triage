# McQueen triage: Rust daemon + Atlaskit UI in one image.

# ── UI ──────────────────────────────────────────────────────────────────────
FROM node:24-alpine AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web/ ./
RUN npm run build

# ── daemon ──────────────────────────────────────────────────────────────────
FROM rust:1.97.1-slim-bookworm AS build
WORKDIR /src
# Cache dependencies separately from sources.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release --locked && rm -rf src target/release/deps/mcqueen*
COPY src ./src
RUN cargo build --release --locked

# ── runtime ─────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home /app mcqueen \
 && mkdir -p /data /repos /schema && chown mcqueen /data
WORKDIR /app
COPY --from=build /src/target/release/mcqueen /usr/local/bin/mcqueen
COPY --from=web /web/dist ./web/dist
COPY mcqueen.toml ./mcqueen.toml
ENV BIND=0.0.0.0:3028 \
    STATIC_DIR=/app/web/dist \
    MCQUEEN_CONFIG=/app/mcqueen.toml \
    DATABASE_PATH=/data/mcqueen.db \
    REPOS_DIR=/repos \
    RUST_LOG=mcqueen=info,rig_agent=warn,warn
USER mcqueen
EXPOSE 3028
VOLUME ["/data"]
ENTRYPOINT ["mcqueen"]
CMD ["serve"]
