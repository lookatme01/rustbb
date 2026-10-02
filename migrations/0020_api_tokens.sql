-- API credentials, separate from browser sessions. Only a SHA-256 of each token is stored.
-- A token works only for its audience (the /api/v1 routes) and scopes, until it expires or is
-- revoked. last_used_at is updated at most once a minute per token.
CREATE TABLE api_tokens (
    id           BIGSERIAL PRIMARY KEY,
    uid          INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    token_hash   TEXT NOT NULL UNIQUE,
    name         TEXT NOT NULL DEFAULT '',
    scopes       TEXT[] NOT NULL DEFAULT '{read}',
    audience     TEXT NOT NULL DEFAULT 'api/v1',
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at   TIMESTAMPTZ NOT NULL,
    revoked_at   TIMESTAMPTZ,
    last_used_at TIMESTAMPTZ,
    last_used_ip INET,
    CONSTRAINT api_tokens_scopes_check CHECK (scopes <@ ARRAY['read', 'write']::text[] AND cardinality(scopes) > 0),
    CONSTRAINT api_tokens_audience_check CHECK (audience IN ('api/v1')),
    CONSTRAINT api_tokens_expiry_check CHECK (expires_at > created_at)
);
CREATE INDEX api_tokens_uid ON api_tokens (uid) WHERE revoked_at IS NULL;

-- Bearer tokens used to be browser-session rows; they stop working (clients get a new token).
DELETE FROM logins WHERE useragent = 'api';
