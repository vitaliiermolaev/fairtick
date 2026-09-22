#!/usr/bin/env python3
"""Soak monitor — polls the server's /readyz and scrapes its stdout for tick-perf,
recording a time series of degradation + load (rooms, players≈conns, tick p95/p99,
over-budget ticks, server RSS, telemetry-log disk growth, and warning counts).

Standalone (stdlib only). Run alongside a soak:
  python3 scripts/soak_monitor.py --url http://127.0.0.1:8080 --pid <server_pid> \
      --server-log /tmp/soak_server.log --logs-dir logs --interval 3 --duration 300 \
      --out /tmp/soak_monitor.ndjson [--strict]

--strict is the beta GO/NO-GO gate: control drops, rate-limit sheds on the honest
profile, and monitor log-read errors all fail the run, not just panics/outtx/readiness.
Pair it with bot_runner --strict. Tick-performance gates are opt-in thresholds checked
whenever provided: --max-p95-ms / --max-p99-ms (peak tick latency) and
--max-over-budget-delta (over-budget ticks accumulated during the run).

THRESHOLD VALUES ARE NOT CHOSEN HERE: docs/perf-baseline.md is the single source
of truth for the hard floors every gate run must pass — quote its numbers in the
flags above; if anything here ever disagrees with that file, the file wins.
"""
import argparse, glob, json, os, re, subprocess, sys, time, urllib.request

TICK_RE = re.compile(
    r"tick_p50=([\d.]+)ms p95=([\d.]+)ms p99=([\d.]+)ms max=([\d.]+)ms over_budget=(\d+)"
)
# Hard-failure / back-pressure markers we count cumulatively from the server stdout.
# Needles MUST be mutually exclusive across keys, and they match only the FULL
# (back-pressure on a live socket) variants: CLOSED-channel teardown races (churn /
# disconnect draining) log at debug by design and never reach these counters —
# 'outtx' = the reliable-forwarder overflow line, 'ctrl' = a dropped control reply.
# 'shed' matches the limiter's first-drop-per-connection INFO line
# ("rate-limited <class> — first drop ..."); further drops are debug-level by design.
WARN_MARKERS = {
    "panic": ("PANICKED", "room_update_panic_evicted"),
    "outtx": ("reliable out_tx full",),
    "ctrl": ("control reply dropped",),
    "shed": ("rate-limited",),
}


def get_readyz(url):
    try:
        with urllib.request.urlopen(url + "/readyz", timeout=3) as r:
            return r.getcode(), json.loads(r.read().decode())
    except urllib.error.HTTPError as e:  # 503 still carries a JSON body
        try:
            return e.code, json.loads(e.read().decode())
        except Exception:
            return e.code, {}
    except Exception:
        return 0, {}  # unreachable (starting up / down)


def rss_mb(pid):
    try:
        out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)],
                             capture_output=True, text=True).stdout.strip()
        return round(int(out) / 1024.0, 1) if out else None
    except Exception:
        return None


def net_bytes(iface):
    """(rx, tx) byte counters for a host interface, or None off-Linux/bad iface."""
    try:
        base = f"/sys/class/net/{iface}/statistics"
        with open(base + "/rx_bytes") as r, open(base + "/tx_bytes") as t:
            return int(r.read()), int(t.read())
    except OSError:
        return None


def host_cpu_ticks():
    """(busy, total) jiffies for the WHOLE host from /proc/stat — covers backend + Caddy TLS
    + softirq, the parts a per-container stat misses. None off-Linux."""
    try:
        with open("/proc/stat") as f:
            parts = f.readline().split()[1:]
        vals = [int(x) for x in parts]
        idle = vals[3] + (vals[4] if len(vals) > 4 else 0)  # idle + iowait
        return sum(vals) - idle, sum(vals)
    except OSError:
        return None


def telemetry_bytes(logs_dir):
    """Total bytes across ALL telemetry run segments, re-globbed on every call.

    The in-app writer ROTATES to a new fairtick-run_*_NNN.ndjson segment at the size
    cap and retention PRUNES old segments — a single file resolved once at startup
    flatlines after the first rotation (and goes missing after pruning), silently
    blinding the disk-growth metric this monitor exists to record. Summing the live
    glob tracks the actual on-disk footprint through rotation and pruning."""
    total = 0
    for f in glob.glob(os.path.join(logs_dir, "fairtick-run_*.ndjson")):
        try:
            total += os.path.getsize(f)
        except OSError:  # pruned between glob and stat
            pass
    return total


class LogScraper:
    """Incremental stdout scraper: keeps a byte offset so each poll reads only NEW
    lines. Re-reading the whole log every interval is O(n^2) over a multi-hour soak —
    the monitor would burn CPU/disk on the same box it is measuring. Counters and the
    last tick-perf sample accumulate across calls. Rotation is detected by INODE change
    (a replaced file can already be larger than the old offset, so a size check alone
    would silently skip its head) and truncation by a shrunken size — both rescan from
    byte 0. Read failures are COUNTED, not swallowed: a monitor that can't read the
    server log must not present zeros as a clean run (gate-tooling failure mode)."""

    def __init__(self, path):
        self.path = path
        self.pos = 0
        self.inode = None
        self.read_errors = 0
        self.tick = {}
        self.warns = {k: 0 for k in WARN_MARKERS}

    def scrape(self):
        try:
            st = os.stat(self.path)
            if self.inode is not None and st.st_ino != self.inode:
                self.pos = 0  # rotated: a different file now lives at this path
            self.inode = st.st_ino
            if st.st_size < self.pos:
                self.pos = 0  # truncated underneath us
            with open(self.path, "rb") as f:
                f.seek(self.pos)
                for raw in f:
                    line = raw.decode("utf-8", errors="ignore")
                    m = TICK_RE.search(line)
                    if m:
                        self.tick = {"p50": float(m[1]), "p95": float(m[2]),
                                     "p99": float(m[3]), "max": float(m[4]),
                                     "over_budget": int(m[5])}
                    for key, needles in WARN_MARKERS.items():
                        if any(n in line for n in needles):
                            self.warns[key] += 1
                self.pos = f.tell()
        except OSError as e:
            self.read_errors += 1
            print(f"[monitor] WARN server log unreadable ({e})", file=sys.stderr, flush=True)
        return self.tick, dict(self.warns)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="http://127.0.0.1:8080")
    ap.add_argument("--pid", type=int, default=0)
    ap.add_argument("--server-log", default="/tmp/soak_server.log")
    ap.add_argument("--logs-dir", default="logs")
    ap.add_argument("--interval", type=float, default=3.0)
    ap.add_argument("--duration", type=float, default=300.0)
    ap.add_argument("--out", default="/tmp/soak_monitor.ndjson")
    ap.add_argument("--strict", action="store_true",
                    help="gate mode: control drops, rate-limit sheds, and log-read errors "
                         "also fail the run (exit 1), not just panics/outtx/readiness")
    # Tick-performance gates (None = off). A soak where the server stays ready but the
    # simulation regularly blows its tick budget is NOT a pass for a real-time game —
    # "alive" and "keeping up" are different claims. Thresholds fail the run whenever
    # provided (with or without --strict).
    ap.add_argument("--max-p95-ms", type=float, default=None,
                    help="fail if peak tick p95 exceeds this")
    ap.add_argument("--max-p99-ms", type=float, default=None,
                    help="fail if peak tick p99 exceeds this")
    ap.add_argument("--max-over-budget-delta", type=int, default=None,
                    help="fail if over_budget ticks ACCUMULATED DURING THE RUN exceed this "
                         "(delta between the first and last tick-perf sample)")
    # Egress observability (the measured bottleneck: JSON snapshots hit 78 Mbit/s at 100
    # conns). --net-iface samples host tx/rx; the budget gate is PER CONNECTION so the
    # threshold doesn't depend on how many bots a given run uses.
    ap.add_argument("--net-iface", default=None,
                    help="host interface (e.g. eth0) to sample tx/rx rates from")
    ap.add_argument("--max-tx-kbs-per-conn", type=float, default=None,
                    help="fail if peak egress per connection (KB/s, sampled while conns>=10) "
                         "exceeds this; requires --net-iface")
    a = ap.parse_args()

    # Fail FAST on a missing server log: otherwise the monitor would happily poll /readyz,
    # report zero panics/outtx/ctrl/shed (because it read nothing), and exit 0 — a green
    # verdict from a blind monitor is the worst gate-tooling failure mode.
    if not os.path.exists(a.server_log):
        print(f"[monitor] ERROR server log not found: {a.server_log}", file=sys.stderr)
        sys.exit(2)
    # Same blind-gate rule for the egress budget: a per-conn threshold without a (readable)
    # interface would compare against a peak of 0.0 and pass green having measured nothing.
    if a.max_tx_kbs_per_conn is not None and not a.net_iface:
        print("[monitor] ERROR --max-tx-kbs-per-conn requires --net-iface", file=sys.stderr)
        sys.exit(2)
    if a.net_iface and net_bytes(a.net_iface) is None:
        print(f"[monitor] ERROR net iface unreadable: {a.net_iface}", file=sys.stderr)
        sys.exit(2)

    log_bytes_start = telemetry_bytes(a.logs_dir)
    scraper = LogScraper(a.server_log)
    print(f"[monitor] logs_dir={a.logs_dir} telemetry_start_bytes={log_bytes_start} pid={a.pid}"
          f" strict={a.strict}", flush=True)

    def log_growth_mb():
        # Footprint DELTA vs start; can dip negative when retention prunes segments
        # that predate the run — that's the honest on-disk picture, not an error.
        return round((telemetry_bytes(a.logs_dir) - log_bytes_start) / 1e6, 2)

    peak = {"rooms": 0, "conns": 0, "tick_age": 0, "p95": 0.0, "p99": 0.0, "rss": 0.0,
            "tx_kbs": 0.0, "tx_kbs_per_conn": 0.0, "host_cpu_pct": 0.0}
    ready_non200 = 0
    prev_net = net_bytes(a.net_iface) if a.net_iface else None
    prev_cpu = host_cpu_ticks()
    prev_t = time.time()
    # over_budget is a cumulative counter in the server's tick-perf line; the run's
    # contribution is last-sample minus first-sample (the server may predate the monitor).
    first_over_budget = None
    last_over_budget = None
    # How many samples the per-conn egress gate actually measured (conns>=10 with a
    # readable iface) — a threshold that never sampled must fail, not pass at peak 0.0.
    tx_per_conn_samples = 0
    t0 = time.time()
    with open(a.out, "w") as out:
        while time.time() - t0 < a.duration:
            t = round(time.time() - t0, 1)
            code, body = get_readyz(a.url)
            rooms = body.get("rooms")
            conns = body.get("active_conns")
            tick_age = body.get("tick_loop_age_ms")
            rss = rss_mb(a.pid) if a.pid else None
            log_mb = log_growth_mb()
            tick, warns = scraper.scrape()

            # Rates since the previous sample: host egress (and per-conn share) + whole-host
            # CPU (backend + Caddy TLS + softirq — what a per-container number misses).
            now_t = time.time()
            dt_s = max(now_t - prev_t, 0.001)
            tx_kbs = rx_kbs = host_cpu = None
            if a.net_iface:
                cur = net_bytes(a.net_iface)
                if cur and prev_net:
                    rx_kbs = round((cur[0] - prev_net[0]) / dt_s / 1024, 1)
                    tx_kbs = round((cur[1] - prev_net[1]) / dt_s / 1024, 1)
                prev_net = cur or prev_net
            cur_cpu = host_cpu_ticks()
            if cur_cpu and prev_cpu and cur_cpu[1] > prev_cpu[1]:
                host_cpu = round(100.0 * (cur_cpu[0] - prev_cpu[0]) / (cur_cpu[1] - prev_cpu[1]), 1)
            prev_cpu = cur_cpu or prev_cpu
            prev_t = now_t
            tx_per_conn = (round(tx_kbs / conns, 1)
                           if tx_kbs is not None and isinstance(conns, int) and conns >= 10 else None)
            if tx_kbs is not None:
                peak["tx_kbs"] = max(peak["tx_kbs"], tx_kbs)
            if tx_per_conn is not None:
                tx_per_conn_samples += 1
                peak["tx_kbs_per_conn"] = max(peak["tx_kbs_per_conn"], tx_per_conn)
            if host_cpu is not None:
                peak["host_cpu_pct"] = max(peak["host_cpu_pct"], host_cpu)

            if code != 200:
                ready_non200 += 1
            for k, v in (("rooms", rooms), ("conns", conns), ("tick_age", tick_age), ("rss", rss)):
                if isinstance(v, (int, float)):
                    peak[k] = max(peak[k], v)
            if tick:
                peak["p95"] = max(peak["p95"], tick.get("p95", 0.0))
                peak["p99"] = max(peak["p99"], tick.get("p99", 0.0))
                if isinstance(tick.get("over_budget"), int):
                    if first_over_budget is None:
                        first_over_budget = tick["over_budget"]
                    last_over_budget = tick["over_budget"]

            rec = {"t": t, "ready_code": code, "rooms": rooms, "conns": conns,
                   "tick_loop_age_ms": tick_age, "tick": tick, "warns": warns,
                   "rss_mb": rss, "log_growth_mb": log_mb,
                   "tx_kbs": tx_kbs, "rx_kbs": rx_kbs, "tx_kbs_per_conn": tx_per_conn,
                   "host_cpu_pct": host_cpu}
            out.write(json.dumps(rec) + "\n")
            out.flush()
            print(f"t={t:6.0f}s ready={code} rooms={rooms} conns={conns} "
                  f"tickage={tick_age}ms p95={tick.get('p95','?')}ms p99={tick.get('p99','?')}ms "
                  f"over_budget={tick.get('over_budget','?')} rss={rss}MB log+={log_mb}MB "
                  f"tx={tx_kbs}KB/s({tx_per_conn}/conn) hostcpu={host_cpu}% "
                  f"warn(panic={warns['panic']},outtx={warns['outtx']},ctrl={warns['ctrl']},shed={warns['shed']})",
                  flush=True)
            time.sleep(a.interval)

    _, warns = scraper.scrape()
    over_budget_delta = (last_over_budget - first_over_budget
                         if first_over_budget is not None else None)
    # Tick-performance gates: "server stayed ready" and "simulation kept its budget" are
    # different claims — a real-time game needs both. Checked whenever thresholds are given.
    perf_fail = []
    if a.max_p95_ms is not None and peak["p95"] > a.max_p95_ms:
        perf_fail.append(f"p95 {peak['p95']}ms > {a.max_p95_ms}ms")
    if a.max_p99_ms is not None and peak["p99"] > a.max_p99_ms:
        perf_fail.append(f"p99 {peak['p99']}ms > {a.max_p99_ms}ms")
    if a.max_over_budget_delta is not None and (over_budget_delta or 0) > a.max_over_budget_delta:
        perf_fail.append(f"over_budget +{over_budget_delta} > {a.max_over_budget_delta}")
    if a.max_tx_kbs_per_conn is not None:
        if tx_per_conn_samples == 0:
            perf_fail.append("egress gate had ZERO samples with conns>=10 — nothing was measured")
        elif peak["tx_kbs_per_conn"] > a.max_tx_kbs_per_conn:
            perf_fail.append(
                f"egress {peak['tx_kbs_per_conn']}KB/s/conn > {a.max_tx_kbs_per_conn}KB/s/conn")
    summary = {"peak": peak, "ready_non200_samples": ready_non200, "final_warns": warns,
               "final_log_growth_mb": log_growth_mb(),
               "over_budget_delta": over_budget_delta,
               "tx_per_conn_samples": tx_per_conn_samples,
               "perf_fail": perf_fail,
               "log_read_errors": scraper.read_errors,
               "server_log_readable": scraper.read_errors == 0,
               "net_iface": a.net_iface,
               "net_iface_readable": bool(a.net_iface) and net_bytes(a.net_iface) is not None,
               "strict": a.strict}
    print("[monitor] SUMMARY " + json.dumps(summary), flush=True)
    # Verdict. ALWAYS fatal: panics, reliable-lane overflow, readiness failures, and any
    # explicitly-requested tick-perf threshold.
    # In --strict (beta go/no-go gate): 'ctrl' (a control reply hit a full FIFO — the server
    # closes that connection; recovery works, but recovery must not be part of normal
    # operation on an honest profile), 'shed' (rate-limit drops on honest traffic mean the
    # profile and the limits are out of sync), and log-read errors (a blind monitor must
    # never report green) also fail. Exploratory/overload runs leave them as soft warnings.
    bad = warns["panic"] > 0 or warns["outtx"] > 0 or ready_non200 > 0 or bool(perf_fail)
    if a.strict:
        bad = bad or warns["ctrl"] > 0 or warns["shed"] > 0 or scraper.read_errors > 0
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
