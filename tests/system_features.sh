#!/usr/bin/env bash
# System account features: automated sender, staff speaking as System, automation, profile.
# Usage: tests/system_features.sh [base_url] [admin_password]
# Needs a local psql (PSQL, DATABASE_URL) to backdate rows and check what was stored.
set -uo pipefail
BASE="${1:-http://127.0.0.1:8088}"; PW="${2:-admin12345}"
PSQL="${PSQL:-/opt/homebrew/opt/postgresql@17/bin/psql}"; DB="${DATABASE_URL:-postgres://rbb@127.0.0.1:5433/rbb}"
ADMIN=$(mktemp); MEMBER=$(mktemp); BODY=$(mktemp); fail=0; N=$RANDOM
ok() { echo "  ok   $1"; }; bad() { echo "  FAIL $1"; fail=1; }
sql() { "$PSQL" "$DB" -Atqc "$1"; }
csrf() { curl -s -b "$1" -c "$1" "$BASE$2" | sed -n 's/.*name="csrf-token" content="\([^"]*\)".*/\1/p' | head -1; }
# req JAR NAME WANT_CODE curl-args…  (body lands in $BODY)
req() { local jar="$1" name="$2" want="$3"; shift 3; local code; code=$(curl -s -o "$BODY" -w "%{http_code}" -b "$jar" -c "$jar" "$@"); if [ "$code" = "$want" ]; then ok "$name ($code)"; else bad "$name (got $code want $want)"; grep -o '<p>[^<]*</p>' "$BODY" | head -2; fi; }
has() { grep -q -- "$2" "$BODY" && ok "$1" || bad "$1"; }
hasnt() { grep -q -- "$2" "$BODY" && bad "$1" || ok "$1"; }
location() { tr -d '\r' | sed -n 's|^location: \(.*\)|\1|Ip' | head -1; }
login() { local jar="$1" user="$2" pass="$3" t; t=$(csrf "$jar" /member/login); curl -s -o /dev/null -b "$jar" -c "$jar" --data-urlencode "my_post_key=$t" --data-urlencode "username=$user" --data-urlencode "password=$pass" "$BASE/member/login"; }

SYS=$(sql "SELECT uid FROM users WHERE is_system"); SYSNAME=$(sql "SELECT username FROM users WHERE is_system")
[ -n "$SYS" ] && ok "System is uid $SYS" || { bad "no System account"; exit 1; }

echo "# Profile"
req /dev/null "System profile (guest)" 200 "$BASE/user/$SYS"
hasnt "time online isn't '(Hidden)'" '<div>Time online</div><div>(Hidden)'
grep -o '<div>Time online</div><div>[^<]*' "$BODY" | grep -qE '[0-9]+ (day|hour|minute)' && ok "time online is a real duration" || bad "time online is a real duration"
has "activity box shown" 'Inactive threads closed'
hasnt "guests don't see the moderator log" 'Latest actions'

echo "# Admin setup"
login "$ADMIN" admin "$PW"
T=$(csrf "$ADMIN" /)
req "$ADMIN" "acp verify" 303 --data-urlencode "my_post_key=$T" --data-urlencode "password=$PW" --data-urlencode "return_to=/admin" "$BASE/admin/verify"
T=$(csrf "$ADMIN" /admin)
req "$ADMIN" "enable welcome message + autoclose" 303 --data-urlencode "my_post_key=$T" --data-urlencode "system_welcome_pm=1" \
  --data-urlencode "system_welcome_subject=Welcome to {boardname}, {username}" --data-urlencode "system_welcome_message=Hello [b]{username}[/b], welcome to {boardname}." \
  --data-urlencode "system_autoclose_days=30" --data-urlencode "system_autoclose_forums=3" --data-urlencode "system_log_expiries=1" "$BASE/admin/settings/system"
req "$ADMIN" "admin sees System's latest actions" 200 "$BASE/user/$SYS"
has "latest actions shown to staff" 'Latest actions'

echo "# Welcome message (admin activation), with a hostile username"
MNAME="sysmember$N"; EVIL="[b]ev$N[/b]"
for u in "$MNAME" "$EVIL"; do
  req "$ADMIN" "create awaiting member $u" 303 --data-urlencode "my_post_key=$T" --data-urlencode "username=$u" --data-urlencode "password=secret$N" \
    --data-urlencode "email=$(echo "$u" | tr -dc 'a-z0-9')$N@example.com" --data-urlencode "usergroup=5" "$BASE/admin/users/new"
done
MUID=$(sql "SELECT uid FROM users WHERE username = '$MNAME'"); EVUID=$(sql "SELECT uid FROM users WHERE username = '$EVIL'")
req "$ADMIN" "activate both" 303 --data-urlencode "my_post_key=$T" --data-urlencode "action=activate" --data-urlencode "uids=$MUID" --data-urlencode "uids=$EVUID" "$BASE/admin/users/awaiting"
[ "$(sql "SELECT count(*) FROM privatemessages WHERE uid = $MUID AND fromid = $SYS")" = 1 ] && ok "welcome PM sent from System" || bad "welcome PM sent from System"
EPM=$(sql "SELECT pmid FROM privatemessages WHERE uid = $EVUID AND fromid = $SYS LIMIT 1")
login "$MEMBER" "$EVIL" "secret$N"
req "$MEMBER" "hostile member reads welcome" 200 "$BASE/pm/read/$EPM"
has "name shown literally" "Hello <strong class=\"mycode_b\">\[b\]ev$N\[/b\]</strong>"
hasnt "name can't add markup" ">ev$N</strong>"
: > "$MEMBER"

echo "# Member view of System messages"
login "$MEMBER" "$MNAME" "secret$N"
PM=$(sql "SELECT pmid FROM privatemessages WHERE uid = $MUID AND fromid = $SYS LIMIT 1")
req "$MEMBER" "inbox" 200 "$BASE/pm"
has "inbox shows System as sender" "name-system"
req "$MEMBER" "read welcome" 200 "$BASE/pm/read/$PM"
has "automated message notice" 'This is an automated message'
hasnt "no reply button" "pm/send?pmid=$PM\">Reply"
hasnt "no report button" "type=pm&id=$PM"
req "$MEMBER" "reply form doesn't address System" 200 "$BASE/pm/send?pmid=$PM"
hasnt "System not prefilled" "name=\"to\" value=\"$SYSNAME\""
MT=$(csrf "$MEMBER" /)
req "$MEMBER" "reporting a System message refused" 422 "$BASE/report?type=pm&id=$PM"
req "$MEMBER" "PM to System refused" 200 --data-urlencode "my_post_key=$MT" --data-urlencode "to=$SYSNAME" --data-urlencode "subject=Hi" --data-urlencode "message=Hello" "$BASE/pm/send"
has "PM refusal shown" 'cannot receive private messages'

echo "# Members can't speak as System"
req "$MEMBER" "member thread as System refused" 403 --data-urlencode "my_post_key=$MT" --data-urlencode "subject=Fake $N" --data-urlencode "message=I am System" --data-urlencode "as_system=1" "$BASE/newthread/3"
req "$MEMBER" "member PM as System refused" 403 --data-urlencode "my_post_key=$MT" --data-urlencode "to=admin" --data-urlencode "subject=Fake" --data-urlencode "message=I am System" --data-urlencode "as_system=1" "$BASE/pm/send"
[ "$(sql "SELECT count(*) FROM threads WHERE subject = 'Fake $N'")" = 0 ] && ok "no thread created" || bad "no thread created"
req "$MEMBER" "editor has no System option" 200 "$BASE/newthread/3"
hasnt "checkbox hidden from members" 'name="as_system"'

echo "# Staff speak as System"
req "$ADMIN" "editor offers System option" 200 "$BASE/newthread/3"
has "checkbox shown to admins" 'name="as_system"'
TID=$(curl -s -o /dev/null -D - -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$T" --data-urlencode "subject=System notice $N" --data-urlencode "message=Scheduled maintenance tonight." --data-urlencode "as_system=1" "$BASE/newthread/3" | location | sed -n 's|^/thread/\([0-9]*\).*|\1|p')
[ -n "$TID" ] && ok "thread posted as System (tid $TID)" || bad "thread posted as System"
read -r TUID PIP <<<"$(sql "SELECT t.uid, p.ipaddress = '' FROM threads t JOIN posts p ON p.pid = t.firstpost WHERE t.tid = ${TID:-0}" | tr '|' ' ')"
[ "$TUID" = "$SYS" ] && ok "thread author is System" || bad "thread author is System (got $TUID)"
[ "$PIP" = t ] && ok "post stores no IP" || bad "post stores no IP"
req "$ADMIN" "reply as System" 303 --data-urlencode "my_post_key=$T" --data-urlencode "message=Done, all good." --data-urlencode "as_system=1" "$BASE/newreply/${TID:-0}"
req "$ADMIN" "second reply as System isn't merged" 303 --data-urlencode "my_post_key=$T" --data-urlencode "message=One more thing." --data-urlencode "as_system=1" "$BASE/newreply/${TID:-0}"
[ "$(sql "SELECT count(*) FROM posts WHERE tid = ${TID:-0} AND uid = $SYS")" = 3 ] && ok "three System posts" || bad "three System posts"
[ "$(sql "SELECT count(*) FROM system_authorship a JOIN users u ON u.uid = a.actor AND u.username = 'admin' WHERE (a.kind = 'thread' AND a.ref_id = ${TID:-0}) OR (a.kind = 'post' AND a.ref_id IN (SELECT pid FROM posts WHERE tid = ${TID:-0}))")" = 3 ] && ok "authorship recorded for all three" || bad "authorship recorded for all three"
if ./target/debug/rbb check >/dev/null 2>&1; then ok "counters consistent"; else bad "counters consistent"; fi
req "$ADMIN" "thread page" 200 "$BASE/thread/${TID:-0}"
has "System shown as author" 'name-system'
req "$ADMIN" "PM as System" 303 --data-urlencode "my_post_key=$T" --data-urlencode "to=$MNAME" --data-urlencode "subject=Notice $N" --data-urlencode "message=Please read the rules." --data-urlencode "as_system=1" --data-urlencode "savecopy=1" --data-urlencode "receipt=1" "$BASE/pm/send"
read -r PFROM PIP2 PREC <<<"$(sql "SELECT fromid, ipaddress = '', receipt FROM privatemessages WHERE uid = $MUID AND subject = 'Notice $N'" | tr '|' ' ')"
[ "$PFROM" = "$SYS" ] && ok "PM is from System" || bad "PM is from System (got $PFROM)"
[ "$PIP2" = t ] && [ "$PREC" = 0 ] && ok "no IP, no read receipt" || bad "no IP, no read receipt"
[ "$(sql "SELECT count(*) FROM privatemessages WHERE subject = 'Notice $N' AND folder = 2")" = 0 ] && ok "no Sent copy" || bad "no Sent copy"
req "$ADMIN" "encrypted PM as System refused" 200 --data-urlencode "my_post_key=$T" --data-urlencode "to=$MNAME" --data-urlencode "subject=x" --data-urlencode "message=x" --data-urlencode "as_system=1" --data-urlencode "pgp_mode=sign" "$BASE/pm/send"
has "explains why" "be signed or encrypted"
req "$ADMIN" "announcement as System" 303 --data-urlencode "my_post_key=$T" --data-urlencode "fid=-1" --data-urlencode "subject=Announcement $N" --data-urlencode "message=Hello all" --data-urlencode "allowmycode=1" --data-urlencode "as_system=1" "$BASE/modcp/announcements/edit"
AID=$(sql "SELECT aid FROM announcements WHERE subject = 'Announcement $N'")
[ "$(sql "SELECT uid FROM announcements WHERE aid = ${AID:-0}")" = "$SYS" ] && ok "announcement author is System" || bad "announcement author is System"
req "$ADMIN" "system log" 200 "$BASE/admin/tools/systemlog"
has "log names the staff member" '>admin</a>'
has "log lists the announcement" "Announcement #${AID:-0}"
has "log lists the PM" "to $MNAME: Notice $N"
req "$ADMIN" "delete announcement" 303 --data-urlencode "my_post_key=$T" --data-urlencode "aid=${AID:-0}" "$BASE/modcp/announcements/delete"

echo "# Automation"
OLD=$(curl -s -o /dev/null -D - -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$T" --data-urlencode "subject=Old thread $N" --data-urlencode "message=Quiet." "$BASE/newthread/3" | location | sed -n 's|^/thread/\([0-9]*\).*|\1|p')
OTHER=$(curl -s -o /dev/null -D - -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$T" --data-urlencode "subject=Old elsewhere $N" --data-urlencode "message=Quiet." "$BASE/newthread/4" | location | sed -n 's|^/thread/\([0-9]*\).*|\1|p')
sql "UPDATE threads SET lastpost = lastpost - 40 * 86400 WHERE tid IN (${OLD:-0}, ${OTHER:-0})"
sql "UPDATE users SET suspendposting = TRUE, suspensiontime = 1 WHERE uid = $MUID"
for key in systemautoclose banlifter; do
  KT=$(sql "SELECT tid FROM tasks WHERE key = '$key'")
  req "$ADMIN" "run task $key" 303 --data-urlencode "my_post_key=$T" --data-urlencode "tid=$KT" --data-urlencode "action=run" "$BASE/admin/tools/tasks"
done
[ "$(sql "SELECT closed FROM threads WHERE tid = ${OLD:-0}")" = 1 ] && ok "inactive thread closed" || bad "inactive thread closed"
[ "$(sql "SELECT closed FROM threads WHERE tid = ${OTHER:-0}")" = "" ] && ok "forum filter respected" || bad "forum filter respected"
[ "$(sql "SELECT count(*) FROM moderatorlog WHERE uid = $SYS AND tid = ${OLD:-0} AND action LIKE 'Thread closed (no posts for 30 days)'")" = 1 ] && ok "closure logged as System" || bad "closure logged as System"
[ "$(sql "SELECT count(*) FROM moderatorlog WHERE uid = $SYS AND action = 'Posting suspension expired' AND data->>'uid' = '$MUID'")" = 1 ] && ok "expiry logged as System" || bad "expiry logged as System"
[ "$(sql "SELECT suspendposting FROM users WHERE uid = $MUID")" = f ] && ok "suspension lifted" || bad "suspension lifted"

echo "# Ban banner"
req "$ADMIN" "ban member" 303 --data-urlencode "my_post_key=$T" --data-urlencode "reason=Spamming <script>x</script> links" --data-urlencode "days=7" "$BASE/admin/users/$MUID/ban"
req "$ADMIN" "profile as staff" 200 "$BASE/user/$MUID"
has "banner shown" "$MNAME is banned."
has "reason shown, escaped" "Spamming &lt;script&gt;x&lt;"
has "expiry shown" 'Expires '
has "staff see who banned" 'Banned by <a'
req /dev/null "profile as guest" 200 "$BASE/user/$MUID"
has "guests see the banner" "$MNAME is banned."
has "guests see the reason" 'Spamming &lt;script&gt;'
hasnt "guests don't see who banned" 'Banned by'
req "$ADMIN" "unrelated profile has no banner" 200 "$BASE/user/1"
hasnt "no banner" 'profile-ban'

echo "# Cleanup"
req "$ADMIN" "restore System settings" 303 --data-urlencode "my_post_key=$T" --data-urlencode "system_welcome_pm=0" \
  --data-urlencode "system_welcome_subject=Welcome to {boardname}!" --data-urlencode "system_welcome_message=$(sql "SELECT 1" >/dev/null; printf 'Hi {username},\n\nWelcome to {boardname}! Take a moment to read the forum rules, then say hello.\n\nThis is an automated message, so replies aren'"'"'t read. If you need help, use the contact page.')" \
  --data-urlencode "system_autoclose_days=0" --data-urlencode "system_autoclose_forums=" --data-urlencode "system_log_expiries=1" "$BASE/admin/settings/system"
for u in "$MUID" "$EVUID"; do req "$ADMIN" "delete test member $u" 303 --data-urlencode "my_post_key=$T" "$BASE/admin/users/$u/delete"; done

rm -f "$ADMIN" "$MEMBER" "$BODY"
[ $fail = 0 ] && echo "ALL OK" || { echo "SOME CHECKS FAILED"; exit 1; }
