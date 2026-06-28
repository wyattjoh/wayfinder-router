# Wayfinder gateway container.
#
# The current service image builds the Rust CLI and runs the Rust gateway surface.
# The Python package remains available through PyPI for legacy CLI and API users,
# but it is not installed in this runtime image.
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
