//! Transactional outbox: durable side effects of a change, run by background workers after the
//! change commits.
//!
//! A use case adds [`Job`]s to its unit of work; they are inserted into `outbox` inside the same
//! transaction, so a job exists if and only if the change it belongs to committed. Workers claim
//! due jobs in a short statement (`FOR UPDATE SKIP LOCKED`, setting a lease), run them outside
//! any transaction with a timeout, then delete them or reschedule them with exponential backoff.
//! A worker claims only as many jobs as it runs at once, so every job starts as soon as it is
//! leased and finishes (or times out) well before the lease runs out. A job whose worker died is
//! reclaimed when its lease expires; one that keeps failing is marked `dead` and kept for
//! inspection. Finishing checks the lease token, so a worker that lost its lease never deletes
//! or reschedules a job another worker has reclaimed.
//!
//! Jobs may run more than once (a worker can die after the work but before the delete, or fail
//! halfway through notifying several members), so every job must be safe to repeat: alerts,
//! private messages and emails are created under a delivery key ([`Delivery`]) recorded in
//! `deliveries`, and a retry skips the ones already delivered.

use crate::app::App;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use std::time::Duration;

pub const CHANNEL: &str = "rbb_outbox";
/// Jobs claimed and run at once. Each claimed job starts immediately, so it finishes or times out
/// after at most [`JOB_TIMEOUT`], well inside its lease.
const BATCH: i64 = 8;
const LEASE_SECS: i64 = 120;
const JOB_TIMEOUT: Duration = Duration::from_secs(60);
const _: () = assert!(JOB_TIMEOUT.as_secs() * 3 / 2 <= LEASE_SECS as u64);
pub const MAX_ATTEMPTS: i32 = 8;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Job {
    /// Run a plugin action hook.
    Hook {
        name: String,
        data: serde_json::Value,
    },
    /// A private message from the System account.
    SystemPm {
        uid: i32,
        subject: String,
        message: String,
    },
    /// Send the board's welcome PM to new members.
    WelcomePm { members: Vec<(i32, String)> },
    /// Subscriptions, mentions and quotes for a new visible post.
    PostNotifications {
        new_thread: bool,
        fid: i32,
        tid: i32,
        pid: i32,
        uid: i32,
        username: String,
        subject: String,
        message: String,
    },
    /// Remove uploaded files (paths relative to the upload directory) whose rows are gone.
    DeleteFiles { paths: Vec<String> },
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
            Job::SystemPm { .. } => "system_pm",
            Job::Alert { .. } => "alert",
            Job::DeleteFiles { .. } => "delete_files",
            Job::PostNotifications { .. } => "post_notifications",
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

/// Where a job delivers something (an alert, private message or email) to a member. A retry of
/// the job uses the same keys, so it skips what the earlier attempt already delivered.
#[derive(Clone, Copy)]
pub struct Delivery<'a> {
    job: &'a str,
}

impl<'a> Delivery<'a> {
    pub fn new(job: &'a str) -> Self {
        Delivery { job }
    }

    /// The key for one delivery of this job, e.g. `kind = "alert:quoted"`, `to = uid`.
    pub fn key(&self, kind: &str, to: i32) -> String {
        format!("{}:{kind}:{to}", self.job)
    }
}

/// Record the delivery `key` as part of the caller's transaction. Returns false if it was
/// already made (the caller then skips it). A concurrent attempt with the same key waits for
/// this transaction and then sees it.
pub async fn first_delivery(conn: &mut PgConnection, key: &str) -> sqlx::Result<bool> {
    let r = sqlx::query("INSERT INTO deliveries (key) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(key)
        .execute(conn)
        .await?;
    Ok(r.rows_affected() == 1)
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
async fn claim(app: &App, lease: uuid::Uuid) -> sqlx::Result<Vec<(i64, serde_json::Value, i32)>> {
    sqlx::query_as(
        "UPDATE outbox SET locked_until = now() + make_interval(secs => $2), lease = $3, attempts = attempts + 1
         WHERE id IN (
             SELECT id FROM outbox
             WHERE status = 'pending' AND available_at <= now() AND (locked_until IS NULL OR locked_until < now())
             ORDER BY available_at, id LIMIT $1 FOR UPDATE SKIP LOCKED)
         RETURNING id, payload, attempts",
    )
    .bind(BATCH)
    .bind(LEASE_SECS as f64)
    .bind(lease)
    .fetch_all(&app.db)
    .await
}

/// Delete or reschedule a job, if this worker still holds its lease.
async fn finish(app: &App, id: i64, lease: uuid::Uuid, attempts: i32, result: anyhow::Result<()>) {
    let r = match result {
        Ok(()) => {
            sqlx::query("DELETE FROM outbox WHERE id = $1 AND lease = $2")
                .bind(id)
                .bind(lease)
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
                "UPDATE outbox SET locked_until = NULL, lease = NULL, last_error = $2,
                    available_at = now() + make_interval(secs => $3),
                    status = CASE WHEN $4 THEN 'dead' ELSE 'pending' END
                 WHERE id = $1 AND lease = $5",
            )
            .bind(id)
            .bind(format!("{e:#}"))
            .bind(backoff(attempts).as_secs_f64())
            .bind(dead)
            .bind(lease)
            .execute(&app.db)
            .await
        }
    };
    match r {
        // The lease ran out and another worker reclaimed the job; its result is the one that counts.
        Ok(r) if r.rows_affected() == 0 => {
            tracing::warn!(job = id, "outbox job finished after losing its lease")
        }
        Ok(_) => {}
        // The lease runs out and the job is retried.
        Err(e) => tracing::warn!(job = id, error = %e, "could not record outbox job result"),
    }
}

async fn run(app: &App, id: i64, job: Job) -> anyhow::Result<()> {
    let job_key = format!("outbox:{id}");
    let d = Delivery::new(&job_key);
    match job {
        Job::Hook { name, data } => app.plugins.run_hook_async(&name, data).await,
        Job::PostNotifications {
            new_thread,
            fid,
            tid,
            pid,
            uid,
            username,
            subject,
            message,
        } => {
            if new_thread {
                crate::notify::forum_subscribers(app, d, fid, tid, uid, &subject, &username)
                    .await?;
            } else {
                crate::notify::thread_subscribers(
                    app, d, tid, pid, uid, &subject, &username, &message,
                )
                .await?;
            }
            crate::notify::mentions_and_quotes(app, d, uid, &username, tid, pid, &subject, &message)
                .await
        }
        Job::DeleteFiles { paths } => {
            for p in paths {
                crate::infra::storage::delete(app, &p).await?;
            }
            Ok(())
        }
        Job::SystemPm {
            uid,
            subject,
            message,
        } => {
            let key = d.key("pm", uid);
            crate::routes::private::deliver_system_pm(app, Some(&key), uid, &subject, &message)
                .await?;
            Ok(())
        }
        Job::WelcomePm { members } => crate::system::welcome(app, Some(d), &members).await,
        Job::Alert {
            uid,
            from_uid,
            alert,
            object_id,
            extra,
        } => {
            let key = d.key("alert", uid);
            crate::notify::deliver_alert(app, Some(&key), uid, from_uid, &alert, object_id, extra)
                .await
        }
    }
}

/// Claim and run one batch. Returns how many jobs were claimed.
pub async fn run_batch(app: &App) -> anyhow::Result<usize> {
    use futures::StreamExt;
    let lease = uuid::Uuid::new_v4();
    let jobs = claim(app, lease).await?;
    let n = jobs.len();
    // Every claimed job runs at once: none waits for a slot while its lease runs down.
    futures::stream::iter(jobs)
        .for_each_concurrent(None, |(id, payload, attempts)| async move {
            let result = match serde_json::from_value::<Job>(payload) {
                Ok(job) => match tokio::time::timeout(JOB_TIMEOUT, run(app, id, job)).await {
                    Ok(r) => r,
                    Err(_) => Err(anyhow::anyhow!("timed out after {JOB_TIMEOUT:?}")),
                },
                // Unknown kind (written by a newer version?): let it retry, then go dead.
                Err(e) => Err(anyhow::anyhow!("cannot decode job: {e}")),
            };
            finish(app, id, lease, attempts, result).await;
        })
        .await;
    Ok(n)
}

/// Run outbox jobs until shutdown: woken by NOTIFY (via the listener), and every second.
pub fn spawn_worker(app: App) -> tokio::task::JoinHandle<()> {
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
    })
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
    fn delivery_keys_are_stable_per_job_and_recipient() {
        let d = Delivery::new("outbox:7");
        assert_eq!(d.key("alert:quoted", 3), "outbox:7:alert:quoted:3");
        assert_eq!(d.key("pm", 3), Delivery::new("outbox:7").key("pm", 3));
        assert_ne!(d.key("pm", 3), Delivery::new("outbox:8").key("pm", 3));
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
