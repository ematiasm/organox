# Backup and restore

The deployment's data is the business: stock, sales and cash records that cannot
be rebuilt. This document says what is backed up, what is **not**, and how to
prove a backup is real.

Backups run on the **host**, not inside the application. The application has no
knowledge of them and no ability to read them.

## What this covers

| Failure | Covered |
|---|---|
| `docker compose down -v` deletes the volume | yes |
| A migration corrupts or drops data | yes, up to the last backup |
| Someone deletes a product or a sale from the UI | yes, up to the last backup |
| The database container is rebuilt from scratch | yes |

## What this does NOT cover

**A dead disk, a stolen machine, or a fire.** The backup lives in
`$HOME/organox-backups/`, on the same machine. If that machine is gone, the
backups are gone with it.

An offsite copy is the next slice and is not implemented. Until it exists, this
is a protection against mistakes, not against disaster. Say it out loud before
trusting it with data that matters.

## What is backed up

One `pg_dump` of the `organox` database, in the **custom format** (`-Fc`):

- compressed, roughly a third of the plain SQL size
- verifiable without restoring (`pg_restore --list`)
- restorable in parts, if one table is ever needed

Not backed up: the Postgres roles and cluster globals. The image recreates the
`organox` role from `POSTGRES_USER`, so a restore into a fresh deployment works
as long as the new deployment uses the same `POSTGRES_USER`.

## How it works

`scripts/backup.sh`:

1. Checks that the `db` service is running and the destination is writable.
2. Runs `pg_dump` **inside the database container**, over its unix socket. No
   password is passed on a command line or held in an environment variable: the
   image's `POSTGRES_USER` is already authenticated there.
3. Writes to a `.partial` file and verifies it with `pg_restore --list`.
4. Renames it to `organox-YYYY-MM-DD.dump` only after verification passes.
5. Rotates: keeps the last 14 daily files and the last 12 monthly ones.

The order is load-bearing. **Rotation runs last**, so a failed dump or a corrupt
archive can never delete the previous good backup. A `.partial` file left behind
by an interruption is never mistaken for a backup.

### Retention

- **Daily**: `organox-YYYY-MM-DD.dump`, last 14.
- **Monthly**: `organox-YYYY-MM.dump`, last 12. Created by copying the dump from
  the first of each month, so the daily policy can expire the original.

Retention is computed from what is **in the directory**, never from a remembered
list. That is what makes changing the destination safe.

## Restoring

Into a fresh deployment:

```bash
# 1. Deploy as usual, and let the migrations run.
docker compose -f compose.prod.yaml up -d

# 2. Stop the app so nothing writes during the restore.
docker compose -f compose.prod.yaml stop app

# 3. Restore over the empty database.
docker compose -f compose.prod.yaml exec -T db \
  pg_restore -U organox -d organox --no-owner --clean --if-exists \
  < ~/organox-backups/organox-2026-11-05.dump

# 4. Start the app again.
docker compose -f compose.prod.yaml start app
```

`--clean --if-exists` drops the existing objects first, which is what makes this
work over a database the migrations already created.

After restoring, **log in and look at the data**. A restore that "succeeded" and
an empty screen is the failure this document exists to prevent.

## Proving a backup is real

An archive nobody restored is a hope. `scripts/restore-check.sh` restores the
most recent dump into a throwaway database, compares row counts table by table
against the live database, checks the stock invariant on the restored copy, and
drops the throwaway database — whether the checks pass or fail.

```bash
make backup-check                    # the most recent dump
scripts/restore-check.sh path/to.dump   # a specific one
```

It refuses to run if the throwaway database name collides with the live one.

**Run it before you need it.** A backup is proven by a successful restore, not by
a successful dump.

## Scheduling

`make backup-install` writes a systemd **user** timer (Linux only) and enables
lingering so it runs without an open session.

Why a user timer and not a system unit: a system unit runs as root, and to reach
the Docker socket root would need `docker` group membership — which is
unrestricted root on the host. The user timer runs as you, with the access you
already have.

```
OnCalendar=*-*-* 03:00:00
Persistent=true
```

`Persistent=true` recovers a missed run: a machine that was off at 03:00 is
backed up when it comes back.

```bash
systemctl --user list-timers organox-backup.timer
journalctl --user -u organox-backup        # every run, and why it failed
systemctl --user start organox-backup      # run one now
```

A failed run is visible in the journal and exits non-zero. It is not silent.

## Changing the destination

`BACKUP_DIR` is read from the environment or the deployment `.env`. Change it and
reload the timer:

```bash
# 1. Edit BACKUP_DIR in .env, then reinstall the timer so the unit carries it.
make backup-install

# 2. Move the existing dumps, or the new directory starts a fresh policy and the
#    old files are never rotated again.
mv ~/organox-backups/*.dump /mnt/disco2/organox/

# 3. Prove the new location.
make backup && make backup-check
```

Step 2 is manual on purpose: deleting or moving historical backups is a decision
only the operator can make.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `BACKUP_DIR` | `$HOME/organox-backups` | Destination on the host |
| `BACKUP_DAILY_KEEP` | `14` | Daily files retained |
| `BACKUP_MONTHLY_KEEP` | `12` | Monthly files retained |
| `BACKUP_ON_CALENDAR` | `*-*-* 03:00:00` | systemd timer schedule |
| `COMPOSE_ENV_FILE` | `.env` beside compose | Where compose reads `POSTGRES_PASSWORD` |

These live in the deployment `.env`, next to `POSTGRES_PASSWORD`, and **not** in
the application's `/settings`. Backup configuration cannot live in the database
it protects: `down -v` would delete the record of how the data was being saved.

A future `/settings` screen will **show** backup status (last run, size, last
verified restore). It will not configure it.
