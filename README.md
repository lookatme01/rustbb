# rustbb

**rbb** is a fast bulletin board written in Rust, modeled on [MyBB](https://mybb.com). It keeps
MyBB's forums, usergroup permissions, MyCode and User/Mod/Admin control panels, and rebuilds them
for large communities: one binary, PostgreSQL as the only dependency, and app nodes that scale
horizontally.

## Highlights

* **One binary.** Templates, CSS, JS, fonts and icons are embedded. The only service you need is PostgreSQL.
* **Built for big boards.** Counters are denormalized, page views are buffered, parsed posts are
  cached, and every busy page is served from an index.
* **Scales out.** Nodes are stateless. They share caches, tasks and live events through Postgres,
  so you don't need Redis or a message broker.
* **Secure by default.** Argon2id, CSRF on every form, a strict CSP, TOTP and passkeys, rate limits.
* **Familiar to MyBB admins.** It has the same settings and permissions, and a direct importer that keeps original IDs and old URLs.

## Features

* **Forums:** subforums, prefixes, polls, ratings, unread tracking, RSS/Atom, sitemap, archive mode.
* **Posting:** a rich-text editor that writes MyCode, live preview, quick reply, multi-quote,
  drafts, drag-and-drop attachments, edit history, `@mentions`, reactions.
* **Private messages:** folders, BCC, read receipts with unsend, optional end-to-end OpenPGP
  signing and encryption.
* **Members:** profiles with custom fields, avatars, signatures, reputation, warnings, badges,
  real-time alerts, dark mode, a phone-first layout, and a data export.
* **Account security:** passkeys, two-factor authentication, and a per-account activity log of
  sign-ins and changes.
* **Privacy:** an image proxy, IP retention limits, anonymized deleted accounts, and erasure requests.
* **Moderation:** report queue with claiming, inline moderation, soft delete, scheduled actions,
  moderator notes, member history, ban appeals, rule-based spam quarantine.
* **Admin CP:** about 150 settings, per-forum group permissions, themes and template editing,
  mass mail, scheduled tasks, backups, logs, and a debug panel showing each page's queries.
* **System account:** a built-in member that sends automated messages, runs board automation, and
  lets staff post on behalf of the board.

## Quick start

You need **PostgreSQL 17** (12 at minimum). To build from source you also need **Rust**: rustup
installs the version pinned in `rust-toolchain.toml`.

```bash
./scripts/dev-db.sh                 # local PostgreSQL on port 5433, or use your own
cp .env.example .env                # set RBB_SECRET:  openssl rand -hex 32
cargo build --release
./target/release/rbb install --admin-user admin --admin-password 'choose-a-password' \
    --admin-email you@example.com --board-name "My Board" --board-url http://localhost:8080
./target/release/rbb serve
```

Open <http://localhost:8080>. The Admin CP is at `/admin`. If something isn't working, run `rbb doctor`. It
checks the whole setup and explains how to fix each problem.

With Docker, run `docker compose up --build` after setting `RBB_SECRET`.

### Release binaries

[GitHub Releases](../../releases) has single-file Linux builds (x86_64 and ARM64, glibc 2.17+) and
`SHA256SUMS`. Unpack a build, copy `.env.example` to `.env`, then run `./rbb doctor && ./rbb serve`.

## Configuration

Process settings come from environment variables or `.env`. Everything else is configured in the
Admin CP.

| Variable | Default | Purpose |
|---|---|---|
| `DATABASE_URL` | – | PostgreSQL connection string. |
| `RBB_SECRET` | – | Random secret, 32+ characters, identical on every node. |
| `RBB_LISTEN` | `127.0.0.1:8080` | Listen address. |
| `RBB_UPLOAD_DIR` | `uploads` | Attachments and avatars (shared between nodes with `local` storage). |
| `RBB_STORAGE` | `local` | `local` or `s3` (`RBB_S3_BUCKET`, `RBB_S3_ENDPOINT`, `RBB_S3_REGION`, `RBB_S3_PREFIX`). |
| `RBB_TRUST_PROXY` | `false` | Trust `X-Forwarded-For` from `RBB_TRUSTED_PROXIES` (default: loopback). |
| `RBB_SECURE_COOKIES` | `false` | `Secure` cookies and HSTS behind HTTPS. |
| `RBB_ROLE` | `all` | Any of `web`, `worker`, `scheduler`, comma-separated. |

<details>
<summary>More variables</summary>

| Variable | Default | Purpose |
|---|---|---|
| `RBB_DB_MAX_CONNECTIONS` | `32` | Database connections per node. |
| `RBB_MAX_UPLOAD_MB` | `25` | Largest upload; other requests are limited to 2 MiB. |
| `RBB_ADMIN_LISTEN` | – | Internal listener for `/metrics`, `/livez`, `/readyz`. |
| `RBB_MIGRATE_ON_START` | `true` | Apply migrations at start (turn off with several nodes; run `rbb migrate`). |
| `RBB_SHUTDOWN_DRAIN_SECS` | `0` | Keep serving this long after SIGTERM while reporting not ready. |
| `RBB_PAGE_CACHE_MB` | `64` | Guest page cache; `0` turns it off. |
| `RBB_QUERY_SAMPLE_RATE` | `0.01` | Fraction of requests whose queries are measured. |
| `RBB_SLOW_QUERY_MS` | `250` | Log queries slower than this. |
| `RBB_SSE_MAX`, `RBB_SSE_MAX_PER_IP`, `RBB_SSE_MAX_PER_USER` | `10000`, `20`, `8` | Live-update stream caps per node. |
| `RBB_PLUGINS_DIR` | `plugins` | Directory of Rhai plugins. |
| `RBB_PLUGINS_TRUSTED` | `false` | Don't sanitize HTML produced by plugins. |
| `RBB_LOG_JSON` | `false` | JSON log lines. |
| `RUST_LOG` | `rbb=info,tower_http=warn` | Log filter. |
| `RBB_DEV_TEMPLATES` | – | Development: read templates from this directory on every request. |

</details>

## Commands

| Command | Purpose |
|---|---|
| `rbb serve` | Run the server (the default). |
| `rbb install …` | Create the default groups, forums, settings and admin account. |
| `rbb migrate [--check]` | Apply migrations, or rehearse them in a rolled-back transaction. |
| `rbb doctor [--strict]` | Check configuration, database and plugins, and explain fixes. |
| `rbb recount` / `rbb check` | Rebuild or verify denormalized counters. |
| `rbb import-mybb --mysql-url … --yes` | Import a MyBB 1.8 board, keeping IDs, passwords and old links. |
| `rbb seed --users N --threads N --posts N` | Generate a synthetic board for load testing. |

## Deployment and scaling

* `deploy/rbb.service` is a sandboxed systemd unit. `deploy/nginx.conf` terminates TLS and proxies
  the SSE stream. Behind nginx, set `RBB_TRUST_PROXY` and `RBB_SECURE_COOKIES`.
* Run any number of web nodes against one database and shared storage (S3 or a shared directory).
  On busy boards, run `rbb worker` and `rbb scheduler` as separate processes.
* Run `rbb doctor --strict` before starting each node. Back up the database (Admin CP → Tools)
  and the upload directory.

## Extending

* **API:** `/api/v1` covers forums, threads, posts, users, search and alerts, with the web's
  permissions applied. `GET /api/v1` lists the endpoints. Get a token from `POST /api/v1/auth/token`.
* **Plugins:** put [Rhai](https://rhai.rs) scripts in `plugins/`. Hooks include `thread_created`,
  `post_created`, `user_registered` and `parse_message`.
* **Themes:** each theme can override any template, and child themes inherit from their parents.
  The built-in Halo theme can be re-branded from one colour.
* **Translations:** add `lang/<code>.json` (see `lang/de.json`).

## Development

```bash
./scripts/dev-db.sh
RBB_DEV_TEMPLATES=templates cargo run -- serve    # templates reload from disk
cargo test                                        # needs the dev database
```

End-to-end suites in `tests/*.sh` run against a server (default `http://127.0.0.1:8088`). Load tests
are `tests/load.sh` and `tests/simulate.mjs`.

## License

LGPL-3.0-or-later, like MyBB.
