# Admin UI - SvelteKit static served by Axum

Administration frontend for a small business (stock and supplies), served by the
same Rust binary in a single application container.

## Specs

S1. The frontend uses **SvelteKit with `adapter-static`** (SPA: `fallback: 'index.html'`, `ssr = false`, `prerender = false`). Versions are **Vite 8 + Svelte 5 + TypeScript 6**.

S2. **Development and deployment are separate concerns.** In development only Postgres runs in Docker (`make db-up`); the backend (`cargo run`) and the frontend (`npm run dev`) run on the host, so the toolchain stays native and the feedback loop stays fast. Deployment runs **two containers**: one for Postgres and another serving backend and frontend together from the same Axum process, so there is one origin and no CORS. The `Dockerfile` **does not run tests**; tests run locally and in CI.

S3. Axum serves `frontend/build/` with **fallback to `index.html`** for every route except `/api/v1/*`, `/healthz` and `/readyz`.

S4. In development, `vite dev` uses the **proxy** `'/api' -> http://localhost:3000`. The browser sees one origin, so the `HttpOnly; SameSite=Strict` cookie works without backend changes.

S5. All frontend `fetch` calls use **relative paths** (`/api/v1/...`). Never an absolute URL with a host.

S6. The **first screen is login**. Without a valid session, every route redirects to `/login`.

S7. After login there is **one sidebar navigation**, the same on every screen: an expandable rail showing icon + name, with groups by category (**Operations**: Sale; **Catalog**: Products, Movements; **System**: Users). The expand/collapse button remembers its state. **Sign out** is not a standalone rail item: it is in the menu opened by the user button, alongside the theme toggle. Opening the point of sale is a rail item (`/operations/pos`), not a link inside the page, and the sale screen has no navigation of its own.

S8. There is **no `/settings` screen** yet. Add it when needed.

S9. The product list shows name, SKU, type, price and **stock**, with search and pagination.

S10. **Decimals arrive as strings** from the API (`"1500.50"`, `"12.5"`) and are stored that way in TypeScript types. Never convert them to `number` for display.

S11. The UI **hides** items according to role, but the backend handles actual authorization. Hiding a link is not authorization.

S12. A `401` on any request leads to `/login`. A `429` displays the wait time from the `Retry-After` header.

S13. The API client is **generated from OpenAPI** (`utoipa` -> `openapi.json` -> TS types), not handwritten.

S14. **Migrations are a separate step, never part of server startup.** The image ships a `migrate` binary with the migrations embedded (`sqlx::migrate!`), and `compose.prod.yaml` runs it as a one-shot service that the server waits for (`condition: service_completed_successfully`). A restarting process that migrates would race itself, and two instances would migrate concurrently.

S15. The image ships **`organox`, `create-user` and `migrate`**. There is no seeding: the first `owner` is created once with a dedicated `bootstrap` service that runs `create-user` interactively. A password in a container environment variable would live there forever.

S16. The deployment reads **one `.env` per machine**, from `.env.prod.example`, and `POSTGRES_PASSWORD` is the **single source** of the database password: the compose file builds `DATABASE_URL` from it, so the two cannot disagree.

S17. **Plain HTTP over both LAN and tailnet, with `APP_COOKIE_SECURE=false`.** A `Secure` cookie is silently dropped over `http://`: the login appears to succeed and every later request returns 401. Tailscale encrypts tailnet traffic end to end, so only the LAN hop is in clear text. `tailscale serve`/TLS is a later decision that also changes the per-IP login limit (the app deliberately ignores `X-Forwarded-For`).

S18. The first `owner` is created by a **`bootstrap` service under the `init` profile**, run explicitly with `docker compose run --rm bootstrap`. It never starts with `up -d`, so a restart cannot recreate a deleted user, and the password is **typed at the terminal, never piped**: a herestring stays in the shell history and an environment variable stays in `docker inspect`.

S19. Backups run **on the host**, not in the application: `scripts/backup.sh` dumps the database from *inside* its container over the unix socket, so no password is passed on a command line or held in a container environment. The archive is `pg_dump -Fc` (compressed, verifiable with `pg_restore --list`), written as `.partial` and renamed only after the integrity check passes. **Rotation runs last**, so a failed dump can never delete the previous good backup.

S20. **A backup is proven by a restore, not by a dump.** `scripts/restore-check.sh` restores into a throwaway database, compares row counts table by table against the live database, checks the stock invariant on the restored copy, and drops the throwaway database whether the checks pass or fail. Backup **configuration** (`BACKUP_DIR`, retention, schedule) lives in the deployment `.env`, never in the database it protects: `down -v` would delete the record of how the data was being saved.

## Tasks
| ID | Task | Specs | Route | Status | Commit |
|---|---|---|---|---|---|
| T1 | OpenAPI with `utoipa` and TS type generation | S13 | inline | done | ae30bc2, 98f7240 |
| T2 | Scaffold SvelteKit + adapter-static + dev proxy | S1, S4 | inline | done | 98f7240 |
| T3 | Axum serves `build/` with fallback (API first) | S2, S3 | inline | done | ae30bc2 |
| T4 | API client: relative paths, 401->login, `Retry-After` | S5, S12, S13 | inline | done | 98f7240 |
| T5 | Login screen | S6, S12 | inline | done | 98f7240 |
| T6 | Layout with sidebar + session guard | S7, S11 | inline | done | 98f7240 |
| T7 | Product list with search and stock | S9, S10 | inline | done (search only, see T11) | 98f7240 |
| T8 | Deploy artifacts: `Dockerfile`, `.dockerignore`, `compose.prod.yaml`, `src/bin/migrate.rs`, `.env.prod.example`, `tests/deploy.rs`, README "Deploy" | S2, S14-S17 | inline | done | 6779565 |
| T9 | `bootstrap` service under the `init` profile, interactive password | S18 | inline | done | b9f104e |
| T10 | Local backup + verified restore: scripts, systemd timer, `docs/backup.md` | S19, S20 | inline | done | fbe2b6e, 5cf2155 |
| T11 | Pagination controls on the product list | S9 | inline | done | this branch |

Nothing in this table is a promise about the backend. A route that the UI needs
and the API does not expose is backend work, and it is recorded as a gap in the
document of its domain (`docs/auth.md`, `docs/catalog.md`, `docs/sales.md`).

## Log

L1. Original user request:

> "1 la idea es usarla en el local para todo el trabajo administrativo, pero tambien (si llegamos) poder vender desde otra PC en la misma red, o como mucho por tailscale en caso de querer acceder por fuera de la red local. Respondiendote la primera mi duda es si seguis sugeriendo htmx por el hecho de poder cubrir todo lo que necesite con htmx. [...] y pr la pregunta 3 si, pantalla de login primero, pero con un sidebar con productos y users y settings. Mas adelante vamos a ir agregando cosas"

L2. User correction:

> "explicame antes que significa adapter-static. Te respondo solamente la 2: vamos sin /settings por ahora, lo agregamos cuando haga falta"

L3. User decision:

> "me inclino por Svelte, pero conviene usar SvelteKit no? La idea del deploy va a ser lanzar un contenedor con postgres y otro con todo el backend y frontend juntos. Dejar test en local"

L4. User decision:

> "Si, vamos con lo último: Vite 8 + Svelte 5 + TS 7"

L5. Scope of this slice: login + sidebar + product list. Creating/editing products, users and sales are left for later slices.

L6. Slice delivered: login, sidebar (Products, Users), product list with search and stock. Creating/editing products and the user list are pending backend work.

L7. Implementation finding: SvelteKit 3 removed `svelte.config.js` (configuration goes in the Vite plugin) and removed `$lib` as a built-in module. Use `paths` in tsconfig.json + `resolve.alias` in vite.config.ts. TypeScript 6 with `moduleResolution: bundler` does not resolve package.json `imports` with `#lib/*` for extensionless targets.

L8. Finding: `fallback_service` on the entire router caused `/api/v1/no-existe` to return `index.html` with 200. The `unknown_api_route_returns_404` test caught it. The fallback now distinguishes `/api/*` (404 problem+json) from the rest (SPA).

L9. Visual direction to define in `design/admin-ui/`: three artboards (dense with dark sidebar, airy with cards, and point of sale with icon rail). The design system was captured in `design/system.md` from `frontend/src/app.css`. Adopt Tailwind v4 + shadcn-svelte (preset to be chosen) and a manual theme toggle; migrate the three current screens.

L10. Handoff implemented: artboard `03-pos-rail-iconos.html` (chosen). The point of sale was implemented at `/pos` with an icon rail, grid of real API products and persisted cart. The complete sales domain was added to support it (migration 0005, model, repository, service, handlers, 16 integration tests) because a browser-side cart would have contradicted the decision that total and stock are validated on the server. Tokens taken from `design/system.md`; styling remains native CSS with variables, the artboard HTML was not copied. Artboards 01 and 02 remain alternatives for administration screens.

L11. Pending from the handoff: artboards 01/02 were not implemented, and migration to Tailwind v4 + shadcn-svelte remains pending (the CLI requested a preset: nova, vega, maia, lyra, mira, luma, sera, rhea).

L12. Fixed the double sidebar at `/pos`: the point of sale was inside the `(app)` group and inherited the administration sidebar layout, so two sidebar navigations appeared. It was moved to its own `(pos)` group with its `+layout.ts` (session guard, no `+layout.svelte`, so it adds no visual shell). The "Go to sale ->" link remained in the product list header.

L13. Styling stack migrated: Tailwind v4 (`@tailwindcss/vite` in `vite.config.ts`) + shadcn-svelte **vega** preset with **olive** base color. The theme was written by hand in `src/app.css` because the interactive CLI `init` does not allow the final question (`Continue?`) to be completed non-interactively; `cssVars` values came from the official registry (`shadcn-svelte.com/init?preset=bK1Hro8`). Self-hosted Inter (`@fontsource-variable/inter`) was added. Login and product list migrated; POS retains its own CSS pending migration. The first build after migration broke the layout (@theme was unavailable), detected with a Playwright screenshot rather than HTTP.

L14. Dark mode implemented: `<ModeWatcher defaultMode="system" />` in the root layout, light/dark toggle (`$lib/components/theme-toggle.svelte`) in the admin sidebar and POS rail. Verified with Playwright in three scenarios (`colorScheme: light`, `colorScheme: dark`, and manual toggle over a light system): `html.class` changes to `dark` and `body` changes to `oklch(0.153 0.006 107.1)`.

L15. Bug found and fixed: POS still used the handcrafted CSS variables (`--bg`, `--surface`, `--accent`, `--text`, `--muted`...) deleted during the Tailwind migration. In light mode text was nearly invisible on a nearly white background. They were mapped to shadcn tokens (`--color-card`, `--color-foreground`, `--color-primary`, etc.). A custom `--warn` token was also added because the vega preset has no warning color and its `--chart-*` values are grayscale.

L16. User request on the design canvas: "hoy en dia existen dos sidebars, este y una que muestra productos y users nomas. Quiero que uses este como main y elimines el otro. Ademas agregale un boton para expandir donde muestre icono mas nombre (como en el ejemplo 1). Quita el boton de logout del sidebar y agregalo al hacer click en el boton del usuario" and "copia el separador por categorias de este diseño". The remaining double sidebar was not the POS one: it was navigation per screen (`(app)` with Products/Users, `(pos)` with its rail). The artboard 03 rail was adopted as the entire app's single navigation, with groups copied from artboard 01 (Operations / Catalog / System), an expand button (64 px icons <-> 236 px icon+name, preference in `localStorage`) and a user menu with theme and "Sign out". Sale moved to `/operations/pos` (with `/pos` as a 307 redirect) so the Sale item shares the rail with Products/Movements/Users without leaving the shell; `/operations/movements` was added so the artboard's Movements item has a real destination. Verified with `frontend/e2e/nav-shell.cjs` (19 checks: one sidebar navigation on all four screens, canonical navigation, sale action in the rail, expand/persist, user menu and real logout) plus `svelte-check` and `npm run build`.

L17. Spec correction: S1 said TypeScript 7, which is not installable today. `@sveltejs/kit` 3.1.1 — the latest published version — declares `peerDependencies: { "typescript": "^6.0.0" }`, and installing TypeScript 7.0.2 breaks `npm run check` with `Cannot read properties of undefined (reading 'readFile')` inside `svelte-kit sync`. The spec now says TypeScript 6, matching `frontend/package.json` (`^6.0.0`) and what is actually installed (6.0.3). Moving to 7 is a follow-up gated on Kit supporting it, not a decision this project can take alone.

L18. S2 correction (user request, 2025-11-05): the spec merged development and deployment into one sentence and promised a `Dockerfile` that does not exist. Original wording:

> "S2. Deployment uses **one container for Postgres and another serving backend and frontend together**. The `Dockerfile` **does not run tests**; tests run locally."

User statement:

> "sigue abierto lo de odd/tasks/ui-admin.md S2, que promete un Dockerfile que no existe. [...] Mi idea original es hacer los test automaticos y manual (yo levantarlo para ver como esta todo) en local usando solo postgres en docker y dejar el backend y frontend juntos en otro docker para deploy."

S2 now separates the two concerns and T8 tracks the missing artifacts. Still open and **not** decided by this correction: how the deploy exposes the app (see L19) and the shape of the first-user bootstrap in production.

L19. Transport decision (resolved in S17). The app defaults to `Secure` cookies (`src/config.rs:70`), which the browser discards over `http://`: login would succeed and every later request would return 401. User decision:

> "Mi decision: vamos con A HTTP plano."

That is `APP_COOKIE_SECURE=false` with LAN and tailnet both over plain HTTP, recorded as S17. The question that prompted it:

> "si elijo HTTP plano puedo seguir accediendo mediante tailscale?"

Yes: a cookie without the `Secure` flag travels over both HTTP and HTTPS, so `false` is the permissive setting and `true` is the restrictive one. Tailscale encrypts the tailnet hop with WireGuard, so only the LAN segment is in clear text.

L20. Out of this slice, tracked as pending: **backup and restore of the production database**. The deployment will hold stock, sales and cash records that cannot be rebuilt, so a scheduled `pg_dump` with retention and one tested restore is required before real use. User decision:

> "Para c anotalo como pendiente, queda fuera de este slice."

Postgres 18 is the target for a clean start; the wiped development volume was confirmed:

> "Para a ya hice el down -v, podemos arrancar de cero con postgres 18."

L21. **`compose.yaml` was broken for Postgres 18, and `down -v` alone did not fix it.** Postgres 18 moved `PGDATA` to `/var/lib/postgresql/18/docker` and its declared `VOLUME` to `/var/lib/postgresql`; the compose file still mounted the pre-18 path. The container restart-looped with "there appears to be PostgreSQL data in /var/lib/postgresql/data (unused mount/volume)", and it did so even on a fresh volume, because Docker creates the mount point. Fix: `db_data:/var/lib/postgresql`. Verified afterwards: `PGDATA=/var/lib/postgresql/18/docker`, `sqlx migrate run` applied 7/7, container healthy.

L22. T8 implemented. Three-stage `Dockerfile` (`node:26-alpine` builds the SPA, `rust:1-bookworm` builds `organox`/`create-user`/`migrate` with `SQLX_OFFLINE=true` against the committed `.sqlx/`, `debian:bookworm-slim` runs unprivileged as uid 10001 with `WORKDIR /app` because `FRONTEND_DIR` in `src/routes.rs:16` is relative and has no env override). `.dockerignore` keeps `target/`, `frontend/node_modules`, `frontend/build` and `.env*` out of the build context so a stale local SPA build cannot reach the image. `compose.prod.yaml` sets `name: organox-prod` so it never shares containers or volumes with the development stack, publishes no database port, gates `app` on `db` healthy and `migrate` completed, and adds a `curl /readyz` healthcheck (hence `curl` in the runtime image).

L23. Decision on configuration templates, answering "entonces el .env es innecesario?": there is **one `.env` per machine**, not one per environment. Dev and deploy each get a template (`.env.example`, `.env.prod.example`) and a machine copies the one that applies. Two coexisting files were rejected because the database password would then exist in two places that can disagree. Bootstrap is not configuration: `create-user` never reads a password from the environment.

L24. Verification of T8 (evidence, not intent): built `organox:local` (174 MB), brought up `compose.prod.yaml` against a throwaway env file and a fresh volume. Observed: `migrate` exited 0 and re-ran idempotently ("migrations applied"), `app` reached healthy, `GET /healthz` 204, `/readyz` 204, `/` 200 `text/html`, `/operations/pos` 200 `text/html` (SPA fallback), `/api/v1/nope` and `/api/foo` 404 `application/problem+json`, a hashed `_app/immutable` asset 200 `text/javascript`, `create-user` inside the container created the first `owner`, and login -> `/auth/me` -> `/products` all 200 with the cookie. Cookie flags compared directly: `APP_COOKIE_SECURE=false` yields `HttpOnly; SameSite=Strict; Path=/`, and `true` adds `Secure` — confirming S17's premise. Stack torn down with `down -v`; no deployment containers or volumes remain. `cargo fmt --check`, `cargo clippy -D warnings` and the suite (39 + 16 + 24 + 1 + 5 + 8 = 93 in the filtered run) pass.

L25. Independent verification (`gentle-ai-verify`) over the change found one blocker that self-review had missed: `.env.prod.example` documented `APP_LOGIN_MAX_FAILURES`, `APP_LOGIN_BLOCK_MINUTES` and `APP_LOGIN_IP_MAX_PER_MINUTE`, but `compose.prod.yaml` never forwarded them, so an operator setting a limit would silently keep the defaults. Fixed by wiring the three keys with defaults, and pinned by `tests/deploy.rs::every_documented_env_key_is_forwarded`, which reads both files and fails if the template names a key the compose file does not forward. Re-verified after the fix: `docker compose exec app printenv` shows `APP_LOGIN_MAX_FAILURES=7` when the env file sets 7. Also corrected from the same report: `docker compose ps` (without `-a`) hides the exited one-shot `migrate` service, and the project-structure line in README omitted `migrate`.

L26. Native review closed `approved` for lineage `review-cda22092d5275f8e` (medium tier, 10 changed files, all four lenses) and its acknowledgement burned the authority. Seven advisory findings, all non-blocking and recorded as later work rather than corrections: backup/restore absent (already L20), plaintext cookie on the LAN hop (the accepted S17 trade-off), two readability nits in README and `tests/deploy.rs`, and three reliability nits in `compose.prod.yaml:34`, `src/bin/migrate.rs:21-23` and the temp-directory handling in `tests/deploy.rs:34-39`.

L27. User question on the documented bootstrap command: "estoy leyendo este comando `docker compose -f compose.prod.yaml exec -T app create-user ... --password-stdin <<< 'la-password'`. No es mejor dejar setteado un user y password en el .env?" Answer: no. Seeding from the environment keeps the plaintext password in `.env` and in `docker inspect` forever, does not rotate (an `upsert` seed would overwrite a changed password and a create-if-missing seed leaves an orphan), and resurrects deleted users. The friction behind the question is real, so the fix is fewer keystrokes without persisting the secret: S18's interactive `bootstrap` service. The documented herestring was itself a defect: it leaves the plaintext password in `.bash_history`, and it is gone from the README.

L28. User request: "hace el 1 nomas, el bootstrap por ahora, descarta 2 y 3". T9 is limited to the interactive `bootstrap` service. Explicitly discarded, so nobody reopens them by accident:

- **Not doing: a settings table for business rules** (VAT included in prices, receipt numbering, business identity). Postponed, not rejected on the merits: it belongs in the database, not `.env`, and it is what S8's `/settings` screen is for.
- **Not doing: choosing which modules load** (sales, invoicing, accounting). The reasoning is recorded so it is not re-litigated from scratch: only sales is optional today, catalog and users are prerequisites; hiding a rail item is not authorization (S11 says the backend authorizes); and the open question that blocks the design is what happens to the data of a module that is switched off — hiding it leaves unauditable records, deleting it destroys history.

L29. T9 implemented and verified. The `bootstrap` service sits under the `init` profile with `entrypoint: ["create-user"]`, so arguments pass through after the service name and no password key exists in its environment (`tests/deploy.rs::bootstrap_is_profiled_and_carries_no_password` asserts the profile, the absence of `--password-stdin`, and the absence of any `PASSWORD` key other than the one embedded in `DATABASE_URL`). Verified with a real TTY (`script -qec`): the `password:` / `repeat password:` prompts appear without echo, the hash is argon2id, and the user is stored. `docker compose config --services` lists `app db migrate` by default and adds `bootstrap` only with `--profile init`, which is what keeps it out of `up -d`. The stack was torn down with `down -v` and left no residue.

L30. Independent verification of T9: `gentle-ai-verify` was launched but **stalled and was reported unavailable** (timeout after 30 minutes with a `bash` call still running). Per the bounded-stop rule it was not retried; the checks it would have performed were done inline instead, with mutation evidence rather than assertion:

- **V1 (profiled out of `up -d`)**: `docker compose -f compose.prod.yaml config --services` lists `app db migrate`; adding `--profile init` adds `bootstrap`. No service lists `bootstrap` under `depends_on`.
- **V2 (argument pass-through, highest risk)**: already proven empirically in L29 — `docker compose run --rm bootstrap --email boot@pyme.com --role owner` created `boot@pyme.com` with role `owner`, so `entrypoint: ["create-user"]` forwards the arguments to clap unchanged. The same run also proves `docker compose run` of an explicitly named profiled service does **not** require `--profile`, which is what the README relies on.
- **V3 (interactive path)**: `src/bin/create-user.rs:110-117` refuses when stdin is not a terminal and otherwise prompts twice through `rpassword` without echo.
- **V4 (the test is not vacuous)**: verified by mutation. Renaming the service to `bootstrapX` fails with "compose.prod.yaml must define a bootstrap service"; deleting the `profiles:` line fails with "bootstrap must sit under a profile". Both mutations were reverted and the full file re-tested (5/5 green). The block extraction is not order-dependent in the way that matters: it slices from `bootstrap:` to the next service header, and renaming or removing the key fails loudly instead of passing silently.
- **V6 (README matches the compose file)**: the documented command is the one that was executed.

L31. Third native review (lineage `review-a31a71a6f3eebc4c`, target 4 files / 131 lines, medium tier, correction budget 66 unused) closed `approved`, and its acknowledgement burned the authority. Five advisory findings, all non-blocking. Three share one root cause worth recording: `tests/deploy.rs` greps the `bootstrap` block for text instead of parsing it, so an **alias** (a YAML anchor that injects `--password-stdin` or a password key) would slip past it (`tests/deploy.rs:236-244`, `:238-239`). The test holds for the file as written and fails loudly when the service is renamed or the profile is removed (verified by mutation, L30), but it is a grep, not a contract. Parsing the rendered YAML would close that gap if this file becomes more dynamic. Also flagged: the README's non-interactive fallback (`README.md:320-321`) still mentions `--password-stdin`, which is the form that can leak into history; it is labeled as the no-terminal fallback and is kept for that reason.

L32. Closed the gap recorded in L31: `bootstrap_is_profiled_and_carries_no_password` no longer greps the compose text. It renders with `--profile init` — compose omits profiled services from the default render entirely, so without the flag there is nothing to assert on — and reads `services.bootstrap` from the JSON. The bypass was reproduced before fixing: with `environment: <<: [*app-env, *inject]` and an `x-inject` anchor defining `USER_PASSWORD`, the rendered service carried the key while the old grep reported an empty hit list, and the new test fails naming it. A first mutation attempt (aliasing `--password-stdin` into the entrypoint) proved nothing, because that string does land inside the block; the useful mutation had to inject through a key the block never spells out. Three advisory findings were left unfixed on purpose (joined-entrypoint grep, `contains("PASSWORD")` vs an allowlist, the `require_docker` early return reading as a guard): all stylistic, and touching the file would invalidate the approved candidate.

L33. T10 implemented after the planning round. Decisions taken with the user, one at a time:

- **Storage**: `~/organox-backups/` (the user's choice). `/var/backups` was rejected *after* checking permissions: it is root-owned, and the user-level timer cannot write there. The home has 187 GB free; the consequence (same disk as the data, so no disaster protection) is stated in `docs/backup.md` rather than glossed over.
- **Scheduler**: Linux only, systemd **user** timer with `loginctl enable-linger`. A system unit would run as root and need `docker` group membership to reach the socket, which is unrestricted root on the host.
- **Offsite (rclone)**: deferred to its own slice. No `BACKUP_REMOTE` placeholder was added: a variable that does nothing is documentation that lies.
- **`/settings`**: out of scope. Backup configuration cannot live in the database it protects, so `/settings` will *show* status, never configure it.

Three bugs were found by running the scripts, not by reading them: `pg_dump -f -` writes a file named `-` *inside the container* and leaves the redirect empty (the dump silently captured 0 bytes); `psql -c` built from one expanded string passes `-c <sql>` as a single argument and psql reads it as a database name; and under systemd the environment is empty, so `docker compose` cannot interpolate `POSTGRES_PASSWORD` unless the env file is passed explicitly (`COMPOSE_ENV_FILE`). Also corrected in the restore check: the stock invariant used a `kind` column that does not exist; the real expression is a `CASE` on `reason`, mirrored from `src/catalog/repository.rs`.

Verified by execution: real dump of a live deployment stack (24 KB, `-Fc`), restore check comparing five tables plus the stock invariant, rotation pruning 7 of 20 planted daily files and keeping 14 daily + 2 monthly, a corrupt archive rejected by `pg_restore --list` and by the restore, an unwritable `BACKUP_DIR` and a non-numeric retention rejected with clear messages, a stopped database rejected without touching existing files, and the systemd unit run for real (`journalctl --user` shows the run and the failure). Stack torn down with `down -v`; the test timer was removed afterwards.

`every_documented_env_key_is_forwarded` was renamed to `every_documented_env_key_is_consumed` and widened: the backup keys are read by host scripts, not by compose, and the contract that matters is "something reads it", not "compose forwards it". Verified that it still catches a planted dead key.

L34. Audit of this document against the tree. Two defects were found and fixed here, both of them in the document rather than in the code: (1) **L17 had been moved to the end of the Log**, behind L33, by `e448d5b`. It arrived between L16 and L18 in `fcf7ba8` and belongs there; the Log is chronological, so an entry out of order reads as a later decision than the ones above it. Moved back. (2) **The task table carried no state and a `-` commit column for T1-T7 and T10**, which made ten finished tasks indistinguishable from ten unstarted ones. The commits were recovered from history: T1 `ae30bc2` (`src/openapi.rs`, `src/bin/openapi.rs`, `openapi.json`) plus `98f7240` (`schema.d.ts`), T2-T7 `98f7240` (the SvelteKit commit that also added `vite.config.ts`, `client.ts`, the login screen, the rail and the product list), T10 `fbe2b6e` (scripts and `docs/backup.md`) plus `5cf2155` (timer and `Makefile`). A third defect, **S9 claiming pagination the product list does not have**, was found by the same audit and deliberately **not** fixed here: `+page.ts` passes `limit` and `offset` from the API response into the page data (`frontend/src/routes/(app)/products/+page.ts:12-13`) and no component reads them, while `limit` defaults to 50 in `src/catalog/handlers.rs:145`, so a catalog over 50 items is silently cut at 50 with nothing on screen saying so. No earlier Log entry declines it, so it is a defect against S9 rather than a deferred decision; recorded as **T11, pending**, and opened as issue #22. Not changed, because it is not a defect: L11 says the Tailwind migration is pending and L13 records it as done. A Log is a record of what was true when it was written; the later entry supersedes the earlier one and the pair is the history. Rewriting L11 would destroy the evidence that the migration was ever outstanding.

L35. Closed the two advisory findings of native review `review-5796d8f72ec581d6` (issue #24). Both were prose this document and `docs/auth.md` had just gained, and both were WARNING severity with informational disposition, which is why the review approved and they arrived here rather than as a correction. (1) `R2-MisleadingAuditSummary` at L34: the entry opened by claiming three defects were found *and fixed* when the third was tracked and left alone. L34 now separates the two that were fixed from the one that was recorded, so a reader learns from the first sentence what the audit actually changed. (2) `R2-SessionRevocationAmbiguity` in `docs/auth.md`: "a cookie copied from a second device" inverted the threat, because a cookie copied *from* a device is one that device already had; the sentence now names a stolen token replayed *on* another device. The same bullet also points at the unused `sessions_user_id_idx`, which serves no query today: the other two statements against `sessions` filter on `token_hash` and `expires_at`. Neither finding's text is quoted here on purpose: the closure envelope reports an id, a lens, a location and a severity, never the finding's wording, so the fixes above are the author's reading of the location and saying otherwise would be an invented citation.

L36. T11 implemented (issue #22). The defect was that the product list rendered whatever the API returned and counted it, while `GET /api/v1/products` defaults `limit` to 50, so a catalog of 51 products showed 50 with nothing saying so. Two decisions were taken with the user before writing code: the API gains a `total` **and** the frontend pages, rather than the frontend inferring "there is more" by requesting `limit + 1`. `total` is computed with `COUNT(*) OVER ()` in the page query (`src/catalog/repository.rs`), so it costs no second round trip; the one case the window function cannot answer is an empty page, where no row carries it, and `service::list_products` then re-counts through `repository::count_products`. That path is not theoretical: paging by `offset` means a product deleted between two requests leaves a page link past the end, and reporting `0` there would tell the user the catalog is empty. The screen carries `page` in the URL (1-based; the API keeps `offset`, 0-based, and `+page.ts` is the only place that converts), page changes are real navigations so Back returns to the previous page, and a new search resets to page 1 because staying on page 4 of a new result set shows an empty screen that reads as "no matches". The counter now says "1–50 of 137" instead of "50 products". Verified with three integration tests: the total ignores the page, the offset past the end still reports the real total, and the pre-existing filter tests now assert the total follows the same filters. `svelte-check` reports 0 errors and 0 warnings; one warning was found and fixed on the way (`<div>` nested in `<p>`).
