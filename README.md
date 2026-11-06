# organox

Business management for an SMB: stock and supplies.
Backend in Rust/Axum + PostgreSQL + sqlx. Single-tenant.

Work arrives as an issue, then a PR that links it. See
[CONTRIBUTING.md](CONTRIBUTING.md) for labels, commit shape and what a PR is
expected to prove.

## Requirements

- Stable Rust (pinned by `rust-toolchain.toml`)
- Docker + Docker Compose
- `sqlx-cli`: `cargo install sqlx-cli --no-default-features --features postgres,rustls`

## Getting started

```bash
cp .env.example .env      # adjust if needed
make db-up                # Postgres in Docker
make migrate              # apply migrations
make create-user EMAIL=owner@pyme.com ROLE=owner   # create the first user
make run                  # http://localhost:3000
```

Quick check:

```bash
curl -i localhost:3000/healthz   # 204 - the process is alive
curl -i localhost:3000/readyz    # 204 - the DB responds
```

## Commands

`make help` lists everything. The important ones:

| Command | What it does |
|---|---|
| `make db-up` / `db-down` | Starts / stops Postgres (preserves data) |
| `make db-reset` | Recreates the DB from scratch (deletes data, asks for confirmation) |
| `make migrate` | Applies pending migrations |
| `make new-migration NAME=...` | Creates a reversible migration |
| `make create-user EMAIL=... ROLE=...` | Creates a user (prompts for a password without echo) |
| `make prepare` | Regenerates `.sqlx/` (required when queries change) |
| `make fe-install` | Installs frontend dependencies |
| `make fe-dev` | Frontend with HMR on `:5173` (proxy `/api` → `:3000`) |
| `make fe-build` | Builds the frontend to `frontend/build` |
| `make fe-check` | Type-checks the frontend |
| `make fe-schema` | Regenerates TS types from OpenAPI |
| `make check` | `fmt` + `clippy -D warnings` + `test` + `fe-check` |

## Project structure

```
src/
  main.rs        startup: config, logging, DB, router, serve
  lib.rs         AppState and modules
  config.rs      typed configuration from the environment
  error.rs       AppError -> application/problem+json
  telemetry.rs   tracing (pretty in dev, json in prod)
  db.rs          connection pool and ping
  routes.rs      HTTP router (/healthz, /readyz, /api/v1)
  auth/
    model.rs      User, Role, PasswordHash, session TTLs
    validation.rs email and password rules
    password.rs   argon2id
    token.rs      session token and its SHA-256
    repository.rs auth SQL (the only place with queries)
    service.rs    use cases and policy
    extract.rs    AuthUser (the only way to identify the actor)
  catalog/
    model.rs      Product, ProductKind, MovementReason, movement sign
    validation.rs name, SKU, price, quantity
    repository.rs catalog SQL and stock calculation
    service.rs    rules: negative stock, service without stock, transaction
    handlers.rs   catalog endpoints
  sales/
    model.rs      Sale, SaleItem, SaleStatus
    repository.rs sales SQL and transactional checkout
    service.rs    rules: one draft per cashier, non-saleable service
    handlers.rs   sales endpoints
  ratelimit/
    account.rs    failed attempts per account (Postgres)
    ip.rs         requests per IP (memory)
    middleware.rs applies the IP limit only to /auth/login
migrations/      versioned SQL migrations
src/bin/         CLI binaries (create-user, openapi, migrate)
src/openapi.rs   OpenAPI 3.1 spec generated from code
tests/           integration tests (in-memory Router + real DB)
docs/            design decisions
.sqlx/           query cache (committed: builds without a DB)

frontend/
  src/routes/
    +layout.svelte      root layout: ModeWatcher (system theme, manual toggle)
    +layout.ts          ssr = false, prerender = false (SPA)
    (app)/              every private screen, behind the session guard
      +layout.svelte    the single navigation rail
      +layout.ts        session guard: api.me(), 401 -> /login
      products/         product list, search, stock
      operations/pos/   point of sale with barcode scanner
      operations/movements/  stock ledger history
      users/            users (owner only)
    (app)/pos/          redirect -> /operations/pos
    login/              the first screen
  src/lib/
    api/client.ts        typed fetch wrapper, 401 -> /login, Retry-After
    api/schema.d.ts      generated from openapi.json (never hand-edited)
    components/app-shell.svelte  the rail: groups, expand, user menu
    components/ui/       shadcn-svelte primitives (vega, olive)
  vite.config.ts     Tailwind v4 plugin, adapter-static, /api dev proxy
```

## Project conventions

- Documentation and code are written in English; see `AGENTS.md` for the convention.
- Applied migrations are **immutable**. A schema change requires a new migration,
  never an edit to an earlier one.
- Every business route lives under `/api/v1`.
- One error type (`AppError`); internal details are logged, not serialized to
  the client.
- Timestamps: `TIMESTAMPTZ` in UTC. IDs: `uuid` (v7). Money: never `f64`.
- The app does **not** run migrations on startup: `make migrate` runs them.
- `/healthz` = liveness (process alive, does not touch the DB). `/readyz` =
  readiness (the DB responds).
- Building without a DB: `SQLX_OFFLINE=true cargo build` uses `.sqlx/`. If you
  change a query, run `make prepare`.
- **Stock is not stored; it is calculated** (the sum of `stock_movements`). The
  ledger is append-only: correct it with an adjustment, never by editing history.
- Decimals travel as **strings** in JSON, never as numbers: `"1500.50"`.
- **Rate limiting does not prohibit abuse; it bounds it.** 5 failures per account
  → a 5-minute lockout that expires on its own. A permanent lockout would let
  anyone lock out the owner by entering the wrong password.
- `X-Forwarded-For` is ignored: the TCP connection IP is used. Honoring it
  without a trusted proxy nullifies the IP limit.

## API

Base: `/api/v1`. Errors use `application/problem+json` (RFC 7807).

| Method | Route | Auth | Description |
|---|---|---|---|
| POST | `/auth/login` | — | `{email, password}` → user + session cookie |
| POST | `/auth/logout` | session | Deletes the session in the DB |
| GET | `/auth/me` | session | Current user |
| POST | `/users` | `owner` | `{email, password, role}` → 201 with id |
| GET | `/products` | session | List with calculated stock (`?kind=&search=&include_inactive=`) |
| POST | `/products` | session | Create a product or service |
| GET | `/products/{id}` | session | Details with stock |
| PATCH | `/products/{id}` | session | Updates only the fields supplied |
| POST | `/products/{id}/deactivate` | session | Soft-deactivate, idempotent |
| GET | `/products/{id}/movements` | session | History, newest first |
| POST | `/products/{id}/movements` | session | Records a stock movement |
| POST | `/sales/draft` | session | Opens (or reuses) the cart |
| GET | `/sales/{id}` | session | Sale with server-calculated total |
| POST | `/sales/{id}/items` | session | Adds an item |
| PUT | `/sales/{id}/items/{product_id}` | session | Sets quantity (0 = remove) |
| DELETE | `/sales/{id}/items/{product_id}` | session | Removes an item |
| POST | `/sales/{id}/complete` | session | Completes checkout and deducts stock |
| POST | `/sales/{id}/cancel` | session | Cancels the draft |
| GET | `/sales` | session | Completed sales |

```bash
# log in and save the cookie
curl -c jar.txt -X POST localhost:3000/api/v1/auth/login \
  -H 'content-type: application/json' \
  -d '{"email":"owner@pyme.com","password":"..."}'

curl -b jar.txt localhost:3000/api/v1/auth/me
```

## Current state

Slices 1–3: skeleton, authentication, and catalog with a stock ledger.

- **Auth**: login/logout/`me`, revocable DB sessions, `HttpOnly`+`SameSite=Strict`
  cookie, argon2id. Binary RBAC (`owner`, `manager`).
- **Catalog**: products and services, unique SKU, search, soft deactivation.
- **Stock**: append-only ledger. Stock **is not a column**; it is the signed sum
  of movements. You cannot sell what is not available.
- **Login rate limiting**: per-account lockout (survives restarts) and per-IP
  limit (blocks before spending time on argon2), with `429` + `Retry-After`.
- **Sales**: server-persisted cart with transactional checkout.
- **114 tests** (unit + integration against real Postgres).
- **UI**: SvelteKit (SPA) served by the same Axum server. Login, **a single
  navigation rail** (expandable, with groups and user menu), product list,
  movements, and **point of sale with barcode scanner** at `/operations/pos`.
- **OpenAPI 3.1** generated from code; frontend types are generated from that
  spec (`make fe-schema`).

See `docs/auth.md`, `docs/catalog.md`, `docs/ratelimit.md` and `docs/sales.md`
for the reasoning and what is missing.

## Frontend

The UI is a **SvelteKit SPA** served by the same Axum process. Adding a UI meant
adding three decisions, not just a framework:

**SvelteKit over a hand-rolled SPA.** A single-page app still needs routing,
lazy chunks and a dev server with a proxy. SvelteKit gives all three plus type
generation from routes; building those three by hand is the part of a frontend
that rots first.

**`adapter-static` with `fallback: 'index.html'`.** The frontend has no server of
its own, so it builds to static files and Axum serves `frontend/build/` with a
fallback to `index.html`. One container, one origin, no CORS. The one contract:
Axum answers `/api/v1/*`, `/healthz` and `/readyz` first and only then falls back
to the SPA — otherwise an unknown API route would return `index.html` with `200`
instead of a `404`.

**Server-rendered data, client-rendered screens.** Every screen loads through a
`+page.ts` against the API, so the total, the stock and the permissions are
always the server's answer. The client renders; it never sums or authorizes.

**Tailwind v4 + shadcn-svelte** (preset `vega`, base color `olive`) for
styling, with tokens documented in `design/system.md`. Dark mode follows the
system theme (`mode-watcher`) with a manual toggle. The design directions
considered before implementing are in `design/admin-ui/`.

Stack: **Vite 8 + Svelte 5 (runes) + TypeScript 6**, `@sveltejs/kit` 3.

## Frontend development

Two options:

**With HMR** (recommended while working on the UI):

```bash
make db-up && make migrate
make run          # terminal 1: backend on :3000
make fe-dev       # terminal 2: frontend on :5173
```

The Vite proxy forwards `/api` to `:3000`, so the browser sees **a single
origin** and the `SameSite=Strict` cookie works. Without the proxy, everything
returns 401.

**Local production** (one server):

```bash
make fe-build     # generates frontend/build
make run          # Axum serves the SPA with a fallback to index.html
```

Open `http://localhost:3000`. Interactive API documentation is at
`http://localhost:3000/api/v1/docs`.

`make fe-check` runs `svelte-kit sync` + `svelte-check`; it is part of
`make check`, so a type error in a component fails the same gate as a clippy
warning. There is no frontend unit-test runner yet: the behavior that matters
lives in the Rust integration tests, and `frontend/e2e/nav-shell.cjs` is a manual
Playwright script for the navigation shell that needs a running app and
credentials.

When an API type changes, regenerate instead of editing: `make fe-schema`
runs the OpenAPI generator and then `openapi-typescript`. `schema.d.ts` is
generated; never edit it by hand.

Next slice: product creation and editing in the UI, user listing, or the first
business report.

## Deploy

Two containers: Postgres and one app container that serves both the API and the
SPA. One origin, so the `SameSite=Strict` cookie works without CORS.

**Development and deployment do not share a compose file.** `compose.yaml` runs
only Postgres so the backend and the frontend run natively on the host, where
the feedback loop is seconds instead of minutes. `compose.prod.yaml` builds the
image and runs the deployment. Separating them is not cosmetic: the production
file must not publish the database port, must restart services, must gate on
health, and must run migrations before the server starts.

**One `.env` per machine, two templates.** `.env.example` documents the
development values and `.env.prod.example` the deployment ones; each machine
copies the template that applies to it to a single `.env`. There is no
`.env` + `.env.prod` pair living side by side, because two files with a
database password is two places for it to disagree with itself.

A machine that both develops and deploys has one `.env` and both blocks
compiled into it: the development keys are simply unused by the deployment,
because `compose.prod.yaml` overrides `DATABASE_URL` for its containers.
Copying `.env.prod.example` over an existing development `.env` replaces it —
merge the two blocks instead.

### First deployment

```bash
cp .env.prod.example .env
# Fill in POSTGRES_PASSWORD. Generate it with:
#   openssl rand -base64 24 | tr -d '/+=' | head -c 32
# It must be URL-safe: it is interpolated into DATABASE_URL.
# Keep APP_COOKIE_SECURE=false while the app is served over plain HTTP.
#
# If this host also develops, merge the block into the existing `.env`
# instead of overwriting it: `make` targets read the same file.

docker compose -f compose.prod.yaml build
docker compose -f compose.prod.yaml up -d
```

The `migrate` service applies the embedded migrations and exits; the `app`
service waits for it to complete successfully before starting. The server itself
never migrates on startup: a restarting process would migrate against itself.

Create the first owner once. There is no seeding, and the password is never a
container environment variable — it would stay there forever:

```bash
docker compose -f compose.prod.yaml run --rm bootstrap \
  --email owner@pyme.com --role owner
# password:        <- typed here, never echoed, never written to disk
# repeat password:
```

`bootstrap` is a service under the `init` profile, so `up -d` never starts it and
a restart never recreates a user that was deleted. It runs interactively, which
is why the password can be typed instead of piped: piping it (`--password-stdin`
with a herestring) leaves the plaintext in the shell history.

The older, non-interactive form still works where there is no terminal:

```bash
printf '%s' "$PW" | docker compose -f compose.prod.yaml exec -T app \
  create-user --email owner@pyme.com --role owner --password-stdin
```

Check it is up:

```bash
docker compose -f compose.prod.yaml ps -a           # db healthy, migrate exited 0, app healthy
curl -i http://localhost:3000/healthz             # 204
curl -i http://localhost:3000/readyz              # 204
curl -i http://localhost:3000/api/v1/does-not-exist # 404 problem+json, not index.html
```

Then open `http://<host-lan-ip>:3000` from another machine on the LAN.

### Access from the LAN and the tailnet

Plain HTTP on both. **`APP_COOKIE_SECURE` must stay `false`**: a cookie marked
`Secure` is silently dropped by the browser over `http://`, so login would appear
to succeed and every later request would return `401`. Tailscale encrypts
tailnet traffic end to end, so HTTP inside the tailnet is not readable on the
wire; only the LAN hop is plain.

Reaching the app by its tailnet IP needs nothing else. `tailscale serve` would
add HTTPS, but then a TLS proxy sits in front of the app and the per-IP login
limit would see one client, not many — the app deliberately ignores
`X-Forwarded-For`. That is a decision for later, together with `APP_COOKIE_SECURE=true`.

### Updating a running deployment

```bash
docker compose -f compose.prod.yaml build
docker compose -f compose.prod.yaml up -d
```

`up -d` recreates only what changed, and the `migrate` service runs again before
the server starts.

### Tests

`tests/deploy.rs` renders `compose.prod.yaml` through `docker compose config`
and asserts the contract: `DATABASE_URL` derived from `POSTGRES_PASSWORD`, the
`migrate` gate, `APP_COOKIE_SECURE=false`, the database port unpublished, and
every key documented in `.env.prod.example` actually forwarded. A documented
knob that compose never forwards is silently ignored, so the two files are
checked against each other rather than against intent. The file skips itself
when Docker is unavailable.

### Not implemented yet

**Backup and restore.** The deployment will hold stock, sales and cash records
that cannot be rebuilt. A scheduled `pg_dump` with retention and one tested
restore is required before real use; it is tracked as pending, outside the
current slice.
