# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Request-selected registry downloads with `/registry/{scheme}/{host}/docker/{filename}`.
- Client-supplied registry credentials with `/registry-auth/{scheme}/{host}/docker/{filename}`.
- Cisco HTTP client credential forwarding for private registry pulls.
- Docker Hub alias handling, GHCR support, and enterprise registry host selection.
- Double-underscore image path encoding for nested repositories.
- OpenTelemetry spans and attributes for route selection, client credential handling, registry token exchange, manifest resolution, config fetches, layer downloads, and tar streaming.
- Interactive documentation site with URL builder, topology views, execution flow, telemetry view, and copyable examples.
- GHCR multi-arch OCI image publishing workflow for `linux/amd64` and `linux/arm64`.
- Docker Buildx Bake targets for local, non-pushed, and pushed OCI image builds.

### Changed
- Docker Hub official images now keep the `library/` default namespace while non-Docker Hub registries do not receive an implicit namespace unless configured.
- Image preparation completes before a success response starts, so registry failures return HTTP errors instead of partial archives.
- Docker build now targets Rust 1.86 or newer.
- Kubernetes deployment now references `ghcr.io/cisco-open/docker-http-passthrough:latest` by default.

### Security
- Registry credentials are not embedded in filenames or URLs.
- Client-supplied Basic credentials are used only for the current upstream registry request and are not persisted by the passthrough service.
- Static service-level registry credentials remain optional for deployments that need a fixed default registry.

## [0.1.0] - 2026-01-27

### Added
- Initial release
- HTTP endpoint for downloading Docker images as tar archives
- Support for Docker Hub public images
- Streaming response with chunked transfer encoding
- OpenTelemetry tracing support
- Health check endpoint
- Multi-architecture manifest resolution (defaults to linux/amd64)
- Kubernetes deployment manifest
- Docker Compose configuration

### Security
- Uses rustls for TLS (no OpenSSL dependency)
- HTTPS-only communication with Docker registry

[Unreleased]: https://github.com/cisco-open/docker-http-passthrough/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/cisco-open/docker-http-passthrough/releases/tag/v0.1.0
