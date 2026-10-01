#!/usr/bin/env bash
# End-to-end smoke test against a running server. Usage: tests/smoke.sh [base_url]
set -uo pipefail
BASE="${1:-http://127.0.0.1:8088}"
JAR=$(mktemp); JAR2=$(mktemp)
fail=0
ok() { echo "  ok   $1"; }
bad() { echo "  FAIL $1"; fail=1; }
csrf() { curl -s -b "$1" -c "$1" "$BASE$2" | sed -n 's/.*name="csrf-token" content="\([^"]*\)".*/\1/p' | head -1; }
check() { # name, expected-code, curl args...
  local name="$1" want="$2"; shift 2
  local code; code=$(curl -s -o /tmp/smoke_body -w "%{http_code}" "$@")
  if [ "$code" = "$want" ]; then ok "$name ($code)"; else bad "$name (got $code want $want)"; head -c 400 /tmp/smoke_body; echo; fi
}
contains() { if grep -q "$2" /tmp/smoke_body; then ok "$1"; else bad "$1 (missing '$2')"; fi }

echo "== guest pages"
for p in / /forum/2 /thread/1 /thread/1/whoposted /members /search /online /stats /portal /calendar /help /rules /smilies /mycode /archive /syndication /sitemap.xml /robots.txt /team /api/v1/forums /member/login /member/register; do
  check "GET $p" 200 -b "$JAR" -c "$JAR" "$BASE$p"
done
check "404 thread" 404 "$BASE/thread/999999"
check "legacy showthread redirect" 308 "$BASE/showthread.php?tid=1"

echo "== admin login"
T=$(csrf "$JAR" /member/login)
check "login" 303 -b "$JAR" -c "$JAR" -d "my_post_key=$T&username=admin&password=admin12345&remember=1" "$BASE/member/login"
check "index as admin" 200 -b "$JAR" -c "$JAR" "$BASE/"
contains "welcome shown" "Welcome back, admin"
T=$(csrf "$JAR" /)
check "csrf rejected" 403 -b "$JAR" -c "$JAR" -d "my_post_key=bad&subject=x&message=y" "$BASE/newthread/3"

echo "== posting"
check "new thread" 303 -b "$JAR" -c "$JAR" --data-urlencode "my_post_key=$T" --data-urlencode "subject=Smoke test thread" --data-urlencode "message=Hello [b]world[/b] :) https://example.com" --data-urlencode "includesig=1" "$BASE/newthread/3"
TID=$(curl -s -b "$JAR" "$BASE/forum/3" | grep -o 'data-tid="[0-9]*"' | head -1 | grep -o '[0-9]*')
echo "  tid=$TID"
check "view thread" 200 -b "$JAR" "$BASE/thread/$TID"
contains "mycode rendered" "<strong class=\"mycode_b\">world</strong>"
contains "smilie rendered" "class=\"smilie\""
check "reply" 303 -b "$JAR" -c "$JAR" --data-urlencode "my_post_key=$T" --data-urlencode "message=A reply with @admin mention" "$BASE/newreply/$TID"
check "thread after reply" 200 -b "$JAR" "$BASE/thread/$TID"
contains "mention linked" "mycode_mention"
PID=$(grep -o 'id="pid[0-9]*"' /tmp/smoke_body | tail -1 | grep -o '[0-9]*')
check "edit form" 200 -b "$JAR" "$BASE/editpost/$PID"
check "edit post" 303 -b "$JAR" -c "$JAR" --data-urlencode "my_post_key=$T" --data-urlencode "message=Edited reply text" --data-urlencode "editreason=typo" "$BASE/editpost/$PID"
check "quote json" 200 -b "$JAR" "$BASE/post/$PID/quote"
check "preview" 200 -b "$JAR" --data-urlencode "my_post_key=$T" --data-urlencode "message=[i]x[/i]" "$BASE/preview"
check "search" 303 -b "$JAR" -c "$JAR" --data-urlencode "my_post_key=$T" --data-urlencode "keywords=smoke" "$BASE/search"
check "quick search" 303 -b "$JAR" "$BASE/search/quick?q=smoke"
check "print view" 200 -b "$JAR" "$BASE/thread/$TID/print"
check "rss" 200 "$BASE/syndication?type=atom"

echo "== register second user"
T2=$(csrf "$JAR2" /member/register)
FT=$(curl -s -b "$JAR2" -c "$JAR2" "$BASE/member/register" | sed -n 's/.*name="formtoken" value="\([^"]*\)".*/\1/p' | head -1)
CAPH=$(sed -n 's/.*name="captcha_hash" value="\([^"]*\)".*/\1/p' /dev/null)
T2=$(csrf "$JAR2" /member/register)
REG=$(curl -s -b "$JAR2" -c "$JAR2" "$BASE/member/register")
FT=$(echo "$REG" | sed -n 's/.*name="formtoken" value="\([^"]*\)".*/\1/p' | head -1)
CAPH=$(echo "$REG" | sed -n 's/.*name="captcha_hash" value="\([^"]*\)".*/\1/p' | head -1)
T2=$(echo "$REG" | sed -n 's/.*name="csrf-token" content="\([^"]*\)".*/\1/p' | head -1)
CODE=$(psql -h 127.0.0.1 -p 5433 -U rbb rbb -tAc "SELECT imagestring FROM captcha WHERE imagehash='$CAPH'" 2>/dev/null || /opt/homebrew/opt/postgresql@17/bin/psql -h 127.0.0.1 -p 5433 -U rbb rbb -tAc "SELECT imagestring FROM captcha WHERE imagehash='$CAPH'")
# Previous runs count toward the per-IP registration limit; age their accounts out of the window.
PSQL_BIN=$(command -v psql || echo /opt/homebrew/opt/postgresql@17/bin/psql)
"$PSQL_BIN" -h 127.0.0.1 -p 5433 -U rbb rbb -qtAc "UPDATE users SET regdate = regdate - 172800 WHERE (username LIKE 'smoke%' OR username LIKE 'man%') AND regdate > extract(epoch from now())::bigint - 86400" >/dev/null 2>&1 || true
sleep 3
U="smoke$RANDOM"
check "register" 303 -b "$JAR2" -c "$JAR2" --data-urlencode "my_post_key=$T2" --data-urlencode "formtoken=$FT" --data-urlencode "username=$U" --data-urlencode "password=Passw0rd!x" --data-urlencode "password2=Passw0rd!x" --data-urlencode "email=$U@example.com" --data-urlencode "email2=$U@example.com" --data-urlencode "agree=1" --data-urlencode "captcha_hash=$CAPH" --data-urlencode "captcha=$CODE" "$BASE/member/register"
check "user cp" 200 -b "$JAR2" "$BASE/usercp"
T2=$(csrf "$JAR2" /usercp)
for p in /usercp/profile /usercp/options /usercp/avatar /usercp/signature /usercp/security /usercp/lists /usercp/alerts /usercp/subscriptions /usercp/drafts /usercp/attachments /usercp/usergroups /usercp/notepad /pm /pm/send /pm/folders; do
  check "GET $p" 200 -b "$JAR2" "$BASE$p"
done
check "send pm" 303 -b "$JAR2" -c "$JAR2" --data-urlencode "my_post_key=$T2" --data-urlencode "to=admin" --data-urlencode "subject=Hi" --data-urlencode "message=Hello admin" --data-urlencode "savecopy=1" "$BASE/pm/send"
check "react" 200 -b "$JAR2" -H "Accept: application/json" --data-urlencode "my_post_key=$T2" --data-urlencode "kind=like" "$BASE/post/$PID/react"
check "report" 303 -b "$JAR2" --data-urlencode "my_post_key=$T2" --data-urlencode "type=post" --data-urlencode "id=$PID" --data-urlencode "reason=1" "$BASE/report"
check "save options" 303 -b "$JAR2" --data-urlencode "my_post_key=$T2" --data-urlencode "showsigs=1" --data-urlencode "timezone=Europe/Berlin" "$BASE/usercp/options"
check "profile page" 200 -b "$JAR2" "$BASE/user/1"

echo "== admin inbox + modcp"
check "admin pm inbox" 200 -b "$JAR" "$BASE/pm"
contains "pm received" "Hi"
check "admin thread view" 200 -b "$JAR" "$BASE/thread/1"
contains "admin debug panel" 'class="devbar'
check "account activity" 200 -b "$JAR" "$BASE/usercp/activity"
contains "audit records sign-in" "Signed in"
check "guest has no debug panel" 200 "$BASE/thread/1"
if grep -q "devbar" /tmp/smoke_body; then bad "debug panel leaked to guest"; else ok "debug panel hidden from guests"; fi
for p in /modcp /modcp/reports /modcp/modqueue /modcp/modlogs /modcp/announcements /modcp/banning /modcp/ipsearch?ip=127.0.0.1 /modcp/warninglogs /usercp/alerts; do
  check "GET $p" 200 -b "$JAR" "$BASE$p"
done
T=$(csrf "$JAR" /)
check "close thread" 303 -b "$JAR" --data-urlencode "my_post_key=$T" --data-urlencode "action=close" "$BASE/moderation/thread/$TID"
check "stick thread" 303 -b "$JAR" --data-urlencode "my_post_key=$T" --data-urlencode "action=stick" "$BASE/moderation/thread/$TID"
check "api me (cookie)" 200 -b "$JAR" "$BASE/api/v1/me"
TOK=$(curl -s -H 'Content-Type: application/json' -d '{"username":"admin","password":"admin12345"}' "$BASE/api/v1/auth/token" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
check "api token me" 200 -H "Authorization: Bearer $TOK" "$BASE/api/v1/me"
check "api reply" 200 -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' -d '{"message":"reply via API"}' "$BASE/api/v1/threads/1/posts"
check "api thread" 200 "$BASE/api/v1/threads/1"
rm -f "$JAR" "$JAR2"
[ $fail = 0 ] && echo "ALL OK" || { echo "SOME CHECKS FAILED"; exit 1; }
