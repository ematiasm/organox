# Login rate limiting

## What it is and what it is not

**It does not prohibit abuse: it bounds it.** It does not stop someone from trying
to guess a password; it makes attempts slow, finite, and observable.

The distinction is not semantic, but a design choice. Any system that promises
to *prohibit* abuse ends up in one of two places:

1. **It lies**: the attacker still tries, only more slowly.
2. **It denies service to legitimate users**: this is what happens with a hard,
   permanent lockout.

The real goal is different: **make the attack economically unviable and leave a
trace**. 5 attempts every 5 minutes are 1440 per day. Against a space of ~10¹²
reasonable combinations, that is irrelevant. And each attempt is logged.

## Three threats, three defenses

| Threat | What it is | Defense |
|---|---|---|
| **Vertical brute force** | Many IPs against **one** account (the owner) | Per-account limit, in Postgres |
| **Horizontal brute force** | One IP against **many** accounts with common passwords | Per-IP limit, in memory |
| **CPU amplification** | Flooding `/auth/login` knowing each request costs 400 ms of argon2 | Per-IP limit + block **before** hashing |

The first two layers are both necessary and do not replace each other:

- **IP only**: a botnet with 1000 IPs tries 1000 passwords per minute without
  triggering any limit.
- **Account only**: the attacker tries 1 password on each of 100,000 accounts.
  No account is locked, and common passwords still fall.

## Layer 1 — Per account (Postgres)

Table `login_attempts`, one row per normalized email.

**Why Postgres and not memory**: the lockout must survive a restart. If
restarting the process clears the counter, the attacker only needs to wait for
a deployment—or crash the process themselves.

**Why nonexistent emails are counted**: if only real users counted, the
difference between "429 locked" and "401 invalid credentials" would allow
**enumeration of registered emails**. It is the same oracle closed by the dummy
hash, through another route.

### Exact semantics

With `APP_LOGIN_MAX_FAILURES=3`:

| Attempt | Password | Response |
|---|---|---|
| 1 | incorrect | `401` |
| 2 | incorrect | `401` |
| 3 | incorrect | `401` ← **triggers the lockout, but tells the truth** |
| 4 | any | `429` + `Retry-After` ← not even evaluated |

The third attempt returns **401, not 429**, because its password *was* actually
incorrect. Telling the user "too many attempts" on the same attempt that locked
them out is lying. The lockout applies from the next attempt.

This was discovered by a failing test (`account_is_locked_after_max_failures`):
the code returned 429 on the third attempt, and the documented semantics were
different. The code was fixed, not the test.

### Why the lockout is soft

It lasts `APP_LOGIN_BLOCK_MINUTES` (5 by default) and expires on its own.

A hard lockout (until manual intervention) turns this defense into a **denial
of service vector**: anyone who knows the owner's email can lock them out of
their own system by entering the wrong password. In an SMB administrative
system, that is worse than the attack it is meant to prevent.

Accepted trade-off: an attacker can wait 5 minutes and try 5 passwords again.
The limit does not stop the attack; it makes it **irrelevant in volume**.

### A successful login clears the counter

Without this, someone who makes 3 mistakes and then remembers their password
would still be locked out. A successful login is sufficient proof that it is not
an attacker.

### Window and cleanup

Attempts are counted within a 15-minute window: outside it, the counter starts
over. `AccountLimiter::purge_stale` deletes rows with no activity (not yet
hooked up to a job; see "What is missing").

## Layer 2 — Per IP (memory)

`IpLimiter`, fixed window, `APP_LOGIN_IP_MAX_PER_MINUTE` + a burst.

**Why memory**: it is the barrier that blocks **before** touching the database
and before spending time on a hash. An attacker making 10,000 requests per
second must not cause 10,000 queries or 10,000 argon2 hashes: that would already
be a successful attack without guessing a single password. Verified: once
blocked, the response arrives in **0 ms** instead of 400 ms.

**Accepted cost**: state lives in the process.
- It is not shared between replicas: with N replicas the effective limit is N×.
- It is lost on restart.

This is acceptable because this layer is a **first barrier**, not the primary
defense. The per-account layer is the one that survives restarts and replicas.
If a distributed IP limit is ever needed, it can move to Redis without changing
the `IpLimiter` interface.

### `X-Forwarded-For` is ignored, deliberately

The middleware reads the IP from `ConnectInfo` (the TCP connection), **not**
from the `X-Forwarded-For` header.

The client controls that header. Honoring it without a trusted proxy in front
lets an attacker choose a different IP on every request and **completely bypass
the limit**. This is the most common way to break this defense without noticing.

If a reverse proxy is added someday (nginx, Caddy, a load balancer):

1. configure the proxy to **overwrite** `X-Forwarded-For` (not append it), and
2. add a trusted proxy list to the app, and only then read the header from them.

## `Retry-After` makes 429 actionable

Every 429 response includes `Retry-After` in seconds. Without it, the client
does not know whether to wait 2 seconds or 2 days, and its default option is to
hammer the server—exactly what the limit is intended to prevent.

## Configuration

| Variable | Default | What it controls |
|---|---|---|
| `APP_LOGIN_MAX_FAILURES` | `5` | Failed attempts per account before lockout |
| `APP_LOGIN_BLOCK_MINUTES` | `5` | Lockout duration |
| `APP_LOGIN_IP_MAX_PER_MINUTE` | `30` | Logins per minute per IP (+ burst of 10) |

An invalid value **prevents startup**, naming the exact variable:

```
fatal error: configuration error: APP_LOGIN_MAX_FAILURES must be at least 1
fatal error: configuration error: APP_LOGIN_MAX_FAILURES has an invalid value ("five"): invalid digit found in string
```

## What it does NOT do

- **It does not protect against leaked credentials.** If `owner@pyme.com`
  appears in another company's dump and uses the same password, the attacker
  gets in on the first attempt. This is addressed by 2FA and checks against
  leaked password lists.
- **It does not protect against phishing.** The owner gives their password to a
  fake website; the limit does not trigger because there are no failed attempts.
  Again, 2FA.
- **It does not distribute state between replicas** (see above).
- **It does not permanently block IPs**, nor should it: an IP blacklist is a
  losing game against shared NAT and mobile mobility.

## What is missing

- **Cleanup job** for expired `login_attempts` and `sessions`. The functions
  exist (`purge_stale`, `purge_expired_sessions`) but there is no scheduler. One
  query per hour at startup is enough for an SMB.
- **2FA (TOTP)** for `owner`. This defense closes the leaked-credentials gap
  that rate limiting does not address.
- **Notification** to the owner when their account is repeatedly locked: the
  log already records it (`WARN login blocked by failed attempts`), but the
  notification is missing.
- **Compromised password list** (Have I Been Pwned, k-anonymity).
