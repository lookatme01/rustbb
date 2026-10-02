# Changelog

## Unreleased

### Added: privacy controls (2026-10-02)
- New **Privacy** settings group. *Shorten IP addresses after (days)* truncates stored IPs to their
  network in posts, messages, logs, poll votes and the System log. *Keep account activity log*,
  *Keep spam log* and *Keep mail log* set retention (the activity and spam logs were fixed at 365
  and 90 days; mail logs were kept forever). A daily "Privacy Retention" task applies them. The
  defaults keep today's behavior.
- *Anonymize kept posts of deleted accounts* (off by default): posts a deleted member keeps are
  shown under a configurable name ("Former member"), and their IPs are removed from posts, votes,
  ratings and messages; their mail and search logs are deleted.
- **Erase personal data** in the Admin CP user editor, for erasure requests: deletes the account,
  always anonymizes what stays, needs the username typed to confirm, and records the former ID,
  who did it and an optional reference in the new Erasure log, without personal data.
- `{retention}` in the privacy policy expands to sentences describing the current Privacy
  settings; the default policy uses it.

### Added: moderation workflow (2026-10-02)
- **Moderator notes:** an append-only, staff-only log of notes per member, replacing the single
  "Moderator notes" box. Existing notes (also from MyBB imports) are carried over. Authors can
  retract a note within 15 minutes and administrators at any time; retracted notes stay visible.
- **Member moderation history** at `/modcp/member/{uid}`: notes, warnings, bans and account
  actions, reports, moderator actions and ban appeals on one filterable timeline. It's linked from
  profiles ("History (N notes)"), reports and the user editors.
- **Report claiming:** claim, release or take over a report; resolve it with a note; reopen it.
  Each report has a detail page with its reporters and full history, and report rows show the
  claimer and the member's latest note.
- **Ban appeals:** banned members can appeal from the banned page. Staff with ban rights see a
  queue (with a count in the Mod CP menu) and accept or reject with a response; members get the
  outcome on the banned page and as a System message. New settings: *Allow Ban Appeals* and
  *Days Before Appealing Again*.

### Security (moderation workflow, 2026-10-02)
- Resolving or reopening reports now requires the same permission as viewing them. Before,
  Mod CP access alone was enough to close profile, reputation and private-message reports.

### Added: `rbb doctor` (2026-10-02)
- New `rbb doctor [--strict]` command checks the setup and explains how to fix each problem:
  the secret, listen address, upload folder, plugins, database connection and authentication,
  PostgreSQL version, the `pg_trgm` extension, table privileges, migrations (pending, from a newer
  release, edited or failed), connection limits across nodes, installation, the System account,
  the board URL against cookie and proxy settings, and mail. It never changes anything.
- When `rbb serve` or `rbb migrate` fails on a known database problem, the error now says how to
  fix it (for example `ident` authentication in pg_hba.conf, or the missing contrib package).

### Added: System account features (2026-10-01)
- **System sends the board's automated messages.** Warning notices, subscription and moderation
  notices and mass-mail PMs come from the System account instead of an anonymous "(system)"
  sender. Older messages are reassigned on start-up. System messages can't be replied to or
  reported and say so, with a link to the contact page.
- **Staff can speak as System.** New group permission *Can post as the System account?*
  (administrators only by default). It adds a "Post as the System account" option to new threads,
  replies, private messages and Mod CP announcements. Who really wrote it, and their IP, is kept in
  Admin CP → Logs → System log; the content itself stores no IP. Not available through the API.
- **Automation** (Admin CP → Settings → System Account): a welcome message for new members when
  their account becomes active; a "Close Inactive Threads" task (by age, optionally limited to some
  forums); and moderator-log entries, as System, for expired bans, suspensions and warnings.
- **System's profile** shows its real time online and what it has done. Staff who can read the
  moderator log also see its latest actions. Time online of 0 now reads "0 minutes" instead of
  "(Hidden)".

### Security (soft-deleted content, 2026-10-01)
- Members and guests could see soft-deleted threads: open them, find them in forum listings
  (with the first post's text in the link tooltip), print them and view them in the archive.
  The REST API also returned the full text of soft-deleted posts to anyone.
  "Can view deletion notices?" now only shows the "This post was deleted." placeholder for
  deleted replies inside a live thread. Deleted threads and all deleted content are visible to
  moderators only (`Ctx::visible_states` vs the new `Ctx::listed_states`). Regression test:
  `tests/deleted_visibility.sh`.

### Security (System account work, 2026-10-01)
- Usernames, thread subjects and post excerpts inserted into automated private messages
  (subscription notices, custom moderator tools, mass-mail PMs) are now literal text. Before,
  a username or subject containing MyCode could add links or images to messages other members
  received.

### Security (audit #1, 2026-09-27)
- Quoting (`/newreply/{tid}?pid=` and the multiquote cookie) now applies full thread checks;
  posts in password-protected or "own threads only" forums could be quoted by anyone.
- Login `return_to` redirects reject tabs, newlines and backslashes (open redirect).
- The HTML sanitizer for HTML-enabled forums and calendars escapes unterminated tags and blocks
  `position` styles; cached posts are re-rendered by migration 0006.
- Attachments of soft-deleted or unapproved posts are no longer downloadable; per-forum
  attachment permissions and file-type rules are enforced when files are bound to a post.
- Uploaded images are decoded with size and memory limits (decompression bombs).
- Moderators need thread-level delete permission to delete a thread via its first post.
- Web and API logins share one credential check: identical error messages, constant-time
  handling of unknown users, lockout counting for API attempts, per-account throttling.
- Two-factor codes are single-use and attempts are limited per account.
- `X-Forwarded-For` is read from the proxy-appended (last) entry when `RBB_TRUST_PROXY=true`.
- rbb refuses to start with an empty, placeholder or short `RBB_SECRET`; auto-install uses a
  random admin password instead of `admin12345`; `/healthz` no longer returns database errors.
- HSTS (with `RBB_SECURE_COOKIES=true`) and `Permissions-Policy` headers.
- Password re-entry in the User CP is throttled; members can only leave publicly joinable
  groups; the per-day member email limit is enforced without mail logging; referrals pages
  honour "can view profiles"; titles-only search runs under the search concurrency cap and timeout.
- No new threads in link forums (web and API).
- Lifting a ban checks the user's pre-ban groups, so moderators can't restore users who outrank
  them; splitting a thread applies the same target-forum rules as moving one.

### Changed
- **New default theme, "Halo".** A rebuilt look for the public pages:
  - An index hero banner.
  - Categories as grouped cards, with icon tiles, avatars on last posts and inline counts.
  - Thread lists with starter and last-poster avatars.
  - Posts with a left author column (avatar, role, stars, stats) that stays in view on long
    posts and folds into a compact header on phones.
  - Profile cover cards, a community stats strip, and a centred sign-in card.
  - System fonts: SF on Apple devices, Segoe UI Variable on Windows, and no web-font download.
  - One brand colour drives every accent in light and dark mode.
- **Theme branding without CSS:** Admin CP → Themes now has a Branding section:
  - Brand colour, with a live preview.
  - Default colour mode.
  - Banner title and text.
  - Banner image, with upload, resizing and JPEG compression.
  - Logo.

  Midnight now takes its accent from a brand colour.

### Performance
- **Guest page cache.** Finished HTML of the index, forums, threads, profiles, member list,
  portal, archive and help pages is served from memory to guests and crawlers:
  - Per-visitor CSRF tokens are substituted on every response.
  - Any write clears the cache on every node; other nodes hear within 50 ms through coalesced
    `LISTEN/NOTIFY`.
  - Entries expire after 30 s regardless.
  - `RBB_PAGE_CACHE_MB` sets its size (default 64, 0 turns it off).

  Guest page throughput went from ~1.2–2.8k to ~9–26k req/s on the 3M-post test board.
- Static assets are served with content-hash URLs (`asset()` in templates) and a one-year immutable
  cache, pre-compressed once with Brotli and gzip, and answer `If-None-Match` with 304.
- The per-request template context reuses prebuilt settings and theme values, and templates are
  compiled at start-up.
- Avatars in lists come from a short-lived in-memory cache (one batched query per page at most).
- Guest thread views no longer create a captcha row unless guests can actually reply.

### Fixed
- Page-specific template variables were silently overridden by global ones with the same name,
  because minijinja's `merge_maps` gives the last map precedence. The effects were:
  - The Admin CP theme and template editors always showed and saved the viewer's current theme.
  - The per-forum `is_mod` flag was replaced by "moderates any forum", so mod tools showed in
    forums the user doesn't moderate (the actions themselves were still permission-checked).

### Added
- **End-to-end identity for private messages (OpenPGP).** Keys are generated, stored and used in
  the browser (OpenPGP.js, Curve25519, Argon2-protected); the server only receives public keys,
  with a proof-of-possession signature bound to the account, plus an optional passphrase-locked
  backup it refuses to store unencrypted. Messages can be signed (a detached signature over sender,
  recipients, subject, body, time and board) or signed and encrypted end-to-end; the server
  rejects signatures that don't match the submission and ciphertext that isn't addressed to every
  recipient, and recipients' browsers re-verify everything. Members verify each other with Signal-
  style safety numbers, QR codes (scan or paste), and verification records signed by the
  verifier's own key. Key changes are pinned per browser and announced to everyone who verified the
  member. New pages: User CP → Encryption & identity, `/pm/verify/{uid}`, and `/user/{uid}/pgp.asc`;
  profiles show the identity key. Migration 0007. Tests: `node tests/pgp_e2e.mjs`.
- **System account:** a built-in bot member ("System") in its own protected group with
  administrator permissions, created automatically on start and after a MyBB import. It is always
  shown online (listed but not counted), never signs in, and cannot be deleted, banned, merged,
  emailed, sent PMs, or moved out of its group. Database triggers enforce this for every code path.
  Admins can change its name, title, signature and avatar. It is the foundation for automated
  reporting, task ownership and, later, autonomous moderation.

## 0.5.0

### Added
- **MyBB importer** (`rbb import-mybb`): users (with their existing MyBB passwords), groups,
  forums, forum permissions, moderators, prefixes, threads, posts, polls, private messages,
  attachments, subscriptions and reputation, all with their original IDs. Tested against the
  MyBB 1.8.41 schema. See `docs/IMPORT.md`.
- Permanent redirects for legacy MyBB URLs (`showthread.php`, `forumdisplay.php`, `member.php`,
  `private.php`, `newreply.php`, `syndication.php`, `archive/index.php`, …).
- **Account activity log** (User CP → Account activity; Admin CP → user → Account activity).
- **Admin debug panel** showing page timing and every SQL query per page (setting `debugpanel`).
- **Relay** default theme: a new visual design, self-hosted Onest typeface, new logo, and a
  refreshed Midnight theme.
- "Who posted?" (`/thread/{tid}/whoposted`) and "Send thread to a friend" (`/thread/{tid}/send`).
- Links are prefetched on hover. Requests marked `Sec-Purpose: prefetch` have no visit side effects.

### Performance
- The board index went from 333 to 3,266 req/s on a board with 3M posts: the online list,
  statistics and birthdays are now cached, and birthdays use a prefix index.
- Deep pages of forums and threads read the index from the far end: the last page of a
  forum went from 54 to 1,698 req/s.
- Similar threads use a bounded trigram query and are cached per thread.
- Full-text search is limited in concurrency and capped at 5 s per query.
- RSS feeds are built from a per-forum top-N query and cached (26 → 12,635 req/s).
- `rbb check` is set-based (176 s → 11 s on 3M posts).
- "You posted here" dots on thread lists use a `(uid, tid)` index probe per thread: hot forum pages
  for active members went from 240 to 1,668 req/s.
- Identical searches by the same viewer within 30 s reuse the stored results.
- New migrations are embedded on the next build (`build.rs` watches `migrations/`).

### Fixed
- Merging threads now keeps the thread you are viewing, as MyBB does.

## System automated moderation — 2026-09-30

- Durable new/edit moderation queue; local phrase, link-burst and repetition rules.
- System-attributed decisions, observation/off/quarantine modes, staff exemptions.
- Atomic visibility/counter/audit updates, retryable failures and multi-node locking.
- ACP review, individual/bulk undo, conflict protection and versioned policy restore.
- Original-source backups and hash-checked rollback; see docs/automated-moderation.md.
