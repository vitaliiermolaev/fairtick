#!/usr/bin/env bash
# Periodic crash-consistent SQLite backup for the live fairtick DB.
#
# The deploy already takes a PRE-DEPLOY backup (remote_deploy.sh — into its OWN
# data/backups/predeploy/ subdir with its own retention, so the two prune policies
# can't fight over one glob). This is the BETWEEN-deploys safety net: a disk death
# or DB corruption two days after a deploy isn't covered by a deploy-time snapshot.
# Run it on the droplet from a cron/systemd timer (e.g. hourly).
#
# Uses `VACUUM INTO` (NOT `cp`): the server is mid-write under WAL, so a plain copy
# can capture a torn page or miss the -wal sidecar. VACUUM INTO reads the live db in
# one consistent transaction and writes a fresh single-file copy (see the inline
# comment at the snapshot step for why it's preferred over `.backup`). Verifies the
# snapshot with `PRAGMA integrity_check` before counting it as good, and prunes to a
# retention cap. FAILS CLOSED (non-zero) so a cron failure is visible (mail/monitor)
# instead of silently producing no backups.
#
# Usage (from the deploy dir, where ./data lives):
#   scripts/backup_db.sh
#   DB=data/fairtick.db BACKUP_DIR=data/backups RETAIN=48 DISK_WARN_PCT=85 scripts/backup_db.sh
#
# Cron (hourly, log to syslog — see scripts/ops.crontab):
#   0 * * * * cd /opt/fairtick && scripts/backup_db.sh 2>&1 | logger -t fairtick-backup
set -euo pipefail

DB="${DB:-data/fairtick.db}"
BACKUP_DIR="${BACKUP_DIR:-data/backups}"
RETAIN="${RETAIN:-48}"            # hourly × 48 = 2 days of snapshots
DISK_WARN_PCT="${DISK_WARN_PCT:-85}"

if ! command -v sqlite3 >/dev/null 2>&1; then
  echo "ERROR: sqlite3 not installed — apt-get install -y sqlite3" >&2
  exit 1
fi
if [ ! -f "$DB" ]; then
  echo "ERROR: db not found at $DB (run from the deploy dir, or set DB=)" >&2
  exit 1
fi

mkdir -p "$BACKUP_DIR"
ts="$(date -u +%Y%m%d-%H%M%S)"
dest="$BACKUP_DIR/fairtick-${ts}.db"
# Second-resolution timestamps collide if two backups land in the same second (manual
# double-run, or a tighter-than-hourly schedule). NEVER overwrite a prior snapshot —
# disambiguate so a second backup can't silently clobber the first. (date +%N isn't
# portable to BSD/macOS, so use a counter.)
dup=0
while [ -e "$dest" ]; do
  dup=$((dup + 1))
  dest="$BACKUP_DIR/fairtick-${ts}-${dup}.db"
done

# Crash-consistent online snapshot via VACUUM INTO: reads the live db in a consistent
# transaction and writes a FRESH, defragmented, SINGLE-file copy in default rollback mode
# — so the backup has no -wal/-shm sidecars to leak or forget to copy (unlike `.backup`,
# which inherits the source's WAL mode). Requires sqlite 3.27+ (2019).
sqlite3 "$DB" "VACUUM INTO '${dest}'"

# Verify the snapshot is readable + structurally sound BEFORE we trust it / prune.
check="$(sqlite3 "$dest" 'PRAGMA integrity_check;' 2>&1 | head -1)"
if [ "$check" != "ok" ]; then
  echo "ERROR: backup integrity_check failed for ${dest}: ${check}" >&2
  exit 1
fi
size="$(du -h "$dest" | cut -f1)"
echo "backup OK -> ${dest} (${size}, integrity_check=ok)"

# Retention: keep the newest $RETAIN snapshots, delete the rest — including any stray
# -wal/-shm/-journal sidecars (a backup converted to single-file shouldn't have them, but
# don't leak them if an older one did).
ls -1t "$BACKUP_DIR"/fairtick-*.db 2>/dev/null | tail -n +"$((RETAIN + 1))" | while IFS= read -r old; do
  rm -f "$old" "$old"-wal "$old"-shm "$old"-journal
done

# Disk guard: warn (non-fatal) if the filesystem holding the backups is filling up,
# so log/backup growth is noticed before it wedges the box.
used_pct="$(df -P "$BACKUP_DIR" | awk 'NR==2 {gsub(/%/,"",$5); print $5}')"
if [ -n "${used_pct:-}" ] && [ "$used_pct" -ge "$DISK_WARN_PCT" ]; then
  echo "WARN: disk at ${used_pct}% (>= ${DISK_WARN_PCT}%) on the backup volume — prune logs/backups" >&2
fi
