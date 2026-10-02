#!/usr/bin/env bash
# Run the end-to-end shell suites against a freshly installed board in a throwaway database.
#
#   scripts/e2e.sh [suite ...]        (default: all suites)
#
# Environment: RBB_BIN (default ./target/debug/rbb), E2E_PORT (8092), E2E_DB (rbb_e2e),
# E2E_SERVER_URL (postgres://rbb@127.0.0.1:5433 — the server, without a database name), PSQL.
set -uo pipefail
cd "$(dirname "$0")/.."
BIN="${RBB_BIN:-./target/debug/rbb}"
PORT="${E2E_PORT:-8092}"
DBNAME="${E2E_DB:-rbb_e2e}"
SERVER="${E2E_SERVER_URL:-postgres://rbb@127.0.0.1:5433}"
PSQL="${PSQL:-$(command -v psql || echo /opt/homebrew/opt/postgresql@17/bin/psql)}"
export PSQL
SUITES=("$@")
[ ${#SUITES[@]} -eq 0 ] && SUITES=(smoke admin_smoke moderation system system_features deleted_visibility privacy mod_workflow)

export DATABASE_URL="$SERVER/$DBNAME"
export RBB_LISTEN="127.0.0.1:$PORT"
export RBB_SECRET="${RBB_SECRET:-e2e-$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')}"
unset RBB_DEV_TEMPLATES
BASE="http://127.0.0.1:$PORT"
LOG="$(mktemp -t rbb-e2e.XXXXXX)"

"$PSQL" "$SERVER/postgres" -qc "DROP DATABASE IF EXISTS $DBNAME WITH (FORCE)" -c "CREATE DATABASE $DBNAME" || exit 1
"$BIN" install --admin-password admin12345 --board-url "$BASE" >>"$LOG" 2>&1 || { cat "$LOG"; exit 1; }

PID=""
start() {
  "$BIN" serve >>"$LOG" 2>&1 &
  PID=$!
  for _ in $(seq 1 100); do
    curl -fsS "$BASE/livez" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  echo "server did not start; log:"; tail -50 "$LOG"; exit 1
}
stop() { [ -n "$PID" ] && kill "$PID" 2>/dev/null && wait "$PID" 2>/dev/null; PID=""; }
trap stop EXIT

failed=()
for s in "${SUITES[@]}"; do
  # A fresh process per suite: the suites sign in often, and sign-in throttling is per address.
  "$PSQL" "$DATABASE_URL" -qc "DELETE FROM ratelimits" >/dev/null 2>&1 || true
  start
  echo "== $s"
  case "$s" in
    moderation) out=$(bash "tests/$s.sh" "$BASE" "$BIN" 2>&1) ;;
    admin_smoke) out=$(bash "tests/$s.sh" "$BASE" admin12345 2>&1) ;;
    *) out=$(bash "tests/$s.sh" "$BASE" 2>&1) ;;
  esac
  code=$?
  echo "$out" | grep -E "FAIL|ALL OK" | head -20
  if [ $code -ne 0 ] || echo "$out" | grep -q "FAIL"; then failed+=("$s"); fi
  stop
done
if [ ${#failed[@]} -gt 0 ]; then
  echo "failed suites: ${failed[*]} (server log: $LOG)"
  exit 1
fi
echo "all suites passed"
