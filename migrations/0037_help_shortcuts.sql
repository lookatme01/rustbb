-- The "Keyboard shortcuts" help document, for boards installed before it was seeded. It goes last
-- in the "Using the Board" section, the same section install seeds it into. If the section has
-- been renamed or removed, or the document already exists, nothing is inserted.
INSERT INTO helpdocs (sid, name, description, document, disporder)
SELECT s.sid,
       'Keyboard shortcuts',
       'Move through forum and thread lists without the mouse.',
       'On the board index, a forum, or any list of threads:

[list]
[*][b]j[/b] — next forum or thread
[*][b]k[/b] — previous forum or thread
[*][b]n[/b] — next one with new posts
[*][b]Enter[/b] — open it
[/list]

Shortcuts are ignored while you are typing in a box, so they never get in the way of writing a post. In the editor, [b]Ctrl+B[/b], [b]Ctrl+I[/b] and [b]Ctrl+U[/b] (⌘ on a Mac) make text bold, italic or underlined, and [b]Ctrl+Enter[/b] posts.',
       (SELECT coalesce(max(d.disporder), 0) + 1 FROM helpdocs d WHERE d.sid = s.sid)
FROM helpsections s
WHERE s.name = 'Using the Board'
  AND NOT EXISTS (SELECT 1 FROM helpdocs WHERE name = 'Keyboard shortcuts')
ORDER BY s.sid
LIMIT 1;
