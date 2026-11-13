# Deployment

## Single binary

```sh
make release                         # target/release/trove with the UI embedded
./target/release/trove --config /etc/trove/trove.toml
```

On first start Trove creates the `admin` user. Without
`security.initial_admin_password` the generated password is written to
`<data_dir>/admin.password`. Change it and delete the file.

A systemd unit:

```ini
[Unit]
Description=Trove artifact repository
After=network-online.target

[Service]
User=trove
ExecStart=/usr/local/bin/trove --config /etc/trove/trove.toml serve
Restart=on-failure
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

## Container image

The `Dockerfile` at the repository root builds a single image (UI embedded,
non-root UID 10001, state in `/data`, default config at
`/etc/trove/trove.toml`). It avoids BuildKit-only syntax, so Docker, Podman,
Buildah and Kaniko all build it the same way.

```sh
make docker                                  # trove:<version> for this machine
make docker-run                              # try it: http://localhost:8081, admin / admin12345
make docker-multiarch IMAGE=harbor.corp/platform/trove   # amd64 + arm64 (Kunpeng), pushed
```

### Build arguments

| Argument | Default | Purpose |
| -------- | ------- | ------- |
| `BASE_REGISTRY` | — | Prefix for all base images, e.g. `harbor.corp/dockerhub/library/` |
| `NODE_IMAGE` | `node:22-bookworm-slim` | UI build stage |
| `RUST_IMAGE` | `rust:1.97-bookworm` | Server build stage (Rust ≥ 1.94) |
| `RUNTIME_IMAGE` | `debian:bookworm-slim` | Final image; Debian/Ubuntu or openEuler/RHEL-family bases are supported |
| `NPM_REGISTRY` | npmjs | npm registry for the UI build |
| `CARGO_REGISTRY` | crates.io | Cargo sparse index used via source replacement |
| `APT_MIRROR`, `APT_SECURITY_MIRROR` | Debian | Debian mirrors for build tools |
| `VERSION`, `REVISION` | — | OCI image labels |

### Air-gapped builds

Once one Trove instance runs, it can feed every later build: create the proxy
repositories `npm-public`, `cargo-public` (crates.io), `debian`
(`http://deb.debian.org/debian/`) and `debian-security`
(`http://security.debian.org/debian-security/`), then:

```sh
docker build \
  --build-arg BASE_REGISTRY=harbor.corp/dockerhub/library/ \
  --build-arg NPM_REGISTRY=http://trove.corp/repository/npm-public/ \
  --build-arg CARGO_REGISTRY=sparse+http://trove.corp/repository/cargo-public/index/ \
  --build-arg APT_MIRROR=http://trove.corp/repository/debian/ \
  --build-arg APT_SECURITY_MIRROR=http://trove.corp/repository/debian-security/ \
  -t harbor.corp/platform/trove:0.3.0 .
```

Builds take 5–10 minutes from scratch; the dependency layer is cached until
`Cargo.toml` or `Cargo.lock` change, so code-only changes rebuild in a fraction
of that.

### openEuler runtime

```sh
docker build --build-arg RUNTIME_IMAGE=harbor.corp/openeuler/openeuler:24.03-lts -t trove:0.3.0-oe .
```

The binary is built on Debian bookworm (glibc 2.36) and therefore needs a
runtime with glibc ≥ 2.36: openEuler 24.03 LTS, Kylin V10 SP3+ with a recent
glibc, Debian 12+, Ubuntu 22.04+. For openEuler 22.03 (glibc 2.34) also build
the server stage on an older base.

### Kaniko and Argo Workflows

`deploy/ci/argo-kaniko-build.yaml` is a WorkflowTemplate that clones the
repository from Forgejo, builds natively per architecture with Kaniko using
the build arguments above, caches layers in Harbor and pushes the image.

### Images built on GitHub

`.github/workflows/release.yml` builds the image with GitHub Actions, natively
on amd64 and arm64 runners, and publishes one multi-arch manifest to
`ghcr.io/<owner>/trove`. Images are published **only for releases**, so every
tag is a version you can pin:

| Image tag | Meaning |
| --------- | ------- |
| `0.3.0` | exact release (recommended for deployments) |
| `0.3` | newest patch release of 0.3 |
| `latest` | newest release |

Two ways to release version `X.Y.Z` (bump `version` in `Cargo.toml` and the
CHANGELOG on `main` first):

* push a tag: `git tag -a vX.Y.Z -m "Trove X.Y.Z" && git push origin vX.Y.Z`
* or **Actions → Container image → Run workflow** on `main`: it releases the
  version in `Cargo.toml` and creates the `vX.Y.Z` tag after the image is
  published

Either way a GitHub Release with the CHANGELOG notes is created. A tag must
match `Cargo.toml`, and an existing version is never overwritten. New GHCR
packages are private; set the package visibility to public once so anyone can
pull. Pushes to `main` only run CI, which builds and smoke-tests the image
without publishing it.

### Running

```sh
docker run -d --name trove -p 8081:8081 -v trove-data:/data \
  -e TROVE__SECURITY__INITIAL_ADMIN_PASSWORD=change-me-now \
  -e TROVE__SERVER__BASE_URL=https://trove.corp.example \
  trove:0.3.0
```

* Configuration: `TROVE__<SECTION>__<KEY>` variables, or mount a file over
  `/etc/trove/trove.toml`.
* The container runs with a read-only root filesystem (`--read-only`); only
  `/data` must be writable.
* `HEALTHCHECK` runs `trove healthcheck`; Kubernetes uses `/healthz` and
  `/readyz`.
* `docker-compose.yml` starts Trove with PostgreSQL.

## Kubernetes

* `deploy/kubernetes/base` — one replica, SQLite and blobs on a PVC. Good for
  teams and evaluation.
* `deploy/kubernetes/overlays/production` — two or more replicas spread across
  zones, PostgreSQL (or openGauss) and an S3-compatible bucket, Ingress tuned
  for large uploads, PodDisruptionBudget.

```sh
kubectl apply -k deploy/kubernetes/base
kubectl apply -k deploy/kubernetes/overlays/production
```

With Argo CD point an Application at the overlay directory.

Probes: `/healthz` (process alive) and `/readyz` (database reachable).
Metrics: `/metrics` in Prometheus text format (downloads, uploads, proxy cache
hits/fetches, upstream errors, auth failures, per-repository asset counts and
sizes, blob totals).

### Scaling notes

* SQLite allows exactly one replica. Use PostgreSQL for more.
* Filesystem storage across replicas requires a shared `ReadWriteMany` volume
  (CephFS); S3 is simpler.
* Proxy caches and single-flight locks are per process; two replicas may each
  download a missing file once. Content is deduplicated so this only costs
  bandwidth.

## Reverse proxy

Trove emits absolute URLs in some metadata (npm tarball links, PyPI file
links, Cargo `config.json`, skills JSON), so it must know the public URL.

**Same paths** (recommended): forward everything unchanged; clients use
`https://mirrors.example.com/repository/<repo>/`, and the UI, `/api` and
`/v2/` work on the same host.

```nginx
server {
    listen 443 ssl;
    server_name mirrors.example.com;
    client_max_body_size 0;          # uploads/publishing
    proxy_request_buffering off;
    proxy_read_timeout 600s;
    location / {
        proxy_pass http://127.0.0.1:8081;   # no URI part: paths unchanged
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto https;
    }
}
```

**Short paths** (`https://mirrors.example.com/<repo>/`): proxy `/` to
`/repository/` and tell Trove where repositories are, otherwise npm, pip and
cargo get links containing `/repository/` that the proxy maps to
`/repository/repository/…` (404). APT, YUM, Maven and raw use relative paths
and work either way.

```nginx
    location / {
        proxy_pass http://127.0.0.1:8081/repository/;
        ...
    }
    location /v2/ {                      # Docker, if served on this host
        proxy_pass http://127.0.0.1:8081/v2/;
        ...
    }
```

```toml
[server]
repository_base_url = "https://mirrors.example.com"   # TROVE__SERVER__REPOSITORY_BASE_URL
```

Disable request buffering and body
size limits for `/repository/` and `/v2/`; artifacts and image layers can be
gigabytes. Docker clients require HTTPS (or an `insecure-registries` entry), so
terminate TLS in front of Trove when serving Docker repositories. Forward
`X-Forwarded-Proto` so session cookies get the `Secure` flag.

## Backup and restore

Back up the database and the blob store together, database first:

* SQLite: `sqlite3 data/trove.db ".backup trove.db.bak"`, then copy
  `data/blobs`.
* PostgreSQL: `pg_dump`, then snapshot/replicate the bucket.

Blobs referenced by the database but missing from storage return errors;
extra blobs are reclaimed by garbage collection.

## Upgrades

Migrations run automatically at start-up. Take a backup, roll out the new
version; with PostgreSQL rolling updates are safe within a minor version.

## Troubleshooting outbound proxies

`tunnel error: unsuccessful` means the outbound HTTP proxy refused to open an
HTTPS tunnel (`CONNECT host:443`); plain `http://` remotes do not use
CONNECT, which is why they may work through the same proxy. Press **Test
connection** in the repository settings: Trove repeats the CONNECT itself and
shows the proxy's answer, for example
`answered CONNECT repo.openeuler.org:443 with "HTTP/1.1 403 Forbidden" [Server: squid]`.

| Proxy answer | Meaning | Fix |
| ------------ | ------- | --- |
| 403 | Policy/whitelist forbids the host or port 443 | Have the host allowed, or use an `http://` mirror |
| 407 | Credentials required | Proxy username/password (Basic only; not NTLM/Kerberos) |
| 400 / 405 / 501 | This port does not support CONNECT | Check the proxy port |
| 502 / 503 / 504 | The proxy cannot reach the host | Proxy's DNS/firewall/parent proxy |
| not HTTP | Wrong protocol | `socks5h://` for SOCKS ports, `https://` for TLS proxies |

The same check by hand from inside the container network:
`curl -v -x http://proxy:3128 https://repo.openeuler.org/` (look at the line
after `CONNECT`).
