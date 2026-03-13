# Docker HTTP Passthrough

[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)
[![Build Status](https://github.com/cisco-open/docker-http-passthrough/actions/workflows/ci.yml/badge.svg)](https://github.com/cisco-open/docker-http-passthrough/actions)

An HTTP server that proxies Docker image downloads as tar archives on-the-fly. Designed for network devices (like Cisco routers/switches) that need to download Docker images via simple HTTP requests.

## Table of Contents

- [How It Works](#how-it-works)
- [Quick Start](#quick-start)
- [URL Format](#url-format)
- [Building](#building)
- [Usage](#usage)
- [Configuration](#configuration)
- [Architecture](#architecture)
- [API Reference](#api-reference)
- [Limitations](#limitations)
- [Contributing](#contributing)
- [License](#license)

## How It Works

1. Client requests a URL like `http://server:8080/docker/nginx_latest.tar`
2. Server parses the filename to extract image name and tag (`nginx:latest`)
3. Server pulls the image from Docker Hub registry (linux/amd64 architecture)
4. Server streams the tar archive back to the client using chunked transfer encoding
5. The response starts immediately to prevent client timeouts

## Quick Start

```bash
# Using Docker Compose
docker-compose up -d

# Test with curl
curl -o nginx.tar http://localhost:8080/docker/nginx_latest.tar

# Load into Docker
docker load -i nginx.tar
```

## URL Format

The filename format is: `{image}_{tag}.tar` or `{namespace}_{image}_{tag}.tar`

| Request URL | Docker Image |
|-------------|--------------|
| `/docker/nginx_latest.tar` | `nginx:latest` |
| `/docker/nginx_1.25.tar` | `nginx:1.25` |
| `/docker/library_nginx_latest.tar` | `library/nginx:latest` |
| `/docker/myrepo_myimage_v1.0.tar` | `myrepo/myimage:v1.0` |

## Building

### Using Docker (Recommended)

```bash
# Build and run with docker-compose
docker-compose up -d

# Or build manually
docker build --platform linux/amd64 -t docker-http-passthrough .
docker run -d -p 8080:8080 docker-http-passthrough
```

### Local Development

```bash
# Requires Rust 1.75+
cargo build --release
./target/release/docker-http-passthrough
```

## Usage

### From Cisco Device

```
copy http://10.20.30.40:8080/docker/nginx_latest.tar flash:nginx_latest.tar
```

### Using curl

```bash
# Download nginx:latest as tar
curl -o nginx.tar http://localhost:8080/docker/nginx_latest.tar

# Download specific version
curl -o alpine.tar http://localhost:8080/docker/alpine_3.19.tar

# Health check
curl http://localhost:8080/health
```

### Load the Downloaded Image

```bash
docker load -i nginx.tar
```

## Configuration

| Environment Variable | Default | Description |
|---------------------|---------|-------------|
| `RUST_LOG` | `info` | Log level (trace, debug, info, warn, error) |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | *(empty)* | OTLP endpoint for OpenTelemetry collector (e.g., `http://otel-collector:4317`) |
| `OTEL_SERVICE_NAME` | `docker-http-passthrough` | Service name for traces |

The server listens on port **8080** by default.

### OpenTelemetry Tracing

The server supports OpenTelemetry instrumentation for distributed tracing. To enable it:

1. Copy `.env.example` to `.env`
2. Set `OTEL_EXPORTER_OTLP_ENDPOINT` to your collector's gRPC endpoint

```bash
cp .env.example .env
# Edit .env and set OTEL_EXPORTER_OTLP_ENDPOINT=http://your-collector:4317
```

When enabled, the following spans are created within a single trace:

| Span Name | Type | Description |
|-----------|------|-------------|
| `GET /docker/{filename}` | Parent | Entire request lifecycle |
| `resolve_manifest` | Child | Fetching and resolving Docker manifest from registry |
| `fetch_config_blob` | Child | Downloading image config blob |
| `download_layer_N` | Child | Downloading and decompressing each layer |
| `write_layer_tar_N` | Child | Writing each layer to the tar stream |

**Semantic Conventions Used:**

| Attribute | Convention | Description |
|-----------|------------|-------------|
| `http.request.method` | HTTP | Request method (GET) |
| `http.route` | HTTP | Route template |
| `url.path` | URL | Request path |
| `url.full` | URL | Full request URL |
| `server.address` | Server | Server address/host |
| `server.port` | Server | Server port |
| `client.address` | General | Client IP address |
| `user_agent.original` | User-Agent | User agent |
| `container.image.name` | Container | Docker image name |
| `container.image.tag` | Container | Docker image tag |
| `oci.image.ref` | Custom (OCI) | Full image reference including tag |
| `oci.manifest.digest` | Custom (OCI) | OCI manifest digest |
| `oci.image.config.digest` | Custom (OCI) | OCI image config digest |
| `container.image.id` | Container | Image ID (config digest) |
| `oci.layer.index` | Custom (OCI) | Layer index (1-based) |
| `oci.layer.digest` | Custom (OCI) | Layer digest |
| `oci.layer.compressed_size` | Custom (OCI) | Compressed layer size |
| `oci.layer.decompressed_size` | Custom (OCI) | Decompressed layer size |
| `http.response.body.size` | HTTP | Response body size |

If `OTEL_EXPORTER_OTLP_ENDPOINT` is empty or not set, tracing export is disabled and no overhead is added.

## Architecture

- **Streaming Response**: Uses chunked transfer encoding to start responding immediately while pulling the image. This prevents client timeouts for large images.
- **Registry API**: Directly communicates with Docker Hub's registry API (no local Docker daemon required for pulling).
- **AMD64 Only**: Always pulls linux/amd64 architecture images.
- **Docker Save Format**: Creates tar archives compatible with `docker load`.

## API Reference

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/docker/{filename}` | GET | Download Docker image as tar |
| `/health` | GET | Health check endpoint |

## Limitations

- Only supports public Docker Hub images (no authentication for private registries)
- Only pulls linux/amd64 architecture
- Large images may take time to download and stream

## Contributing

We welcome contributions! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

Before contributing, please read our [Code of Conduct](CODE_OF_CONDUCT.md).

## Security

For security concerns, please see [SECURITY.md](SECURITY.md).

## License

This project is licensed under the Apache License 2.0 - see the [LICENSE](LICENSE) file for details.

```
Copyright 2026 Cisco Systems, Inc.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
```
