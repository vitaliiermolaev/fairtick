#!/usr/bin/env bash
# Combined soak — the ONE run that closes the v8/v9 gap for a sale:
#   v8 proved real-hardware economics (droplet, real internet path: egress + host CPU)
#       but bots were NOT claim-ready, so the filler->human eat path was never exercised.
#   v9 proved the full eat path under load (72 filler->human eats accepted, 0 mystery
#       rejects) but on LOOPBACK, where egress/CPU are meaningless and were not quoted.
# This merges them: real $12 droplet (as v8) + `--client-ready` bots (as v9) + egress/CPU
# monitoring ON. One report where the eat/respawn/churn logic runs on real hardware through
# the real internet AND latency + egress + CPU are all measured together.
#
# Division of labour (unchanged from v8/v9, just wired into one invocation):
#   - the MONITOR runs ON the droplet: it samples eth0 tx (egress), the backend container's
#     host-PID CPU/RSS, /readyz, and scrapes the container stdout for panic/outtx/ctrl/shed.
#     None of that is measurable from off-box, which is exactly why v9 (loopback) couldn't.
#   - bot_runner runs HERE (external Mac, residential uplink) against wss://api-dev... — the
#     real production TLS path through Caddy, so egress includes real TLS framing.
#
# Usage:
#   scripts/soak_combined.sh root@<IP> [count] [duration_s] [churn_frac] [label]
#   defaults = the gate profile:            100      300         0.3        v10
#   sparse eat profile e.g.:  ... root@<IP>  4       180         0.5        v10b-sparse
# Requires: passwordless SSH to the droplet (the deploy key), local release bot_runner built,
# FAIRTICK_DOMAIN = the server's public hostname, DNS already pointing at the droplet
# (curl https://$FAIRTICK_DOMAIN/pingz -> ok).
set -euo pipefail

SSH_TARGET="${1:?usage: soak_combined.sh root@<droplet-ip> [count] [duration] [churn] [label]}"
COUNT="${2:-100}"
DURATION_BOTS="${3:-300}"
CHURN="${4:-0.3}"
LABEL="${5:-v10}"
DOMAIN="${FAIRTICK_DOMAIN:?set FAIRTICK_DOMAIN to the server's public hostname}"
ENDPOINT="wss://${DOMAIN}/ws"
PINGZ="https://${DOMAIN}/pingz"
REPO_DIR="/opt/fairtick"          # git working tree + compose on the droplet
CONTAINER="fairtick"
DURATION_MON=$(( DURATION_BOTS + 80 ))      # covers the bot window + warmup/teardown
MON_HEADSTART=5                             # let the monitor establish a net baseline first
BIN="./target/release/bot_runner"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
LOCAL_ART="./metrics/combined-${LABEL}-${STAMP}"
MON_NDJSON="/tmp/soak_monitor_${LABEL}.ndjson"
MON_OUT="/tmp/soak_monitor_${LABEL}.out"
SRV_LOG="/tmp/soak_server_${LABEL}.log"
mkdir -p "$LOCAL_ART"

echo "== combined soak (${LABEL}) =="
echo "   droplet   : $SSH_TARGET"
echo "   endpoint  : $ENDPOINT"
echo "   profile   : ${COUNT} bots / ${DURATION_BOTS}s / churn ${CHURN} / --client-ready / --strict"
echo "   artifacts : $LOCAL_ART"
echo

# 0. Preflight: binary present, DNS+droplet live, SSH works.
[ -x "$BIN" ] || { echo "FATAL: $BIN not built — cargo build --release --bin bot_runner"; exit 2; }
echo "-- preflight: $PINGZ"
curl -fsS --max-time 10 "$PINGZ" && echo "  <- pingz ok" || { echo "FATAL: droplet not answering pingz (DNS not repointed / stack down?)"; exit 2; }
echo "-- preflight: ssh $SSH_TARGET"
ssh -o ConnectTimeout=10 -o BatchMode=yes "$SSH_TARGET" "docker ps --format '{{.Names}}' | grep -qx $CONTAINER" \
  || { echo "FATAL: ssh failed or container '$CONTAINER' not running on droplet"; exit 2; }
echo "  <- ssh + container ok"

# 1. Ship the CURRENT monitor to the droplet (don't trust a stale /tmp copy from a past run).
echo "-- scp soak_monitor.py -> droplet:/tmp/"
scp -q scripts/soak_monitor.py "$SSH_TARGET:/tmp/soak_monitor.py"

# 2. Launch the monitor ON the droplet, in the background locally so we can start bots alongside.
#    The heredoc derives the container's bridge IP + host PID and tees container stdout to a
#    real file (soak_monitor fails fast without one — json-file logging isn't a path).
echo "-- launching droplet-side monitor (duration ${DURATION_MON}s)"
ssh "$SSH_TARGET" 'bash -s' <<REMOTE &
set -euo pipefail
cd ${REPO_DIR}
CIP=\$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' ${CONTAINER})
HPID=\$(docker inspect -f '{{.State.Pid}}' ${CONTAINER})
echo "[droplet] container ip=\$CIP host-pid=\$HPID"
# Fresh stdout capture for THIS run only (--since 0s), backgrounded; killed when the shell exits.
: > ${SRV_LOG}
( docker logs -f --since 0s ${CONTAINER} >> ${SRV_LOG} 2>&1 ) &
LOGPID=\$!
trap 'kill \$LOGPID 2>/dev/null || true' EXIT
python3 /tmp/soak_monitor.py \
  --url http://\$CIP:8080 --pid \$HPID \
  --server-log ${SRV_LOG} --logs-dir ${REPO_DIR}/logs \
  --interval 3 --duration ${DURATION_MON} --strict \
  --max-p95-ms 10 --max-p99-ms 16 --max-over-budget-delta 30 \
  --net-iface eth0 --max-tx-kbs-per-conn 30 \
  --out ${MON_NDJSON} | tee ${MON_OUT}
REMOTE
MON_SSH_PID=$!

# 3. Give the monitor a head start, then run the bots locally (foreground, ~300 s).
sleep "$MON_HEADSTART"
echo "-- running bots: $BIN --count $COUNT --duration $DURATION_BOTS --client-ready --strict"
set +e
"$BIN" --count "$COUNT" --duration "$DURATION_BOTS" \
  --server "$ENDPOINT" --out "$LOCAL_ART" \
  --churn-frac "$CHURN" --strict --client-ready
BOTS_EXIT=$?
set -e
echo "-- bot_runner exit=$BOTS_EXIT (0 = strict gate PASS)"

# 4. Wait for the monitor (it runs ~80 s past the bots) and capture its exit.
echo "-- waiting for droplet monitor to finish its ${DURATION_MON}s window..."
set +e
wait "$MON_SSH_PID"
MON_EXIT=$?
set -e
echo "-- monitor exit=$MON_EXIT (0 = strict gate PASS)"

# 5. Pull the droplet-side artifacts back for the report.
echo "-- collecting droplet artifacts -> $LOCAL_ART"
scp -q "$SSH_TARGET:${MON_NDJSON}" "$LOCAL_ART/" || echo "  WARN: no monitor ndjson"
scp -q "$SSH_TARGET:${MON_OUT}"    "$LOCAL_ART/" || echo "  WARN: no monitor out"
scp -q "$SSH_TARGET:${SRV_LOG}"    "$LOCAL_ART/" || echo "  WARN: no server log"
# Pull the run's telemetry NDJSON (newest in logs/) for the eat-path counts.
NEWEST_NDJSON=$(ssh "$SSH_TARGET" "ls -1t ${REPO_DIR}/logs/fairtick-run_*.ndjson 2>/dev/null | head -1")
[ -n "$NEWEST_NDJSON" ] && scp -q "$SSH_TARGET:$NEWEST_NDJSON" "$LOCAL_ART/" && echo "  telemetry: $(basename "$NEWEST_NDJSON")"

echo
echo "== combined soak done =="
echo "   bot_runner exit : $BOTS_EXIT"
echo "   monitor    exit : $MON_EXIT"
echo "   artifacts       : $LOCAL_ART"
echo
if [ "$BOTS_EXIT" -eq 0 ] && [ "$MON_EXIT" -eq 0 ]; then
  echo "VERDICT: PASS (both strict gates green). Write the ${LABEL} report from $LOCAL_ART."
else
  echo "VERDICT: FAIL — do NOT paper over it. Diagnose bots=$BOTS_EXIT monitor=$MON_EXIT before any sale claim."
fi
exit $(( BOTS_EXIT != 0 || MON_EXIT != 0 ))
