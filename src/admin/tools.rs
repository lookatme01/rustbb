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
                            sqlx::query("UPDATE users u SET reputation = COALESCE(r.total, 0) FROM users u2 LEFT JOIN (SELECT uid, SUM(reputation)::int AS total FROM reputation GROUP BY uid) r ON r.uid = u2.uid WHERE u.uid = u2.uid").execute(&app.db).await?;
                        }
                        "pms" => {
                            sqlx::query(
                                "UPDATE users u SET totalpms = COALESCE(p.total, 0), unreadpms = COALESCE(p.unread, 0)
                                 FROM users u2 LEFT JOIN (
                                     SELECT uid, COUNT(*)::int AS total,
                                            COUNT(*) FILTER (WHERE status = 0 AND folder NOT IN (2,3))::int AS unread
                                     FROM privatemessages GROUP BY uid
                                 ) p ON p.uid = u2.uid WHERE u.uid = u2.uid",
                            )
                            .execute(&app.db)
                            .await?;
                        }
                        "attachments" => {
                            sqlx::query("UPDATE threads t SET attachmentcount = COALESCE(a.n, 0) FROM threads t2 LEFT JOIN (SELECT p.tid, COUNT(*)::int AS n FROM attachments a JOIN posts p ON p.pid = a.pid WHERE p.visible = 1 AND a.visible GROUP BY p.tid) a ON a.tid = t2.tid WHERE t.tid = t2.tid").execute(&app.db).await?;
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
        "SELECT l.id, l.module, l.action, COALESCE(host(l.ipaddress), ''), l.dateline, l.data, u.username FROM adminlog l LEFT JOIN users u ON u.uid = l.uid ORDER BY l.id DESC LIMIT 50 OFFSET $1",
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
        "SELECT kind, ref_id, actor, actor_name, COALESCE(host(ipaddress), ''), dateline, summary FROM system_authorship ORDER BY id DESC LIMIT 50 OFFSET $1",
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
        "SELECT mid, subject, dateline, fromemail, toemail, COALESCE(host(ipaddress), ''), type FROM maillogs ORDER BY mid DESC LIMIT 50 OFFSET $1",
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
            // Messages a worker is sending right now keep their lease.
            sqlx::query("UPDATE mailqueue SET attempts = 0, status = 'pending', available_at = now(), locked_until = NULL, lease = NULL
                         WHERE locked_until IS NULL OR locked_until < now()")
                .execute(&ctx.app.db)
                .await?;
        }
        "clear" => {
            sqlx::query("DELETE FROM mailqueue WHERE status = 'dead'")
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
    if !util::valid_email(&to) {
        return Err(AppError::user("Enter a valid email address."));
    }
    let st = ctx.settings();
    let smtp = st.get("mail_handler") == "smtp";
    if smtp && st.get("smtp_host").trim().is_empty() {
        return Err(AppError::user(
            "Mail is set to SMTP but no SMTP host is configured in Settings.",
        ));
    }
    // `mail::queue` only logs a failure; call `deliver` so the admin hears about it.
    crate::mail::deliver(
        &ctx.app,
        None,
        &to,
        "rbb test email",
        "This is a test email sent from the rbb Admin CP. If you received it, outgoing mail works.",
    )
    .await?;
    let msg = if smtp {
        format!(
            "A test email has been queued for {to}. If it does not arrive, check the failures below."
        )
    } else {
        format!(
            "A test email for {to} has been queued, but mail is not set to SMTP: it will be written to the server log, not sent."
        )
    };
    Ok(ctx.redirect("/admin/tools/mailerrors", &msg))
}

pub async fn spamlog(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "logs");
    let rows: Vec<(String, String, String, i64, String)> =
        sqlx::query_as("SELECT username, email, COALESCE(host(ipaddress), ''), dateline, data FROM spamlog ORDER BY sid DESC LIMIT 200").fetch_all(&ctx.app.db).await?;
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

/// The connection URL without its password, plus the password for `PGPASSWORD`, so the secret
/// is not visible in the process list. An unparsable URL is passed through as it is.
fn split_db_password(url: &str) -> (String, Option<String>) {
    let Ok(mut u) = url::Url::parse(url) else {
        return (url.to_string(), None);
    };
    let Some(pw) = u.password() else {
        return (url.to_string(), None);
    };
    let pw = percent_encoding::percent_decode_str(pw)
        .decode_utf8_lossy()
        .into_owned();
    let _ = u.set_password(None);
    (u.to_string(), Some(pw))
}

/// Stream a `pg_dump` of the database (custom format) to the browser.
///
/// The status line is sent before the dump ends, so a failure part-way aborts the connection
/// (the browser sees a failed download, not a short file that looks complete) and is logged.
pub async fn backup(ctx: Ctx, CsrfForm(_): CsrfForm<AnyForm>) -> AppResult<Response> {
    use tokio::io::AsyncReadExt;
    crate::admin::acp_guard!(ctx, "tools");
    let bin = pg_dump_path()
        .ok_or_else(|| AppError::user("pg_dump was not found. Set RBB_PG_DUMP to its path."))?;
    let (url, password) = split_db_password(&ctx.app.cfg.database_url);
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("--format=custom")
        .arg("--no-owner")
        .arg(url)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(pw) = password {
        cmd.env("PGPASSWORD", pw);
    }
    let mut child = cmd.spawn().map_err(|e| AppError::Other(e.into()))?;
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(AppError::Other(anyhow::anyhow!("pg_dump pipes missing")));
    };
    // Drained concurrently so a chatty pg_dump can never block on a full stderr pipe.
    let errs = tokio::spawn(async move {
        let mut b = Vec::new();
        let _ = stderr.read_to_end(&mut b).await;
        String::from_utf8_lossy(&b).into_owned()
    });
    // pg_dump writes its header as soon as it is connected; no output at all means it failed
    // (bad credentials, unreachable database), which can still be reported as an error page.
    let mut first = vec![0u8; 64 * 1024];
    let n = stdout
        .read(&mut first)
        .await
        .map_err(|e| AppError::Other(e.into()))?;
    if n == 0 {
        let status = child.wait().await.map_err(|e| AppError::Other(e.into()))?;
        let msg = errs.await.unwrap_or_default();
        tracing::error!("pg_dump produced no output ({status}): {}", msg.trim());
        return Err(AppError::Other(anyhow::anyhow!(
            "pg_dump failed ({status}); see the server log"
        )));
    }
    first.truncate(n);
    crate::admin::log(
        &ctx,
        "tools",
        "Downloaded database backup",
        serde_json::json!({}),
    )
    .await;
    let stream = dump_stream(child, stdout, bytes::Bytes::from(first), errs);
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

/// `first` (already read), then the rest of `stdout`; ends in an error when `pg_dump` exits
/// non-zero, which aborts the response instead of completing it.
fn dump_stream(
    mut child: tokio::process::Child,
    mut stdout: tokio::process::ChildStdout,
    first: bytes::Bytes,
    errs: tokio::task::JoinHandle<String>,
) -> impl futures::Stream<Item = std::io::Result<bytes::Bytes>> {
    use tokio::io::AsyncReadExt;
    async_stream::try_stream! {
        yield first;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = stdout.read(&mut buf).await?;
            if n == 0 { break; }
            yield bytes::Bytes::copy_from_slice(&buf[..n]);
        }
        let status = child.wait().await?;
        if !status.success() {
            let msg = errs.await.unwrap_or_default();
            tracing::error!("pg_dump failed part-way ({status}); the download was cut off: {}", msg.trim());
            Err::<(), _>(std::io::Error::other(format!("pg_dump exited with {status}")))?;
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
    let n = crate::ops::prune_orphaned_attachments(&ctx.app).await?;
    Ok(ctx.redirect(
        "/admin/tools/attachments",
        &format!("Removed {n} orphaned attachments."),
    ))
}
