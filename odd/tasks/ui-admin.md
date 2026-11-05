# UI del administrador — SvelteKit static servido por Axum

Frontend de administración para una PyME (stock e insumos), servido por el mismo
binario Rust en un solo contenedor de aplicación.

## Specs

S1. El frontend usa **SvelteKit con `adapter-static`** (SPA: `fallback: 'index.html'`, `ssr = false`, `prerender = false`). Las versiones son **Vite 8 + Svelte 5 + TypeScript 7**.

S2. El deploy es **un contenedor con Postgres y otro con backend y frontend servidos juntos**. El `Dockerfile` **no corre tests**; los tests se corren en local.

S3. Axum sirve `frontend/build/` con **fallback a `index.html`** para toda ruta que no sea `/api/v1/*`, `/healthz` ni `/readyz`.

S4. En desarrollo, `vite dev` usa **proxy** `'/api' → http://localhost:3000`. El navegador ve un solo origen, así la cookie `HttpOnly; SameSite=Strict` funciona sin cambios en el backend.

S5. Todos los `fetch` del frontend usan **rutas relativas** (`/api/v1/...`). Nunca una URL absoluta con host.

S6. La **primera pantalla es el login**. Sin sesión válida, cualquier ruta redirige a `/login`.

S7. Tras el login hay **una sola navegación lateral**, la misma en todas las pantallas: rail expandible que muestra icono + nombre, con grupos por categoría (**Operación**: Venta; **Catálogo**: Productos, Movimientos; **Sistema**: Usuarios). El botón de expandir/contraer recuerda su estado. **Salir** no vive suelto en el rail: está en el menú que abre el botón de usuario, junto al interruptor de tema. Abrir el punto de venta es un ítem del rail (`/operations/pos`), no un enlace dentro de la página, y la pantalla de venta no trae navegación propia.

S8. **No hay pantalla de `/settings`** por ahora. Se agrega cuando haga falta.

S9. El listado de productos muestra nombre, SKU, tipo, precio y **stock**, con búsqueda y paginación.

S10. Los **decimales llegan como string** desde la API (`"1500.50"`, `"12.5"`) y así se guardan en los tipos de TypeScript. Nunca se convierten a `number` para mostrarlos.

S11. La UI **oculta** ítems según el rol, pero la autorización real la hace el backend. Ocultar un link no es autorizar.

S12. Un `401` en cualquier request lleva a `/login`. Un `429` muestra el tiempo de espera del header `Retry-After`.

S13. El cliente de API es **generado desde OpenAPI** (`utoipa` → `openapi.json` → tipos TS), no escrito a mano.

## Tasks

| ID | Tarea | Specs | Route | Commit |
|---|---|---|---|---|
| T1 | OpenAPI con `utoipa` y generación de tipos TS | S13 | inline | — |
| T2 | Scaffold SvelteKit + adapter-static + proxy de dev | S1, S4 | inline | — |
| T3 | Axum sirve `build/` con fallback (orden API primero) | S2, S3 | inline | — |
| T4 | Cliente de API: rutas relativas, 401→login, `Retry-After` | S5, S12, S13 | inline | — |
| T5 | Pantalla de login | S6, S12 | inline | — |
| T6 | Layout con sidebar + guard de sesión | S7, S11 | inline | — |
| T7 | Listado de productos con búsqueda y stock | S9, S10 | inline | — |

## Log

L1. Pedido original del usuario:

> "1 la idea es usarla en el local para todo el trabajo administrativo, pero tambien (si llegamos) poder vender desde otra PC en la misma red, o como mucho por tailscale en caso de querer acceder por fuera de la red local. Respondiendote la primera mi duda es si seguis sugeriendo htmx por el hecho de poder cubrir todo lo que necesite con htmx. [...] y pr la pregunta 3 si, pantalla de login primero, pero con un sidebar con productos y users y settings. Mas adelante vamos a ir agregando cosas"

L2. Corrección del usuario:

> "explicame antes que significa adapter-static. Te respondo solamente la 2: vamos sin /settings por ahora, lo agregamos cuando haga falta"

L3. Decisión del usuario:

> "me inclino por Svelte, pero conviene usar SvelteKit no? La idea del deploy va a ser lanzar un contenedor con postgres y otro con todo el backend y frontend juntos. Dejar test en local"

L4. Decisión del usuario:

> "Si, vamos con lo último: Vite 8 + Svelte 5 + TS 7"

L5. Alcance de esta rebanada: login + sidebar + listado de productos. Alta/edición de productos, usuarios y ventas quedan para rebanadas siguientes.

L6. Rebanada entregada: login, sidebar (Productos, Usuarios), listado de productos con búsqueda y stock. Alta/edición de productos y listado de usuarios quedan pendientes de backend.

L7. Hallazgo durante la implementación: SvelteKit 3 eliminó `svelte.config.js` (configuración va en el plugin de Vite) y eliminó `$lib` como módulo integrado. Se usa `paths` en tsconfig.json + `resolve.alias` en vite.config.ts. El `imports` de package.json con `#lib/*` no lo resuelve TypeScript 6 con `moduleResolution: bundler` para destinos sin extensión.

L8. Hallazgo: `fallback_service` sobre todo el router hacía que `/api/v1/no-existe` devolviera `index.html` con 200. El test `unknown_api_route_returns_404` lo detectó. Ahora el fallback distingue `/api/*` (404 problem+json) del resto (SPA).

L9. Dirección visual a definir en `design/admin-ui/`: tres artboards (denso con sidebar oscuro, aireado con tarjetas, y punto de venta con rail de iconos). El sistema de diseño se capturó en `design/system.md` desde `frontend/src/app.css`. Se adopta Tailwind v4 + shadcn-svelte (preset a elegir) y toggle manual de tema; las tres pantallas actuales se migran.

L10. Handoff implementado: artboard `03-pos-rail-iconos.html` (elegido). Se implementó el punto de venta en `/pos` con rail de iconos, grilla de productos reales del API y carrito persistido. Para sostenerlo se agregó el dominio de ventas completo (migración 0005, modelo, repositorio, servicio, handlers, 16 tests de integración) porque el carrito en el navegador habría contradicho la decisión de que el total y el stock se validan en el servidor. Tokens tomados de `design/system.md`; el estilo se mantiene en CSS nativo con variables, no se copió el HTML del artboard. Los artboards 01 y 02 quedan como alternativas para las pantallas de administración.

L11. Pendiente derivado del handoff: los artboards 01/02 no se implementaron, y la migración a Tailwind v4 + shadcn-svelte sigue pendiente (el CLI pidió un preset: nova, vega, maia, lyra, mira, luma, sera, rhea).

L12. Corregido el doble sidebar en `/pos`: el punto de venta estaba dentro del grupo `(app)` y heredaba el layout con sidebar de administración, así que se veían dos navegaciones laterales. Se movió a un grupo propio `(pos)` con su `+layout.ts` (guard de sesión, sin `+layout.svelte`, así no agrega cáscara visual). El enlace "Ir a venta →" quedó en el encabezado del listado de productos.

L13. Stack de estilos migrado: Tailwind v4 (`@tailwindcss/vite` en `vite.config.ts`) + shadcn-svelte preset **vega** con base color **olive**. El tema se escribió a mano en `src/app.css` porque el `init` interactivo del CLI no permite completar la última pregunta (`Continue?`) de forma no interactiva; los valores de `cssVars` se tomaron del registry oficial (`shadcn-svelte.com/init?preset=bK1Hro8`). Se agregó Inter autohospedada (`@fontsource-variable/inter`). Migradas login y listado de productos; el POS conserva CSS propio pendiente de migrar. El primer build tras la migración rompió el layout (los @theme no estaban disponibles) y se detectó con captura de Playwright, no por HTTP.

L14. Modo oscuro implementado: `<ModeWatcher defaultMode="system" />` en el layout raíz, botón de alternancia claro/oscuro (`$lib/components/theme-toggle.svelte`) en el sidebar de admin y en el rail del POS. Verificado con Playwright en tres escenarios (`colorScheme: light`, `colorScheme: dark`, y toggle manual sobre sistema claro): `html.class` pasa a `dark` y `body` cambia a `oklch(0.153 0.006 107.1)`.

L15. Bug encontrado y corregido: el POS seguía usando las variables del CSS artesanal (`--bg`, `--surface`, `--accent`, `--text`, `--muted`…) que se borraron al migrar a Tailwind. En tema claro quedaba texto casi invisible sobre fondo casi blanco. Se mapearon a los tokens de shadcn (`--color-card`, `--color-foreground`, `--color-primary`, etc.). Además se agregó un token propio `--warn` porque el preset vega no trae color de advertencia y sus `--chart-*` son escala de grises.

L16. Pedido del usuario en el canvas del diseño: "hoy en dia existen dos sidebars, este y una que muestra productos y users nomas. Quiero que uses este como main y elimines el otro. Ademas agregale un boton para expandir donde muestre icono mas nombre (como en el ejemplo 1). Quita el boton de logout del sidebar y agregalo al hacer click en el boton del usuario" y "copia el separador por categorias de este diseño". El doble sidebar que quedaba no era el del POS: era una navegación por pantalla (`(app)` con Productos/Usuarios, `(pos)` con su rail). Se adoptó el rail del artboard 03 como navegación única de toda la app, con grupos Copiados del artboard 01 (Operación / Catálogo / Sistema), botón de expandir (64 px iconos ↔ 236 px icono+nombre, preferencia en `localStorage`) y menú de usuario con el tema y "Salir". La venta se movió a `/operations/pos` (con `/pos` como redirección 307) para que el ítem Venta comparta rail con Productos/Movimientos/Usuarios sin navegar fuera de la cáscara; se agregó `/operations/movements` para que el ítem Movimientos del artboard tenga destino real. Verificado con `frontend/e2e/nav-shell.cjs` (19 checks: una sola navegación lateral en las cuatro pantallas, navegación canónica, acción de venta en el rail, expandir/persistir, menú de usuario y logout real) más `svelte-check` y `npm run build`.
