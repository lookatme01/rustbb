-- An admin-chosen glyph and colour per forum. '' and -1 mean "pick automatically"
-- (a monogram of the name, and a tone from the forum id).
ALTER TABLE forums ADD COLUMN IF NOT EXISTS icon TEXT NOT NULL DEFAULT '';
ALTER TABLE forums ADD COLUMN IF NOT EXISTS tone SMALLINT NOT NULL DEFAULT -1;
ALTER TABLE forums ADD CONSTRAINT forums_tone_range CHECK (tone BETWEEN -1 AND 5);
