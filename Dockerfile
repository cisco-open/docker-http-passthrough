# syntax=docker/dockerfile:1.7
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Cisco Systems, Inc.

# Build stage - Buildx selects the target platform for multi-arch builds.
FROM rust:1.86-bookworm AS builder

WORKDIR /app

# Copy manifests first for better caching
COPY Cargo.toml Cargo.lock ./

# Create a dummy main.rs to build dependencies
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --locked --release && \
    rm -rf src

# Copy actual source code
COPY src ./src

# Build the actual application
RUN touch src/main.rs && cargo build --locked --release

# Runtime stage - debian:bookworm-slim publishes amd64 and arm64 variants.
FROM debian:bookworm-slim

LABEL org.opencontainers.image.title="Docker HTTP Passthrough" \
      org.opencontainers.image.description="HTTP server that proxies container registry images as Docker save-compatible tar archives" \
      org.opencontainers.image.source="https://github.com/cisco-open/docker-http-passthrough" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.vendor="Cisco Systems, Inc."

# Install CA certificates for HTTPS requests to Docker registry and curl for the healthcheck
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the binary from builder
COPY --from=builder /app/target/release/docker-http-passthrough /app/docker-http-passthrough

# Expose the HTTP port
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=10s --start-period=10s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1

# Run the server
CMD ["/app/docker-http-passthrough"]
