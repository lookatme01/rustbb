-- Indexes for sorts and filters that were scanning: member list sorts, forum thread sorts by
-- replies / views, mail log lookups and purges, and the daily alert cleanup. Built under a table
-- lock like 0023; on large boards create them beforehand with CREATE INDEX CONCURRENTLY.

-- Member list ordered by threads started / reputation, ties broken by uid (as the query does).
CREATE INDEX IF NOT EXISTS users_threadnum ON users (threadnum DESC, uid);
CREATE INDEX IF NOT EXISTS users_reputation ON users (reputation DESC, uid);

-- Forum thread list sorted by replies or views (sticky first, newest tid on ties).
CREATE INDEX IF NOT EXISTS threads_fid_replies ON threads (fid, sticky DESC, replies DESC, tid DESC);
CREATE INDEX IF NOT EXISTS threads_fid_views ON threads (fid, sticky DESC, views DESC, tid DESC);

-- Mail log: per-sender flood counts, erasing a member's rows, and age-based purging.
CREATE INDEX IF NOT EXISTS maillogs_fromuid ON maillogs (fromuid, dateline);
CREATE INDEX IF NOT EXISTS maillogs_touid ON maillogs (touid);
CREATE INDEX IF NOT EXISTS maillogs_dateline ON maillogs (dateline);

-- Read alerts older than 90 days are deleted daily.
CREATE INDEX IF NOT EXISTS alerts_read_dateline ON alerts (dateline) WHERE unread = FALSE;
