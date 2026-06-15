# Architecture

The service is intentionally small: an HTTP server, a registry client, and a tar builder. The HTTP layer parses requests and handles Cisco/client credentials. The registry layer speaks the OCI Distribution API. The tar builder prepares an archive that IOS-XE app-hosting can install.

## Component view

```mermaid
flowchart TB
  subgraph client["Client side"]
    ios["Cisco IOS-XE HTTP client"]
    cli["copy http command"]
    apphosting["app-hosting"]
    cli --> ios --> apphosting
  end

  subgraph service["docker-http-passthrough"]
    router["Axum route handlers"]
    parser["Filename parser"]
    auth["Client credential extraction"]
    registry_client["Registry client"]
    tar_builder["Image tar builder"]
    stream["Chunked HTTP response stream"]

    router --> parser
    router --> auth
    parser --> registry_client
    auth --> registry_client
    registry_client --> tar_builder
    tar_builder --> stream
  end

  subgraph registries["Registries"]
    dockerhub["Docker Hub"]
    ghcr["GHCR"]
    private["Private registry registry2 or enterprise host"]
  end

  ios --> router
  registry_client --> dockerhub
  registry_client --> ghcr
  registry_client --> private
  stream --> ios
```

## Topology without client-supplied credentials

```mermaid
flowchart LR
  cisco["Cisco Device copy http"] --> passthrough["docker-passthrough container Docker or Kubernetes"]
  passthrough --> registry["Container Registry Docker Hub / GHCR / Private"]

  passthrough -. "REGISTRY_URL, REGISTRY_USERNAME, REGISTRY_PASSWORD optional" .-> config["Service environment"]
  config -. "static target and credentials" .-> passthrough
```

Use this mode when the passthrough service owner controls the registry destination and credentials.

## Topology with Cisco-supplied authentication

```mermaid
flowchart LR
  cisco["Cisco Device ip http client username/password"] --> passthrough["docker-passthrough container neutral conduit"]
  passthrough --> registry["Selected Registry Docker Hub / GHCR / registry2"]
  registry --> token["Registry token service when challenged"]
  token --> registry

  cisco -- "copy http /registry-auth/..." --> passthrough
  passthrough -- "Basic or Bearer upstream auth" --> registry
```

Use this mode when users do not control the passthrough container environment and need to provide all registry details from the Cisco CLI.

## Request lifecycle

```mermaid
sequenceDiagram
  autonumber
  participant Cisco as Cisco IOS-XE
  participant HTTP as Passthrough HTTP server
  participant Registry as Container registry
  participant Token as Token service
  participant Tar as Tar builder

  Cisco->>HTTP: GET /registry-auth/https/ghcr.io/docker/org__app_latest.tar
  HTTP-->>Cisco: 401 WWW-Authenticate: Basic
  Cisco->>HTTP: Retry GET with Authorization: Basic
  HTTP->>HTTP: Parse filename and route registry target
  HTTP->>Registry: GET /v2/org/app/manifests/latest
  Registry-->>HTTP: 401 WWW-Authenticate: Bearer realm=...
  HTTP->>Token: GET token with Basic credentials
  Token-->>HTTP: Bearer token
  HTTP->>Registry: Retry manifest with Bearer token
  Registry-->>HTTP: Manifest or manifest list
  HTTP->>Registry: GET config blob and layers
  Registry-->>HTTP: OCI config and layer streams
  HTTP->>Tar: Prepare Docker save archive
  Tar-->>Cisco: Stream image.tar
```

## Tar preparation pipeline

```mermaid
flowchart TD
  request["Parsed image reference"] --> manifest["Resolve linux/amd64 manifest"]
  manifest --> config["Fetch image config blob"]
  manifest --> layers["Fetch layer blobs"]
  layers --> detect["Detect compression"]
  detect -->|gzip| decompress["Decompress to tar"]
  detect -->|plain tar| keep["Keep layer tar"]
  decompress --> layer_files["Prepared layer temp files"]
  keep --> layer_files
  config --> manifest_json["Generate manifest.json"]
  layer_files --> archive["Stream Docker save tar"]
  manifest_json --> archive
```

The service prepares the image before committing a `200 OK` response so registry errors are returned as request failures instead of partial archives.
