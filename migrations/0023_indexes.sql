-- Composite indexes behind keyset pagination and frequent filtered lookups.
-- Migrations run in a transaction, so these are built with a lock on the table. On very large
-- boards create them beforehand with CREATE INDEX CONCURRENTLY (same names); IF NOT EXISTS
-- then skips them here.

-- Moderator log: newest first, by thread / moderator / forum, continued by id (keyset).
CREATE INDEX IF NOT EXISTS moderatorlog_tid_id ON moderatorlog (tid, id DESC) WHERE tid > 0;
CREATE INDEX IF NOT EXISTS moderatorlog_uid_id ON moderatorlog (uid, id DESC);
CREATE INDEX IF NOT EXISTS moderatorlog_fid_id ON moderatorlog (fid, id DESC);

-- Uploads never attached to a post, pruned hourly by age.
CREATE INDEX IF NOT EXISTS attachments_orphans ON attachments (dateuploaded) WHERE pid = 0;

-- Unread private messages per member (unread counts, inbox badges).
CREATE INDEX IF NOT EXISTS privatemessages_unread ON privatemessages (uid) WHERE status = 0;

-- Expired sign-ins, removed hourly.
CREATE INDEX IF NOT EXISTS logins_expires ON logins (expires);
