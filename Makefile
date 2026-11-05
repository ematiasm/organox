# Atajos de desarrollo. Todos los comandos asumen que existe .env
# (copiá .env.example) y que la DB está arriba (`make db-up`).

-include .env
export

.PHONY: help db-up db-down db-reset migrate new-migration run create-user prepare test fmt lint check fe-install fe-dev fe-build fe-check fe-schema dev

help: ## Muestra esta ayuda
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "};{printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

db-up: ## Levanta Postgres en Docker
	docker compose up -d
	@docker compose exec -T db sh -c 'until pg_isready -U admin_app -d admin_app >/dev/null 2>&1; do sleep 0.3; done'
	@echo "postgres listo en localhost:5432"

db-down: ## Baja Postgres (conserva los datos)
	docker compose down

db-reset: ## Baja Postgres y BORRA los datos. Solo desarrollo.
	@printf 'Esto borra la base de desarrollo. Escribi "si" para seguir: ' && read ans && [ "$$ans" = "si" ]
	docker compose down -v
	docker compose up -d
	@docker compose exec -T db sh -c 'until pg_isready -U admin_app -d admin_app >/dev/null 2>&1; do sleep 0.3; done'
	sqlx migrate run

migrate: ## Aplica migraciones pendientes
	sqlx migrate run

new-migration: ## Crea una migracion reversible: make new-migration NAME=crear_users
	@test -n "$(NAME)" || (echo "falta NAME=..." && exit 1)
	sqlx migrate add -r $(NAME)

run: ## Arranca el servidor en modo dev (sirve frontend/build si existe)
	cargo run

fe-install: ## Instala dependencias del frontend
	cd frontend && npm install

fe-dev: ## Frontend en modo dev con HMR (proxy /api -> :3000)
	cd frontend && npm run dev

fe-build: ## Compila el frontend a frontend/build
	cd frontend && npm run build

fe-check: ## Type-check del frontend (svelte-check)
	cd frontend && npm run check

fe-schema: ## Regenera los tipos TS desde OpenAPI
	cargo run --quiet --bin openapi > openapi.json
	cd frontend && npm run prepare-schema

dev: ## Backend + frontend en paralelo (dos terminales o usa esto)
	@echo "Backend en :3000, frontend en :5173 (proxy /api)"
	@(cargo run &) && cd frontend && npm run dev

create-user: ## Crea un usuario: make create-user EMAIL=x@y.com ROLE=owner
	@test -n "$(EMAIL)" || (echo "falta EMAIL=..." && exit 1)
	@test -n "$(ROLE)" || (echo "falta ROLE=owner|encargado" && exit 1)
	cargo run --bin create-user -- --email $(EMAIL) --role $(ROLE)

prepare: ## Regenera el cache de queries de sqlx (commitear .sqlx/)
	cargo sqlx prepare --workspace

test: ## Corre todos los tests (necesita DB arriba)
	cargo test

fmt: ## Formatea
	cargo fmt

lint: ## Clippy con warnings como errores
	cargo clippy --all-targets -- -D warnings

check: fmt lint test fe-check ## Lo que tiene que pasar antes de commitear
