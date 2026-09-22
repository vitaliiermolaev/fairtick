#!/bin/bash
# Local dev server for the Unity client.
# Port 8080 is taken by a local Docker proxy, so we bind 8090.
# Override the port with FAIRTICK_PORT=xxxx ./run_server.sh
set -e
cd "$(dirname "$0")"

PORT="${FAIRTICK_PORT:-8090}"
IP=$(ipconfig getifaddr en0 2>/dev/null || ifconfig | grep "inet " | grep -v 127.0.0.1 | awk '{print $2}' | head -1)

echo " fairtick dev server"
echo "   Editor / desktop : ws://127.0.0.1:$PORT/ws"
echo "   Real device      : ws://$IP:$PORT/ws"
echo "   (set this LAN address as serverUrl in GameClient.cs for an iPhone build)"
echo ""
echo "🔥 starting on 0.0.0.0:$PORT (Ctrl+C to stop)…"
echo ""

# Debug build for fast restarts. Use `cargo run --release` for perf testing.
# Auth fails closed (audit item 3): a local dev server must explicitly opt into
# insecure passthrough + the DEV gate, so the Editor/device builds (which send a
# device token, not a real Apple JWT) authenticate locally.
FAIRTICK_BIND_ADDR="0.0.0.0:$PORT" \
FAIRTICK_DEV=1 \
FAIRTICK_AUTH_MODE="${FAIRTICK_AUTH_MODE:-insecure}" \
  exec cargo run
