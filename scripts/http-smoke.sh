#!/usr/bin/env bash
# HTTP smoke test for the real ghostcloak-mcp binary.
# Usage: http-smoke.sh <binary> [port]
set -euo pipefail

BIN="${1:?usage: http-smoke.sh <binary> [port]}"
PORT="${2:-18787}"
KEY="smoke-test-key-0123456789abcdef"
BASE="http://127.0.0.1:${PORT}/mcp"
PID=""

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    kill -TERM "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
  fi
  rm -f /tmp/gf_http_code /tmp/gf_init_hdr /tmp/gf_init_body /tmp/gf_tools
}
trap cleanup EXIT

# Start the binary in HTTP mode
"$BIN" \
  --transport http \
  --http-host 127.0.0.1 \
  --http-port "$PORT" \
  --http-api-key "$KEY" \
  >/dev/null 2>&1 &
PID=$!

# Wait for the port to accept connections
CONNECTED=false
for _ in $(seq 1 50); do
  if (echo >/dev/tcp/127.0.0.1/"$PORT") 2>/dev/null; then
    CONNECTED=true
    break
  fi
  sleep 0.2
done

if [ "$CONNECTED" != "true" ]; then
  echo "FAIL: server did not start within 10s"
  exit 1
fi

# --- Test 1: no API key -> 401 ---
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE")
if [ "$CODE" != "401" ]; then
  echo "FAIL: no key expected 401, got $CODE"
  exit 1
fi

# --- Test 2: wrong API key -> 401 ---
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
  -H "Authorization: Bearer wrong-key-here" "$BASE")
if [ "$CODE" != "401" ]; then
  echo "FAIL: wrong key expected 401, got $CODE"
  exit 1
fi

# --- Test 3: wrong scheme -> 401 ---
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
  -H "Authorization: Basic dXNlcjpwYXNz" "$BASE")
if [ "$CODE" != "401" ]; then
  echo "FAIL: wrong scheme expected 401, got $CODE"
  exit 1
fi

# --- Test 4: valid key + initialize -> 200 ---
CODE=$(curl -s -o /tmp/gf_init_body -D /tmp/gf_init_hdr -w '%{http_code}' \
  -X POST \
  -H "Authorization: Bearer $KEY" \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"http-smoke","version":"0.0.0"}}}' \
  "$BASE")
if [ "$CODE" != "200" ]; then
  echo "FAIL: initialize expected 200, got $CODE"
  cat /tmp/gf_init_body
  exit 1
fi
if ! grep -q "protocolVersion" /tmp/gf_init_body; then
  echo "FAIL: initialize response missing protocolVersion"
  cat /tmp/gf_init_body
  exit 1
fi

# Extract session ID
SID=$(grep -i '^mcp-session-id:' /tmp/gf_init_hdr | tr -d '\r' | awk '{print $2}')
if [ -z "$SID" ]; then
  echo "FAIL: missing mcp-session-id header"
  cat /tmp/gf_init_hdr
  exit 1
fi

# --- Test 5: tools/list -> 200 + expected tools ---
CODE=$(curl -s -o /tmp/gf_tools -w '%{http_code}' \
  -X POST \
  -H "Authorization: Bearer $KEY" \
  -H "mcp-session-id: $SID" \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  "$BASE")
if [ "$CODE" != "200" ]; then
  echo "FAIL: tools/list expected 200, got $CODE"
  cat /tmp/gf_tools
  exit 1
fi
if ! grep -q '"name":"session_create"' /tmp/gf_tools; then
  echo "FAIL: session_create missing from tools/list"
  cat /tmp/gf_tools
  exit 1
fi
if ! grep -q '"name":"page_a11y"' /tmp/gf_tools; then
  echo "FAIL: page_a11y missing from tools/list"
  cat /tmp/gf_tools
  exit 1
fi

# --- Test 6: graceful shutdown ---
kill -TERM "$PID"
wait "$PID" 2>/dev/null || true
PID=""

echo "PASS: HTTP smoke test (auth 401 x3, initialize 200, tools/list, shutdown)"
