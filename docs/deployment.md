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

```sh
docker build -f deploy/docker/Dockerfile -t trove:0.1.0 .
# multi-arch (x86_64 + Kunpeng / ARM64)
docker buildx build --platform linux/amd64,linux/arm64 -f deploy/docker/Dockerfile -t registry.example.com/trove:0.1.0 --push .
# openEuler runtime base
docker build --build-arg RUNTIME_IMAGE=openeuler/openeuler:24.03-lts -f deploy/docker/Dockerfile .
```

The image runs as UID 10001, keeps state in `/data` and reads
`/etc/trove/trove.toml`.

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

Set `server.base_url` to the public URL. Disable request buffering and body
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
