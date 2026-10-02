# rbb — architecture

rbb is a modular monolith: one Rust crate (a library plus a thin `rbb` binary), one PostgreSQL
database, any number of identical processes. This page describes how the code is layered, the
consistency rules every change follows, and how work is split between processes. Operating it is
covered in [OPERATIONS.md](OPERATIONS.md).

## Layers

```
 routes/, admin/      HTTP adapters: extract input, check the request (CSRF, login), call a
                      use case or operation, render HTML or JSON. No transaction boundaries.
        │
 usecase/, posting,   Application use cases. Each owns one unit of work (`usecase::Uow`): a
 ops                  transaction plus the side effects that may only happen after it commits.
        │
 domain/              Rules with no I/O: who may see what (`access::ForumAccess`), staff
                      capabilities (`staff::Staff`), validation of untrusted files
                      (`theme_export`).
        │
 infra/               Adapters to the outside: transactional outbox, cluster invalidation log,
                      metrics, health, uploads and object storage, stream limits.
        │
 PostgreSQL           Source of truth, including queues (outbox, mail) and leases (tasks).
```

Older modules (`cache`, `render`, `notify`, `mail`, `tasks`, `system`, `automod`) sit between
these layers and are being moved into them as they are touched. The direction of dependencies is
what matters: domain code never touches HTTP or the database, and HTTP handlers never open
transactions.

## Consistency rules

1. **One change, one transaction.** Everything a change writes — rows, denormalized counters,
   audit records, moderator log entries, queued mail, outbox jobs, cache invalidation events —
   goes through one `Uow` and commits or rolls back together.
2. **Side effects after commit.** Email, plugin hooks, notifications, file deletion, live
   updates and cache invalidation never run before the transaction commits. Durable ones are
   outbox jobs (`infra::outbox`): leased with `FOR UPDATE SKIP LOCKED`, retried with exponential
   backoff, dead after repeated failure. Jobs may run more than once, so each is idempotent.
3. **Locks in a fixed order.** Writes to existing content lock the thread first, then its posts,
   each in id order (`ops::lock_threads`, `ops::lock_posts`). Replies, automatic double-post
   merges, edits, moderation and merges of the same thread therefore serialize, and concurrent
   operations cannot deadlock.
4. **One-time codes are consumed with `DELETE … RETURNING`** in the transaction that acts on
   them, so a code works exactly once under any concurrency. Only hashes are stored.
5. **No network I/O while holding locks.** Slow work (SMTP, image decoding, password hashing,
   plugins) happens before the transaction starts or after it commits.

## Authorization

* `domain::access::ForumAccess` is the only evaluator of what a viewer may see. It considers the
  forum and every ancestor (existence, active, `canview`, passwords) and decides whether threads
  are readable in full, only the viewer's own, or not at all. Every reader of forum content uses
  it: forum and thread pages, the API, search, feeds, the sitemap (always as a guest), the portal,
  statistics, the archive, similar threads, notifications and the counters aggregated on the
  index.
* `domain::staff::Staff` turns group permissions and moderator assignments into explicit
  capabilities (notes, warnings, bans, post/member/PM reports, moderator log, queue, cross-forum
  history, IPs) with a forum scope; forum moderators act only within their forums.
* Browser sessions (cookie + CSRF token) and API tokens (`api_tokens`, scoped, accepted only under
  `/api/v1`) are separate credentials.

## Caches and their contracts

| Cache | Contents | Invalidation | Staleness bound |
| --- | --- | --- | --- |
| Board configuration (`cache.rs`) | settings, forums, groups, permissions, moderators, themes, templates, parser data | `cluster_events` log, applied locally at once and by every node via NOTIFY + 2 s polling with gap tracking | ~2 s on other nodes, none locally |
| Forum access memo | `ForumAccess` per permission set | dropped with every configuration reload | same as configuration |
| Guest page cache (`pagecache.rs`) | finished HTML of guest pages, tagged by board/forum/thread | tags or everything, through `cluster_events`; entries also expire after 30 s | 30 s worst case |
| Parsed post HTML | rendered posts, keyed by parser revision | parser revision bump; edits set `parser_rev = -1` | none (revision check on read) |
| Short caches (stats, online list, similar threads, avatars) | derived display data | time-based | 30 s – 1 h, never used for authorization |

Authorization never depends on a cache that could miss an invalidation: permission data comes
from the configuration cache, whose invalidations are durable (a node that misses a NOTIFY reads
the event on its next poll, and reloads everything after reconnecting). Cache hit rates are
measured (`rbb_page_cache_requests_total`) before any new cache layer is considered.

## Processes

One binary runs any combination of roles (`RBB_ROLE`): **web** (HTTP, activity buffering),
**worker** (mail, outbox jobs) and **scheduler** (tasks, leased per task). Every process follows
the cluster log. Large boards run them separately so slow SMTP servers, image work, plugins or
maintenance never take request-serving capacity.

## Data

* Timestamps are Unix seconds (`BIGINT`) throughout the MyBB-derived schema, always UTC; newer
  tables (outbox, cluster events, API tokens, mail leases) use `TIMESTAMPTZ`.
* Counters (forum/thread/member post counts, last posts) are denormalized and kept exact by
  `ops` (contribution deltas plus per-thread recounts); `rbb check` verifies them and a property
  test drives random moderation sequences against them.
* Search is PostgreSQL full-text search (`tsvector` + GIN).
* Uploaded files go through `infra::storage` (local directory or S3-compatible), are immutable,
  and are deleted only after the deleting transaction commits.

## Other components

* **Templates:** minijinja, embedded defaults overridable per theme.
* **System account:** `src/system.rs` and triggers in `migrations/0005_system_user.sql` protect a
  built-in bot account; errors carry SQLSTATE `RBSYS`.
* **Plugins:** Rhai scripts hooking named events, run on a bounded blocking executor with time
  budgets, circuit breakers and output sanitization (see `plugins.rs`).
