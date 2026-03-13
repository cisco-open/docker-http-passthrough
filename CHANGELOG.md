# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
