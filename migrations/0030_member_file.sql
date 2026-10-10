-- The member file (admin and moderator user pages, flag chips on lists and posts) looks up other
-- accounts on the same address and mail stuck for an address, and can pin one staff note.
CREATE INDEX IF NOT EXISTS users_lastip ON users (lastip) WHERE lastip IS NOT NULL;
CREATE INDEX IF NOT EXISTS users_regip ON users (regip) WHERE regip IS NOT NULL;
CREATE INDEX IF NOT EXISTS mailqueue_mailto_lower ON mailqueue (lower(mailto));

ALTER TABLE moderator_notes ADD COLUMN pinned BOOLEAN NOT NULL DEFAULT FALSE;
CREATE UNIQUE INDEX moderator_notes_one_pinned ON moderator_notes (uid) WHERE pinned AND retracted_at = 0;
