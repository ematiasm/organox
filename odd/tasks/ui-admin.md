# Admin UI - SvelteKit static served by Axum

Administration frontend for a small business (stock and supplies), served by the
same Rust binary in a single application container.

## Specs

S1. The frontend uses **SvelteKit with `adapter-static`** (SPA: `fallback: 'index.html'`, `ssr = false`, `prerender = false`). Versions are **Vite 8 + Svelte 5 + TypeScript 6**.

S2. Deployment uses **one container for Postgres and another serving backend and frontend together**. The `Dockerfile` **does not run tests**; tests run locally.

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

## Tasks

| ID | Task | Specs | Route | Commit |
|---|---|---|---|---|
| T1 | OpenAPI with `utoipa` and TS type generation | S13 | inline | - |
| T2 | Scaffold SvelteKit + adapter-static + dev proxy | S1, S4 | inline | - |
| T3 | Axum serves `build/` with fallback (API first) | S2, S3 | inline | - |
| T4 | API client: relative paths, 401->login, `Retry-After` | S5, S12, S13 | inline | - |
| T5 | Login screen | S6, S12 | inline | - |
| T6 | Layout with sidebar + session guard | S7, S11 | inline | - |
| T7 | Product list with search and stock | S9, S10 | inline | - |

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
