-- Per-account audit trail shown to the account owner (User CP → Account activity).
CREATE TABLE user_audit (
    id         BIGSERIAL PRIMARY KEY,
    uid        INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    dateline   BIGINT NOT NULL,
    action     TEXT NOT NULL,
    ipaddress  TEXT NOT NULL DEFAULT '',
    useragent  TEXT NOT NULL DEFAULT '',
    actor_uid  INT NOT NULL DEFAULT 0,      -- who did it, when not the account owner (staff)
    details    JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX user_audit_uid ON user_audit (uid, id DESC);
CREATE INDEX user_audit_dateline ON user_audit (dateline);
