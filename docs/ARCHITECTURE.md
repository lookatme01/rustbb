# rbb — architecture

rbb is a MyBB-compatible-in-spirit forum engine written in Rust.

* **HTTP**: axum + tokio, server-rendered HTML (progressive-enhancement JS), JSON API under `/api`.
* **DB**: PostgreSQL (sqlx). All timestamps are unix seconds (`BIGINT`) like MyBB's `dateline`.
  Counters (forum/thread/user post counts, last post info) are denormalized and maintained
  transactionally so every hot page is a handful of indexed lookups.
* **Templates**: minijinja. Default templates are embedded in the binary; every template can be
  overridden per theme from the Admin CP (stored in `templates` table) — MyBB's template sets.
* **Caching**: in-process caches (forums, usergroups, permissions, settings, smilies, …) are
  invalidated cluster-wide with Postgres `LISTEN/NOTIFY`, so any number of app nodes can run
  behind a load balancer. Sessions live in Postgres, so nodes are stateless.
* **Search**: Postgres full-text search (`tsvector` + GIN), no external service needed.
* **Background tasks**: in-process scheduler (MyBB "tasks") guarded by advisory locks so only one
  node runs each task.
* **System account**: `src/system.rs` ensures a built-in bot user (`users.is_system`) in a
  protected group (`usergroups.is_system`). Triggers in `migrations/0005_system_user.sql` block
  deleting, banning, signing in to, or changing the credentials and groups of that account.
  Errors carry SQLSTATE `RBSYS` and surface as user-facing messages.
* **Plugins**: Rhai scripts in `plugins/` hooking named events (MyBB hook model).
