# Client setup

The **Set up client** tab of each repository in the UI generates these
snippets with the correct URL. Below, `trove.example.com` is your server.

Downloads need no credentials while anonymous read is on (the default). The
`<server>`, `_authToken`, `username`/`password` entries below are only needed
for **publishing** to hosted repositories, or for reads when anonymous read
has been switched off. `<TOKEN>` is an access token created under **Access
tokens**; tokens work wherever a password is accepted.

## Maven

`~/.m2/settings.xml`: route every request through the group:

```xml
<settings>
  <servers>
    <server><id>maven-public</id><username>you</username><password><TOKEN></password></server>
    <server><id>maven-releases</id><username>you</username><password><TOKEN></password></server>
    <server><id>maven-snapshots</id><username>you</username><password><TOKEN></password></server>
  </servers>
  <mirrors>
    <mirror>
      <id>maven-public</id>
      <mirrorOf>*</mirrorOf>
      <url>https://trove.example.com/repository/maven-public/</url>
    </mirror>
  </mirrors>
</settings>
```

`pom.xml` for `mvn deploy`:

```xml
<distributionManagement>
  <repository>
    <id>maven-releases</id>
    <url>https://trove.example.com/repository/maven-releases/</url>
  </repository>
  <snapshotRepository>
    <id>maven-snapshots</id>
    <url>https://trove.example.com/repository/maven-snapshots/</url>
  </snapshotRepository>
</distributionManagement>
```

Gradle (`settings.gradle.kts`):

```kotlin
dependencyResolutionManagement {
    repositories {
        maven {
            url = uri("https://trove.example.com/repository/maven-public/")
            credentials { username = "you"; password = "<TOKEN>" }
        }
    }
}
```

Notes: hosted repositories enforce the version policy (`release`, `snapshot`,
`mixed`); `.sha1/.md5/.sha256/.sha512` are generated when not uploaded; groups
merge `maven-metadata.xml` across members.

## npm / pnpm / yarn

`.npmrc`:

```ini
registry=https://trove.example.com/repository/npm-public/
//trove.example.com/repository/npm-public/:_authToken=<TOKEN>

# publish scoped packages to the hosted repository
@acme:registry=https://trove.example.com/repository/npm-hosted/
//trove.example.com/repository/npm-hosted/:_authToken=<TOKEN>
```

`npm login` also works with the legacy flow:
`npm login --auth-type=legacy --registry https://trove.example.com/repository/npm-hosted/`.

Supported: publish, install, `npm view`, dist-tags, deprecate, unpublish
(admin), `npm whoami`, `npm search` (hosted), `npm audit` (forwarded to the
first proxy member).

## PyPI (pip, uv, poetry, twine)

Install through the group:

```ini
# pip.conf
[global]
index-url = https://trove.example.com/repository/pypi-public/simple/
```

```sh
uv pip install --index-url https://trove.example.com/repository/pypi-public/simple/ requests
```

Upload with twine (`~/.pypirc`):

```ini
[distutils]
index-servers = trove

[trove]
repository = https://trove.example.com/repository/pypi-hosted/
username = __token__
password = <TOKEN>
```

```sh
twine upload --repository trove dist/*
```

Proxy repositories should point at `https://pypi.org/simple/`. Files fetched
through a proxy are verified against the SHA-256 published in the index.

## Cargo

`.cargo/config.toml`:

```toml
[registries.trove]
index = "sparse+https://trove.example.com/repository/cargo-hosted/index/"

# Optional: send every crates.io download through Trove
[registries.trove-public]
index = "sparse+https://trove.example.com/repository/cargo-public/index/"

[source.crates-io]
replace-with = "trove-public"

[source.trove-public]
registry = "sparse+https://trove.example.com/repository/cargo-public/index/"
```

```sh
cargo login --registry trove <TOKEN>
cargo publish --registry trove
cargo yank --registry trove --version 0.1.0
```

In `Cargo.toml`: `publish = ["trove"]` and
`my-crate = { version = "0.1", registry = "trove" }`.
Proxy repositories should point at `https://index.crates.io/`. When anonymous
read is off, `config.json` advertises `auth-required` and Cargo sends the
token on every request.

## Docker / OCI (pull-through cache)

Create a Docker proxy per upstream registry and, optionally, a group:

| Upstream | Remote URL |
| -------- | ---------- |
| Docker Hub | `https://registry-1.docker.io` |
| GitHub Container Registry | `https://ghcr.io` |
| Quay | `https://quay.io` |
| Kubernetes images | `https://registry.k8s.io` |
| Harbor / any v2 registry | `https://harbor.example.com` |

Image names start with the repository name:

```sh
docker login trove.example.com            # only needed when anonymous read is off
docker pull trove.example.com/docker-hub/library/nginx:1.27
docker pull trove.example.com/docker-hub/nginx:1.27   # library/ is added for Docker Hub
```

containerd (Kubernetes nodes), `/etc/containerd/certs.d/docker.io/hosts.toml`:

```toml
server = "https://registry-1.docker.io"

[host."https://trove.example.com/v2/docker-hub"]
  capabilities = ["pull", "resolve"]
  override_path = true
```

Docker daemon as a plain mirror: set the repository as **Default Docker
repository** under System, then in `/etc/docker/daemon.json`:

```json
{ "registry-mirrors": ["https://trove.example.com"] }
```

Docker requires HTTPS; for a plain-HTTP test server add it to
`insecure-registries`. Upstream credentials (Docker Hub account to avoid
anonymous rate limits, private registries) go into the proxy's remote
username/password. Pushing images is not supported yet.

## APT (Debian, Ubuntu, Kylin, UOS)

One proxy per upstream archive, e.g. `http://archive.ubuntu.com/ubuntu/`,
`http://security.ubuntu.com/ubuntu/`, `http://deb.debian.org/debian/`.

```text
# /etc/apt/sources.list.d/ubuntu.list
deb http://trove.example.com/repository/ubuntu/ noble main restricted universe multiverse
deb http://trove.example.com/repository/ubuntu/ noble-updates main restricted universe multiverse
```

Release signatures are passed through, so keep the distribution keyring
installed. `apt-get update` revalidates with `If-Modified-Since`.

## YUM / DNF (openEuler, RHEL family, Fedora, EPEL)

One proxy per repository directory, e.g.
`https://repo.openeuler.org/openEuler-24.03-LTS/OS/x86_64/` or
`https://repo.openeuler.org/openEuler-24.03-LTS/update/x86_64/`.

```ini
# /etc/yum.repos.d/openeuler-os.repo
[openeuler-os]
name=openEuler OS (via Trove)
baseurl=http://trove.example.com/repository/openeuler-os/
enabled=1
gpgcheck=1
gpgkey=http://trove.example.com/repository/openeuler-os/RPM-GPG-KEY-openEuler
```

Point `baseurl` at Trove instead of `metalink`/`mirrorlist`.

## Raw (certificates, binaries, installers, any file)

Raw hosted repositories store arbitrary files by path, like Nexus' raw format.

* **Web UI:** open the repository → **Browse** → **Upload**. Pick files or a
  whole folder (sub-folders are kept) and a target folder. Admins can delete
  files and folders from the same view.
* **Private repositories:** tick **Require sign-in to read** for certificates
  with private keys, keystores (`.p12`, `.jks`) or anything internal. They are
  hidden from anonymous users in listings and search, return 401 to
  anonymous downloads, and cannot be members of public groups.
* **Downloads:** files are sent as attachments with their file name by
  default. *Show in the browser* serves them inline under a sandboxing
  `Content-Security-Policy`, so uploaded HTML cannot run scripts against
  Trove.

Example layout for a certificate repository:

```
certs/
  ca/root-ca.crt              # public: also fine in a public repository
  ca/intermediate-2026.crt
  servers/api.corp/fullchain.pem
  servers/api.corp/privkey.pem    # keep in a private repository
  java/truststore.jks
```

```sh
# download (public repository, no credentials)
curl -fO https://trove.example.com/repository/certs-public/ca/root-ca.crt
# private repository
curl -fO -u you:<TOKEN> https://trove.example.com/repository/certs/servers/api.corp/privkey.pem
```

Command line:

```sh
curl -u you:<TOKEN> --upload-file app.tar.gz https://trove.example.com/repository/raw-hosted/releases/app.tar.gz
curl -O https://trove.example.com/repository/raw-hosted/releases/app.tar.gz
curl -u admin:<TOKEN> -X DELETE https://trove.example.com/repository/raw-hosted/releases/
```

`GET` on a directory path ending in `/` returns an HTML listing.

### Mutable files in a raw proxy

Raw proxies treat files as immutable by default. For files that change under
the same URL — a plugin marketplace index on a branch, `latest/` download
links — set **File revalidation** (`content_max_age_secs`): after that time
Trove asks the remote with `If-None-Match`/`If-Modified-Since` and downloads
only when the file changed. Example: a raw proxy of
`https://raw.githubusercontent.com/` with a 10-minute revalidation.

## Git (GitHub, Gitee, GitLab, Gitea)

A Git proxy keeps a bare mirror (branches and tags) of every repository a
client asks for and serves it over smart HTTP. Mirrors are refreshed with
`git fetch` after the repository's refresh interval (default 30 minutes); if
the remote is unreachable the last mirrored state is served. Pushes are
refused. Remote: the root that repository paths are appended to, e.g.
`https://github.com/` (repository `github` below).

```sh
# Route every GitHub clone through Trove (also what pnpm/npm use for
# "github:" dependencies, and what plugin installers call)
git config --global url."http://trove.example.com/repository/github/".insteadOf "https://github.com/"
git clone https://github.com/owner/repo.git
```

Besides the Git protocol, the mirror answers without contacting the remote:

| Path | Equivalent |
| ---- | ---------- |
| `owner/repo/archive/<ref>.tar.gz` (`.zip`, `.tar`) | `github.com/owner/repo/archive/…` |
| `owner/repo/tar.gz/<ref>`, `owner/repo/zip/<ref>` | `codeload.github.com/…` |
| `owner/repo/raw/<ref>/<path>` | `raw.githubusercontent.com/owner/repo/<ref>/<path>` |

For nested groups (GitLab) separate the project with `/-/`:
`group/sub/project/-/raw/main/README.md`. Raw files are served with
`nosniff` and a sandboxing CSP. Admins evict a mirror with
`DELETE /repository/github/owner/repo`; deleting the repository removes all
of its mirrors.

**DeepSeek Harness plugins.** `dsh plugin add` runs `pnpm add` inside the
profile (`~/.dsh/profiles/<profile>`):

```sh
# npm-published plugins: point the profile at an npm proxy
echo "registry=http://trove.example.com/repository/npm/" > ~/.dsh/profiles/web/.npmrc
dsh plugin --profile web add dsh-plugin-example
# GitHub-hosted plugins: with the insteadOf rule above
dsh plugin --profile web add github:owner/dsh-plugin
# …or without a global rule
dsh plugin --profile web add git+http://trove.example.com/repository/github/owner/dsh-plugin.git
```

With GitHub unreachable, pnpm resolves `github:` dependencies through
`git ls-remote`/`git clone`, which the insteadOf rule sends to Trove (verified
with pnpm 10). Marketplace front-ends that download indexes from
`raw.githubusercontent.com`, `cdn.jsdelivr.net` or the GitHub API cannot be
redirected by a Git setting; give them the `raw/` URL above or a raw proxy
with file revalidation if they accept a custom index URL.

**Claude Code plugin marketplaces** are Git repositories:
`/plugin marketplace add http://trove.example.com/repository/github/owner/marketplace.git`.

## Skills (SkillHub, ClawHub)

Proxy for agent-skill marketplaces that speak the ClawHub HTTP API:

| Marketplace | Remote URL |
| ----------- | ---------- |
| SkillHub (skillhub.cn, Tencent) | `https://lightmake.site/` |
| ClawHub | `https://clawhub.ai/` |
| Self-hosted ClawHub-compatible registries | their base URL |

Every `GET` is forwarded with its query string and cached. Pinned downloads
(`api/v1/download?slug=<slug>&version=<v>`, package artifacts, `.zip`/`.tgz`)
are immutable and appear in **Browse** as `api/v1/download/<slug>/<version>`;
searches, listings, skill details and "latest" downloads are refreshed after
the cache time and served stale while the remote is down. URLs of the remote
inside JSON responses are rewritten to Trove.

```sh
export CLAWHUB_REGISTRY=http://trove.example.com/repository/skillhub
clawhub search pdf
clawhub install <slug>

# any tool / script
curl "http://trove.example.com/repository/skillhub/api/v1/search?q=pdf&limit=10"
curl -o skill.zip "http://trove.example.com/repository/skillhub/api/v1/download?slug=<slug>&version=<version>"
unzip skill.zip -d ~/.claude/skills/<slug>
```

Limitations: the proxy is read-only (no login or publish); download links in
responses that point at another host (e.g. object storage) are passed through
unchanged; skills that a marketplace delivers from GitHub need the Git proxy
as well. SkillHub's own `skillhub` CLI is installed from Tencent COS; use it
through Trove only if it lets you set the registry URL, otherwise use a
ClawHub-compatible CLI or the HTTP API above.
