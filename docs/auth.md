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
hashes. Cost measured on this machine: ~390 ms per hash. That is intentional,
and is why hashing runs in `spawn_blocking` rather than the tokio runtime.

## Preventing user enumeration

`POST /auth/login` returns **the same 401 and the same body** for:

- nonexistent email
- incorrect password
- malformed email

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

## Login rate limiting

Implemented, with its own document: `docs/ratelimit.md`. Two layers, applied to
`POST /auth/login` only: a per-account lockout in Postgres, which survives a
restart, and a per-IP fixed window in memory, which rejects before argon2 is
paid. Both answer `429` with `Retry-After`.

## Password rotation

Two routes, both answering `204` on success:

- `POST /api/v1/auth/password` — the caller changes their own password. Body
  `{current_password, new_password}`. A wrong current password answers `401`
  with the same `invalid credentials` detail as login, so the route cannot be
  used to tell a wrong password from anything else.
- `PUT /api/v1/users/{id}/password` — an `owner` resets anyone's password. Body
  `{new_password}` only: an owner cannot know another user's password, so asking
  for it would be theatre. A `manager` gets `403`, an unknown id `404`.

Both apply `validation::validate_password` to the new password and answer `422`
with that detail when it fails. The new password is validated **before** the
current one is verified: the rule is public, so checking it first reveals nothing
and saves an argon2 round on a request that cannot succeed.

## Sessions die with the password they belonged to

A password change that leaves the other sessions alive is theatre: a stolen
cookie keeps working. Both routes revoke inside the same transaction that writes
the new hash, with one rule — every session of the target ends except the
caller's, when the caller is that user.

So changing your own password signs the other devices out and keeps this one, and
an owner resetting their own password is not logged out by it. An owner resetting
someone else ends every session of that account, including the one the target is
holding at that moment.

The statement that does it, `DELETE FROM sessions WHERE user_id = $1 AND
($2::bytea IS NULL OR token_hash <> $2)`, is the query `sessions_user_id_idx`
(`migrations/0002_auth.sql:51`) exists for. Until these routes, no statement
filtered sessions by user.

## What is missing

- **Rate limiting on `POST /auth/password`.** `POST /auth/login` is limited by
  account and by IP (`docs/ratelimit.md`); the password change is not. Someone
  holding a stolen session could use it to guess the current password. The
  per-account machinery does not transfer as it stands: it keys on the email in a
  login body, and this route already knows who the caller is.
- **Manual unlock.** None, and none is wanted: the per-account lockout is soft on
  purpose and expires on its own (`APP_LOGIN_BLOCK_MINUTES`, 5 by default). A
  hard lockout would let anyone who knows the owner's email lock them out of
  their own system by typing the wrong password. `docs/ratelimit.md` has the
  reasoning and the accepted trade-off.
- **Cleanup of `login_attempts` and `sessions`.** The functions exist
  (`purge_stale`, `purge_expired_sessions`) but nothing schedules them; see
  `docs/ratelimit.md`.
