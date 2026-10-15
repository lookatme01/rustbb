-- Twitch embeds now name the board's own host as their parent instead of localhost: re-render
-- every cached post so existing embeds are fixed.
INSERT INTO settings (name, value) VALUES ('parser_rev', '1')
ON CONFLICT (name) DO UPDATE SET value = (COALESCE(NULLIF(settings.value, ''), '0')::int + 1)::text;
