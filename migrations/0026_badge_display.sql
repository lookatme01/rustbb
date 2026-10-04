-- Members choose which of their badges are shown and in what order (User CP → Badges).

-- Hidden badges are kept but not shown on the member's profile or posts, nor listed among the
-- badge's holders.
ALTER TABLE user_badges ADD COLUMN hidden BOOLEAN NOT NULL DEFAULT FALSE;
-- The member's own order, lower first. NULL until they arrange their badges; unarranged badges
-- (including ones earned later) follow the arranged ones, in the board's display order.
ALTER TABLE user_badges ADD COLUMN position INT;
