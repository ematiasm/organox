# English as the default language for artifacts

Make English the default language for every technical artifact in this
repository: documentation, code comments, doc comments, SQL comments, CLI help,
API descriptions and end-user UI copy. Conversation stays Spanish.

## Specs

S1. Every documentation file in the repository is written in English:
`README.md`, `docs/*.md`, `design/system.md`, `design/*.html`, and the author
prose in `odd/tasks/*.md`.

S2. Every code comment and doc comment is written in English: `//!`, `///`, `//`
and `/* */` in `src/**` and `tests/**`. Migrations from `0007` onward write
`--` comments in English; migrations already applied stay byte-identical,
because sqlx checksums them.

S3. Every runtime string a human reads is English: `AppError` `detail` values,
validation messages, conflict messages, rate-limit messages, `create-user` CLI
help and output, and OpenAPI `summary`/`description`/`example` values.

S4. Every string rendered by the frontend is English, and the document language
follows: `frontend/src/app.html` declares `lang="en"`.

S5. All date, number and currency formatting uses the `en-US` locale. The
`es-AR` locale is removed.

S6. The role literal stored in PostgreSQL and exposed by the API is `manager`,
not `encargado`, and the Rust variant is `Role::Manager`. The rename is applied
by a new migration; applied migrations stay immutable.

S7. `make db-reset` asks for the literal confirmation `yes`. The literal `si` is
no longer accepted.

S8. Generated files are regenerated, never hand-edited: `openapi.json` from
`cargo run --quiet --bin openapi`, and `frontend/src/lib/api/schema.d.ts` from
`make fe-schema`.

S9. Verbatim user quotes remain verbatim. The quotes embedded in
`odd/tasks/ui-admin.md` (`L1`-`L4`, `L16`) and in
`design/admin-ui/comments.json` stay in their original Spanish, because they are
recorded evidence, not documentation prose.

S10. The convention is durable and discoverable by agents and humans:
`AGENTS.md` at the repository root states it, and `README.md` points to it.

S11. No behavior changes beyond S4, S5 and S6. Test assertions that pin
behavior keep passing unchanged in meaning.

## Tasks

| ID | Task | Specs | Route | Commit |
|---|---|---|---|---|
| T1 | Rename role `encargado` to `manager` (migration, Rust, CLI, tests, Makefile, docs word) | S6 | inline | `refactor:` |
| T2 | Translate `src/**` runtime strings and comments | S2, S3 | worker | `refactor:` |
| T3 | Translate `tests/**` comments, assertion messages and fixtures | S2 | worker | `docs(test):` |
| T4 | Translate the append-only trigger message via a new migration | S2 | inline | `docs(db):` |
| T5 | Translate frontend copy, `lang="en"`, `en-US` locale, e2e labels | S4, S5 | worker | `refactor(frontend):` |
| T6 | Translate `Makefile`, `compose.yaml`, `.env.example`, `yes` confirmation | S3, S7 | inline | `chore:` |
| T7 | Regenerate `openapi.json` and `schema.d.ts` | S8 | inline | `chore:` |
| T8 | Translate `README.md` and `docs/*.md` | S1 | worker | `docs:` |
| T9 | Translate `design/**` and author prose in `odd/tasks/ui-admin.md` | S1, S9 | worker | `docs(design):` |
| T10 | Write `AGENTS.md` with the language convention | S10 | inline | `docs:` |
| T11 | Full check: `make check`, zero residual Spanish scan | S11 | inline | — |

## Log

L1. Original user request:

> "cambia el readme a ingles y siempre usa default ingles para todo lo que es documentacion, codigo (gentle-shell no tiene esa convencion?). Solo español para hablar aca en el chat"

L2. User decisions, taken before starting:

> Alcance: "Todo, incluida la UI"
> Convención durable: "AGENTS.md en la raíz"
> Rol `encargado`: "Renombrar a `manager` en DB y API"
> Locale: "Pasar también a en-US"
> Confirmación de `db-reset`: "`yes` (traducido)"

L3. Baseline before touching anything: `cargo fmt --check` clean, `clippy
--all-targets -D warnings` clean, 77 `sqlx::test` + 39 `#[test]` green,
`svelte-check` 0 errors / 0 warnings. The local database had no migrations
applied; all five were run.

L4. Initial inventory: 458 non-ASCII lines in `src/`, 102 in `tests/`, 147 in
`frontend/src/`, 66 in `migrations/`, ~750 documentation lines, 56 in
`openapi.json`. `tests/` never asserts against Spanish text, so no test breaks
from translating a runtime string. The only suite that does is
`frontend/e2e/nav-shell.cjs`, which is not part of `make check`.

L5. Finding that changed T4's scope: **sqlx checksums every applied
migration.** Proven by appending a comment to `migrations/0005_sales.sql`;
`sqlx migrate run` failed with "migration 5 was previously applied but has been
modified". Translating a comment in an applied migration is therefore a breaking
change, exactly like editing its SQL, because it would force
`sqlx migrate repair` on every existing database. `0001`-`0005` are frozen as
they are, and the rule becomes: English comments from `0007` onward.

L6. Scope authorization given to the tests worker: it may edit SQL literals
inside `tests/**` when they embed a translated fixture (they are not checksummed
migrations), under the rule that an assertion must never change what it proves.

L7. The `stock_movements_are_append_only()` trigger message **is** fixed without
breaking checksums: a new migration `0007` with `CREATE OR REPLACE FUNCTION`,
same name, same behavior (still rejects `UPDATE` and `DELETE`). Verified by hand
against the local database: the trigger now answers `ERROR: stock_movements is
append-only: record an adjustment instead of rewriting history`. That is why
`docs/catalog.md` can quote the real English error instead of an invented text.

L8. `make db-reset` confirmation verified live: `no` and `si` both abort with
exit 1 and leave Postgres running; only `yes` proceeds. `.env` and
`.env.example` are blocked by the harness as sensitive paths, so the user
translates those two files by hand.

L9. Commit slices, recorded so a reviewer can take them one at a time:

| Commit | Contents | Tasks |
|---|---|---|
| `docs: establish English as the default artifact language` | `AGENTS.md`, the feature document, author prose in `odd/tasks/ui-admin.md` | T9, T10 |
| `refactor!: rename the operator role from encargado to manager` | migrations `0006`/`0007`, `src/auth/model.rs` role, role assertions in `tests/auth.rs`, `.sqlx/` | T1, T4 |
| `refactor: translate the Rust backend to English` | `src/**` strings and comments | T2 |
| `docs(test): translate the test suite to English` | `tests/**` | T3 |
| `refactor(frontend): translate the UI to English` | `frontend/**`, `lang="en"`, `en-US`, e2e labels | T5 |
| `chore: translate build and environment files` | `Makefile`, `compose.yaml`, `.env.example` | T6 |
| `docs: translate the project documentation to English` | `README.md`, `docs/**` | T8 |
| `docs(design): translate the design artifacts to English` | `design/**` | T9 |
| `chore: regenerate OpenAPI and TypeScript types` | `openapi.json`, `schema.d.ts`, `.sqlx/` | T7 |

The role rename is its own commit because a caller consuming the API breaks
when the wire value changes: it gets native review alone rather than buried in a
translation diff.

L10. Verification actually observed, not assumed:

- `make check` green end to end: `fmt` clean, `clippy --all-targets -D warnings`
  clean, 114 tests passing (39 unit + 75 integration), `svelte-check` 0 errors /
  0 warnings.
- `npm run build` writes the site; the built bundle contains no Spanish copy.
- Live smoke test against the running binary: `/healthz` and `/readyz` return
  `204`, an unauthenticated `/api/v1/auth/me` answers
  `{"detail":"Invalid credentials or expired session.","status":401,...}`, an
  unknown API route returns `404` instead of the SPA shell, the served OpenAPI
  reports `Role: ['owner', 'manager']`, and `/login` serves the SPA.
- `make db-reset` refuses `no` and the old `si`, and the trigger now raises the
  English message.
- `SQLX_OFFLINE=true cargo build` still compiles from `.sqlx/`.
- Residual Spanish, every line accounted for: 66 lines in frozen migrations
  `0001`-`0005`, 2 verbatim quotes in `design/admin-ui/comments.json`, and 6
  lines in `odd/tasks/*.md` that are user quotes or the decisions quoted from
  this conversation. Nothing else.

L11. Known limit: `.env` and `.env.example` are blocked by the harness as
sensitive paths, so their comments are still Spanish. The user translates those
two files by hand; `AGENTS.md` covers the convention for the future.
