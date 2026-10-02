-- Database-enforced invariants. Checks are added NOT VALID and then validated, which scans the
-- table without blocking writes for the duration.

-- One open report per piece of content: later reports join it (see routes/report.rs). Existing
-- duplicates are merged into the oldest open one.
WITH dup AS (
    SELECT type, id, MIN(rid) AS keep, array_agg(rid) AS rids
    FROM reportedcontent WHERE reportstatus = 0 GROUP BY type, id HAVING COUNT(*) > 1
), merged AS (
    SELECT d.keep, SUM(r.reports)::int AS reports, MAX(r.lastreport) AS lastreport,
           ARRAY(SELECT DISTINCT unnest(array_agg(r.reporters))) AS reporters
    FROM dup d JOIN reportedcontent r ON r.rid = ANY(d.rids) GROUP BY d.keep
)
UPDATE reportedcontent r SET reports = m.reports, lastreport = m.lastreport, reporters = m.reporters
FROM merged m WHERE r.rid = m.keep;
UPDATE reportedcontent r SET reportstatus = 1, resolution = 'Merged into an earlier report of the same content'
WHERE r.reportstatus = 0 AND EXISTS (
    SELECT 1 FROM reportedcontent o WHERE o.reportstatus = 0 AND o.type = r.type AND o.id = r.id AND o.rid < r.rid);
CREATE UNIQUE INDEX reportedcontent_one_open ON reportedcontent (type, id) WHERE reportstatus = 0;

-- One rating per member per thread (the rating counters are rebuilt from what remains).
DELETE FROM threadratings a USING threadratings b WHERE a.tid = b.tid AND a.uid = b.uid AND a.rid > b.rid;
CREATE UNIQUE INDEX threadratings_one_per_member ON threadratings (tid, uid);
UPDATE threads t SET numratings = s.n, totalratings = s.total
FROM (SELECT tid, COUNT(*)::int AS n, COALESCE(SUM(rating), 0)::int AS total FROM threadratings GROUP BY tid) s
WHERE t.tid = s.tid AND (t.numratings <> s.n OR t.totalratings <> s.total);

-- Who claimed, resolved or acted: a member, or nobody (NULL), never the made-up member 0.
ALTER TABLE reportedcontent ALTER COLUMN claimed_by DROP NOT NULL, ALTER COLUMN claimed_by DROP DEFAULT,
                            ALTER COLUMN resolved_by DROP NOT NULL, ALTER COLUMN resolved_by DROP DEFAULT;
UPDATE reportedcontent SET claimed_by = NULL WHERE claimed_by = 0 OR claimed_by NOT IN (SELECT uid FROM users);
UPDATE reportedcontent SET resolved_by = NULL WHERE resolved_by = 0 OR resolved_by NOT IN (SELECT uid FROM users);
ALTER TABLE reportedcontent
    ADD CONSTRAINT reportedcontent_claimed_by_fkey FOREIGN KEY (claimed_by) REFERENCES users(uid) ON DELETE SET NULL,
    ADD CONSTRAINT reportedcontent_resolved_by_fkey FOREIGN KEY (resolved_by) REFERENCES users(uid) ON DELETE SET NULL;

ALTER TABLE user_audit ALTER COLUMN actor_uid DROP NOT NULL, ALTER COLUMN actor_uid DROP DEFAULT;
UPDATE user_audit SET actor_uid = NULL WHERE actor_uid = 0 OR actor_uid NOT IN (SELECT uid FROM users);
ALTER TABLE user_audit ADD CONSTRAINT user_audit_actor_fkey FOREIGN KEY (actor_uid) REFERENCES users(uid) ON DELETE SET NULL;

ALTER TABLE moderator_notes ALTER COLUMN author DROP NOT NULL, ALTER COLUMN author DROP DEFAULT;
UPDATE moderator_notes SET author = NULL WHERE author = 0 OR author NOT IN (SELECT uid FROM users);
ALTER TABLE moderator_notes ADD CONSTRAINT moderator_notes_author_fkey FOREIGN KEY (author) REFERENCES users(uid) ON DELETE SET NULL;

-- References that were never enforced.
DELETE FROM tasklog WHERE tid NOT IN (SELECT tid FROM tasks);
ALTER TABLE tasklog ADD CONSTRAINT tasklog_tid_fkey FOREIGN KEY (tid) REFERENCES tasks(tid) ON DELETE CASCADE NOT VALID;
ALTER TABLE tasklog VALIDATE CONSTRAINT tasklog_tid_fkey;
ALTER TABLE posts ADD CONSTRAINT posts_fid_fkey FOREIGN KEY (fid) REFERENCES forums(fid) ON DELETE CASCADE NOT VALID;
ALTER TABLE posts VALIDATE CONSTRAINT posts_fid_fkey;

-- States and kinds that only have a few valid values.
ALTER TABLE posts ADD CONSTRAINT posts_visible_check CHECK (visible IN (-1, 0, 1)) NOT VALID;
ALTER TABLE posts VALIDATE CONSTRAINT posts_visible_check;
ALTER TABLE threads ADD CONSTRAINT threads_visible_check CHECK (visible IN (-1, 0, 1)) NOT VALID;
ALTER TABLE threads VALIDATE CONSTRAINT threads_visible_check;
ALTER TABLE reportedcontent
    ADD CONSTRAINT reportedcontent_status_check CHECK (reportstatus IN (0, 1)),
    ADD CONSTRAINT reportedcontent_type_check CHECK (type IN ('post', 'profile', 'reputation', 'pm'));
ALTER TABLE forums ADD CONSTRAINT forums_type_check CHECK (type IN ('f', 'c'));
ALTER TABLE usergroups ADD CONSTRAINT usergroups_type_check CHECK (type IN (1, 2, 3, 4)); -- core, custom, public, by request
ALTER TABLE banfilters ADD CONSTRAINT banfilters_type_check CHECK (type IN (1, 2, 3));
ALTER TABLE ban_appeals ADD CONSTRAINT ban_appeals_status_check CHECK (status IN (0, 1, 2));
ALTER TABLE pgp_keys ADD CONSTRAINT pgp_keys_status_check CHECK (status IN (0, 1, 2));
ALTER TABLE massemails ADD CONSTRAINT massemails_status_check CHECK (status IN (0, 1, 2, 3));
ALTER TABLE privatemessages ADD CONSTRAINT privatemessages_status_check CHECK (status IN (0, 1, 3, 4)) NOT VALID;
ALTER TABLE privatemessages VALIDATE CONSTRAINT privatemessages_status_check;
ALTER TABLE threadratings ADD CONSTRAINT threadratings_rating_check CHECK (rating BETWEEN 1 AND 5);

-- Denormalized counters never go negative.
ALTER TABLE threads ADD CONSTRAINT threads_counters_check
    CHECK (replies >= 0 AND unapprovedposts >= 0 AND deletedposts >= 0 AND views >= 0 AND numratings >= 0) NOT VALID;
ALTER TABLE threads VALIDATE CONSTRAINT threads_counters_check;
ALTER TABLE forums ADD CONSTRAINT forums_counters_check
    CHECK (threads >= 0 AND posts >= 0 AND unapprovedthreads >= 0 AND unapprovedposts >= 0 AND deletedthreads >= 0 AND deletedposts >= 0);
ALTER TABLE users ADD CONSTRAINT users_counters_check CHECK (postnum >= 0 AND threadnum >= 0) NOT VALID;
ALTER TABLE users VALIDATE CONSTRAINT users_counters_check;
