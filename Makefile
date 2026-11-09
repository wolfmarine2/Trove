# Common development tasks. Run `make help` for a list.

.PHONY: help web build release run test test-pg lint fmt docker docker-multiarch docker-run clean

IMAGE ?= trove
VERSION ?= $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
# Extra docker build flags, e.g. BUILD_ARGS="--build-arg BASE_REGISTRY=harbor.corp/dockerhub/library/"
BUILD_ARGS ?=

help:
	@grep -E '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  %-10s %s\n", $$1, $$2}'

web: ## Build the web UI into web/dist
	cd web && npm ci && npm run build

build: ## Debug build of the server
	cargo build

release: web ## Release binary with the web UI embedded
	cargo build --release -p trove-server --features embed-ui

run: ## Run the server with config/trove.dev.toml (UI served from web/dist)
	cargo run -p trove-server -- --config config/trove.dev.toml

test: ## Unit and end-to-end tests (SQLite)
	cargo test --workspace

test-pg: ## Same tests, also against PostgreSQL (needs TROVE_TEST_POSTGRES_URL)
	TROVE_TEST_POSTGRES_URL=$${TROVE_TEST_POSTGRES_URL:?set TROVE_TEST_POSTGRES_URL} cargo test --workspace

lint: ## rustfmt, clippy and TypeScript checks
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	cd web && npx tsc -b

fmt: ## Format Rust code
	cargo fmt --all

docker: ## Build the container image for this machine's architecture
	docker build $(BUILD_ARGS) --build-arg VERSION=$(VERSION) --build-arg REVISION=$$(git rev-parse --short HEAD) \
		-t $(IMAGE):$(VERSION) -t $(IMAGE):latest .

docker-multiarch: ## Build and push linux/amd64 + linux/arm64 (IMAGE=registry/path/trove)
	docker buildx build --platform linux/amd64,linux/arm64 $(BUILD_ARGS) \
		--build-arg VERSION=$(VERSION) --build-arg REVISION=$$(git rev-parse --short HEAD) \
		-t $(IMAGE):$(VERSION) -t $(IMAGE):latest --push .

docker-run: ## Run the image locally with SQLite (http://localhost:8081, admin / admin12345)
	docker run --rm -it -p 8081:8081 -v trove-data:/data \
		-e TROVE__SECURITY__INITIAL_ADMIN_PASSWORD=admin12345 $(IMAGE):$(VERSION)

clean:
	cargo clean
	rm -rf web/dist
