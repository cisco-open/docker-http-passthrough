# Docker HTTP Passthrough

[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.86%2B-orange.svg)](https://www.rust-lang.org/)
[![Build Status](https://github.com/cisco-open/docker-http-passthrough/actions/workflows/ci.yml/badge.svg)](https://github.com/cisco-open/docker-http-passthrough/actions)

An HTTP server that proxies Docker image downloads as tar archives on-the-fly. Designed for network devices (like Cisco routers/switches) that need to download Docker images via simple HTTP requests.

## Table of Contents

- [How It Works](#how-it-works)
- [Choose a Download Mode](#choose-a-download-mode)
- [Image Filename Format](#image-filename-format)
- [Quick Start](#quick-start)
- [Building](#building)
- [Dynamic Website](#dynamic-website)
- [Zensical Documentation](#zensical-documentation)
- [Published OCI Image](#published-oci-image)
- [Usage Examples](#usage-examples)
- [Configuration](#configuration)
- [Private Registry Authentication](#private-registry-authentication)
- [Cisco HTTP Client Credentials](#cisco-http-client-credentials)
- [OpenTelemetry Tracing](#opentelemetry-tracing)
- [Validated Scenarios](#validated-scenarios)
- [Architecture](#architecture)
- [API Reference](#api-reference)
- [Troubleshooting](#troubleshooting)
- [Limitations](#limitations)
- [Contributing](#contributing)
- [License](#license)

## How It Works

Docker HTTP Passthrough turns a simple HTTP GET into a Docker save-format tar archive.

1. A client requests a URL such as `http://server:8080/docker/nginx_latest.tar`.
2. The server parses the filename into an image repository and tag.
3. The server chooses the registry from either environment configuration or the request URL.
4. The server chooses registry credentials from environment configuration, client-supplied HTTP Basic auth, or anonymous access.
5. The server resolves the `linux/amd64` manifest through the registry V2 API.
6. The server downloads the image config and layers into temporary storage.
7. The server returns a Docker save-compatible tar archive that can be loaded with `docker load`.

The passthrough service does not use the local Docker daemon to pull images. It talks directly to the registry API.

## Choose a Download Mode

Pick the route based on where the registry details and credentials should come from.

| Route | Registry Source | Credential Source | Use When |
|-------|-----------------|-------------------|----------|
| `/docker/{filename}` | Environment variables such as `REGISTRY_URL` | Environment credentials, or client Basic auth controlled by `REGISTRY_CLIENT_AUTH` | You operate the passthrough container and want one default registry configuration |
| `/registry/{scheme}/{host}/docker/{filename}` | Request URL | Environment credentials or optional client Basic auth if the client sends it | The client should choose the registry, but credentials are static or not required |
| `/registry-auth/{scheme}/{host}/docker/{filename}` | Request URL | Client Basic auth; Cisco can supply it with `ip http client username/password` | The client should choose both the registry and the registry identity per download |

Use `/registry-auth/...` for the most neutral passthrough behavior. The container does not need registry-specific environment variables, provided no static bearer token is configured.

The passthrough service never stores credentials supplied by the client. It reads the Basic `Authorization` header for the current request, uses it for the upstream registry token exchange or Basic registry request, and then lets the request state expire.

## Image Filename Format

All download routes end with `/docker/{filename}`. The filename describes the image to fetch.

```text
{image}_{tag}.tar
{namespace}_{image}_{tag}.tar
{path__to__image}_{tag}.tar
```

Parsing rules:

- The filename must end in `.tar`.
- The final underscore separates the image path from the tag.
- A single underscore preserves the original `namespace_image` shorthand.
- Double underscores become additional repository path separators.
- For Docker Hub only, a single-component image such as `nginx_latest.tar` defaults to `library/nginx:latest`.
- For non-Docker Hub registries, a single-component image remains single-component unless `REGISTRY_DEFAULT_NAMESPACE` is set.
- Tags containing underscores cannot be represented unambiguously by this filename format.

| Filename | Docker Image |
|----------|--------------|
| `nginx_latest.tar` | `library/nginx:latest` on Docker Hub; `nginx:latest` on registries without a default namespace |
| `nginx_1.25.tar` | `library/nginx:1.25` on Docker Hub |
| `library_nginx_latest.tar` | `library/nginx:latest` |
| `myrepo_myimage_v1.0.tar` | `myrepo/myimage:v1.0` |
| `team__platform__api_2026.06.tar` | `team/platform/api:2026.06` |

Full URL examples:

| Request URL | Meaning |
|-------------|---------|
| `/docker/nginx_latest.tar` | Fetch from the configured default registry |
| `/registry/https/ghcr.io/docker/my-org__private-image_latest.tar` | Fetch `my-org/private-image:latest` from `https://ghcr.io` |
| `/registry-auth/http/registry2.example.com:5000/docker/team__platform__api_2026.06.tar` | Fetch `team/platform/api:2026.06` from `http://registry2.example.com:5000` and require client-supplied credentials |

The `{host}` value is resolved from the passthrough service, not from the Cisco device. Use a registry DNS name or IP address that the passthrough container can reach. If a client does not accept a literal port separator in the path segment, percent-encode it, for example `registry2.example.com%3A5000`.

For GHCR, repository paths are usually owner/package or owner/nested/package. Encode each `/` in the repository path as `__`, for example `my-org__private-image_latest.tar` for `my-org/private-image:latest`.

## Quick Start

```bash
# Using Docker Compose
docker compose up -d

# Test with curl
curl -o nginx.tar http://localhost:8080/docker/nginx_latest.tar

# Load into Docker
docker load -i nginx.tar
```

## Building

### Using Docker (Recommended)

```bash
# Build and run with Docker Compose
docker compose up -d

# Or build manually
docker build -t docker-http-passthrough .
docker run -d -p 8080:8080 docker-http-passthrough
```

### Multi-Arch Image Builds

The repository includes a Buildx Bake definition for repeatable OCI image builds:

```bash
# Build and load one local image for the default local platform
docker buildx bake image-local

# Build and load one local image for a specific platform
docker buildx bake --set image-local.platform=linux/amd64 image-local

# Build a multi-arch image without pushing it
docker buildx bake image-all

# Build and push a multi-arch image
docker login ghcr.io
IMAGE=ghcr.io/cisco-open/docker-http-passthrough TAG=latest docker buildx bake image-push
```

The default multi-arch platform set is `linux/amd64,linux/arm64`.

### Local Development

```bash
# Requires Rust 1.86+
cargo build --release
./target/release/docker-http-passthrough
```

## Dynamic Website

The repository includes a standalone project website in [docs/index.html](docs/index.html). It includes the route builder, topology views for unauthenticated and authenticated flows, registry examples, OpenTelemetry flow, OCI image notes, and validation guidance.

Preview it locally with:

```bash
python3 -m http.server 8088 --directory docs
```

Then open:

```text
http://127.0.0.1:8088/
```

## Zensical Documentation

Comprehensive Markdown documentation is available under [docs/zensical](docs/zensical/index.md) and is configured with [zensical.toml](zensical.toml). The Zensical docs include Mermaid diagrams for architecture, registry routing, authentication, Cisco IOS-XE app-hosting workflows, OpenTelemetry traces, deployment, validation, and security.

Preview the documentation with:

```bash
zensical serve
```

Build the static documentation site with:

```bash
zensical build
```

Mermaid rendering is enabled through the `pymdownx.superfences` custom fence configuration in `zensical.toml` and the runtime initializer in [docs/zensical/javascripts/mermaid.mjs](docs/zensical/javascripts/mermaid.mjs).

## Published OCI Image

The project is configured to publish a multi-arch OCI image to GHCR:

```text
ghcr.io/cisco-open/docker-http-passthrough:<tag>
```

The `Publish OCI Image` GitHub Actions workflow builds `linux/amd64` and `linux/arm64` variants with Docker Buildx and publishes a single manifest list. Pulls from Docker, Kubernetes, or another OCI-aware runtime automatically select the matching platform.

Tag behavior:

| Event | Tags |
|-------|------|
| Push to `main` | `main`, `sha-<commit>`, `latest` |
| Version tag such as `v0.1.0` | `0.1.0`, `0.1`, `sha-<commit>` |
| Pull request | Build validation only; image is not pushed |
| Manual workflow dispatch | Publishes according to the selected ref |

Run the published image directly:

```bash
docker run -d \
  --name docker-http-passthrough \
  -p 8080:8080 \
  ghcr.io/cisco-open/docker-http-passthrough:latest
```

Use it from Kubernetes with [kubernetes.yaml](kubernetes.yaml), which points at the GHCR image by default:

```bash
kubectl apply -f kubernetes.yaml
```

The Kubernetes manifest includes the base deployment, service, health probes, and `RUST_LOG`. Add registry and OpenTelemetry environment variables to the deployment when the cluster-hosted service needs static registry credentials, a custom default registry, or OTLP export.

For app-hosting environments that can install or pull an OCI image reference directly, use:

```text
ghcr.io/cisco-open/docker-http-passthrough:latest
```

For IOS-XE CLI workflows that require a Docker save archive in `flash:` or `usbflash1:`, use a running passthrough instance to turn the same GHCR image into an installable tar archive:

```text
copy http://<passthrough-ip>:8080/registry/https/ghcr.io/docker/cisco-open__docker-http-passthrough_latest.tar usbflash1:docker-http-passthrough.tar
app-hosting install appid docker-http-passthrough package usbflash1:docker-http-passthrough.tar
```

## Usage Examples

### Public Docker Hub Image

```text
copy http://10.20.30.40:8080/docker/nginx_latest.tar flash:nginx_latest.tar
```

```bash
curl -o nginx.tar http://localhost:8080/docker/nginx_latest.tar
docker load -i nginx.tar
```

### Static Default Private Registry

Set the registry and credentials on the passthrough service:

```bash
export REGISTRY_URL=https://registry.example.com
export REGISTRY_NAME=registry.example.com
export REGISTRY_USERNAME=my-user
export REGISTRY_PASSWORD=my-token
docker compose up -d
```

Then use the normal `/docker/{filename}` route:

```text
copy http://<passthrough-ip>:8080/docker/team__platform__api_2026.06.tar flash:api.tar
```

### Cisco Supplies Credentials for the Default Registry

Set the passthrough service to challenge the Cisco HTTP client:

```bash
export REGISTRY_URL=https://registry-1.docker.io
export REGISTRY_NAME=docker.io
export REGISTRY_CLIENT_AUTH=required
docker compose up -d
```

Configure the Cisco HTTP client and copy the image:

```text
configure terminal
ip http client username <registry-username>
ip http client password <registry-password-or-token>
end
copy http://<passthrough-ip>:8080/docker/<dockerhub-user>_private-image_2026.06.tar flash:private-image.tar
```

Remove the temporary Cisco client credentials after the transfer if they are no longer needed:

```text
configure terminal
no ip http client username
no ip http client password
end
```

### Cisco Supplies Registry and Credentials Per Request

This mode does not require the passthrough container to know the registry ahead of time.

```text
configure terminal
ip http client username <registry-username>
ip http client password <registry-password-or-token>
end
copy http://<passthrough-ip>:8080/registry-auth/https/docker.io/docker/<dockerhub-user>_private-image_2026.06.tar flash:private-image.tar
copy http://<passthrough-ip>:8080/registry-auth/https/ghcr.io/docker/my-org__private-image_latest.tar flash:ghcr-image.tar
copy http://<passthrough-ip>:8080/registry-auth/http/registry2.example.com:5000/docker/team__platform__api_2026.06.tar flash:api.tar
```

Use `docker.io` in the route for Docker Hub. The service maps Docker Hub aliases to `https://registry-1.docker.io` and `https://auth.docker.io/token` internally.

Use a registry token with pull permission for the target package. For private GHCR packages, that means a GitHub token with package read permission for the package owner.

After the copy finishes, remove the Cisco HTTP client credentials if they were only needed for the transfer:

```text
configure terminal
no ip http client username
no ip http client password
end
```

Then install and start the downloaded archive with IOS-XE app-hosting:

```text
app-hosting install appid <appid> package flash:<image>.tar
configure terminal
app-hosting appid <appid>
 app-vnic AppGigabitEthernet trunk
  vlan <vlan-id> guest-interface 0
end
app-hosting activate appid <appid>
app-hosting start appid <appid>
show app-hosting list | include <appid>
show app-hosting detail appid <appid>
```

### Request Selects Registry, Static Credentials Are Used

Use this when clients can choose the target registry, but credentials are still configured on the passthrough service.

```bash
export REGISTRY_USERNAME=my-user
export REGISTRY_PASSWORD=my-token
docker compose up -d
curl -o image.tar http://localhost:8080/registry/https/ghcr.io/docker/my-org__private-image_latest.tar
```

### Health Check

```bash
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
| `REGISTRY_URL` | `https://registry-1.docker.io` | Container registry V2 API URL |
| `REGISTRY_AUTH_URL` | Docker Hub token URL for Docker Hub, otherwise challenge-discovered | Registry token endpoint |
| `REGISTRY_SERVICE` | `registry.docker.io` for Docker Hub, otherwise registry host | Registry service name for auth scopes |
| `REGISTRY_NAME` | `docker.io` | Registry display name for tags and telemetry |
| `REGISTRY_USERNAME` | *(empty)* | Username for private registry token or Basic auth |
| `REGISTRY_PASSWORD` | *(empty)* | Password, personal access token, or robot token for `REGISTRY_USERNAME` |
| `REGISTRY_BEARER_TOKEN` | *(empty)* | Static bearer token for registries that provide one directly |
| `REGISTRY_TOKEN` | *(empty)* | Compatibility alias for `REGISTRY_BEARER_TOKEN` |
| `REGISTRY_CLIENT_AUTH` | `optional` | Whether to use HTTP Basic credentials supplied by the downloading client: `optional`, `required`, or `off` |
| `REGISTRY_DEFAULT_NAMESPACE` | `library` for Docker Hub, empty for other registries | Namespace prepended to single-component image names |

The server listens on **0.0.0.0:8080** inside the container. Map any host-side port to container port 8080 when running under Docker, Kubernetes, or another container runtime.

With Docker Compose, set variables by exporting them in the shell or placing them in `.env`. `REGISTRY_DEFAULT_NAMESPACE` is passed through only when explicitly set, so leaving it unset preserves the service's Docker Hub `library/` default while allowing non-Docker Hub registries to stay namespace-neutral.

## Private Registry Authentication

### Registry Selection

There are two registry selection models:

- Static default registry: set `REGISTRY_URL` and use `/docker/{filename}`.
- Request-selected registry: put `{scheme}` and `{host}` in `/registry/...` or `/registry-auth/...`.

Docker Hub aliases are handled specially. `docker.io`, `index.docker.io`, and `registry-1.docker.io` all map to:

```text
Registry API: https://registry-1.docker.io
Token endpoint: https://auth.docker.io/token
Service: registry.docker.io
Default namespace: library
```

Other registries use the Docker Registry V2 API at `{scheme}://{host}`. If the registry returns a `WWW-Authenticate: Bearer ...` challenge, the token endpoint and service are discovered from that challenge.

Examples:

| Registry | Route Host | Notes |
|----------|------------|-------|
| Docker Hub | `docker.io` | Alias for `https://registry-1.docker.io` with `library/` default namespace for official images |
| GHCR | `ghcr.io` | Uses GHCR's registry challenge; private packages require a package-readable GitHub token |
| Local registry | `registry2.example.com:5000` | Use `http` or `https` to match the registry listener |
| IP-based registry | `10.10.20.30:5000` | Use a host reachable from the passthrough container |

### Credential Sources and Precedence

Registry credentials are never embedded in the image filename.

The effective upstream auth behavior is:

1. If `REGISTRY_BEARER_TOKEN` or `REGISTRY_TOKEN` is set, the service sends that bearer token to the registry.
2. If a Docker Hub or configured token endpoint is available, the service fetches a registry bearer token. Client-supplied Basic credentials are used for that token request when present; otherwise `REGISTRY_USERNAME` and `REGISTRY_PASSWORD` are used when set.
3. If no token endpoint is configured or discovered, the service uses direct Basic auth with client-supplied credentials when present; otherwise it uses `REGISTRY_USERNAME` and `REGISTRY_PASSWORD` when set.
4. If no credentials are available, the service attempts an anonymous registry request.

For a fully neutral per-request conduit, leave `REGISTRY_BEARER_TOKEN`, `REGISTRY_USERNAME`, and `REGISTRY_PASSWORD` unset and use `/registry-auth/{scheme}/{host}/docker/{filename}`.

If static credentials are configured on the passthrough service, they can still be used by `/registry/...` requests. If the Cisco device must choose both the registry and the identity, keep those static credential variables unset and use `/registry-auth/...`.

### Static Credentials

```bash
REGISTRY_URL=https://registry.example.com
REGISTRY_NAME=registry.example.com
REGISTRY_USERNAME=my-user
REGISTRY_PASSWORD=my-token
```

For registries that accept Basic auth directly, leave `REGISTRY_AUTH_URL` empty:

```bash
REGISTRY_URL=https://registry.example.com
REGISTRY_NAME=registry.example.com
REGISTRY_AUTH_URL=
REGISTRY_USERNAME=my-user
REGISTRY_PASSWORD=my-token
```

For registries that provide a ready-to-use bearer token:

```bash
REGISTRY_URL=https://registry.example.com
REGISTRY_NAME=registry.example.com
REGISTRY_BEARER_TOKEN=<token>
```

## Cisco HTTP Client Credentials

Cisco IOS-XE HTTP client credentials are configured globally:

```text
configure terminal
ip http client username <registry-username>
ip http client password <registry-password-or-token>
end
```

The Cisco HTTP client normally does not send the `Authorization` header preemptively. When the passthrough service requires client credentials, it returns:

```text
HTTP/1.1 401 Unauthorized
WWW-Authenticate: Basic realm="registry credentials"
```

IOS-XE then retries the same URL with Basic auth. The passthrough service uses those credentials only as upstream registry credentials. It does not treat them as an application login for the passthrough service.

Some IOS-XE sessions echo the value entered for `ip http client password`. Treat registry tokens used this way as short-lived or rotate them after lab validation if they were visible in a terminal transcript.

Use one of these patterns:

| Pattern | Passthrough Setting | Cisco URL |
|---------|---------------------|-----------|
| Cisco supplies credentials for the default registry | `REGISTRY_CLIENT_AUTH=required` | `/docker/{filename}` |
| Cisco supplies registry and credentials per request | Leave static registry credential variables unset for a neutral conduit | `/registry-auth/{scheme}/{host}/docker/{filename}` |

Clean up Cisco HTTP client credentials when finished if they should not remain in running config:

```text
configure terminal
no ip http client username
no ip http client password
end
```

### Private GHCR Example

GHCR requires the package owner and package path in the repository name. A private image such as `ghcr.io/my-org/private-image:latest` is requested as:

```text
configure terminal
ip http client username <github-user>
ip http client password <github-token-with-package-read-permission>
end
copy http://<passthrough-ip>:8080/registry-auth/https/ghcr.io/docker/my-org__private-image_latest.tar flash:ghcr-image.tar
```

If the image path is nested, keep replacing repository slashes with double underscores:

```text
ghcr.io/my-org/platform/private-image:2026.06
copy http://<passthrough-ip>:8080/registry-auth/https/ghcr.io/docker/my-org__platform__private-image_2026.06.tar flash:ghcr-image.tar
```

## OpenTelemetry Tracing

The server supports OpenTelemetry instrumentation for distributed tracing. To enable it:

1. Copy `.env.example` to `.env` or export the variables directly
2. Set `OTEL_EXPORTER_OTLP_ENDPOINT` to your collector's gRPC endpoint

```bash
cp .env.example .env
# Edit .env and set OTEL_EXPORTER_OTLP_ENDPOINT=http://your-collector:4317
```

When enabled, the following spans are created within a single trace:

| Span Name | Type | Description |
|-----------|------|-------------|
| `GET /docker/{filename}` | Parent | Entire request lifecycle for the default registry route |
| `GET /registry/{scheme}/{host}/docker/{filename}` | Parent | Entire request lifecycle for request-selected registries |
| `GET /registry-auth/{scheme}/{host}/docker/{filename}` | Parent | Entire request lifecycle for request-selected registries with client-supplied credentials |
| `parse_image_request` | Child | Filename parsing, default namespace handling, and image/tag resolution |
| `extract_registry_client_credentials` | Child | Client Basic auth detection, validation, and challenge decisions |
| `resolve_manifest` | Child | Fetching and resolving Docker manifest from registry |
| `GET /v2/{repository}/manifests/{reference}` | Child | Registry manifest HTTP request attempts, including auth retries |
| `GET {registry_token_endpoint}` | Child | Registry bearer-token request, when token auth is required |
| `fetch_config_blob` | Child | Downloading image config blob |
| `GET /v2/{repository}/blobs/{digest}` | Child | Registry config or layer blob HTTP request attempts |
| `download_layer_N` | Child | Downloading and decompressing each layer into temporary storage |
| `write_layer_tar_N` | Child | Writing each prepared layer to the tar stream |

The request route span is the distributed root for the local operation. Image parsing, Cisco credential handling, registry token exchange, manifest resolution, config fetch, layer fetch, layer decompression, and response streaming all attach below that same context.

**Semantic Conventions Used:**

| Attribute | Convention | Description |
|-----------|------------|-------------|
| `http.request.method` | HTTP | Request method (GET) |
| `http.route` | HTTP | Route template |
| `url.path` | URL | Request path |
| `url.full` | URL | Full request URL |
| `url.scheme` | URL | URL scheme |
| `url.template` | URL | Low-cardinality upstream registry URL template for client spans |
| `server.address` | Server | Server address/host |
| `server.port` | Server | Server port |
| `client.address` | General | Client IP address |
| `network.peer.address` | Network | Connected peer address |
| `network.peer.port` | Network | Connected peer port |
| `network.protocol.name` | Network | Application protocol name (`http`) |
| `network.transport` | Network | Transport protocol (`tcp`) |
| `user_agent.original` | User-Agent | User agent |
| `http.response.status_code` | HTTP | Response status code |
| `http.request.resend_count` | HTTP | Upstream registry retry attempt count for auth challenge retries |
| `error.type` | Error | Stable error class or status code for failed operations |
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
| `registry.name` | Custom (Registry) | Registry display name |
| `registry.url` | Custom (Registry) | Registry V2 API URL |
| `registry.target.source` | Custom (Registry) | Whether the registry came from environment config or request URL |
| `registry.operation` | Custom (Registry) | Upstream registry operation such as manifest, token, config, or layer fetch |
| `registry.auth.configured` | Custom (Registry) | Whether a token endpoint was configured or inferred |
| `registry.auth.type` | Custom (Registry) | Non-secret upstream auth type (`none`, `basic`, or `bearer`) |
| `registry.auth.service` | Custom (Registry) | Registry auth service name, when present |
| `registry.auth.scope_present` | Custom (Registry) | Whether a token request included a pull scope |
| `registry.client_auth.mode` | Custom (Registry) | Cisco/client credential mode (`off`, `optional`, or `required`) |
| `registry.client_auth.credentials_present` | Custom (Registry) | Whether the request provided Basic credentials |
| `registry.client_auth.credentials_used` | Custom (Registry) | Whether client credentials were forwarded upstream for the request |
| `registry.client_auth.challenge_sent` | Custom (Registry) | Whether the passthrough service challenged the client for credentials |
| `registry.default_namespace` | Custom (Registry) | Namespace prepended for single-component image names |

If `OTEL_EXPORTER_OTLP_ENDPOINT` is empty or not set, tracing export is disabled and no overhead is added.

## Validated Scenarios

Automated tests cover filename parsing, registry target parsing, client Basic auth extraction, auth challenge behavior, registry token challenge handling, tar assembly, and telemetry attribute creation.

The Cisco app-hosting lab validation covered these copy, install, activate, start, and cleanup combinations:

| Scenario | Route | Registry | Credential Source | Result |
|----------|-------|----------|-------------------|--------|
| Public default registry | `/docker/{filename}` | Docker Hub | Anonymous | App installed and reached `RUNNING` |
| Request-selected registry | `/registry/http/{host}/docker/{filename}` | Local registry | Anonymous | App installed and reached `RUNNING` |
| Static private registry | `/docker/{filename}` | Docker Hub/local registry | Passthrough environment | App installed and reached `RUNNING` |
| Cisco-supplied credentials | `/docker/{filename}` with `REGISTRY_CLIENT_AUTH=required` | Docker Hub | Cisco HTTP client Basic auth | App installed and reached `RUNNING` |
| Cisco-selected registry and credentials | `/registry-auth/{scheme}/{host}/docker/{filename}` | Docker Hub/local registry | Cisco HTTP client Basic auth | App installed and reached `RUNNING` |
| Public GHCR | `/registry/https/ghcr.io/docker/{filename}` | GHCR | Anonymous | App installed and reached `RUNNING` |
| GHCR authenticated path | `/registry-auth/https/ghcr.io/docker/{filename}` | GHCR public image | Cisco HTTP client Basic auth | App installed and reached `RUNNING` |

Private GHCR uses the same `/registry-auth/https/ghcr.io/...` path, but it requires a package-readable GitHub token for the target private package. Validate it with an actual private package before relying on it for production private GHCR delivery.

## Architecture

- **Prepared Response**: Resolves the image and prepares layers before returning HTTP 200, so registry failures are reported as errors instead of successful empty tar files.
- **Streaming Transfer**: Uses chunked transfer encoding to stream the prepared archive to the client without holding the full tar archive in memory.
- **Registry API**: Directly communicates with the configured registry API (no local Docker daemon required for pulling).
- **Private Registry Auth**: Supports anonymous pulls, bearer-token challenges, username/password token exchange, direct Basic auth, and static bearer tokens.
- **Request-Selected Registries**: Allows a client to choose Docker Hub, GHCR, or an enterprise registry per request.
- **Client-Supplied Registry Credentials**: Allows Cisco HTTP client Basic credentials to be forwarded as upstream registry credentials for the current request.
- **Published Service Image**: Publishes the passthrough server as a multi-arch OCI image for `linux/amd64` and `linux/arm64`.
- **Payload Image Architecture**: Downloads `linux/amd64` payload images when converting registry images into Docker save archives.
- **Docker Save Format**: Creates tar archives compatible with `docker load`.

## API Reference

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/docker/{filename}` | GET | Download Docker image as tar from the configured default registry |
| `/registry/{scheme}/{host}/docker/{filename}` | GET | Download Docker image as tar from a registry selected in the request URL |
| `/registry-auth/{scheme}/{host}/docker/{filename}` | GET | Download Docker image as tar from a registry selected in the request URL, requiring HTTP Basic credentials from the client |
| `/health` | GET | Health check endpoint |

## Troubleshooting

| Symptom | Likely Cause | What To Check |
|---------|--------------|---------------|
| `400 Invalid filename format` | Filename does not end in `.tar`, has no underscore separator, or has an empty image/tag | Check the filename rules in [Image Filename Format](#image-filename-format) |
| `401 Registry credentials are required` | `/registry-auth/...` was used and the client did not send Basic auth yet | On Cisco, confirm `ip http client username` and `ip http client password`; the first unauthenticated request and retry are normal |
| `502 Failed to prepare image` | Registry rejected the request, image/tag does not exist, credentials are wrong, or no amd64/linux manifest exists | Check registry URL, image path, tag, credential source, and registry permissions |
| Docker Hub official image resolves unexpectedly on private registry | Docker Hub gets the implicit `library/` namespace; other registries do not unless configured | Set `REGISTRY_DEFAULT_NAMESPACE` or include the namespace explicitly in the filename |
| Cisco cannot parse a registry host with a port | Some clients dislike `:` inside the path segment | Percent-encode the port separator, for example `registry2.example.com%3A5000` |
| Private GHCR image fails | Token does not have package read permission or image path is wrong | Use a token with package read permission and encode nested paths with double underscores |
| Traces do not appear | OTLP endpoint is unset or collector is unreachable | Set `OTEL_EXPORTER_OTLP_ENDPOINT` and verify collector connectivity from the passthrough container |

## Limitations

- Docker save archive generation currently selects linux/amd64 payload images
- Large images may take time to prepare before the download response starts
- Temporary disk space is required for prepared layer tar files
- Static registry credentials are configured per running service instance; use `/registry-auth/...` when credentials need to vary per request
- The filename format cannot represent tags containing underscores
- The passthrough service trusts the requested registry host; restrict network access appropriately in production

## Contributing

We welcome contributions! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

Before contributing, please read our [Code of Conduct](CODE_OF_CONDUCT.md).

## Security

For security concerns, please see [SECURITY.md](SECURITY.md).

## License

This project is licensed under the Apache License 2.0 - see the [LICENSE](LICENSE) file for details.

Third-party notices, including the MIT-licensed Mermaid runtime used by the Zensical documentation and the MIT-licensed Zensical documentation build tool, are listed in [NOTICE](NOTICE).

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
