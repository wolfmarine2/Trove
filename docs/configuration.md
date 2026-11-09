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
| `http_client.proxy` | env | Default outbound proxy (http, https, socks5, socks5h). |
| `http_client.no_proxy` | — | Hosts bypassing `http_client.proxy`. |
| `http_client.ca_bundle` | — | Extra trusted CAs (PEM). |
| `log.json` | `false` | JSON lines for log shippers. |

## Outbound proxies

Proxy repositories fetch from their remotes over HTTP. The route is chosen per
repository (**Settings → Outbound proxy** in the UI, or
`config.proxy.outbound` in the API):

| Mode | Behaviour |
| ---- | --------- |
| `inherit` (default) | `http_client.proxy` / `http_client.no_proxy`; when unset, `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY` |
| `direct` | No proxy, even if the server default or environment sets one |
| `custom` | The repository's own proxy |

Supported proxy URLs:

* `http://host:3128` — HTTP proxy (CONNECT for https remotes, absolute-form for http remotes)
* `https://host:443` — HTTP proxy reached over TLS
* `socks5://host:1080` — SOCKS5, Trove resolves remote host names
* `socks5h://host:1080` — SOCKS5, the proxy resolves host names (use this when
  internal DNS cannot resolve public registries)

Credentials are given as username/password (Basic `Proxy-Authorization` for
HTTP proxies, RFC 1929 for SOCKS5) and are masked in the API. `no_proxy` takes
comma-separated hosts, `.domain` suffixes and CIDRs. Docker token requests use
the same route as the registry itself.

Example: Docker Hub through a SOCKS5 jump host, the npm mirror direct, PyPI
through the corporate Squid:

```json
{"name": "docker-hub", "format": "docker", "kind": "proxy", "config": {"proxy": {
  "remote_url": "https://registry-1.docker.io",
  "outbound": {"mode": "custom", "url": "socks5h://jump.corp:1080", "username": "svc-trove", "password": "…"}}}}
```

**Test connection** in the repository form (`POST /api/v1/remote-test`)
checks the remote through the selected route without saving.

## Runtime settings

Settings that can change without a restart are stored in the database and
edited under **System** in the UI or via `PUT /api/v1/system/settings`:

* `anonymous_read` (default **on**) — anyone who can reach Trove may browse,
  search and download from every repository, including `docker pull`, without
  an account. Publishing, deleting and administration always require a user
  with the right role. When off, every read needs credentials and Cargo's
  `config.json` advertises `auth-required`.
* `docker_default_repository` — see [client setup](formats.md).

The configuration value `security.anonymous_read` only seeds this setting on
first start; afterwards the value stored in the database wins.

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
