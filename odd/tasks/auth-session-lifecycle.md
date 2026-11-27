# Auth session lifecycle hardening (#110)

Objective: Harden the auth session lifecycle for GitHub issue #110 (password-change rate limiting, scheduled purges, atomic session token rotation, and sliding session cookie expiration sync).
Branch: `fix/auth-session-lifecycle` | Delivery: `ask-on-risk` (~450 authored changed lines forecasted across 4 work units) | Runner: `cargo test` (`make check`, `make prepare`, `make fe-schema`)

## Specs

S1. Rate-limit `POST /api/v1/auth/password` (IP + per-user throttling).
  - Issue quote: "Rate-limit `POST /api/v1/auth/password` (IP + per-user throttling)."
  - IP layer via `crate::ratelimit::middleware::limit_by_ip` (same layer as `/auth/login`), returning RFC 7807 `429 Too Many Requests` + `Retry-After`.
  - Per-user layer via `AccountLimiter` (`begin_attempt` before user lookup / Argon2, `record_success` on valid current password), preserving existing validation order (`validate_password(new_password)` and `current_password.len() > MAX_PASSWORD_BYTES` before DB / Argon2).
  - OpenAPI `change_password_doc` documents `429` + `Retry-After`.

S2. Run periodic cleanup for expired `sessions` and stale `login_attempts` (background task in `main.rs` or scheduled CLI job).
  - Issue quote: "Run periodic cleanup for expired `sessions` and stale `login_attempts` (background task in `main.rs` or scheduled CLI job)."
  - Background task in `src/main.rs` powered by a testable `auth::cleanup` module (`DEFAULT_CLEANUP_INTERVAL = 1h`, `MIN_ATTEMPT_RETENTION = 24h` floored above `window + block_duration`), exiting cleanly on `SIGINT`/`SIGTERM` via `tokio::sync::watch`.
  - Harden `AccountLimiter::purge_stale` with `AND (blocked_until IS NULL OR blocked_until <= now())` so active lockouts are never purged early.

S3. Rotate the caller's session token atomically (new session + revoke all old sessions + emit new `Set-Cookie`) on password change and self-reset.
  - Issue quote: "Rotate the caller's session token atomically (new session + revoke all old sessions + emit new `Set-Cookie`) on password change and self-reset."
  - Generalize `repository::create_session` to accept generic Postgres `Executor`.
  - Inside one transaction, `service::change_password` and `service::reset_password` update the password hash, revoke ALL existing sessions of the target user, and (for `change_password` and owner self-`reset_password`) insert a newly generated session token and return `IssuedSession`.
  - Handlers emit `Set-Cookie` with the rotated token while returning `204 No Content`. Old caller cookie must return `401`; rotated cookie must return `200`.

S4. Periodically refresh the session cookie expiration (or align cookie lifetime with server-side sliding expiry).
  - Issue quote: "Periodically refresh the session cookie expiration (or align cookie lifetime with server-side sliding expiry)."
  - Define `SESSION_REFRESH_INTERVAL` (1 hour) and single shared `session_cookie` builder in `src/auth/extract.rs`.
  - `find_session_user` computes `needs_refresh` against `LEAST(now() + SESSION_TTL, created_at + SESSION_MAX_LIFETIME)`; when due (advances by `>= SESSION_REFRESH_INTERVAL` or reaches the 30-day cap), `authenticate_session` runs synchronous best-effort `touch_session` (`RETURNING expires_at`) and `AuthUser` emits a refreshed `Set-Cookie` with the exact DB expiry.
  - No `Set-Cookie` churn within the refresh interval or once already at the 30-day cap; `logout`, `change_password`, and self-`reset_password` still emit exactly one `Set-Cookie` header (removal or rotated token).

## Tasks

- [x] T1 | S1 | inline | commit `19b6094`: Rate-limit `POST /api/v1/auth/password` (IP + `AccountLimiter`), update OpenAPI and tests.
- [x] T2 | S3 | inline | commit `668e407`: Rotate session token atomically in `change_password` and owner self-`reset_password`, update OpenAPI and tests.
- [x] T3 | S4 | inline | commit `c61267a`: Sync session cookie `Expires` with server-side sliding expiry (`SESSION_REFRESH_INTERVAL`), add tests.
- [x] T4 | S2 | inline | commit `bf55555`: Harden `purge_stale`, add `auth::cleanup` background scheduler with graceful shutdown in `main.rs`, update docs (`docs/auth.md`, `docs/ratelimit.md`), regenerate SQLx/OpenAPI, full `make check` + `SQLX_OFFLINE=true cargo check --all-targets` green; independent security verifier PASS.

## Log

L1. User request (2026-11-27): "sigamos con el issue #110"
L2. Route & risk assessment (2026-11-27): All four items share `src/auth/{handlers,service,repository,extract}.rs` and `tests/auth.rs`, so work runs inline in 4 sequential work-unit commits on `fix/auth-session-lifecycle`. Risk: item 2 (security: auth, session tokens, cookies, rate limiting) — independent read-only verifier required at feature end before delivery.
L3. Verification evidence (2026-11-27): `make prepare`, `make fe-schema`, `make check` (`cargo fmt`, `cargo clippy --all-targets -- -D warnings`, full `cargo test`, `cd frontend && npm run check`), and `SQLX_OFFLINE=true cargo check --all-targets` all passed on `bf55555`. Launching independent security verifier against baseline `6fb9764`.
L4. Independent security verification (2026-11-27): Fresh read-only verifier audited `6fb9764..bf55555` against S1–S4 and re-ran `SQLX_OFFLINE=true cargo check --all-targets`, `SQLX_OFFLINE=true cargo clippy --all-targets -- -D warnings`, OpenAPI diff check, `cargo test --lib auth` (22/22), and `cargo test --test auth --test ratelimit --test openapi` (46/46, 24/24, 3/3). Verdict: PASS across S1, S2, S3, S4 with 0 blockers.
