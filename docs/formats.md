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

## Raw

```sh
curl -u you:<TOKEN> --upload-file app.tar.gz https://trove.example.com/repository/raw-hosted/releases/app.tar.gz
curl -O https://trove.example.com/repository/raw-hosted/releases/app.tar.gz
curl -u admin:<TOKEN> -X DELETE https://trove.example.com/repository/raw-hosted/releases/
```

`GET` on a directory path ending in `/` returns an HTML listing.
