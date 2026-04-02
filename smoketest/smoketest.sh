#!/usr/bin/env bash
set -euo pipefail

# Smoke test for gatel — verifies basic start/serve/stop functionality.
# Usage: ./smoketest.sh [path-to-gatel-binary]

GATEL="${1:-./target/release/gatel}"
PORT=19876
TMPDIR="$(mktemp -d)"
PID=""
cleanup() {
    if [[ -n "$PID" ]]; then
        kill "$PID" 2>/dev/null || true
    fi
    rm -rf "$TMPDIR"
}
trap cleanup EXIT

info()  { printf '\033[1;34m[SMOKE]\033[0m %s\n' "$*"; }
pass()  { printf '\033[1;32m[PASS]\033[0m  %s\n' "$*"; }
fail()  { printf '\033[1;31m[FAIL]\033[0m  %s\n' "$*"; exit 1; }

if [[ ! -x "$GATEL" ]]; then
    fail "Binary not found or not executable: $GATEL"
fi

# ---- Test 1: --help works ----
info "Test 1: --help"
"$GATEL" --help >/dev/null 2>&1 || fail "--help returned non-zero"
pass "--help"

# ---- Test 2: --version works ----
info "Test 2: --version"
"$GATEL" --version >/dev/null 2>&1 || fail "--version returned non-zero"
pass "--version"

# ---- Test 3: validate config ----
info "Test 3: validate config"
cat > "$TMPDIR/test.kdl" <<EOF
global {
    http ":${PORT}"
}
site "*" {
    route "/*" {
        respond "smoke-ok" status=200
    }
}
EOF
"$GATEL" validate --config "$TMPDIR/test.kdl" >/dev/null 2>&1 || fail "validate failed"
pass "validate config"

# ---- Test 4: validate rejects bad config ----
info "Test 4: validate rejects bad config"
echo "not valid kdl {{{{" > "$TMPDIR/bad.kdl"
if "$GATEL" validate --config "$TMPDIR/bad.kdl" >/dev/null 2>&1; then
    fail "validate accepted bad config"
fi
pass "rejects bad config"

# ---- Test 5: start, serve HTTP, stop ----
info "Test 5: start, serve, stop"
"$GATEL" run --config "$TMPDIR/test.kdl" &
PID=$!

# Wait for server to start
for _ in $(seq 1 30); do
    if curl -s -o /dev/null "http://127.0.0.1:${PORT}/" 2>/dev/null; then
        break
    fi
    sleep 0.2
done

BODY="$(curl -sf "http://127.0.0.1:${PORT}/" 2>/dev/null)" || fail "HTTP request failed"
if [[ "$BODY" != "smoke-ok" ]]; then
    fail "unexpected response: $BODY"
fi

# Graceful stop
kill "$PID" 2>/dev/null || true
wait "$PID" 2>/dev/null || true
PID=""
pass "start, serve, stop"

echo ""
info "All smoke tests passed."
