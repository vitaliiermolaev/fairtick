# syntax=docker/dockerfile:1
# ---- builder: compile the release binary on the (big) CI runner, never on the droplet ----
FROM rust:1-bookworm AS builder
WORKDIR /build
# Cargo manifests + sources are all that's needed to build the `fairtick` bin.
# gameplay_config.toml / maze.json are read at RUNTIME (not include_str'd by this bin),
# and there is no compile-time DATABASE_URL (sqlx uses runtime queries), so the build
# needs no database and no extra files.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# --locked: build against the committed Cargo.lock exactly, like CI (`cargo test --locked`),
# so the Docker image can't silently resolve different dependency versions than CI verified.
RUN cargo build --release --locked --bin fairtick

# ---- runtime: slim image with just the binary + the shared config files ----
FROM debian:bookworm-slim
# curl is for the container HEALTHCHECK (probes /readyz) — the deploy's health gate + rollback
# wait on `docker compose up --wait`, which keys off that healthcheck. ca-certificates for TLS.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /build/target/release/fairtick /usr/local/bin/fairtick
# Shared gameplay config (its sha256 is the config_hash the client must match). Baked into
# the image so a config change ships with the image, NOT persisted in a volume.
COPY gameplay_config.toml maze.json ./
# Persisted at runtime via volumes (see docker-compose.yml): the SQLite DB and the
# per-run telemetry NDJSON.
RUN mkdir -p /app/data /app/logs
# Build/release id (the git sha), passed by the deploy and surfaced in server logs so every
# run names the exact build it came from. "dev" for a plain local `docker build`.
ARG BUILD_ID=dev
ENV FAIRTICK_BIND_ADDR=0.0.0.0:8080 \
    DATABASE_URL=sqlite:/app/data/fairtick.db?mode=rwc \
    FAIRTICK_BUILD_ID=${BUILD_ID}
EXPOSE 8080
# Liveness/readiness for the deploy health gate. Probes the LOCAL backend (bypasses Caddy,
# which blocks /readyz externally). `docker compose up --wait` waits for this to go healthy;
# a failed deploy stays unhealthy → the workflow rolls back to the previous image.
HEALTHCHECK --interval=10s --timeout=3s --retries=5 --start-period=10s \
  CMD curl -fsS http://localhost:8080/readyz || exit 1
CMD ["fairtick"]
