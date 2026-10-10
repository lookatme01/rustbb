#!/usr/bin/env bash
# System account regression test. Usage: tests/system.sh [base_url] [admin_password]
set -uo pipefail
BASE="${1:-http://127.0.0.1:8088}"; PW="${2:-admin12345}"
JAR=$(mktemp); BODY=$(mktemp); fail=0
ok() { echo "  ok   $1"; }; bad() { echo "  FAIL $1"; fail=1; }
csrf() { curl -s -b "$JAR" -c "$JAR" "$BASE$1" | sed -n 's/.*name="csrf-token" content="\([^"]*\)".*/\1/p' | head -1; }
check() { local name="$1" want="$2"; shift 2; local code; code=$(curl -s -o "$BODY" -w "%{http_code}" -b "$JAR" -c "$JAR" "$@"); if [ "$code" = "$want" ]; then ok "$name ($code)"; else bad "$name (got $code want $want)"; grep -o '<p>[^<]*</p>' "$BODY" | head -2; fi; }
has() { grep -q "$2" "$BODY" && ok "$1" || bad "$1"; }

# Anonymous: System is visible and online.
SYS=$(curl -s -o /dev/null -w "%{redirect_url}" "$BASE/user/name/System" | sed -n 's#.*/user/\([0-9]*\).*#\1#p')
[ -n "$SYS" ] && ok "System profile exists (uid $SYS)" || { bad "System profile not found"; exit 1; }
check "System profile" 200 "$BASE/user/$SYS"
has "profile shows online" 'Online now'
has "profile shows bot badge" 'name-system'
has "profile shows activity" 'Keeping the board running'
check "index" 200 "$BASE/"
has "index lists System online" 'name-system'
check "who's online" 200 "$BASE/online"
has "who's online lists System" 'Keeping the board running'

# System can never sign in (and doesn't accrue lockouts).
T=$(csrf /member/login)
check "web login refused" 401 -d "my_post_key=$T&username=System&password=!" "$BASE/member/login"
has "generic login error" 'invalid username'
code=$(curl -s -o /dev/null -w "%{http_code}" -H 'Content-Type: application/json' -d '{"username":"System","password":"!"}' "$BASE/api/v1/auth/token")
[ "$code" = 403 ] && ok "api token refused ($code)" || bad "api token (got $code want 403)"

# Admin: cosmetic edits work, destructive actions are refused.
T=$(csrf /member/login)
check "admin login" 303 -d "my_post_key=$T&username=admin&password=$PW&remember=1" "$BASE/member/login"
T=$(csrf /)
check "acp verify" 303 --data-urlencode "my_post_key=$T" --data-urlencode "password=$PW" --data-urlencode "return_to=/admin" "$BASE/admin/verify"
check "edit form" 200 "$BASE/admin/users/$SYS?tab=details"
has "edit form explains System" 'built-in System account'
grep -q 'name="email"' "$BODY" && bad "email field hidden" || ok "email field hidden"
T=$(csrf /admin)
check "cosmetic edit saved" 303 --data-urlencode "my_post_key=$T" --data-urlencode "username=System" --data-urlencode "usertitle=Board robot" \
  --data-urlencode "email=evil@example.com" --data-urlencode "usergroup=2" --data-urlencode "newpassword=" --data-urlencode "suspendposting=1" --data-urlencode "suspendposting_days=7" "$BASE/admin/users/$SYS"
check "profile after edit" 200 "$BASE/user/$SYS"
has "user title changed" 'Board robot'
has "still in System group" 'name-system'
check "password refused" 422 --data-urlencode "my_post_key=$T" --data-urlencode "username=System" --data-urlencode "newpassword=hunter22" "$BASE/admin/users/$SYS"
check "delete refused" 422 --data-urlencode "my_post_key=$T" "$BASE/admin/users/$SYS/delete"
has "delete message" 'System account cannot be deleted'
check "ban refused" 422 --data-urlencode "my_post_key=$T" --data-urlencode "reason=x" --data-urlencode "days=1" "$BASE/admin/users/$SYS/ban"
check "merge refused" 422 --data-urlencode "my_post_key=$T" --data-urlencode "source=System" --data-urlencode "destination=admin" "$BASE/admin/users/merge"
SGID=$(curl -s -b "$JAR" "$BASE/admin/users/$SYS?tab=details" | grep -o '<option value="[0-9]*"[^>]*>System<' | head -1 | sed 's/[^0-9]//g')
[ -n "$SGID" ] && ok "System group found (gid $SGID)" || bad "System group not found"
check "put member in System group refused" 422 --data-urlencode "my_post_key=$T" --data-urlencode "username=sysprobe$RANDOM" --data-urlencode "password=secret123" \
  --data-urlencode "email=probe$RANDOM@example.com" --data-urlencode "usergroup=${SGID:-x}" "$BASE/admin/users/new"
check "pm to System refused" 200 --data-urlencode "my_post_key=$T" --data-urlencode "to=System" --data-urlencode "subject=Hi" --data-urlencode "message=Hello" "$BASE/pm/send"
has "pm error shown" 'cannot receive private messages'
check "restore title" 303 --data-urlencode "my_post_key=$T" --data-urlencode "username=System" --data-urlencode "usertitle=" "$BASE/admin/users/$SYS"

rm -f "$JAR" "$BODY"
[ $fail = 0 ] && echo "ALL OK" || { echo "SOME CHECKS FAILED"; exit 1; }
