//! Transactional outbox: durable side effects of a change, run by background workers after the
//! change commits.
//!
//! A use case adds [`Job`]s to its unit of work; they are inserted into `outbox` inside the same
//! transaction, so a job exists if and only if the change it belongs to committed. Workers claim
//! due jobs in a short statement (`FOR UPDATE SKIP LOCKED`, setting a lease), run them outside
//! any transaction with bounded concurrency and a timeout, then delete them or reschedule them
//! with exponential backoff. A job whose worker died is reclaimed when its lease expires; one that
//! keeps failing is marked `dead` and kept for inspection. Jobs may run more than once (a worker
//! can die after the work but before the delete), so every job must be safe to repeat.

use crate::app::App;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use std::time::Duration;

pub const CHANNEL: &str = "rbb_outbox";
const BATCH: i64 = 32;
const CONCURRENCY: usize = 8;
const LEASE_SECS: i64 = 120;
const JOB_TIMEOUT: Duration = Duration::from_secs(60);
pub const MAX_ATTEMPTS: i32 = 8;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Job {
    /// Run a plugin action hook.
    Hook {
        name: String,
        data: serde_json::Value,
    },
    /// Send the board's welcome PM to new members.
    WelcomePm { members: Vec<(i32, String)> },
    /// Create an alert for a member (and push it live).
    Alert {
        uid: i32,
        from_uid: i32,
        alert: String,
        object_id: i32,
        extra: serde_json::Value,
    },
}

impl Job {
    pub fn kind(&self) -> &'static str {
        match self {
            Job::Hook { .. } => "hook",
            Job::WelcomePm { .. } => "welcome_pm",
            Job::Alert { .. } => "alert",
        }
    }
}

/// Add a job to the outbox as part of the caller's transaction. With an idempotency key, a
/// second job with the same key is ignored.
pub async fn enqueue(conn: &mut PgConnection, job: &Job, key: Option<&str>) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO outbox (kind, payload, idempotency_key) VALUES ($1, $2, $3)
         ON CONFLICT (idempotency_key) WHERE idempotency_key IS NOT NULL DO NOTHING",
    )
    .bind(job.kind())
    .bind(serde_json::to_value(job).unwrap_or_default())
    .bind(key)
    .execute(&mut *conn)
    .await?;
    sqlx::query("SELECT pg_notify('rbb_outbox', '')")
        .execute(conn)
        .await?;
    Ok(())
}

/// Delay before attempt `n + 1`: 5 s, 10 s, 20 s… capped at an hour, with jitter.
pub fn backoff(attempts: i32) -> Duration {
    let base = 5u64
        .saturating_mul(1 << attempts.clamp(0, 20) as u64)
        .min(3600);
    let jitter = rand::random::<u64>() % (base / 4 + 1);
    Duration::from_secs(base + jitter)
}

/// Lease up to a batch of due jobs (one short statement; nothing stays locked while they run).
async fn claim(app: &App) -> sqlx::Result<Vec<(i64, serde_json::Value, i32)>> {
    sqlx::query_as(
        "UPDATE outbox SET locked_until = now() + make_interval(secs => $2), attempts = attempts + 1
         WHERE id IN (
             SELECT id FROM outbox
             WHERE status = 'pending' AND available_at <= now() AND (locked_until IS NULL OR locked_until < now())
             ORDER BY available_at, id LIMIT $1 FOR UPDATE SKIP LOCKED)
         RETURNING id, payload, attempts",
    )
    .bind(BATCH)
    .bind(LEASE_SECS as f64)
    .fetch_all(&app.db)
    .await
}

async fn finish(app: &App, id: i64, attempts: i32, result: anyhow::Result<()>) {
    let r = match result {
        Ok(()) => {
            sqlx::query("DELETE FROM outbox WHERE id = $1")
                .bind(id)
                .execute(&app.db)
                .await
        }
        Err(e) => {
            let dead = attempts >= MAX_ATTEMPTS;
            if dead {
                tracing::error!(job = id, attempts, error = %format!("{e:#}"), "outbox job failed permanently");
            } else {
                tracing::warn!(job = id, attempts, error = %format!("{e:#}"), "outbox job failed; will retry");
            }
            crate::infra::metrics::counter("rbb_outbox_failures_total", 1);
            sqlx::query(
                "UPDATE outbox SET locked_until = NULL, last_error = $2,
                    available_at = now() + make_interval(secs => $3),
                    status = CASE WHEN $4 THEN 'dead' ELSE 'pending' END
                 WHERE id = $1",
            )
            .bind(id)
            .bind(format!("{e:#}"))
            .bind(backoff(attempts).as_secs_f64())
            .bind(dead)
            .execute(&app.db)
            .await
        }
    };
    if let Err(e) = r {
        // The lease runs out and the job is retried.
        tracing::warn!(job = id, error = %e, "could not record outbox job result");
    }
}

async fn run(app: &App, job: Job) -> anyhow::Result<()> {
    match job {
        Job::Hook { name, data } => app.plugins.run_hook_async(&name, data).await,
        Job::WelcomePm { members } => {
            crate::system::welcome(app, &members).await;
            Ok(())
        }
        Job::Alert {
            uid,
            from_uid,
            alert,
            object_id,
            extra,
        } => {
            crate::notify::alert(app, uid, from_uid, &alert, object_id, extra).await;
            Ok(())
        }
    }
}

/// Claim and run one batch. Returns how many jobs were claimed.
pub async fn run_batch(app: &App) -> anyhow::Result<usize> {
    use futures::StreamExt;
    let jobs = claim(app).await?;
    let n = jobs.len();
    futures::stream::iter(jobs)
        .for_each_concurrent(CONCURRENCY, |(id, payload, attempts)| async move {
            let result = match serde_json::from_value::<Job>(payload) {
                Ok(job) => match tokio::time::timeout(JOB_TIMEOUT, run(app, job)).await {
                    Ok(r) => r,
                    Err(_) => Err(anyhow::anyhow!("timed out after {JOB_TIMEOUT:?}")),
                },
                // Unknown kind (written by a newer version?): let it retry, then go dead.
                Err(e) => Err(anyhow::anyhow!("cannot decode job: {e}")),
            };
            finish(app, id, attempts, result).await;
        })
        .await;
    Ok(n)
}

/// Run outbox jobs until shutdown: woken by NOTIFY (via the listener), and every second.
pub fn spawn_worker(app: App) {
    tokio::spawn(async move {
        let mut stop = app.shutdown.subscribe();
        loop {
            match run_batch(&app).await {
                Ok(n) if n as i64 == BATCH => continue,
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "outbox worker error"),
            }
            tokio::select! {
                _ = app.outbox_wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = stop.wait_for(|s| *s) => break,
            }
        }
    });
}

/// (pending, dead, age in seconds of the oldest pending job).
pub async fn stats(db: &sqlx::PgPool) -> sqlx::Result<(i64, i64, f64)> {
    sqlx::query_as(
        "SELECT COUNT(*) FILTER (WHERE status = 'pending'), COUNT(*) FILTER (WHERE status = 'dead'),
                COALESCE(EXTRACT(EPOCH FROM now() - MIN(created_at) FILTER (WHERE status = 'pending')), 0)::float8
         FROM outbox",
    )
    .fetch_one(db)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        assert!(backoff(0) >= Duration::from_secs(5) && backoff(0) <= Duration::from_secs(7));
        assert!(backoff(3) >= Duration::from_secs(40));
        assert!(backoff(30) <= Duration::from_secs(3600 + 901));
    }

    #[test]
    fn jobs_serialize_with_their_kind() {
        let j = Job::Hook {
            name: "x".into(),
            data: serde_json::json!({"a": 1}),
        };
        let v = serde_json::to_value(&j).unwrap();
        assert_eq!(v["kind"], j.kind());
        assert_eq!(serde_json::from_value::<Job>(v).unwrap(), j);
    }
}
