-- Badges: achievements shown on profiles and posts, earned automatically from activity (see the
-- hourly "Badges" task) or awarded by staff.

CREATE TABLE badges (
    bid          SERIAL PRIMARY KEY,
    name         TEXT NOT NULL CHECK (btrim(name) <> ''),
    description  TEXT NOT NULL DEFAULT '',
    -- One of the built-in icons and colours (src/badges.rs), never markup or a URL.
    icon         TEXT NOT NULL DEFAULT 'award' CHECK (icon IN ('award', 'calendar', 'chat', 'star', 'flame', 'heart', 'users', 'shield', 'trophy', 'sparkle', 'bolt', 'leaf')),
    color        TEXT NOT NULL DEFAULT 'bronze' CHECK (color IN ('bronze', 'silver', 'gold', 'green', 'blue', 'purple', 'red')),
    -- All must hold, e.g. {"registered_days": [">=", 365]}. Empty: awarded by hand only.
    requirements JSONB NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(requirements) = 'object'),
    enabled      BOOLEAN NOT NULL DEFAULT TRUE,
    -- Lower first. Posts show a member's first few, so the most prestigious badges come first.
    disporder    INT NOT NULL DEFAULT 0
);

CREATE TABLE user_badges (
    uid        INT NOT NULL REFERENCES users(uid) ON DELETE CASCADE,
    bid        INT NOT NULL REFERENCES badges(bid) ON DELETE CASCADE,
    dateline   BIGINT NOT NULL,
    -- Awarded by staff (and by whom, while that account exists) or earned automatically.
    manual     BOOLEAN NOT NULL DEFAULT FALSE,
    awarded_by INT REFERENCES users(uid) ON DELETE SET NULL,
    reason     TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (uid, bid)
);
-- Holders of a badge, newest first; how many hold it.
CREATE INDEX user_badges_bid ON user_badges (bid, dateline DESC);

INSERT INTO badges (name, description, icon, color, requirements, disporder) VALUES
    ('10 Years of Service', 'A member for ten years.', 'calendar', 'purple', '{"registered_days": [">=", 3652]}', 10),
    ('5 Years of Service', 'A member for five years.', 'calendar', 'gold', '{"registered_days": [">=", 1826]}', 20),
    ('Legend', 'Wrote 10,000 posts.', 'trophy', 'gold', '{"posts": [">=", 10000]}', 30),
    ('2 Years of Service', 'A member for two years.', 'calendar', 'silver', '{"registered_days": [">=", 730]}', 40),
    ('Veteran', 'Wrote 1,000 posts.', 'chat', 'silver', '{"posts": [">=", 1000]}', 50),
    ('Respected', 'Reached a reputation of 100.', 'heart', 'gold', '{"reputation": [">=", 100]}', 60),
    ('1 Year of Service', 'A member for a year.', 'calendar', 'bronze', '{"registered_days": [">=", 365]}', 70),
    ('Regular', 'Wrote 100 posts.', 'chat', 'bronze', '{"posts": [">=", 100]}', 80),
    ('Conversation Starter', 'Started 10 threads.', 'sparkle', 'blue', '{"threads": [">=", 10]}', 90),
    ('Well Regarded', 'Reached a reputation of 10.', 'heart', 'green', '{"reputation": [">=", 10]}', 100),
    ('Recruiter', 'Referred 3 members.', 'users', 'blue', '{"referrals": [">=", 3]}', 110),
    ('First Post', 'Wrote their first post.', 'leaf', 'green', '{"posts": [">=", 1]}', 120);
