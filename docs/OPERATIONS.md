# Operating rbb

This is the runbook: how to run rbb as a service, watch it, and recover it.

## Processes and roles

One binary does everything; which parts a process runs is its *role* (`RBB_ROLE` or
`rbb serve --role`, comma-separated):

| Role | Does | Scale |
| --- | --- | --- |
| `web` | Serves HTTP, flushes "who's online" and view counts | horizontally, behind a load balancer |
| `worker` | Delivers mail, runs outbox jobs (notifications, plugin hooks, welcome PMs, file cleanup) | horizontally; jobs are leased, never run twice at once |
| `scheduler` | Runs scheduled tasks (cleanup, bans, promotions, mass mail…) | one is enough; more are safe (tasks are leased) |

`all` (the default) runs everything in one process, which is right for small boards. On busy
boards run them separately (`rbb worker`, `rbb scheduler`) so slow SMTP servers, image work,
plugins or maintenance tasks never take capacity from page views. See `docker-compose.yml` and
`deploy/rbb.service`.

Every process follows cache invalidations from the others through the `cluster_events` table
(woken by `NOTIFY rbb_cluster`, polled every 2 s), so a missed notification never leaves a node
with stale settings or forum permissions.

## Installing and upgrading

* `rbb install --admin-user NAME --admin-email ADDRESS --board-url URL` with
  `RBB_ADMIN_PASSWORD` set creates the board. `serve` refuses to start on an empty database; it
  never creates accounts or prints passwords by itself.
* Migrations run when a process starts unless `RBB_MIGRATE_ON_START=false`. With several nodes,
  turn that off and run `rbb migrate` once per deploy, before starting the new version.
* `rbb doctor --strict` checks configuration, database, migrations and plugins and explains how
  to fix what it finds; run it after changing the environment.

## Health checks

| Endpoint | Meaning | Use for |
| --- | --- | --- |
| `/livez` | the process is running | restart policy (liveness) |
| `/readyz` | the database answers within 2 s and the process is not shutting down | load balancer / readiness |
| `/metrics` | Prometheus metrics | scraping |

`/livez` and `/readyz` are on the web port and on the admin listener; `/metrics` only on the
admin listener (`RBB_ADMIN_LISTEN`, e.g. `0.0.0.0:9090`), which must not be reachable from the
internet. Worker and scheduler processes have no web port, so give them an admin listener for
their health checks. `rbb healthcheck [--url URL]` probes `/readyz` without curl (the Docker
image uses it).

On SIGTERM a process reports not ready, keeps serving for `RBB_SHUTDOWN_DRAIN_SECS` (set it to a
little more than your load balancer's health-check interval), then stops accepting connections,
ends live-update streams (browsers reconnect elsewhere), lets in-flight requests and background
batches finish, and flushes buffered counters.

## Metrics and alerts

Scrape every process's admin listener. Labels come from small fixed sets (route templates,
methods, status classes, job kinds), never from user input.

* Traffic: `rbb_http_requests_total`, `rbb_http_request_seconds` (per route template),
  `rbb_http_in_flight`.
* Database: `rbb_db_pool_connections`, `rbb_db_pool_idle`, `rbb_db_pool_acquire_seconds` (time
  to get a connection — rising values mean the pool is the bottleneck),
  `rbb_db_queries_per_request` and `rbb_db_query_seconds` from a sample of requests
  (`RBB_QUERY_SAMPLE_RATE`, default 0.01).
* Saturation: `rbb_runtime_lag_seconds` (how late the async runtime runs ready tasks).
* Background work: `rbb_outbox_pending`/`_dead`/`_oldest_seconds`,
  `rbb_mail_pending`/`_dead`/`_oldest_seconds`, `rbb_task_seconds`, `rbb_outbox_failures_total`,
  `rbb_mail_total{result}`.
* Caches and streams: `rbb_page_cache_entries`, `rbb_page_cache_requests_total{result}`,
  `rbb_live_streams`.
* Plugins: `rbb_plugin_hook_seconds`, `rbb_plugin_failures_total`,
  `rbb_plugin_breaker_open_total`.

`deploy/alerts.yml` has Prometheus rules for the conditions worth waking someone up for: the
node down, 5xx rate, p99 latency, pool saturation, runtime lag, outbox and mail backlogs, dead
jobs, and plugins being switched off.

## Logs

Set `RBB_LOG_JSON=true` for one JSON object per line. Each request has an id (`x-request-id`,
kept from the proxy if it sets one — the example Nginx config does — and returned in the
response); log lines inside a request carry it. Request logs contain the method and path only:
query strings can hold one-time codes and headers hold cookies, so neither is logged.

Statements slower than `RBB_SLOW_QUERY_MS` (default 250) are logged with a warning. To see
*why* they are slow, enable PostgreSQL's `auto_explain` on the database server:

```
shared_preload_libraries = 'auto_explain'
auto_explain.log_min_duration = '250ms'
auto_explain.log_analyze = on
auto_explain.log_buffers = on
auto_explain.log_format = json
```

## Reverse proxy

rbb believes `X-Forwarded-For` / `X-Real-IP` only when the connection comes from an address in
`RBB_TRUSTED_PROXIES` (comma-separated addresses or CIDRs; `RBB_TRUST_PROXY=true` alone trusts
loopback). It reads `X-Forwarded-For` from the right, skipping trusted proxies; the first other
address is the client. Point `RBB_TRUSTED_PROXIES` at exactly your proxies — never `0.0.0.0/0` —
and make the app port unreachable from anywhere else. `deploy/nginx.conf` shows a matching
configuration, including per-route body limits and edge rate limits.

## Uploads, request sizes and live streams

* Request bodies are limited to 2 MiB, except upload routes (attachments, avatars, theme
  banners), which accept up to `RBB_MAX_UPLOAD_MB` (default 25). Files stream to temporary
  files under `RBB_UPLOAD_DIR/tmp` and are cut off as soon as they exceed the limit for their
  kind; stale temporary files are swept hourly. Keep the reverse proxy's limits in line
  (`deploy/nginx.conf`).
* Images are decoded on blocking threads, at most half the cores at once, and refused above
  10,000 px per side, 40 megapixels or 512 MiB of decoder memory.
* Storage: `RBB_STORAGE=local` (default) keeps files in `RBB_UPLOAD_DIR` — with several web
  nodes that directory must be shared. `RBB_STORAGE=s3` stores them in an S3-compatible bucket
  (`RBB_S3_BUCKET`, optional `RBB_S3_ENDPOINT`, `RBB_S3_REGION`, `RBB_S3_PREFIX`, credentials
  from `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`), so nodes share nothing on disk. Stored files
  are immutable; deleting content removes its files after the deletion commits.
* Live-update (SSE) streams are capped per node: `RBB_SSE_MAX` (10,000), `RBB_SSE_MAX_PER_IP`
  (20) and `RBB_SSE_MAX_PER_USER` (8). Over the cap the client gets 429 and retries later.
  Each stream ends after an hour (the browser reconnects).

## Database settings

Keep PostgreSQL's durability defaults in production. In particular never set
`synchronous_commit = off` there: a crash would silently lose the last committed posts,
registrations and moderation actions (the setting in `scripts/dev-db.sh` is for local
development only). Do not reuse the example credentials from any file; generate passwords.

Size `RBB_DB_MAX_CONNECTIONS` per process so that the sum over all processes stays well below
PostgreSQL's `max_connections` (or put PgBouncer in transaction mode in between).

## Backups

What to back up:

1. **The database** — everything except uploaded files.
2. **Uploads** — `RBB_UPLOAD_DIR` (attachments, avatars, banners), unless they live in object
   storage, in which case enable versioning or replication on the bucket.
3. **Configuration** — the environment (`.env`), especially `RBB_SECRET` (without it, existing
   sessions, CSRF tokens and forum-password cookies stop working).

Recommended setup:

* Continuous archiving with point-in-time recovery (pgBackRest, WAL-G or Barman), with a full
  backup at least weekly. This loses at most a few seconds of data.
* Additionally, a nightly logical dump that is easy to restore anywhere:
  `pg_dump --format=custom --file=rbb-$(date +%F).dump "$DATABASE_URL"`.
* Copy backups off the machine (another region or provider) and keep them for 30 days or more.

### Restore drill (do it regularly — a backup you never restored is a hope, not a backup)

```sh
# 1. Restore into a scratch database.
createdb rbb_restore_test
pg_restore --no-owner --dbname=rbb_restore_test rbb-2026-10-01.dump

# 2. Check it: migrations match this rbb version, and denormalized counters agree with the data.
DATABASE_URL=postgres://.../rbb_restore_test rbb doctor --strict
DATABASE_URL=postgres://.../rbb_restore_test rbb check

# 3. Start it on a spare port and look at a few pages.
DATABASE_URL=postgres://.../rbb_restore_test RBB_LISTEN=127.0.0.1:8099 RBB_ROLE=web rbb serve

# 4. Throw it away.
dropdb rbb_restore_test
```

Record how long the restore took: that is your recovery time. Automate the drill (for example
monthly in CI against the latest dump) and alert when it fails.

## Imports

`rbb import-mybb` loads a MyBB board into staging tables first, checks counts and references,
and only then replaces the live data in one transaction; a failed import leaves the existing
board untouched. Take a backup first anyway.
