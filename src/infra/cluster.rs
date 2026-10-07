//! Cluster-wide cache invalidation through the durable `cluster_events` log.
//!
//! Consistency contract:
//! * A node that changes shared state applies the invalidation to its own caches immediately
//!   after the change commits, and records it in `cluster_events` (inside the change's
//!   transaction when there is one).
//! * Other nodes are woken by `NOTIFY rbb_cluster` and read every event they have not applied
//!   yet. They also poll every [`POLL`], so a lost notification delays an invalidation by at most
//!   that long instead of leaving a cache stale until it expires.
//! * Event ids are allocated before commit, so they can become visible out of order. Each node
//!   remembers the ids it skipped over ("gaps") for [`GAP_TTL`] and keeps asking for them; a gap
//!   still empty after that was a rolled-back transaction.
//! * After (re)connecting, a node reloads all caches before reading events, so nothing that
//!   happened while it was disconnected is lost.
//! * An event is marked applied only once applying it succeeds. One that fails (a cache reload
//!   whose query failed) is kept and retried on every catch-up, whichever node recorded it. If it
//!   still fails after [`RETRY_TTL`], the listener reconnects, which reloads every cache.
//! * Applying an event is idempotent, so seeing one twice is harmless.

use crate::app::App;
use serde_json::{Value, json};
use sqlx::PgConnection;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub const CHANNEL: &str = "rbb_cluster";
pub const POLL: Duration = Duration::from_secs(2);
pub const GAP_TTL: Duration = Duration::from_secs(120);
/// How long events are kept for slow or reconnecting nodes.
pub const RETENTION_SECS: i64 = 3600;
/// How long an event that fails to apply is retried before the node reloads everything instead.
/// Well inside [`RETENTION_SECS`], so the event is still there to be retried.
pub const RETRY_TTL: Duration = Duration::from_secs(300);

/// An invalidation every node applies.
#[derive(Clone, Debug, PartialEq)]
pub enum Broadcast {
    /// Reload these parts of the shared cache (`settings`, `forums`, `themes`…).
    Cache(Vec<String>),
    /// Guest pages tagged with any of these changed.
    PageTags(Vec<String>),
    /// Every guest page changed.
    PageCache,
}

impl Broadcast {
    fn kind(&self) -> &'static str {
        match self {
            Broadcast::Cache(_) => "cache",
            Broadcast::PageTags(_) => "pagetags",
            Broadcast::PageCache => "pagecache",
        }
    }

    fn payload(&self) -> Value {
        match self {
            Broadcast::Cache(p) => json!({ "parts": p }),
            Broadcast::PageTags(t) => json!({ "tags": t }),
            Broadcast::PageCache => json!({}),
        }
    }

    fn parse(kind: &str, payload: &Value) -> Option<Broadcast> {
        let strings = |k: &str| -> Vec<String> {
            payload[k]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        match kind {
            "cache" => Some(Broadcast::Cache(strings("parts"))),
            "pagetags" => Some(Broadcast::PageTags(strings("tags"))),
            "pagecache" => Some(Broadcast::PageCache),
            _ => None,
        }
    }
}

/// Record an event (and wake the other nodes when it commits). Returns its id.
pub async fn record(conn: &mut PgConnection, origin: &str, b: &Broadcast) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "WITH e AS (INSERT INTO cluster_events (origin, kind, payload) VALUES ($1, $2, $3) RETURNING id)
         SELECT id FROM e, LATERAL (SELECT pg_notify('rbb_cluster', '')) n",
    )
    .bind(origin)
    .bind(b.kind())
    .bind(b.payload())
    .fetch_one(conn)
    .await
}

/// Which events this node has applied.
#[derive(Default)]
pub struct Cursor {
    max: i64,
    gaps: BTreeMap<i64, Instant>,
    /// Events seen but not applied because applying them failed, with when they first failed.
    retry: BTreeMap<i64, Instant>,
}

impl Cursor {
    /// Start after the newest event, treating recent missing ids as gaps (they may belong to
    /// transactions that have not committed yet).
    pub async fn start(conn: &mut PgConnection) -> sqlx::Result<Cursor> {
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM cluster_events WHERE id > (SELECT COALESCE(MAX(id), 0) - 1000 FROM cluster_events) ORDER BY id",
        )
        .fetch_all(conn)
        .await?;
        let mut c = Cursor::default();
        if let (Some(first), Some(last)) = (ids.first(), ids.last()) {
            c.max = *last;
            let present: std::collections::HashSet<i64> = ids.iter().copied().collect();
            let now = Instant::now();
            for id in *first..*last {
                if !present.contains(&id) {
                    c.gaps.insert(id, now);
                }
            }
        }
        Ok(c)
    }

    /// Note that `id` has been applied (or need not be). Returns false if it already was.
    pub fn mark(&mut self, id: i64) -> bool {
        if id > self.max {
            let now = Instant::now();
            for g in (self.max + 1)..id {
                self.gaps.insert(g, now);
            }
            self.max = id;
            true
        } else {
            let gap = self.gaps.remove(&id).is_some();
            let retry = self.retry.remove(&id).is_some();
            gap || retry
        }
    }

    /// Note that applying `id` failed: it is retried until [`Cursor::mark`]ed.
    pub fn defer(&mut self, id: i64) {
        self.mark(id);
        self.retry.entry(id).or_insert_with(Instant::now);
    }

    /// Whether `id` still has to be applied.
    fn pending(&self, id: i64) -> bool {
        id > self.max || self.gaps.contains_key(&id) || self.retry.contains_key(&id)
    }

    fn expire_gaps(&mut self) {
        self.gaps.retain(|_, t| t.elapsed() < GAP_TTL);
    }

    /// The oldest retry has been failing for longer than `ttl`.
    fn retries_overdue(&self, ttl: Duration) -> bool {
        self.retry.values().any(|t| t.elapsed() >= ttl)
    }
}

/// Read and apply every event this node has not seen yet.
pub async fn catch_up(app: &App, conn: &mut PgConnection) -> anyhow::Result<usize> {
    let (gaps, max) = {
        let mut c = app.cluster_cursor.lock().unwrap();
        c.expire_gaps();
        if c.retries_overdue(RETRY_TTL) {
            // Give up on single events: reconnecting reloads every cache.
            anyhow::bail!("cluster events have failed to apply for {RETRY_TTL:?}");
        }
        let ids: Vec<i64> = c.gaps.keys().chain(c.retry.keys()).copied().collect();
        (ids, c.max)
    };
    let rows: Vec<(i64, String, String, Value)> = sqlx::query_as(
        "SELECT id, origin, kind, payload FROM cluster_events WHERE id > $1 OR id = ANY($2) ORDER BY id LIMIT 5000",
    )
    .bind(max)
    .bind(&gaps)
    .fetch_all(conn)
    .await?;
    let mut applied = 0;
    for (id, origin, kind, payload) in rows {
        let (pending, retrying) = {
            let c = app.cluster_cursor.lock().unwrap();
            (c.pending(id), c.retry.contains_key(&id))
        };
        if !pending {
            continue;
        }
        // This node applies its own events when it records them, unless that failed.
        let Some(b) =
            Broadcast::parse(&kind, &payload).filter(|_| origin != app.node_id || retrying)
        else {
            app.cluster_cursor.lock().unwrap().mark(id);
            continue;
        };
        match app.apply_broadcast(&b).await {
            Ok(()) => {
                app.cluster_cursor.lock().unwrap().mark(id);
                applied += 1;
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), event = id, "applying cluster event failed; will retry");
                app.cluster_cursor.lock().unwrap().defer(id);
            }
        }
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_tracks_gaps() {
        let mut c = Cursor::default();
        assert!(c.mark(1));
        assert!(c.mark(4)); // 2 and 3 not visible yet
        assert_eq!(c.gaps.keys().copied().collect::<Vec<_>>(), vec![2, 3]);
        assert!(!c.mark(4), "already applied");
        assert!(c.mark(3), "a late commit fills its gap");
        assert!(!c.mark(3));
        assert_eq!(c.gaps.keys().copied().collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn failed_events_are_retried_until_applied() {
        let mut c = Cursor::default();
        assert!(c.mark(1));
        c.defer(3); // 2 not visible yet, 3 failed
        assert!(c.pending(2) && c.pending(3) && !c.pending(1));
        assert!(c.mark(4));
        c.expire_gaps();
        assert!(c.pending(3), "a failed event is not dropped like a gap");
        c.defer(3);
        assert!(c.pending(3), "failing again keeps it");
        assert!(!c.retries_overdue(RETRY_TTL));
        assert!(c.retries_overdue(Duration::ZERO));
        assert!(c.mark(3), "applied at last");
        assert!(!c.pending(3) && !c.mark(3));
        assert!(c.retry.is_empty());
    }

    #[test]
    fn broadcast_round_trip() {
        for b in [
            Broadcast::Cache(vec!["forums".into(), "settings".into()]),
            Broadcast::PageTags(vec!["thread:5".into()]),
            Broadcast::PageCache,
        ] {
            assert_eq!(Broadcast::parse(b.kind(), &b.payload()), Some(b));
        }
    }

    mod properties {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// Whatever order events become visible in (and however often they are re-read),
            /// each is applied exactly once.
            #[test]
            fn each_event_applies_once(order in Just((1i64..=40).collect::<Vec<_>>()).prop_shuffle(), repeats in prop::collection::vec(1i64..=40, 0..30)) {
                let mut c = Cursor::default();
                let mut applied = std::collections::HashMap::new();
                for id in order.iter().chain(repeats.iter()) {
                    if c.mark(*id) {
                        *applied.entry(*id).or_insert(0) += 1;
                    }
                }
                prop_assert_eq!(applied.len(), 40);
                prop_assert!(applied.values().all(|n| *n == 1));
                prop_assert!(c.gaps.is_empty());
            }
        }
    }
}
