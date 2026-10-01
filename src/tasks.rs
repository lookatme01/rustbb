//! Scheduled tasks (MyBB's task system). Each run takes a Postgres advisory lock so only one node
//! executes a given task when several app servers are running.

use crate::app::App;
use crate::util::now;
use std::time::Duration;

pub fn spawn_scheduler(app: App) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tick.tick().await;
            if let Err(e) = run_due(&app).await {
                tracing::warn!("task scheduler error: {e:#}");
            }
        }
    });
}

async fn run_due(app: &App) -> anyhow::Result<()> {
    let due: Vec<(i32, String, i32, bool)> =
        sqlx::query_as("SELECT tid, key, interval_secs, logging FROM tasks WHERE enabled AND nextrun <= $1 ORDER BY nextrun").bind(now()).fetch_all(&app.db).await?;
    for (tid, key, interval, logging) in due {
        run_task(app, tid, &key, interval, logging).await?;
    }
    Ok(())
}

pub async fn run_task(
    app: &App,
    tid: i32,
    key: &str,
    interval: i32,
    logging: bool,
) -> anyhow::Result<String> {
    let mut conn = app.db.acquire().await?;
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(424242, $1)")
        .bind(tid)
        .fetch_one(&mut *conn)
        .await?;
    if !locked {
        return Ok("locked by another node".into());
    }
    let _ = interval;
    let result = execute(app, key).await;
    let msg = match &result {
        Ok(m) => m.clone(),
        Err(e) => format!("error: {e:#}"),
    };
    sqlx::query("UPDATE tasks SET lastrun = $2, nextrun = $2 + interval_secs WHERE tid = $1")
        .bind(tid)
        .bind(now())
        .execute(&mut *conn)
        .await?;
    if logging {
        sqlx::query("INSERT INTO tasklog (tid, dateline, data) VALUES ($1, $2, $3)")
            .bind(tid)
            .bind(now())
            .bind(&msg)
            .execute(&mut *conn)
            .await?;
    }
    let _ = sqlx::query("SELECT pg_advisory_unlock(424242, $1)")
        .bind(tid)
        .execute(&mut *conn)
        .await;
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
            sqlx::query("DELETE FROM user_audit WHERE dateline < $1")
                .bind(t - 365 * 86400)
                .execute(db)
                .await?;
            sqlx::query("DELETE FROM ratelimits WHERE reset_at < $1")
                .bind(t)
                .execute(db)
                .await?;
            format!("removed {a} stale sessions")
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
            sqlx::query("DELETE FROM spamlog WHERE dateline < $1")
                .bind(t - 90 * 86400)
                .execute(db)
                .await?;
            sqlx::query("DELETE FROM mailqueue WHERE attempts >= 5 AND dateline < $1")
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
                ("UPDATE users SET suspendposting = FALSE, suspensiontime = 0 WHERE suspendposting AND suspensiontime > 0 AND suspensiontime <= $1 RETURNING uid, username", "Posting suspension expired"),
                ("UPDATE users SET moderateposts = FALSE, moderationtime = 0 WHERE moderateposts AND moderationtime > 0 AND moderationtime <= $1 RETURNING uid, username", "Post moderation expired"),
                ("UPDATE users SET suspendsignature = FALSE, suspendsigtime = 0 WHERE suspendsignature AND suspendsigtime > 0 AND suspendsigtime <= $1 RETURNING uid, username", "Signature suspension expired"),
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
        "delayedmoderation" => crate::routes::moderation::run_delayed(app).await?,
        "promotions" => crate::admin::promotions::run_promotions(app).await?,
        "massmail" => crate::admin::massmail::run_batch(app).await?,
        other => format!("unknown task {other}"),
    })
}
