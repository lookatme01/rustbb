//! Scheduled tasks (MyBB's task system).
//!
//! Several nodes may run the scheduler. A due task is claimed in one short transaction: a
//! transaction-scoped advisory lock serializes claims of that task, and the claim sets a lease
//! (`locked_until`) and the next run time. The task then runs with no lock or connection held,
//! and the lease is released when it ends. A node that dies mid-task leaves a lease that simply
//! expires. Unrelated due tasks run in parallel, a few at a time.

use crate::app::App;
use crate::util::now;
use std::time::Duration;

/// Due tasks run at once per node.
const PARALLEL: usize = 3;
/// A task running longer than this may be started again elsewhere.
const LEASE: Duration = Duration::from_secs(30 * 60);

pub fn spawn_scheduler(app: App) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        let mut stop = app.shutdown.subscribe();
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = stop.wait_for(|s| *s) => break,
            }
            if let Err(e) = run_due(&app).await {
                tracing::warn!("task scheduler error: {e:#}");
            }
        }
    })
}

pub async fn run_due(app: &App) -> anyhow::Result<()> {
    use futures::StreamExt;
    let due: Vec<(i32, String, i32, bool)> = sqlx::query_as(
        "SELECT tid, key, interval_secs, logging FROM tasks
         WHERE enabled AND nextrun <= $1 AND (locked_until IS NULL OR locked_until < now()) ORDER BY nextrun",
    )
    .bind(now())
    .fetch_all(&app.db)
    .await?;
    futures::stream::iter(due)
        .for_each_concurrent(PARALLEL, |(tid, key, interval, logging)| async move {
            let _ = interval;
            if let Err(e) = run(app, tid, &key, logging, false).await {
                tracing::warn!(task = %key, "task failed: {e:#}");
            }
        })
        .await;
    Ok(())
}

/// Take the lease on a task if nobody holds it. `force` ignores the schedule (run now).
async fn claim(app: &App, tid: i32, force: bool) -> sqlx::Result<bool> {
    let mut tx = app.db.begin().await?;
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(424242, $1)")
        .bind(tid)
        .fetch_one(&mut *tx)
        .await?;
    if !locked {
        return Ok(false);
    }
    let claimed = sqlx::query(
        "UPDATE tasks SET locked_until = now() + make_interval(secs => $2), locked_by = $3, nextrun = $4 + interval_secs
         WHERE tid = $1 AND (locked_until IS NULL OR locked_until < now()) AND ($5 OR nextrun <= $4)",
    )
    .bind(tid)
    .bind(LEASE.as_secs_f64())
    .bind(&app.node_id)
    .bind(now())
    .bind(force)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    tx.commit().await?;
    Ok(claimed)
}

/// Run one task now, whatever its schedule (if no other node is running it), and log it.
pub async fn run_task(
    app: &App,
    tid: i32,
    key: &str,
    _interval: i32,
    logging: bool,
) -> anyhow::Result<String> {
    run(app, tid, key, logging, true).await
}

async fn run(app: &App, tid: i32, key: &str, logging: bool, force: bool) -> anyhow::Result<String> {
    if !claim(app, tid, force).await? {
        return Ok("already running or done on another node".into());
    }
    let t0 = std::time::Instant::now();
    // Run in its own task so a panic is contained and the lease is still released.
    let result = {
        let (app, key) = (app.clone(), key.to_string());
        tokio::spawn(async move { execute(&app, &key).await }).await
    };
    let msg = match result {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => format!("error: {e:#}"),
        Err(e) => format!("error: task panicked: {e}"),
    };
    crate::infra::metrics::observe(
        "rbb_task_seconds",
        &[("task", key)],
        t0.elapsed().as_secs_f64(),
    );
    sqlx::query(
        "UPDATE tasks SET lastrun = $2, locked_until = NULL, locked_by = NULL WHERE tid = $1",
    )
    .bind(tid)
    .bind(now())
    .execute(&app.db)
    .await?;
    if logging {
        sqlx::query("INSERT INTO tasklog (tid, dateline, data) VALUES ($1, $2, $3)")
            .bind(tid)
            .bind(now())
            .bind(&msg)
            .execute(&app.db)
            .await?;
    }
    Ok(msg)
}

async fn execute(app: &App, key: &str) -> anyhow::Result<String> {
    let t = now();
    let db = &app.db;
    let cache = app.cache();
    let s = &cache.settings;
    Ok(match key {
        "hourlycleanup" => {
            let a = sqlx::query("DELETE FROM sessions WHERE time < $1")
                .bind(t - 86400)
                .execute(db)
                .await?
                .rows_affected();
            sqlx::query("DELETE FROM captcha WHERE dateline < $1")
                .bind(t - 3600)
                .execute(db)
                .await?;
            sqlx::query("DELETE FROM searchlog WHERE dateline < $1")
                .bind(t - 86400)
                .execute(db)
                .await?;
            sqlx::query("DELETE FROM awaitingactivation WHERE type = 'p' AND dateline < $1")
                .bind(t - 86400)
                .execute(db)
                .await?;
            sqlx::query("DELETE FROM logins WHERE expires < $1")
                .bind(t)
                .execute(db)
                .await?;
            crate::passkeys::prune(db).await?;
            sqlx::query(
                "DELETE FROM cluster_events WHERE created_at < now() - make_interval(secs => $1)",
            )
            .bind(crate::infra::cluster::RETENTION_SECS as f64)
            .execute(db)
            .await?;
            sqlx::query("DELETE FROM applied_batches WHERE applied_at < now() - interval '1 day'")
                .execute(db)
                .await?;
            sqlx::query("DELETE FROM ratelimits WHERE reset_at < $1")
                .bind(t)
                .execute(db)
                .await?;
            let orphans = crate::ops::prune_orphaned_attachments(app)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let tmp = crate::infra::uploads::sweep_tmp(&app.cfg.upload_dir).await;
            format!(
                "removed {a} stale sessions, {orphans} orphaned attachments, {tmp} stale upload files"
            )
        }
        "dailycleanup" => {
            let cut = t - s.int("threadreadcut").max(1) * 86400;
            let a = sqlx::query("DELETE FROM threadsread WHERE dateline < $1")
                .bind(cut)
                .execute(db)
                .await?
                .rows_affected();
            sqlx::query("DELETE FROM forumsread WHERE dateline < $1")
                .bind(cut)
                .execute(db)
                .await?;
            sqlx::query("DELETE FROM alerts WHERE unread = FALSE AND dateline < $1")
                .bind(t - 90 * 86400)
                .execute(db)
                .await?;
            sqlx::query("DELETE FROM tasklog WHERE dateline < $1")
                .bind(t - 30 * 86400)
                .execute(db)
                .await?;
            sqlx::query("DELETE FROM outbox WHERE status = 'dead' AND created_at < now() - interval '30 days'")
                .execute(db)
                .await?;
            sqlx::query("DELETE FROM mailqueue WHERE status = 'dead' AND dateline < $1")
                .bind(t - 7 * 86400)
                .execute(db)
                .await?;
            format!("pruned {a} read markers")
        }
        "banlifter" => {
            let lifted: Vec<(i32, i32, Vec<i32>, i32)> = sqlx::query_as(
                "DELETE FROM banned WHERE lifted > 0 AND lifted <= $1 RETURNING uid, oldgroup, oldadditionalgroups, olddisplaygroup",
            )
            .bind(t)
            .fetch_all(db)
            .await?;
            let mut unbanned: Vec<(i32, String)> = vec![];
            for (uid, g, ag, dg) in &lifted {
                let name: Option<String> = sqlx::query_scalar("UPDATE users SET usergroup = $2, additionalgroups = $3, displaygroup = $4 WHERE uid = $1 RETURNING username")
                    .bind(uid)
                    .bind(g)
                    .bind(ag)
                    .bind(dg)
                    .fetch_optional(db)
                    .await?;
                unbanned.extend(name.map(|n| (*uid, n)));
            }
            crate::system::log_expiries(app, "Ban expired", &unbanned).await;
            for (sql, action) in [
                (
                    "UPDATE users SET suspendposting = FALSE, suspensiontime = 0 WHERE suspendposting AND suspensiontime > 0 AND suspensiontime <= $1 RETURNING uid, username",
                    "Posting suspension expired",
                ),
                (
                    "UPDATE users SET moderateposts = FALSE, moderationtime = 0 WHERE moderateposts AND moderationtime > 0 AND moderationtime <= $1 RETURNING uid, username",
                    "Post moderation expired",
                ),
                (
                    "UPDATE users SET suspendsignature = FALSE, suspendsigtime = 0 WHERE suspendsignature AND suspendsigtime > 0 AND suspendsigtime <= $1 RETURNING uid, username",
                    "Signature suspension expired",
                ),
            ] {
                let ended: Vec<(i32, String)> = sqlx::query_as(sql).bind(t).fetch_all(db).await?;
                crate::system::log_expiries(app, action, &ended).await;
            }
            format!("lifted {} bans", lifted.len())
        }
        "warnings" => {
            let rows: Vec<(i32, i32)> = sqlx::query_as(
                "UPDATE warnings SET expired = TRUE WHERE expired = FALSE AND daterevoked = 0 AND expires > 0 AND expires <= $1 RETURNING uid, points",
            )
            .bind(t)
            .fetch_all(db)
            .await?;
            let mut expired: Vec<(i32, String)> = vec![];
            for (uid, pts) in &rows {
                let name: Option<String> = sqlx::query_scalar("UPDATE users SET warningpoints = GREATEST(warningpoints - $2, 0) WHERE uid = $1 RETURNING username").bind(uid).bind(pts).fetch_optional(db).await?;
                expired.extend(name.map(|n| (*uid, n)));
            }
            crate::system::log_expiries(app, "Warning expired", &expired).await;
            format!("expired {} warnings", rows.len())
        }
        "threadviews" => {
            let n = sqlx::query("DELETE FROM threads WHERE redirect_expires > 0 AND redirect_expires <= $1 AND closed LIKE 'moved|%'").bind(t).execute(db).await?.rows_affected();
            format!("removed {n} expired redirects")
        }
        "dailystats" => {
            let day = t - t % 86400;
            sqlx::query(
                "INSERT INTO stats (dateline, numusers, numthreads, numposts)
                 SELECT $1, (SELECT numusers FROM counters WHERE id = 1), COALESCE(SUM(threads), 0), COALESCE(SUM(posts), 0) FROM forums
                 ON CONFLICT (dateline) DO UPDATE SET numusers = EXCLUDED.numusers, numthreads = EXCLUDED.numthreads, numposts = EXCLUDED.numposts",
            )
            .bind(day)
            .execute(db)
            .await?;
            "recorded".into()
        }
        "userpruning" => {
            let n = sqlx::query(
                "DELETE FROM users WHERE usergroup = 5 AND regdate < $1 AND postnum = 0",
            )
            .bind(t - 30 * 86400)
            .execute(db)
            .await?
            .rows_affected();
            if n > 0 {
                sqlx::query(
                    "UPDATE counters SET numusers = (SELECT COUNT(*) FROM users) WHERE id = 1",
                )
                .execute(db)
                .await?;
            }
            format!("pruned {n} unactivated users")
        }
        "recyclebin" => {
            let tids: Vec<i32> = sqlx::query_scalar("SELECT tid FROM threads WHERE visible = -1 AND deletetime > 0 AND deletetime < $1 LIMIT 500")
                .bind(t - 90 * 86400)
                .fetch_all(db)
                .await?;
            if !tids.is_empty() {
                crate::ops::delete_threads(app, &tids)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            format!("purged {} old soft-deleted threads", tids.len())
        }
        "automoderation" => crate::automod::run(app).await?,
        "systemautoclose" => crate::system::autoclose(app).await?,
        "privacy" => crate::privacy::run(app).await?,
        "delayedmoderation" => crate::routes::moderation::run_delayed(app).await?,
        "promotions" => crate::admin::promotions::run_promotions(app).await?,
        "badges" => crate::badges::run(app).await?,
        "massmail" => crate::admin::massmail::run_batch(app).await?,
        other => format!("unknown task {other}"),
    })
}
