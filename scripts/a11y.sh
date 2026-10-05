#!/usr/bin/env bash
# Accessibility audit (axe-core WCAG 2.2 A/AA rules plus keyboard focus checks) against a freshly
# installed board in a throwaway database.
#
#   scripts/a11y.sh
#
# Needs Node.js. Installs the audit's pinned packages (tests/a11y/package.json) on first run and
# a Chromium build for Playwright unless CHROMIUM_PATH points at one.
# Environment: RBB_BIN (default ./target/debug/rbb), A11Y_PORT (8093), A11Y_DB (rbb_a11y),
# E2E_SERVER_URL (postgres://rbb@127.0.0.1:5433 — the server, without a database name), PSQL.
set -uo pipefail
cd "$(dirname "$0")/.."
BIN="${RBB_BIN:-./target/debug/rbb}"
PORT="${A11Y_PORT:-8093}"
DBNAME="${A11Y_DB:-rbb_a11y}"
SERVER="${E2E_SERVER_URL:-postgres://rbb@127.0.0.1:5433}"
PSQL="${PSQL:-$(command -v psql || echo /opt/homebrew/opt/postgresql@17/bin/psql)}"
PASSWORD=admin12345

export DATABASE_URL="$SERVER/$DBNAME"
export RBB_LISTEN="127.0.0.1:$PORT"
export RBB_SECRET="${RBB_SECRET:-a11y-$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')}"
unset RBB_DEV_TEMPLATES
BASE="http://127.0.0.1:$PORT"
LOG="$(mktemp -t rbb-a11y.XXXXXX)"

if [ ! -d tests/a11y/node_modules ]; then
  (cd tests/a11y && npm ci --silent) || exit 1
fi
if [ -z "${CHROMIUM_PATH:-}" ]; then
  (cd tests/a11y && npx playwright install chromium >/dev/null) || exit 1
fi

"$PSQL" "$SERVER/postgres" -qc "DROP DATABASE IF EXISTS $DBNAME WITH (FORCE)" -c "CREATE DATABASE $DBNAME" || exit 1
"$BIN" install --admin-password "$PASSWORD" --board-url "$BASE" >>"$LOG" 2>&1 || { cat "$LOG"; exit 1; }

"$BIN" serve >>"$LOG" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null; wait $PID 2>/dev/null' EXIT
for _ in $(seq 1 100); do
  curl -fsS "$BASE/livez" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -fsS "$BASE/livez" >/dev/null 2>&1 || { echo "server did not start; log:"; tail -50 "$LOG"; exit 1; }

node tests/a11y/axe.mjs "$BASE" "$PASSWORD"
