-- Security audit #1 (2026-09-27).
-- Last accepted TOTP time step per user, so a two-factor code can be used only once.
ALTER TABLE users ADD COLUMN IF NOT EXISTS totp_last_step BIGINT NOT NULL DEFAULT 0;

-- The HTML sanitizer and MyCode parser changed: re-render every cached post, so markup stored
-- by the old parser (unterminated tags in HTML-enabled forums, forged /me markers) is dropped.
INSERT INTO settings (name, value) VALUES ('parser_rev', '1')
ON CONFLICT (name) DO UPDATE SET value = (COALESCE(NULLIF(settings.value, ''), '0')::int + 1)::text;
