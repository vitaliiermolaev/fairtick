#!/usr/bin/env bash
# Runs ON the dev droplet — shipped + invoked by .github/workflows/deploy.yml after the new
# image is `docker load`ed as fairtick:latest. Rolls the backend forward behind a
# HEALTH GATE with AUTOMATIC ROLLBACK: if the new image's /readyz healthcheck never goes
# healthy, revert to the image that was running before — so one bad evening deploy can't
# leave the beta down with only manual recovery.
#
# Safe to re-run. Kept as a committed file (not an inline heredoc in the workflow) so the
# deploy logic is reviewable and shell-lintable.
set -euo pipefail

cd /opt/fairtick

WAIT_TIMEOUT="${WAIT_TIMEOUT:-90}"

# 1. Back up the SQLite DB before touching the stack (keep the last 10 backups). Use SQLite's
#    ONLINE backup API, NOT a plain `cp`: the OLD container is still running and may be mid-write
#    when we back up, and a raw file copy can capture a half-written transaction (and would miss
#    the -wal/-shm sidecars under WAL). `.backup` produces a consistent snapshot of a live db.
#    FAIL CLOSED if sqlite3 is missing — a "warning + maybe-corrupt backup" reads as a real backup
#    until the day you need it. Install it once on the droplet: `apt-get install -y sqlite3`.
#    (review Blocker 4 + follow-up: fail-closed)
if [ -f data/fairtick.db ]; then
  if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "ERROR: sqlite3 is required for a crash-consistent DB backup — install it (apt-get install -y sqlite3) and re-deploy" >&2
    exit 1
  fi
  # Pre-deploy snapshots live in their OWN subdir with their OWN retention: the hourly
  # backup_db.sh prunes data/backups/fairtick-*.db to RETAIN=48, and this prune used to
  # hit the SAME glob with keep-10 — so every deploy silently collapsed two days of
  # hourly history down to 10 files. Separate dirs = separate policies, no fighting.
  mkdir -p data/backups/predeploy
  ts="$(date -u +%Y%m%dT%H%M%SZ)"
  dest="data/backups/predeploy/fairtick-${ts}.db"
  sqlite3 data/fairtick.db ".backup '${dest}'"
  echo "backed up DB (sqlite3 .backup, crash-consistent) -> ${dest}"
  # shellcheck disable=SC2012  # filenames are timestamps we control; ls -t is fine here
  ls -1t data/backups/predeploy/fairtick-*.db | tail -n +11 | xargs -r rm -f
fi

# 2. Remember the image the backend runs RIGHT NOW, as the rollback target — captured BEFORE
#    `compose up` recreates the container with the just-loaded :latest.
PREV="$(docker inspect --format '{{.Image}}' fairtick 2>/dev/null || true)"

# 3. Roll forward. --wait blocks until the container's /readyz HEALTHCHECK reports healthy
#    (or the timeout fires), so a broken build fails HERE instead of silently serving 503s.
if docker compose up -d --wait --wait-timeout "${WAIT_TIMEOUT}"; then
  # compose does NOT recreate caddy when only the bind-mounted Caddyfile changed (the
  # container/image are unchanged), so an edge-config change would silently keep the OLD
  # routing/auth until the next caddy restart. Found live 2026-06-11: the new /ops
  # basic_auth shipped but didn't bite until a manual reload. Reload is graceful
  # (zero-downtime) and a no-op when the config didn't change.
  docker exec fairtick-caddy caddy reload --config /etc/caddy/Caddyfile \
    || echo "WARN: caddy reload failed — edge may be serving the previous Caddyfile" >&2
  echo "deploy healthy"
  docker image prune -f
  exit 0
fi

# 4. Health gate failed → roll back. A bad deploy can be a bad IMAGE *or* a bad compose/Caddy
#    file — and this same workflow just OVERWROTE both. An image-only rollback would relaunch the
#    previous image under the NEW (possibly broken) stack definition. So first restore the
#    previous compose + Caddyfile that the workflow saved as .prev BEFORE shipping the new ones,
#    then roll the image back too. (review Blocker 5)
echo "new image failed the health gate -> rolling back" >&2
docker compose logs --tail 80 backend || true
if [ -f docker-compose.yml.prev ]; then
  cp -f docker-compose.yml docker-compose.yml.failed || true   # keep the bad one for inspection
  mv -f docker-compose.yml.prev docker-compose.yml
  echo "restored previous docker-compose.yml"
fi
if [ -f Caddyfile.prev ]; then
  cp -f Caddyfile Caddyfile.failed || true
  mv -f Caddyfile.prev Caddyfile
  echo "restored previous Caddyfile"
fi
if [ -n "${PREV}" ]; then
  docker tag "${PREV}" fairtick:latest
  if docker compose up -d --wait --wait-timeout "${WAIT_TIMEOUT}"; then
    echo "rolled back to ${PREV} + previous compose/Caddyfile (healthy)"
  else
    echo "ROLLBACK ALSO UNHEALTHY — manual intervention needed" >&2
  fi
else
  echo "no previous image to roll back to (first deploy?)" >&2
fi
exit 1
