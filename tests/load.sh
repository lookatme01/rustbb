#!/usr/bin/env bash
# Load test against a seeded board (see `rbb seed`). Requires `hey` (go install github.com/rakyll/hey@latest).
# Usage: tests/load.sh [base_url] [concurrency] [duration]
set -uo pipefail
BASE="${1:-http://127.0.0.1:8090}"; C="${2:-64}"; D="${3:-10s}"
HEY="${HEY:-$HOME/go/bin/hey}"
PSQL="${PSQL:-psql}"; DBURL="${DATABASE_URL:-postgres://rbb@127.0.0.1:5433/rbb_load}"
q() { $PSQL "$DBURL" -tAc "$1"; }
HOT=$(q "SELECT fid FROM forums WHERE type='f' ORDER BY threads DESC LIMIT 1")
HOTPAGES=$(q "SELECT threads/20 FROM forums WHERE fid=$HOT")
MEGA=$(q "SELECT tid FROM threads ORDER BY replies DESC LIMIT 1")
MEGAPAGES=$(q "SELECT (replies+1)/20+1 FROM threads WHERE tid=$MEGA")
NORMAL=$(q "SELECT tid FROM threads WHERE replies BETWEEN 5 AND 30 LIMIT 1")
UID_=$(q "SELECT uid FROM users WHERE postnum > 10 LIMIT 1")
echo "hot forum $HOT ($HOTPAGES pages), mega thread $MEGA ($MEGAPAGES pages), normal thread $NORMAL"

run() { # name url [extra hey args]
  local name="$1" url="$2"; shift 2
  local out; out=$($HEY -z "$D" -c "$C" "$@" "$url" 2>&1)
  local rps p50 p99 codes
  rps=$(echo "$out" | awk '/Requests\/sec/ {printf "%.0f", $2}')
  p50=$(echo "$out" | awk '/50%% in/ {printf "%.1f", $3*1000}')
  p99=$(echo "$out" | awk '/99%% in/ {printf "%.1f", $3*1000}')
  codes=$(echo "$out" | awk '/Status code distribution/,0' | grep -o '\[[0-9]*\][^[]*responses' | tr -s ' ' | paste -sd' ' -)
  printf "%-34s %8s req/s   p50 %7s ms   p99 %7s ms   %s\n" "$name" "$rps" "$p50" "$p99" "$codes"
}

echo "== guest (concurrency $C, $D each)"
run "board index" "$BASE/"
run "hot forum, page 1" "$BASE/forum/$HOT"
run "hot forum, page 50" "$BASE/forum/$HOT?page=50"
run "hot forum, last page" "$BASE/forum/$HOT?page=$HOTPAGES"
run "normal thread" "$BASE/thread/$NORMAL"
run "mega thread, page 1" "$BASE/thread/$MEGA"
run "mega thread, last page" "$BASE/thread/$MEGA?page=$MEGAPAGES"
run "profile" "$BASE/user/$UID_"
run "member list" "$BASE/members"
run "API thread" "$BASE/api/v1/threads/$NORMAL"
run "archive thread" "$BASE/archive/thread/$NORMAL"
run "RSS feed" "$BASE/syndication"

echo "== logged in"
JAR=$(mktemp)
T=$(curl -s -c "$JAR" -b "$JAR" "$BASE/member/login" | sed -n 's/.*name="csrf-token" content="\([^"]*\)".*/\1/p' | head -1)
curl -s -o /dev/null -c "$JAR" -b "$JAR" -d "my_post_key=$T&username=user1&password=password123&remember=1" "$BASE/member/login"
COOKIE=$(sed "s/^#HttpOnly_//" "$JAR" | awk '!/^#/ && NF >= 7 {printf "%s=%s; ", $6, $7}')
T=$(curl -s -b "$JAR" "$BASE/" | sed -n 's/.*name="csrf-token" content="\([^"]*\)".*/\1/p' | head -1)
run "board index (member)" "$BASE/" -H "Cookie: $COOKIE"
run "hot forum (member)" "$BASE/forum/$HOT" -H "Cookie: $COOKIE"
run "normal thread (member)" "$BASE/thread/$NORMAL" -H "Cookie: $COOKIE"
run "search (member)" "$BASE/search/quick?q=postgres+performance" -H "Cookie: $COOKIE"
run "post reply (member, writes)" "$BASE/newreply/$NORMAL" -H "Cookie: $COOKIE" -m POST -T "application/x-www-form-urlencoded" -d "my_post_key=$T&message=Load+test+reply+with+some+text+%3A%29&quickreply=1"
rm -f "$JAR"
