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
: > "$MEMBER"; login "$MEMBER" "mwmember$N" "secret$N"; TM=$(tok "$MEMBER")   # the ban signed them out
req "$ADMIN" "report member's profile" 303 --data-urlencode "my_post_key=$T" --data-urlencode "type=profile" --data-urlencode "id=$MUID" --data-urlencode "reason=4" --data-urlencode "comment=Profile report $N" "$BASE/report"
req "$MOD" "timeline" 200 "$BASE/modcp/member/$MUID"
has "timeline shows the ban" "History test ban $N"
has "timeline shows the ban lift" "Ban lifted"
has "timeline shows the report" "Profile report $N"
has "timeline shows notes" "Old note $N"
req "$MOD" "timeline filtered to notes" 200 "$BASE/modcp/member/$MUID?type=notes"
hasnt "filter hides bans" "History test ban $N"

echo "# Report claiming and resolution"
sql "INSERT INTO moderators (fid, id, isgroup, perms) VALUES (4, $MODUID, FALSE, '{}')"
curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$TA" --data-urlencode "part=all" "$BASE/admin/tools/cache"
# The member posts it themselves, so every counter stays consistent.
TID=$(curl -s -o /dev/null -D - -b "$MEMBER" -c "$MEMBER" --data-urlencode "my_post_key=$TM" --data-urlencode "subject=Report target $N" --data-urlencode "message=Reported content $N" "$BASE/newthread/4" | tr -d '\r' | sed -n 's|^location: /thread/\([0-9]*\).*|\1|Ip')
PID=$(sql "SELECT firstpost FROM threads WHERE tid = ${TID:-0}")
[ -n "$PID" ] && ok "member posted thread $TID" || bad "member posted thread"
req "$MOD" "note on the reported member" 303 --data-urlencode "my_post_key=$TMOD" --data-urlencode "note=Latest context note $N" "$BASE/modcp/member/$MUID/notes"
req "$SMOD" "super moderator reports the post" 303 --data-urlencode "my_post_key=$TS" --data-urlencode "type=post" --data-urlencode "id=$PID" --data-urlencode "reason=3" "$BASE/report"
RID=$(sql "SELECT rid FROM reportedcontent WHERE type = 'post' AND id = ${PID:-0} AND reportstatus = 0")
[ -n "$RID" ] && ok "report $RID created" || bad "report created"
req "$MOD" "reports list" 200 "$BASE/modcp/reports"
has "list links to the report" "/modcp/reports/$RID"
has "list shows the member's latest note" "Latest context note $N"
req "$MOD" "moderator claims" 303 --data-urlencode "my_post_key=$TMOD" --data-urlencode "action=claim" "$BASE/modcp/reports/$RID/claim"
req "$SMOD" "list after claim" 200 "$BASE/modcp/reports"
has "list shows the claim" "Claimed by mwmod$N"
req "$SMOD" "claiming someone else's report is refused" 422 --data-urlencode "my_post_key=$TS" --data-urlencode "action=claim" "$BASE/modcp/reports/$RID/claim"
has "explains who has it" "mwmod$N"
req "$SMOD" "take over" 303 --data-urlencode "my_post_key=$TS" --data-urlencode "action=takeover" "$BASE/modcp/reports/$RID/claim"
req "$MOD" "previous claimer can't release it now" 403 --data-urlencode "my_post_key=$TMOD" --data-urlencode "action=release" "$BASE/modcp/reports/$RID/claim"
req "$SMOD" "resolve with a note" 303 --data-urlencode "my_post_key=$TS" --data-urlencode "resolution=Removed the spam link <b>x</b> $N" "$BASE/modcp/reports/$RID/resolve"
[ "$(sql "SELECT reportstatus FROM reportedcontent WHERE rid = $RID")" = 1 ] && ok "report closed" || bad "report closed"
req "$MOD" "report detail" 200 "$BASE/modcp/reports/$RID"
has "detail shows the resolution, escaped" "Removed the spam link &lt;b&gt;x&lt;"
has "history: claimed" "Claimed"
has "history: taken over" "Took over from mwmod$N"
has "history: resolved" "Resolved"
has "detail shows the member's latest note" "Latest context note $N"
req "$MOD" "reopen" 303 --data-urlencode "my_post_key=$TMOD" "$BASE/modcp/reports/$RID/reopen"
[ "$(sql "SELECT reportstatus FROM reportedcontent WHERE rid = $RID")" = 0 ] && ok "report reopened" || bad "report reopened"
req "$MEMBER" "members can't see report details" 403 "$BASE/modcp/reports/$RID"
# Mod CP access alone (no "can manage reported content") must not be enough to close reports.
NG=$(sql "INSERT INTO usergroups (type, title, description, namestyle, usertitle, stars, starimage, disporder, isbannedgroup, perms)
  SELECT 2, 'No reports $N', '', '{username}', '', 0, '', 99, FALSE, perms || '{\"canmanagereportedcontent\": false}'::jsonb FROM usergroups WHERE gid = 6 RETURNING gid")
NG=$(echo "$NG" | head -1)
curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$TA" --data-urlencode "part=all" "$BASE/admin/tools/cache"
NRUID=$(mkuser "mwnoreports$N" "$NG"); NR=$(mktemp); login "$NR" "mwnoreports$N" "secret$N"; TN=$(tok "$NR")
req "$NR" "can't view reports" 403 "$BASE/modcp/reports"
req "$NR" "can't mark reports handled" 403 --data-urlencode "my_post_key=$TN" --data-urlencode "action=handled" --data-urlencode "ids=$RID" "$BASE/modcp/reports"
req "$NR" "can't claim" 403 --data-urlencode "my_post_key=$TN" --data-urlencode "action=claim" "$BASE/modcp/reports/$RID/claim"
[ "$(sql "SELECT reportstatus FROM reportedcontent WHERE rid = $RID")" = 0 ] && ok "report still open" || bad "report still open"
curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$TA" "$BASE/admin/users/$NRUID/delete"
sql "DELETE FROM usergroups WHERE gid = $NG"; curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$TA" --data-urlencode "part=all" "$BASE/admin/tools/cache"
curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$T" --data-urlencode "fid=4" --data-urlencode "action=delete" --data-urlencode "tids=$TID" "$BASE/moderation/threads"
echo "# Ban appeals"
req "$ADMIN" "ban member for appeals" 303 --data-urlencode "my_post_key=$TA" --data-urlencode "reason=Appeal test ban $N" --data-urlencode "days=30" "$BASE/admin/users/$MUID/ban"
: > "$MEMBER"; login "$MEMBER" "mwmember$N" "secret$N"; TM=$(tok "$MEMBER")
req "$MEMBER" "banned page offers an appeal" 403 "$BASE/"
has "appeal form shown" 'name="statement"'
req "$MEMBER" "empty appeal refused" 422 --data-urlencode "my_post_key=$TM" --data-urlencode "statement=  " "$BASE/member/appeal"
req "$MEMBER" "appeal submitted" 303 --data-urlencode "my_post_key=$TM" --data-urlencode "statement=I'm sorry <script>alert(1)</script>, it won't happen again $N" "$BASE/member/appeal"
req "$MEMBER" "banned page shows pending appeal" 403 "$BASE/"
has "pending status shown" "Your appeal is waiting"
hasnt "no second form while pending" 'name="statement"'
req "$MEMBER" "second appeal refused" 422 --data-urlencode "my_post_key=$TM" --data-urlencode "statement=Again $N" "$BASE/member/appeal"
req "$MEMBER" "banned member can't reach the Mod CP" 403 "$BASE/modcp/appeals"
hasnt "gate shows the banned page" "Ban appeals</h2>"
AID=$(sql "SELECT id FROM ban_appeals WHERE uid = $MUID AND status = 0")
[ -n "$AID" ] && ok "appeal $AID stored" || bad "appeal stored"
req "$MOD" "moderator without ban rights can't see appeals" 403 "$BASE/modcp/appeals"
req "$SMOD" "appeals queue" 200 "$BASE/modcp/appeals"
has "queue lists the member" "mwmember$N"
has "statement escaped" "I&#x27;m sorry &lt;script&gt;" 
has "nav shows the count" 'Ban appeals<span class="count">'
req "$SMOD" "appeal detail" 200 "$BASE/modcp/appeals/$AID"
has "detail shows the ban reason" "Appeal test ban $N"
req "$SMOD" "rejecting needs a response" 422 --data-urlencode "my_post_key=$TS" --data-urlencode "decision=reject" --data-urlencode "response= " "$BASE/modcp/appeals/$AID/decide"
req "$SMOD" "reject" 303 --data-urlencode "my_post_key=$TS" --data-urlencode "decision=reject" --data-urlencode "response=Not yet, try again later $N" "$BASE/modcp/appeals/$AID/decide"
req "$SMOD" "deciding twice is refused" 422 --data-urlencode "my_post_key=$TS" --data-urlencode "decision=accept" --data-urlencode "response=x" "$BASE/modcp/appeals/$AID/decide"
req "$MEMBER" "banned page after rejection" 403 "$BASE/"
has "shows the staff response" "Not yet, try again later $N"
has "shows when to try again" "You can appeal again"
req "$MEMBER" "appeal during cooldown refused" 422 --data-urlencode "my_post_key=$TM" --data-urlencode "statement=Please $N" "$BASE/member/appeal"
sql "UPDATE ban_appeals SET decided_at = decided_at - 31 * 86400 WHERE id = $AID"
req "$MEMBER" "appeal again after the cooldown" 303 --data-urlencode "my_post_key=$TM" --data-urlencode "statement=Second appeal $N" "$BASE/member/appeal"
AID2=$(sql "SELECT id FROM ban_appeals WHERE uid = $MUID AND status = 0")
req "$SMOD" "accept" 303 --data-urlencode "my_post_key=$TS" --data-urlencode "decision=accept" --data-urlencode "response=Welcome back $N" "$BASE/modcp/appeals/$AID2/decide"
[ "$(sql "SELECT count(*) FROM banned WHERE uid = $MUID")" = 0 ] && ok "ban lifted" || bad "ban lifted"
[ "$(sql "SELECT usergroup FROM users WHERE uid = $MUID")" = 2 ] && ok "group restored" || bad "group restored"
req "$MEMBER" "member is back" 200 "$BASE/"
[ "$(sql "SELECT count(*) FROM privatemessages WHERE uid = $MUID AND fromid = (SELECT uid FROM users WHERE is_system) AND subject ILIKE '%appeal%'")" -ge 2 ] && ok "System messages sent for both decisions" || bad "System messages sent for both decisions"
req "$MOD" "timeline shows appeals" 200 "$BASE/modcp/member/$MUID?type=appeals"
has "rejected appeal on timeline" "Ban appeal rejected"
has "accepted appeal on timeline" "Ban appeal accepted"
[ "$(sql "SELECT count(*) FROM moderatorlog WHERE action = 'Accepted ban appeal' AND data->>'uid' = '$MUID'")" = 1 ] && ok "moderator log records the decision" || bad "moderator log records the decision"

echo "# Cleanup"
sql "DELETE FROM moderators WHERE id = $MODUID AND NOT isgroup"
curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$TA" --data-urlencode "part=all" "$BASE/admin/tools/cache"
for u in "$MUID" "$MODUID" "$SMODUID"; do curl -s -o /dev/null -b "$ADMIN" -c "$ADMIN" --data-urlencode "my_post_key=$TA" "$BASE/admin/users/$u/delete"; done
rm -f "$ADMIN" "$MEMBER" "$MOD" "$SMOD" "$BODY"
[ $fail = 0 ] && echo "ALL OK" || { echo "SOME CHECKS FAILED"; exit 1; }
