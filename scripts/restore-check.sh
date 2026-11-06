#!/usr/bin/env bash
#
# Prove that a backup can actually be restored.
#
# An archive nobody restored is a hope, not a backup. This restores the most
# recent dump into a throwaway database, asks real questions of the data, and
# drops the database whether the checks pass or not.
#
# It never touches the live database: the target name is derived here and the
# script refuses to run against it.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${COMPOSE_FILE:-$REPO_ROOT/compose.prod.yaml}"
COMPOSE_ENV_FILE="${COMPOSE_ENV_FILE:-$REPO_ROOT/.env}"
BACKUP_DIR="${BACKUP_DIR:-$HOME/organox-backups}"
DB_USER="${BACKUP_DB_USER:-organox}"
DB_NAME="${BACKUP_DB_NAME:-organox}"
SCRATCH_DB="organox_restore_check"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

ARCHIVE="${1:-}"
if [ -z "$ARCHIVE" ]; then
    ARCHIVE="$(find "$BACKUP_DIR" -maxdepth 1 -type f -name 'organox-*.dump' \
        | sort -r | head -1)"
fi
[ -n "$ARCHIVE" ] || die "no backup found in $BACKUP_DIR; run scripts/backup.sh first"
[ -f "$ARCHIVE" ] || die "backup not found: $ARCHIVE"

# Guard rail: the scratch database is created and dropped here. Refusing to
# proceed on a name collision means a mistake in configuration cannot delete the
# live database.
[ "$SCRATCH_DB" != "$DB_NAME" ] || die "refusing to use the live database name"

# `-c` and its SQL are separate arguments to psql. Building the flag and the
# query from one expanded string would pass "-c <sql>" as a single argument,
# which psql reads as a database name.
psql_db() {
    local db="$1"
    shift
    docker compose -f "$COMPOSE_FILE" --env-file "$COMPOSE_ENV_FILE" exec -T db \
        psql -U "$DB_USER" -d "$db" -v ON_ERROR_STOP=1 -tA -c "$1"
}

# Drop first, so an interrupted previous run cannot make this one fail on an
# existing database and look like a broken backup.
psql_db postgres "drop database if exists \"$SCRATCH_DB\"" >/dev/null
psql_db postgres "create database \"$SCRATCH_DB\"" >/dev/null

cleanup() { psql_db postgres "drop database if exists \"$SCRATCH_DB\"" >/dev/null 2>&1 || true; }
trap cleanup EXIT

say "restoring $ARCHIVE into $SCRATCH_DB"
docker compose -f "$COMPOSE_FILE" --env-file "$COMPOSE_ENV_FILE" exec -T db \
    pg_restore -U "$DB_USER" -d "$SCRATCH_DB" --no-owner --exit-on-error \
    <"$ARCHIVE" \
    || die "restore failed: the archive cannot rebuild the database"

# Control counts against the live database, table by table. A restore that
# "succeeded" but produced empty tables is the failure this exists to catch.
TABLES=(users products stock_movements sales sale_items)
FAILED=0
for table in "${TABLES[@]}"; do
    live="$(psql_db "$DB_NAME" "select count(*) from $table")"
    restored="$(psql_db "$SCRATCH_DB" "select count(*) from $table")"
    if [ "$live" = "$restored" ]; then
        say "  $table: $restored row(s) ok"
    else
        printf '  %s: live=%s restored=%s MISMATCH\n' "$table" "$live" "$restored" >&2
        FAILED=1
    fi
done

[ "$FAILED" -eq 0 ] || die "restored data does not match the live database"

# The business invariant, checked on the restored copy rather than trusted. The
# CASE mirrors the one in src/catalog/repository.rs, so this fails if the archive
# captured a state the application could not have produced: a product whose
# movements sum to negative stock. If that expression ever changes in the source,
# this test starts checking the wrong thing — the two must move together.
negative="$(psql_db "$SCRATCH_DB" "
    select count(*) from (
        select product_id, sum(case reason
            when 'purchase'   then quantity
            when 'adjustment' then quantity
            when 'sale'       then -quantity
            when 'loss'       then -quantity
        end) as qty
        from stock_movements group by product_id
    ) s where qty < 0")"
[ "$negative" = "0" ] || die "$negative product(s) would have negative stock after restore"

say "restore check ok: $ARCHIVE"
