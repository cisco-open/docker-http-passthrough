# Licensing

Docker HTTP Passthrough is licensed under the Apache License 2.0. The canonical project license is in the repository `LICENSE` file, and attribution notices are in the repository `NOTICE` file.

## Third-party documentation components

| Component | Use | License |
|-----------|-----|---------|
| Mermaid | Runtime diagram rendering for Zensical documentation pages | MIT License |
| Zensical | Static site generation for the documentation in `docs/zensical` | MIT License |

The project does not vendor the Zensical package into the repository. The generated documentation loads Mermaid from jsDelivr at runtime through `docs/zensical/javascripts/mermaid.mjs`.

## Notice handling

Third-party notices are kept in the repository `NOTICE` file so release artifacts, source distributions, and documentation all reference the same attribution source.
