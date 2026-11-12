# Trove

[![CI](https://github.com/wolfmarine2/Trove/actions/workflows/ci.yml/badge.svg)](https://github.com/wolfmarine2/Trove/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/wolfmarine2/Trove)](LICENSE)
[![Release](https://img.shields.io/github/v/release/wolfmarine2/Trove?sort=semver)](https://github.com/wolfmarine2/Trove/releases)
[![Container](https://img.shields.io/badge/ghcr.io-wolfmarine2%2Ftrove-blue)](https://github.com/wolfmarine2/Trove/pkgs/container/trove)

[中文说明](README.zh-CN.md)

Trove is a self-hosted universal artifact repository, in the spirit of Sonatype
Nexus Repository, built to be a company's **internal public mirror**: one
server that caches public registries (Maven Central, npm, PyPI, crates.io,
Docker Hub, Ubuntu, openEuler, GitHub, agent-skill marketplaces, …), hosts in-house packages and combines both
behind a single URL per ecosystem.

Downloads need no account by default, so build machines, CI runners and
developers just point their tools at Trove. Publishing and administration
require a user; anonymous read can be switched off for stricter setups.

It is a single Rust binary with an embedded React UI, runs on SQLite out of the
box and scales out on PostgreSQL (or openGauss) with S3-compatible storage.

| Format | Hosted (publish) | Proxy (cache a remote) | Group (merge) | Clients tested |
| ------ | ---------------- | ---------------------- | ------------- | -------------- |
| Maven  | ✓ release / snapshot policies, generated checksums | ✓ | ✓ merged `maven-metadata.xml` | Maven 2 layout over HTTP |
| npm    | ✓ publish, dist-tags, unpublish, `npm login` | ✓ | ✓ merged packuments | npm 10 |
| PyPI   | ✓ twine upload (PEP 503 simple index) | ✓ JSON and HTML indexes, hash-verified | ✓ | pip, twine, uv |
| Cargo  | ✓ sparse index, publish, yank | ✓ crates.io, checksum-verified | ✓ | cargo 1.97 |
| Raw    | ✓ any file: web upload (files/folders) or HTTP PUT | ✓ | ✓ | curl, browser |
| Docker / OCI | — (push planned) | ✓ Docker Hub, GHCR, Quay, Harbor…, token auth, digest-verified | ✓ | Registry API v2 (docker, containerd) |
| APT    | — | ✓ Debian, Ubuntu, Kylin, UOS; by-hash aware | — | apt 2.7 |
| YUM / DNF | — | ✓ openEuler, RHEL family, Fedora, EPEL | — | Registry layout (dnf) |
| Git    | — | ✓ GitHub, Gitee, GitLab…: mirrored clones, source archives, raw files | — | git 2.43, pnpm `github:` deps, DeepSeek Harness / Claude Code plugins |
| Skills | — | ✓ SkillHub (skillhub.cn), ClawHub and compatible skill marketplaces | — | ClawHub HTTP API, curl |

## Why

* **Air-gap friendly.** No external services, no CDN, fonts bundled. Each
  proxy repository picks its own outbound route (server default, direct, or an
  HTTP/HTTPS/SOCKS5 proxy with credentials), custom CA bundles are supported,
  and a proxy can be switched to cache-only mode.
* **Private where it matters.** Any repository can require sign-in to read
  (certificates with private keys, keystores, internal tools) while the rest
  stays anonymous.
* **Auditable.** Every login, publish, delete and configuration change is
  written to an audit log viewable in the UI and emitted as structured logs.
* **Small and predictable.** Content-addressed storage deduplicates identical
  files across repositories; garbage collection reclaims unreferenced blobs.
* **Operable.** `/healthz`, `/readyz`, Prometheus `/metrics`, JSON logs,
  Kustomize manifests for single-node and HA deployments.

## Quick start

```sh
# Prerequisites: Rust 1.94+, Node.js 20+
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

## Community

* Questions, ideas and show-and-tell: [GitHub Discussions](https://github.com/wolfmarine2/Trove/discussions)
* Bugs and feature requests: [issues](https://github.com/wolfmarine2/Trove/issues/new/choose)
* Security vulnerabilities: report privately, see [SECURITY.md](SECURITY.md)

Contributions are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) and the
[code of conduct](CODE_OF_CONDUCT.md); good first areas are new package
formats, client snippets and documentation. Changes are listed in
[CHANGELOG.md](CHANGELOG.md).

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
