-- OpenPGP identity keys and verified contacts for private messages.

-- Every key a member has published. Old keys are kept (status replaced/revoked) so messages
-- signed with them still verify and contacts can see when and how a key changed.
CREATE TABLE pgp_keys (
    kid            SERIAL PRIMARY KEY,
    uid            INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    fingerprint    TEXT NOT NULL,
    algorithm      TEXT NOT NULL,
    armored        TEXT NOT NULL,
    user_ids       JSONB NOT NULL DEFAULT '[]',
    enc_keyids     TEXT[] NOT NULL DEFAULT '{}',
    key_created    BIGINT NOT NULL,
    expires        BIGINT NOT NULL DEFAULT 0,       -- 0 = never
    added          BIGINT NOT NULL,
    status         SMALLINT NOT NULL DEFAULT 0,     -- 0 active, 1 replaced, 2 revoked
    retired        BIGINT NOT NULL DEFAULT 0,       -- when it stopped being active
    source         TEXT NOT NULL DEFAULT 'generated', -- generated | imported
    -- Signature by the member's previous key vouching for this one (rbb-key-transition/v1).
    transition_from TEXT NOT NULL DEFAULT '',
    transition_sig TEXT NOT NULL DEFAULT '',
    -- The private key, encrypted in the browser with the member's passphrase. Optional; lets
    -- them restore the key on another device. The server cannot decrypt it.
    backup         TEXT NOT NULL DEFAULT '',
    UNIQUE (uid, fingerprint)
);
CREATE UNIQUE INDEX pgp_keys_one_active ON pgp_keys (uid) WHERE status = 0;
-- A key can only be the active identity of one account at a time.
CREATE UNIQUE INDEX pgp_keys_active_fpr ON pgp_keys (fingerprint) WHERE status = 0;

-- "I compared safety numbers with this member." Each row carries a signature by the verifier's
-- key over the statement, so browsers can check it wasn't made up by the server.
CREATE TABLE pgp_verifications (
    verifier      INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    subject       INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    verifier_fpr  TEXT NOT NULL,
    subject_fpr   TEXT NOT NULL,
    statement     TEXT NOT NULL,
    signature     TEXT NOT NULL,
    created       BIGINT NOT NULL,
    PRIMARY KEY (verifier, subject)
);
CREATE INDEX pgp_verifications_subject ON pgp_verifications (subject);

-- pgp: 0 plain, 1 signed (pgp_payload + detached pgp_sig), 2 end-to-end encrypted (the
-- message column holds an armored OpenPGP message whose signed plaintext is the payload).
ALTER TABLE privatemessages
    ADD COLUMN pgp         SMALLINT NOT NULL DEFAULT 0,
    ADD COLUMN pgp_fpr     TEXT NOT NULL DEFAULT '',
    ADD COLUMN pgp_payload TEXT NOT NULL DEFAULT '',
    ADD COLUMN pgp_sig     TEXT NOT NULL DEFAULT '';
