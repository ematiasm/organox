#!/usr/bin/env bash
#
# Install the backup timer for the current user.
#
# The timer runs as a **user** unit, not a system one. A system unit runs as
# root, which would need group membership to reach the Docker socket, and
# reaching the Docker socket is equivalent to unlimited root on the host. Keeping
# the timer in the user session avoids widening that.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
UNIT_NAME="organox-backup"
BACKUP_DIR="${BACKUP_DIR:-$HOME/organox-backups}"
ON_CALENDAR="${BACKUP_ON_CALENDAR:-*-*-* 03:00:00}"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Linux" ] || die "this installer only handles Linux (systemd); run scripts/backup.sh by hand elsewhere"
[ -d /run/systemd/system ] || die "systemd is not running here; run scripts/backup.sh by hand"

mkdir -p "$UNIT_DIR" "$BACKUP_DIR"

# The unit carries the resolved paths instead of relying on the environment:
# systemd starts the unit with an almost empty environment, so `$HOME` and any
# variable exported in the shell are not there.
cat >"$UNIT_DIR/$UNIT_NAME.service" <<EOF
[Unit]
Description=Back up the organox database
# A backup that starts while the machine is waking up is still worth running;
# it will fail cleanly if the database container is not ready.
After=docker.service

[Service]
Type=oneshot
WorkingDirectory=$REPO_ROOT
Environment=BACKUP_DIR=$BACKUP_DIR
Environment=BACKUP_DAILY_KEEP=${BACKUP_DAILY_KEEP:-14}
Environment=BACKUP_MONTHLY_KEEP=${BACKUP_MONTHLY_KEEP:-12}
ExecStart=$REPO_ROOT/scripts/backup.sh
# Journal, not silence: journalctl --user -u $UNIT_NAME is how an operator finds
# out whether last night worked.
StandardOutput=journal
StandardError=journal
EOF

cat >"$UNIT_DIR/$UNIT_NAME.timer" <<EOF
[Unit]
Description=Daily organox database backup

[Timer]
OnCalendar=$ON_CALENDAR
# Run on the next boot after a missed window: a laptop that was asleep at 03:00
# still gets backed up. Without this, a machine that is off at night is never
# backed up at all.
Persistent=true
Unit=$UNIT_NAME.service

[Install]
WantedBy=timers.target
EOF

systemctl --user daemon-reload
systemctl --user enable --now "$UNIT_NAME.timer"

# Without lingering, user units stop when the last session closes: the timer
# would only fire while a terminal is open, which is exactly the moment a
# background backup is least needed.
if command -v loginctl >/dev/null 2>&1; then
    if ! loginctl show-user "$(id -un)" --property=Linger 2>/dev/null | grep -q 'Linger=yes'; then
        say "enabling lingering so the timer runs without an open session (needs sudo)"
        sudo loginctl enable-linger "$(id -un)" \
            || die "could not enable lingering; run: sudo loginctl enable-linger $(id -un)"
    fi
fi

say "installed $UNIT_DIR/$UNIT_NAME.{service,timer}"
say "next run: $(systemctl --user list-timers "$UNIT_NAME.timer" --no-pager | sed -n '2p')"
say "logs: journalctl --user -u $UNIT_NAME"
