# Architecture

## Components

```
               ┌──────────────── trove-server ────────────────┐
 build tools → │ /repository/{name}/…  format handlers         │
 web UI      → │ /api/v1/…             management API          │
 Prometheus  → │ /metrics /healthz /readyz                     │
               │        │                     │                │
               │   content.rs (resolve hosted/proxy/group)     │
               │        │            proxy.rs (upstream cache) │
               └────────┼─────────────────────┼────────────────┘
                        │                     │
            trove-db (metadata)       trove-storage (blobs)
          SQLite │ PostgreSQL       filesystem │ S3 / MinIO / Ceph
```

* **trove-core** — domain types (`Repository`, `Asset`, `BlobInfo`, `User`,
  `Token`), configuration loading with `TROVE__*` overrides, error type,
  helpers (path normalisation, PEP 503, Cargo index paths). No I/O.
* **trove-db** — all SQL. Uses the sqlx `Any` driver so one set of portable
  queries runs on SQLite and PostgreSQL; migrations are embedded.
* **trove-storage** — content-addressed blob store. Uploads stream to a temp
  file while SHA-1/256/512 and MD5 are computed, then move to
  `sha256/aa/bb/<sha256>`. Duplicate content is stored once.
* **trove-server** — axum HTTP server. `formats/` implements each package
  protocol; `api/` is the management REST API; `ui.rs` serves the embedded UI.

## Data model

* A **repository** has a format (maven, npm, pypi, cargo, raw), a kind
  (hosted, proxy, group) and a JSON config (write policy, proxy remote,
  group members, Maven version policy).
* An **asset** is a path inside a repository pointing at a **blob** by SHA-256.
  Format-specific attributes (upstream URL, cache timestamps, Python
  `requires_python`, crate description) live in a JSON column.
* Deleting or overwriting an asset never deletes its blob directly. The
  garbage collector removes blobs no asset references, after a grace period,
  under an exclusive lock that uploads respect.

## Request flow

1. `/repository/{name}/{path}` resolves the repository and authenticates the
   caller (Bearer token, raw Cargo token, Basic password or token, session
   cookie).
2. The format handler interprets the path. Reads go through a resolver:
   * **hosted** — database lookup;
   * **proxy** — cached asset if fresh, otherwise a conditional upstream
     request (`If-None-Match`/`If-Modified-Since`); content files are
     immutable, metadata is refreshed after `metadata_max_age_secs`; stale
     cache is served if the remote is down; 404s are remembered for 60 s;
     concurrent misses for the same file share one download;
   * **group** — members in order. A failing member is logged and skipped.
3. Metadata documents are merged for groups (Maven `maven-metadata.xml`, npm
   packuments, PyPI file lists, Cargo index lines) and URLs inside them are
   rewritten to point at the repository the client asked, so clients never
   learn about members.
4. Responses carry `ETag` (SHA-256), `Last-Modified` and
   `X-Checksum-Sha1/Sha256/Md5`.

## Security model

* Roles: **viewer** (read), **deployer** (read + publish + yank),
  **admin** (everything, including deletes and configuration).
* Anonymous read is a global switch; publishing always requires a user.
* Passwords use Argon2id. Basic-auth verifications are cached for five minutes
  in memory because build tools send credentials on every request.
* API tokens (`trv_…`) are stored as SHA-256 hashes; the secret is shown once.
* UI sessions are HttpOnly, SameSite=Strict cookies. Mutating API calls with
  a cookie additionally require the `X-Trove-Request` header (CSRF).
* Paths are normalised and traversal is rejected; upstream downloads are
  checked against published hashes (PyPI sha256, Cargo cksum, npm integrity on
  publish).

## Roadmap

* Docker / OCI distribution (hosted, proxy, group)
* Go module proxy, Helm, NuGet, RubyGems, Conda
* Per-repository permissions and content selectors
* LDAP and OIDC sign-in
* Cleanup policies (age, last download, snapshot retention)
* Range requests and resumable downloads
* Background re-validation and pre-fetching for proxies
* Replication between sites
