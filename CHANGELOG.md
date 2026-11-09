# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

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
