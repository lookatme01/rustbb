//! Lite ("archive") mode: minimal, fast, markup-light pages for low-bandwidth clients and crawlers.

use crate::ctx::Ctx;
use crate::error::{AppError, AppResult};
use crate::models::{POST_COLUMNS, Post};
use crate::util;
use axum::extract::{Path, Query};
use axum::response::Response;
use serde::Deserialize;

fn enabled(ctx: &Ctx) -> AppResult<()> {
    if !ctx.settings().bool("enablearchive") {
        return Err(AppError::not_found("page"));
    }
    Ok(())
}

pub async fn index(ctx: Ctx) -> AppResult<Response> {
    enabled(&ctx)?;
    let forums: Vec<_> = ctx
        .cache
        .forums
        .iter()
        .filter(|f| ctx.access().can_see(f.fid))
        .map(|f| minijinja::context! { fid => f.fid, name => &f.name, depth => ctx.cache.forum_depth.get(&f.fid).copied().unwrap_or(0), category => f.is_category() })
        .collect();
    ctx.allow_guest_cache(&["board".to_string()]);
    ctx.render(
        "archive.html",
        minijinja::context! { mode => "index", forums => forums, full_url => "/" },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct PageQuery {
    pub page: Option<i64>,
}

pub async fn forum(
    ctx: Ctx,
    Path(fid): Path<i32>,
    Query(q): Query<PageQuery>,
) -> AppResult<Response> {
    enabled(&ctx)?;
    let (forum, fp) = ctx.check_forum(fid)?;
    let forum = forum.clone();
    if !fp.canviewthreads {
        return Err(AppError::no_perm());
    }
    // Use resolved access: guests cannot have "own" threads, and moderators see all.
    let own_only = ctx
        .access()
        .forum(fid)
        .is_ok_and(|a| a.threads == crate::domain::access::Threads::Own);
    if own_only && ctx.uid() == 0 {
        return Err(AppError::no_perm());
    }
    let per = 50;
    let total: i64 = if own_only {
        sqlx::query_scalar("SELECT COUNT(*) FROM threads WHERE fid = $1 AND uid = $2 AND visible = 1 AND closed NOT LIKE 'moved|%'")
            .bind(fid).bind(ctx.uid()).fetch_one(&ctx.app.db).await?
    } else {
        sqlx::query_scalar("SELECT threads FROM forums WHERE fid = $1")
            .bind(fid)
            .fetch_one(&ctx.app.db)
            .await
            .map(|t: i32| t as i64)?
    };
    let pg = util::paginate(
        total,
        per,
        util::clamp_page(q.page),
        &format!("/archive/forum/{fid}?page={{page}}"),
    );
    let own = if own_only { ctx.uid() } else { 0 };
    let threads: Vec<(i32, String, i32, bool)> = sqlx::query_as(
        "SELECT tid, subject, replies, sticky FROM threads WHERE fid = $1 AND visible = 1 AND closed NOT LIKE 'moved|%' AND ($4 = 0 OR uid = $4) ORDER BY sticky DESC, lastpost DESC LIMIT $2 OFFSET $3",
    )
    .bind(fid)
    .bind(per)
    .bind((pg.page - 1) * per)
    .bind(own)
    .fetch_all(&ctx.app.db)
    .await?;
    let subforums: Vec<(i32, String)> = ctx
        .cache
        .children(fid)
        .filter(|f| ctx.access().can_see(f.fid))
        .map(|f| (f.fid, f.name.clone()))
        .collect();
    ctx.allow_guest_cache(&[format!("forum:{fid}")]);
    ctx.render(
        "archive.html",
        minijinja::context! { mode => "forum", forum => &forum, threads => threads, subforums => subforums, pagination => pg, full_url => crate::templates::url_forum(fid as i64, Some(&forum.name)) },
    )
    .await
}

pub async fn thread(
    ctx: Ctx,
    Path(tid): Path<i32>,
    Query(q): Query<PageQuery>,
) -> AppResult<Response> {
    enabled(&ctx)?;
    let (t, forum, _) = crate::routes::showthread::check_thread(&ctx, tid).await?;
    let per = 30;
    let total = t.replies as i64 + 1;
    let pg = util::paginate(
        total,
        per,
        util::clamp_page(q.page),
        &format!("/archive/thread/{tid}?page={{page}}"),
    );
    let posts: Vec<Post> = sqlx::query_as(&format!("SELECT {POST_COLUMNS} FROM posts WHERE tid = $1 AND visible = 1 ORDER BY dateline, pid LIMIT $2 OFFSET $3"))
        .bind(tid)
        .bind(per)
        .bind((pg.page - 1) * per)
        .fetch_all(&ctx.app.db)
        .await?;
    let mut stale = vec![];
    let list: Vec<_> = posts.iter().map(|p| minijinja::context! { username => &p.username, dateline => p.dateline, html => crate::render::post_html(&ctx, p, &mut stale) }).collect();
    crate::render::store_parsed(&ctx, stale);
    *ctx.app.thread_views.entry(tid).or_insert(0) += 1;
    ctx.allow_guest_cache(&[format!("thread:{tid}")]);
    ctx.render(
        "archive.html",
        minijinja::context! { mode => "thread", thread => &t, forum => &forum, posts => list, pagination => pg, full_url => crate::templates::url_thread(tid as i64, Some(&t.subject)) },
    )
    .await
}
