//! Portal page: announcements from selected forums plus sidebar boxes.

use crate::ctx::Ctx;
use crate::error::{AppError, AppResult};
use crate::models::{POST_COLUMNS, Post, Thread};
use crate::templates::url_thread;
use axum::response::Response;
use std::collections::HashMap;

pub async fn portal(ctx: Ctx) -> AppResult<Response> {
    let s = ctx.settings().clone();
    if !s.bool("portal") {
        return Err(AppError::not_found("page"));
    }
    let (mut fids, _) = crate::routes::search::readable_forums(&ctx);
    let ann_fids: Vec<i32> = s
        .get("portal_announcementsfid")
        .split(',')
        .filter_map(|x| x.trim().parse().ok())
        .collect();
    let ann_forums: Vec<i32> = if ann_fids.is_empty() {
        fids.clone()
    } else {
        fids.iter()
            .copied()
            .filter(|f| ann_fids.contains(f))
            .collect()
    };
    let threads: Vec<Thread> = sqlx::query_as("SELECT * FROM threads WHERE fid = ANY($1) AND visible = 1 AND closed NOT LIKE 'moved|%' ORDER BY dateline DESC LIMIT $2")
        .bind(&ann_forums)
        .bind(s.int("portal_numannouncements").max(1))
        .fetch_all(&ctx.app.db)
        .await?;
    let pids: Vec<i32> = threads.iter().map(|t| t.firstpost).collect();
    let posts: HashMap<i32, Post> = sqlx::query_as::<_, Post>(&format!(
        "SELECT {POST_COLUMNS} FROM posts WHERE pid = ANY($1)"
    ))
    .bind(&pids)
    .fetch_all(&ctx.app.db)
    .await?
    .into_iter()
    .map(|p| (p.pid, p))
    .collect();
    let authors =
        crate::render::load_authors(&ctx, &threads.iter().map(|t| t.uid).collect::<Vec<_>>())
            .await?;
    let mut stale = vec![];
    let announcements: Vec<_> = threads
        .iter()
        .filter_map(|t| {
            let p = posts.get(&t.firstpost)?;
            let html = crate::render::post_html(&ctx, p, &mut stale);
            Some(minijinja::context! { tid => t.tid, subject => &t.subject, url => url_thread(t.tid as i64, Some(&t.subject)), dateline => t.dateline, replies => t.replies, views => t.views,
                uid => t.uid, username => &t.username, author => authors.get(&t.uid), html => html, forum => ctx.cache.forum(t.fid).map(|f| f.name.clone()) })
        })
        .collect();
    crate::render::store_parsed(&ctx, stale);
    fids.retain(|_| true);
    let latest: Vec<(i32, String, i64, String, i32, i32)> = if s.bool("portal_showdiscussions") {
        sqlx::query_as("SELECT tid, subject, lastpost, lastposter, lastposteruid, replies FROM threads WHERE fid = ANY($1) AND visible = 1 AND closed NOT LIKE 'moved|%' ORDER BY lastpost DESC LIMIT $2")
            .bind(&fids)
            .bind(s.int("portal_showdiscussionsnum").max(1))
            .fetch_all(&ctx.app.db)
            .await?
    } else {
        vec![]
    };
    let online = if s.bool("portal_showwol") && ctx.perms.canviewonline {
        Some(crate::routes::index::online_summary(&ctx).await?)
    } else {
        None
    };
    let stats = if s.bool("portal_showstats") {
        Some(crate::routes::index::board_stats(&ctx).await?)
    } else {
        None
    };
    ctx.allow_guest_cache(&["board".to_string()]);
    ctx.render(
        "portal.html",
        minijinja::context! { title => "Portal", announcements => announcements, latest => latest, online => online, stats => stats },
    )
    .await
}
