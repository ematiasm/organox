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

## What is missing

- **Login rate limiting.** A single unlimited login endpoint enables distributed
  brute force. `tower_governor` per IP, plus per-account lockout.
- **Password change** (`POST /auth/password`) and revocation of all the user's
  sessions.
- **Unlocking**: there is no lockout for failed attempts yet.
