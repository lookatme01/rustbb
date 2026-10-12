-- A member's accent colour for the Ink theme: one of the vetted palette keys
-- (ink, cobalt, moss, plum, raspberry, amber), or empty for the theme default.
ALTER TABLE users ADD COLUMN IF NOT EXISTS accent TEXT NOT NULL DEFAULT '';
