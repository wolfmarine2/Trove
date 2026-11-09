# Trove

[中文说明](README.zh-CN.md)

Trove is a self-hosted universal artifact repository, in the spirit of Sonatype
Nexus Repository: one server that hosts your private packages, caches public
registries and combines both behind a single URL per ecosystem.

It is a single Rust binary with an embedded React UI, runs on SQLite out of the
box and scales out on PostgreSQL (or openGauss) with S3-compatible storage.

| Format | Hosted (publish) | Proxy (cache a remote) | Group (merge) | Clients tested |
| ------ | ---------------- | ---------------------- | ------------- | -------------- |
| Maven  | ✓ release / snapshot policies, generated checksums | ✓ | ✓ merged `maven-metadata.xml` | Maven 2 layout over HTTP |
| npm    | ✓ publish, dist-tags, unpublish, `npm login` | ✓ | ✓ merged packuments | npm 10 |
| PyPI   | ✓ twine upload (PEP 503 simple index) | ✓ JSON and HTML indexes, hash-verified | ✓ | pip, twine, uv |
| Cargo  | ✓ sparse index, publish, yank | ✓ crates.io, checksum-verified | ✓ | cargo 1.97 |
| Raw    | ✓ any file over HTTP PUT | ✓ | ✓ | curl |
| Docker / OCI | — (push planned) | ✓ Docker Hub, GHCR, Quay, Harbor…, token auth, digest-verified | ✓ | Registry API v2 (docker, containerd) |
| APT    | — | ✓ Debian, Ubuntu, Kylin, UOS; by-hash aware | — | apt 2.7 |
| YUM / DNF | — | ✓ openEuler, RHEL family, Fedora, EPEL | — | Registry layout (dnf) |

## Why

* **Air-gap friendly.** No external services, no CDN, fonts bundled. Proxies
  honour an outbound HTTP proxy and a custom CA bundle; a proxy can be switched
  to cache-only mode.
* **Auditable.** Every login, publish, delete and configuration change is
  written to an audit log viewable in the UI and emitted as structured logs.
* **Small and predictable.** Content-addressed storage deduplicates identical
  files across repositories; garbage collection reclaims unreferenced blobs.
* **Operable.** `/healthz`, `/readyz`, Prometheus `/metrics`, JSON logs,
  Kustomize manifests for single-node and HA deployments.

## Quick start

```sh
# Prerequisites: Rust 1.85+, Node.js 20+
make web                      # build the UI into web/dist
make run                      # serve on http://127.0.0.1:8081 (admin / admin12345)
```

Or with containers (PostgreSQL included):

```sh
docker compose up -d          # http://localhost:8081, admin / change-me-now
```

Then sign in, open **Repositories → Create repository**, and follow the
**Set up client** tab of the new repository. A typical layout per ecosystem is:

```
npm-hosted   (hosted)  ─┐
npm-proxy    (proxy → https://registry.npmjs.org/) ─┤→ npm-public (group)  ← clients use this
```

Put hosted repositories first in a group so that private packages always win
over public packages with the same name (dependency-confusion protection).

## Documentation

* [Architecture](docs/architecture.md): crates, storage model, request flow
* [Configuration](docs/configuration.md): every setting, environment overrides
* [Client setup](docs/formats.md): Maven, Gradle, npm, pip, twine, uv, Cargo, curl
* [Deployment](docs/deployment.md): Docker, Kubernetes, HA, backups, upgrades
* [REST API](docs/api.md): management API used by the UI and automation

## Project layout

```
crates/
  trove-core      domain model, configuration, errors (no I/O)
  trove-db        metadata store on SQLite / PostgreSQL via sqlx
  trove-storage   content-addressed blob store (filesystem / S3)
  trove-server    HTTP server: format handlers, management API, UI hosting
web/              React + TypeScript UI (Vite)
deploy/           Dockerfile and Kustomize manifests
config/           example configuration
docs/             documentation
```

## Status

Trove 0.2 covers the day-to-day workflows of the formats above. Docker image
push (hosted), hosted APT/YUM, Go modules, Helm charts, per-repository
permissions, LDAP/OIDC and cleanup policies are on the
[roadmap](docs/architecture.md#roadmap).

## License

Apache License 2.0. See [LICENSE](LICENSE).
