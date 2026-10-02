#!/usr/bin/env bash
# Enforce performance budgets against a running, seeded board (`rbb seed`).
#
#   tests/load_budget.sh BASE_URL SERVER_PID METRICS_URL
#
# Per route (tests/load_budgets.txt): p95/p99 latency, error rate, statements per request (the
# server must run with RBB_QUERY_SAMPLE_RATE=1). Then: database pool wait, server memory, and
# behaviour under overload (only 429/503 may fail, nothing may crash). Needs `hey`.
# Environment: CONCURRENCY (16), DURATION (10s), MAX_RSS_MB (512), MAX_POOL_WAIT_MS (20),
# MAX_ERROR_RATE (0.001), OVERLOAD_CONCURRENCY (400), DATABASE_URL, PSQL, HEY.
set -uo pipefail
BASE="$1"; PID="$2"; METRICS="$3"
C="${CONCURRENCY:-16}"; D="${DURATION:-10s}"
HEY="${HEY:-$(command -v hey || echo "$HOME/go/bin/hey")}"
PSQL="${PSQL:-psql}"; DB="${DATABASE_URL:?set DATABASE_URL}"
q() { "$PSQL" "$DB" -tAc "$1"; }
HOT=$(q "SELECT fid FROM forums WHERE type='f' ORDER BY threads DESC LIMIT 1")
THREAD=$(q "SELECT tid FROM threads WHERE replies BETWEEN 5 AND 30 AND visible = 1 LIMIT 1")
MEGA=$(q "SELECT tid FROM threads WHERE visible = 1 ORDER BY replies DESC LIMIT 1")
MEGALAST=$(q "SELECT (replies+1)/20+1 FROM threads WHERE tid=$MEGA")
USER_=$(q "SELECT uid FROM users WHERE postnum > 10 LIMIT 1")
fail=0
bad() { echo "  OVER BUDGET: $1"; fail=1; }

metric_avg() { # name route -> average of a histogram (sum/count) for that route label
  curl -s "$METRICS" | awk -v n="$1" -v r="$2" '
    index($0, n "_sum{route=\"" r "\"}") == 1 { s = $2 }
    index($0, n "_count{route=\"" r "\"}") == 1 { c = $2 }
    END { if (c > 0) printf "%.1f", s / c; else print "0" }'
}

echo "== routes (concurrency $C, $D each)"
printf "%-16s %8s %8s %8s %9s %8s\n" route req/s p95ms p99ms errors queries
while read -r name path p95b p99b qb; do
  [[ -z "$name" || "$name" == \#* ]] && continue
  url="$BASE$(echo "$path" | sed "s/{HOT}/$HOT/; s/{THREAD}/$THREAD/; s/{MEGA}/$MEGA/; s/{USER}/$USER_/; s/page=last/page=$MEGALAST/")"
  out=$("$HEY" -z "$D" -c "$C" "$url" 2>&1)
  rps=$(echo "$out" | awk '/Requests\/sec/ {printf "%.0f", $2}')
  p95=$(echo "$out" | awk '/95%% in/ {printf "%.1f", $3*1000}')
  p99=$(echo "$out" | awk '/99%% in/ {printf "%.1f", $3*1000}')
  total=$(echo "$out" | awk '/\[[0-9]+\]/ {gsub(/[^0-9 ]/,"",$0); split($0,a," "); t+=a[2]} END {print t+0}')
  errs=$(echo "$out" | awk '/\[[45][0-9][0-9]\]/ {gsub(/[^0-9 ]/,"",$0); split($0,a," "); e+=a[2]} END {print e+0}')
  route=$(echo "$path" | sed 's/?.*//; s/{HOT}/{fid}/; s/{THREAD}/{tid}/; s/{MEGA}/{tid}/; s/{USER}/{uid}/')
  queries=$(metric_avg rbb_db_queries_per_request "$route")
  printf "%-16s %8s %8s %8s %4s/%-5s %8s\n" "$name" "$rps" "$p95" "$p99" "$errs" "$total" "$queries"
  awk -v a="$p95" -v b="$p95b" 'BEGIN { exit !(a > b) }' && bad "$name p95 ${p95}ms > ${p95b}ms"
  awk -v a="$p99" -v b="$p99b" 'BEGIN { exit !(a > b) }' && bad "$name p99 ${p99}ms > ${p99b}ms"
  awk -v e="$errs" -v t="$total" -v m="${MAX_ERROR_RATE:-0.001}" 'BEGIN { exit !(t == 0 || e / t > m) }' && bad "$name error rate $errs/$total"
  awk -v a="$queries" -v b="$qb" 'BEGIN { exit !(a > b) }' && bad "$name ${queries} statements per request > $qb"
done < "$(dirname "$0")/load_budgets.txt"

echo "== resources"
wait_ms=$(curl -s "$METRICS" | awk '/^rbb_db_pool_acquire_seconds_sum/ {s=$2} /^rbb_db_pool_acquire_seconds_count/ {c=$2} END { if (c>0) printf "%.2f", s/c*1000; else print "0" }')
rss=$(( $(ps -o rss= -p "$PID") / 1024 ))
echo "  pool acquire wait (avg): ${wait_ms} ms; server memory: ${rss} MB"
awk -v a="$wait_ms" -v b="${MAX_POOL_WAIT_MS:-20}" 'BEGIN { exit !(a > b) }' && bad "pool wait ${wait_ms}ms"
[ "$rss" -gt "${MAX_RSS_MB:-512}" ] && bad "memory ${rss} MB > ${MAX_RSS_MB:-512} MB"

echo "== overload (${OVERLOAD_CONCURRENCY:-400} concurrent clients)"
out=$("$HEY" -z 10s -c "${OVERLOAD_CONCURRENCY:-400}" "$BASE/thread/$THREAD" 2>&1)
dist=$(echo "$out" | awk '/Status code distribution/,0' | grep -o '\[[0-9]*\][^[]*responses' | tr -s ' ' | paste -sd' ' -)
p99=$(echo "$out" | awk '/99%% in/ {printf "%.0f", $3*1000}')
echo "  $dist; p99 ${p99} ms"
echo "$out" | grep -qE '\[5(0[0-2]|0[4-9]|[1-9][0-9])\]' && bad "server errors under overload (only 429/503 are acceptable)"
kill -0 "$PID" 2>/dev/null || bad "the server died under overload"
curl -fsS "$BASE/livez" >/dev/null || bad "not alive after overload"

if [ $fail -ne 0 ]; then echo "budgets exceeded"; exit 1; fi
echo "all budgets met"
