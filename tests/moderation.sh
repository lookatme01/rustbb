#!/usr/bin/env bash
# Exercises every moderation state change and verifies all counters after each step.
# Usage: tests/moderation.sh [base_url] [rbb binary]
set -uo pipefail
BASE="${1:-http://127.0.0.1:8088}"; RBB="${2:-./target/debug/rbb}"
JAR=$(mktemp); fail=0
ok() { echo "  ok   $1"; }; bad() { echo "  FAIL $1"; fail=1; }
csrf() { curl -s -b "$JAR" -c "$JAR" "$BASE${1:-/}" | sed -n 's/.*name="csrf-token" content="\([^"]*\)".*/\1/p' | head -1; }
post() { curl -s -o /dev/null -w "%{http_code}" -b "$JAR" -c "$JAR" --data-urlencode "my_post_key=$T" "$@"; }
verify() { if out=$($RBB check 2>&1); then ok "$1: counters consistent"; else bad "$1: $out"; fi; }
T=$(csrf /member/login)
curl -s -o /dev/null -b "$JAR" -c "$JAR" -d "my_post_key=$T&username=admin&password=admin12345" "$BASE/member/login"
T=$(csrf /)
mk_thread() { curl -s -o /dev/null -D - -b "$JAR" -c "$JAR" --data-urlencode "my_post_key=$T" --data-urlencode "subject=$1" --data-urlencode "message=Body of $1" "$BASE/newthread/$2" | tr -d '\r' | sed -n 's|^location: /thread/\([0-9]*\).*|\1|Ip'; }
reply() { post --data-urlencode "message=$2" "$BASE/newreply/$1" >/dev/null; }
pids() { curl -s -b "$JAR" "$BASE/thread/$1?page=1" | grep -o 'id="pid[0-9]*"' | grep -o '[0-9]*'; }
modposts() { local tid=$1 action=$2; shift 2; local args=(); for p in "$@"; do args+=(--data-urlencode "pids=$p"); done; post --data-urlencode "tid=$tid" --data-urlencode "action=$action" "${args[@]}" "$BASE/moderation/posts" >/dev/null; }
modthreads() { local fid=$1 action=$2; shift 2; local args=(); for t in "$@"; do args+=(--data-urlencode "tids=$t"); done; post --data-urlencode "fid=$fid" --data-urlencode "action=$action" "${args[@]}" "$BASE/moderation/threads" >/dev/null; }

A=$(mk_thread "Moderation A $RANDOM" 3); for i in 1 2 3 4 5; do reply $A "A reply $i"; done
B=$(mk_thread "Moderation B $RANDOM" 3); for i in 1 2 3; do reply $B "B reply $i"; done
echo "threads A=$A B=$B"; verify "setup"
P=($(pids $A)); echo "A posts: ${P[*]}"
modposts $A softdelete ${P[1]} ${P[2]}; verify "soft delete replies"
modposts $A restore ${P[1]}; verify "restore reply"
modposts $A unapprove ${P[3]}; verify "unapprove reply"
modposts $A approve ${P[3]}; verify "approve reply"
modthreads 3 softdelete $B; verify "soft delete thread"
modthreads 3 restore $B; verify "restore thread"
modthreads 3 unapprove $B; verify "unapprove thread"
reply $A "reply while B unapproved"; verify "reply elsewhere"
modthreads 3 approve $B; verify "approve thread"
post --data-urlencode "tids=$B" --data-urlencode "target=4" --data-urlencode "method=redirect" --data-urlencode "redirect_days=7" "$BASE/moderation/move" >/dev/null; verify "move with redirect"
post --data-urlencode "tids=$B" --data-urlencode "target=3" --data-urlencode "method=copy" "$BASE/moderation/move" >/dev/null; verify "copy thread"
P=($(pids $A))
post --data-urlencode "tid=$A" --data-urlencode "pids=${P[3]}" --data-urlencode "pids=${P[4]}" --data-urlencode "subject=Split off $RANDOM" --data-urlencode "fid=6" "$BASE/moderation/split" >/dev/null; verify "split posts"
post --data-urlencode "tid=$A" --data-urlencode "target=$B" "$BASE/moderation/merge" >/dev/null; verify "merge threads"
P=($(pids $A)); modposts $A merge ${P[1]} ${P[2]}; verify "merge posts"
P=($(pids $A)); modposts $A delete ${P[${#P[@]}-1]}; verify "hard delete reply"
C=$(mk_thread "Moderation C $RANDOM" 3); reply $C "c1"; reply $C "c2"
P=($(pids $C)); post --data-urlencode "tid=$C" --data-urlencode "pids=${P[1]}" --data-urlencode "target=$A" "$BASE/moderation/moveposts" >/dev/null; verify "move posts"
modthreads 3 delete $C; verify "hard delete thread"
P=($(pids $A)); post "$BASE/deletepost/${P[1]}" >/dev/null; verify "author soft delete via deletepost"
rm -f "$JAR"
[ $fail = 0 ] && echo "ALL OK" || { echo "SOME CHECKS FAILED"; exit 1; }
