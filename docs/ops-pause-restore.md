# Pause / restore the dev server

How to take the dev backend fully **off DigitalOcean billing** during a pause and
bring it back later without losing state. The `docker-compose.yml`, `Caddyfile`,
`Dockerfile` and deploy flow already live in git — the only state that lives ONLY
on the droplet is captured by a **snapshot**:

- `data/fairtick.db` — SQLite (accounts / auth)
- `caddy_data` volume — issued Let's Encrypt certs (survive the snapshot, so no
  re-issue and no LE rate-limit risk on restore)
- `ops.env` — the `/ops` basic_auth bcrypt hash (gitignored, droplet-only)

**Why snapshot + destroy, not just power off:** a powered-off droplet is still
billed (disk + IP stay reserved). Cost stops only when the droplet is **destroyed**.
A snapshot is cheap (~$0.06/GB/mo).

Droplet must be **1 vCPU / 2 GB** — `docker-compose.yml` pins `cpus: 1.0`; a
smaller box makes `docker compose up` reject the config.

---

## Pause (stop paying)

Run over SSH to the droplet + the DO panel (or `doctl`).

```bash
# 1. Safety copy of the DB to local (the snapshot captures it too — belt & braces).
ssh <droplet> "sqlite3 /opt/fairtick/data/fairtick.db \".backup '/tmp/fairtick-pause.db'\""
scp <droplet>:/tmp/fairtick-pause.db ./backups/fairtick-pause-$(date -u +%Y%m%d).db

# 2. Power the droplet off FROM INSIDE.
#    Do NOT `docker compose down` — that deletes the containers and they won't
#    auto-start on the restored droplet. `poweroff` stops them gracefully
#    (SQLite WAL is crash-safe regardless) and leaves them in `running` desired
#    state, so `restart: unless-stopped` brings them back on the next boot.
ssh <droplet> "sudo poweroff"
```

Then in the DO panel:

1. Wait until the droplet shows **Off**.
2. **Create Snapshot** — name it e.g. `fairtick-dev-paused-YYYY-MM-DD`. Wait for it to finish.
3. **Destroy Droplet**. Billing now = snapshot storage only.

Leave the `api-dev` DNS record as-is (it points at a dead IP during the pause — fine for dev).

---

## Restore (~10 min)

1. DO → **Create Droplet → From Snapshot** → same region, **1 vCPU / 2 GB**.
2. It boots and Docker auto-starts both containers (they were `running` before
   poweroff + `restart: unless-stopped`). DB and certs are already in place.
3. CloudFlare → point `<dev-domain>` A record at the **new IP**, keep it
   **grey cloud** (DNS only). The client connects by domain, so the new IP is
   absorbed — no client rebuild.

Verify: `curl https://<dev-domain>/pingz` → `ok`.

---

## Gotchas

- **New IP on restore is expected** — DNS absorbs it (grey cloud). No client rebuild.
- **Don't bother with a Reserved IP** to keep the same address: a *detached*
  Reserved IP is billed by DO, so it defeats the point of pausing.
- **Deploying new code after restore:** update the droplet IP in the deploy SSH
  target (GitHub Actions secret) before `gh workflow run deploy.yml` — the old IP
  died with the destroyed droplet. Not needed to just bring the stack back up as-is.
