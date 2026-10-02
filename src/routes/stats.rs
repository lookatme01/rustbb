//! Forum statistics page.

use crate::ctx::Ctx;
use crate::error::{AppError, AppResult};
use crate::templates::url_thread;
use crate::util::now;
use axum::response::Response;

pub async fn stats(ctx: Ctx) -> AppResult<Response> {
    let s = ctx.settings();
    if !s.bool("statsenabled") {
        return Err(AppError::not_found("page"));
    }
    let n = s.int("statstopcount").max(1);
    let cache_key = "statspage";
    let base = crate::routes::index::board_stats(&ctx).await?;
    let (fids, _) = crate::routes::search::searchable_forums(&ctx);
    let data = if let Some(v) = ctx
        .app
        .stats_cache
        .get(&cache_key)
        .filter(|v| v["fids"] == serde_json::json!(fids))
    {
        v
    } else {
        let top_posters: Vec<(i32, String, i32, i32, i32)> =
            sqlx::query_as("SELECT uid, username, usergroup, displaygroup, postnum FROM users ORDER BY postnum DESC LIMIT $1").bind(n).fetch_all(&ctx.app.db).await?;
        let top_replied: Vec<(i32, String, i32)> = sqlx::query_as(
            "SELECT tid, subject, replies FROM threads WHERE fid = ANY($1) AND visible = 1 AND closed NOT LIKE 'moved|%' ORDER BY replies DESC LIMIT $2",
        )
        .bind(&fids)
        .bind(n)
        .fetch_all(&ctx.app.db)
        .await?;
        let top_viewed: Vec<(i32, String, i32)> = sqlx::query_as(
            "SELECT tid, subject, views FROM threads WHERE fid = ANY($1) AND visible = 1 AND closed NOT LIKE 'moved|%' ORDER BY views DESC LIMIT $2",
        )
        .bind(&fids)
        .bind(n)
        .fetch_all(&ctx.app.db)
        .await?;
        let top_forums: Vec<(i32, String, i32)> = sqlx::query_as(
            "SELECT fid, name, posts FROM forums WHERE fid = ANY($1) ORDER BY posts DESC LIMIT $2",
        )
        .bind(&fids)
        .bind(n)
        .fetch_all(&ctx.app.db)
        .await?;
        let today: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM posts WHERE dateline > $1 AND visible = 1), (SELECT COUNT(*) FROM users WHERE regdate > $1)",
        )
        .bind(now() - 86400)
        .fetch_one(&ctx.app.db)
        .await?;
        let first_reg: Option<i64> = sqlx::query_scalar("SELECT MIN(regdate) FROM users")
            .fetch_one(&ctx.app.db)
            .await?;
        let top_referrer: Option<(i32, String, i32)> = sqlx::query_as("SELECT uid, username, referrals FROM users WHERE referrals > 0 ORDER BY referrals DESC LIMIT 1").fetch_optional(&ctx.app.db).await?;
        let with_posts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE postnum > 0")
            .fetch_one(&ctx.app.db)
            .await?;
        let history: Vec<(i64, i32, i32, i32)> = sqlx::query_as("SELECT dateline, numusers, numthreads, numposts FROM stats ORDER BY dateline DESC LIMIT 30").fetch_all(&ctx.app.db).await?;
        let v = serde_json::json!({
            "fids": fids,
            "top_posters": top_posters.iter().map(|r| serde_json::json!({"uid": r.0, "formatted": ctx.cache.format_name(&r.1, r.2, r.3), "postnum": r.4})).collect::<Vec<_>>(),
            "top_replied": top_replied.iter().map(|r| serde_json::json!({"url": url_thread(r.0 as i64, Some(&r.1)), "subject": r.1, "count": r.2})).collect::<Vec<_>>(),
            "top_viewed": top_viewed.iter().map(|r| serde_json::json!({"url": url_thread(r.0 as i64, Some(&r.1)), "subject": r.1, "count": r.2})).collect::<Vec<_>>(),
            "top_forums": top_forums.iter().map(|r| serde_json::json!({"url": crate::templates::url_forum(r.0 as i64, Some(&r.1)), "name": r.1, "count": r.2})).collect::<Vec<_>>(),
            "posts_today": today.0, "users_today": today.1, "first_reg": first_reg.unwrap_or(now()),
            "top_referrer": top_referrer.map(|r| serde_json::json!({"uid": r.0, "username": r.1, "referrals": r.2})),
            "with_posts": with_posts,
            "history": history.iter().rev().map(|h| serde_json::json!({"dateline": h.0, "users": h.1, "threads": h.2, "posts": h.3})).collect::<Vec<_>>(),
        });
        ctx.app.stats_cache.insert(cache_key, v.clone());
        v
    };
    let days = ((now() - data["first_reg"].as_i64().unwrap_or(now())) as f64 / 86400.0).max(1.0);
    let users = base["users"].as_f64().unwrap_or(1.0).max(1.0);
    ctx.allow_guest_cache(&["board".to_string()]);
    ctx.render(
        "stats.html",
        minijinja::context! {
            title => "Forum Statistics", base => base.clone(), data => data.clone(),
            posts_per_day => format!("{:.2}", base["posts"].as_f64().unwrap_or(0.0) / days),
            threads_per_day => format!("{:.2}", base["threads"].as_f64().unwrap_or(0.0) / days),
            members_per_day => format!("{:.2}", users / days),
            posts_per_member => format!("{:.2}", base["posts"].as_f64().unwrap_or(0.0) / users),
            replies_per_thread => format!("{:.2}", (base["posts"].as_f64().unwrap_or(0.0) - base["threads"].as_f64().unwrap_or(0.0)) / base["threads"].as_f64().unwrap_or(1.0).max(1.0)),
            percent_posted => format!("{:.1}", data["with_posts"].as_f64().unwrap_or(0.0) * 100.0 / users),
        },
    )
    .await
}
