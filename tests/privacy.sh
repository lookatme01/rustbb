#!/usr/bin/env bash
# Privacy controls: IP shortening, retention limits, anonymizing deleted members, erasure, {retention}.
# Usage: tests/privacy.sh [base_url] [admin_password]
set -uo pipefail
BASE="${1:-http://127.0.0.1:8088}"; PW="${2:-admin12345}"
PSQL="${PSQL:-/opt/homebrew/opt/postgresql@17/bin/psql}"; DB="${DATABASE_URL:-postgres://rbb@127.0.0.1:5433/rbb}"
ADMIN=$(mktemp); MEMBER=$(mktemp); BODY=$(mktemp); fail=0; N=$RANDOM
ok() { echo "  ok   $1"; }; bad() { echo "  FAIL $1"; fail=1; }
sql() { "$PSQL" "$DB" -Atqc "$1"; }
eq() { [ "$2" = "$3" ] && ok "$1" || bad "$1 (got '$2', want '$3')"; }
csrf() { curl -s -b "$1" -c "$1" "$BASE$2" | sed -n 's/.*name="csrf-token" content="\([^"]*\)".*/\1/p' | head -1; }
req() { local jar="$1" name="$2" want="$3"; shift 3; local code; code=$(curl -s -o "$BODY" -w "%{http_code}" -b "$jar" -c "$jar" "$@"); if [ "$code" = "$want" ]; then ok "$name ($code)"; else bad "$name (got $code want $want)"; grep -o '<p>[^<]*</p>\|<li>[^<]*</li>' "$BODY" | head -2; fi; }
has() { grep -qF -- "$2" "$BODY" && ok "$1" || bad "$1"; }
hasnt() { grep -qF -- "$2" "$BODY" && bad "$1" || ok "$1"; }
login() { local t; t=$(csrf "$1" /member/login); curl -s -o /dev/null -b "$1" -c "$1" --data-urlencode "my_post_key=$t" --data-urlencode "username=$2" --data-urlencode "password=$3" "$BASE/member/login"; }

login "$ADMIN" admin "$PW"; T=$(csrf "$ADMIN" /)
curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$T" --data-urlencode "password=$PW" --data-urlencode "return_to=/admin" "$BASE/admin/verify"
TA=$(csrf "$ADMIN" /admin)
privacy() { curl -s -o /dev/null -w "%{http_code}" -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$TA" --data-urlencode "privacy_ip_days=$1" --data-urlencode "privacy_audit_days=$2" \
  --data-urlencode "privacy_spamlog_days=90" --data-urlencode "privacy_maillog_days=$3" --data-urlencode "privacy_anonymize_deleted=$4" --data-urlencode "privacy_deleted_name=Former member" "$BASE/admin/settings/privacy"; }
mkuser() { curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$TA" --data-urlencode "username=$1" --data-urlencode "password=secret$N" \
  --data-urlencode "email=$1@example.com" --data-urlencode "usergroup=2" "$BASE/admin/users/new"; sql "SELECT uid FROM users WHERE username = '$1'"; }
runtask() { local id; id=$(sql "SELECT tid FROM tasks WHERE key = '$1'"); curl -s -o /dev/null -w "%{http_code}" -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$TA" --data-urlencode "tid=$id" --data-urlencode "action=run" "$BASE/admin/tools/tasks"; }

echo "# IP shortening function"
eq "IPv4 keeps the /24" "$(sql "SELECT rbb_anon_ip('203.0.113.77')")" "203.0.113.0"
eq "IPv6 keeps the /48" "$(sql "SELECT rbb_anon_ip('2001:db8:abcd:1234::1')")" "2001:db8:abcd::"
eq "already shortened stays the same" "$(sql "SELECT rbb_anon_ip('203.0.113.0')")" "203.0.113.0"
eq "empty stays empty" "$(sql "SELECT rbb_anon_ip('')")" ""
eq "garbage becomes empty" "$(sql "SELECT rbb_anon_ip('not-an-ip')")" ""

echo "# Defaults change nothing"
eq "IP shortening off by default" "$(sql "SELECT COALESCE((SELECT value FROM settings WHERE name = 'privacy_ip_days'), '0')")" "0"

echo "# Retention"
eq "settings saved" "$(privacy 30 30 30 0)" "303"
MUID=$(mkuser "privmember$N"); login "$MEMBER" "privmember$N" "secret$N"; TM=$(csrf "$MEMBER" /)
OLD=$(curl -s -o /dev/null -D - -b "$MEMBER" -c "$MEMBER" --data-urlencode "my_post_key=$TM" --data-urlencode "subject=Old post $N" --data-urlencode "message=Old body $N" "$BASE/newthread/4" | tr -d '\r' | sed -n 's|^location: /thread/\([0-9]*\).*|\1|Ip')
sql "UPDATE users SET lastpost = 0 WHERE uid = $MUID"
NEW=$(curl -s -o /dev/null -D - -b "$MEMBER" -c "$MEMBER" --data-urlencode "my_post_key=$TM" --data-urlencode "subject=New post $N" --data-urlencode "message=New body $N" "$BASE/newthread/4" | tr -d '\r' | sed -n 's|^location: /thread/\([0-9]*\).*|\1|Ip')
OLDPID=$(sql "SELECT firstpost FROM threads WHERE tid = ${OLD:-0}"); NEWPID=$(sql "SELECT firstpost FROM threads WHERE tid = ${NEW:-0}")
sql "UPDATE posts SET dateline = dateline - 40 * 86400, ipaddress = '198.51.100.23' WHERE pid = ${OLDPID:-0}"
sql "UPDATE posts SET ipaddress = '198.51.100.24' WHERE pid = ${NEWPID:-0}"
sql "INSERT INTO user_audit (uid, dateline, action, ipaddress) VALUES ($MUID, extract(epoch from now())::bigint - 40 * 86400, 'login', '198.51.100.25')"
sql "INSERT INTO maillogs (dateline, fromuid, touid, tid, type, subject, message, ipaddress, toemail, fromemail) VALUES (extract(epoch from now())::bigint - 40 * 86400, 0, $MUID, 0, 1, 'Old mail $N', 'x', '198.51.100.26', 'a@example.com', 'b@example.com')"
eq "privacy task runs" "$(runtask privacy)" "303"
eq "old post IP shortened" "$(sql "SELECT ipaddress FROM posts WHERE pid = ${OLDPID:-0}")" "198.51.100.0"
eq "recent post IP untouched" "$(sql "SELECT ipaddress FROM posts WHERE pid = ${NEWPID:-0}")" "198.51.100.24"
eq "old activity log entry removed" "$(sql "SELECT count(*) FROM user_audit WHERE uid = $MUID AND ipaddress IN ('198.51.100.25', '198.51.100.0')")" "0"
eq "old mail log removed" "$(sql "SELECT count(*) FROM maillogs WHERE subject = 'Old mail $N'")" "0"

echo "# Privacy page"
sql "INSERT INTO settings (name, value) VALUES ('privacypolicy', 'Our policy. {retention}') ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value"
curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$TA" --data-urlencode "part=settings" "$BASE/admin/tools/cache"
req /dev/null "privacy page" 200 "$BASE/privacy"
has "placeholder expanded" "shortened to their network"
has "uses the configured days" "30 days"
hasnt "placeholder not shown raw" "{retention}"

echo "# Deleting an account"
DUID=$(mkuser "privdel$N"); DJ=$(mktemp); login "$DJ" "privdel$N" "secret$N"; TD=$(csrf "$DJ" /)
DT=$(curl -s -o /dev/null -D - -b "$DJ" -c "$DJ" --data-urlencode "my_post_key=$TD" --data-urlencode "subject=Kept post $N" --data-urlencode "message=Kept body $N" "$BASE/newthread/4" | tr -d '\r' | sed -n 's|^location: /thread/\([0-9]*\).*|\1|Ip')
DPID=$(sql "SELECT firstpost FROM threads WHERE tid = ${DT:-0}")
req "$ADMIN" "delete keeping posts (anonymizing off)" 303 --data-urlencode "my_post_key=$TA" "$BASE/admin/users/$DUID/delete"
eq "name kept when off (unchanged behavior)" "$(sql "SELECT username FROM posts WHERE pid = ${DPID:-0}")" "privdel$N"
eq "anonymizing on" "$(privacy 30 30 30 1)" "303"
AUID=$(mkuser "privanon$N"); AJ=$(mktemp); login "$AJ" "privanon$N" "secret$N"; TAN=$(csrf "$AJ" /)
AT=$(curl -s -o /dev/null -D - -b "$AJ" -c "$AJ" --data-urlencode "my_post_key=$TAN" --data-urlencode "subject=Anon post $N" --data-urlencode "message=Anon body $N" "$BASE/newthread/4" | tr -d '\r' | sed -n 's|^location: /thread/\([0-9]*\).*|\1|Ip')
APID=$(sql "SELECT firstpost FROM threads WHERE tid = ${AT:-0}")
req "$ADMIN" "delete keeping posts (anonymizing on)" 303 --data-urlencode "my_post_key=$TA" "$BASE/admin/users/$AUID/delete"
eq "post shows Former member" "$(sql "SELECT username FROM posts WHERE pid = ${APID:-0}")" "Former member"
eq "post IP removed" "$(sql "SELECT ipaddress FROM posts WHERE pid = ${APID:-0}")" ""
eq "thread starter renamed" "$(sql "SELECT username FROM threads WHERE tid = ${AT:-0}")" "Former member"

echo "# Erasure tool (always anonymizes)"
eq "anonymizing off again" "$(privacy 30 30 30 0)" "303"
EUID_=$(mkuser "priverase$N"); EJ=$(mktemp); login "$EJ" "priverase$N" "secret$N"; TE=$(csrf "$EJ" /)
ET=$(curl -s -o /dev/null -D - -b "$EJ" -c "$EJ" --data-urlencode "my_post_key=$TE" --data-urlencode "subject=Erase post $N" --data-urlencode "message=Erase body $N" "$BASE/newthread/4" | tr -d '\r' | sed -n 's|^location: /thread/\([0-9]*\).*|\1|Ip')
EPID=$(sql "SELECT firstpost FROM threads WHERE tid = ${ET:-0}")
req "$ADMIN" "user editor offers erasure" 200 "$BASE/admin/users/$EUID_"
has "erase form shown" "/admin/users/$EUID_/erase"
req "$ADMIN" "wrong confirmation refused" 422 --data-urlencode "my_post_key=$TA" --data-urlencode "confirm=someone else" --data-urlencode "keepposts=1" "$BASE/admin/users/$EUID_/erase"
eq "account still there" "$(sql "SELECT count(*) FROM users WHERE uid = $EUID_")" "1"
req "$ADMIN" "erase keeping posts" 303 --data-urlencode "my_post_key=$TA" --data-urlencode "confirm=priverase$N" --data-urlencode "keepposts=1" --data-urlencode "reference=Ticket $N" "$BASE/admin/users/$EUID_/erase"
eq "account gone" "$(sql "SELECT count(*) FROM users WHERE uid = $EUID_")" "0"
eq "post kept and anonymized" "$(sql "SELECT username FROM posts WHERE pid = ${EPID:-0}")" "Former member"
eq "erasure logged" "$(sql "SELECT count(*) FROM erasure_log WHERE former_uid = $EUID_ AND reference = 'Ticket $N' AND kept_posts")" "1"
eq "erasure log holds no username" "$(sql "SELECT count(*) FROM erasure_log WHERE reference LIKE '%priverase%'")" "0"
eq "admin log holds no username" "$(sql "SELECT count(*) FROM adminlog WHERE data::text LIKE '%priverase$N%' AND action LIKE '%rase%'")" "0"
req "$ADMIN" "erasure log page" 200 "$BASE/admin/tools/erasurelog"
has "log page lists it" "Ticket $N"
req "$MEMBER" "members can't erase" 403 --data-urlencode "my_post_key=$TM" --data-urlencode "confirm=privmember$N" "$BASE/admin/users/$MUID/erase"

echo "# Cleanup"
eq "settings restored" "$(privacy 0 365 0 0)" "303"
sql "DELETE FROM settings WHERE name = 'privacypolicy'"; curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$TA" --data-urlencode "part=settings" "$BASE/admin/tools/cache"
for t in "$OLD" "$NEW" "$DT" "$AT" "$ET"; do curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$T" --data-urlencode "fid=4" --data-urlencode "action=delete" --data-urlencode "tids=$t" "$BASE/moderation/threads"; done
curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$TA" "$BASE/admin/users/$MUID/delete"
rm -f "$ADMIN" "$MEMBER" "$BODY" "$DJ" "$AJ" "$EJ"
[ $fail = 0 ] && echo "ALL OK" || { echo "SOME CHECKS FAILED"; exit 1; }
