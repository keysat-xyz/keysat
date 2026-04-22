# Multi-stage Dockerfile for the Keysat daemon.
#
# Stage 1: build the Rust binary with musl so it's statically linked and
# needs no libc/ssl in the runtime stage. This keeps the final image tiny
# (~20 MB) and boot times fast, which matters on a home server.
#
# Stage 2: a bare-bones runtime image that just runs the binary.
#
# The upstream source directory is still called `licensing-service` on disk
# for continuity with earlier revisions; the binary it produces is `keysat`.

# syntax=docker/dockerfile:1.6

ARG RUST_VERSION=1.75

# -------- builder --------
FROM rust:${RUST_VERSION}-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config musl-tools ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Add musl target for the current architecture. Docker fills in
# TARGETARCH/TARGETPLATFORM when the image is built with buildx for multi-arch.
ARG TARGETARCH
RUN case "${TARGETARCH}" in \
        amd64)  rustup target add x86_64-unknown-linux-musl  ;; \
        arm64)  rustup target add aarch64-unknown-linux-musl ;; \
        *) echo "unsupported TARGETARCH: ${TARGETARCH}" && exit 1 ;; \
    esac

WORKDIR /src

# Cache dependencies: copy only the manifest/lock first so `cargo fetch`
# can be re-used across builds that don't change deps.
COPY licensing-service/Cargo.toml licensing-service/Cargo.lock* ./licensing-service/
COPY licensing-service/migrations ./licensing-service/migrations

# Make a dummy src to let cargo fetch resolve deps. Real src comes next.
RUN mkdir -p licensing-service/src && \
    echo 'fn main() {}' > licensing-service/src/main.rs && \
    cd licensing-service && cargo fetch

# Copy the actual source.
COPY licensing-service/src ./licensing-service/src

# Build.
ARG TARGETARCH
RUN case "${TARGETARCH}" in \
        amd64) TARGET=x86_64-unknown-linux-musl  ;; \
        arm64) TARGET=aarch64-unknown-linux-musl ;; \
    esac && \
    cd licensing-service && \
    CARGO_NET_RETRY=10 \
    cargo build --release --target ${TARGET} --locked && \
    cp target/${TARGET}/release/keysat /keysat

# -------- runtime --------
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates tini \
 && rm -rf /var/lib/apt/lists/*

# Non-root user to avoid running as root even though we're in a container.
RUN useradd --system --create-home --uid 10001 keysat
USER keysat
WORKDIR /home/keysat

COPY --from=builder /keysat /usr/local/bin/keysat

ENV KEYSAT_BIND=0.0.0.0:8080 \
    KEYSAT_DB_PATH=/data/keysat.db

EXPOSE 8080

# tini reaps zombie processes and forwards signals — StartOS sends SIGTERM
# on service stop; the binary installs a graceful handler for it.
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/usr/local/bin/keysat"]
