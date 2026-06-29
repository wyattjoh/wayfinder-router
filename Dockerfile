# Wayfinder gateway container.
#
# The service image builds the Rust CLI and runs the Rust gateway surface.
FROM rust:1-slim-bookworm AS builder

WORKDIR /app
COPY . /app

RUN cargo build --release --bin wayfinder-router

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/wayfinder-router /usr/local/bin/wayfinder-router

# Routing config lives here; mount a volume with wayfinder-router.toml when needed.
WORKDIR /data
EXPOSE 8088

CMD ["wayfinder-router", "serve", "--host", "0.0.0.0", "--port", "8088"]
