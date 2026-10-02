#!/usr/bin/env bash
# `rbb doctor` against the dev setup and against deliberately broken ones.
# Usage: tests/doctor.sh [rbb binary]   (reads .env for the working values)
set -uo pipefail
RBB="${1:-./target/debug/rbb}"; OUT=$(mktemp); fail=0
ok() { echo "  ok   $1"; }; bad() { echo "  FAIL $1"; fail=1; }
set -a; [ -f .env ] && . ./.env; set +a
SECRET="$RBB_SECRET"
# run NAME WANT_EXIT [VAR=value…] [-- doctor args…]
run() { local name="$1" want="$2"; shift 2; local envs=() args=()
  while [ $# -gt 0 ]; do [ "$1" = -- ] && { shift; args=("$@"); break; }; envs+=("$1"); shift; done
  env ${envs[@]+"${envs[@]}"} "$RBB" doctor ${args[@]+"${args[@]}"} >"$OUT" 2>&1; local code=$?
  [ "$code" = "$want" ] && ok "$name (exit $code)" || { bad "$name (exit $code, want $want)"; sed 's/^/       /' "$OUT" | head -40; }; }
has() { grep -qF -- "$2" "$OUT" && ok "$1" || bad "$1"; }
hasnt() { grep -qF -- "$2" "$OUT" && bad "$1" || ok "$1"; }

echo "# Working dev setup"
run "dev setup passes" 0
has "reports PostgreSQL version" "PostgreSQL 17"
has "reports migrations" "Migrations"
has "reports installed board" "System account"
hasnt "no failures" "✗"
hasnt "never prints the secret" "$SECRET"

echo "# Broken setups"
run "missing secret" 1 RBB_SECRET=
has "explains the secret" "openssl rand -hex 32"
run "short secret" 1 RBB_SECRET=tooshort
run "unknown database" 1 DATABASE_URL=postgres://rbb@127.0.0.1:5433/rbb_no_such_db
has "says how to create it" "createdb"
run "nothing listening" 1 DATABASE_URL=postgres://rbb@127.0.0.1:5999/rbb
has "says to check the server" "running"
run "upload folder missing" 1 RBB_UPLOAD_DIR=/nonexistent/rbb-uploads
has "names the upload folder" "/nonexistent/rbb-uploads"
run "bad listen address" 1 RBB_LISTEN=not-an-address

echo "# Warnings and --strict"
run "dev templates only warn" 0 RBB_DEV_TEMPLATES=templates
has "warns about dev templates" "RBB_DEV_TEMPLATES"
run "--strict fails on warnings" 1 RBB_DEV_TEMPLATES=templates -- --strict

rm -f "$OUT"
[ $fail = 0 ] && echo "ALL OK" || { echo "SOME CHECKS FAILED"; exit 1; }
