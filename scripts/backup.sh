#!/usr/bin/env bash
#
# Back up the deployment database to a local directory.
#
# Runs as the operator's user, not root: the systemd timer is a user timer, and
# the destination defaults to a directory in that user's home.
#
# Order matters and is load-bearing: dump, verify, rotate. Rotation runs last so
# a failed dump or a corrupt file can never delete the previous good backup.
#
# The dump runs *inside* the database container over its unix socket, so no
# password is ever passed on a command line or held in an environment variable.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${COMPOSE_FILE:-$REPO_ROOT/compose.prod.yaml}"
# compose.prod.yaml interpolates POSTGRES_PASSWORD, so `docker compose` cannot
# even parse it without the deployment `.env`. Under systemd the environment is
# empty, so the file is passed explicitly instead of relying on the shell.
COMPOSE_ENV_FILE="${COMPOSE_ENV_FILE:-$REPO_ROOT/.env}"
BACKUP_DIR="${BACKUP_DIR:-$HOME/organox-backups}"
DAILY_KEEP="${BACKUP_DAILY_KEEP:-14}"
MONTHLY_KEEP="${BACKUP_MONTHLY_KEEP:-12}"
DB_USER="${BACKUP_DB_USER:-organox}"
DB_NAME="${BACKUP_DB_NAME:-organox}"
STAMP="$(date +%F)"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

# A dump interrupted mid-write leaves a partial file. It must never be mistaken
# for a backup, so it is written under a temporary name and renamed only after
# the integrity check passes.
PARTIAL="$BACKUP_DIR/.organox-$STAMP.dump.partial"
FINAL="$BACKUP_DIR/organox-$STAMP.dump"

cleanup_partial() { rm -f "$PARTIAL"; }
trap cleanup_partial EXIT

case "$DAILY_KEEP$MONTHLY_KEEP" in
    *[!0-9]*) die "BACKUP_DAILY_KEEP and BACKUP_MONTHLY_KEEP must be numbers" ;;
esac

mkdir -p "$BACKUP_DIR"
[ -w "$BACKUP_DIR" ] || die "cannot write to BACKUP_DIR=$BACKUP_DIR"

[ -f "$COMPOSE_FILE" ] || die "compose file not found: $COMPOSE_FILE"
[ -f "$COMPOSE_ENV_FILE" ] || die "deployment env file not found: $COMPOSE_ENV_FILE"

# The database service must be up: this backs up a running deployment, not a
# stopped one. A stopped database has no data to dump beyond the volume itself.
if ! docker compose -f "$COMPOSE_FILE" --env-file "$COMPOSE_ENV_FILE" \
        ps --status running --format '{{.Service}}' | grep -qx 'db'; then
    die "the db service is not running (docker compose -f $COMPOSE_FILE --env-file $COMPOSE_ENV_FILE ps)"
fi

say "dumping $DB_NAME to $FINAL"

# `exec -T` inside the container: pg_dump reaches Postgres over the unix socket,
# where the image's POSTGRES_USER is already authenticated. `-Fc` is the custom
# format: compressed, and readable by `pg_restore --list` for verification.
# No `-f -`: pg_dump writes to stdout by default. With `-f` it writes a file
# *inside the container* named "-", so the redirect captures nothing and the
# dump silently lands in the container's working directory.
docker compose -f "$COMPOSE_FILE" --env-file "$COMPOSE_ENV_FILE" exec -T db \
    pg_dump -U "$DB_USER" -Fc --no-owner "$DB_NAME" >"$PARTIAL" \
    || die "pg_dump failed"

[ -s "$PARTIAL" ] || die "pg_dump produced an empty file"

# Integrity: a truncated or corrupt archive fails here, before it can displace a
# good backup. This is cheap and catches the failure mode that matters most.
docker compose -f "$COMPOSE_FILE" --env-file "$COMPOSE_ENV_FILE" exec -T db \
    pg_restore --list <"$PARTIAL" >/dev/null 2>&1 \
    || die "the dump is not a readable pg_restore archive: $PARTIAL"

mv "$PARTIAL" "$FINAL"
chmod 0600 "$FINAL"

SIZE="$(du -h "$FINAL" | cut -f1)"
say "backup ok: $FINAL ($SIZE)"

# --- rotation -------------------------------------------------------------
#
# Retention is computed from what is actually in the directory, never from a
# remembered list. That is what makes changing BACKUP_DIR safe: a new directory
# starts its own policy from empty instead of inheriting a stale idea of what
# should exist.
#
# Daily files are `organox-YYYY-MM-DD.dump`. Monthly ones are kept by copying
# the first-of-month dump to `organox-YYYY-MM.dump`, so the daily policy can
# expire the original without losing the monthly snapshot.

rotate() {
    local dir="$1" keep_daily="$2" keep_monthly="$3"
    local monthly_name="${STAMP:0:7}"

    # Promote today's dump to a monthly snapshot on the first of the month.
    if [ "${STAMP:8:2}" = "01" ]; then
        cp -p "$dir/organox-$STAMP.dump" "$dir/organox-$monthly_name.dump"
        say "monthly snapshot kept: organox-$monthly_name.dump"
    fi

    # Daily: only files matching exactly one date. `organox-YYYY-MM.dump` cannot
    # match, so a monthly snapshot is never counted or removed as a daily.
    mapfile -t dailies < <(
        find "$dir" -maxdepth 1 -type f -regextype posix-extended \
            -regex '.*/organox-[0-9]{4}-[0-9]{2}-[0-9]{2}\.dump' -printf '%f\n' \
            | sort -r
    )
    local i=0
    for f in "${dailies[@]}"; do
        i=$((i + 1))
        if [ "$i" -gt "$keep_daily" ]; then
            rm -f "$dir/$f"
            say "pruned daily: $f"
        fi
    done

    mapfile -t monthlies < <(
        find "$dir" -maxdepth 1 -type f -regextype posix-extended \
            -regex '.*/organox-[0-9]{4}-[0-9]{2}\.dump' -printf '%f\n' | sort -r
    )
    local j=0
    for f in "${monthlies[@]}"; do
        j=$((j + 1))
        if [ "$j" -gt "$keep_monthly" ]; then
            rm -f "$dir/$f"
            say "pruned monthly: $f"
        fi
    done
}

rotate "$BACKUP_DIR" "$DAILY_KEEP" "$MONTHLY_KEEP"

say "kept: $(find "$BACKUP_DIR" -maxdepth 1 -name 'organox-*.dump' | wc -l) file(s)"
