#!/usr/bin/env bash
# Soft-deleted content must stay hidden from members and guests (they may only see the
# "This post was deleted." placeholder for a deleted reply in a live thread); moderators see it.
# Usage: tests/deleted_visibility.sh [base_url] [admin_password]
set -uo pipefail
BASE="${1:-http://127.0.0.1:8088}"; PW="${2:-admin12345}"
PSQL="${PSQL:-/opt/homebrew/opt/postgresql@17/bin/psql}"; DB="${DATABASE_URL:-postgres://rbb@127.0.0.1:5433/rbb}"
ADMIN=$(mktemp); MEMBER=$(mktemp); BODY=$(mktemp); fail=0; N=$RANDOM; F=4
ok() { echo "  ok   $1"; }; bad() { echo "  FAIL $1"; fail=1; }
sql() { "$PSQL" "$DB" -Atqc "$1"; }
csrf() { curl -s -b "$1" -c "$1" "$BASE$2" | sed -n 's/.*name="csrf-token" content="\([^"]*\)".*/\1/p' | head -1; }
login() { local t; t=$(csrf "$1" /member/login); curl -s -o /dev/null -b "$1" -c "$1" --data-urlencode "my_post_key=$t" --data-urlencode "username=$2" --data-urlencode "password=$3" "$BASE/member/login"; }
# hidden NAME URL: neither the member nor a guest may see the secret text there
hidden() { for who in member guest; do local jar=$MEMBER; [ $who = guest ] && jar=/dev/null
  curl -s -L -o "$BODY" -b "$jar" "$BASE$2"
  if grep -q "DELSUBJ$N\|DELBODY$N" "$BODY"; then bad "$1 ($who)"; else ok "$1 ($who)"; fi; done; }

login "$ADMIN" admin "$PW"; T=$(csrf "$ADMIN" /)
curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$T" --data-urlencode "password=$PW" --data-urlencode "return_to=/admin" "$BASE/admin/verify"
newthread() { curl -s -o /dev/null -D - -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$T" --data-urlencode "subject=$1" --data-urlencode "message=$2" "$BASE/newthread/$F" | tr -d '\r' | sed -n 's|^location: /thread/\([0-9]*\).*|\1|Ip'; }
# A deleted thread, and a live thread with one deleted reply.
DT=$(newthread "DELSUBJ$N" "DELBODY$N"); DP=$(sql "SELECT firstpost FROM threads WHERE tid = $DT")
curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$T" --data-urlencode "fid=$F" --data-urlencode "action=softdelete" --data-urlencode "tids=$DT" "$BASE/moderation/threads"
LT=$(newthread "Live thread $N" "Visible first post")
sql "UPDATE users SET lastpost = 0 WHERE username = 'admin'"
curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$T" --data-urlencode "message=DELBODY$N reply" "$BASE/newreply/$LT"
RP=$(sql "SELECT max(pid) FROM posts WHERE tid = $LT")
curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$T" --data-urlencode "tid=$LT" --data-urlencode "action=softdelete" --data-urlencode "pids=$RP" "$BASE/moderation/posts"
[ "$(sql "SELECT visible FROM threads WHERE tid = $DT")/$(sql "SELECT visible FROM posts WHERE pid = $RP")" = "-1/-1" ] && ok "setup: thread $DT and reply $RP soft-deleted" || { bad "setup"; exit 1; }
T2=$(csrf "$ADMIN" /admin)
curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$T2" --data-urlencode "username=delvis$N" --data-urlencode "password=secret$N" --data-urlencode "email=delvis$N@example.com" --data-urlencode "usergroup=2" "$BASE/admin/users/new"
login "$MEMBER" "delvis$N" "secret$N"

echo "# Deleted thread is invisible"
for u in "/thread/$DT" "/thread/$DT/print" "/thread/$DT/whoposted" "/thread/$DT/lastpost" "/thread/$DT/newpost" "/post/$DP" "/archive/thread/$DT" "/forum/$F" "/archive/forum/$F" "/api/v1/posts/$DP" "/api/v1/threads/$DT" "/api/v1/forums/$F/threads"; do hidden "$u" "$u"; done
code=$(curl -s -o /dev/null -w "%{http_code}" -b "$MEMBER" "$BASE/thread/$DT"); [ "$code" = 404 ] && ok "member gets 404 on deleted thread" || bad "member gets 404 on deleted thread (got $code)"

echo "# Deleted reply shows only the placeholder"
hidden "live thread hides deleted reply body" "/thread/$LT"
hidden "API thread hides deleted reply" "/api/v1/threads/$LT"
hidden "API post hides deleted reply" "/api/v1/posts/$RP"
hidden "print view hides deleted reply" "/thread/$LT/print"
curl -s -b "$MEMBER" "$BASE/thread/$LT" -o "$BODY"; grep -q "This post was deleted." "$BODY" && ok "member sees the deletion notice" || bad "member sees the deletion notice"
grep -q "Visible first post" "$BODY" && ok "live content still shown" || bad "live content still shown"

echo "# Moderators still see deleted content"
curl -s -b "$ADMIN" "$BASE/thread/$DT" -o "$BODY"; grep -q "DELBODY$N" "$BODY" && ok "admin opens deleted thread" || bad "admin opens deleted thread"
curl -s -b "$ADMIN" "$BASE/thread/$LT" -o "$BODY"; grep -q "DELBODY$N reply" "$BODY" && ok "admin sees deleted reply" || bad "admin sees deleted reply"
curl -s -b "$ADMIN" "$BASE/forum/$F" -o "$BODY"; grep -q "DELSUBJ$N" "$BODY" && ok "admin sees deleted thread in forum" || bad "admin sees deleted thread in forum"

echo "# Cleanup"
for t in "$DT" "$LT"; do curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$T" --data-urlencode "fid=$F" --data-urlencode "action=delete" --data-urlencode "tids=$t" "$BASE/moderation/threads"; done
MU=$(sql "SELECT uid FROM users WHERE username = 'delvis$N'"); curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$T2" "$BASE/admin/users/$MU/delete"
rm -f "$ADMIN" "$MEMBER" "$BODY"
[ $fail = 0 ] && echo "ALL OK" || { echo "SOME CHECKS FAILED"; exit 1; }
