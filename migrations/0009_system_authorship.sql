-- Who really wrote content published as the System account (threads, replies, PMs,
-- announcements). Written in the same transaction as the content. The staff member's IP is kept
-- here only; the content row itself stores no IP.
CREATE TABLE system_authorship (
    id          BIGSERIAL PRIMARY KEY,
    kind        TEXT NOT NULL,                -- thread | post | pm | announcement
    ref_id      INT NOT NULL,
    actor       INT NOT NULL,                 -- no FK: the record outlives the staff account
    actor_name  TEXT NOT NULL,
    ipaddress   TEXT NOT NULL DEFAULT '',
    dateline    BIGINT NOT NULL,
    summary     TEXT NOT NULL DEFAULT ''
);
CREATE INDEX system_authorship_recent ON system_authorship (dateline DESC);
CREATE INDEX system_authorship_ref ON system_authorship (kind, ref_id);

-- New group permission: administrators (Admin CP access) get it once; later changes in the
-- Admin CP are kept.
UPDATE usergroups SET perms = perms || '{"canpostassystem": true}'::jsonb
WHERE (perms->>'cancp')::boolean IS TRUE AND NOT perms ? 'canpostassystem';
