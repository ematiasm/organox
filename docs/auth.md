# Authentication and authorization

## Model

Single-tenant: there is no `tenant_id`. One deployment = one business.

| Concept | Implementation |
|---|---|
| Identity | `users.email` (`citext`, unique). Case-insensitive login. |
| Password | argon2id, PHC hash. Never plaintext, never `bcrypt`. |
| Session | Row in `sessions`, key = SHA-256 of the token. |
| Token | 32 bytes from a CSPRNG, base64url. Sent in an `HttpOnly` cookie. |
| Actor | `AuthUser` extractor. The only way to identify who makes the request. |
| Authorization | `Role` + `Role::can_manage_users()`. Single source of truth. |

## Why sessions instead of JWT

A web admin needs **real logout** and **immediate revocation**. With JWT, logout
is a suggestion to the client and revocation requires a blacklist in the DB—in
other words, bringing back the state it avoids. Here logout deletes the row and
the token stops working on the next request.

## Why store only the token's SHA-256 in the DB

The token already has 256 bits of entropy from a CSPRNG: there is no dictionary
to attack. Storing the hash means reading the `sessions` table reveals that
sessions exist, not how to use them. SHA-256 (not argon2) is used because it is
calculated on every authenticated request and the token already has entropy.

## Why argon2id instead of bcrypt

argon2id is the current standard (recommended by OWASP), and its cost parameters
are in the PHC string itself: increasing the cost does not invalidate old
hashes. Cost measured on this machine: ~390 ms and ~19 MiB per hash. That is
intentional, and is why hashing runs in `spawn_blocking` rather than the tokio
runtime.

To prevent authentication traffic from turning that cost into a memory or CPU
denial of service:

- Auth and user-management routes enforce a tighter HTTP request body cap
  (`AUTH_BODY_LIMIT_BYTES`, 16 KiB) before JSON deserialization, rejecting
  larger payloads with `413 Payload Too Large` (`application/problem+json`).
- Passwords exceeding `MAX_PASSWORD_BYTES` (1024 bytes) are rejected before
  reaching Argon2 on every verification path (`login`, `change_password`, and
  `password::verify` itself).
- Concurrent Argon2 hashing and verification tasks are capped per process by a
  Tokio semaphore (`MAX_CONCURRENT_HASHES`). Each task moves its owned permit
  into `spawn_blocking`, so cancelling a caller's request future cannot free a
  slot while hashing is still running.

## Preventing user enumeration

`POST /auth/login` returns **the same 401 and the same body** for:

- nonexistent email
- incorrect password
- malformed email
- password exceeding `MAX_PASSWORD_BYTES`

In addition, when the email does not exist, the password is checked against a
dummy hash of equivalent cost, precomputed at server startup. Without this
warm-up, the first failed attempt paid the cost of generating the dummy hash,
and the latency exposed it as an oracle (measured: 773 ms vs 391 ms).

## Cookies

```
Set-Cookie: session=<token>; HttpOnly; Secure; SameSite=Strict; Path=/; Expires=...
```

- `HttpOnly`: JavaScript cannot read it (mitigates session theft via XSS).
- `SameSite=Strict`: it is not sent on requests originating from another site.
  This stops CSRF without a CSRF token, provided the frontend is served from the
  same origin.
- `Secure`: required in production. `APP_COOKIE_SECURE=false` is only for
  development over `http://localhost`.

**If the frontend is on another origin** (`app.pyme.com` + `api.pyme.com`),
`SameSite=Strict` no longer works: switch to `Lax` with a common parent domain,
or to `None` + an explicit CSRF token. This depends on the deployment decision.

## Session lifetime

- Sliding: every authenticated request extends it by 7 days.
- Absolute cap: 30 days from creation, even if used every day. Without this cap,
  a stolen cookie is valid indefinitely.

## Roles

Today: `owner` (everything) and `manager` (everything except managing users).

**There are no `roles`/`permissions` tables.** With two roles and no permission
that crosses roles, a permissions table is unused scaffolding. The rule lives in
one place:

```rust
impl Role {
    pub fn can_manage_users(self) -> bool {
        matches!(self, Role::Owner)
    }
}
```

When a permission crosses roles (for example, "warehouse staff enter stock but
cannot see prices"), the change is:

1. A new migration with `permissions` and `role_permissions`.
2. Change **the body** of the `can_*` functions in `src/auth/model.rs`.

Handlers do not change, because they already call
`user.require_user_management()` rather than inspecting the role manually.

## Creating the first user

`create-user` is the only creation mechanism outside the API. **There is no
automatic seed at startup**, deliberately:

| Seed via `.env` at boot | `create-user` |
|---|---|
| Password remains plaintext in `.env` forever | Does not persist plaintext |
| Race condition with 2 replicas (SELECT+INSERT) | No |
| Resurrects deleted users | No |
| Overwrites changed passwords | No |
| Works for users 2..N | Yes |

```bash
# interactive (prompts without echo)
cargo run --bin create-user -- --email owner@pyme.com --role owner

# in Docker / non-interactive
printf '%s' "$ADMIN_PW" | docker compose exec -T app create-user \
  --email owner@pyme.com --role owner --password-stdin
```

The password is never passed as an argument: it would be visible in `ps aux` to
any system user.

### A fixed account for development

Running `create-user` by hand every session is friction with no safety value on a
local database. `make dev-user` creates the account once and, on later runs,
**resets its password** and revokes its sessions:

```bash
make dev-user    # reads the variables below from .env, like every other target
```

| Variable | Default | Notes |
|---|---|---|
| `DEV_PASSWORD` | none, required | The only accepted home for a literal; the file is gitignored. The same 12-character minimum the tool always enforced applies. Avoid `#` and `$`: that file is parsed as make syntax before the tool sees it, so `#` starts a comment and `$` expands |
| `DEV_EMAIL` | `owner@pyme.com` | Not a secret |
| `DEV_ROLE` | `owner` | Applied on creation only; a reset keeps the existing role |

A reset does not clear a failed-login lockout (`docs/ratelimit.md`): if the
account is locked, the next login is still rejected until the lock expires. A
rejected login right after `make dev-user` is that lock at work, not a failed
reset.

This does **not** reopen the decision in the table above. Nothing here runs at
startup: `make dev-user` is a command a person types, so it cannot resurrect a
deleted user between deploys or overwrite a password nobody asked it to touch.

What protects the value is that no `$(DEV_PASSWORD)` exists anywhere in the
target: make never expands it into the recipe text, so the text it hands to the
shell — and the dry run `make -n` prints, safe to paste into a log — carries
only the variable name. The shell expands `$$DEV_PASSWORD` itself and pipes the
value to the tool as data on stdin. That distinction is the whole point of the
`$$` in the target: make expands `$(VAR)` into the text it hands to the shell as
its `-c` argument, which would put the plaintext password in `ps` for the whole
pipeline — including the seconds argon2 spends hashing.

One residual, stated precisely: the recipe does pass the value to `printf` as
an argument. `printf` is a builtin in the shells make uses here (`/bin/sh`), so
normally no process `argv` ever carries the value — but that is a property of
the shell, not a guarantee of this design, and an external `printf` would expose
the value in that process's command line for as long as the pipe takes. The
environment, by contrast, is readable by its own user and by root, not by every
local user the way a command line is.

Never point it at a deployment database: the target reads `DATABASE_URL` from the
same file, so the two travel together and the mistake would be a conscious one.

## Login and password-change rate limiting

Implemented, with its own document: `docs/ratelimit.md`. Two layers, applied to
`POST /auth/login` and `POST /auth/password`: a per-account lockout in Postgres,
which survives a restart, and a per-IP fixed window in memory, which rejects
before argon2 is paid. Both answer `429` with `Retry-After`.

## Password rotation

Two routes, both answering `204` on success:

- `POST /api/v1/auth/password` — the caller changes their own password. Body
  `{current_password, new_password}`. A wrong current password answers `401`
  with the same `invalid credentials` detail as login, so the route cannot be
  used to tell a wrong password from anything else. Rate-limited by IP and by
  account (`429` + `Retry-After`).
- `PUT /api/v1/users/{id}/password` — an `owner` resets anyone's password. Body
  `{new_password}` only: an owner cannot know another user's password, so asking
  for it would be theatre. A `manager` gets `403`, an unknown id `404`.

Both apply `validation::validate_password` to the new password and answer `422`
with that detail when it fails. The new password is validated **before** the
current one is verified: the rule is public, so checking it first reveals nothing
and saves a rate-limit slot and an argon2 round on a request that cannot succeed.

## Sessions die with the password they belonged to

A password change that leaves existing sessions alive is theatre: a stolen
cookie keeps working. Both routes revoke **every** session of the target inside
the same transaction that writes the new hash. When the caller changes or resets
their own password, a fresh session token is created in that same transaction
and delivered via `Set-Cookie`, so the caller's old token is revoked atomically
instead of preserved.

An owner resetting someone else ends every session of that account, including
the one the target is holding at that moment, without rotating the owner's own
cookie.

## Sliding session expiry and cleanup

Authenticated requests extend `sessions.expires_at` up to the 30-day absolute
cap (`SESSION_MAX_LIFETIME`) whenever sliding forward advances the deadline by
at least `SESSION_REFRESH_INTERVAL` (1 hour) or reaches the cap, and re-emit
the `session` cookie with the matching `Expires` timestamp so the browser stays
in sync with Postgres without churning `Set-Cookie` on every request.

A background cleanup task started in `src/main.rs` (`auth::cleanup`) runs at
startup and every hour to purge expired `sessions` and stale `login_attempts`
(preserving active locks), shutting down cleanly on `SIGINT`/`SIGTERM`.

## What is missing

- **Manual unlock.** None, and none is wanted: the per-account lockout is soft on
  purpose and expires on its own (`APP_LOGIN_BLOCK_MINUTES`, 5 by default). A
  hard lockout would let anyone who knows the owner's email lock them out of
  their own system by typing the wrong password. `docs/ratelimit.md` has the
  reasoning and the accepted trade-off.
