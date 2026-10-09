# ambient occlusion (working name)

A light rbb theme built from the Paper design in "Engaging frost": a soft grey ground (`#f5f5f7`), white bound panels with hairlines, Onest throughout, and one cobalt accent (`#1f45e0`).

- **Top bar to dock.** The top bar scrolls away. A dark floating dock (Forums, Unread, Messages, Search, the page title, and Reply on threads) fades in at the bottom. A 3px cobalt line at the top of thread pages shows reading progress. Both use CSS scroll-driven animations, so the theme ships no script; the CSP doesn't allow inline scripts anyway. Browsers without support keep a sticky top bar. On phones the dock is always shown and acts as the tab bar.
- **Index.** Each category is one panel with Threads / Posts / Last post columns. Who's Online and the board stats are folded into one line under the page title.
- **Threads.** Classic posts: an author card on the left, the body at 17/28 with a 700px measure, quotes in an inset grey well.
- **Messages.** Folder pills, a conversation-style list, and a single message shown as a chat bubble with a reply pill.

The theme defaults to light (`colormode: light`). Dark tokens exist but aren't designed yet.

## Files

`ambient-occlusion.css`, `layout.html`, `index.html` and `pm/{base,folder,read}.html` are the source files. `python3 build.py` bundles them into `ambient-occlusion.json`, which can be imported in Admin CP → Themes. On the local board it is installed as theme 4.
