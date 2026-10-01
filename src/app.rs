//! Shared application state and background plumbing (cache invalidation, activity batching).

use crate::cache::Cache;
use crate::config::Config;
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
    fn subscribe(map: &DashMap<i32, broadcast::Sender<LiveEvent>>, key: i32) -> broadcast::Receiver<LiveEvent> {
        map.entry(key).or_insert_with(|| broadcast::channel(64).0).subscribe()
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
        self.threads.iter().map(|s| s.receiver_count()).sum::<usize>()
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
    /// Similar-threads results per thread (1 hour).
    pub similar_cache: moka::sync::Cache<i32, serde_json::Value>,
    /// Bounds concurrent full-text searches so they can't exhaust the DB pool.
    pub search_sem: tokio::sync::Semaphore,
    pub plugins: crate::plugins::Plugins,
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
}

impl AppState {
    pub async fn new(cfg: Config, db: PgPool) -> anyhow::Result<App> {
        let cache = Arc::new(ArcSwap::from_pointee(Cache::load_all(&db).await?));
        let tpl = Templates::new(cache.clone(), cfg.dev_templates.clone());
        let plugins = crate::plugins::Plugins::load(&cfg.plugins_dir);
        let page_cache_mb = if cfg.dev_templates.is_some() { 0 } else { cfg.page_cache_mb };
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
            plugins,
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
        }))
    }

    pub fn cache(&self) -> Arc<Cache> {
        self.cache.load_full()
    }

    /// Content changed: drop cached guest pages here now, and on other nodes within 50 ms
    /// (notifications are coalesced so a burst of writes costs one NOTIFY, not one per write).
    pub fn content_changed(&self) {
        self.page_cache.clear();
        self.page_cache_dirty.store(true, std::sync::atomic::Ordering::Release);
    }

    /// Only pages tagged with `tags` changed (here now, on other nodes within 50 ms).
    pub fn content_changed_tags(&self, tags: Vec<String>) {
        if tags.is_empty() {
            return;
        }
        self.page_cache.invalidate_tags(&tags);
        self.page_cache_dirty_tags.lock().unwrap().extend(tags);
    }

    /// Reload cache parts locally and tell other nodes to do the same.
    pub async fn invalidate(&self, parts: &[&str]) -> anyhow::Result<()> {
        self.reload_parts(parts).await?;
        for p in parts {
            sqlx::query("SELECT pg_notify('rbb_cache', $1)")
                .bind(format!("{}:{}", self.node_id, p))
                .execute(&self.db)
                .await?;
        }
        Ok(())
    }

    async fn reload_parts(&self, parts: &[&str]) -> anyhow::Result<()> {
        // Another node's scoped page-cache invalidation.
        if let [p] = parts {
            if let Some(tags) = p.strip_prefix("pagetags|") {
                self.page_cache.invalidate_tags(&tags.split(',').collect::<Vec<_>>());
                return Ok(());
            }
        }
        // Settings, forums, themes… all change what guests see.
        self.page_cache.clear();
        let parts: Vec<&str> = parts.iter().copied().filter(|p| *p != "pagecache").collect();
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

    pub fn publish(&self, ev: LiveEvent) {
        self.live.send(ev);
        // Other nodes learn about it via NOTIFY; keep payload small.
    }

    /// Publish a live event cluster-wide.
    pub async fn publish_all(&self, ev: LiveEvent) {
        let payload = serde_json::json!({"node": self.node_id, "kind": ev.kind, "tid": ev.tid, "uid": ev.uid, "data": ev.data});
        let s = payload.to_string();
        if s.len() < 7000 {
            let _ = sqlx::query("SELECT pg_notify('rbb_live', $1)")
                .bind(s)
                .execute(&self.db)
                .await;
        }
        self.publish(ev);
    }

    /// Per-node sliding-window-ish limiter. Returns false when the limit is exceeded.
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
        e.0 <= limit
    }
}

/// Listen for cache invalidations and live events from other nodes.
pub fn spawn_listener(app: App) {
    tokio::spawn(async move {
        loop {
            match run_listener(&app).await {
                Ok(()) => {}
                Err(e) => tracing::warn!("LISTEN connection lost: {e:#}; reconnecting"),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

async fn run_listener(app: &App) -> anyhow::Result<()> {
    let mut l = sqlx::postgres::PgListener::connect_with(&app.db).await?;
    l.listen_all(["rbb_cache", "rbb_live"]).await?;
    // After (re)connecting we may have missed notifications: reload everything.
    let fresh = Cache::load_all(&app.db).await?;
    app.cache.store(Arc::new(fresh));
    app.tpl.reset();
    loop {
        let n = l.recv().await?;
        match n.channel() {
            "rbb_cache" => {
                if let Some((node, part)) = n.payload().split_once(':') {
                    if node != app.node_id {
                        if let Err(e) = app.reload_parts(&[part]).await {
                            tracing::warn!("cache reload {part} failed: {e:#}");
                        }
                    }
                }
            }
            "rbb_live" => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(n.payload()) {
                    if v["node"].as_str() != Some(app.node_id.as_str()) {
                        let kind: &'static str = match v["kind"].as_str().unwrap_or("") {
                            "newpost" => "newpost",
                            "alert" => "alert",
                            "pm" => "pm",
                            "editpost" => "editpost",
                            "typing" => "typing",
                            _ => continue,
                        };
                        app.publish(LiveEvent {
                            kind,
                            tid: v["tid"].as_i64().unwrap_or(0) as i32,
                            uid: v["uid"].as_i64().unwrap_or(0) as i32,
                            data: v["data"].clone(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
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
            let everything = a.page_cache_dirty.swap(false, std::sync::atomic::Ordering::AcqRel)
                || tags.iter().map(|t| t.len() + 1).sum::<usize>() > 7000; // NOTIFY payload limit
            let payload = if everything {
                Some(format!("{}:pagecache", a.node_id))
            } else if !tags.is_empty() {
                Some(format!("{}:pagetags|{}", a.node_id, tags.join(",")))
            } else {
                None
            };
            if let Some(p) = payload {
                let _ = sqlx::query("SELECT pg_notify('rbb_cache', $1)").bind(p).execute(&a.db).await;
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

pub async fn flush_activity(app: &App) -> anyhow::Result<()> {
    if app.activity.is_empty() {
        return Ok(());
    }
    let keys: Vec<String> = app.activity.iter().map(|e| e.key().clone()).collect();
    let mut rows: Vec<(String, Activity)> = Vec::with_capacity(keys.len());
    for k in keys {
        if let Some((k, v)) = app.activity.remove(&k) {
            rows.push((k, v));
        }
    }
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
        if a.uid > 0 {
            let e = user_times.entry(a.uid).or_insert((0, String::new()));
            if a.time > e.0 {
                *e = (a.time, a.ip.clone());
            }
        }
        sids.push(sid);
        uids.push(a.uid);
        ips.push(a.ip);
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
         SELECT * FROM UNNEST($1::text[], $2::int[], $3::text[], $4::bigint[], $5::text[], $6::text[], $7::bool[], $8::int[], $9::int[], $10::text[])
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
        // lastvisit becomes the previous lastactive when the user returns after 15+ minutes.
        sqlx::query(
            "UPDATE users SET
                lastvisit = CASE WHEN users.lastactive < d.t - 900 THEN users.lastactive ELSE users.lastvisit END,
                timeonline = users.timeonline + CASE WHEN d.t - users.lastactive BETWEEN 0 AND 900 THEN d.t - users.lastactive ELSE 0 END,
                lastactive = d.t,
                lastip = d.ip
             FROM UNNEST($1::int[], $2::bigint[], $3::text[]) AS d(uid, t, ip)
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

pub async fn flush_views(app: &App) -> anyhow::Result<()> {
    if app.thread_views.is_empty() {
        return Ok(());
    }
    let keys: Vec<i32> = app.thread_views.iter().map(|e| *e.key()).collect();
    let mut tids = Vec::new();
    let mut counts = Vec::new();
    for k in keys {
        if let Some((k, v)) = app.thread_views.remove(&k) {
            tids.push(k);
            counts.push(v);
        }
    }
    sqlx::query(
        "UPDATE threads SET views = views + d.c FROM UNNEST($1::int[], $2::int[]) AS d(tid, c) WHERE threads.tid = d.tid",
    )
    .bind(&tids)
    .bind(&counts)
    .execute(&app.db)
    .await?;
    Ok(())
}
