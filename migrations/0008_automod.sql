-- Additive only: no historical content is scanned or altered.
CREATE TABLE automod_config (
    id INT PRIMARY KEY CHECK (id = 1),
    revision BIGINT NOT NULL DEFAULT 1,
    policy JSONB NOT NULL DEFAULT '{"mode":"observe","max_links":5,"new_user_posts":5,"duplicate_limit":3,"phrases":[]}'
);
INSERT INTO automod_config (id) VALUES (1);
CREATE TABLE automod_config_history (
    id BIGSERIAL PRIMARY KEY, uid INT NOT NULL, dateline BIGINT NOT NULL,
    before_policy JSONB NOT NULL, after_policy JSONB NOT NULL
);
ALTER TABLE posts ADD COLUMN automod_revision BIGINT NOT NULL DEFAULT 0;
ALTER TABLE threads ADD COLUMN automod_revision BIGINT NOT NULL DEFAULT 0;
CREATE TABLE automod_queue (
    pid INT PRIMARY KEY REFERENCES posts(pid) ON DELETE CASCADE,
    queued_at BIGINT NOT NULL
);
CREATE INDEX automod_queue_order ON automod_queue (queued_at, pid);
CREATE TABLE automod_actions (
    id BIGSERIAL PRIMARY KEY, pid INT NOT NULL, tid INT NOT NULL, fid INT NOT NULL,
    uid INT NOT NULL, dateline BIGINT NOT NULL, policy_revision BIGINT NOT NULL,
    reasons JSONB NOT NULL, status TEXT NOT NULL CHECK (status IN ('observed','quarantined','undone')),
    post_revision BIGINT NOT NULL, thread_revision BIGINT NOT NULL,
    is_first BOOLEAN NOT NULL, undone_by INT, undone_at BIGINT
);
CREATE INDEX automod_actions_pid ON automod_actions (pid, id DESC);
CREATE INDEX automod_actions_pending ON automod_actions (id DESC) WHERE status = 'quarantined';
-- Revision guards catch intervening edits/moves/moderation, even a change away and back.
CREATE FUNCTION rbb_automod_post_revision() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.automod_revision := OLD.automod_revision + 1;
    RETURN NEW;
END $$;
CREATE TRIGGER automod_post_revision BEFORE UPDATE ON posts FOR EACH ROW
WHEN (OLD.subject IS DISTINCT FROM NEW.subject OR OLD.message IS DISTINCT FROM NEW.message
   OR OLD.visible IS DISTINCT FROM NEW.visible OR OLD.tid IS DISTINCT FROM NEW.tid OR OLD.fid IS DISTINCT FROM NEW.fid)
EXECUTE FUNCTION rbb_automod_post_revision();
CREATE FUNCTION rbb_automod_thread_revision() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.automod_revision := OLD.automod_revision + 1;
    RETURN NEW;
END $$;
CREATE TRIGGER automod_thread_revision BEFORE UPDATE ON threads FOR EACH ROW
WHEN (OLD.visible IS DISTINCT FROM NEW.visible OR OLD.fid IS DISTINCT FROM NEW.fid
   OR OLD.firstpost IS DISTINCT FROM NEW.firstpost OR OLD.closed IS DISTINCT FROM NEW.closed)
EXECUTE FUNCTION rbb_automod_thread_revision();
CREATE FUNCTION rbb_automod_enqueue() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO automod_queue (pid, queued_at) VALUES (NEW.pid, EXTRACT(EPOCH FROM NOW())::bigint)
    ON CONFLICT (pid) DO UPDATE SET queued_at = EXCLUDED.queued_at;
    RETURN NEW;
END $$;
CREATE TRIGGER automod_post_insert AFTER INSERT ON posts FOR EACH ROW EXECUTE FUNCTION rbb_automod_enqueue();
CREATE TRIGGER automod_post_edit AFTER UPDATE ON posts FOR EACH ROW
WHEN (OLD.subject IS DISTINCT FROM NEW.subject OR OLD.message IS DISTINCT FROM NEW.message)
EXECUTE FUNCTION rbb_automod_enqueue();
INSERT INTO tasks (key, title, description, interval_secs)
VALUES ('automoderation', 'System automated moderation', 'Evaluate new/edited posts and preserve reversible quarantine history.', 30);
