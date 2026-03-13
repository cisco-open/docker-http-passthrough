# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Cisco Systems, Inc.

# Build stage - build natively on amd64
FROM rust:1.83-bookworm AS builder

WORKDIR /app

# Copy manifests first for better caching
COPY Cargo.toml ./

# Create a dummy main.rs to build dependencies
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release && \
    rm -rf src

# Copy actual source code
COPY src ./src

# Build the actual application
RUN touch src/main.rs && cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install CA certificates for HTTPS requests to Docker registry
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the binary from builder
COPY --from=builder /app/target/release/docker-http-passthrough /app/docker-http-passthrough

# Expose the HTTP port
EXPOSE 8080

# Run the server
CMD ["/app/docker-http-passthrough"]
