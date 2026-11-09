# syntax=docker/dockerfile:1
#
# Multi-stage build producing a single static-ish binary with the web UI
# embedded. Works for linux/amd64 and linux/arm64 (Kunpeng), e.g.:
#   docker buildx build --platform linux/amd64,linux/arm64 -f deploy/docker/Dockerfile .
#
# For openEuler-based images, override the runtime base:
#   --build-arg RUNTIME_IMAGE=openeuler/openeuler:24.03-lts

ARG NODE_IMAGE=node:22-bookworm-slim
ARG RUST_IMAGE=rust:1-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim

FROM ${NODE_IMAGE} AS web
WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web/ ./
RUN npm run build

FROM ${RUST_IMAGE} AS server
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
COPY --from=web /src/web/dist web/dist
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p trove-server --features embed-ui \
    && cp target/release/trove /usr/local/bin/trove

FROM ${RUNTIME_IMAGE}
RUN if command -v apt-get >/dev/null; then \
        apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*; \
    else \
        (dnf install -y ca-certificates shadow-utils || yum install -y ca-certificates shadow-utils) && (dnf clean all || yum clean all); \
    fi \
    && useradd --system --uid 10001 --home-dir /data --shell /sbin/nologin trove \
    && mkdir -p /data /etc/trove && chown trove /data
COPY --from=server /usr/local/bin/trove /usr/local/bin/trove
USER 10001
WORKDIR /data
ENV TROVE__SERVER__DATA_DIR=/data \
    TROVE__SERVER__BIND=0.0.0.0:8081 \
    TROVE_CONFIG=/etc/trove/trove.toml
EXPOSE 8081
VOLUME ["/data"]
# Liveness: GET /healthz, readiness: GET /readyz (see deploy/kubernetes).
ENTRYPOINT ["/usr/local/bin/trove"]
CMD ["serve"]
