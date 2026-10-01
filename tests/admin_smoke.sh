#!/usr/bin/env bash
# Admin CP smoke test. Usage: tests/admin_smoke.sh [base_url] [admin_password]
set -uo pipefail
BASE="${1:-http://127.0.0.1:8088}"; PW="${2:-admin12345}"
JAR=$(mktemp); fail=0
ok() { echo "  ok   $1"; }; bad() { echo "  FAIL $1"; fail=1; }
csrf() { curl -s -b "$JAR" -c "$JAR" "$BASE$1" | sed -n 's/.*name="csrf-token" content="\([^"]*\)".*/\1/p' | head -1; }
check() { local name="$1" want="$2"; shift 2; local code; code=$(curl -s -o /tmp/asmoke_body -w "%{http_code}" -b "$JAR" -c "$JAR" "$@"); if [ "$code" = "$want" ]; then ok "$name ($code)"; else bad "$name (got $code want $want)"; grep -o '<p>[^<]*</p>' /tmp/asmoke_body | head -2; fi; }
T=$(csrf /member/login)
check "login" 303 -d "my_post_key=$T&username=admin&password=$PW&remember=1" "$BASE/member/login"
check "acp redirects to verify" 303 "$BASE/admin"
T=$(csrf /)
check "verify" 303 --data-urlencode "my_post_key=$T" --data-urlencode "password=$PW" --data-urlencode "return_to=/admin" "$BASE/admin/verify"
for p in /admin /admin/settings /admin/settings/general /admin/settings/posting "/admin/settings?q=flood" /admin/forums /admin/forums/edit "/admin/forums/edit?fid=3" /admin/forums/3/permissions /admin/forums/3/permissions/2 /admin/forums/3/moderators \
  /admin/users /admin/users/new /admin/users/1 /admin/users/awaiting /admin/users/merge /admin/adminperms /admin/groups /admin/groups/edit "/admin/groups/edit?gid=2" /admin/groups/2/leaders \
  /admin/themes "/admin/themes/edit?tid=2" /admin/themes/1/templates "/admin/themes/2/template?name=index.html" /admin/themes/1/export \
  /admin/tools /admin/tools/tasks /admin/tools/recount /admin/tools/cache /admin/tools/adminlog /admin/tools/maillogs /admin/tools/mailerrors /admin/tools/spamlog /admin/tools/stats /admin/tools/backup /admin/tools/plugins /admin/tools/attachments \
  /admin/massmail /admin/massmail/new /admin/promotions /admin/promotions/edit /admin/promotions/logs; do
  check "GET $p" 200 "$BASE$p"
done
for t in smilies icons badwords mycode attachtypes profilefields prefixes reportreasons questions helpsections helpdocs calendars warningtypes warninglevels usertitles modtools banfilters; do
  check "crud list $t" 200 "$BASE/admin/crud/$t"; check "crud edit $t" 200 "$BASE/admin/crud/$t/edit"
done
T=$(csrf /admin)
check "save settings" 303 --data-urlencode "my_post_key=$T" --data-urlencode "bbname=rbb Test Board" --data-urlencode "bburl=$BASE" --data-urlencode "homename=Home" --data-urlencode "homeurl=/" --data-urlencode "adminemail=admin@example.com" --data-urlencode "boardclosed=0" --data-urlencode "debugpanel=1" --data-urlencode "seourls=1" --data-urlencode "gzipoutput=1" --data-urlencode "boardclosed_reason=x" --data-urlencode "tos=Be nice." --data-urlencode "privacypolicy=We keep data." "$BASE/admin/settings/general"
check "add prefix" 303 --data-urlencode "my_post_key=$T" --data-urlencode "prefix=Question" --data-urlencode "displaystyle=" "$BASE/admin/crud/prefixes/save"
check "add mycode" 303 --data-urlencode "my_post_key=$T" --data-urlencode "title=Highlight" --data-urlencode 'regex=\[hl\](.*?)\[/hl\]' --data-urlencode 'replacement=<mark>$1</mark>' --data-urlencode "active=1" --data-urlencode "parseorder=1" "$BASE/admin/crud/mycode/save"
check "bad regex rejected" 422 --data-urlencode "my_post_key=$T" --data-urlencode "title=x" --data-urlencode 'regex=(' --data-urlencode 'replacement=x' "$BASE/admin/crud/mycode/save"
check "add forum" 303 --data-urlencode "my_post_key=$T" --data-urlencode "name=Test Forum" --data-urlencode "type=f" --data-urlencode "pid=5" --data-urlencode "disporder=5" --data-urlencode "active=1" --data-urlencode "open=1" --data-urlencode "allowmycode=1" --data-urlencode "allowsmilies=1" --data-urlencode "showinjump=1" "$BASE/admin/forums/edit"
check "forum perms matrix save" 303 --data-urlencode "my_post_key=$T" --data-urlencode "custom_1=1" --data-urlencode "p_1_canview=1" "$BASE/admin/forums/3/permissions"
check "add group" 303 --data-urlencode "my_post_key=$T" --data-urlencode "title=VIP" --data-urlencode "namestyle=<b>{username}</b>" --data-urlencode "type=3" --data-urlencode "canview=1" "$BASE/admin/groups/edit"
check "template save" 303 --data-urlencode "my_post_key=$T" --data-urlencode "name=board_closed.html" --data-urlencode 'template={% extends "layout.html" %}{% block content %}<p>Closed!</p>{% endblock %}' "$BASE/admin/themes/2/template"
check "template syntax error" 200 --data-urlencode "my_post_key=$T" --data-urlencode "name=board_closed.html" --data-urlencode 'template={% if %}' "$BASE/admin/themes/2/template"
grep -q "syntax error" /tmp/asmoke_body && ok "syntax error shown" || bad "syntax error not shown"
check "run task" 303 --data-urlencode "my_post_key=$T" --data-urlencode "tid=1" --data-urlencode "action=run" "$BASE/admin/tools/tasks"
check "recount forums" 303 --data-urlencode "my_post_key=$T" --data-urlencode "what=forums" "$BASE/admin/tools/recount"
check "cache reload" 303 --data-urlencode "my_post_key=$T" --data-urlencode "part=all" "$BASE/admin/tools/cache"
check "index after changes" 200 "$BASE/"
grep -q "Test Forum" /tmp/asmoke_body && ok "new forum visible" || bad "new forum not visible"
rm -f "$JAR"
[ $fail = 0 ] && echo "ALL OK" || { echo "SOME CHECKS FAILED"; exit 1; }
