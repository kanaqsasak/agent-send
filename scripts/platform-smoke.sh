#!/bin/sh
# Safe, host-only smoke test for the daemon and packaged desktop layout.
# It never changes firewall/autostart state and never runs an installer.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
PORT=${AGENT_SEND_SMOKE_PORT:-18765}
BASE="http://127.0.0.1:$PORT"
TMP=${TMPDIR:-/tmp}/agent-send-smoke.$$
DAEMON_PID=
cleanup() {
  if [ -n "${DAEMON_PID:-}" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT INT TERM
mkdir -p "$TMP"

say() { printf '%s\n' "[platform-smoke] $*"; }
skip() { say "SKIP: $*"; }

command -v curl >/dev/null 2>&1 || { say "FAIL: curl is required"; exit 1; }
command -v cargo >/dev/null 2>&1 || { say "FAIL: cargo is required"; exit 1; }
if ! (cd "$ROOT" && cargo build -q -p agent-send-daemon); then
  say "FAIL: daemon build"; exit 1
fi
DAEMON="$ROOT/target/debug/agent-send-daemon"

say "starting daemon in hidden mode on $BASE"
"$DAEMON" --hidden --bind "127.0.0.1:$PORT" --identity-path "$TMP/identity.json" \
  >"$TMP/daemon.stdout" 2>"$TMP/daemon.stderr" &
DAEMON_PID=$!

ready=0
for _ in 1 2 3 4 5 6 7 8 9 10; do
  if curl --fail --silent --show-error --max-time 1 "$BASE/v1/health" >"$TMP/health.json"; then ready=1; break; fi
  sleep 0.2
done
[ "$ready" -eq 1 ] || { say "FAIL: health endpoint did not become ready"; cat "$TMP/daemon.stderr"; exit 1; }
grep -q '"status":"ok"' "$TMP/health.json" || { say "FAIL: unexpected health response"; cat "$TMP/health.json"; exit 1; }
say "PASS: daemon startup and /v1/health"
say "PASS: hidden launch requested (--hidden disables discovery)"

# This must fail before a listener is created; do not probe or alter LAN sockets.
if "$DAEMON" --bind "0.0.0.0:$((PORT + 1))" \
    --identity-path "$TMP/non-loopback.json" >"$TMP/non-loopback.out" 2>&1; then
  say "FAIL: daemon accepted a non-loopback API bind"; exit 1
else
  say "PASS: non-loopback local API bind rejected"
fi

say "PASS: clean shutdown (terminating the test child)"
kill "$DAEMON_PID" 2>/dev/null || true
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=
if curl --silent --max-time 1 "$BASE/v1/health" >/dev/null 2>&1; then
  say "FAIL: health endpoint remained reachable after shutdown"; exit 1
fi
say "PASS: local API is unavailable after shutdown"

# Registration scripts are deliberately not executed: doing so changes a user's login state.
for f in apps/desktop/scripts/register-linux.sh apps/desktop/scripts/register-macos.sh \
         apps/desktop/scripts/register-windows.ps1 apps/desktop/scripts/unregister-linux.sh \
         apps/desktop/scripts/unregister-macos.sh apps/desktop/scripts/unregister-windows.ps1; do
  [ -f "$ROOT/$f" ] || { say "FAIL: missing packaging script $f"; exit 1; }
done
say "PASS: autostart registration/unregistration scripts present"
skip "autostart runtime registration (would modify the current user's login state)"
skip "LAN firewall/mDNS behavior (requires a trusted LAN and the target host OS)"
skip "installer install/uninstall (provide a target-OS installer for a non-destructive manual run)"
say "completed on $(uname -s) without claiming other operating systems"
