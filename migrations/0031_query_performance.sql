-- Migrations run in a transaction. For a large board, build these beforehand using
-- CREATE INDEX CONCURRENTLY with the same names; IF NOT EXISTS skips those builds.

-- Visibility recounts can use an index-only scan; keep this narrow so duplicate
-- (tid, visible) keys can be deduplicated. posts_tid_dateline supplies post order.
CREATE INDEX IF NOT EXISTS posts_tid_visibility ON posts (tid, visible);

-- Feeds take the newest N per forum. The old index has sticky between fid and
-- dateline, preventing it from supplying this order directly.
CREATE INDEX IF NOT EXISTS threads_public_forum_started ON threads (fid, dateline DESC, tid DESC)
    WHERE visible = 1 AND closed NOT LIKE 'moved|%';

-- Poll lookups and the ON DELETE CASCADE from threads otherwise scan all polls.
CREATE INDEX IF NOT EXISTS polls_tid ON polls (tid, pid);

-- Frequent cleanup must find expired rows without scanning all retained rows.
CREATE INDEX IF NOT EXISTS captcha_dateline ON captcha (dateline);
CREATE INDEX IF NOT EXISTS ratelimits_reset_at ON ratelimits (reset_at);
CREATE INDEX IF NOT EXISTS applied_batches_applied_at ON applied_batches (applied_at);
CREATE INDEX IF NOT EXISTS threadsread_dateline ON threadsread (dateline);
CREATE INDEX IF NOT EXISTS forumsread_dateline ON forumsread (dateline);
CREATE INDEX IF NOT EXISTS tasklog_dateline ON tasklog (dateline);

-- Banlifter runs frequently; most users have no temporary restrictions.
CREATE INDEX IF NOT EXISTS users_posting_expiry ON users (suspensiontime)
    WHERE suspendposting AND suspensiontime > 0;
CREATE INDEX IF NOT EXISTS users_moderation_expiry ON users (moderationtime)
    WHERE moderateposts AND moderationtime > 0;
CREATE INDEX IF NOT EXISTS users_signature_expiry ON users (suspendsigtime)
    WHERE suspendsignature AND suspendsigtime > 0;
