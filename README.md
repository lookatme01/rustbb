# rustbb

**rbb** is a fast, production-ready bulletin board written in Rust, modeled on
[MyBB](https://mybb.com). It keeps MyBB's concepts (forums and subforums, usergroups with
per-forum permissions, MyCode, the User/Moderator/Admin control panels, themes and templates,
reputation, warnings, private messages, calendar, portal…) and rebuilds them for large
communities: one binary, PostgreSQL as the only dependency, and app nodes that scale horizontally.

```
cargo build --release && ./target/release/rbb serve
```

---

## Contents

- [Highlights](#highlights)
- [Features](#features)
- [Requirements](#requirements)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Commands](#commands)
- [Architecture](#architecture)
- [Project layout](#project-layout)
- [Security](#security)
- [Performance](#performance)
- [Scaling out](#scaling-out)
- [Deployment](#deployment)
- [Migrating from MyBB](#migrating-from-mybb)
- [JSON API](#json-api)
- [Plugins](#plugins)
- [Themes and templates](#themes-and-templates)
- [Translations](#translations)
- [Development and testing](#development-and-testing)
- [License](#license)

---

## Highlights

* **One binary.** Templates, CSS, JavaScript, fonts and icons are embedded and fingerprinted.
  The only external service is PostgreSQL.
* **Built for big boards.** Denormalized counters maintained transactionally, batched activity
  and view tracking, parsed post HTML cached in the database, permission and configuration caches
  in memory, and an index behind every hot page.
* **Horizontally scalable.** App nodes are stateless (sessions live in Postgres). Caches are
  invalidated across nodes with `LISTEN/NOTIFY`, scheduled tasks use advisory locks, and live
  events fan out through Postgres too. No Redis or message broker needed.
* **Secure by default.** Argon2id passwords, CSRF on every form, a strict Content Security Policy
  with no inline script, auto-escaping templates, an allow-list MyCode renderer, rate limiting,
  TOTP two-factor authentication, Admin CP re-authentication, login lockout, and registration
  defenses (honeypot, captcha, security questions).
* **Familiar to MyBB admins.** The same settings, permissions and control panels, plus a direct
  importer that keeps original IDs and old URLs working.

## Features

### Community

* Categories, forums and unlimited subforums; link forums; password-protected forums; forum rules;
  per-forum themes; forum jump; "users browsing this forum"; subscriptions; mark read.
* Threads with prefixes, icons, sticky/closed/moved (redirect) states, polls (single or multiple
  choice, public, timeouts, undo votes), ratings, views, "hot" threads, unread indicators, similar
  threads, print view, lite (archive) mode, RSS 2.0 / Atom feeds and an XML sitemap.
* Posting with a MyCode toolbar, live preview, quick reply, multi-quote, drafts (server-side plus
  local autosave), drag-and-drop or paste attachments with thumbnails, edit history, edit reasons,
  edit time limits, automatic double-post merging, flood control, per-day post limits, and guest
  posting with captcha.
* **MyCode:** b/i/u/s, sup/sub, color, size, font, align, url, email, img (sizes and alignment),
  quote (with author, post and date), code (with language), php, list (bullets, numbers, letters,
  roman), hr, spoiler, video (YouTube, Vimeo, Dailymotion, Twitch), attachment, `/me`,
  `@mentions`, auto-links, smilies, word filters and admin-defined custom MyCode.
* **Private messages:** folders, To/BCC, drafts, read receipts and tracking (with unsend), quotas,
  friends-only mode, email notifications and search.
* **Profiles:** custom profile fields, avatars (upload with resizing, remote URL, Gravatar),
  signatures, away status, birthdays, time online, referrals, reputation (including per-post
  reputation) and warnings. Banned members' profiles show a ban notice with the reason and expiry;
  staff also see who issued the ban.
* **Real-time updates:** new replies and alerts arrive over Server-Sent Events.
* **Alerts** for quotes, mentions, replies, reactions, private messages and reputation;
  post reactions.
* Dark mode, responsive phone-first layout, session management ("log out other devices"),
  GDPR data export and account self-deletion, language packs.

### Account security

* **Two-factor authentication** (TOTP) with single-use codes and per-account attempt limits.
* **Account activity log:** every account has an audit trail in the User CP covering sign-ins,
  failed attempts and lockouts; password, email, username and two-factor changes; devices signed
  out; API tokens; data exports; and staff actions such as bans, warnings and edits. Each entry
  records the IP and browser. Members are warned about recent failed sign-ins, and admins can open
  any member's log from the Admin CP.
* **End-to-end identity for private messages:** members create an OpenPGP key in their browser
  (User CP → Encryption & identity), then sign messages, encrypt them end-to-end, and verify each
  other by comparing a safety number or scanning a QR code. Recipients' browsers check every
  signature against the sender's key and flag verified senders, changed keys and tampered messages.
  Keys can be backed up (passphrase-locked), restored on other devices, rotated with a vouch from
  the old key, exported to GnuPG, or revoked. See [docs/PGP.md](docs/PGP.md).

### The System account

A built-in, always-online member that automated features act as. It can't sign in, be banned,
deleted or merged (enforced by database triggers), and admins can rename it and change its avatar.

* **Sends the board's automated messages:** warning notices, subscription notices, moderator-tool
  messages and mass-mail PMs. Members can't reply to or report them; they're pointed to the
  contact page instead.
* **Automated moderation:** evaluates new and edited posts against configurable spam rules, with an
  observation mode, reversible quarantine and full policy history. See
  [docs/automated-moderation.md](docs/automated-moderation.md).
* **Board automation** (Admin CP → Settings → System Account): a welcome message for new members,
  a task that closes inactive threads (optionally limited to selected forums), and moderator-log
  entries for expired bans, suspensions and warnings.
* **Staff can speak as System:** groups with *Can post as the System account?* (administrators by
  default) can post threads, replies, announcements and private messages as System. The real
  author and their IP are kept in Admin CP → Logs → System log; the content itself stores no IP.
* **Profile:** shows what System has done (quarantines, automated messages, closed threads, staff
  posts); staff who can read the moderator log also see its latest actions.

### Moderation (Mod CP)

Reports queue; moderation queue (threads, posts, attachments); inline moderation of threads and
posts (approve, soft delete and restore, delete, open/close, stick, move or copy with optional
redirects, merge, split, move posts); custom moderator tools; delayed (scheduled) moderation;
moderator log; announcements; profile editing and restrictions (suspend posting, moderate posts,
suspend signature); bans with expiry; IP search; warning logs. Soft-deleted content is visible only
to moderators. Other members see just a "This post was deleted." placeholder for a deleted reply.

### Administration (Admin CP)

Board settings (about 150 settings in 20 searchable groups); forum management with a per-group
permission matrix and forum moderators; users (search, add, edit, merge, delete, activate); user
groups with about 80 permissions; group leaders and public or join-request groups; user titles;
promotions; admin permissions; ban filters (IP with CIDR, username, email); themes (inheritance,
stylesheets, properties, import/export) and per-theme template editing with syntax validation;
smilies; post icons; custom MyCode; word filters; attachment types; profile fields; thread
prefixes; report reasons; security questions; help documents; calendars; warning types and
levels; mass mail (email or PM, batched); scheduled tasks; recount and rebuild; cache manager;
database backup (`pg_dump`); system health; statistics; admin, moderator, mail, spam and System logs;
plugin list.

**Admin debug panel:** administrators see a panel at the bottom of every page with total time,
handler and template-render time, query count and combined query time, response size, pool usage,
and each SQL statement with its timing and row count. Repeated statements are flagged as likely
N+1 patterns. It costs nothing for other visitors and can be turned off under Settings → General.

### Look and feel

The default **Halo** theme has light and dark modes, a hero banner, avatars throughout and a
phone-first layout. Admins re-brand it without writing CSS: one brand colour (every accent is
derived from it), default colour mode, banner image and text, and logo. A dark **Midnight** theme
is included, and both can be extended with child themes.

## Requirements

* **PostgreSQL 17** is recommended: it's what rbb is developed and tested against. Version 12 is
  the hard minimum (generated columns); versions between 12 and 17 are untested.
* **Rust 1.85 or newer** (edition 2024) to build from source; or Docker.
* Optional: an SMTP server for outgoing mail; MySQL/MariaDB access to import a MyBB board.

## Quick start

```bash
# 1. PostgreSQL: a project-local cluster on port 5433 (or use your own / docker compose)
./scripts/dev-db.sh

# 2. Configure
cp .env.example .env
#    set RBB_SECRET to a long random value:  openssl rand -hex 32

# 3. Build, install and run
cargo build --release
./target/release/rbb install --admin-user admin --admin-password 'choose-a-password' \
    --admin-email you@example.com --board-name "My Board" --board-url http://127.0.0.1:8080
./target/release/rbb serve
```

Open <http://127.0.0.1:8080>, sign in, and visit the **Admin CP** at `/admin`.

Something not working? `rbb doctor` checks the whole setup (secret, upload folder, database
connection and authentication, required extensions, migrations, connection limits, mail) and
says how to fix each problem.

If you start `rbb serve` on an empty database it installs itself with an `admin` account and a
random password, printed once in the server log. Change it after signing in.

### Docker

```bash
docker compose up --build
```

`docker-compose.yml` starts PostgreSQL and rbb together; set `RBB_SECRET` in the environment first.

## Configuration

Process configuration comes from environment variables, optionally loaded from a `.env` file.
Everything else (about 150 board settings, permissions, themes, templates) is configured in the
Admin CP and stored in the database.

| Variable | Default | Purpose |
|---|---|---|
| `DATABASE_URL` | – | PostgreSQL connection string. |
| `RBB_LISTEN` | `127.0.0.1:8080` | Address and port to listen on. |
| `RBB_SECRET` | – | Long random secret for CSRF tokens, captchas, signed cookies and 2FA challenges. Must be identical on all nodes. rbb refuses to start with a placeholder or anything under 32 characters. |
| `RBB_DB_MAX_CONNECTIONS` | `32` | Database connections per node. |
| `RBB_UPLOAD_DIR` | `uploads` | Attachments and avatars. Share it between nodes (NFS, EFS…). |
| `RBB_TRUST_PROXY` | `false` | Read client IPs from `X-Forwarded-For` (the proxy-appended entry) behind a reverse proxy. |
| `RBB_SECURE_COOKIES` | `false` | Mark cookies `Secure` and send HSTS when served over HTTPS. |
| `RBB_RUN_TASKS` | `true` | Run scheduled tasks on this node (safe on every node: tasks take advisory locks). |
| `RBB_PLUGINS_DIR` | `plugins` | Directory of Rhai plugins. |
| `RBB_PAGE_CACHE_MB` | `64` | Memory for the guest page cache; `0` turns it off. |
| `RBB_LOG_JSON` | `false` | JSON log lines for log shipping. |
| `RUST_LOG` | `rbb=info,tower_http=warn` | Log filter. |
| `RBB_DEV_TEMPLATES` | – | Development only: read templates from this directory on every request. |

## Commands

| Command | Purpose |
|---|---|
| `rbb serve` | Run the web server (applies migrations first). The default command. |
| `rbb migrate` | Apply database migrations only. |
| `rbb install …` | Create the default groups, forums, settings and the admin account. |
| `rbb seed --users N --threads N --posts N` | Generate a synthetic board for load testing. |
| `rbb recount` | Rebuild all denormalized counters. |
| `rbb check` | Compare denormalized counters with freshly computed values and report differences. |
| `rbb doctor [--strict]` | Check configuration, database, board and plugins, and explain how to fix each problem. Exits 1 on failures (or on warnings with `--strict`). Read-only. |
| `rbb import-mybb --mysql-url … --yes` | Import a MyBB 1.8 board from its MySQL database. |

## Architecture

```
            ┌─────────────┐   ┌─────────────┐   ┌─────────────┐
 browsers ─▶│  rbb node   │   │  rbb node   │   │  rbb node   │ ◀─ load balancer
            │ axum + tokio│   │             │   │             │
            │ caches, SSE │   │             │   │             │
            └──────┬──────┘   └──────┬──────┘   └──────┬──────┘
                   │  SQL · LISTEN/NOTIFY · advisory locks │
                   └───────────────┬───────────────────────┘
                            ┌──────▼──────┐        ┌──────────────┐
                            │ PostgreSQL  │        │ shared upload│
                            └─────────────┘        │  directory   │
                                                   └──────────────┘
```

* **HTTP:** [axum](https://github.com/tokio-rs/axum) on tokio, with tower middleware for
  compression, timeouts, request IDs, body limits and panic recovery.
* **Data:** [sqlx](https://github.com/launchbadge/sqlx) against PostgreSQL. Schema changes are
  plain SQL migrations in `migrations/`, applied at start-up.
* **Request context:** every handler receives a `Ctx` with the viewer, their merged group
  permissions, theme, CSRF token and helpers such as `visible_states` (which thread and post states
  the viewer may see in a forum).
* **Permissions:** typed permission structs stored as JSONB, so new permissions need no migration.
  A member's effective permissions merge all of their groups. Booleans OR together, and limits take
  the most generous value.
* **Caching:** board configuration (settings, forums, groups, themes, templates, parser data) lives
  in an atomically swapped in-memory cache, reloaded on every node via `NOTIFY`. Parsed post HTML
  is stored with a parser revision and re-rendered lazily when MyCode, smilies or filters change.
  Finished guest pages are cached in memory.
* **Templates:** [minijinja](https://github.com/mitsuhiko/minijinja), compiled at start-up, with
  per-theme overrides stored in the database.
* **Background work:** a scheduler runs tasks (cleanup, ban lifting, warning expiry, promotions,
  mass mail, automated moderation, closing inactive threads…). Activity and view counts are
  buffered and flushed in bulk, and outgoing mail is queued and delivered by a worker.
* **Live updates:** Server-Sent Events, fanned out across nodes through Postgres.

More detail: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Project layout

```
src/
  main.rs            CLI entry point (serve, migrate, install, seed, recount, check, import-mybb)
  app.rs             shared state, cache invalidation, activity batching
  ctx.rs             per-request context: viewer, permissions, CSRF, visibility helpers
  config.rs          environment configuration
  auth.rs            password hashing, login tokens, ban filters
  perms.rs           usergroup, forum and moderator permission models
  settings.rs        board settings registry (metadata and defaults)
  cache.rs           in-memory configuration caches
  pagecache.rs       guest page cache
  parser.rs          MyCode parser and HTML sanitizer
  render.rs          posts and users to template data ("postbit"), parse caching
  posting.rs         creating and editing threads and posts
  ops.rs             state changes that keep denormalized counters consistent
  notify.rs          subscriptions, alerts, mentions
  mail.rs            queued outgoing email
  tasks.rs           scheduled tasks
  system.rs          the System account and its automation
  automod.rs         rule-based automated moderation
  audit.rs           per-account activity log
  pgp.rs             OpenPGP validation for private messages
  plugins.rs         Rhai plugin hooks
  templates.rs       template engine setup
  i18n.rs            translations
  import.rs          MyBB importer
  seed.rs            synthetic data for load tests
  admin/             Admin CP (settings, forums, users, groups, themes, tools, mass mail, …)
  routes/            public pages, User CP, Mod CP, moderation, private messages, API, feeds
migrations/          SQL schema migrations
templates/           page templates (overridable per theme)
static/              CSS, JavaScript, fonts, icons, smilies (embedded at build time)
lang/                language packs
plugins/             example Rhai plugin
deploy/              systemd unit and nginx configuration
scripts/             development database helper
tests/               end-to-end, regression and load-test scripts
docs/                architecture, performance, import, PGP and automated moderation guides
```

## Security

* **Authentication:** Argon2id password hashing on a blocking pool; constant-time handling of
  unknown users; one credential check shared by web and API logins; lockout after repeated failures;
  per-account throttling; optional TOTP with single-use codes; Admin CP re-authentication (and 2FA
  if required by the board).
* **Requests:** CSRF tokens on every form; `SameSite` and `HttpOnly` cookies; strict CSP with no
  inline script; `X-Content-Type-Options`, `Permissions-Policy`, and HSTS when cookies are secure.
* **Content:** auto-escaping templates; an allow-list MyCode renderer; a sanitizer for forums that
  allow HTML; names and subjects inserted into generated messages as literal text, so they can't
  inject markup.
* **Access control:** every content endpoint (pages, feeds, archive, API, attachment downloads)
  applies forum permissions and visibility. Unapproved and soft-deleted content is shown only to
  moderators with the matching permission.
* **Abuse prevention:** rate limits, flood control, registration honeypot, built-in SVG captcha,
  security questions, ban filters by IP range, username and email, and decompression-bomb-safe image
  decoding.
* **Accountability:** account activity logs, moderator log, admin log, and a System log recording who
  published content as the System account.

## Performance

* **Guests are served from memory.** The finished HTML of public pages is cached for anonymous
  visitors and crawlers, with each visitor's CSRF token substituted per response. Any write clears
  it on every node (coalesced `NOTIFY`), and entries live at most 30 seconds. Size it with
  `RBB_PAGE_CACHE_MB`.
* **Assets are immutable.** CSS and JS URLs carry a content hash and are cached for a year; they
  are pre-compressed with Brotli and gzip at start-up.
* **Hot pages are a handful of index lookups.** Forum listings use `(fid, sticky, lastpost)`, and
  thread pages use `(tid, dateline, pid)`. Counters and "last post" columns are denormalized and
  updated with O(1) deltas when posting.
* **No write per page view.** Who's Online activity and thread views are buffered and flushed in
  bulk every few seconds with `UNNEST` upserts.
* **Parse once.** Post HTML is cached in `posts.message_html`; signatures are cached in memory.
* **Full-text search in Postgres.** A generated `tsvector` column with a GIN index; searches are
  permission-filtered, capped in concurrency and runtime, and cached.
* **Deep pages stay fast.** Pages past the middle of a forum or thread are read from the other end
  of the index, so the last page of a 20,000-post thread costs the same as the first.

`tests/load.sh` runs a load test against a seeded board, and `tests/simulate.mjs` simulates
realistic browsing traffic. Measured results: [docs/PERFORMANCE.md](docs/PERFORMANCE.md).

## Scaling out

Run any number of `rbb serve` nodes behind a load balancer, pointing at the same PostgreSQL
database and a shared upload directory. Sessions live in Postgres; caches and live events are
synchronized with `LISTEN/NOTIFY`; scheduled tasks take advisory locks so each runs once. For very
large boards, add PgBouncer and read replicas as usual.

## Deployment

* **systemd:** `deploy/rbb.service` runs rbb as an unprivileged user with sandboxing
  (`NoNewPrivileges`, `ProtectSystem=strict`, private `/tmp`), reading `/opt/rbb/.env`.
* **nginx:** `deploy/nginx.conf` terminates TLS and proxies to rbb, including the long-lived SSE
  stream. Set `RBB_TRUST_PROXY=true` and `RBB_SECURE_COOKIES=true` behind it.
* **Docker:** the multi-stage `Dockerfile` builds a small Debian image that runs as a non-root user,
  with a health check on the listening port.
* **Backups:** Admin CP → Tools → Database backup runs `pg_dump`; back up the upload directory too.
* **Preflight:** run `rbb doctor --strict` on each node before starting it (for example as a
  systemd `ExecStartPre=` or a deploy step). It fails fast on misconfiguration, such as PostgreSQL
  rejecting the password with `ident` authentication or the `pg_trgm` extension missing (install
  the distribution's PostgreSQL contrib package).

## Migrating from MyBB

```bash
DATABASE_URL=postgres://… rbb import-mybb --mysql-url mysql://user:pass@host/mybb --yes
```

The import keeps users, passwords (upgraded to Argon2id on first sign-in), groups, forums,
permissions, moderators, threads, posts, polls, private messages, attachments, subscriptions and
reputation, all with their original IDs. Old `showthread.php?tid=…`-style links redirect
permanently. See [docs/IMPORT.md](docs/IMPORT.md).

## JSON API

`/api/v1`: `GET /api/v1` lists the endpoints. Authenticate with
`POST /api/v1/auth/token {"username", "password", "code"?}` and send `Authorization: Bearer <token>`.

```bash
TOKEN=$(curl -s -H 'Content-Type: application/json' -d '{"username":"admin","password":"…"}' \
  http://127.0.0.1:8080/api/v1/auth/token | jq -r .token)
curl -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"subject":"Hello","message":"From the API"}' http://127.0.0.1:8080/api/v1/forums/3/threads
```

Endpoints cover forums, threads, posts, users, search, statistics and alerts, and apply the same
permissions as the web interface.

## Plugins

Drop [Rhai](https://rhai.rs) scripts into `plugins/` (see `plugins/example.rhai.disabled`).
Functions named after hooks run on events: `thread_created(data)`, `post_created(data)`,
`user_registered(data)`, and the filter `parse_message(html) -> html`.

## Themes and templates

Every page is a Jinja2-style template. Admins can override any template per theme in the Admin CP,
with syntax validation; child themes inherit templates and stylesheets from their parents. The
stylesheet is driven by CSS custom properties, so most restyling is a few lines of CSS.

## Translations

Templates call `t("English text")`. Add `lang/<code>.json` mapping English strings to translations
(see `lang/de.json`) and rebuild; members pick a language in the footer.

## Development and testing

```bash
./scripts/dev-db.sh                               # local PostgreSQL on port 5433
RBB_DEV_TEMPLATES=templates cargo run -- serve    # templates reload from disk
cargo test                                        # unit and integration tests (needs the dev DB)
```

End-to-end scripts run against a server (default `http://127.0.0.1:8088`, admin password as the
second argument where applicable):

| Script | Covers |
|---|---|
| `tests/smoke.sh` | registration, posting, MyCode, editing, reactions, reports, API |
| `tests/admin_smoke.sh` | Admin CP pages and common admin actions |
| `tests/moderation.sh` | every moderation state change, with counter checks after each step |
| `tests/system.sh` | System account protections |
| `tests/system_features.sh` | System sender, staff posting as System, automation, profile, ban notice |
| `tests/deleted_visibility.sh` | soft-deleted content hidden from members, guests and the API |
| `tests/doctor.sh` | `rbb doctor` against the dev setup and deliberately broken ones |
| `tests/pgp_e2e.mjs` | end-to-end PGP flows with the shipped browser module |
| `tests/load.sh` | load test against a seeded board |
| `tests/simulate.mjs` | realistic traffic simulation with optional ramp-up |

`rbb check` verifies that every denormalized counter matches the data. The moderation and System
scripts call it after state changes.

## License

LGPL-3.0-or-later, like MyBB.
