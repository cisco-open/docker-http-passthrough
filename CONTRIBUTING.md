# Contributing to Docker HTTP Passthrough

Thank you for your interest in contributing to this project! We welcome contributions from the community.

## Code of Conduct

This project follows the [Cisco Open Source Code of Conduct](CODE_OF_CONDUCT.md). By participating, you are expected to uphold this code.

## How to Contribute

### Reporting Issues

- Check existing issues to avoid duplicates
- Use the issue template when available
- Provide clear reproduction steps
- Include relevant environment details (OS, Rust version, Docker version)

### Submitting Changes

1. **Fork the repository** and create your branch from `main`
2. **Write clear, concise commit messages**
3. **Add tests** for new functionality
4. **Ensure all tests pass**: `cargo test --locked --all-targets --all-features`
5. **Run lints**: `cargo clippy --locked --all-targets --all-features -- -D warnings`
6. **Format code**: `cargo fmt --check`
7. **Update documentation** when routes, registry behavior, telemetry, or Cisco workflows change
8. **Submit a pull request**

### Pull Request Guidelines

- Keep PRs focused on a single change
- Update documentation if needed
- Add entries to CHANGELOG.md for notable changes
- Ensure CI passes before requesting review

### Development Setup

```bash
# Clone your fork
git clone https://github.com/YOUR_USERNAME/docker-http-passthrough.git
cd docker-http-passthrough

# Build
cargo build

# Run tests
cargo test --locked --all-targets --all-features

# Run with debug logging
RUST_LOG=debug cargo run

# Run lints
cargo clippy --locked --all-targets --all-features -- -D warnings

# Check formatting
cargo fmt --check
```

For registry changes, include coverage for the affected route mode:

- `/docker/{filename}` for default registry behavior
- `/registry/{scheme}/{host}/docker/{filename}` for request-selected registries
- `/registry-auth/{scheme}/{host}/docker/{filename}` for client-supplied credentials
- telemetry attributes and parent/child span relationships for new request or registry operations

For container image changes, validate the local OCI image build path:

```bash
docker buildx bake image-local
```

### Commit Message Format

Use conventional commits:

```
type(scope): description

[optional body]

[optional footer]
```

Types: `feat`, `fix`, `docs`, `style`, `refactor`, `test`, `chore`

Examples:
- `feat(registry): add support for private registries`
- `fix(tar): handle empty layers correctly`
- `docs: update API documentation`

## Licensing

By contributing, you agree that your contributions will be licensed under the Apache License 2.0. All new files should include the SPDX license header:

```rust
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Cisco Systems, Inc.
```

## Questions?

Open an issue with the `question` label or reach out to the maintainers.
