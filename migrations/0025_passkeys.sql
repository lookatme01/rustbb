-- Passkeys (WebAuthn): sign in with a device's screen lock or a security key instead of a password.

-- The random WebAuthn user handle (64 bytes) authenticators store with each passkey. Created with
-- the member's first passkey; never their uid or username, which would leak through the device.
ALTER TABLE users ADD COLUMN webauthn_handle BYTEA UNIQUE;

CREATE TABLE passkeys (
    id            SERIAL PRIMARY KEY,
    uid           INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    credential_id BYTEA NOT NULL UNIQUE,
    name          TEXT NOT NULL DEFAULT '',
    -- webauthn_rp encodings: how the authenticator is reached, the public key and extensions
    -- (fixed at registration), and the signature counter and backup flags (updated on use).
    transports    BYTEA NOT NULL,
    static_state  BYTEA NOT NULL,
    dynamic_state BYTEA NOT NULL,
    created       BIGINT NOT NULL,
    last_used     BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX passkeys_uid ON passkeys (uid);

-- Ceremonies in progress (registration or sign-in), keyed by the challenge sent to the browser.
-- Each is consumed with DELETE … RETURNING, so a challenge works exactly once; expired ones are
-- removed by the hourly cleanup.
CREATE TABLE webauthn_ceremonies (
    challenge  BYTEA PRIMARY KEY,
    -- Who is adding a passkey; NULL for sign-in, where the passkey says who it is.
    uid        INT REFERENCES users(uid) ON DELETE CASCADE,
    state      BYTEA NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX webauthn_ceremonies_expiry ON webauthn_ceremonies (expires_at);
