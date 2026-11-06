# syntax=docker/dockerfile:1
#
# One image, one origin: the Rust binary serves the API and the SvelteKit SPA.
# Tests are not run here. `make check` is the local and CI gate; this file only
# packages a build that already passed it.

# ---- 1. Frontend: the static SPA ----
FROM node:26-alpine AS frontend
WORKDIR /app/frontend

# The lockfile is copied alone so dependencies cache independently of source
# edits. `npm ci` is used instead of `npm install`: it installs exactly what the
# lockfile pins and fails if package.json and the lockfile disagree.
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci

COPY frontend/ ./
RUN npm run build

# ---- 2. Backend: the release binaries ----
FROM rust:1-bookworm AS backend
WORKDIR /app

# The `sqlx::query!` macros are checked against the committed `.sqlx/` cache
# instead of a live database. Without this the build would need a running
# Postgres, which the image does not have.
ENV SQLX_OFFLINE=true

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY .sqlx ./.sqlx
COPY migrations ./migrations
COPY src ./src

# `--locked` fails instead of silently resolving versions the lockfile does not
# hold, so the image matches the Cargo.lock under review.
RUN cargo build --release --locked --bin organox --bin create-user --bin migrate

# ---- 3. Runtime ----
FROM debian:bookworm-slim AS runtime

# `ca-certificates` for outbound TLS, `curl` for the compose healthcheck. The
# process runs unprivileged: a server reachable from the LAN has no reason to be
# root inside its container.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /app --shell /usr/sbin/nologin organox

WORKDIR /app

COPY --from=backend /app/target/release/organox /usr/local/bin/organox
# Shipped so the first owner can be created inside the running container. There
# is no seeding: a password in the environment would live there forever.
COPY --from=backend /app/target/release/create-user /usr/local/bin/create-user
# One-shot migration step, run by compose before the server starts.
COPY --from=backend /app/target/release/migrate /usr/local/bin/migrate

# `FRONTEND_DIR` in src/routes.rs is relative, so this path is the contract: the
# server looks for `frontend/build` under its working directory.
COPY --from=frontend /app/frontend/build ./frontend/build

USER organox
ENV APP_BIND_ADDR=0.0.0.0:3000
EXPOSE 3000

ENTRYPOINT ["organox"]
