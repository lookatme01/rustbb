-- Referential integrity beyond the counters (`rbb check` covers those).
--
--   psql "$DATABASE_URL" -f scripts/integrity.sql
--
-- Each row is one invariant and how many rows break it; everything should be 0. Many of these
-- columns have no foreign key (a post's author, a report's target, a forum's last-post pointer),
-- so only an audit like this notices dangling references after heavy concurrent work such as
-- scripts/soak-http.py. uid 0 means "guest/deleted account" and is allowed everywhere.
SELECT invariant, violations FROM (
  SELECT 'posts without a thread' AS invariant, count(*) AS violations FROM posts p WHERE NOT EXISTS (SELECT 1 FROM threads t WHERE t.tid = p.tid)
  UNION ALL SELECT 'posts whose fid differs from their thread''s', count(*) FROM posts p JOIN threads t ON t.tid = p.tid WHERE p.fid <> t.fid
  UNION ALL SELECT 'posts by a user that does not exist', count(*) FROM posts p WHERE p.uid > 0 AND NOT EXISTS (SELECT 1 FROM users u WHERE u.uid = p.uid)
  UNION ALL SELECT 'threads without a forum', count(*) FROM threads t WHERE NOT EXISTS (SELECT 1 FROM forums f WHERE f.fid = t.fid)
  UNION ALL SELECT 'threads by a user that does not exist', count(*) FROM threads t WHERE t.uid > 0 AND NOT EXISTS (SELECT 1 FROM users u WHERE u.uid = t.uid)
  UNION ALL SELECT 'threads whose first post is missing (not redirects)', count(*) FROM threads t WHERE t.closed NOT LIKE 'moved|%' AND NOT EXISTS (SELECT 1 FROM posts p WHERE p.pid = t.firstpost)
  UNION ALL SELECT 'threads whose first post is in another thread', count(*) FROM threads t JOIN posts p ON p.pid = t.firstpost WHERE p.tid <> t.tid
  UNION ALL SELECT 'threads with no posts at all (not redirects)', count(*) FROM threads t WHERE t.closed NOT LIKE 'moved|%' AND NOT EXISTS (SELECT 1 FROM posts p WHERE p.tid = t.tid)
  UNION ALL SELECT 'redirect stubs pointing at a thread that is gone', count(*) FROM threads s WHERE s.closed LIKE 'moved|%' AND NOT EXISTS (SELECT 1 FROM threads t WHERE t.tid = substr(s.closed, 7)::int)
  UNION ALL SELECT 'forums whose last-post thread is gone', count(*) FROM forums f WHERE f.lastposttid > 0 AND NOT EXISTS (SELECT 1 FROM threads t WHERE t.tid = f.lastposttid)
  UNION ALL SELECT 'forums with a parent that does not exist', count(*) FROM forums f WHERE f.pid > 0 AND NOT EXISTS (SELECT 1 FROM forums p WHERE p.fid = f.pid)
  UNION ALL SELECT 'attachments of a post that is gone', count(*) FROM attachments a WHERE a.pid > 0 AND NOT EXISTS (SELECT 1 FROM posts p WHERE p.pid = a.pid)
  UNION ALL SELECT 'polls without a thread', count(*) FROM polls p WHERE NOT EXISTS (SELECT 1 FROM threads t WHERE t.tid = p.tid)
  UNION ALL SELECT 'threads pointing at a poll that is gone', count(*) FROM threads t WHERE t.poll > 0 AND NOT EXISTS (SELECT 1 FROM polls p WHERE p.pid = t.poll)
  UNION ALL SELECT 'poll tallies that disagree with the votes', count(*) FROM polls p WHERE p.numvotes <> (SELECT count(DISTINCT uid) FROM pollvotes v WHERE v.pid = p.pid)
  UNION ALL SELECT 'poll votes by a user that does not exist', count(*) FROM pollvotes v WHERE v.uid > 0 AND NOT EXISTS (SELECT 1 FROM users u WHERE u.uid = v.uid)
  UNION ALL SELECT 'thread ratings by a user that does not exist', count(*) FROM threadratings r WHERE NOT EXISTS (SELECT 1 FROM users u WHERE u.uid = r.uid)
  UNION ALL SELECT 'thread rating totals that disagree', count(*) FROM threads t WHERE t.numratings <> (SELECT count(*) FROM threadratings r WHERE r.tid = t.tid) OR t.totalratings <> (SELECT COALESCE(sum(rating), 0) FROM threadratings r WHERE r.tid = t.tid)
  UNION ALL SELECT 'reputation given by or to a user that does not exist', count(*) FROM reputation r WHERE NOT EXISTS (SELECT 1 FROM users u WHERE u.uid = r.uid) OR (r.adduid > 0 AND NOT EXISTS (SELECT 1 FROM users u WHERE u.uid = r.adduid))
  UNION ALL SELECT 'reputation a user gave themselves', count(*) FROM reputation WHERE uid = adduid
  UNION ALL SELECT 'private messages of a user that does not exist', count(*) FROM privatemessages m WHERE NOT EXISTS (SELECT 1 FROM users u WHERE u.uid = m.uid)
  UNION ALL SELECT 'reports of a post that is gone', count(*) FROM reportedcontent r WHERE r.type = 'post' AND NOT EXISTS (SELECT 1 FROM posts p WHERE p.pid = r.id)
  UNION ALL SELECT 'post reports filed under the wrong forum', count(*) FROM reportedcontent r JOIN posts p ON p.pid = r.id WHERE r.type = 'post' AND r.id3 <> p.fid
  UNION ALL SELECT 'warnings of a user that does not exist', count(*) FROM warnings w WHERE NOT EXISTS (SELECT 1 FROM users u WHERE u.uid = w.uid)
  UNION ALL SELECT 'bans of a user that does not exist', count(*) FROM banned b WHERE NOT EXISTS (SELECT 1 FROM users u WHERE u.uid = b.uid)
  UNION ALL SELECT 'users in the banned group with no ban record', count(*) FROM users u JOIN usergroups g ON g.gid = u.usergroup WHERE g.isbannedgroup AND NOT EXISTS (SELECT 1 FROM banned b WHERE b.uid = u.uid)
  UNION ALL SELECT 'ban records for users no longer in a banned group', count(*) FROM banned b JOIN users u ON u.uid = b.uid JOIN usergroups g ON g.gid = u.usergroup WHERE NOT g.isbannedgroup
  UNION ALL SELECT 'subscriptions of a user that does not exist', count(*) FROM threadsubscriptions s WHERE NOT EXISTS (SELECT 1 FROM users u WHERE u.uid = s.uid)
  UNION ALL SELECT 'users who belong to a group that does not exist', count(*) FROM users u WHERE NOT EXISTS (SELECT 1 FROM usergroups g WHERE g.gid = u.usergroup)
  UNION ALL SELECT 'moderator assignments for a forum or user that is gone', count(*) FROM moderators m WHERE NOT EXISTS (SELECT 1 FROM forums f WHERE f.fid = m.fid) OR (NOT m.isgroup AND NOT EXISTS (SELECT 1 FROM users u WHERE u.uid = m.id))
  UNION ALL SELECT 'alerts for a user that does not exist', count(*) FROM alerts a WHERE NOT EXISTS (SELECT 1 FROM users u WHERE u.uid = a.uid)
  UNION ALL SELECT 'user badges for a user or badge that is gone', count(*) FROM user_badges ub WHERE NOT EXISTS (SELECT 1 FROM users u WHERE u.uid = ub.uid) OR NOT EXISTS (SELECT 1 FROM badges b WHERE b.bid = ub.bid)
) v
ORDER BY violations DESC, invariant;
