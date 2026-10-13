-- The guest welcome text moves from the default theme's hero_text property to the bbintro
-- board setting. The default theme is the one marked def, or the lowest tid when none is
-- (the same choice Cache::default_theme makes). An existing bbintro setting is kept.
INSERT INTO settings (name, value)
SELECT 'bbintro', left(trim(properties->>'hero_text'), 300)
FROM themes
WHERE tid = (SELECT tid FROM themes ORDER BY def DESC, tid LIMIT 1)
  AND coalesce(trim(properties->>'hero_text'), '') <> ''
ON CONFLICT (name) DO NOTHING;

-- hero_title was only ever shown in the banner's title, which now comes from the board name.
UPDATE themes SET properties = properties - 'hero_text' - 'hero_title'
WHERE properties ?| array['hero_text', 'hero_title'];
