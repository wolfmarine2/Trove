# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

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
