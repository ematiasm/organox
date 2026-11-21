# Development shortcuts. Every command assumes `.env` exists
# (copy `.env.example`) and that the DB is up (`make db-up`).

-include .env
# The bare `export` makes every variable `.env` defines (DATABASE_URL,
# DEV_PASSWORD, ...) part of the environment of every recipe below, not only of
# the recipes that use it. Deliberate: recipes like `run`, `migrate` and
# `create-user` need DATABASE_URL in their environment, and make has no way to
# export a subset of what `-include` just read in. The comment on `dev-user`
# explains how that recipe keeps the password out of the command line make
# hands to the shell.
export

.PHONY: help db-up db-down db-reset migrate new-migration run create-user dev-user prepare test fmt lint check fe-install fe-dev fe-build fe-check fe-schema dev backup backup-check backup-install

help: ## Show this help
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "};{printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

db-up: ## Start Postgres in Docker
	docker compose up -d
	@docker compose exec -T db sh -c 'until pg_isready -U organox -d organox >/dev/null 2>&1; do sleep 0.3; done'
	@echo "postgres ready on localhost:5432"

db-down: ## Stop Postgres (keeps the data)
	docker compose down

db-reset: ## Stop Postgres and WIPE the data. Development only.
	@printf 'This wipes the development database. Type "yes" to continue: ' && read ans && [ "$$ans" = "yes" ]
	docker compose down -v
	docker compose up -d
	@docker compose exec -T db sh -c 'until pg_isready -U organox -d organox >/dev/null 2>&1; do sleep 0.3; done'
	sqlx migrate run

migrate: ## Apply pending migrations
	sqlx migrate run

new-migration: ## Create a reversible migration: make new-migration NAME=create_users
	@test -n "$(NAME)" || (echo "missing NAME=..." && exit 1)
	sqlx migrate add -r $(NAME)

run: ## Start the server in dev mode (serves frontend/build when present)
	cargo run

fe-install: ## Install frontend dependencies
	cd frontend && npm install

fe-dev: ## Frontend in dev mode with HMR (proxy /api -> :3000)
	cd frontend && npm run dev

fe-build: ## Build the frontend into frontend/build
	cd frontend && npm run build

fe-check: ## Type-check the frontend (svelte-check)
	cd frontend && npm run check

fe-schema: ## Regenerate the TS types from OpenAPI
	cargo run --quiet --bin openapi > openapi.json
	cd frontend && npm run prepare-schema

dev: ## Backend + frontend in parallel (two terminals, or use this)
	@echo "Backend on :3000, frontend on :5173 (proxy /api)"
	@(cargo run &) && cd frontend && npm run dev

create-user: ## Create a user: make create-user EMAIL=x@y.com ROLE=owner
	@test -n "$(EMAIL)" || (echo "missing EMAIL=..." && exit 1)
	@test -n "$(ROLE)" || (echo "missing ROLE=owner|manager" && exit 1)
	cargo run --bin create-user -- --email $(EMAIL) --role $(ROLE)

# DEV_PASSWORD arrives in this recipe's environment through the bare `export`
# at the top of this file, like every other variable `.env` defines: there is
# no per-target scoping in make to restore, and a second `export DEV_PASSWORD`
# here would only have repeated that fact. The recipe reads it from the
# environment with `$$DEV_PASSWORD`: make expands `$(VAR)` into the command
# line it hands to the shell, which would put the password in `ps` for the
# whole pipeline, and `$$VAR` leaves the expansion to the shell.
#
# A reset does not clear a failed-login lockout: if the account is locked, the
# next login is still rejected until the lock expires, so a rejected login
# right after this target is the lock at work, not a failed reset.
dev-user: ## Create or reset the dev user from .env: DEV_PASSWORD, DEV_EMAIL, DEV_ROLE
	@test -n "$$DEV_PASSWORD" || (echo "missing DEV_PASSWORD in .env (see docs/auth.md)" && exit 1)
	@printf '%s' "$$DEV_PASSWORD" | cargo run --quiet --bin create-user -- \
		--email "$(or $(DEV_EMAIL),owner@pyme.com)" \
		--role "$(or $(DEV_ROLE),owner)" \
		--password-stdin --upsert

prepare: ## Regenerate the sqlx query cache (commit .sqlx/)
	# --all-targets matters: CI compiles with SQLX_OFFLINE=true and builds the
	# test targets too, so a query macro that lives only in tests/ must be cached.
	cargo sqlx prepare --workspace -- --all-targets

test: ## Run all tests (needs the DB up)
	cargo test

fmt: ## Format
	cargo fmt

lint: ## Clippy with warnings as errors
	cargo clippy --all-targets -- -D warnings

check: fmt lint test fe-check ## What has to pass before committing

# --- Deployment operations -------------------------------------------------
#
# These talk to the deployment stack (`compose.prod.yaml`), not the development
# database. `backup` and `backup-check` work from any machine with Docker and the
# compose file; `backup-install` is Linux-only and registers the systemd timer.

backup: ## Back up the deployment database (see docs/backup.md)
	BACKUP_DIR=$(or $(BACKUP_DIR),$(HOME)/organox-backups) scripts/backup.sh

backup-check: ## Restore the last backup into a throwaway database and verify it
	BACKUP_DIR=$(or $(BACKUP_DIR),$(HOME)/organox-backups) scripts/restore-check.sh

backup-install: ## Install the daily systemd timer (Linux only)
	BACKUP_DIR=$(or $(BACKUP_DIR),$(HOME)/organox-backups) scripts/install-timer.sh
