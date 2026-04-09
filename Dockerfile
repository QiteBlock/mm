FROM rust:1.90-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

# ── Runtime image ──────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/market-making /usr/local/bin/market-making

# Config is injected at build time via --build-arg so the image is self-contained.
# Override CONFIG_FILE to select which venue config to embed.
ARG CONFIG_FILE=config/settings.example.toml
COPY ${CONFIG_FILE} /app/config/settings.toml

RUN mkdir -p /app/data

CMD ["market-making", "config/settings.toml"]
