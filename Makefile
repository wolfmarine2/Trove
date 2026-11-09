# Common development tasks. Run `make help` for a list.

.PHONY: help web build release run test test-pg lint fmt docker clean

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

docker: ## Build the container image
	docker build -f deploy/docker/Dockerfile -t trove:dev .

clean:
	cargo clean
	rm -rf web/dist
