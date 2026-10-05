# Performance

Measured on a single laptop (Apple Silicon, PostgreSQL 17 on the same machine)
with `tests/load.sh` against a seeded board: 100k users, 300k threads, 3M posts
(`rbb seed --users 100000 --threads 300000 --posts 3000000`).
64 concurrent clients, 8 s per endpoint, release build, `RBB_DB_MAX_CONNECTIONS=48`. Runs on a busy laptop vary by roughly ±30%.

| Endpoint | 0.5 (req/s) | 0.6 (req/s) | p99 in 0.6 |
|---|---:|---:|---:|
| Board index (guest) | 1,239 | 22,185 | 6 ms |
| Hot forum, page 1 | 1,509 | 19,993 | 7 ms |
| Hot forum, page 50 | 1,414 | 20,275 | 8 ms |
| Hot forum, last page (3,750 pages) | 1,903 | 18,976 | 9 ms |
| Normal thread | 1,991 | 20,587 | 8 ms |
| Mega thread, page 1 | 1,897 | 19,289 | 8 ms |
| Mega thread, last page | 2,057 | 19,712 | 10 ms |
| Profile | 2,835 | 38,803 | 7 ms |
| Member list | 521 | 18,255 | 13 ms |
| API thread | 4,132 | 10,200 | 15 ms |
| Archive thread | 5,857 | 37,832 | 5 ms |
| RSS feed | 9,528 | 16,331 | 11 ms |
| Board index (member) | 2,510 | 3,107 | 43 ms |
| Hot forum (member) | 1,215 | 1,589 | 80 ms |
| Normal thread (member) | 1,290 | 1,862 | 59 ms |
| Search (member, repeated query) | 1,564 | 1,509 | 112 ms |
| Post reply (writes) | 287–420 | 381–478 | ~500 ms |

Both columns were measured back to back on the same laptop and database (release builds). Guest
rows in 0.6 are served by the guest page cache. Writes vary by about ±30% between runs because of
lock contention on the hot thread, so the ranges come from alternating A/B runs.

Where the time goes on a logged-in page: the admin debug panel shows the board index at 1.6 ms of
handler time, of which template rendering is 0.5 ms, and a profile at 5.2 ms, of which rendering is
1.0 ms. That is why rbb keeps MiniJinja, with its admin-editable templates, instead of moving to
compile-time templates: the database round-trips dominate, and guests skip both through the cache.

The search row measures one member repeating one query, which the 30-second per-viewer result
cache answers. An uncached full-text search for two very common words in this corpus takes about
0.6 s. At most 8 run at once, each capped at 5 s, so the pool is never exhausted.

## What made the difference

- **Guest page cache** (0.6): anonymous requests for opted-in pages are answered from memory
  with the visitor's CSRF token substituted. It is cleared on any write, on every node, and
  entries live at most 30 s.
- **Immutable, pre-compressed assets** (0.6): content-hashed URLs with a one-year cache, and
  Brotli/gzip computed once at start-up instead of on every request.
- **Cheaper page context** (0.6): the public settings map, theme versions and language list are
  prebuilt when they change instead of on every request; templates are compiled at start-up; avatars
  for lists come from a short-lived in-memory cache.

- **Related threads**: a full-text query over first posts built from the thread's ten most
  distinctive words, with a 2 s statement timeout and a 1 h per-thread cache. A page waits at
  most 150 ms for an uncached lookup (the rest finishes in the background and fills the cache),
  and at most two lookups run at once, so crawlers walking cold threads can't load the database.
- **Cookie-less guests** (crawlers, load tools) share one session keyed by IP hash
  instead of creating a session row per request.
- **Online summary, board stats, birthdays and feeds** are cached for 30–60 s;
  birthdays also have a prefix index (`migrations/0002_perf.sql`).
- **Feeds** take the top N threads per forum through the `(fid, dateline)` index
  and merge them, instead of sorting every visible thread.
- **Deep pagination**: when a page falls past the middle of a forum or thread, the query
  scans the index from the other end with a small OFFSET and reverses the rows
  in memory. The output is verified to be identical to the plain OFFSET query.
- **Search**: at most 8 concurrent full-text searches (semaphore), each capped
  at 5 s (`SET LOCAL statement_timeout`). Thread-mode results walk matching
  posts newest-first (`ORDER BY p.pid DESC LIMIT`), so the scan stops early.
  Queries that are too broad get a clear message instead of tying up the pool.
- **"You posted here" dots** probe a `(uid, tid)` index once per listed thread
  (`LATERAL … LIMIT 1`) instead of collecting every post a prolific member wrote in those threads.
  This took hot forum pages for members from 240 to 1,668 req/s.
- **Batched writes**: session activity and thread view counts are buffered in
  memory and flushed with `UNNEST` upserts every 5–15 s.

## Seeing it yourself

Administrators get a debug panel at the bottom of every page. It shows total, handler and render
time, every SQL statement with its duration and row count, and repeated statements flagged as
likely N+1 patterns. Other visitors pay nothing for it: the query capture only switches on inside
an administrator's request.

## Scaling out

The web tier is stateless. Caches are invalidated across nodes through the
`cluster_events` log (woken by `NOTIFY rbb_cluster`), live events fan out over
`NOTIFY rbb_live`, and scheduled tasks and background jobs are leased, so any
number of web processes can sit behind a load balancer, with workers and the
scheduler scaled separately (`RBB_ROLE`). Performance budgets are enforced by
`tests/load_budget.sh` (see `tests/load_budgets.txt`).

## Reproducing

```sh
scripts/dev-db.sh start
createdb -h 127.0.0.1 -p 5433 -U rbb rbb_load
DATABASE_URL=postgres://rbb@127.0.0.1:5433/rbb_load ./target/release/rbb seed --users 100000 --threads 300000 --posts 3000000
DATABASE_URL=postgres://rbb@127.0.0.1:5433/rbb_load RBB_LISTEN=127.0.0.1:8090 ./target/release/rbb serve &
DATABASE_URL=postgres://rbb@127.0.0.1:5433/rbb_load tests/load.sh http://127.0.0.1:8090 64 8s
```
