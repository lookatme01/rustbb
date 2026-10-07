//! Shared application state and background plumbing (cache invalidation, activity batching).

use crate::cache::Cache;
use crate::config::Config;
use crate::infra::cluster::Broadcast;
use crate::templates::Templates;
use crate::util::now;
use arc_swap::ArcSwap;
use dashmap::DashMap;
use serde::Serialize;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

pub type App = Arc<AppState>;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[derive(Clone, Debug)]
pub struct Activity {
    pub uid: i32,
    pub ip: String,
    pub time: i64,
    pub location: String,
    pub useragent: String,
    pub anonymous: bool,
    pub location1: i32,
    pub location2: i32,
    pub bot: String,
}

/// Events pushed to connected browsers over Server-Sent Events.
#[derive(Clone, Debug, Serialize)]
pub struct LiveEvent {
    pub kind: &'static str,
    /// Thread id for thread-scoped events, 0 for global.
    pub tid: i32,
    /// Target user for user-scoped events (alerts / PMs), 0 for thread-scoped.
    pub uid: i32,
    pub data: serde_json::Value,
}

/// Live-event fan-out by topic: one channel per thread being read and one per member, so an
/// event wakes only the streams it concerns instead of every open stream on the node.
#[derive(Default)]
pub struct LiveHub {
    threads: DashMap<i32, broadcast::Sender<LiveEvent>>,
    users: DashMap<i32, broadcast::Sender<LiveEvent>>,
}

impl LiveHub {
    fn subscribe(
        map: &DashMap<i32, broadcast::Sender<LiveEvent>>,
        key: i32,
    ) -> broadcast::Receiver<LiveEvent> {
        map.entry(key)
            .or_insert_with(|| broadcast::channel(64).0)
            .subscribe()
    }
    pub fn thread(&self, tid: i32) -> broadcast::Receiver<LiveEvent> {
        Self::subscribe(&self.threads, tid)
    }
    pub fn user(&self, uid: i32) -> broadcast::Receiver<LiveEvent> {
        Self::subscribe(&self.users, uid)
    }
    pub fn send(&self, ev: LiveEvent) {
        match ev.kind {
            "newpost" | "editpost" | "typing" if ev.tid > 0 => {
                if let Some(s) = self.threads.get(&ev.tid) {
                    let _ = s.send(ev);
                }
            }
            "alert" | "pm" if ev.uid > 0 => {
                if let Some(s) = self.users.get(&ev.uid) {
                    let _ = s.send(ev);
                }
            }
            _ => {}
        }
    }
    /// Drop channels nobody listens to any more.
    pub fn sweep(&self) {
        self.threads.retain(|_, s| s.receiver_count() > 0);
        self.users.retain(|_, s| s.receiver_count() > 0);
    }
    pub fn subscribers(&self) -> usize {
        self.threads
            .iter()
            .map(|s| s.receiver_count())
            .sum::<usize>()
            + self.users.iter().map(|s| s.receiver_count()).sum::<usize>()
    }
}

pub struct AppState {
    pub cfg: Config,
    pub db: PgPool,
    pub cache: Arc<ArcSwap<Cache>>,
    pub tpl: Templates,
    pub node_id: String,
    pub activity: DashMap<String, Activity>,
    pub thread_views: DashMap<i32, i32>,
    pub live: LiveHub,
    pub ratelimit: DashMap<String, (u32, i64)>,
    pub mod_counts: moka::sync::Cache<i32, (i64, i64, i64)>,
    pub stats_cache: moka::sync::Cache<&'static str, serde_json::Value>,
    /// Short-lived (30s) cache for expensive shared fragments (online list, feeds).
    pub short_cache: moka::sync::Cache<String, serde_json::Value>,
    /// Related-threads results per thread (1 hour).
    pub similar_cache: moka::sync::Cache<i32, serde_json::Value>,
    /// Bounds concurrent full-text searches so they can't exhaust the DB pool.
    pub search_sem: tokio::sync::Semaphore,
    /// Related-thread lookups running at once (see `routes::showthread`).
    pub related_sem: tokio::sync::Semaphore,
    pub plugins: crate::plugins::Plugins,
    /// Where uploaded files live (local directory or object storage).
    pub storage: crate::infra::storage::Storage,
    /// Open live-update streams, capped (RBB_SSE_MAX, RBB_SSE_MAX_PER_IP, RBB_SSE_MAX_PER_USER).
    pub streams: Arc<crate::infra::streams::StreamLimits>,
    pub started: i64,
    /// Finished HTML of guest pages (see `crate::pagecache`).
    pub page_cache: crate::pagecache::PageCache,
    /// Set when other nodes still need to hear that content changed.
    pub page_cache_dirty: std::sync::atomic::AtomicBool,
    /// Tags other nodes still need to hear about.
    pub page_cache_dirty_tags: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Avatar URL per member for lists (thread rows, last posts, who's online). Avatars change
    /// rarely; entries are dropped when a member changes theirs and expire after two minutes.
    pub avatar_cache: moka::sync::Cache<i32, Arc<str>>,
    /// Post HTML parsed recently but maybe not yet written back to `posts.message_html`.
    pub parsed_cache: moka::sync::Cache<i32, (i32, Arc<[u8]>, Arc<str>)>,
    /// Posts whose parsed HTML is being written back right now.
    pub parse_inflight: dashmap::DashSet<i32>,
    /// Bounds concurrent HTML write-backs.
    pub parse_writeback: tokio::sync::Semaphore,
    /// Flips to `true` when the server is shutting down (ends live streams so it can exit).
    pub shutdown: tokio::sync::watch::Sender<bool>,
    /// Which cluster events this node has applied.
    pub cluster_cursor: std::sync::Mutex<crate::infra::cluster::Cursor>,
    /// Wakes the outbox worker (jobs committed here, or NOTIFY from another node).
    pub outbox_wake: tokio::sync::Notify,
    /// Wakes the mail worker.
    pub mail_wake: tokio::sync::Notify,
    /// Activity rows whose flush failed, retried with the next flush.
    pending_activity: std::sync::Mutex<Option<Pending<HashMap<String, Activity>>>>,
    /// View counts whose flush failed, retried with the next flush.
    pending_views: std::sync::Mutex<Option<Pending<HashMap<i32, i32>>>>,
}

/// A batch that failed to flush, kept for a bounded number of retries.
struct Pending<T> {
    batch: T,
    /// Identifies the batch so a retry after an unseen commit is not applied twice.
    id: uuid::Uuid,
    attempts: u32,
}

/// Give up on a batch after this many failed flushes (minutes of database trouble).
const MAX_FLUSH_ATTEMPTS: u32 = 20;
/// Cap on buffered entries kept across failures, so an outage cannot exhaust memory.
const MAX_PENDING_ENTRIES: usize = 500_000;

impl AppState {
    pub async fn new(cfg: Config, db: PgPool) -> anyhow::Result<App> {
        let cache = Arc::new(ArcSwap::from_pointee(Cache::load_all(&db).await?));
        let tpl = Templates::new(cache.clone(), cfg.dev_templates.clone());
        let plugins = crate::plugins::Plugins::load_with(&cfg.plugins_dir, cfg.plugins_trusted);
        let storage = crate::infra::storage::Storage::from_config(&cfg)?;
        let page_cache_mb = if cfg.dev_templates.is_some() {
            0
        } else {
            cfg.page_cache_mb
        };
        Ok(Arc::new(AppState {
            cfg,
            db,
            cache,
            tpl,
            node_id: crate::util::random_token(12),
            activity: DashMap::new(),
            thread_views: DashMap::new(),
            live: LiveHub::default(),
            ratelimit: DashMap::new(),
            mod_counts: moka::sync::Cache::builder()
                .time_to_live(Duration::from_secs(30))
                .max_capacity(10_000)
                .build(),
            stats_cache: moka::sync::Cache::builder()
                .time_to_live(Duration::from_secs(60))
                .build(),
            short_cache: moka::sync::Cache::builder()
                .time_to_live(Duration::from_secs(30))
                .max_capacity(2_000)
                .build(),
            similar_cache: moka::sync::Cache::builder()
                .time_to_live(Duration::from_secs(3600))
                .max_capacity(200_000)
                .build(),
            search_sem: tokio::sync::Semaphore::new(8),
            related_sem: tokio::sync::Semaphore::new(2),
            plugins,
            storage,
            streams: Arc::new(crate::infra::streams::StreamLimits::new(
                env_usize("RBB_SSE_MAX", 10_000),
                env_usize("RBB_SSE_MAX_PER_IP", 20),
                env_usize("RBB_SSE_MAX_PER_USER", 8),
            )),
            started: now(),
            page_cache: crate::pagecache::PageCache::new(page_cache_mb),
            page_cache_dirty: std::sync::atomic::AtomicBool::new(false),
            page_cache_dirty_tags: std::sync::Mutex::new(std::collections::HashSet::new()),
            avatar_cache: moka::sync::Cache::builder()
                .time_to_live(Duration::from_secs(120))
                .max_capacity(200_000)
                .build(),
            parsed_cache: moka::sync::Cache::builder()
                .time_to_live(Duration::from_secs(300))
                .max_capacity(20_000)
                .build(),
            parse_inflight: dashmap::DashSet::new(),
            parse_writeback: tokio::sync::Semaphore::new(4),
            shutdown: tokio::sync::watch::channel(false).0,
            cluster_cursor: std::sync::Mutex::new(Default::default()),
            outbox_wake: tokio::sync::Notify::new(),
            mail_wake: tokio::sync::Notify::new(),
            pending_activity: std::sync::Mutex::new(None),
            pending_views: std::sync::Mutex::new(None),
        }))
    }

    pub fn cache(&self) -> Arc<Cache> {
        self.cache.load_full()
    }

    /// Content changed: drop cached guest pages here now, and on other nodes within 50 ms
    /// (notifications are coalesced so a burst of writes costs one NOTIFY, not one per write).
    pub fn content_changed(&self) {
        self.page_cache.clear();
        self.page_cache_dirty
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Only pages tagged with `tags` changed (here now, on other nodes within 50 ms).
    pub fn content_changed_tags(&self, tags: Vec<String>) {
        if tags.is_empty() {
            return;
        }
        self.page_cache.invalidate_tags(&tags);
        self.page_cache_dirty_tags.lock().unwrap().extend(tags);
    }

    /// Reload cache parts here and on every other node (through the durable cluster log). If the
    /// reload here fails, this node's listener retries it like an event from another node.
    pub async fn invalidate(&self, parts: &[&str]) -> anyhow::Result<()> {
        let b = Broadcast::Cache(parts.iter().map(|p| p.to_string()).collect());
        let mut c = self.db.acquire().await?;
        let id = crate::infra::cluster::record(&mut c, &self.node_id, &b).await?;
        drop(c);
        let r = self.apply_broadcast(&b).await;
        let mut cursor = self.cluster_cursor.lock().unwrap();
        match r {
            Ok(()) => {
                cursor.mark(id);
            }
            Err(_) => cursor.defer(id),
        }
        r
    }

    /// Record an invalidation this node already applied, for the other nodes.
    async fn broadcast(&self, b: &Broadcast) -> anyhow::Result<()> {
        let mut c = self.db.acquire().await?;
        let id = crate::infra::cluster::record(&mut c, &self.node_id, b).await?;
        self.cluster_cursor.lock().unwrap().mark(id);
        Ok(())
    }

    /// Apply an invalidation to this node's caches.
    pub async fn apply_broadcast(&self, b: &Broadcast) -> anyhow::Result<()> {
        match b {
            Broadcast::PageTags(tags) => self.page_cache.invalidate_tags(tags),
            Broadcast::PageCache => self.page_cache.clear(),
            Broadcast::Cache(parts) => {
                let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
                self.reload_parts(&parts).await?;
            }
        }
        Ok(())
    }

    async fn reload_parts(&self, parts: &[&str]) -> anyhow::Result<()> {
        // Settings, forums, themes… all change what guests see.
        self.page_cache.clear();
        let parts: Vec<&str> = parts
            .iter()
            .copied()
            .filter(|p| *p != "pagecache")
            .collect();
        if parts.is_empty() {
            return Ok(());
        }
        let parts = &parts[..];
        let mut c = (*self.cache.load_full()).clone();
        for p in parts {
            c.reload(&self.db, p).await?;
        }
        self.cache.store(Arc::new(c));
        if parts.iter().any(|p| matches!(*p, "templates" | "themes")) {
            self.tpl.reset();
        }
        Ok(())
    }

    /// Bump the parser revision so cached post HTML is regenerated lazily.
    pub async fn bump_parser_rev(&self) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO settings (name, value) VALUES ('parser_rev', '1')
             ON CONFLICT (name) DO UPDATE SET value = (COALESCE(NULLIF(settings.value, ''), '0')::int + 1)::text",
        )
        .execute(&self.db)
        .await?;
        self.invalidate(&["settings", "parser"]).await
    }

    /// Deliver a live event to this node's streams.
    pub fn publish(&self, ev: LiveEvent) {
        self.live.send(ev);
    }

    /// The NOTIFY payload carrying `ev` to other nodes (`None` if it is too large to send).
    pub fn live_payload(&self, ev: &LiveEvent) -> Option<String> {
        let s = serde_json::json!({"node": self.node_id, "kind": ev.kind, "tid": ev.tid, "uid": ev.uid, "data": ev.data}).to_string();
        (s.len() < 7000).then_some(s)
    }

    /// Publish a live event cluster-wide. Live events are best-effort: a browser that misses
    /// one catches up on its next page load.
    pub async fn publish_all(&self, ev: LiveEvent) {
        if let Some(s) = self.live_payload(&ev) {
            let _ = sqlx::query("SELECT pg_notify('rbb_live', $1)")
                .bind(s)
                .execute(&self.db)
                .await;
        }
        self.publish(ev);
    }

    /// Cluster-wide limiter for security-sensitive actions (sign-in, password reset, 2FA,
    /// registration mail, API tokens…): one atomic bucket per key in the shared `ratelimits`
    /// table, so limits hold across nodes and restarts. The part of the key after the first `:`
    /// is stored hashed. If the database cannot be reached the per-node limiter decides.
    /// Returns false when the limit is exceeded.
    pub async fn throttle(&self, key: &str, limit: u32, window_secs: i64) -> bool {
        if limit == 0 {
            return true;
        }
        let (name, rest) = key.split_once(':').unwrap_or(("other", key));
        let stored = format!("{name}:{}", &crate::util::sha256_hex(rest)[..32]);
        let t = now();
        let r: Result<i32, _> = sqlx::query_scalar(
            "INSERT INTO ratelimits (key, hits, reset_at) VALUES ($1, 1, $2 + $3)
             ON CONFLICT (key) DO UPDATE SET
                hits = CASE WHEN ratelimits.reset_at <= $2 THEN 1 ELSE ratelimits.hits + 1 END,
                reset_at = CASE WHEN ratelimits.reset_at <= $2 THEN $2 + $3 ELSE ratelimits.reset_at END
             RETURNING hits",
        )
        .bind(&stored)
        .bind(t)
        .bind(window_secs)
        .fetch_one(&self.db)
        .await;
        match r {
            Ok(hits) => {
                let ok = hits as u32 <= limit;
                if !ok {
                    crate::infra::metrics::counter_with(
                        "rbb_ratelimited_total",
                        &[("limit", name)],
                        1,
                    );
                }
                ok
            }
            Err(e) => {
                tracing::warn!("shared rate limit unavailable ({e}); using the local one");
                self.rate_check(key, limit, window_secs)
            }
        }
    }

    /// Per-node limiter for load shedding (cheap, approximate). Returns false when the limit
    /// is exceeded. Security-sensitive limits use [`AppState::throttle`].
    pub fn rate_check(&self, key: &str, limit: u32, window_secs: i64) -> bool {
        if limit == 0 {
            return true;
        }
        let t = now();
        let mut e = self
            .ratelimit
            .entry(key.to_string())
            .or_insert((0, t + window_secs));
        if e.1 <= t {
            *e = (0, t + window_secs);
        }
        e.0 += 1;
        let ok = e.0 <= limit;
        if !ok {
            // The key's prefix names the limit (`login:…`, `req:…`); the rest is never a label.
            let name = key.split(':').next().unwrap_or("other");
            crate::infra::metrics::counter_with("rbb_ratelimited_total", &[("limit", name)], 1);
        }
        ok
    }
}

/// Follow cache invalidations, live events and outbox wake-ups from other nodes.
pub fn spawn_listener(app: App) {
    tokio::spawn(async move {
        loop {
            match run_listener(&app).await {
                Ok(()) => {}
                Err(e) => tracing::warn!("cluster listener stopped: {e:#}; reconnecting"),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

async fn run_listener(app: &App) -> anyhow::Result<()> {
    let mut l = sqlx::postgres::PgListener::connect_with(&app.db).await?;
    l.listen_all([
        crate::infra::cluster::CHANNEL,
        "rbb_live",
        crate::infra::outbox::CHANNEL,
    ])
    .await?;
    // Events may have been missed while disconnected: start from a full reload.
    let mut conn = app.db.acquire().await?;
    *app.cluster_cursor.lock().unwrap() = crate::infra::cluster::Cursor::start(&mut conn).await?;
    drop(conn);
    let fresh = Cache::load_all(&app.db).await?;
    app.cache.store(Arc::new(fresh));
    app.tpl.reset();
    app.page_cache.clear();
    let mut poll = tokio::time::interval(crate::infra::cluster::POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let catch_up = tokio::select! {
            n = l.recv() => {
                let n = n?;
                match n.channel() {
                    crate::infra::cluster::CHANNEL => true,
                    crate::infra::outbox::CHANNEL => {
                        app.outbox_wake.notify_one();
                        app.mail_wake.notify_one();
                        false
                    }
                    "rbb_live" => {
                        relay_live(app, n.payload());
                        false
                    }
                    _ => false,
                }
            }
            _ = poll.tick() => true,
        };
        if catch_up {
            let mut conn = app.db.acquire().await?;
            crate::infra::cluster::catch_up(app, &mut conn).await?;
        }
    }
}

fn relay_live(app: &App, payload: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
        return;
    };
    if v["node"].as_str() == Some(app.node_id.as_str()) {
        return;
    }
    let kind: &'static str = match v["kind"].as_str().unwrap_or("") {
        "newpost" => "newpost",
        "alert" => "alert",
        "pm" => "pm",
        "editpost" => "editpost",
        "typing" => "typing",
        _ => return,
    };
    app.publish(LiveEvent {
        kind,
        tid: v["tid"].as_i64().unwrap_or(0) as i32,
        uid: v["uid"].as_i64().unwrap_or(0) as i32,
        data: v["data"].clone(),
    });
}

/// Periodically flush batched session activity and thread view counts to the database.
/// Batching turns one write per page view into one bulk statement every few seconds.
pub fn spawn_flushers(app: App) {
    let a = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            a.live.sweep();
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let tags: Vec<String> = a.page_cache_dirty_tags.lock().unwrap().drain().collect();
            let everything = a
                .page_cache_dirty
                .swap(false, std::sync::atomic::Ordering::AcqRel)
                || tags.len() > 500;
            let event = if everything {
                Some(Broadcast::PageCache)
            } else if !tags.is_empty() {
                Some(Broadcast::PageTags(tags))
            } else {
                None
            };
            if let Some(b) = event
                && let Err(e) = a.broadcast(&b).await
            {
                // Other nodes' entries still expire after pagecache::TTL.
                tracing::warn!("page cache invalidation not recorded: {e:#}");
            }
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            if let Err(e) = flush_activity(&a).await {
                tracing::warn!("activity flush failed: {e:#}");
            }
        }
    });
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tick.tick().await;
            if let Err(e) = flush_views(&app).await {
                tracing::warn!("view flush failed: {e:#}");
            }
            // housekeeping of the in-memory limiter
            let t = now();
            app.ratelimit.retain(|_, v| v.1 > t);
        }
    });
}

/// Move everything buffered into a batch. Entries are removed one at a time (each removal is
/// atomic), so updates arriving meanwhile land in the buffer for the next flush, not nowhere.
fn drain<K: Eq + std::hash::Hash + Clone, V>(live: &DashMap<K, V>) -> HashMap<K, V> {
    let keys: Vec<K> = live.iter().map(|e| e.key().clone()).collect();
    keys.into_iter().filter_map(|k| live.remove(&k)).collect()
}

fn new_batch<T>(batch: T) -> Pending<T> {
    Pending {
        batch,
        id: uuid::Uuid::new_v4(),
        attempts: 0,
    }
}

/// Keep a failed batch for the next flush, unless it has failed too often or grown too big.
fn keep_failed<K, V>(
    pending: &std::sync::Mutex<Option<Pending<HashMap<K, V>>>>,
    mut p: Pending<HashMap<K, V>>,
    what: &'static str,
) {
    p.attempts += 1;
    if p.attempts >= MAX_FLUSH_ATTEMPTS || p.batch.len() > MAX_PENDING_ENTRIES {
        tracing::error!(
            entries = p.batch.len(),
            attempts = p.attempts,
            "giving up on flushing buffered {what}"
        );
        crate::infra::metrics::counter_with(
            "rbb_flush_dropped_total",
            &[("what", what)],
            p.batch.len() as u64,
        );
        return;
    }
    *pending.lock().unwrap() = Some(p);
}

/// Write buffered "who's online" activity. The writes are idempotent upserts, so retrying a
/// batch that did commit changes nothing.
pub async fn flush_activity(app: &App) -> anyhow::Result<()> {
    // A batch that failed before goes first, alone, so it is retried exactly as it was.
    let retry = app.pending_activity.lock().unwrap().take();
    if let Some(p) = retry
        && let Err(e) = write_activity(app, &p.batch).await
    {
        keep_failed(&app.pending_activity, p, "activity");
        return Err(e);
    }
    let batch = drain(&app.activity);
    if batch.is_empty() {
        return Ok(());
    }
    let p = new_batch(batch);
    if let Err(e) = write_activity(app, &p.batch).await {
        keep_failed(&app.pending_activity, p, "activity");
        return Err(e);
    }
    Ok(())
}

async fn write_activity(app: &App, rows: &HashMap<String, Activity>) -> anyhow::Result<()> {
    let mut sids = Vec::new();
    let mut uids = Vec::new();
    let mut ips = Vec::new();
    let mut times = Vec::new();
    let mut locs = Vec::new();
    let mut uas = Vec::new();
    let mut anons = Vec::new();
    let mut l1 = Vec::new();
    let mut l2 = Vec::new();
    let mut bots = Vec::new();
    let mut user_times: HashMap<i32, (i64, String)> = HashMap::new();
    for (sid, a) in rows {
        let (sid, a) = (sid.clone(), a.clone());
        if a.uid > 0 {
            let e = user_times.entry(a.uid).or_insert((0, String::new()));
            if a.time > e.0 {
                *e = (a.time, a.ip.clone());
            }
        }
        sids.push(sid);
        uids.push(a.uid);
        ips.push(crate::util::IpText(a.ip));
        times.push(a.time);
        locs.push(a.location);
        uas.push(a.useragent);
        anons.push(a.anonymous);
        l1.push(a.location1);
        l2.push(a.location2);
        bots.push(a.bot);
    }
    sqlx::query(
        "INSERT INTO sessions (sid, uid, ip, time, location, useragent, anonymous, location1, location2, bot)
         SELECT * FROM UNNEST($1::text[], $2::int[], $3::inet[], $4::bigint[], $5::text[], $6::text[], $7::bool[], $8::int[], $9::int[], $10::text[])
         ON CONFLICT (sid) DO UPDATE SET uid = EXCLUDED.uid, ip = EXCLUDED.ip, time = EXCLUDED.time, location = EXCLUDED.location,
            useragent = EXCLUDED.useragent, anonymous = EXCLUDED.anonymous, location1 = EXCLUDED.location1,
            location2 = EXCLUDED.location2, bot = EXCLUDED.bot",
    )
    .bind(&sids)
    .bind(&uids)
    .bind(&ips)
    .bind(&times)
    .bind(&locs)
    .bind(&uas)
    .bind(&anons)
    .bind(&l1)
    .bind(&l2)
    .bind(&bots)
    .execute(&app.db)
    .await?;
    if !user_times.is_empty() {
        let (u, rest): (Vec<i32>, Vec<(i64, String)>) = user_times.into_iter().unzip();
        let (t, ip): (Vec<i64>, Vec<String>) = rest.into_iter().unzip();
        let ip: Vec<crate::util::IpText> = ip.into_iter().map(crate::util::IpText).collect();
        // lastvisit becomes the previous lastactive when the user returns after 15+ minutes.
        sqlx::query(
            "UPDATE users SET
                lastvisit = CASE WHEN users.lastactive < d.t - 900 THEN users.lastactive ELSE users.lastvisit END,
                timeonline = users.timeonline + CASE WHEN d.t - users.lastactive BETWEEN 0 AND 900 THEN d.t - users.lastactive ELSE 0 END,
                lastactive = d.t,
                lastip = d.ip
             FROM UNNEST($1::int[], $2::bigint[], $3::inet[]) AS d(uid, t, ip)
             WHERE users.uid = d.uid AND users.lastactive < d.t",
        )
        .bind(&u)
        .bind(&t)
        .bind(&ip)
        .execute(&app.db)
        .await?;
    }
    Ok(())
}

/// Add buffered thread view counts. Each batch is recorded in `applied_batches` in the same
/// transaction, so a retry of a batch that committed (the reply was lost) is not counted twice.
pub async fn flush_views(app: &App) -> anyhow::Result<()> {
    let retry = app.pending_views.lock().unwrap().take();
    if let Some(p) = retry
        && let Err(e) = write_views(app, p.id, &p.batch).await
    {
        keep_failed(&app.pending_views, p, "views");
        return Err(e);
    }
    let batch = drain(&app.thread_views);
    if batch.is_empty() {
        return Ok(());
    }
    let p = new_batch(batch);
    if let Err(e) = write_views(app, p.id, &p.batch).await {
        keep_failed(&app.pending_views, p, "views");
        return Err(e);
    }
    Ok(())
}

async fn write_views(app: &App, id: uuid::Uuid, views: &HashMap<i32, i32>) -> anyhow::Result<()> {
    let (tids, counts): (Vec<i32>, Vec<i32>) = views.iter().map(|(k, v)| (*k, *v)).unzip();
    let mut tx = app.db.begin().await?;
    let fresh = sqlx::query("INSERT INTO applied_batches (id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    if fresh {
        sqlx::query(
            "UPDATE threads SET views = views + d.c FROM UNNEST($1::int[], $2::int[]) AS d(tid, c) WHERE threads.tid = d.tid",
        )
        .bind(&tids)
        .bind(&counts)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}
