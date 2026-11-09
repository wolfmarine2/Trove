# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.3.0] - 2026-11-09

### Added
- Per-repository outbound proxy for proxy repositories: server default,
  direct, or a custom HTTP, HTTPS, SOCKS5 or SOCKS5h proxy with credentials
  and a bypass list. Docker token requests follow the same route.
- `http_client.proxy` accepts SOCKS5 URLs; new `http_client.no_proxy`.
- "Test connection" for remotes (`POST /api/v1/remote-test`), also in the
  repository form.
- Container build reworked for air-gapped pipelines: `Dockerfile` at the
  root, portable across Docker, Podman/Buildah and Kaniko, every base image
  and package source configurable (`BASE_REGISTRY`, `NPM_REGISTRY`,
  `CARGO_REGISTRY`, `APT_MIRROR`), cached dependency layer, OCI labels,
  default config baked in, Debian or openEuler runtime.
- `trove healthcheck` subcommand used by the image's `HEALTHCHECK`.
- Release workflow publishing a multi-arch (amd64/arm64) image to GHCR;
  CI smoke-tests the image.
- Argo Workflows + Kaniko template (`deploy/ci/argo-kaniko-build.yaml`).

### Changed
- Positioned as an internal public mirror: anonymous read stays on by default
  (now also in the production Kustomize overlay), and the generated client
  configuration omits credentials for downloads, showing tokens only for
  publishing or when anonymous read is off.

### Fixed
- The image no longer fails to start without a mounted configuration file.
- Minimum supported Rust version corrected to 1.94.

## [0.2.0] - 2026-11-09

### Added
- Docker / OCI proxy and group repositories (Registry API v2 under `/v2/`):
  pull-through cache for Docker Hub, GHCR, Quay, Harbor and other v2
  registries with automatic bearer-token negotiation, digest verification,
  tag revalidation, cross-repository layer deduplication, path-based routing
  and a default repository for `registry-mirrors`.
- APT proxy repositories (Debian, Ubuntu, Kylin, UOS) with by-hash support and
  index refresh tied to `InRelease`.
- YUM / DNF proxy repositories (openEuler, RHEL family, Fedora, EPEL) with
  repodata refresh tied to `repomd.xml`.
- Byte-range requests (resumable downloads) and `If-Modified-Since` on all
  downloads.

### Changed
- `http_client.timeout_secs` is now an idle (read) timeout so large downloads
  are not cut off.
- An asset's `updated_at` only changes when its content changes.

## [0.1.0] - 2026-11-09

### Added
- Hosted, proxy and group repositories for Maven, npm, PyPI, Cargo (sparse
  index) and raw files.
- Content-addressed blob storage on the local filesystem or S3-compatible
  stores, with deduplication and garbage collection.
- Metadata on SQLite (default) or PostgreSQL / openGauss.
- Users with viewer / deployer / admin roles, API tokens, web sessions,
  anonymous read switch, audit log.
- Web UI (React) in English and Simplified Chinese: repositories, client
  setup snippets, browsing with checksums, search, tokens, users, audit, system.
- Prometheus metrics, health and readiness endpoints, JSON logging.
- Dockerfile (amd64/arm64), docker-compose, Kustomize base and HA overlay.

[Unreleased]: https://github.com/wolfmarine2/Trove/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/wolfmarine2/Trove/releases/tag/v0.3.0
