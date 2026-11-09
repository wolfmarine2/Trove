# REST API

Base path `/api/v1`. Authenticate with `Authorization: Bearer <token>`, Basic
auth (password or token), or the UI session cookie (mutating requests then also
need `X-Trove-Request: 1`). Errors are `{"error": "message"}` with a matching
HTTP status. Timestamps are milliseconds since the Unix epoch.

| Method | Path | Role | Description |
| ------ | ---- | ---- | ----------- |
| GET | `/status` | — | Version, anonymous-read flag |
| GET | `/stats` | read | Counts, storage totals, recent uploads |
| POST | `/auth/login` | — | `{username, password}` → sets session cookie |
| POST | `/auth/logout` | user | Ends the session |
| GET | `/auth/me` | user | Current user |
| PUT | `/auth/me/password` | user | `{current, new}` |
| GET | `/repositories` | read | List with URL, file count and size |
| POST | `/repositories` | admin | `{name, format, kind, description, online, config}` |
| GET | `/repositories/{name}` | read | |
| PUT | `/repositories/{name}` | admin | `{description?, online?, config?}`; format, kind and name are immutable |
| DELETE | `/repositories/{name}` | admin | Refused while a group references it |
| GET | `/repositories/{name}/browse?path=` | read | Immediate children of a path |
| POST | `/repositories/{name}/invalidate-cache` | admin | Proxy only: refetch metadata on next request |
| POST | `/remote-test` | admin | `{format, proxy, name?}` → `{ok, url, status, elapsed_ms, message}`; checks a remote through its outbound route without saving |
| GET | `/assets?q=&repo=&limit=&offset=` | read | Search by path substring |
| GET | `/assets/{id}` | read | Asset with checksums |
| DELETE | `/assets/{id}` | admin | |
| GET/POST | `/users` | admin | List / create `{username, password, role, display_name, email}` |
| PUT/DELETE | `/users/{id}` | admin | Update (role, disabled, password) / delete |
| GET/POST | `/tokens` | user | Own API tokens / create `{name, expires_in_days}` → `{token, secret}` |
| DELETE | `/tokens/{id}` | user | Revoke |
| GET/PUT | `/system/settings` | admin | `{anonymous_read, docker_default_repository}` |
| POST | `/system/gc` | admin | Run blob garbage collection now |
| GET | `/system/audit?actor=&action=&limit=&offset=` | admin | Audit events, newest first |

Supported `kind` per `format`: maven, npm, pypi, cargo, raw — hosted, proxy,
group; docker — proxy, group; apt, yum — proxy.

The Docker Registry API v2 is served separately under `/v2/` (see
[client setup](formats.md#docker--oci-pull-through-cache)).

Repository `config` shape:

```json
{
  "write_policy": "allow_once | allow_redeploy | read_only",
  "version_policy": "release | snapshot | mixed",
  "proxy": {
    "remote_url": "https://registry.npmjs.org/",
    "metadata_max_age_secs": 1800,
    "username": "",
    "password": "",
    "offline": false,
    "outbound": {
      "mode": "inherit | direct | custom",
      "url": "socks5h://proxy.corp:1080",
      "username": "",
      "password": "",
      "no_proxy": ".corp.example,10.0.0.0/8"
    }
  },
  "group": { "members": ["npm-hosted", "npm-proxy"] }
}
```

Example: create a proxy for PyPI.

```sh
curl -u admin:$TOKEN -H 'content-type: application/json' \
  -d '{"name":"pypi-proxy","format":"pypi","kind":"proxy","config":{"proxy":{"remote_url":"https://pypi.org/simple/"}}}' \
  https://trove.example.com/api/v1/repositories
```
