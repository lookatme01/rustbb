#!/usr/bin/env bash
# Moderation workflow: moderator notes, member history, report claiming, ban appeals.
# Usage: tests/mod_workflow.sh [base_url] [admin_password]
set -uo pipefail
BASE="${1:-http://127.0.0.1:8088}"; PW="${2:-admin12345}"
PSQL="${PSQL:-/opt/homebrew/opt/postgresql@17/bin/psql}"; DB="${DATABASE_URL:-postgres://rbb@127.0.0.1:5433/rbb}"
ADMIN=$(mktemp); MEMBER=$(mktemp); MOD=$(mktemp); SMOD=$(mktemp); BODY=$(mktemp); fail=0; N=$RANDOM
ok() { echo "  ok   $1"; }; bad() { echo "  FAIL $1"; fail=1; }
sql() { "$PSQL" "$DB" -Atqc "$1"; }
csrf() { curl -s -b "$1" -c "$1" "$BASE$2" | sed -n 's/.*name="csrf-token" content="\([^"]*\)".*/\1/p' | head -1; }
req() { local jar="$1" name="$2" want="$3"; shift 3; local code; code=$(curl -s -o "$BODY" -w "%{http_code}" -b "$jar" -c "$jar" "$@"); if [ "$code" = "$want" ]; then ok "$name ($code)"; else bad "$name (got $code want $want)"; grep -o '<p>[^<]*</p>\|<li>[^<]*</li>' "$BODY" | head -2; fi; }
has() { grep -qF -- "$2" "$BODY" && ok "$1" || bad "$1"; }
hasnt() { grep -qF -- "$2" "$BODY" && bad "$1" || ok "$1"; }
login() { local t; t=$(csrf "$1" /member/login); curl -s -o /dev/null -b "$1" -c "$1" --data-urlencode "my_post_key=$t" --data-urlencode "username=$2" --data-urlencode "password=$3" "$BASE/member/login"; }
tok() { csrf "$1" /; }

login "$ADMIN" admin "$PW"; T=$(tok "$ADMIN")
curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$T" --data-urlencode "password=$PW" --data-urlencode "return_to=/admin" "$BASE/admin/verify"
TA=$(csrf "$ADMIN" /admin)
mkuser() { curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$TA" --data-urlencode "username=$1" --data-urlencode "password=secret$N" \
  --data-urlencode "email=$1@example.com" --data-urlencode "usergroup=$2" "$BASE/admin/users/new"; sql "SELECT uid FROM users WHERE username = '$1'"; }
MUID=$(mkuser "mwmember$N" 2); MODUID=$(mkuser "mwmod$N" 6); SMODUID=$(mkuser "mwsmod$N" 3)
login "$MEMBER" "mwmember$N" "secret$N"; login "$MOD" "mwmod$N" "secret$N"; login "$SMOD" "mwsmod$N" "secret$N"
TM=$(tok "$MEMBER"); TMOD=$(tok "$MOD"); TS=$(tok "$SMOD")
[ -n "$MUID" ] && [ -n "$MODUID" ] && [ -n "$SMODUID" ] && ok "setup: member $MUID, moderator $MODUID, super moderator $SMODUID" || { bad "setup"; exit 1; }

echo "# Moderator notes"
req "$MOD" "moderator adds a note" 303 --data-urlencode "my_post_key=$TMOD" --data-urlencode "note=Spammy links in intro <script>x</script> $N" "$BASE/modcp/member/$MUID/notes"
req "$MOD" "member history page" 200 "$BASE/modcp/member/$MUID"
has "note shown, escaped" "Spammy links in intro &lt;script&gt;"
has "note author shown" "mwmod$N"
req "$MEMBER" "member can't open history" 403 "$BASE/modcp/member/$MUID"
req "$MEMBER" "member can't add notes" 403 --data-urlencode "my_post_key=$TM" --data-urlencode "note=x" "$BASE/modcp/member/$MUID/notes"
req "$MOD" "empty note refused" 422 --data-urlencode "my_post_key=$TMOD" --data-urlencode "note=   " "$BASE/modcp/member/$MUID/notes"
req "$MEMBER" "member's own profile" 200 "$BASE/user/$MUID"
hasnt "member doesn't see notes link" "/modcp/member/$MUID"
req "$MOD" "profile as staff" 200 "$BASE/user/$MUID"
has "staff see history link with count" "History (1 note)"
NID=$(sql "SELECT max(id) FROM moderator_notes WHERE uid = $MUID")
req "$SMOD" "other staff can't retract" 403 --data-urlencode "my_post_key=$TS" "$BASE/modcp/notes/$NID/retract"
req "$MOD" "author retracts within window" 303 --data-urlencode "my_post_key=$TMOD" "$BASE/modcp/notes/$NID/retract"
req "$MOD" "history after retraction" 200 "$BASE/modcp/member/$MUID"
has "retracted note still listed" "retracted by mwmod$N"
[ "$(sql "SELECT count(*) FROM moderator_notes WHERE id = $NID")" = 1 ] && ok "retracted note kept in database" || bad "retracted note kept in database"
req "$MOD" "second note" 303 --data-urlencode "my_post_key=$TMOD" --data-urlencode "note=Old note $N" "$BASE/modcp/member/$MUID/notes"
OLD=$(sql "SELECT max(id) FROM moderator_notes WHERE uid = $MUID"); sql "UPDATE moderator_notes SET created = created - 3600 WHERE id = $OLD"
req "$MOD" "author can't retract after 15 minutes" 403 --data-urlencode "my_post_key=$TMOD" "$BASE/modcp/notes/$OLD/retract"
req "$ADMIN" "administrator can always retract" 303 --data-urlencode "my_post_key=$T" "$BASE/modcp/notes/$OLD/retract"
[ "$(sql "SELECT count(*) FROM users u WHERE btrim(usernotes) <> '' AND NOT EXISTS (SELECT 1 FROM moderator_notes n WHERE n.uid = u.uid AND n.author = 0)")" = 0 ] \
  && ok "legacy notes were imported" || bad "legacy notes were imported"
req "$SMOD" "Mod CP profile editor" 200 "$BASE/modcp/editprofile/$MUID"
hasnt "old notes field removed (Mod CP)" 'name="usernotes"'
req "$ADMIN" "Admin CP user editor" 200 "$BASE/admin/users/$MUID"
hasnt "old notes field removed (Admin CP)" 'name="usernotes"'

echo "# Member history timeline"
req "$ADMIN" "ban member" 303 --data-urlencode "my_post_key=$TA" --data-urlencode "reason=History test ban $N" --data-urlencode "days=7" "$BASE/admin/users/$MUID/ban"
req "$ADMIN" "lift ban" 303 --data-urlencode "my_post_key=$TA" --data-urlencode "lift=1" "$BASE/admin/users/$MUID/ban"
req "$ADMIN" "report member's profile" 303 --data-urlencode "my_post_key=$T" --data-urlencode "type=profile" --data-urlencode "id=$MUID" --data-urlencode "reason=4" --data-urlencode "comment=Profile report $N" "$BASE/report"
req "$MOD" "timeline" 200 "$BASE/modcp/member/$MUID"
has "timeline shows the ban" "History test ban $N"
has "timeline shows the ban lift" "Ban lifted"
has "timeline shows the report" "Profile report $N"
has "timeline shows notes" "Old note $N"
req "$MOD" "timeline filtered to notes" 200 "$BASE/modcp/member/$MUID?type=notes"
hasnt "filter hides bans" "History test ban $N"

## @@PART2@@
## @@PART3@@

echo "# Cleanup"
for u in "$MUID" "$MODUID" "$SMODUID"; do curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$TA" "$BASE/admin/users/$u/delete"; done
rm -f "$ADMIN" "$MEMBER" "$MOD" "$SMOD" "$BODY"
[ $fail = 0 ] && echo "ALL OK" || { echo "SOME CHECKS FAILED"; exit 1; }
