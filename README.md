# admin-app

Administrador de negocio para una PyME: stock e insumos.
Backend en Rust/Axum + PostgreSQL + sqlx. Single-tenant.

## Requisitos

- Rust estable (lo fija `rust-toolchain.toml`)
- Docker + Docker Compose
- `sqlx-cli`: `cargo install sqlx-cli --no-default-features --features postgres,rustls`

## Puesta en marcha

```bash
cp .env.example .env      # ajustar si hace falta
make db-up                # Postgres en Docker
make migrate              # aplica migraciones
make create-user EMAIL=duenio@pyme.com ROLE=owner   # crea el primer usuario
make run                  # http://localhost:3000
```

Comprobación rápida:

```bash
curl -i localhost:3000/healthz   # 204 - el proceso vive
curl -i localhost:3000/readyz    # 204 - la DB responde
```

## Comandos

`make help` lista todo. Los importantes:

| Comando | Qué hace |
|---|---|
| `make db-up` / `db-down` | Levanta / baja Postgres (conserva datos) |
| `make db-reset` | Recrea la DB desde cero (borra datos, pide confirmación) |
| `make migrate` | Aplica migraciones pendientes |
| `make new-migration NAME=...` | Crea una migración reversible |
| `make create-user EMAIL=... ROLE=...` | Crea un usuario (pide la contraseña sin eco) |
| `make prepare` | Regenera `.sqlx/` (necesario al cambiar queries) |
| `make fe-install` | Instala dependencias del frontend |
| `make fe-dev` | Frontend con HMR en `:5173` (proxy `/api` → `:3000`) |
| `make fe-build` | Compila el frontend a `frontend/build` |
| `make fe-check` | Type-check del frontend |
| `make fe-schema` | Regenera los tipos TS desde el OpenAPI |
| `make check` | `fmt` + `clippy -D warnings` + `test` |

## Estructura

```
src/
  main.rs        arranque: config, logging, DB, router, serve
  lib.rs         AppState y módulos
  config.rs      configuración tipada desde entorno
  error.rs       AppError -> application/problem+json
  telemetry.rs   tracing (pretty en dev, json en prod)
  db.rs          pool de conexiones y ping
  routes.rs      router HTTP (/healthz, /readyz, /api/v1)
  auth/
    model.rs      User, Role, PasswordHash, TTLs de sesión
    validation.rs email y reglas de contraseña
    password.rs   argon2id
    token.rs      token de sesión y su SHA-256
    repository.rs SQL de auth (único lugar con queries)
    service.rs    casos de uso y política
    extract.rs    AuthUser (única vía para conocer al actor)
  catalog/
    model.rs      Product, ProductKind, MovementReason, signo del movimiento
    validation.rs nombre, SKU, precio, cantidad
    repository.rs SQL del catálogo y cálculo de stock
    service.rs    reglas: stock negativo, servicio sin stock, transacción
    handlers.rs   endpoints del catálogo
  sales/
    model.rs      Sale, SaleItem, SaleStatus
    repository.rs SQL de ventas y el cobro transaccional
    service.rs    reglas: un borrador por cajero, servicio no vendible
    handlers.rs   endpoints de venta
  ratelimit/
    account.rs    intentos fallidos por cuenta (Postgres)
    ip.rs         requests por IP (memoria)
    middleware.rs aplica el límite por IP solo a /auth/login
migrations/      migraciones SQL versionadas
  bin/create-user.rs  alta de usuarios por terminal
  openapi.rs     spec OpenAPI 3.1 generado desde el código
  bin/openapi.rs imprime el spec (cargo run --bin openapi)
tests/           tests de integración (Router en memoria + DB real)
frontend/        SvelteKit 3 + Svelte 5 + TS 6 (SPA, adapter-static)
docs/            decisiones de diseño
.sqlx/           cache de queries (commiteado: compila sin DB)
```

## Convenciones del proyecto

- Las migraciones aplicadas son **inmutables**. Un cambio de schema es una
  migración nueva, nunca una edición de la anterior.
- Toda ruta de negocio vive bajo `/api/v1`.
- Un solo tipo de error (`AppError`); los detalles internos se loguean, no se
  serializan al cliente.
- Timestamps: `TIMESTAMPTZ` en UTC. Ids: `uuid` (v7). Dinero: nunca `f64`.
- La app **no** corre migraciones al arrancar: las corre `make migrate`.
- `/healthz` = liveness (proceso vivo, no toca la DB). `/readyz` = readiness
  (la DB responde).
- Compilación sin DB: `SQLX_OFFLINE=true cargo build` usa `.sqlx/`. Si cambiás
  una query, corré `make prepare`.
- **El stock no se guarda, se calcula** (suma de `stock_movements`). El libro es
  append-only: se corrige con un ajuste, nunca editando historial.
- Decimales viajan como **string** en JSON, nunca como número: `"1500.50"`.
- **Rate limiting no prohíbe el abuso, lo acota.** 5 fallos por cuenta → bloqueo
  de 5 min que expira solo. Un bloqueo permanente dejaría a cualquiera dejar
  afuera al dueño escribiendo mal su contraseña.
- `X-Forwarded-For` se ignora: se usa la IP de la conexión TCP. Honrarlo sin
  proxy confiable anula el límite por IP.

## API

Base: `/api/v1`. Errores en `application/problem+json` (RFC 7807).

| Método | Ruta | Auth | Descripción |
|---|---|---|---|
| POST | `/auth/login` | — | `{email, password}` → usuario + cookie de sesión |
| POST | `/auth/logout` | sesión | Borra la sesión en la DB |
| GET | `/auth/me` | sesión | Usuario actual |
| POST | `/users` | `owner` | `{email, password, role}` → 201 con id |
| GET | `/products` | sesión | Lista con stock calculado (`?kind=&search=&include_inactive=`) |
| POST | `/products` | sesión | Alta de producto o servicio |
| GET | `/products/{id}` | sesión | Detalle con stock |
| PATCH | `/products/{id}` | sesión | Actualiza solo los campos enviados |
| POST | `/products/{id}/deactivate` | sesión | Baja lógica, idempotente |
| GET | `/products/{id}/movements` | sesión | Historial, más nuevo primero |
| POST | `/products/{id}/movements` | sesión | Registra movimiento de stock |
| POST | `/sales/draft` | sesión | Abre (o reusa) el carrito |
| GET | `/sales/{id}` | sesión | Venta con total calculado por el servidor |
| POST | `/sales/{id}/items` | sesión | Agrega ítem |
| PUT | `/sales/{id}/items/{product_id}` | sesión | Fija cantidad (0 = quitar) |
| DELETE | `/sales/{id}/items/{product_id}` | sesión | Quita ítem |
| POST | `/sales/{id}/complete` | sesión | Cobra y descuenta stock |
| POST | `/sales/{id}/cancel` | sesión | Cancela el borrador |
| GET | `/sales` | sesión | Ventas cobradas |

```bash
# login y guardar cookie
curl -c jar.txt -X POST localhost:3000/api/v1/auth/login \
  -H 'content-type: application/json' \
  -d '{"email":"duenio@pyme.com","password":"..."}'

curl -b jar.txt localhost:3000/api/v1/auth/me
```

## Estado actual

Rebanadas 1–3: esqueleto, autenticación y catálogo con libro de stock.

- **Auth**: login/logout/`me`, sesiones en DB revocables, cookie
  `HttpOnly`+`SameSite=Strict`, argon2id. RBAC binario (`owner`, `encargado`).
- **Catálogo**: productos y servicios, SKU único, búsqueda, baja lógica.
- **Stock**: libro append-only. El stock **no es una columna**, es la suma
  firmada de movimientos. No se puede vender lo que no hay.
- **Rate limiting en login**: bloqueo por cuenta (sobrevive reinicios) y por IP
  (corta antes de gastar argon2), con `429` + `Retry-After`.
- **Ventas**: carrito persistido en el servidor con cobro transaccional.
- **113 tests** (unitarios + integración contra Postgres real).
- **UI**: SvelteKit (SPA) servida por el mismo Axum. Login, **un único rail de
  navegación** (expandible, con grupos y menú de usuario), listado de productos,
  movimientos y **punto de venta con lector de código de barras** en
  `/operations/pos`.
- **OpenAPI 3.1** generado desde el código; los tipos del frontend se generan
  de ese spec (`make fe-schema`).

Ver `docs/auth.md`, `docs/catalog.md` y `docs/ratelimit.md` para el razonamiento
y qué falta.

## Desarrollo del frontend

Dos opciones:

**Con HMR** (recomendado mientras trabajás en la UI):

```bash
make db-up && make migrate
make run          # terminal 1: backend en :3000
make fe-dev       # terminal 2: frontend en :5173
```

El proxy de Vite reenvía `/api` a `:3000`, así el navegador ve **un solo
origen** y la cookie `SameSite=Strict` funciona. Sin el proxy, todo da 401.

**Producción local** (un solo servidor):

```bash
make fe-build     # genera frontend/build
make run          # Axum sirve la SPA con fallback a index.html
```

Abrí `http://localhost:3000`. La documentación interactiva de la API está en
`http://localhost:3000/api/v1/docs`.

Próxima rebanada: alta y edición de productos en la UI, listado de usuarios, o
el primer reporte de negocio.
