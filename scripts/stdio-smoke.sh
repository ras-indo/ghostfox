#!/usr/bin/env bash
# Stdio regression: stdout must carry ONLY MCP JSON-RPC lines while
# startup/config/log noise goes to stderr.
# Usage: stdio-smoke.sh <binary>
set -euo pipefail

BIN="${1:?usage: stdio-smoke.sh <binary>}"
OUT="$(mktemp)"
ERR="$(mktemp)"

cleanup() { rm -f "$OUT" "$ERR"; }
trap cleanup EXIT

# Send one initialize request, then close stdin (EOF triggers waiting() return)
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"stdio-smoke","version":"0.0.0"}}}' \
  | timeout 10 "$BIN" --transport stdio >"$OUT" 2>"$ERR" || true

# Every non-empty line on stdout must be valid JSON (MCP JSON-RPC)
python3 - "$OUT" "$ERR" <<'PY'
import json, sys

out_path = sys.argv[1]
err_path = sys.argv[2]

lines = [l for l in open(out_path, encoding="utf-8") if l.strip()]
err_lines = open(err_path, encoding="utf-8").read().splitlines()

if not lines:
    print(f"FAIL: no output on stdout ({len(err_lines)} stderr lines)")
    sys.exit(1)

for i, line in enumerate(lines):
    try:
        json.loads(line)
    except Exception as e:
        print(f"FAIL: stdout line {i} is not JSON: {line[:200]!r}")
        sys.exit(1)

print(f"PASS: {len(lines)} JSON-RPC line(s) on stdout, {len(err_lines)} log line(s) on stderr")
PY
