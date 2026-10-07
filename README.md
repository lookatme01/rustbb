# rustbb

It's a forum. The kind with categories, threads, signatures and a "Who's Online" box at the bottom.
You probably posted on one in 2009.

rbb is [MyBB](https://mybb.com) rebuilt in Rust. If you've ever run a MyBB board, you already know
your way around: same forums, same usergroups, same Admin CP with way too many settings. The
difference is underneath. It's a single binary, Postgres is the only thing it needs, and when your
board gets big you just run more copies of it.

```
cargo build --release && ./target/release/rbb serve
```

## What you get

The stuff you'd expect from a forum:

- Threads, polls, prefixes, unread tracking, RSS feeds
- A rich-text editor that writes MyCode under the hood (and never mangles your old posts)
- Private messages with folders, BCC, read receipts and unsend
- Profiles, avatars, signatures, reputation, warnings and badges
- Alerts and new replies that show up live, no refresh needed
- Dark mode and a layout that works on a phone

And some stuff you might not expect:

- **Encrypted DMs.** Members can sign and end-to-end encrypt private messages with OpenPGP keys
  made in the browser. Not even the server can read them.
- **Passkeys and 2FA.** Sign in with your fingerprint instead of a password.
- **A System account.** A built-in member that sends the automated messages, quarantines spam and
  closes dead threads, so your staff doesn't have to.
- **Moderation that keeps receipts.** Reports you can claim, private notes on members, a full
  history per member, and ban appeals.
- **Privacy settings that actually work.** It can shorten old IP addresses, prune old logs, proxy
  remote images and fully erase an account. Members can download all their data as a zip.
- **A debug panel for admins.** It sits at the bottom of every page and shows every SQL query that
  page ran, how long each one took, and which ones look like an N+1.

## Try it

You need PostgreSQL 17 and Rust. rustup will grab the right Rust version on its own.

```bash
./scripts/dev-db.sh              # spins up Postgres on port 5433 (or point it at your own)
cp .env.example .env             # put something random in RBB_SECRET: openssl rand -hex 32
cargo build --release
./target/release/rbb install --admin-user admin --admin-password 'pick-something' \
    --admin-email you@example.com --board-name "My Board" --board-url http://localhost:8080
./target/release/rbb serve
```

Then open <http://localhost:8080>. The Admin CP lives at `/admin`.

If something's off, run `rbb doctor`. It checks your config, database and mail setup and tells
you what to fix in plain English.

Prefer Docker? Set `RBB_SECRET` and run `docker compose up --build`.

## Running it for real

Grab a binary from [Releases](../../releases). There are builds for x86_64 and ARM64 Linux, and
they run on pretty much any distro from the last ten years. Unpack it, copy `.env.example` to
`.env`, and run `./rbb doctor && ./rbb serve`.

`deploy/` has a locked-down systemd unit and an nginx config if you want them.

Most settings live in the Admin CP. The few that don't are environment variables:

| Variable | What it does |
|---|---|
| `DATABASE_URL` | Where Postgres is. |
| `RBB_SECRET` | A long random string. Use the same one on every server. |
| `RBB_LISTEN` | Address to listen on. Defaults to `127.0.0.1:8080`. |
| `RBB_UPLOAD_DIR` | Where avatars and attachments go. Or set `RBB_STORAGE=s3` to use a bucket. |
| `RBB_TRUST_PROXY` | Set to `true` when you're behind nginx or a load balancer. |
| `RBB_SECURE_COOKIES` | Set to `true` when you're on HTTPS. |

<details>
<summary>The rest of them</summary>

| Variable | Default | What it does |
|---|---|---|
| `RBB_ROLE` | `all` | Any of `web`, `worker`, `scheduler`, comma-separated. |
| `RBB_TRUSTED_PROXIES` | loopback | Which proxies to believe, e.g. `10.0.0.0/8`. |
| `RBB_S3_BUCKET`, `RBB_S3_ENDPOINT`, `RBB_S3_REGION`, `RBB_S3_PREFIX` | – | S3 storage settings. |
| `RBB_DB_MAX_CONNECTIONS` | `32` | Database connections per server. |
| `RBB_MAX_UPLOAD_MB` | `25` | Biggest file someone can upload. |
| `RBB_ADMIN_LISTEN` | – | A private port for `/metrics`, `/livez` and `/readyz`. |
| `RBB_MIGRATE_ON_START` | `true` | Turn off if you run several servers, and use `rbb migrate` instead. |
| `RBB_SHUTDOWN_DRAIN_SECS` | `0` | How long to keep serving after a shutdown signal. |
| `RBB_PAGE_CACHE_MB` | `64` | Memory for caching guest pages. `0` turns it off. |
| `RBB_QUERY_SAMPLE_RATE` | `0.01` | How many requests get their queries timed. |
| `RBB_SLOW_QUERY_MS` | `250` | Log queries slower than this. |
| `RBB_SSE_MAX`, `RBB_SSE_MAX_PER_IP`, `RBB_SSE_MAX_PER_USER` | `10000`, `20`, `8` | Caps on live-update connections. |
| `RBB_PLUGINS_DIR` | `plugins` | Where plugins live. |
| `RBB_PLUGINS_TRUSTED` | `false` | Skip sanitizing plugin HTML. |
| `RBB_LOG_JSON` | `false` | Log as JSON. |
| `RUST_LOG` | `rbb=info,tower_http=warn` | How chatty the logs are. |
| `RBB_DEV_TEMPLATES` | – | Reload templates from this folder on every request. For development. |

</details>

### When one server isn't enough

Run more. Every server talks to the same Postgres and the same uploads (S3 or a shared folder), and
they keep each other in sync through Postgres. You don't need Redis or a message queue. On a busy
board you can split background work into its own processes with `rbb worker` and `rbb scheduler`.

## Moving from MyBB

```bash
rbb import-mybb --mysql-url mysql://user:pass@host/mybb --yes
```

That brings over users, passwords, forums, permissions, threads, posts, PMs and attachments, all
with their original IDs. Old `showthread.php?tid=123` links keep working, and passwords quietly
upgrade to Argon2 the next time each person signs in.

## Other commands

| Command | What it does |
|---|---|
| `rbb migrate --check` | Shows what an upgrade will do to your database without doing it. |
| `rbb check` | Makes sure every post and thread count matches the real data. |
| `rbb recount` | Fixes them if they don't. |
| `rbb seed --users N --threads N --posts N` | Fills a board with fake data so you can load-test it. |

## Making it yours

- **Themes:** override any template, or make a child theme that only changes what you need. The
  default theme can be re-branded from a single colour in the Admin CP.
- **Plugins:** drop a [Rhai](https://rhai.rs) script in `plugins/` and hook into events like
  `post_created` or `user_registered`.
- **API:** `/api/v1` gets you forums, threads, posts, users and search, with the same permissions
  as the website. Start at `GET /api/v1` to see what's there.
- **Translations:** add a `lang/<code>.json` file. `lang/de.json` shows the format.

## Hacking on it

```bash
./scripts/dev-db.sh
RBB_DEV_TEMPLATES=templates cargo run -- serve    # edit templates without rebuilding
cargo test
```

There are also end-to-end scripts in `tests/*.sh` that poke a running board at
`http://127.0.0.1:8088`, plus a load test in `tests/load.sh`.

## License

LGPL-3.0-or-later, same as MyBB.
