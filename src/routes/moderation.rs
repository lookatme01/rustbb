//! Moderation: inline thread/post tools, move/merge/split/copy, custom moderator tools and
//! delayed (scheduled) moderation.

use crate::app::App;
use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::infra::outbox::Job;
use crate::models::Thread;
use crate::ops;
use crate::perms::ModPerms;
use crate::templates::{url_forum, url_thread};
use crate::usecase::Uow;
use crate::util::now;
use axum::Router;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::post;
use serde::Deserialize;

pub fn router() -> Router<App> {
    Router::new()
        .route("/moderation/threads", post(inline_threads))
        .route("/moderation/posts", post(inline_posts))
        .route("/moderation/thread/{tid}", post(thread_tool))
        .route("/moderation/move", post(do_move))
        .route("/moderation/merge", post(do_merge))
        .route("/moderation/split", post(do_split))
        .route("/moderation/moveposts", post(do_moveposts))
        .route("/moderation/editthread", post(do_editthread))
        .route("/moderation/delayed", post(schedule_delayed))
        .route("/moderation/delayed/cancel", post(cancel_delayed))
}

#[derive(Deserialize, Default)]
pub struct InlineForm {
    #[serde(default, deserialize_with = "de::string")]
    pub action: String,
    #[serde(default, deserialize_with = "de::vec_i32")]
    pub tids: Vec<i32>,
    #[serde(default, deserialize_with = "de::vec_i32")]
    pub pids: Vec<i32>,
    #[serde(default, deserialize_with = "de::i32")]
    pub fid: i32,
    #[serde(default, deserialize_with = "de::i32")]
    pub tid: i32,
}

async fn load_threads(ctx: &Ctx, tids: &[i32]) -> AppResult<Vec<Thread>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {} FROM threads WHERE tid = ANY($1)",
        crate::models::THREAD_COLUMNS
    ))
    .bind(tids)
    .fetch_all(&ctx.app.db)
    .await?)
}

/// Ensure the moderator has `check` permission in every thread's forum.
fn require(ctx: &Ctx, threads: &[Thread], check: impl Fn(&ModPerms) -> bool) -> AppResult<()> {
    if threads.is_empty() {
        return Err(AppError::user("Please select at least one thread."));
    }
    for t in threads {
        match ctx.mod_perms(t.fid) {
            Some(m) if check(&m) => {}
            _ => return Err(AppError::no_perm()),
        }
    }
    Ok(())
}

/// Record a moderator action in the unit of work that performs it.
async fn log(
    uow: &mut Uow,
    ctx: &Ctx,
    t: &Thread,
    pid: i32,
    action: &str,
    data: serde_json::Value,
) -> AppResult<()> {
    ops::log_moderator_action_in(
        uow.conn(),
        ctx.uid(),
        &ctx.ip,
        t.fid,
        t.tid,
        pid,
        action,
        data,
    )
    .await
}

/// Apply a simple thread action to many threads. Returns a message.
async fn thread_action(ctx: &Ctx, threads: &[Thread], action: &str) -> AppResult<String> {
    let tids: Vec<i32> = threads.iter().map(|t| t.tid).collect();
    let mut uow = Uow::begin(&ctx.app).await?;
    ops::lock_threads(uow.conn(), &tids).await?;
    let msg = match action {
        "close" | "open" => {
            require(ctx, threads, |m| m.canopenclosethreads)?;
            sqlx::query(
                "UPDATE threads SET closed = $2 WHERE tid = ANY($1) AND closed NOT LIKE 'moved|%'",
            )
            .bind(&tids)
            .bind(if action == "close" { "1" } else { "" })
            .execute(uow.conn())
            .await?;
            if action == "close" {
                "The threads have been closed."
            } else {
                "The threads have been opened."
            }
        }
        "stick" | "unstick" => {
            require(ctx, threads, |m| m.canstickunstickthreads)?;
            sqlx::query("UPDATE threads SET sticky = $2 WHERE tid = ANY($1)")
                .bind(&tids)
                .bind(action == "stick")
                .execute(uow.conn())
                .await?;
            if action == "stick" {
                "The threads are now sticky."
            } else {
                "The threads are no longer sticky."
            }
        }
        "approve" | "unapprove" => {
            require(ctx, threads, |m| m.canapproveunapprovethreads)?;
            ops::set_threads_visibility_in(
                &mut uow,
                &tids,
                if action == "approve" { 1 } else { 0 },
            )
            .await?;
            if action == "approve" {
                "The threads have been approved."
            } else {
                "The threads have been unapproved."
            }
        }
        "softdelete" => {
            require(ctx, threads, |m| m.cansoftdeletethreads)?;
            ops::set_threads_visibility_in(&mut uow, &tids, -1).await?;
            "The threads have been soft deleted."
        }
        "restore" => {
            require(ctx, threads, |m| m.canrestorethreads)?;
            ops::set_threads_visibility_in(&mut uow, &tids, 1).await?;
            "The threads have been restored."
        }
        "delete" => {
            require(ctx, threads, |m| m.candeletethreads)?;
            ops::delete_threads_in(&mut uow, &tids).await?;
            "The threads have been permanently deleted."
        }
        "deletepoll" => {
            require(ctx, threads, |m| m.canmanagepolls)?;
            sqlx::query("DELETE FROM polls WHERE tid = ANY($1)")
                .bind(&tids)
                .execute(uow.conn())
                .await?;
            sqlx::query("UPDATE threads SET poll = 0 WHERE tid = ANY($1)")
                .bind(&tids)
                .execute(uow.conn())
                .await?;
            "The poll has been deleted."
        }
        "removeredirects" => {
            require(ctx, threads, |m| m.canmanagethreads)?;
            let targets: Vec<String> = tids.iter().map(|t| format!("moved|{t}")).collect();
            sqlx::query("DELETE FROM threads WHERE closed = ANY($1)")
                .bind(&targets)
                .execute(uow.conn())
                .await?;
            "Redirects have been removed."
        }
        "removesubscriptions" => {
            require(ctx, threads, |m| m.canmanagethreads)?;
            sqlx::query("DELETE FROM threadsubscriptions WHERE tid = ANY($1)")
                .bind(&tids)
                .execute(uow.conn())
                .await?;
            "Subscriptions have been removed."
        }
        _ => return Err(AppError::user("Unknown moderation action.")),
    };
    for t in threads {
        log(
            &mut uow,
            ctx,
            t,
            0,
            &action_label(action),
            serde_json::json!({"subject": t.subject}),
        )
        .await?;
    }
    uow.commit(&ctx.app).await?;
    ctx.app.mod_counts.invalidate_all();
    Ok(msg.to_string())
}

fn action_label(a: &str) -> String {
    match a {
        "close" => "Thread closed",
        "open" => "Thread opened",
        "stick" => "Thread stuck",
        "unstick" => "Thread unstuck",
        "approve" => "Thread approved",
        "unapprove" => "Thread unapproved",
        "softdelete" => "Thread soft deleted",
        "restore" => "Thread restored",
        "delete" => "Thread deleted",
        "deletepoll" => "Poll deleted",
        "removeredirects" => "Redirects removed",
        "removesubscriptions" => "Subscriptions removed",
        other => other,
    }
    .to_string()
}

async fn move_form(ctx: &Ctx, tids: &[i32], fid: i32) -> AppResult<Response> {
    ctx.render(
        "moderation/move.html",
        minijinja::context! { title => "Move / Copy Threads", tids => tids, fid => fid, forums => crate::routes::forumdisplay::forum_jump(ctx), breadcrumb => crate::routes::forumdisplay::breadcrumb(ctx, fid) },
    )
    .await
}

pub async fn inline_threads(ctx: Ctx, CsrfForm(f): CsrfForm<InlineForm>) -> AppResult<Response> {
    let threads = load_threads(&ctx, &f.tids).await?;
    let back = ctx
        .cache
        .forum(f.fid)
        .map(|fo| url_forum(fo.fid as i64, Some(&fo.name)))
        .unwrap_or_else(|| "/".into());
    match f.action.as_str() {
        "move" => {
            require(&ctx, &threads, |m| m.canmanagethreads)?;
            move_form(&ctx, &f.tids, f.fid).await
        }
        "merge" => {
            require(&ctx, &threads, |m| m.canmanagethreads)?;
            if threads.len() < 2 {
                return Err(AppError::user("Select at least two threads to merge."));
            }
            // Merge all selected threads into the oldest one.
            let mut sorted = threads.clone();
            sorted.sort_by_key(|t| t.dateline);
            let into = sorted[0].tid;
            let mut uow = Uow::begin(&ctx.app).await?;
            for t in &sorted[1..] {
                ops::merge_threads_in(&mut uow, into, t.tid, None).await?;
            }
            log(
                &mut uow,
                &ctx,
                &sorted[0],
                0,
                "Threads merged",
                serde_json::json!({"merged": f.tids}),
            )
            .await?;
            uow.commit(&ctx.app).await?;
            Ok(ctx.redirect(
                &url_thread(into as i64, None),
                "The threads have been merged.",
            ))
        }
        a if a.starts_with("tool:") => {
            let id: i32 = a[5..].parse().unwrap_or(0);
            require(&ctx, &threads, |m| m.canusecustomtools)?;
            let msg = run_thread_tool(&ctx, id, &threads).await?;
            Ok(ctx.redirect(&back, &msg))
        }
        a => {
            let msg = thread_action(&ctx, &threads, a).await?;
            Ok(ctx.redirect(&back, &msg))
        }
    }
}

pub async fn thread_tool(
    ctx: Ctx,
    axum::extract::Path(tid): axum::extract::Path<i32>,
    CsrfForm(f): CsrfForm<InlineForm>,
) -> AppResult<Response> {
    let threads = load_threads(&ctx, &[tid]).await?;
    let t = threads
        .first()
        .cloned()
        .ok_or_else(|| AppError::not_found("thread"))?;
    let turl = url_thread(tid as i64, Some(&t.subject));
    match f.action.as_str() {
        "move" => {
            require(&ctx, &threads, |m| m.canmanagethreads)?;
            move_form(&ctx, &[tid], t.fid).await
        }
        "merge" => {
            require(&ctx, &threads, |m| m.canmanagethreads)?;
            ctx.render(
                "moderation/merge.html",
                minijinja::context! { title => "Merge Threads", thread => &t, thread_url => &turl },
            )
            .await
        }
        "edit" => {
            require(&ctx, &threads, |m| m.canmanagethreads)?;
            ctx.render("moderation/editthread.html", minijinja::context! { title => "Edit Thread", thread => &t, thread_url => &turl, prefixes => ctx.cache.prefixes_for(t.fid, &[]) }).await
        }
        "editpoll" => Ok(Redirect::to(&format!("/thread/{tid}/poll/edit")).into_response()),
        "log" => Ok(Redirect::to(&format!("/modcp/modlogs?tid={tid}")).into_response()),
        "delete" => {
            let msg = thread_action(&ctx, &threads, "delete").await?;
            let forum = ctx.cache.forum(t.fid);
            Ok(ctx.redirect(
                &url_forum(t.fid as i64, forum.map(|f| f.name.as_str())),
                &msg,
            ))
        }
        a if a.starts_with("tool:") => {
            let id: i32 = a[5..].parse().unwrap_or(0);
            require(&ctx, &threads, |m| m.canusecustomtools)?;
            let msg = run_thread_tool(&ctx, id, &threads).await?;
            let still: Option<i32> = sqlx::query_scalar("SELECT tid FROM threads WHERE tid = $1")
                .bind(tid)
                .fetch_optional(&ctx.app.db)
                .await?;
            Ok(ctx.redirect(if still.is_some() { &turl } else { "/" }, &msg))
        }
        a => {
            let msg = thread_action(&ctx, &threads, a).await?;
            Ok(ctx.redirect(&turl, &msg))
        }
    }
}

pub async fn inline_posts(ctx: Ctx, CsrfForm(f): CsrfForm<InlineForm>) -> AppResult<Response> {
    let threads = load_threads(&ctx, &[f.tid]).await?;
    let t = threads
        .first()
        .cloned()
        .ok_or_else(|| AppError::not_found("thread"))?;
    let mp = ctx.mod_perms(t.fid).ok_or_else(AppError::no_perm)?;
    // Only posts of this thread.
    let pids: Vec<i32> =
        sqlx::query_scalar("SELECT pid FROM posts WHERE pid = ANY($1) AND tid = $2")
            .bind(&f.pids)
            .bind(f.tid)
            .fetch_all(&ctx.app.db)
            .await?;
    if pids.is_empty() {
        return Err(AppError::user("Please select at least one post."));
    }
    let turl = url_thread(t.tid as i64, Some(&t.subject));
    let need = |ok: bool| if ok { Ok(()) } else { Err(AppError::no_perm()) };
    let mut uow = Uow::begin(&ctx.app).await?;
    let msg = match f.action.as_str() {
        "approve" => {
            need(mp.canapproveunapproveposts)?;
            ops::set_posts_visibility_in(&mut uow, &pids, 1).await?;
            "The posts have been approved."
        }
        "unapprove" => {
            need(mp.canapproveunapproveposts)?;
            ops::set_posts_visibility_in(&mut uow, &pids, 0).await?;
            "The posts have been unapproved."
        }
        "softdelete" => {
            need(mp.cansoftdeleteposts)?;
            ops::set_posts_visibility_in(&mut uow, &pids, -1).await?;
            "The posts have been soft deleted."
        }
        "restore" => {
            need(mp.canrestoreposts)?;
            ops::set_posts_visibility_in(&mut uow, &pids, 1).await?;
            "The posts have been restored."
        }
        "delete" => {
            need(mp.candeleteposts)?;
            ops::delete_posts_in(&mut uow, &pids).await?;
            "The posts have been deleted."
        }
        "merge" => {
            need(mp.canmanagethreads)?;
            ops::merge_posts_in(&mut uow, &pids, "\n[hr]\n").await?;
            "The posts have been merged."
        }
        "split" => {
            need(mp.canmanagethreads)?;
            drop(uow);
            return ctx
                .render(
                    "moderation/split.html",
                    minijinja::context! { title => "Split Thread", thread => &t, thread_url => &turl, pids => pids, forums => crate::routes::forumdisplay::forum_jump(&ctx) },
                )
                .await;
        }
        "moveposts" => {
            need(mp.canmanagethreads)?;
            drop(uow);
            return ctx.render("moderation/moveposts.html", minijinja::context! { title => "Move Posts", thread => &t, thread_url => &turl, pids => pids }).await;
        }
        a if a.starts_with("tool:") => {
            need(mp.canusecustomtools)?;
            drop(uow);
            let id: i32 = a[5..].parse().unwrap_or(0);
            let msg = run_post_tool(&ctx, id, &t, &pids).await?;
            return Ok(ctx.redirect(&turl, &msg));
        }
        _ => return Err(AppError::user("Unknown moderation action.")),
    };
    log(
        &mut uow,
        &ctx,
        &t,
        pids[0],
        &format!("Posts: {}", f.action),
        serde_json::json!({"pids": pids}),
    )
    .await?;
    uow.commit(&ctx.app).await?;
    ctx.app.mod_counts.invalidate_all();
    let still: Option<i32> = sqlx::query_scalar("SELECT tid FROM threads WHERE tid = $1")
        .bind(t.tid)
        .fetch_optional(&ctx.app.db)
        .await?;
    if still.is_some() {
        Ok(ctx.redirect(&turl, msg))
    } else {
        let forum = ctx.cache.forum(t.fid);
        Ok(ctx.redirect(
            &url_forum(t.fid as i64, forum.map(|f| f.name.as_str())),
            msg,
        ))
    }
}

#[derive(Deserialize, Default)]
pub struct MoveForm {
    #[serde(default, deserialize_with = "de::vec_i32")]
    pub tids: Vec<i32>,
    #[serde(default, deserialize_with = "de::i32")]
    pub target: i32,
    #[serde(default, deserialize_with = "de::string")]
    pub method: String, // move | redirect | copy
    #[serde(default, deserialize_with = "de::i64")]
    pub redirect_days: i64,
}

pub async fn do_move(ctx: Ctx, CsrfForm(f): CsrfForm<MoveForm>) -> AppResult<Response> {
    let threads = load_threads(&ctx, &f.tids).await?;
    require(&ctx, &threads, |m| m.canmanagethreads)?;
    let target = ctx
        .cache
        .forum(f.target)
        .cloned()
        .ok_or_else(|| AppError::not_found("forum"))?;
    if target.is_category() || !target.linkto.is_empty() {
        return Err(AppError::user(
            "You cannot move threads into a category or link forum.",
        ));
    }
    if !ctx.access().can_see(f.target) {
        return Err(AppError::no_perm());
    }
    if let Some(mp) = ctx.mod_perms(threads[0].fid)
        && !mp.canmovetononmodforum
        && ctx.mod_perms(f.target).is_none()
    {
        return Err(AppError::user(
            "You can only move threads to forums you moderate.",
        ));
    }
    let tids: Vec<i32> = threads.iter().map(|t| t.tid).collect();
    let mut uow = Uow::begin(&ctx.app).await?;
    let msg = match f.method.as_str() {
        "copy" => {
            let mut last = 0;
            for t in &tids {
                last = ops::copy_thread_in(&mut uow, *t, f.target).await?;
            }
            let _ = last;
            "The threads have been copied."
        }
        "redirect" => {
            ops::move_threads_in(&mut uow, &tids, f.target, Some(f.redirect_days.max(0))).await?;
            "The threads have been moved and redirects left behind."
        }
        _ => {
            ops::move_threads_in(&mut uow, &tids, f.target, None).await?;
            "The threads have been moved."
        }
    };
    for t in &threads {
        log(
            &mut uow,
            &ctx,
            t,
            0,
            if f.method == "copy" {
                "Thread copied"
            } else {
                "Thread moved"
            },
            serde_json::json!({"from": t.fid, "to": f.target, "subject": t.subject}),
        )
        .await?;
        if f.method != "copy" && t.uid > 0 && t.uid != ctx.uid() {
            uow.job(Job::Alert {
                uid: t.uid,
                from_uid: ctx.uid(),
                alert: "thread_moved".into(),
                object_id: t.tid,
                extra: serde_json::json!({"tid": t.tid, "subject": t.subject}),
            });
        }
    }
    uow.commit(&ctx.app).await?;
    let to = if tids.len() == 1 && f.method != "copy" {
        url_thread(tids[0] as i64, Some(&threads[0].subject))
    } else {
        url_forum(f.target as i64, Some(&target.name))
    };
    Ok(ctx.redirect(&to, msg))
}

fn parse_tid(s: &str) -> Option<i32> {
    let s = s.trim();
    if let Ok(n) = s.parse() {
        return Some(n);
    }
    for key in ["/thread/", "tid=", "thread-"] {
        if let Some(pos) = s.find(key) {
            return crate::util::leading_id(&s[pos + key.len()..]);
        }
    }
    None
}

#[derive(Deserialize, Default)]
pub struct MergeForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub tid: i32,
    #[serde(default, deserialize_with = "de::string")]
    pub target: String,
    #[serde(default, deserialize_with = "de::string")]
    pub subject: String,
}

pub async fn do_merge(ctx: Ctx, CsrfForm(f): CsrfForm<MergeForm>) -> AppResult<Response> {
    let target = parse_tid(&f.target)
        .ok_or_else(|| AppError::user("Please enter the URL or ID of the thread to merge with."))?;
    let threads = load_threads(&ctx, &[f.tid, target]).await?;
    if threads.len() != 2 {
        return Err(AppError::not_found("thread"));
    }
    require(&ctx, &threads, |m| m.canmanagethreads)?;
    // As in MyBB, the thread being viewed survives; the target is merged into it.
    let (into, from) = if threads[0].tid == f.tid {
        (&threads[0], &threads[1])
    } else {
        (&threads[1], &threads[0])
    };
    let mut uow = Uow::begin(&ctx.app).await?;
    ops::merge_threads_in(&mut uow, into.tid, from.tid, Some(&f.subject)).await?;
    log(
        &mut uow,
        &ctx,
        into,
        0,
        "Threads merged",
        serde_json::json!({"from": from.tid, "subject": from.subject}),
    )
    .await?;
    uow.commit(&ctx.app).await?;
    Ok(ctx.redirect(
        &url_thread(into.tid as i64, None),
        "The threads have been merged.",
    ))
}

#[derive(Deserialize, Default)]
pub struct SplitForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub tid: i32,
    #[serde(default, deserialize_with = "de::vec_i32")]
    pub pids: Vec<i32>,
    #[serde(default, deserialize_with = "de::string")]
    pub subject: String,
    #[serde(default, deserialize_with = "de::i32")]
    pub fid: i32,
}

pub async fn do_split(ctx: Ctx, CsrfForm(f): CsrfForm<SplitForm>) -> AppResult<Response> {
    let threads = load_threads(&ctx, &[f.tid]).await?;
    require(&ctx, &threads, |m| m.canmanagethreads)?;
    let t = &threads[0];
    let pids: Vec<i32> =
        sqlx::query_scalar("SELECT pid FROM posts WHERE pid = ANY($1) AND tid = $2 AND pid <> $3")
            .bind(&f.pids)
            .bind(t.tid)
            .bind(t.firstpost)
            .fetch_all(&ctx.app.db)
            .await?;
    if pids.is_empty() {
        return Err(AppError::user(
            "Select posts other than the first post to split.",
        ));
    }
    let subject = if f.subject.trim().is_empty() {
        format!("{} (split)", t.subject)
    } else {
        f.subject.trim().to_string()
    };
    let fid = if f.fid > 0 && f.fid != t.fid {
        // Same rules as moving a thread: a real, viewable forum, and one the moderator
        // moderates unless they may move threads to forums they don't.
        let target = ctx
            .cache
            .forum(f.fid)
            .ok_or_else(|| AppError::not_found("forum"))?;
        if target.is_category() || !target.linkto.is_empty() {
            return Err(AppError::user(
                "You cannot split posts into a category or link forum.",
            ));
        }
        if !ctx.access().can_see(f.fid) {
            return Err(AppError::no_perm());
        }
        let can_move_out = ctx
            .mod_perms(t.fid)
            .map(|m| m.canmovetononmodforum)
            .unwrap_or(false);
        if !can_move_out && ctx.mod_perms(f.fid).is_none() {
            return Err(AppError::user(
                "You can only split posts into forums you moderate.",
            ));
        }
        f.fid
    } else {
        t.fid
    };
    let mut uow = Uow::begin(&ctx.app).await?;
    let new_tid = ops::split_posts_in(&mut uow, &pids, &subject, fid).await?;
    log(
        &mut uow,
        &ctx,
        t,
        0,
        "Thread split",
        serde_json::json!({"new_tid": new_tid, "pids": pids}),
    )
    .await?;
    uow.commit(&ctx.app).await?;
    Ok(ctx.redirect(
        &url_thread(new_tid as i64, Some(&subject)),
        "The thread has been split.",
    ))
}

#[derive(Deserialize, Default)]
pub struct MovePostsForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub tid: i32,
    #[serde(default, deserialize_with = "de::vec_i32")]
    pub pids: Vec<i32>,
    #[serde(default, deserialize_with = "de::string")]
    pub target: String,
}

pub async fn do_moveposts(ctx: Ctx, CsrfForm(f): CsrfForm<MovePostsForm>) -> AppResult<Response> {
    let target = parse_tid(&f.target)
        .ok_or_else(|| AppError::user("Please enter the URL or ID of the destination thread."))?;
    let threads = load_threads(&ctx, &[f.tid, target]).await?;
    if threads.len() != 2 {
        return Err(AppError::not_found("thread"));
    }
    require(&ctx, &threads, |m| m.canmanagethreads)?;
    let src = threads.iter().find(|t| t.tid == f.tid).unwrap();
    let pids: Vec<i32> =
        sqlx::query_scalar("SELECT pid FROM posts WHERE pid = ANY($1) AND tid = $2 AND pid <> $3")
            .bind(&f.pids)
            .bind(src.tid)
            .bind(src.firstpost)
            .fetch_all(&ctx.app.db)
            .await?;
    if pids.is_empty() {
        return Err(AppError::user(
            "Select posts other than the first post to move.",
        ));
    }
    // Split into a temporary thread, then merge it into the destination (one transaction, so
    // the temporary thread is never visible and never left behind).
    let mut uow = Uow::begin(&ctx.app).await?;
    let tmp = ops::split_posts_in(&mut uow, &pids, &src.subject, src.fid).await?;
    ops::merge_threads_in(&mut uow, target, tmp, None).await?;
    log(
        &mut uow,
        &ctx,
        src,
        0,
        "Posts moved",
        serde_json::json!({"to": target, "pids": pids}),
    )
    .await?;
    uow.commit(&ctx.app).await?;
    Ok(ctx.redirect(
        &url_thread(target as i64, None),
        "The posts have been moved.",
    ))
}

#[derive(Deserialize, Default)]
pub struct EditThreadForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub tid: i32,
    #[serde(default, deserialize_with = "de::string")]
    pub subject: String,
    #[serde(default, deserialize_with = "de::string")]
    pub notes: String,
    #[serde(default, deserialize_with = "de::i32")]
    pub prefix: i32,
}

pub async fn do_editthread(ctx: Ctx, CsrfForm(f): CsrfForm<EditThreadForm>) -> AppResult<Response> {
    let threads = load_threads(&ctx, &[f.tid]).await?;
    require(&ctx, &threads, |m| m.canmanagethreads)?;
    let subject = f.subject.trim();
    if subject.is_empty() {
        return Err(AppError::user("The subject cannot be empty."));
    }
    let mut uow = Uow::begin(&ctx.app).await?;
    ops::lock_threads(uow.conn(), &[f.tid]).await?;
    sqlx::query("UPDATE threads SET subject = $2, notes = $3, prefix = $4 WHERE tid = $1")
        .bind(f.tid)
        .bind(subject)
        .bind(f.notes.trim())
        .bind(f.prefix)
        .execute(uow.conn())
        .await?;
    sqlx::query("UPDATE posts SET subject = $1 WHERE pid = $2")
        .bind(subject)
        .bind(threads[0].firstpost)
        .execute(uow.conn())
        .await?;
    sqlx::query("UPDATE forums SET lastpostsubject = $2 WHERE lastposttid = $1")
        .bind(f.tid)
        .bind(subject)
        .execute(uow.conn())
        .await?;
    log(
        &mut uow,
        &ctx,
        &threads[0],
        0,
        "Thread edited",
        serde_json::json!({"old": threads[0].subject, "new": subject}),
    )
    .await?;
    uow.commit(&ctx.app).await?;
    Ok(ctx.redirect(
        &url_thread(f.tid as i64, Some(subject)),
        "The thread has been updated.",
    ))
}

// ---------------------------------------------------------------- custom moderator tools

async fn load_tool(
    ctx: &Ctx,
    id: i32,
    kind: &str,
    fid: i32,
) -> AppResult<(serde_json::Value, serde_json::Value, String)> {
    let row: Option<(String, Vec<i32>, Vec<i32>, serde_json::Value, serde_json::Value, String)> =
        sqlx::query_as("SELECT type::text, forums, groups, threadoptions, postoptions, name FROM modtools WHERE tid = $1").bind(id).fetch_optional(&ctx.app.db).await?;
    let (ty, forums, groups, topts, popts, name) =
        row.ok_or_else(|| AppError::not_found("moderator tool"))?;
    let parents = ctx
        .cache
        .forum(fid)
        .map(|f| f.parentlist.clone())
        .unwrap_or_default();
    if ty != kind
        || (!forums.is_empty() && !forums.iter().any(|f| parents.contains(f)))
        || (!groups.is_empty() && !groups.iter().any(|g| ctx.groups.contains(g)))
    {
        return Err(AppError::no_perm());
    }
    Ok((topts, popts, name))
}

fn opt_str<'a>(o: &'a serde_json::Value, k: &str) -> &'a str {
    o.get(k).and_then(|v| v.as_str()).unwrap_or("")
}
fn opt_i(o: &serde_json::Value, k: &str) -> i64 {
    o.get(k)
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(0)
}
fn opt_b(o: &serde_json::Value, k: &str) -> bool {
    o.get(k)
        .map(|v| v.as_bool().unwrap_or(false) || v.as_str() == Some("1") || v.as_i64() == Some(1))
        .unwrap_or(false)
}

/// Thread tool options (subset of MyBB's): openthread (open|close|toggle), stickthread (stick|unstick|toggle),
/// approvethread (approve|unapprove|toggle), softdeletethread (softdelete|restore), deletethread,
/// deletepoll, removeredirects, removesubscriptions, movethread (fid), movethreadredirect, copythread (fid),
/// newsubject ("{subject}" placeholder), threadprefix (pid), addreply (message), pm_subject/pm_message.
async fn run_thread_tool(ctx: &Ctx, id: i32, threads: &[Thread]) -> AppResult<String> {
    let (o, _, name) = load_tool(ctx, id, "t", threads[0].fid).await?;
    let mut uow = Uow::begin(&ctx.app).await?;
    let tids: Vec<i32> = threads.iter().map(|t| t.tid).collect();
    ops::lock_threads(uow.conn(), &tids).await?;
    for t in threads {
        let tid = t.tid;
        let toggle = |v: &str, on: bool| match v {
            "toggle" => Some(!on),
            "open" | "stick" | "approve" => Some(true),
            "close" | "unstick" | "unapprove" => Some(false),
            _ => None,
        };
        if let Some(close) = toggle(opt_str(&o, "openthread"), !t.is_closed()).map(|open| !open) {
            sqlx::query("UPDATE threads SET closed = $2 WHERE tid = $1")
                .bind(tid)
                .bind(if close { "1" } else { "" })
                .execute(uow.conn())
                .await?;
        }
        if let Some(stick) = toggle(opt_str(&o, "stickthread"), t.sticky) {
            sqlx::query("UPDATE threads SET sticky = $2 WHERE tid = $1")
                .bind(tid)
                .bind(stick)
                .execute(uow.conn())
                .await?;
        }
        if let Some(appr) = toggle(opt_str(&o, "approvethread"), t.visible == 1) {
            ops::set_threads_visibility_in(&mut uow, &[tid], if appr { 1 } else { 0 }).await?;
        }
        match opt_str(&o, "softdeletethread") {
            "softdelete" => ops::set_threads_visibility_in(&mut uow, &[tid], -1).await?,
            "restore" => ops::set_threads_visibility_in(&mut uow, &[tid], 1).await?,
            _ => {}
        }
        if opt_b(&o, "deletepoll") {
            sqlx::query("DELETE FROM polls WHERE tid = $1")
                .bind(tid)
                .execute(uow.conn())
                .await?;
            sqlx::query("UPDATE threads SET poll = 0 WHERE tid = $1")
                .bind(tid)
                .execute(uow.conn())
                .await?;
        }
        if opt_b(&o, "removesubscriptions") {
            sqlx::query("DELETE FROM threadsubscriptions WHERE tid = $1")
                .bind(tid)
                .execute(uow.conn())
                .await?;
        }
        if opt_b(&o, "removeredirects") {
            sqlx::query("DELETE FROM threads WHERE closed = $1")
                .bind(format!("moved|{tid}"))
                .execute(uow.conn())
                .await?;
        }
        let newsub = opt_str(&o, "newsubject");
        if !newsub.is_empty() && newsub != "{subject}" {
            let s = newsub
                .replace("{subject}", &t.subject)
                .replace("{username}", ctx.username());
            sqlx::query("UPDATE threads SET subject = $2 WHERE tid = $1")
                .bind(tid)
                .bind(&s)
                .execute(uow.conn())
                .await?;
        }
        let prefix = opt_i(&o, "threadprefix");
        if prefix > 0 || opt_str(&o, "threadprefix") == "0" {
            sqlx::query("UPDATE threads SET prefix = $2 WHERE tid = $1")
                .bind(tid)
                .bind(prefix as i32)
                .execute(uow.conn())
                .await?;
        }
        let reply = opt_str(&o, "addreply");
        if !reply.is_empty() {
            let input = crate::posting::PostInput {
                subject: format!("RE: {}", t.subject),
                message: reply.replace("{username}", &crate::parser::literal(&t.username)),
                icon: 0,
                includesig: true,
                smilieoff: false,
                posthash: String::new(),
                replyto: 0,
                as_system: false,
            };
            crate::posting::create_reply_in(&mut uow, ctx, tid, t.fid, &input, None).await?;
        }
        let pm_sub = opt_str(&o, "pm_subject");
        if !pm_sub.is_empty() && t.uid > 0 {
            let msg = opt_str(&o, "pm_message")
                .replace("{username}", &crate::parser::literal(&t.username))
                .replace("{subject}", &crate::parser::literal(&t.subject));
            uow.job(Job::SystemPm {
                uid: t.uid,
                subject: pm_sub.to_string(),
                message: msg,
            });
        }
        let copy_to = opt_i(&o, "copythread") as i32;
        if copy_to > 0 {
            ops::copy_thread_in(&mut uow, tid, copy_to).await?;
        }
        let move_to = opt_i(&o, "movethread") as i32;
        if move_to > 0 && move_to != t.fid {
            let redirect = if opt_b(&o, "movethreadredirect") {
                Some(opt_i(&o, "movethreadredirectexpire"))
            } else {
                None
            };
            ops::move_threads_in(&mut uow, &[tid], move_to, redirect).await?;
        }
        if opt_b(&o, "deletethread") {
            ops::delete_threads_in(&mut uow, &[tid]).await?;
        }
        log(
            &mut uow,
            ctx,
            t,
            0,
            &format!("Custom tool: {name}"),
            serde_json::json!({}),
        )
        .await?;
    }
    uow.commit(&ctx.app).await?;
    ctx.app.mod_counts.invalidate_all();
    Ok(format!("The moderation tool “{name}” has been run."))
}

/// Post tool options: approveposts, softdeleteposts (softdelete|restore), deleteposts, mergeposts,
/// splitposts (fid; -2 = same forum), splitpostsnewsubject.
async fn run_post_tool(ctx: &Ctx, id: i32, t: &Thread, pids: &[i32]) -> AppResult<String> {
    let (_, o, name) = load_tool(ctx, id, "p", t.fid).await?;
    let mut uow = Uow::begin(&ctx.app).await?;
    match opt_str(&o, "approveposts") {
        "approve" => ops::set_posts_visibility_in(&mut uow, pids, 1).await?,
        "unapprove" => ops::set_posts_visibility_in(&mut uow, pids, 0).await?,
        _ => {}
    }
    match opt_str(&o, "softdeleteposts") {
        "softdelete" => ops::set_posts_visibility_in(&mut uow, pids, -1).await?,
        "restore" => ops::set_posts_visibility_in(&mut uow, pids, 1).await?,
        _ => {}
    }
    if opt_b(&o, "mergeposts") {
        ops::merge_posts_in(&mut uow, pids, "\n[hr]\n").await?;
    }
    let split = opt_i(&o, "splitposts");
    if split != 0 && split != -1 {
        let fid = if split == -2 { t.fid } else { split as i32 };
        let subj = opt_str(&o, "splitpostsnewsubject").replace("{subject}", &t.subject);
        let subj = if subj.is_empty() {
            format!("{} (split)", t.subject)
        } else {
            subj
        };
        let valid: Vec<i32> = pids.iter().copied().filter(|p| *p != t.firstpost).collect();
        if !valid.is_empty() {
            ops::split_posts_in(&mut uow, &valid, &subj, fid).await?;
        }
    }
    if opt_b(&o, "deleteposts") {
        ops::delete_posts_in(&mut uow, pids).await?;
    }
    log(
        &mut uow,
        ctx,
        t,
        pids[0],
        &format!("Custom tool: {name}"),
        serde_json::json!({"pids": pids}),
    )
    .await?;
    uow.commit(&ctx.app).await?;
    ctx.app.mod_counts.invalidate_all();
    Ok(format!("The moderation tool “{name}” has been run."))
}

// ---------------------------------------------------------------- delayed moderation

#[derive(Deserialize, Default)]
pub struct DelayedForm {
    #[serde(default, deserialize_with = "de::vec_i32")]
    pub tids: Vec<i32>,
    #[serde(default, deserialize_with = "de::string")]
    pub action: String,
    #[serde(default, deserialize_with = "de::i64")]
    pub days: i64,
    #[serde(default, deserialize_with = "de::i32")]
    pub target: i32,
}

pub async fn schedule_delayed(ctx: Ctx, CsrfForm(f): CsrfForm<DelayedForm>) -> AppResult<Response> {
    let threads = load_threads(&ctx, &f.tids).await?;
    require(&ctx, &threads, |m| {
        m.canmanagethreads || m.canopenclosethreads
    })?;
    if !matches!(
        f.action.as_str(),
        "close" | "open" | "stick" | "unstick" | "softdelete" | "move" | "delete" | "approve"
    ) {
        return Err(AppError::user("That action cannot be scheduled."));
    }
    let days = f.days.clamp(1, 365);
    sqlx::query("INSERT INTO delayedmoderation (type, delaydateline, uid, fid, tids, dateline, inputs) VALUES ($1, $2, $3, $4, $5, $6, $7)")
        .bind(&f.action)
        .bind(now() + days * 86400)
        .bind(ctx.uid())
        .bind(threads[0].fid)
        .bind(&f.tids)
        .bind(now())
        .bind(serde_json::json!({"target": f.target}))
        .execute(&ctx.app.db)
        .await?;
    Ok(ctx.redirect(
        &url_thread(f.tids[0] as i64, None),
        &format!("The action has been scheduled to run in {days} day(s)."),
    ))
}

#[derive(Deserialize, Default)]
pub struct CancelForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub did: i32,
}

pub async fn cancel_delayed(ctx: Ctx, CsrfForm(f): CsrfForm<CancelForm>) -> AppResult<Response> {
    // Only moderators of the forum the action was scheduled in.
    let fid: Option<i32> = sqlx::query_scalar("SELECT fid FROM delayedmoderation WHERE did = $1")
        .bind(f.did)
        .fetch_optional(&ctx.app.db)
        .await?;
    let Some(fid) = fid else {
        return Err(AppError::not_found("scheduled action"));
    };
    if !ctx
        .mod_perms(fid)
        .is_some_and(|m| m.canmanagethreads || m.canopenclosethreads)
    {
        return Err(AppError::no_perm());
    }
    sqlx::query("DELETE FROM delayedmoderation WHERE did = $1")
        .bind(f.did)
        .execute(&ctx.app.db)
        .await?;
    Ok(ctx.redirect("/modcp/delayed", "The scheduled action has been cancelled."))
}

/// Task: execute due delayed moderation actions. Each action is claimed (deleted, skipping
/// rows another node is working on), performed and logged in one transaction, so a failure
/// leaves it scheduled for the next run instead of losing it.
pub async fn run_delayed(app: &App) -> anyhow::Result<String> {
    let mut n = 0;
    loop {
        let mut uow = Uow::begin(app).await?;
        let due: Option<(i32, String, Vec<i32>, serde_json::Value, i32)> = sqlx::query_as(
            "DELETE FROM delayedmoderation WHERE did = (
                SELECT did FROM delayedmoderation WHERE delaydateline <= $1 ORDER BY did LIMIT 1 FOR UPDATE SKIP LOCKED)
             RETURNING did, type, tids, inputs, uid",
        )
        .bind(now())
        .fetch_optional(uow.conn())
        .await?;
        let Some((did, kind, tids, inputs, uid)) = due else {
            break;
        };
        let r: AppResult<()> = async {
            match kind.as_str() {
                "close" | "open" => {
                    sqlx::query("UPDATE threads SET closed = $2 WHERE tid = ANY($1) AND closed NOT LIKE 'moved|%'")
                        .bind(&tids)
                        .bind(if kind == "close" { "1" } else { "" })
                        .execute(uow.conn())
                        .await?;
                }
                "stick" | "unstick" => {
                    sqlx::query("UPDATE threads SET sticky = $2 WHERE tid = ANY($1)")
                        .bind(&tids)
                        .bind(kind == "stick")
                        .execute(uow.conn())
                        .await?;
                }
                "softdelete" => ops::set_threads_visibility_in(&mut uow, &tids, -1).await?,
                "approve" => ops::set_threads_visibility_in(&mut uow, &tids, 1).await?,
                "delete" => ops::delete_threads_in(&mut uow, &tids).await?,
                "move" => {
                    let target = inputs["target"].as_i64().unwrap_or(0) as i32;
                    if target > 0 {
                        ops::move_threads_in(&mut uow, &tids, target, None).await?;
                    }
                }
                _ => {}
            }
            for t in &tids {
                ops::log_moderator_action_in(
                    uow.conn(),
                    uid,
                    "",
                    0,
                    *t,
                    0,
                    &format!("Delayed moderation: {kind}"),
                    serde_json::json!({}),
                )
                .await?;
            }
            Ok(())
        }
        .await;
        match r {
            Ok(()) => {
                uow.commit(app).await?;
                n += 1;
            }
            Err(e) => {
                // Rolled back: the action stays scheduled. Push it back so the loop moves on.
                drop(uow);
                tracing::warn!(action = did, "delayed moderation failed: {e}");
                sqlx::query("UPDATE delayedmoderation SET delaydateline = $2 WHERE did = $1")
                    .bind(did)
                    .bind(now() + 3600)
                    .execute(&app.db)
                    .await?;
            }
        }
    }
    if n > 0 {
        app.mod_counts.invalidate_all();
    }
    Ok(format!("ran {n} delayed actions"))
}
