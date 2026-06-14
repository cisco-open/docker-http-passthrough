# Registry Support

The service supports Docker Registry HTTP API V2-compatible registries. Registry choice can be static through environment variables or dynamic through the request URL.

## Registry behavior

| Registry | Route host | Auth behavior | Namespace behavior |
|----------|------------|---------------|--------------------|
| Docker Hub | `docker.io`, `index.docker.io`, or `registry-1.docker.io` | Uses `https://auth.docker.io/token` and service `registry.docker.io` | Single-component images default to `library/` |
| GHCR | `ghcr.io` | Uses registry Bearer challenge discovery | Repository path must include owner/package |
| Enterprise registry | `registry2.example.com` or `registry2.example.com:5000` | Anonymous, Basic, or Bearer challenge depending on registry | No default namespace unless configured |
| IP registry | `10.10.20.30:5000` | Same as enterprise registry | No default namespace unless configured |

## Docker Hub alias handling

```mermaid
flowchart TD
  host["Requested host"] --> alias{"Docker Hub alias?"}
  alias -->|docker.io| canonical["https://registry-1.docker.io"]
  alias -->|index.docker.io| canonical
  alias -->|registry-1.docker.io| canonical
  canonical --> auth["https://auth.docker.io/token"]
  canonical --> service["service=registry.docker.io"]
  canonical --> namespace["default namespace=library"]
  alias -->|other host| generic["{scheme}://{host}"]
  generic --> challenge["Discover Bearer auth from WWW-Authenticate"]
```

## GHCR path encoding

GHCR image paths include the owner and package name:

```text
ghcr.io/my-org/private-image:latest
```

The route filename encodes repository slashes as double underscores:

```text
my-org__private-image_latest.tar
```

Nested package paths follow the same rule:

```text
ghcr.io/my-org/platform/private-image:2026.06
my-org__platform__private-image_2026.06.tar
```

## Enterprise registry flow

```mermaid
sequenceDiagram
  autonumber
  participant Cisco
  participant Service
  participant Registry2 as registry2.example.com:5000

  Cisco->>Service: GET /registry-auth/http/registry2.example.com:5000/docker/team__api_1.0.tar
  Service-->>Cisco: Basic challenge when credentials are required
  Cisco->>Service: Retry with configured HTTP client credentials
  Service->>Registry2: GET /v2/team/api/manifests/1.0
  Registry2-->>Service: Manifest or auth challenge
  Service->>Registry2: Fetch config and layers with selected auth
  Registry2-->>Service: Image blobs
  Service-->>Cisco: team/api:1.0 Docker save archive
```

## Image architecture

When the source image is a manifest list or OCI index, the service selects the `linux/amd64` payload image for the Docker save archive. The passthrough service container itself can be published as a multi-arch OCI image for `linux/amd64` and `linux/arm64`.
