#!/usr/bin/env bash
# Spins up the real relay + controller (relay/E2EE mode) and drives the Node
# reference dashboard against them, asserting it completes the Noise handshake
# and DECRYPTS the controller's channel-1 output. Proves JS↔Rust E2EE interop.
#
# Usage: cargo build --bins && dashboard/interop-test.sh
set -euo pipefail
cd "$(dirname "$0")/.."

BIN="$PWD/target/debug/claude-pty-controller"
RELAYBIN="$PWD/target/debug/relay"
[ -x "$BIN" ] && [ -x "$RELAYBIN" ] || { echo "build first: cargo build --bins"; exit 2; }
command -v node >/dev/null || { echo "node required"; exit 2; }
command -v tmux >/dev/null || { echo "tmux required"; exit 2; }

PORT=$(( ( $$ % 8000 ) + 54000 )); ADDR="127.0.0.1:$PORT"; URL="ws://$ADDR"
RTOK="itok"; PAIR="0123456789abcdef0123456789abcdef-interop"; SOCK="cpc-interop-$$"
BASE="$(mktemp -d)"; mkdir -p "$BASE/home" "$BASE/claude/projects/-tmp"
printf '#!/bin/sh\nwhile true; do printf "INTEROP-MARK\\n"; sleep 0.3; done\n' > "$BASE/agent.sh"
chmod +x "$BASE/agent.sh"

cleanup() {
  kill -INT "${CP:-}" 2>/dev/null || true
  tmux -L "$SOCK" kill-server 2>/dev/null || true
  kill "${RP:-}" "${CP:-}" "${NP:-}" 2>/dev/null || true
  rm -rf "$BASE"
}
trap cleanup EXIT

RELAY_ADDR="$ADDR" RELAY_TOKEN="$RTOK" RUST_LOG=warn "$RELAYBIN" >"$BASE/relay.log" 2>&1 & RP=$!
sleep 0.5
CPC_INSECURE=1 REMOTE_URL="$URL" PAIRING_SECRET="$PAIR" RELAY_TOKEN="$RTOK" CPC_ALLOW_ENROLL=1 \
  CLAUDE_PTY_HOME="$BASE/home" CLAUDE_CONFIG_DIR="$BASE/claude" \
  TMUX_SOCKET="$SOCK" TMUX_SESSION="$SOCK" AGENT_CMD="$BASE/agent.sh" RUST_LOG=warn \
  sh -c 'cd /tmp && exec "$1"' _ "$BIN" >"$BASE/ctl.log" 2>&1 & CP=$!
sleep 1.5
timeout 8 node dashboard/cli.mjs --url "$URL" --pairing "$PAIR" --relay-token "$RTOK" \
  >"$BASE/dash.out" 2>"$BASE/dash.err" & NP=$!
sleep 6; kill "$NP" 2>/dev/null || true

echo "--- dashboard saw message types ---"
grep -oE '"type":"[a-z]+"' "$BASE/dash.out" | sort | uniq -c || true

if grep -q '"type":"hello"' "$BASE/dash.out" \
   && grep -q '"type":"output","raw":"[^"]*INTEROP-MARK' "$BASE/dash.out"; then
  echo "PASS: dashboard handshook and decrypted hello + channel-1 output"
  exit 0
else
  echo "FAIL: expected hello + decrypted INTEROP-MARK output"
  echo "--- dash.err ---"; tail -5 "$BASE/dash.err" || true
  exit 1
fi
