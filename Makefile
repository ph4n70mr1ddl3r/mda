# Common flows for MDA. Works with Docker OR Podman — set CTN=podman to switch.
#   make test         # unit + DB-backed suites (single-threaded; needs a Postgres)
#   make run-dev      # start postgres+redis, run the server from source
#   make up-staging   # build & run the whole stack (prod-like)
#   make quadlet-install   # install systemd/Podman units (prod; sudo)

CTN ?= docker
COMPOSE := $(CTN) compose
DB_URL ?= postgres://mda:mda@127.0.0.1:5433/mda?sslmode=disable

.PHONY: build run-dev up-deps down-deps test test-unit test-db fmt clippy \
        up-staging image quadlet-install quadlet-reload

build:
	cargo build --release

# --- dev (server from source against containerized deps) ---
up-deps:
	$(COMPOSE) up -d postgres redis

down-deps:
	$(COMPOSE) down

run-dev: up-deps
	DATABASE_URL="$(DB_URL)" cargo run

# --- image & staging (containerized app) ---
image:
	$(CTN) build -t mda:latest .

up-staging: image
	$(COMPOSE) -f docker-compose.yml -f compose.staging.yml --profile app up -d

# --- quality / tests ---
fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

clippy:
	cargo clippy --all-targets --all-features -- -D warnings

test-unit:
	cargo test --lib --bins --doc

# DB-backed suites share one database and are not parallel-safe → single-threaded.
test-db:
	DATABASE_URL="$(DB_URL)" cargo test --test data --test studio --test integration -- --test-threads=1

test: test-unit test-db

# --- production: Podman Quadlet (systemd) ---
QUADLET_DIR ?= /etc/containers/systemd
quadlet-install:
	sudo mkdir -p $(QUADLET_DIR) /etc/mda
	sudo cp deploy/quadlet/mda.network deploy/quadlet/mda-postgres.volume \
	     deploy/quadlet/mda-*.container $(QUADLET_DIR)/
	sudo cp deploy/quadlet/mda-app.env.example /etc/mda/mda-app.env
	@echo "edit /etc/mda/mda-app.env (MDA_APP_DATABASE_URL, MDA_JWT_SECRET), then:"
	@echo "  sudo systemctl daemon-reload && sudo systemctl enable --now mda-app"

quadlet-reload:
	sudo systemctl daemon-reload
	sudo systemctl restart mda-app
