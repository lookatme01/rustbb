#!/usr/bin/env bash
# Resilience gate: hostile input, access control and a concurrent soak against a scratch board.
#
#   scripts/soak.sh [seconds]        (soak duration, default 120)
#
# It installs and seeds a throwaway board in its own database, serves it on a port the OS says is
# free, and makes sure the server answering is the one it started (a port already in use makes
# `rbb serve` fail while a fuzzer would happily attack whatever else is listening). Then:
#
#   1. authz   staff pages and forms replayed as a guest and a plain member (scripts/fuzz-http.py)
#   2. get     hostile path numbers and query parameters
#   3. api     malformed JSON against the API's write endpoints
#   4. soak    members and moderators working the board at random, all at once
#   5. forms   every POST form resubmitted with hostile values (destructive, so it goes last)
#   6. checks  `rbb check` (counters), scripts/integrity.sql (dangling references), no ERROR lines
#              and no 5xx in the server log
#
# Exit status is non-zero if any step finds a problem. Environment: RBB_BIN (default
# ./target/debug/rbb), E2E_SERVER_URL (postgres://rbb@127.0.0.1:5433, the server without a
# database name), PSQL, SOAK_DB (default rbb_soak_<pid>).
set -uo pipefail
cd "$(dirname "$0")/.."
BIN="${RBB_BIN:-./target/debug/rbb}"
SERVER="${E2E_SERVER_URL:-postgres://rbb@127.0.0.1:5433}"
PSQL="${PSQL:-$(command -v psql || echo /opt/homebrew/opt/postgresql@17/bin/psql)}"
export PSQL
SECONDS_TO_SOAK="${1:-120}"
DBNAME="${SOAK_DB:-rbb_soak_$$}"
LOG="$(mktemp -t rbb-soak.XXXXXX)"
PORTFILE="$(mktemp -t rbb-soak-port.XXXXXX)"

export DATABASE_URL="$SERVER/$DBNAME"
export RBB_SECRET="${RBB_SECRET:-soak-$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')}"
unset RBB_DEV_TEMPLATES

PORT=$(python3 -c "import socket;s=socket.socket();s.bind(('127.0.0.1',0));print(s.getsockname()[1]);s.close()")
echo "$PORT" > "$PORTFILE"
export RBB_LISTEN="127.0.0.1:$PORT"
BASE="http://127.0.0.1:$PORT"

PID=""
cleanup() {
  [ -n "$PID" ] && kill "$PID" 2>/dev/null && wait "$PID" 2>/dev/null
  "$PSQL" "$SERVER/postgres" -qc "DROP DATABASE IF EXISTS $DBNAME WITH (FORCE)" >/dev/null 2>&1
  rm -f "$PORTFILE"
}
trap cleanup EXIT

"$PSQL" "$SERVER/postgres" -qc "DROP DATABASE IF EXISTS $DBNAME WITH (FORCE)" -c "CREATE DATABASE $DBNAME" 2>&1 | grep -v NOTICE
"$BIN" install --admin-password admin12345 --board-url "$BASE" >>"$LOG" 2>&1 || { cat "$LOG"; exit 2; }
"$BIN" seed --users 60 --threads 120 --posts 900 >>"$LOG" 2>&1 || { cat "$LOG"; exit 2; }
"$PSQL" "$DATABASE_URL" -q \
  -c "INSERT INTO settings (name,value) VALUES ('ratelimit_requests','0'),('postfloodcheck','0'),('postmergemins','0'),('searchfloodtime','0'),('pmfloodsecs','0') ON CONFLICT (name) DO UPDATE SET value=EXCLUDED.value" \
  -c "UPDATE users SET password=(SELECT password FROM users WHERE uid=1) WHERE username ~ '^user([1-9]|[12][0-9]|30)\$'" || exit 2

"$BIN" serve >>"$LOG" 2>&1 &
PID=$!
for _ in $(seq 1 100); do
  curl -fsS "$BASE/livez" >/dev/null 2>&1 && break
  kill -0 "$PID" 2>/dev/null || { echo "the server exited at startup:"; tail -20 "$LOG"; PID=""; exit 2; }
  sleep 0.1
done
# The process that answers must be ours: if our server could not bind, it has exited.
if ! kill -0 "$PID" 2>/dev/null || grep -q "already in use" "$LOG"; then
  echo "the server did not start on $BASE (port taken?); refusing to fuzz whatever answers there"; PID=""; exit 2
fi
echo "scratch board on $BASE (pid $PID, database $DBNAME)"

failed=()
step() { local name="$1"; shift; echo "== $name"; "$@" || failed+=("$name"); }
step authz python3 scripts/fuzz-http.py authz "$BASE" --member user1 --disposable-board
step get python3 scripts/fuzz-http.py get "$BASE" --disposable-board
step api python3 scripts/fuzz-http.py api "$BASE" --disposable-board
step soak python3 scripts/soak-http.py "$PORTFILE" "$SECONDS_TO_SOAK"
step forms python3 scripts/fuzz-http.py forms "$BASE" --disposable-board

echo "== checks"
"$BIN" check || failed+=("rbb check")
bad=$("$PSQL" "$DATABASE_URL" -Atf scripts/integrity.sql | awk -F'|' '$2 != 0')
if [ -n "$bad" ]; then echo "integrity violations:"; echo "$bad"; failed+=("integrity"); else echo "integrity: all invariants hold"; fi
errors=$(sed 's/\x1b\[[0-9;]*m//g' "$LOG" | grep -c " ERROR ")
if [ "$errors" -gt 0 ]; then
  echo "$errors ERROR lines in the server log:"
  sed 's/\x1b\[[0-9;]*m//g' "$LOG" | grep " ERROR " | cut -c1-240 | sort | uniq -c | sort -rn | head -8
  failed+=("server log")
fi

if [ ${#failed[@]} -gt 0 ]; then echo "FAILED: ${failed[*]} (server log: $LOG)"; exit 1; fi
echo "soak gate passed"
