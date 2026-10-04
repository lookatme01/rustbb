//! Content state changes that keep the denormalized counters consistent.
//!
//! Forum counters are maintained with deltas: each thread "contributes" a set of counts to its
//! forum depending on its visibility; an operation snapshots the contribution before and after
//! and applies the difference. Thread counters are recounted exactly (a cheap indexed query per
//! thread). This keeps every write O(size of the affected thread), never O(size of forum).
//!
//! Every operation has a `*_in` form that runs inside a caller's unit of work, so a use case can
//! combine several of them (and its own writes) atomically; the plain form is its own unit of
//! work. Locks are always taken in the same order — threads by id, then posts by id — so
//! concurrent operations cannot deadlock each other. Files are removed only after commit.

use crate::app::App;
use crate::error::AppResult;
use crate::infra::outbox::Job;
use crate::usecase::Uow;
use crate::util::now;
use sqlx::PgConnection;

#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub struct Contrib {
    pub threads: i64,
    pub posts: i64,
    pub unapprovedthreads: i64,
    pub unapprovedposts: i64,
    pub deletedthreads: i64,
    pub deletedposts: i64,
}

impl Contrib {
    fn sub(self, o: Contrib) -> Contrib {
        Contrib {
            threads: self.threads - o.threads,
            posts: self.posts - o.posts,
            unapprovedthreads: self.unapprovedthreads - o.unapprovedthreads,
            unapprovedposts: self.unapprovedposts - o.unapprovedposts,
            deletedthreads: self.deletedthreads - o.deletedthreads,
            deletedposts: self.deletedposts - o.deletedposts,
        }
    }
    fn is_zero(&self) -> bool {
        *self == Contrib::default()
    }
}

/// (fid, contribution) of a thread to its forum's counters. Missing thread = zero.
pub async fn thread_contrib(c: &mut PgConnection, tid: i32) -> AppResult<(i32, Contrib)> {
    let t: Option<(i32, i16, String, i32, i32, i32)> =
        sqlx::query_as("SELECT fid, visible, closed, replies, unapprovedposts, deletedposts FROM threads WHERE tid = $1")
            .bind(tid)
            .fetch_optional(&mut *c)
            .await?;
    let Some((fid, vis, closed, replies, unap, del)) = t else {
        return Ok((0, Contrib::default()));
    };
    if closed.starts_with("moved|") {
        return Ok((fid, Contrib::default()));
    }
    let (replies, unap, del) = (replies as i64, unap as i64, del as i64);
    let total = replies + 1 + unap + del;
    Ok((
        fid,
        match vis {
            1 => Contrib {
                threads: 1,
                posts: replies + 1,
                unapprovedposts: unap,
                deletedposts: del,
                ..Default::default()
            },
            0 => Contrib {
                unapprovedthreads: 1,
                unapprovedposts: total - 1 + 1,
                ..Default::default()
            },
            _ => Contrib {
                deletedthreads: 1,
                deletedposts: total,
                ..Default::default()
            },
        },
    ))
}

pub async fn apply_forum_delta(c: &mut PgConnection, fid: i32, d: Contrib) -> AppResult<()> {
    if fid == 0 || d.is_zero() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE forums SET threads = GREATEST(threads + $2, 0), posts = GREATEST(posts + $3, 0),
            unapprovedthreads = GREATEST(unapprovedthreads + $4, 0), unapprovedposts = GREATEST(unapprovedposts + $5, 0),
            deletedthreads = GREATEST(deletedthreads + $6, 0), deletedposts = GREATEST(deletedposts + $7, 0)
         WHERE fid = $1",
    )
    .bind(fid)
    .bind(d.threads as i32)
    .bind(d.posts as i32)
    .bind(d.unapprovedthreads as i32)
    .bind(d.unapprovedposts as i32)
    .bind(d.deletedthreads as i32)
    .bind(d.deletedposts as i32)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Recount a thread's counters and first/last post info from its posts.
/// Note: `replies` counts visible posts minus the first post; when the first post is not
/// visible the thread itself is not visible, so the counts still add up.
pub async fn recount_thread(c: &mut PgConnection, tid: i32) -> AppResult<()> {
    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*) FILTER (WHERE visible = 1), COUNT(*) FILTER (WHERE visible = 0), COUNT(*) FILTER (WHERE visible = -1)
         FROM posts WHERE tid = $1",
    )
    .bind(tid)
    .fetch_one(&mut *c)
    .await?;
    let first: Option<(i32, i32, String, i64, i16)> =
        sqlx::query_as("SELECT pid, uid, username, dateline, visible FROM posts WHERE tid = $1 ORDER BY dateline, pid LIMIT 1")
            .bind(tid)
            .fetch_optional(&mut *c)
            .await?;
    let Some((fpid, fuid, fname, fdate, fvis)) = first else {
        return Ok(());
    };
    let last: Option<(i64, String, i32)> = sqlx::query_as(
        "SELECT dateline, username, uid FROM posts WHERE tid = $1 AND visible = 1 ORDER BY dateline DESC, pid DESC LIMIT 1",
    )
    .bind(tid)
    .fetch_optional(&mut *c)
    .await?;
    let (lp, lpname, lpuid) = last.unwrap_or((fdate, fname.clone(), fuid));
    // The first post's own state is represented by the thread's visibility.
    let (mut vis, mut unap, mut del) = counts;
    match fvis {
        1 => vis -= 1,
        0 => unap -= 1,
        _ => del -= 1,
    }
    let attach: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM attachments a JOIN posts p ON p.pid = a.pid WHERE p.tid = $1 AND p.visible = 1 AND a.visible",
    )
    .bind(tid)
    .fetch_one(&mut *c)
    .await?;
    sqlx::query(
        "UPDATE threads SET replies = $2, unapprovedposts = $3, deletedposts = $4, firstpost = $5, uid = $6, username = $7,
            dateline = $8, lastpost = $9, lastposter = $10, lastposteruid = $11, attachmentcount = $12
         WHERE tid = $1",
    )
    .bind(tid)
    .bind(vis.max(0) as i32)
    .bind(unap.max(0) as i32)
    .bind(del.max(0) as i32)
    .bind(fpid)
    .bind(fuid)
    .bind(&fname)
    .bind(fdate)
    .bind(lp)
    .bind(&lpname)
    .bind(lpuid)
    .bind(attach.0 as i32)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Recompute a forum's "last post" columns from its most recently active visible thread.
pub async fn update_forum_lastpost(c: &mut PgConnection, fid: i32) -> AppResult<()> {
    let last: Option<(i64, String, i32, i32, String)> = sqlx::query_as(
        "SELECT lastpost, lastposter, lastposteruid, tid, subject FROM threads
         WHERE fid = $1 AND visible = 1 AND closed NOT LIKE 'moved|%' ORDER BY lastpost DESC LIMIT 1",
    )
    .bind(fid)
    .fetch_optional(&mut *c)
    .await?;
    let (lp, lpname, lpuid, lptid, lpsub) = last.unwrap_or((0, String::new(), 0, 0, String::new()));
    sqlx::query("UPDATE forums SET lastpost = $2, lastposter = $3, lastposteruid = $4, lastposttid = $5, lastpostsubject = $6 WHERE fid = $1")
        .bind(fid)
        .bind(lp)
        .bind(lpname)
        .bind(lpuid)
        .bind(lptid)
        .bind(lpsub)
        .execute(&mut *c)
        .await?;
    Ok(())
}

/// Snapshot contributions for a set of threads.
async fn snapshot(c: &mut PgConnection, tids: &[i32]) -> AppResult<Vec<(i32, i32, Contrib)>> {
    let mut v = Vec::with_capacity(tids.len());
    for &t in tids {
        let (fid, con) = thread_contrib(c, t).await?;
        v.push((t, fid, con));
    }
    Ok(v)
}

/// Apply before/after deltas and refresh last-post info for all touched forums.
async fn settle(c: &mut PgConnection, before: Vec<(i32, i32, Contrib)>) -> AppResult<()> {
    let mut fids = Vec::new();
    for (tid, fid_b, con_b) in before {
        recount_thread(c, tid).await?;
        let (fid_a, con_a) = thread_contrib(c, tid).await?;
        if fid_a == fid_b {
            apply_forum_delta(c, fid_a, con_a.sub(con_b)).await?;
        } else {
            apply_forum_delta(c, fid_b, Contrib::default().sub(con_b)).await?;
            apply_forum_delta(c, fid_a, con_a).await?;
        }
        for f in [fid_a, fid_b] {
            if f > 0 && !fids.contains(&f) {
                fids.push(f);
            }
        }
    }
    for f in fids {
        update_forum_lastpost(c, f).await?;
    }
    Ok(())
}

/// Adjust user post counts for visible posts (in post-counting forums and visible threads).
async fn adjust_user_postcounts(c: &mut PgConnection, pids: &[i32], sign: i32) -> AppResult<()> {
    if pids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE users SET postnum = GREATEST(postnum + $2 * d.c, 0) FROM (
            SELECT p.uid, COUNT(*)::int AS c FROM posts p JOIN forums f ON f.fid = p.fid JOIN threads t ON t.tid = p.tid
            WHERE p.pid = ANY($1) AND p.visible = 1 AND t.visible = 1 AND f.usepostcounts AND p.uid > 0 GROUP BY p.uid) d
         WHERE users.uid = d.uid",
    )
    .bind(pids)
    .bind(sign)
    .execute(&mut *c)
    .await?;
    Ok(())
}

async fn adjust_user_threadcounts(c: &mut PgConnection, tids: &[i32], sign: i32) -> AppResult<()> {
    sqlx::query(
        "UPDATE users SET threadnum = GREATEST(threadnum + $2 * d.c, 0) FROM (
            SELECT t.uid, COUNT(*)::int AS c FROM threads t JOIN forums f ON f.fid = t.fid
            WHERE t.tid = ANY($1) AND t.visible = 1 AND f.usethreadcounts AND t.uid > 0 AND t.closed NOT LIKE 'moved|%' GROUP BY t.uid) d
         WHERE users.uid = d.uid",
    )
    .bind(tids)
    .bind(sign)
    .execute(&mut *c)
    .await?;
    Ok(())
}

async fn all_pids_of_threads(c: &mut PgConnection, tids: &[i32]) -> AppResult<Vec<i32>> {
    Ok(
        sqlx::query_scalar("SELECT pid FROM posts WHERE tid = ANY($1)")
            .bind(tids)
            .fetch_all(&mut *c)
            .await?,
    )
}

/// Lock threads (in id order) for the rest of the transaction.
pub async fn lock_threads(c: &mut PgConnection, tids: &[i32]) -> AppResult<()> {
    sqlx::query("SELECT tid FROM threads WHERE tid = ANY($1) ORDER BY tid FOR UPDATE")
        .bind(tids)
        .fetch_all(&mut *c)
        .await?;
    Ok(())
}

/// Lock the threads of these posts, then the posts (both in id order). Returns
/// (pid, tid, firstpost of its thread) for the posts that exist.
pub async fn lock_posts(c: &mut PgConnection, pids: &[i32]) -> AppResult<Vec<(i32, i32, i32)>> {
    let tids: Vec<i32> = sqlx::query_scalar("SELECT DISTINCT tid FROM posts WHERE pid = ANY($1)")
        .bind(pids)
        .fetch_all(&mut *c)
        .await?;
    lock_threads(c, &tids).await?;
    Ok(sqlx::query_as(
        "SELECT p.pid, p.tid, t.firstpost FROM posts p JOIN threads t ON t.tid = p.tid
         WHERE p.pid = ANY($1) ORDER BY p.pid FOR UPDATE OF p",
    )
    .bind(pids)
    .fetch_all(&mut *c)
    .await?)
}

/// Run `f` as its own unit of work.
macro_rules! own_uow {
    ($app:expr, |$uow:ident| $body:expr) => {{
        let mut $uow = Uow::begin($app).await?;
        let r = $body;
        $uow.commit($app).await?;
        Ok(r)
    }};
}

/// Change visibility of posts (approve = 1, unapprove = 0, soft delete = -1).
/// First posts are routed to thread visibility changes.
pub async fn set_posts_visibility(app: &App, pids: &[i32], vis: i16) -> AppResult<()> {
    own_uow!(app, |uow| set_posts_visibility_in(&mut uow, pids, vis)
        .await?)
}

pub async fn set_posts_visibility_in(uow: &mut Uow, pids: &[i32], vis: i16) -> AppResult<()> {
    let rows = lock_posts(uow.conn(), pids).await?;
    let mut first_tids = Vec::new();
    let mut reply_pids = Vec::new();
    let mut tids = Vec::new();
    for (pid, tid, first) in rows {
        if pid == first {
            first_tids.push(tid);
        } else {
            reply_pids.push(pid);
            if !tids.contains(&tid) {
                tids.push(tid);
            }
        }
    }
    if !reply_pids.is_empty() {
        let c = uow.conn();
        let before = snapshot(c, &tids).await?;
        adjust_user_postcounts(c, &reply_pids, -1).await?;
        sqlx::query("UPDATE posts SET visible = $2 WHERE pid = ANY($1)")
            .bind(&reply_pids)
            .bind(vis)
            .execute(&mut *c)
            .await?;
        adjust_user_postcounts(c, &reply_pids, 1).await?;
        settle(c, before).await?;
    }
    if !first_tids.is_empty() {
        set_threads_visibility_in(uow, &first_tids, vis).await?;
    }
    Ok(())
}

/// Change visibility of whole threads (their first post follows the thread).
pub async fn set_threads_visibility(app: &App, tids: &[i32], vis: i16) -> AppResult<()> {
    own_uow!(app, |uow| set_threads_visibility_in(&mut uow, tids, vis)
        .await?)
}

pub async fn set_threads_visibility_in(uow: &mut Uow, tids: &[i32], vis: i16) -> AppResult<()> {
    let c = uow.conn();
    lock_threads(c, tids).await?;
    let before = snapshot(c, tids).await?;
    let pids = all_pids_of_threads(c, tids).await?;
    adjust_user_postcounts(c, &pids, -1).await?;
    adjust_user_threadcounts(c, tids, -1).await?;
    let dt = if vis == -1 { now() } else { 0 };
    sqlx::query("UPDATE threads SET visible = $2, deletetime = $3 WHERE tid = ANY($1)")
        .bind(tids)
        .bind(vis)
        .bind(dt)
        .execute(&mut *c)
        .await?;
    sqlx::query("UPDATE posts SET visible = $2 WHERE pid IN (SELECT firstpost FROM threads WHERE tid = ANY($1))")
        .bind(tids)
        .bind(vis)
        .execute(&mut *c)
        .await?;
    adjust_user_postcounts(c, &pids, 1).await?;
    adjust_user_threadcounts(c, tids, 1).await?;
    settle(c, before).await?;
    Ok(())
}

/// Permanently delete posts (first posts delete their whole thread).
pub async fn delete_posts(app: &App, pids: &[i32]) -> AppResult<()> {
    own_uow!(app, |uow| delete_posts_in(&mut uow, pids).await?)
}

pub async fn delete_posts_in(uow: &mut Uow, pids: &[i32]) -> AppResult<()> {
    let rows = lock_posts(uow.conn(), pids).await?;
    let mut thread_deletes = Vec::new();
    let mut reply_pids = Vec::new();
    let mut tids = Vec::new();
    for (pid, tid, first) in rows {
        if pid == first {
            thread_deletes.push(tid);
        } else {
            reply_pids.push(pid);
            if !tids.contains(&tid) {
                tids.push(tid);
            }
        }
    }
    tids.retain(|t| !thread_deletes.contains(t));
    if !reply_pids.is_empty() {
        let before = snapshot(uow.conn(), &tids).await?;
        adjust_user_postcounts(uow.conn(), &reply_pids, -1).await?;
        delete_attachments_of_posts(uow, &reply_pids).await?;
        let c = uow.conn();
        sqlx::query("DELETE FROM posts WHERE pid = ANY($1)")
            .bind(&reply_pids)
            .execute(&mut *c)
            .await?;
        sqlx::query("DELETE FROM reportedcontent WHERE type = 'post' AND id = ANY($1)")
            .bind(&reply_pids)
            .execute(&mut *c)
            .await?;
        settle(c, before).await?;
    }
    if !thread_deletes.is_empty() {
        delete_threads_in(uow, &thread_deletes).await?;
    }
    Ok(())
}

pub async fn delete_threads(app: &App, tids: &[i32]) -> AppResult<()> {
    own_uow!(app, |uow| delete_threads_in(&mut uow, tids).await?)
}

pub async fn delete_threads_in(uow: &mut Uow, tids: &[i32]) -> AppResult<()> {
    lock_threads(uow.conn(), tids).await?;
    let before = snapshot(uow.conn(), tids).await?;
    let pids = all_pids_of_threads(uow.conn(), tids).await?;
    adjust_user_postcounts(uow.conn(), &pids, -1).await?;
    adjust_user_threadcounts(uow.conn(), tids, -1).await?;
    delete_attachments_of_posts(uow, &pids).await?;
    let c = uow.conn();
    sqlx::query("DELETE FROM reportedcontent WHERE type = 'post' AND id = ANY($1)")
        .bind(&pids)
        .execute(&mut *c)
        .await?;
    // Redirects pointing at these threads go too.
    let redirect_tids: Vec<i32> =
        sqlx::query_scalar("SELECT tid FROM threads WHERE closed = ANY($1)")
            .bind(
                tids.iter()
                    .map(|t| format!("moved|{t}"))
                    .collect::<Vec<_>>(),
            )
            .fetch_all(&mut *c)
            .await?;
    sqlx::query("DELETE FROM threads WHERE tid = ANY($1) OR tid = ANY($2)")
        .bind(tids)
        .bind(&redirect_tids)
        .execute(&mut *c)
        .await?;
    let mut fids = Vec::new();
    for (_, fid, con) in before {
        apply_forum_delta(c, fid, Contrib::default().sub(con)).await?;
        if !fids.contains(&fid) {
            fids.push(fid);
        }
    }
    for f in fids {
        update_forum_lastpost(c, f).await?;
    }
    Ok(())
}

/// Delete attachment rows now; their files are removed by a job once this commits.
async fn delete_attachments_of_posts(uow: &mut Uow, pids: &[i32]) -> AppResult<()> {
    let files: Vec<(String, String)> = sqlx::query_as(
        "DELETE FROM attachments WHERE pid = ANY($1) RETURNING attachname, thumbnail",
    )
    .bind(pids)
    .fetch_all(uow.conn())
    .await?;
    let paths: Vec<String> = files
        .into_iter()
        .flat_map(|(a, t)| [a, t])
        .filter(|p| !p.is_empty())
        .collect();
    if !paths.is_empty() {
        uow.job(Job::DeleteFiles { paths });
    }
    Ok(())
}

/// Move threads to another forum, optionally leaving a redirect behind that expires after
/// `redirect_days` (0 = permanent, None = no redirect).
pub async fn move_threads(
    app: &App,
    tids: &[i32],
    to_fid: i32,
    redirect_days: Option<i64>,
) -> AppResult<()> {
    own_uow!(app, |uow| move_threads_in(
        &mut uow,
        tids,
        to_fid,
        redirect_days
    )
    .await?)
}

pub async fn move_threads_in(
    uow: &mut Uow,
    tids: &[i32],
    to_fid: i32,
    redirect_days: Option<i64>,
) -> AppResult<()> {
    let tx = uow.conn();
    lock_threads(tx, tids).await?;
    let before = snapshot(tx, tids).await?;
    let pids = all_pids_of_threads(tx, tids).await?;
    adjust_user_postcounts(tx, &pids, -1).await?;
    adjust_user_threadcounts(tx, tids, -1).await?;
    if let Some(days) = redirect_days {
        for &(tid, fid, _) in &before {
            if fid == to_fid {
                continue;
            }
            sqlx::query(
                "INSERT INTO threads (fid, subject, prefix, icon, uid, username, dateline, firstpost, lastpost, lastposter, lastposteruid, closed, visible, redirect_expires)
                 SELECT fid, subject, prefix, icon, uid, username, dateline, 0, lastpost, lastposter, lastposteruid, 'moved|' || tid, 1, $2
                 FROM threads WHERE tid = $1",
            )
            .bind(tid)
            .bind(if days > 0 { now() + days * 86400 } else { 0 })
            .execute(&mut *tx)
            .await?;
        }
    }
    sqlx::query("UPDATE threads SET fid = $2 WHERE tid = ANY($1)")
        .bind(tids)
        .bind(to_fid)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE posts SET fid = $2 WHERE tid = ANY($1)")
        .bind(tids)
        .bind(to_fid)
        .execute(&mut *tx)
        .await?;
    adjust_user_postcounts(tx, &pids, 1).await?;
    adjust_user_threadcounts(tx, tids, 1).await?;
    settle(tx, before).await?;
    Ok(())
}

/// Merge thread `from` into thread `into` (posts are moved; `from` is deleted).
pub async fn merge_threads(
    app: &App,
    into: i32,
    from: i32,
    subject: Option<&str>,
) -> AppResult<()> {
    if into == from {
        return Ok(());
    }
    own_uow!(app, |uow| merge_threads_in(&mut uow, into, from, subject)
        .await?)
}

pub async fn merge_threads_in(
    uow: &mut Uow,
    into: i32,
    from: i32,
    subject: Option<&str>,
) -> AppResult<()> {
    if into == from {
        return Ok(());
    }
    let tx = uow.conn();
    lock_threads(tx, &[into, from]).await?;
    let before = snapshot(tx, &[into, from]).await?;
    let pids = all_pids_of_threads(tx, &[into, from]).await?;
    adjust_user_postcounts(tx, &pids, -1).await?;
    adjust_user_threadcounts(tx, &[into, from], -1).await?;
    let into_fid: i32 = sqlx::query_scalar("SELECT fid FROM threads WHERE tid = $1")
        .bind(into)
        .fetch_one(&mut *tx)
        .await?;
    // The old first post of `from` becomes a regular post: its visibility follows its thread.
    sqlx::query("UPDATE posts SET visible = (SELECT visible FROM threads WHERE tid = $1) WHERE pid = (SELECT firstpost FROM threads WHERE tid = $1)")
        .bind(from)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE posts SET tid = $1, fid = $3 WHERE tid = $2")
        .bind(into)
        .bind(from)
        .bind(into_fid)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE threadsubscriptions SET tid = $1 WHERE tid = $2 AND uid NOT IN (SELECT uid FROM threadsubscriptions WHERE tid = $1)")
        .bind(into)
        .bind(from)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE threads SET closed = 'moved|' || $1::text WHERE closed = 'moved|' || $2::text",
    )
    .bind(into)
    .bind(from)
    .execute(&mut *tx)
    .await?;
    // polls: keep `into`'s poll, else adopt `from`'s
    sqlx::query("UPDATE threads SET poll = (SELECT poll FROM threads WHERE tid = $2) WHERE tid = $1 AND poll = 0").bind(into).bind(from).execute(&mut *tx).await?;
    sqlx::query("UPDATE polls SET tid = $1 WHERE tid = $2 AND pid = (SELECT poll FROM threads WHERE tid = $1)")
        .bind(into)
        .bind(from)
        .execute(&mut *tx)
        .await?;
    if let Some(s) = subject.filter(|s| !s.trim().is_empty()) {
        sqlx::query("UPDATE threads SET subject = $2 WHERE tid = $1")
            .bind(into)
            .bind(s.trim())
            .execute(&mut *tx)
            .await?;
    }
    // Remove `from` (its contribution is removed via settle since it no longer exists).
    let from_before = before.iter().find(|b| b.0 == from).cloned();
    sqlx::query("DELETE FROM threads WHERE tid = $1")
        .bind(from)
        .execute(&mut *tx)
        .await?;
    // Ensure the new first post of `into` has the thread's visibility.
    recount_thread(tx, into).await?;
    sqlx::query("UPDATE posts SET visible = (SELECT visible FROM threads WHERE tid = $1) WHERE pid = (SELECT firstpost FROM threads WHERE tid = $1)")
        .bind(into)
        .execute(&mut *tx)
        .await?;
    let into_before: Vec<_> = before.into_iter().filter(|b| b.0 == into).collect();
    if let Some((_, fid, con)) = from_before {
        apply_forum_delta(tx, fid, Contrib::default().sub(con)).await?;
        update_forum_lastpost(tx, fid).await?;
    }
    settle(tx, into_before).await?;
    adjust_user_postcounts(tx, &pids, 1).await?;
    adjust_user_threadcounts(tx, &[into], 1).await?;
    Ok(())
}

/// Split posts out of their thread into a new thread. Returns the new tid.
pub async fn split_posts(app: &App, pids: &[i32], subject: &str, to_fid: i32) -> AppResult<i32> {
    own_uow!(app, |uow| split_posts_in(&mut uow, pids, subject, to_fid)
        .await?)
}

pub async fn split_posts_in(
    uow: &mut Uow,
    pids: &[i32],
    subject: &str,
    to_fid: i32,
) -> AppResult<i32> {
    let tx = uow.conn();
    let old_tid: i32 = sqlx::query_scalar("SELECT tid FROM posts WHERE pid = $1")
        .bind(pids[0])
        .fetch_one(&mut *tx)
        .await?;
    lock_threads(tx, &[old_tid]).await?;
    let before = snapshot(tx, &[old_tid]).await?;
    adjust_user_postcounts(tx, pids, -1).await?;
    let first: (i32, String, i64) = sqlx::query_as("SELECT uid, username, dateline FROM posts WHERE pid = ANY($1) ORDER BY dateline, pid LIMIT 1")
        .bind(pids)
        .fetch_one(&mut *tx)
        .await?;
    let new_tid: i32 = sqlx::query_scalar(
        "INSERT INTO threads (fid, subject, uid, username, dateline, lastpost, visible) VALUES ($1, $2, $3, $4, $5, $5, 1) RETURNING tid",
    )
    .bind(to_fid)
    .bind(subject)
    .bind(first.0)
    .bind(&first.1)
    .bind(first.2)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE posts SET tid = $2, fid = $3 WHERE pid = ANY($1) AND tid = $4")
        .bind(pids)
        .bind(new_tid)
        .bind(to_fid)
        .bind(old_tid)
        .execute(&mut *tx)
        .await?;
    // First post of the new thread must be visible for the thread to be visible.
    recount_thread(tx, new_tid).await?;
    sqlx::query(
        "UPDATE posts SET visible = 1 WHERE pid = (SELECT firstpost FROM threads WHERE tid = $1)",
    )
    .bind(new_tid)
    .execute(&mut *tx)
    .await?;
    let mut all = before;
    all.push((new_tid, to_fid, Contrib::default()));
    settle(tx, all).await?;
    adjust_user_postcounts(tx, pids, 1).await?;
    adjust_user_threadcounts(tx, &[new_tid], 1).await?;
    Ok(new_tid)
}

/// Copy a thread (with all posts) to another forum. Returns new tid.
pub async fn copy_thread(app: &App, tid: i32, to_fid: i32) -> AppResult<i32> {
    own_uow!(app, |uow| copy_thread_in(&mut uow, tid, to_fid).await?)
}

pub async fn copy_thread_in(uow: &mut Uow, tid: i32, to_fid: i32) -> AppResult<i32> {
    let tx = uow.conn();
    lock_threads(tx, &[tid]).await?;
    let new_tid: i32 = sqlx::query_scalar(
        "INSERT INTO threads (fid, subject, prefix, icon, uid, username, dateline, lastpost, lastposter, lastposteruid, closed, sticky, visible, notes)
         SELECT $2, subject, prefix, icon, uid, username, dateline, lastpost, lastposter, lastposteruid, closed, sticky, visible, notes FROM threads WHERE tid = $1
         RETURNING tid",
    )
    .bind(tid)
    .bind(to_fid)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO posts (tid, replyto, fid, subject, icon, uid, username, dateline, message, ipaddress, includesig, smilieoff, edituid, edittime, editreason, visible)
         SELECT $2, 0, $3, subject, icon, uid, username, dateline, message, ipaddress, includesig, smilieoff, edituid, edittime, editreason, visible
         FROM posts WHERE tid = $1 ORDER BY dateline, pid",
    )
    .bind(tid)
    .bind(new_tid)
    .bind(to_fid)
    .execute(&mut *tx)
    .await?;
    settle(tx, vec![(new_tid, to_fid, Contrib::default())]).await?;
    let pids = all_pids_of_threads(tx, &[new_tid]).await?;
    adjust_user_postcounts(tx, &pids, 1).await?;
    adjust_user_threadcounts(tx, &[new_tid], 1).await?;
    Ok(new_tid)
}

/// Merge several posts of one thread into the earliest one. The thread and the posts are
/// locked first, so concurrent merges, edits and deletions of the same posts serialize.
pub async fn merge_posts(app: &App, pids: &[i32], sep: &str) -> AppResult<()> {
    own_uow!(app, |uow| merge_posts_in(&mut uow, pids, sep).await?)
}

pub async fn merge_posts_in(uow: &mut Uow, pids: &[i32], sep: &str) -> AppResult<()> {
    if pids.len() < 2 {
        return Ok(());
    }
    let locked = lock_posts(uow.conn(), pids).await?;
    if locked.len() != pids.len() {
        return Err(crate::error::AppError::user(
            "Some of the posts no longer exist.",
        ));
    }
    let posts: Vec<(i32, i32, String)> = sqlx::query_as(
        "SELECT pid, tid, message FROM posts WHERE pid = ANY($1) ORDER BY dateline, pid",
    )
    .bind(pids)
    .fetch_all(uow.conn())
    .await?;
    if posts.is_empty() || posts.iter().any(|p| p.1 != posts[0].1) {
        return Err(crate::error::AppError::user(
            "Posts must belong to the same thread.",
        ));
    }
    let target = posts[0].0;
    let combined = posts
        .iter()
        .map(|p| p.2.as_str())
        .collect::<Vec<_>>()
        .join(sep);
    let c = uow.conn();
    sqlx::query("UPDATE posts SET message = $2, parser_rev = -1 WHERE pid = $1")
        .bind(target)
        .bind(&combined)
        .execute(&mut *c)
        .await?;
    sqlx::query("UPDATE attachments SET pid = $1 WHERE pid = ANY($2)")
        .bind(target)
        .bind(pids)
        .execute(&mut *c)
        .await?;
    let rest: Vec<i32> = posts.iter().skip(1).map(|p| p.0).collect();
    delete_posts_in(uow, &rest).await
}

/// Remove attachments uploaded more than a day ago but never attached to a post (rows now,
/// files after commit). Returns how many.
pub async fn prune_orphaned_attachments(app: &App) -> AppResult<usize> {
    let mut uow = Uow::begin(app).await?;
    let files: Vec<(String, String)> = sqlx::query_as(
        "DELETE FROM attachments WHERE pid = 0 AND dateuploaded < $1 RETURNING attachname, thumbnail",
    )
    .bind(now() - 86400)
    .fetch_all(uow.conn())
    .await?;
    let n = files.len();
    let paths: Vec<String> = files
        .into_iter()
        .flat_map(|(a, t)| [a, t])
        .filter(|p| !p.is_empty())
        .collect();
    if !paths.is_empty() {
        uow.job(Job::DeleteFiles { paths });
    }
    uow.commit(app).await?;
    Ok(n)
}

/// Recalculate everything from scratch (ACP "Recount & Rebuild"). Batched per forum.
pub async fn rebuild_all_counters(app: &App) -> AppResult<()> {
    let tids: Vec<i32> = sqlx::query_scalar("SELECT tid FROM threads ORDER BY tid")
        .fetch_all(&app.db)
        .await?;
    for chunk in tids.chunks(500) {
        let mut tx = app.db.begin().await?;
        for &t in chunk {
            recount_thread(&mut tx, t).await?;
        }
        tx.commit().await?;
    }
    rebuild_forum_counters(app).await?;
    rebuild_user_counters(app).await?;
    Ok(())
}

pub async fn rebuild_forum_counters(app: &App) -> AppResult<()> {
    sqlx::query(
        "UPDATE forums f SET
            threads = COALESCE(s.threads, 0), posts = COALESCE(s.posts, 0),
            unapprovedthreads = COALESCE(s.uthreads, 0), unapprovedposts = COALESCE(s.uposts, 0),
            deletedthreads = COALESCE(s.dthreads, 0), deletedposts = COALESCE(s.dposts, 0)
         FROM (SELECT fx.fid,
                SUM(CASE WHEN t.visible = 1 THEN 1 ELSE 0 END) AS threads,
                SUM(CASE WHEN t.visible = 1 THEN t.replies + 1 ELSE 0 END) AS posts,
                SUM(CASE WHEN t.visible = 0 THEN 1 ELSE 0 END) AS uthreads,
                SUM(CASE WHEN t.visible = 1 THEN t.unapprovedposts WHEN t.visible = 0 THEN t.replies + t.unapprovedposts + t.deletedposts + 1 ELSE 0 END) AS uposts,
                SUM(CASE WHEN t.visible = -1 THEN 1 ELSE 0 END) AS dthreads,
                SUM(CASE WHEN t.visible = 1 THEN t.deletedposts WHEN t.visible = -1 THEN t.replies + t.unapprovedposts + t.deletedposts + 1 ELSE 0 END) AS dposts
               FROM forums fx LEFT JOIN threads t ON t.fid = fx.fid AND t.closed NOT LIKE 'moved|%'
               GROUP BY fx.fid) s
         WHERE f.fid = s.fid",
    )
    .execute(&app.db)
    .await?;
    let fids: Vec<i32> = sqlx::query_scalar("SELECT fid FROM forums")
        .fetch_all(&app.db)
        .await?;
    let mut c = app.db.acquire().await?;
    for f in fids {
        update_forum_lastpost(&mut c, f).await?;
    }
    Ok(())
}

pub async fn rebuild_user_counters(app: &App) -> AppResult<()> {
    sqlx::query(
        "UPDATE users u SET postnum = COALESCE(s.c, 0) FROM (SELECT u2.uid, (SELECT COUNT(*) FROM posts p JOIN threads t ON t.tid = p.tid JOIN forums f ON f.fid = p.fid
            WHERE p.uid = u2.uid AND p.visible = 1 AND t.visible = 1 AND f.usepostcounts) AS c FROM users u2) s WHERE u.uid = s.uid",
    )
    .execute(&app.db)
    .await?;
    sqlx::query(
        "UPDATE users u SET threadnum = COALESCE(s.c, 0) FROM (SELECT u2.uid, (SELECT COUNT(*) FROM threads t JOIN forums f ON f.fid = t.fid
            WHERE t.uid = u2.uid AND t.visible = 1 AND f.usethreadcounts AND t.closed NOT LIKE 'moved|%') AS c FROM users u2) s WHERE u.uid = s.uid",
    )
    .execute(&app.db)
    .await?;
    sqlx::query("UPDATE counters SET numusers = (SELECT COUNT(*) FROM users), lastuid = COALESCE((SELECT MAX(uid) FROM users WHERE NOT is_system), 0)")
        .execute(&app.db)
        .await?;
    sqlx::query("UPDATE counters SET lastusername = COALESCE((SELECT username FROM users WHERE uid = counters.lastuid), '')").execute(&app.db).await?;
    Ok(())
}

pub async fn log_moderator_action(
    app: &App,
    uid: i32,
    ip: &str,
    fid: i32,
    tid: i32,
    pid: i32,
    action: &str,
    data: serde_json::Value,
) {
    let r = match app.db.acquire().await {
        Ok(mut c) => log_moderator_action_in(&mut c, uid, ip, fid, tid, pid, action, data).await,
        Err(e) => Err(e.into()),
    };
    if let Err(e) = r {
        tracing::warn!("moderator log write failed: {e}");
    }
}

/// Record a moderator action as part of the caller's transaction.
pub async fn log_moderator_action_in(
    c: &mut PgConnection,
    uid: i32,
    ip: &str,
    fid: i32,
    tid: i32,
    pid: i32,
    action: &str,
    data: serde_json::Value,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO moderatorlog (uid, dateline, fid, tid, pid, action, data, ipaddress) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(uid)
    .bind(now())
    .bind(fid)
    .bind(tid)
    .bind(pid)
    .bind(action)
    .bind(data)
    .bind(crate::util::IpText(ip.to_string()))
    .execute(c)
    .await?;
    Ok(())
}

/// Compare denormalized counters with freshly computed values. Returns human-readable problems.
pub async fn check_counters(db: &sqlx::PgPool) -> anyhow::Result<Vec<String>> {
    let mut problems = vec![];
    let bad_threads: Vec<(i32, i32, i64, i32, i64, i32, i64)> = sqlx::query_as(
        "SELECT t.tid, t.replies, s.vis - (CASE WHEN fp.visible = 1 THEN 1 ELSE 0 END), t.unapprovedposts, s.unap - (CASE WHEN fp.visible = 0 THEN 1 ELSE 0 END),
                t.deletedposts, s.del - (CASE WHEN fp.visible = -1 THEN 1 ELSE 0 END)
         FROM threads t
         JOIN (SELECT tid, COUNT(*) FILTER (WHERE visible = 1) vis, COUNT(*) FILTER (WHERE visible = 0) unap, COUNT(*) FILTER (WHERE visible = -1) del
               FROM posts GROUP BY tid) s ON s.tid = t.tid
         JOIN posts fp ON fp.pid = t.firstpost
         WHERE t.closed NOT LIKE 'moved|%' AND (t.replies <> s.vis - (CASE WHEN fp.visible = 1 THEN 1 ELSE 0 END)
            OR t.unapprovedposts <> s.unap - (CASE WHEN fp.visible = 0 THEN 1 ELSE 0 END)
            OR t.deletedposts <> s.del - (CASE WHEN fp.visible = -1 THEN 1 ELSE 0 END))
         LIMIT 50",
    )
    .fetch_all(db)
    .await?;
    for t in bad_threads {
        problems.push(format!(
            "thread {}: replies {} (expected {}), unapproved {} ({}), deleted {} ({})",
            t.0, t.1, t.2, t.3, t.4, t.5, t.6
        ));
    }
    let orphan_first: Vec<i32> = sqlx::query_scalar(
        "SELECT t.tid FROM threads t WHERE t.closed NOT LIKE 'moved|%' AND NOT EXISTS (SELECT 1 FROM posts p WHERE p.pid = t.firstpost AND p.tid = t.tid) LIMIT 50",
    )
    .fetch_all(db)
    .await?;
    for t in orphan_first {
        problems.push(format!("thread {t}: firstpost missing"));
    }
    let bad_first_vis: Vec<i32> = sqlx::query_scalar(
        "SELECT t.tid FROM threads t JOIN posts p ON p.pid = t.firstpost WHERE p.visible <> t.visible AND t.closed NOT LIKE 'moved|%' LIMIT 50",
    )
    .fetch_all(db)
    .await?;
    for t in bad_first_vis {
        problems.push(format!(
            "thread {t}: first post visibility differs from thread"
        ));
    }
    let bad_forums: Vec<(i32, i32, i64, i32, i64, i32, i64, i32, i64)> = sqlx::query_as(
        "SELECT f.fid, f.threads, COALESCE(s.threads, 0), f.posts, COALESCE(s.posts, 0), f.unapprovedposts, COALESCE(s.uposts, 0), f.deletedposts, COALESCE(s.dposts, 0)
         FROM forums f LEFT JOIN (
            SELECT t.fid,
                COUNT(*) FILTER (WHERE t.visible = 1) AS threads,
                SUM(CASE WHEN t.visible = 1 THEN t.replies + 1 ELSE 0 END) AS posts,
                SUM(CASE WHEN t.visible = 1 THEN t.unapprovedposts WHEN t.visible = 0 THEN t.replies + t.unapprovedposts + t.deletedposts + 1 ELSE 0 END) AS uposts,
                SUM(CASE WHEN t.visible = 1 THEN t.deletedposts WHEN t.visible = -1 THEN t.replies + t.unapprovedposts + t.deletedposts + 1 ELSE 0 END) AS dposts
            FROM threads t WHERE t.closed NOT LIKE 'moved|%' GROUP BY t.fid) s ON s.fid = f.fid
         WHERE f.threads <> COALESCE(s.threads, 0) OR f.posts <> COALESCE(s.posts, 0) OR f.unapprovedposts <> COALESCE(s.uposts, 0) OR f.deletedposts <> COALESCE(s.dposts, 0)",
    )
    .fetch_all(db)
    .await?;
    for f in bad_forums {
        problems.push(format!("forum {}: threads {} ({}), posts {} ({}), unapproved posts {} ({}), deleted posts {} ({})", f.0, f.1, f.2, f.3, f.4, f.5, f.6, f.7, f.8));
    }
    let bad_users: Vec<(i32, i32, i64)> = sqlx::query_as(
        "SELECT u.uid, u.postnum, COALESCE(s.c, 0) FROM users u LEFT JOIN (
            SELECT p.uid, COUNT(*) c FROM posts p JOIN threads t ON t.tid = p.tid JOIN forums f ON f.fid = p.fid
            WHERE p.visible = 1 AND t.visible = 1 AND f.usepostcounts GROUP BY p.uid) s ON s.uid = u.uid
         WHERE u.postnum <> COALESCE(s.c, 0) LIMIT 50",
    )
    .fetch_all(db)
    .await?;
    for u in bad_users {
        problems.push(format!("user {}: postnum {} (expected {})", u.0, u.1, u.2));
    }
    Ok(problems)
}

/// Automod calls this inside the same transaction as its durable audit record.
/// Caller holds the thread and post locks. Only public ↔ pending transitions are allowed.
pub async fn automod_visibility(
    c: &mut PgConnection,
    pid: i32,
    tid: i32,
    first: bool,
    vis: i16,
) -> AppResult<()> {
    let before = snapshot(c, &[tid]).await?;
    let pids = if first {
        all_pids_of_threads(c, &[tid]).await?
    } else {
        vec![pid]
    };
    adjust_user_postcounts(c, &pids, -1).await?;
    if first {
        adjust_user_threadcounts(c, &[tid], -1).await?;
        sqlx::query("UPDATE threads SET visible = $2 WHERE tid = $1")
            .bind(tid)
            .bind(vis)
            .execute(&mut *c)
            .await?;
    }
    sqlx::query("UPDATE posts SET visible = $2 WHERE pid = $1")
        .bind(pid)
        .bind(vis)
        .execute(&mut *c)
        .await?;
    adjust_user_postcounts(c, &pids, 1).await?;
    if first {
        adjust_user_threadcounts(c, &[tid], 1).await?;
    }
    settle(c, before).await
}
