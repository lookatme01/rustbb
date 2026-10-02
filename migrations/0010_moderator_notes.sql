-- Moderator notes about members: an append-only log replacing the single users.usernotes field.
-- Notes are retracted, never edited or deleted, so the record of what staff knew stays intact.
CREATE TABLE moderator_notes (
    id           BIGSERIAL PRIMARY KEY,
    uid          INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    author       INT NOT NULL DEFAULT 0,         -- 0 = imported from the old notes field
    note         TEXT NOT NULL,
    created      BIGINT NOT NULL,
    retracted_by INT NOT NULL DEFAULT 0,
    retracted_at BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX moderator_notes_uid ON moderator_notes (uid, id DESC);

-- The old free-text notes become each member's first note. The column stays (older imports
-- write it) but is no longer edited.
INSERT INTO moderator_notes (uid, author, note, created)
SELECT uid, 0, usernotes, EXTRACT(EPOCH FROM now())::bigint FROM users WHERE btrim(usernotes) <> '';
