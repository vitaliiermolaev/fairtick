#!/usr/bin/env bash
# Restore drill — prove a backup can actually be restored and read.
#
# A backup you've never restored is a hope, not a backup. This restores a snapshot
# into a THROWAWAY copy (never touches the live db) and verifies it is structurally
# sound and the rows that matter (accounts + reward ledger) are readable. Run it
# periodically (e.g. weekly cron) and as part of incident response before trusting a
# snapshot.
#
# With no argument it checks BOTH backup classes — the newest hourly snapshot
# (data/backups, required) AND the newest pre-deploy snapshot
# (data/backups/predeploy, if any exist) — so neither class can silently rot while
# the other keeps passing the drill.
#
# Usage (from the deploy dir):
#   scripts/restore_check.sh                       # newest hourly + newest predeploy
#   scripts/restore_check.sh data/backups/fairtick-20260609-120000.db
#   BACKUP_DIR=data/backups scripts/restore_check.sh
#
# Exit 0 = restorable + readable; non-zero = a problem (don't trust this snapshot).
set -euo pipefail

BACKUP_DIR="${BACKUP_DIR:-data/backups}"

if ! command -v sqlite3 >/dev/null 2>&1; then
  echo "ERROR: sqlite3 not installed — apt-get install -y sqlite3" >&2
  exit 1
fi

fail=0
note() { echo "  $1"; }

# Restore one snapshot into a throwaway copy and verify it. Accumulates into $fail.
check_snapshot() {
  local src="$1"
  local tmp
  tmp="$(mktemp -t fairtick-restore.XXXXXX.db)"
  # shellcheck disable=SC2064  # expand $tmp now: each snapshot gets its own cleanup
  trap "rm -f '$tmp' '$tmp'-wal '$tmp'-shm" RETURN
  cp -f "$src" "$tmp"
  echo "restore-check: $src"

  # `|| true`: a corrupt/garbage snapshot makes sqlite3 itself exit non-zero, which under
  # `set -e` would kill the whole script mid-report with sqlite's raw exit code. Capture
  # the error text instead and let the checks below fail DETERMINISTICALLY (exit 1, full
  # report) — same fails-closed outcome, predictable for cron/automation.
  local integrity fk cnt pending
  integrity="$(sqlite3 "$tmp" 'PRAGMA integrity_check;' 2>&1 | head -1 || true)"
  if [ "$integrity" = "ok" ]; then note "integrity_check: ok"; else note "integrity_check: FAIL ($integrity)"; fail=1; fi

  fk="$(sqlite3 "$tmp" 'PRAGMA foreign_key_check;' 2>&1 | head -1 || true)"
  if [ -z "$fk" ]; then note "foreign_key_check: ok"; else note "foreign_key_check: VIOLATIONS ($fk)"; fail=1; fi

  # FLOOR: these tables must EXIST — a structurally-valid but empty/foreign db is not a
  # usable restore. This is the only schema knowledge duplicated from src/db.rs migrations
  # (keep in sync when a money/identity-relevant table is added); everything else below is
  # derived from the snapshot itself.
  local tbl
  for tbl in users reward_outbox game_stats; do
    if [ "$(sqlite3 "$tmp" "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='${tbl}';")" != "1" ]; then
      note "${tbl}: MISSING from snapshot"; fail=1
    fi
  done

  # Every table must be queryable — the list is read from the snapshot's own sqlite_master,
  # so a table added by a FUTURE migration is verified automatically instead of silently
  # skipped by a stale hardcoded list.
  while IFS= read -r tbl; do
    if cnt="$(sqlite3 "$tmp" "SELECT count(*) FROM \"${tbl}\";" 2>&1)" && [[ "$cnt" =~ ^[0-9]+$ ]]; then
      note "${tbl}: ${cnt} rows"
    else
      note "${tbl}: UNREADABLE ($cnt)"; fail=1
    fi
  done < <(sqlite3 "$tmp" "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%';")

  # A pending reward backlog is worth surfacing — those are grants not yet credited.
  pending="$(sqlite3 "$tmp" "SELECT count(*) FROM reward_outbox WHERE status='pending';" 2>/dev/null || echo '?')"
  note "reward_outbox pending: ${pending}"
}

if [ -n "${1:-}" ]; then
  # Explicit snapshot: check exactly that one.
  if [ ! -f "$1" ]; then
    echo "ERROR: no such backup: $1" >&2
    exit 1
  fi
  check_snapshot "$1"
else
  # Newest hourly snapshot — REQUIRED: the hourly safety net not producing backups is
  # itself a failure the weekly drill must surface.
  hourly="$(ls -1t "$BACKUP_DIR"/fairtick-*.db 2>/dev/null | head -1 || true)"
  if [ -z "$hourly" ]; then
    echo "ERROR: no hourly backup to check (populate $BACKUP_DIR or pass a path)" >&2
    exit 1
  fi
  check_snapshot "$hourly"

  # Newest pre-deploy snapshot — checked when present (a box that has never deployed has
  # none; that's fine, but existing predeploy backups must not be exempt from the drill).
  predeploy="$(ls -1t "$BACKUP_DIR"/predeploy/fairtick-*.db 2>/dev/null | head -1 || true)"
  if [ -n "$predeploy" ]; then
    check_snapshot "$predeploy"
  else
    note "predeploy: no snapshots yet (skipped)"
  fi
fi

# Fails CLOSED, explicitly: automation (cron/CI) must see a non-zero exit on any failed
# check — a printed "RESTORE FAILED" with exit 0 would be invisible to everything but eyes.
if [ "$fail" -eq 0 ]; then
  echo "RESTORE OK — snapshot(s) restorable and readable"
  exit 0
else
  echo "RESTORE FAILED — do NOT trust this snapshot" >&2
  exit 1
fi
