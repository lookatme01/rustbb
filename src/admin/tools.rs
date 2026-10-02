//! Tools & maintenance: system health, tasks, recount/rebuild, cache, logs, backups, plugins.

use crate::ctx::{CsrfForm, Ctx};
use crate::error::{AppError, AppResult};
use crate::util::{self, now};
use axum::Router;
use axum::body::Body;
use axum::extract::Query;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use std::collections::HashMap;

pub fn router() -> Router<crate::app::App> {
    Router::new()
        .route("/tools", get(health))
        .route("/tools/tasks", get(tasks).post(tasks_action))
        .route("/tools/recount", get(recount).post(recount_run))
        .route("/tools/cache", get(cache).post(cache_reload))
        .route("/tools/adminlog", get(adminlog))
        .route("/tools/maillogs", get(maillogs))
        .route("/tools/mailerrors", get(mailerrors).post(mailerrors_action))
        .route("/tools/spamlog", get(spamlog))
        .route("/tools/systemlog", get(systemlog))
        .route("/tools/erasurelog", get(erasurelog))
        .route("/tools/stats", get(stats))
        .route("/tools/backup", get(backup_page).post(backup))
        .route("/tools/plugins", get(plugins))
        .route(
            "/tools/attachments",
            get(attachments).post(attachments_cleanup),
        )
        .route("/tools/testmail", post(testmail))
}

#[derive(Deserialize, Default)]
pub struct AnyForm {
    #[serde(default, flatten)]
    pub fields: HashMap<String, serde_json::Value>,
}

fn s(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(a)) => {
            a.last().and_then(|x| x.as_str()).unwrap_or("").to_string()
        }
        _ => String::new(),
    }
}

async fn dir_size(path: String) -> u64 {
    tokio::task::spawn_blocking(move || {
        fn walk(p: &std::path::Path) -> u64 {
            let Ok(rd) = std::fs::read_dir(p) else {
                return 0;
            };
            rd.flatten()
                .map(|e| {
                    let m = e.metadata();
                    match m {
                        Ok(m) if m.is_dir() => walk(&e.path()),
                        Ok(m) => m.len(),
                        _ => 0,
                    }
                })
                .sum()
        }
        walk(std::path::Path::new(&path))
    })
    .await
    .unwrap_or(0)
}

pub async fn health(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "tools");
    let db = &ctx.app.db;
    let tables: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT relname::text, n_live_tup, pg_total_relation_size(relid) FROM pg_stat_user_tables ORDER BY pg_total_relation_size(relid) DESC LIMIT 20",
    )
    .fetch_all(db)
    .await?;
    let conns: (i64, i64) = sqlx::query_as("SELECT COUNT(*) FILTER (WHERE state = 'active'), COUNT(*) FROM pg_stat_activity WHERE datname = current_database()").fetch_one(db).await?;
    let cache_hit: Option<f64> = sqlx::query_scalar("SELECT ROUND(100.0 * sum(blks_hit) / NULLIF(sum(blks_hit) + sum(blks_read), 0), 2)::float8 FROM pg_stat_database WHERE datname = current_database()").fetch_one(db).await?;
    let uploads = dir_size(ctx.app.cfg.upload_dir.clone()).await;
    let pool = (ctx.app.db.size(), ctx.app.db.num_idle());
    crate::admin::page(
        &ctx,
        "admin/health.html",
        "tools",
        "System Health",
        minijinja::context! { tables => tables, active => conns.0, total_conns => conns.1, cache_hit => cache_hit, uploads => uploads as i64, pool_size => pool.0, pool_idle => pool.1,
            activity_buffer => ctx.app.activity.len(), view_buffer => ctx.app.thread_views.len(), live_subscribers => ctx.app.live.subscribers(), node => &ctx.app.node_id,
            uptime => crate::routes::member::format_duration(now() - ctx.app.started), version => env!("CARGO_PKG_VERSION") },
    )
    .await
}

pub async fn tasks(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "tools");
    let rows: Vec<(i32, String, String, String, i32, i64, i64, bool, bool)> =
        sqlx::query_as("SELECT tid, key, title, description, interval_secs, nextrun, lastrun, enabled, logging FROM tasks ORDER BY title").fetch_all(&ctx.app.db).await?;
    let logs: Vec<(String, i64, String)> = sqlx::query_as("SELECT t.title, l.dateline, l.data FROM tasklog l JOIN tasks t ON t.tid = l.tid ORDER BY l.lid DESC LIMIT 50").fetch_all(&ctx.app.db).await?;
    crate::admin::page(
        &ctx,
        "admin/tasks.html",
        "tools",
        "Scheduled Tasks",
        minijinja::context! { tasks => rows, logs => logs },
    )
    .await
}

pub async fn tasks_action(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "tools");
    let tid: i32 = s(f.fields.get("tid")).parse().unwrap_or(0);
    let (key, interval, logging): (String, i32, bool) =
        sqlx::query_as("SELECT key, interval_secs, logging FROM tasks WHERE tid = $1")
            .bind(tid)
            .fetch_optional(&ctx.app.db)
            .await?
            .ok_or_else(|| AppError::not_found("task"))?;
    let msg = match s(f.fields.get("action")).as_str() {
        "run" => {
            let r = crate::tasks::run_task(&ctx.app, tid, &key, interval, logging)
                .await
                .map_err(AppError::Other)?;
            format!("Task ran: {r}")
        }
        "toggle" => {
            sqlx::query("UPDATE tasks SET enabled = NOT enabled WHERE tid = $1")
                .bind(tid)
                .execute(&ctx.app.db)
                .await?;
            "Task updated.".into()
        }
        "interval" => {
            let secs: i32 = s(f.fields.get("interval"))
                .parse::<i32>()
                .unwrap_or(interval)
                .max(60);
            sqlx::query("UPDATE tasks SET interval_secs = $2, nextrun = LEAST(nextrun, $3 + $2) WHERE tid = $1").bind(tid).bind(secs).bind(now()).execute(&ctx.app.db).await?;
            "Interval updated.".into()
        }
        _ => return Err(AppError::user("Unknown action.")),
    };
    crate::admin::log(
        &ctx,
        "tools",
        "Task action",
        serde_json::json!({"task": key}),
    )
    .await;
    Ok(ctx.redirect("/admin/tools/tasks", &msg))
}

pub async fn recount(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "tools");
    crate::admin::page(
        &ctx,
        "admin/recount.html",
        "tools",
        "Recount & Rebuild",
        minijinja::context! {},
    )
    .await
}

pub async fn recount_run(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "tools");
    let what = s(f.fields.get("what"));
    let app = ctx.app.clone();
    let w = what.clone();
    let msg = match what.as_str() {
        "posthtml" => {
            ctx.app.bump_parser_rev().await?;
            "The parsed post cache has been invalidated; posts will be re-parsed as they are viewed."
        }
        "forums" | "threads" | "users" | "all" | "reputation" | "pms" | "attachments" => {
            tokio::spawn(async move {
                let t0 = std::time::Instant::now();
                let r: AppResult<()> = async {
                    match w.as_str() {
                        "forums" => crate::ops::rebuild_forum_counters(&app).await?,
                        "users" => crate::ops::rebuild_user_counters(&app).await?,
                        "all" | "threads" => crate::ops::rebuild_all_counters(&app).await?,
                        "reputation" => {
                            sqlx::query("UPDATE users u SET reputation = COALESCE((SELECT SUM(reputation) FROM reputation r WHERE r.uid = u.uid), 0)").execute(&app.db).await?;
                        }
                        "pms" => {
                            sqlx::query(
                                "UPDATE users u SET totalpms = (SELECT COUNT(*) FROM privatemessages p WHERE p.uid = u.uid),
                                    unreadpms = (SELECT COUNT(*) FROM privatemessages p WHERE p.uid = u.uid AND p.status = 0 AND p.folder NOT IN (2,3))",
                            )
                            .execute(&app.db)
                            .await?;
                        }
                        "attachments" => {
                            sqlx::query("UPDATE threads t SET attachmentcount = (SELECT COUNT(*) FROM attachments a JOIN posts p ON p.pid = a.pid WHERE p.tid = t.tid AND p.visible = 1 AND a.visible)").execute(&app.db).await?;
                        }
                        _ => {}
                    }
                    Ok(())
                }
                .await;
                let msg = match r {
                    Ok(()) => format!(
                        "Rebuild '{w}' finished in {:.1}s",
                        t0.elapsed().as_secs_f64()
                    ),
                    Err(e) => format!("Rebuild '{w}' failed: {e}"),
                };
                tracing::info!("{msg}");
                let _ = sqlx::query("INSERT INTO adminlog (uid, dateline, module, action, data) VALUES (0, $1, 'tools', $2, '{}')").bind(now()).bind(&msg).execute(&app.db).await;
                app.stats_cache.invalidate_all();
            });
            "The rebuild has started in the background. Its result will appear in the admin log."
        }
        _ => return Err(AppError::user("Unknown rebuild.")),
    };
    crate::admin::log(
        &ctx,
        "tools",
        "Started rebuild",
        serde_json::json!({"what": what}),
    )
    .await;
    Ok(ctx.redirect("/admin/tools/recount", msg))
}

pub async fn cache(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "tools");
    let c = &ctx.cache;
    let parts: Vec<(String, usize)> = vec![
        ("settings".into(), c.settings.map().len()),
        ("groups".into(), c.groups.len()),
        ("forums".into(), c.forums.len()),
        ("forumperms".into(), c.forum_perms.len()),
        ("moderators".into(), c.moderators.len()),
        (
            "parser".into(),
            c.parser.smilies.len() + c.parser.badwords.len() + c.parser.custom.len(),
        ),
        ("icons".into(), c.icons.len()),
        ("prefixes".into(), c.prefixes.len()),
        ("themes".into(), c.themes.len()),
        ("templates".into(), c.templates.len()),
        ("attachtypes".into(), c.attachtypes.len()),
        ("profilefields".into(), c.profilefields.len()),
        ("usertitles".into(), c.usertitles.len()),
        ("reportreasons".into(), c.reportreasons.len()),
        ("announcements".into(), c.announcements.len()),
        ("calendars".into(), c.calendars.len()),
    ];
    crate::admin::page(
        &ctx,
        "admin/cache.html",
        "tools",
        "Cache Manager",
        minijinja::context! { parts => parts, parser_rev => c.parser_rev },
    )
    .await
}

pub async fn cache_reload(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "tools");
    let part = s(f.fields.get("part"));
    if part == "all" {
        ctx.app.invalidate(crate::cache::PARTS).await?;
        ctx.app.stats_cache.invalidate_all();
        ctx.app.mod_counts.invalidate_all();
    } else if crate::cache::PARTS.contains(&part.as_str()) {
        ctx.app.invalidate(&[part.as_str()]).await?;
    }
    Ok(ctx.redirect(
        "/admin/tools/cache",
        "The cache has been reloaded on all nodes.",
    ))
}

#[derive(Deserialize, Default)]
pub struct PageQ {
    pub page: Option<i64>,
}

pub async fn adminlog(ctx: Ctx, Query(q): Query<PageQ>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "logs");
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM adminlog")
        .fetch_one(&ctx.app.db)
        .await?;
    let pg = util::paginate(
        total,
        50,
        util::clamp_page(q.page),
        "/admin/tools/adminlog?page={page}",
    );
    let rows: Vec<(i64, String, String, String, i64, serde_json::Value, Option<String>)> = sqlx::query_as(
        "SELECT l.id, l.module, l.action, l.ipaddress, l.dateline, l.data, u.username FROM adminlog l LEFT JOIN users u ON u.uid = l.uid ORDER BY l.id DESC LIMIT 50 OFFSET $1",
    )
    .bind((pg.page - 1) * 50)
    .fetch_all(&ctx.app.db)
    .await?;
    let rows: Vec<_> = rows
        .into_iter()
        .map(|r| (r.0, r.1, r.2, r.3, r.4, r.5.to_string(), r.6))
        .collect();
    crate::admin::page(
        &ctx,
        "admin/adminlog.html",
        "tools",
        "Admin Log",
        minijinja::context! { rows => rows, pagination => pg },
    )
    .await
}

/// Who really wrote what was published as the System account.
pub async fn systemlog(ctx: Ctx, Query(q): Query<PageQ>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "logs");
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM system_authorship")
        .fetch_one(&ctx.app.db)
        .await?;
    let pg = util::paginate(
        total,
        50,
        util::clamp_page(q.page),
        "/admin/tools/systemlog?page={page}",
    );
    let rows: Vec<(String, i32, i32, String, String, i64, String)> = sqlx::query_as(
        "SELECT kind, ref_id, actor, actor_name, ipaddress, dateline, summary FROM system_authorship ORDER BY id DESC LIMIT 50 OFFSET $1",
    )
    .bind((pg.page - 1) * 50)
    .fetch_all(&ctx.app.db)
    .await?;
    let rows: Vec<_> = rows
        .into_iter()
        .map(|(kind, id, actor, name, ip, dl, summary)| {
            let link = match kind.as_str() {
                "thread" => Some(format!("/thread/{id}")),
                "post" => Some(format!("/post/{id}")),
                "announcement" => Some(format!("/announcement/{id}")),
                _ => None,
            };
            minijinja::context! { kind => kind, id => id, link => link, actor => actor, actor_name => name, ip => ip, dateline => dl, summary => summary }
        })
        .collect();
    crate::admin::page(
        &ctx,
        "admin/systemlog.html",
        "tools",
        "System Log",
        minijinja::context! { rows => rows, pagination => pg },
    )
    .await
}

/// Erasure requests carried out (no personal data is kept about the erased member).
pub async fn erasurelog(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "logs");
    let rows: Vec<(i32, i32, i64, bool, String, Option<String>)> = sqlx::query_as(
        "SELECT e.id, e.former_uid, e.dateline, e.kept_posts, e.reference, u.username FROM erasure_log e LEFT JOIN users u ON u.uid = e.performed_by ORDER BY e.id DESC LIMIT 500",
    )
    .fetch_all(&ctx.app.db)
    .await?;
    crate::admin::page(
        &ctx,
        "admin/erasurelog.html",
        "tools",
        "Erasure Log",
        minijinja::context! { rows => rows },
    )
    .await
}

pub async fn maillogs(ctx: Ctx, Query(q): Query<PageQ>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "logs");
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM maillogs")
        .fetch_one(&ctx.app.db)
        .await?;
    let pg = util::paginate(
        total,
        50,
        util::clamp_page(q.page),
        "/admin/tools/maillogs?page={page}",
    );
    let rows: Vec<(i64, String, i64, String, String, String, i16)> = sqlx::query_as(
        "SELECT mid, subject, dateline, fromemail, toemail, ipaddress, type FROM maillogs ORDER BY mid DESC LIMIT 50 OFFSET $1",
    )
    .bind((pg.page - 1) * 50)
    .fetch_all(&ctx.app.db)
    .await?;
    crate::admin::page(
        &ctx,
        "admin/maillogs.html",
        "tools",
        "Mail Logs",
        minijinja::context! { rows => rows, pagination => pg },
    )
    .await
}

pub async fn mailerrors(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "logs");
    let rows: Vec<(i64, String, String, i64, i32, String)> =
        sqlx::query_as("SELECT mid, mailto, subject, dateline, attempts, lasterror FROM mailqueue ORDER BY mid DESC LIMIT 200").fetch_all(&ctx.app.db).await?;
    crate::admin::page(
        &ctx,
        "admin/mailerrors.html",
        "tools",
        "Mail Queue",
        minijinja::context! { rows => rows, handler => ctx.settings().get("mail_handler") },
    )
    .await
}

pub async fn mailerrors_action(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "logs");
    match s(f.fields.get("action")).as_str() {
        "retry" => {
            sqlx::query("UPDATE mailqueue SET attempts = 0")
                .execute(&ctx.app.db)
                .await?;
        }
        "clear" => {
            sqlx::query("DELETE FROM mailqueue WHERE attempts >= 5")
                .execute(&ctx.app.db)
                .await?;
        }
        _ => {}
    }
    Ok(ctx.redirect(
        "/admin/tools/mailerrors",
        "The mail queue has been updated.",
    ))
}

pub async fn testmail(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "tools");
    let to = s(f.fields.get("to"));
    let to = if to.trim().is_empty() {
        ctx.me()?.email.clone()
    } else {
        to.trim().to_string()
    };
    crate::mail::queue(
        &ctx.app,
        &to,
        "rbb test email",
        "This is a test email sent from the rbb Admin CP. If you received it, outgoing mail works.",
    )
    .await;
    Ok(ctx.redirect(
        "/admin/tools/mailerrors",
        &format!("A test email has been queued for {to}."),
    ))
}

pub async fn spamlog(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "logs");
    let rows: Vec<(String, String, String, i64, String)> =
        sqlx::query_as("SELECT username, email, ipaddress, dateline, data FROM spamlog ORDER BY sid DESC LIMIT 200").fetch_all(&ctx.app.db).await?;
    crate::admin::page(
        &ctx,
        "admin/spamlog.html",
        "tools",
        "Spam Log",
        minijinja::context! { rows => rows },
    )
    .await
}

pub async fn stats(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "tools");
    let rows: Vec<(i64, i32, i32, i32)> = sqlx::query_as("SELECT dateline, numusers, numthreads, numposts FROM stats ORDER BY dateline DESC LIMIT 90").fetch_all(&ctx.app.db).await?;
    let top_forums: Vec<(String, i32, i32)> = sqlx::query_as(
        "SELECT name, threads, posts FROM forums WHERE type = 'f' ORDER BY posts DESC LIMIT 10",
    )
    .fetch_all(&ctx.app.db)
    .await?;
    let activity: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT (dateline / 3600 % 24) AS hour, COUNT(*) FROM posts WHERE dateline > $1 GROUP BY hour ORDER BY hour",
    )
    .bind(now() - 30 * 86400)
    .fetch_all(&ctx.app.db)
    .await?;
    let max_act = activity.iter().map(|a| a.1).max().unwrap_or(1).max(1);
    crate::admin::page(&ctx, "admin/stats.html", "tools", "Statistics", minijinja::context! { rows => rows, top_forums => top_forums, activity => activity, max_act => max_act }).await
}

fn pg_dump_path() -> Option<String> {
    if let Ok(p) = std::env::var("RBB_PG_DUMP") {
        return Some(p);
    }
    for p in [
        "/opt/homebrew/opt/postgresql@17/bin/pg_dump",
        "/usr/bin/pg_dump",
        "/usr/local/bin/pg_dump",
        "/usr/lib/postgresql/17/bin/pg_dump",
        "/usr/lib/postgresql/16/bin/pg_dump",
    ] {
        if std::path::Path::new(p).exists() {
            return Some(p.to_string());
        }
    }
    None
}

pub async fn backup_page(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "tools");
    crate::admin::page(
        &ctx,
        "admin/backup.html",
        "tools",
        "Database Backup",
        minijinja::context! { available => pg_dump_path().is_some() },
    )
    .await
}

/// Stream a `pg_dump` of the database (custom format) to the browser.
pub async fn backup(ctx: Ctx, CsrfForm(_): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "tools");
    let bin = pg_dump_path()
        .ok_or_else(|| AppError::user("pg_dump was not found. Set RBB_PG_DUMP to its path."))?;
    let mut child = tokio::process::Command::new(bin)
        .arg("--format=custom")
        .arg("--no-owner")
        .arg(&ctx.app.cfg.database_url)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| AppError::Other(e.into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Other(anyhow::anyhow!("no stdout")))?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    crate::admin::log(
        &ctx,
        "tools",
        "Downloaded database backup",
        serde_json::json!({}),
    )
    .await;
    let stream = tokio_util_reader(stdout);
    let name = format!(
        "rbb-backup-{}.dump",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    );
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

fn tokio_util_reader(
    r: tokio::process::ChildStdout,
) -> impl futures::Stream<Item = std::io::Result<bytes::Bytes>> {
    use tokio::io::AsyncReadExt;
    async_stream::try_stream! {
        let mut r = r;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = r.read(&mut buf).await?;
            if n == 0 { break; }
            yield bytes::Bytes::copy_from_slice(&buf[..n]);
        }
    }
}

pub async fn plugins(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "tools");
    let list: Vec<_> = ctx.app.plugins.list.iter().map(|p| minijinja::context! { name => &p.name, file => &p.file, hooks => &p.hooks, info => p.info.to_string() }).collect();
    crate::admin::page(
        &ctx,
        "admin/plugins.html",
        "tools",
        "Plugins",
        minijinja::context! { list => list, dir => &ctx.app.cfg.plugins_dir },
    )
    .await
}

pub async fn attachments(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "tools");
    let stats: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(filesize), 0)::bigint, COUNT(*) FILTER (WHERE pid = 0 AND dateuploaded < $1), COALESCE(SUM(downloads), 0)::bigint FROM attachments",
    )
    .bind(now() - 86400)
    .fetch_one(&ctx.app.db)
    .await?;
    let biggest: Vec<(i32, String, i64, i32, i32, Option<String>)> = sqlx::query_as(
        "SELECT a.aid, a.filename, a.filesize, a.downloads, a.pid, u.username FROM attachments a LEFT JOIN users u ON u.uid = a.uid ORDER BY a.filesize DESC LIMIT 25",
    )
    .fetch_all(&ctx.app.db)
    .await?;
    crate::admin::page(
        &ctx,
        "admin/attachments.html",
        "tools",
        "Attachments",
        minijinja::context! { stats => stats, biggest => biggest },
    )
    .await
}

pub async fn attachments_cleanup(ctx: Ctx, CsrfForm(_): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "tools");
    let files: Vec<(String, String)> =
        sqlx::query_as("DELETE FROM attachments WHERE pid = 0 AND dateuploaded < $1 RETURNING attachname, thumbnail").bind(now() - 86400).fetch_all(&ctx.app.db).await?;
    let n = files.len();
    for (a, t) in files {
        let _ = tokio::fs::remove_file(format!("{}/{a}", ctx.app.cfg.upload_dir)).await;
        if !t.is_empty() {
            let _ = tokio::fs::remove_file(format!("{}/{t}", ctx.app.cfg.upload_dir)).await;
        }
    }
    Ok(ctx.redirect(
        "/admin/tools/attachments",
        &format!("Removed {n} orphaned attachments."),
    ))
}
