-- "You posted in this thread" dots on thread lists probe (uid, tid) for ~20 threads per page.
CREATE INDEX IF NOT EXISTS posts_uid_tid ON posts (uid, tid);
