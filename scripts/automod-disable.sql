-- Use only after disabling/undoing quarantines in Admin CP and stopping the server.
-- Extra tables/columns and migration history intentionally remain for audit/compatibility.
BEGIN;
SELECT pg_advisory_xact_lock(424244);
INSERT INTO automod_config_history (uid, dateline, before_policy, after_policy)
SELECT (SELECT uid FROM users WHERE is_system), EXTRACT(EPOCH FROM NOW())::bigint,
       policy, jsonb_set(policy, '{mode}', '"off"') FROM automod_config WHERE id = 1;
UPDATE automod_config SET policy = jsonb_set(policy, '{mode}', '"off"'), revision = revision + 1 WHERE id = 1;
UPDATE tasks SET enabled = FALSE WHERE key = 'automoderation';
DROP TRIGGER IF EXISTS automod_post_insert ON posts;
DROP TRIGGER IF EXISTS automod_post_edit ON posts;
DROP TRIGGER IF EXISTS automod_post_revision ON posts;
DROP TRIGGER IF EXISTS automod_thread_revision ON threads;
DROP FUNCTION IF EXISTS rbb_automod_enqueue();
DROP FUNCTION IF EXISTS rbb_automod_post_revision();
DROP FUNCTION IF EXISTS rbb_automod_thread_revision();
COMMIT;
