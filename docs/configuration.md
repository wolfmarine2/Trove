# Configuration

Trove reads a TOML file (default `trove.toml`, or `--config`, or the
`TROVE_CONFIG` environment variable). Every key can be overridden by an
environment variable named `TROVE__<SECTION>__<KEY>`:

```sh
TROVE__DATABASE__URL=postgres://trove:secret@db/trove
TROVE__STORAGE__KIND=s3
TROVE__STORAGE__S3__BUCKET=trove
TROVE__SECURITY__ANONYMOUS_READ=false
```

`trove config` prints the effective configuration with secrets redacted.

The annotated reference is [`config/trove.example.toml`](../config/trove.example.toml).
The most important settings:

| Key | Default | Notes |
| --- | ------- | ----- |
| `server.bind` | `0.0.0.0:8081` | |
| `server.base_url` | derived from request | Set behind a reverse proxy so generated URLs are correct. |
| `server.data_dir` | `data` | SQLite file, filesystem blobs, temp files. |
| `server.max_upload_mb` | `4096` | Enforced while streaming. |
| `database.url` | SQLite in `data_dir` | `postgres://…` for PostgreSQL or openGauss. |
| `storage.kind` | `filesystem` | `s3` for S3, MinIO, Ceph RGW. |
| `security.initial_admin_password` | random | Used once, when no user exists. |
| `security.anonymous_read` | `true` | Initial value; changed later in the UI. |
| `http_client.proxy` | env | Outbound proxy for remote registries. |
| `http_client.ca_bundle` | — | Extra trusted CAs (PEM). |
| `log.json` | `false` | JSON lines for log shippers. |

## Runtime settings

Settings that can change without a restart are stored in the database and
edited under **System** in the UI or via `PUT /api/v1/system/settings`:

* `anonymous_read` — allow downloads without signing in.

## Administration commands

```sh
trove serve                                   # default
trove config                                  # print effective configuration
trove reset-password admin --password '...'   # recover access on the host
```

## openGauss

openGauss speaks the PostgreSQL protocol. Use a user created with
`password_encryption_type = 1` (MD5 + SHA-256) or `0` (MD5) so the PostgreSQL
client can authenticate, or put a PgBouncer in front that handles SHA-256.
