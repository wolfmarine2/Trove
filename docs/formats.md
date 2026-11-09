# Client setup

The **Set up client** tab of each repository in the UI generates these
snippets with the correct URL. Below, `trove.example.com` is your server and
`<TOKEN>` an access token created under **Access tokens** (tokens work wherever
a password is accepted).

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

## Raw

```sh
curl -u you:<TOKEN> --upload-file app.tar.gz https://trove.example.com/repository/raw-hosted/releases/app.tar.gz
curl -O https://trove.example.com/repository/raw-hosted/releases/app.tar.gz
curl -u admin:<TOKEN> -X DELETE https://trove.example.com/repository/raw-hosted/releases/
```

`GET` on a directory path ending in `/` returns an HTML listing.
