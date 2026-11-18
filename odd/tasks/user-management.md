# User management: the users screen and credential rotation

The Users screen renders "the user list is not available yet" and the only user route is `POST /api/v1/users`. `docs/auth.md:132` and `:136` record the rest: a password can be set once by `create-user` and never rotated, and no statement filters `sessions` by `user_id`, so `sessions_user_id_idx` serves nothing. This feature closes all three in four slices.

## Specs

**Users list**

- S1. `GET /api/v1/users` returns every user as `{id, email, role}`, ordered by email.
- S2. Authorization is the existing rule and nothing else: `Role::can_manage_users` (`src/auth/model.rs:65`), i.e. `owner` only. A `manager` gets `403`, an anonymous caller `401`, and neither leaves a trace.
- S3. The Users screen shows the users in a table (email, role) with the count, instead of the "not available yet" notice. The owner-only gate and the sidebar entry stay as they are.

**Password rotation**

- S4. A user changes their own password with `POST /api/v1/auth/password`, sending `{current_password, new_password}`. Any authenticated user may call it. A wrong current password is `401` with the existing `invalid credentials` detail and changes nothing.
- S5. A successful self change destroys every other session of that user and keeps the session that performed it: the other devices are logged out, this one is not.
- S6. The owner resets any user's password with `PUT /api/v1/users/{id}/password`, sending `{new_password}` and no current password — the owner cannot know it. A `manager` gets `403`; an unknown id `404`.
- S7. An owner reset destroys every session of the target user. When the target is the caller, the caller's session survives, so resetting your own password does not log you out.
- S8. Both routes apply `validation::validate_password` (min 12 characters, forbidden list) and answer `422` with the existing detail, leaving password, sessions and row count unchanged.
- S9. Neither route hashes or writes when a check fails, and the new hash is computed off the runtime thread, as `create_user` does.

**Docs and proof**

- S10. `docs/auth.md` stops listing password change and session revocation as missing, and `sessions_user_id_idx` gains the query it was created for.
- S11. `frontend/e2e/users.cjs` proves in a browser: the list renders; an owner reset logs the target out; a self change keeps the current session while killing another.

## Tasks

| ID | Task | Specs | Route | Commit |
|---|---|---|---|---|
| T1 | `GET /api/v1/users` (owner), OpenAPI stub, expected-route list, contract test | S1, S2 | writer | a18f43a |
| T2 | Users screen: table with email and role, count, API client method | S3 | writer | a18f43a |
| T3 | Repository: delete sessions by user, optionally keeping one token; `.sqlx` cache | S5, S7, S10 | writer | 3153f34 |
| T4 | `POST /api/v1/auth/password`: current password check, revoke the others, `204` | S4, S5, S8, S9 | writer | 3153f34 |
| T5 | `PUT /api/v1/users/{id}/password`: owner reset, revoke the target's sessions | S6, S7, S8, S9 | writer | 3153f34 |
| T6 | Frontend: owner reset from the users table, self change from the account menu | S3, S4, S6 | writer | |
| T7 | Docs: `docs/auth.md` gaps, README index, this log | S10 | inline | |
| T8 | `frontend/e2e/users.cjs`, full check and close | S11 | writer | |

Slices, each its own PR so review stays small: T1+T2 · T3+T4+T5 · T6 · T7+T8.

## Log

L5. Slice 2 (T3, T4, T5) is done, commit `3153f34`: the revocation statement, `POST /auth/password` and `PUT /users/{id}/password`, with eight integration tests. RED was observed first: all seven route tests failed with `404` before the routes existed, and the unknown-id test was then strengthened, because an unregistered path also answers `404` — a test that would have passed for the wrong reason. One honest residual: if a target user is deleted between the existence check and the transaction, a hash is computed and discarded; no write happens.

L4. Slice 1 (T1, T2) is done, commit `a18f43a`: `GET /api/v1/users` owner-only, three integration tests (401 with unchanged counts, 403 with unchanged counts, owner list ordered by email) and the screen reading it. RED was observed first: the three tests failed with `405` before the route existed. Not done in this slice, on purpose: no password route, no session statement.

L1. User request, verbatim: "segui con el 2" — the second item of the backlog offered after the CodeQL work: "Pantalla de usuarios + rotación de contraseña".

L2. Scope, selected by the user in one questionnaire, verbatim: "Lista + rotación + revocación (recomendado)" with the preview "Tres slices: GET /users + pantalla real, rotación de contraseña con revocación de sesiones, y el probe e2e." Rotation rule, also selected: "Autocambio + reset del owner (recomendado)", preview: "Autocambio — cualquier autenticado, exige la contraseña actual. Reset — solo `owner`, no exige la actual. Revocación: al autocambiar se matan todas las sesiones menos la que ejecuta el cambio; al resetear, todas las del usuario afectado." The specs above are the English specification of those choices; the choices are the requirement.

L3. Decisions taken while planning, from the exploration of the tree (`src/auth/handlers.rs:24-41`, `model.rs:65`, `extract.rs:44-51`, `migrations/0002_auth.sql:35-51`, `docs/auth.md:130-150`):
- Revocation is `DELETE FROM sessions WHERE user_id = $1`, not a new `password_changed_at` column checked by the extractor. The index for it already exists and is currently unused; a timestamp column would add a migration and a check on every request.
- One uniform revocation rule instead of two: every session of the target user dies except the caller's, when the caller is that user. It makes S5 and S7 the same statement with the same parameter.
- No `DELETE`/deactivate for users, no password history, no forced rotation on first login: none was asked for, and each is its own decision.
- A new route must also be added to the expected-route list in `src/openapi.rs:199-206`, or `every_documented_route_is_registered` fails; `make fe-schema` regenerates `openapi.json` and `schema.d.ts` and both are committed.
