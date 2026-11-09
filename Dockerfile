# Trove container image.
#
# Portable across Docker (BuildKit or classic), Podman/Buildah and Kaniko: no
# BuildKit-only syntax, no `# syntax=` frontend download. Every image and
# package source is a build argument, so the image can be built entirely
# inside an air-gapped network (for example through Harbor and Trove itself):
#
#   docker build \
#     --build-arg BASE_REGISTRY=harbor.corp/dockerhub/library/ \
#     --build-arg NPM_REGISTRY=http://trove.corp/repository/npm-public/ \
#     --build-arg CARGO_REGISTRY=sparse+http://trove.corp/repository/cargo-public/index/ \
#     --build-arg APT_MIRROR=http://trove.corp/repository/debian/ \
#     --build-arg APT_SECURITY_MIRROR=http://trove.corp/repository/debian-security/ \
#     -t harbor.corp/platform/trove:0.3.0 .
#
# Multi-arch (x86_64 + Kunpeng/ARM64): docker buildx build --platform linux/amd64,linux/arm64 …
# See docs/deployment.md for Kaniko / Argo Workflows and openEuler runtimes.

# Prefix for all base images, e.g. "harbor.corp/dockerhub/library/" (keep the trailing slash).
ARG BASE_REGISTRY=
ARG NODE_IMAGE=${BASE_REGISTRY}node:22-bookworm-slim
# Builder and runtime must share the glibc generation: bookworm (2.36) binaries
# also run on openEuler 24.03 (2.38).
ARG RUST_IMAGE=${BASE_REGISTRY}rust:1.97-bookworm
ARG RUNTIME_IMAGE=${BASE_REGISTRY}debian:bookworm-slim

# ---------------------------------------------------------------- web UI
FROM ${NODE_IMAGE} AS web
ARG NPM_REGISTRY=
WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
# npm rewrites the registry.npmjs.org URLs in package-lock.json to the
# configured registry (replace-registry-host=npmjs), so only the config changes.
RUN if [ -n "$NPM_REGISTRY" ]; then npm config set registry "$NPM_REGISTRY"; fi \
    && npm ci --no-audit --no-fund
COPY web/ ./
RUN npm run build

# ---------------------------------------------------------------- server
FROM ${RUST_IMAGE} AS server
ARG APT_MIRROR=
ARG APT_SECURITY_MIRROR=
ARG CARGO_REGISTRY=
# Optional Debian mirrors (e.g. Trove APT proxies of deb.debian.org/debian
# and security.debian.org/debian-security).
RUN SRC=/etc/apt/sources.list.d/debian.sources; \
    if [ -n "$APT_MIRROR" ]; then \
        sed -i "s#http://deb.debian.org/debian\$#${APT_MIRROR%/}#" "$SRC"; \
    fi; \
    if [ -n "$APT_SECURITY_MIRROR" ]; then \
        sed -i "s#http://deb.debian.org/debian-security\$#${APT_SECURITY_MIRROR%/}#" "$SRC"; \
    fi; \
    apt-get update \
    && apt-get install -y --no-install-recommends cmake clang \
    && rm -rf /var/lib/apt/lists/*
# Route crates.io through a mirror (e.g. a Trove Cargo proxy) when given.
RUN if [ -n "$CARGO_REGISTRY" ]; then \
        printf '[source.crates-io]\nreplace-with = "mirror"\n\n[source.mirror]\nregistry = "%s"\n' "$CARGO_REGISTRY" \
            > "$CARGO_HOME/config.toml"; \
    fi
WORKDIR /src

# 1. Dependencies only: compile against stub sources so this layer is cached
#    until Cargo.toml / Cargo.lock change.
COPY Cargo.toml Cargo.lock ./
COPY crates/trove-core/Cargo.toml crates/trove-core/
COPY crates/trove-db/Cargo.toml crates/trove-db/
COPY crates/trove-storage/Cargo.toml crates/trove-storage/
COPY crates/trove-server/Cargo.toml crates/trove-server/
RUN for c in trove-core trove-db trove-storage trove-server; do \
        mkdir -p crates/$c/src && echo '' > crates/$c/src/lib.rs; \
    done \
    && echo 'fn main() {}' > crates/trove-server/src/main.rs \
    && cargo build --release --locked -p trove-server \
    && rm -rf crates/*/src

# 2. The real build. Sources are touched so Cargo never mistakes them for
#    the stubs above (COPY keeps the original, possibly older, mtimes).
COPY crates crates
COPY --from=web /src/web/dist web/dist
RUN find crates -name '*.rs' -exec touch {} + \
    && cargo build --release --locked -p trove-server --features embed-ui \
    && install -m 0755 target/release/trove /usr/local/bin/trove \
    && /usr/local/bin/trove --version

# ---------------------------------------------------------------- runtime
FROM ${RUNTIME_IMAGE}
ARG VERSION=0.3.0
ARG REVISION=
LABEL org.opencontainers.image.title="Trove" \
      org.opencontainers.image.description="Self-hosted universal artifact repository and internal mirror" \
      org.opencontainers.image.source="https://github.com/wolfmarine2/Trove" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"
# CA certificates for HTTPS remotes; works on Debian/Ubuntu and openEuler/RHEL bases.
RUN set -eu; \
    if command -v apt-get >/dev/null 2>&1; then \
        apt-get update && apt-get install -y --no-install-recommends ca-certificates \
        && rm -rf /var/lib/apt/lists/*; \
        groupadd --system --gid 10001 trove; \
        useradd --system --uid 10001 --gid 10001 --home-dir /data --no-create-home --shell /usr/sbin/nologin trove; \
    else \
        PM=$(command -v dnf || command -v yum || command -v microdnf); \
        $PM install -y ca-certificates; \
        command -v useradd >/dev/null 2>&1 || $PM install -y shadow-utils || $PM install -y shadow; \
        $PM clean all; \
        groupadd -r -g 10001 trove; \
        useradd -r -u 10001 -g 10001 -d /data -M -s /sbin/nologin trove; \
    fi; \
    mkdir -p /data /etc/trove; \
    chown 10001:10001 /data
COPY --from=server /usr/local/bin/trove /usr/local/bin/trove
COPY deploy/docker/trove.toml /etc/trove/trove.toml
ENV TROVE_CONFIG=/etc/trove/trove.toml
USER 10001:10001
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8081
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 CMD ["trove", "healthcheck"]
ENTRYPOINT ["trove"]
CMD ["serve"]
