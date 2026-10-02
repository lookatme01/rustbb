-- Staff features are no longer unlocked by moderating any forum; each has its own permission.
-- Keep existing staff groups working: groups with Mod CP access may read and write moderator
-- notes, and super moderators and administrators also see reported private messages and
-- members' moderation history in every forum.
UPDATE usergroups
   SET perms = perms || '{"canviewmodnotes": true, "canaddmodnotes": true}'::jsonb
 WHERE COALESCE((perms->>'canmodcp')::boolean, false)
    OR COALESCE((perms->>'issupermod')::boolean, false)
    OR COALESCE((perms->>'cancp')::boolean, false);
UPDATE usergroups
   SET perms = perms || '{"canviewpmreports": true, "canviewallmodhistory": true}'::jsonb
 WHERE COALESCE((perms->>'issupermod')::boolean, false)
    OR COALESCE((perms->>'cancp')::boolean, false);
