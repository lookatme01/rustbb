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
            self.gaps.remove(&id).is_some()
        }
    }

    fn expire_gaps(&mut self) {
        self.gaps.retain(|_, t| t.elapsed() < GAP_TTL);
    }
}

/// Read and apply every event this node has not seen yet.
pub async fn catch_up(app: &App, conn: &mut PgConnection) -> anyhow::Result<usize> {
    let gaps: Vec<i64> = {
        let mut c = app.cluster_cursor.lock().unwrap();
        c.expire_gaps();
        c.gaps.keys().copied().collect()
    };
    let max = app.cluster_cursor.lock().unwrap().max;
    let rows: Vec<(i64, String, String, Value)> = sqlx::query_as(
        "SELECT id, origin, kind, payload FROM cluster_events WHERE id > $1 OR id = ANY($2) ORDER BY id LIMIT 5000",
    )
    .bind(max)
    .bind(&gaps)
    .fetch_all(conn)
    .await?;
    let mut applied = 0;
    for (id, origin, kind, payload) in rows {
        if !app.cluster_cursor.lock().unwrap().mark(id) || origin == app.node_id {
            continue;
        }
        if let Some(b) = Broadcast::parse(&kind, &payload) {
            if let Err(e) = app.apply_broadcast(&b).await {
                tracing::warn!(error = %e, event = id, "applying cluster event failed");
            }
            applied += 1;
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
    fn broadcast_round_trip() {
        for b in [
            Broadcast::Cache(vec!["forums".into(), "settings".into()]),
            Broadcast::PageTags(vec!["thread:5".into()]),
            Broadcast::PageCache,
        ] {
            assert_eq!(Broadcast::parse(b.kind(), &b.payload()), Some(b));
        }
    }
}
