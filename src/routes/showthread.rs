//! Thread display.

use crate::ctx::Ctx;
use crate::error::{AppError, AppResult};
use crate::models::{Forum, POST_COLUMNS, Post, Thread};
use crate::perms::{ForumPerms, ModPerms};
use crate::render::{self, AuthorInfo};
use crate::routes::forumdisplay::{breadcrumb, forum_jump, prefix_html};
use crate::templates::{url_forum, url_thread};
use crate::util::{self, leading_id, now};
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Redirect, Response};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Deserialize, Default)]
pub struct StQuery {
    pub page: Option<i64>,
    pub highlight: Option<String>,
}

/// Flag chips for the authors on a page, for staff only (see `member_file`): restrictions,
/// warnings and the like, plus "New".
async fn staff_flags(
    ctx: &Ctx,
    posts: &[PostView],
) -> AppResult<Option<std::collections::HashMap<i32, Vec<crate::member_file::Flag>>>> {
    if !ctx.can(crate::domain::staff::Cap::ModCp) {
        return Ok(None);
    }
    let mut uids: Vec<i32> = posts.iter().map(|p| p.uid).filter(|u| *u > 0).collect();
    uids.sort_unstable();
    uids.dedup();
    let mut flags =
        crate::member_file::flags_for(ctx, &uids, crate::member_file::View::of(ctx)).await?;
    for v in flags.values_mut() {
        v.retain(|f| {
            matches!(
                f.level,
                crate::member_file::Level::Red | crate::member_file::Level::Orange
            ) || f.key == "new"
        });
    }
    flags.retain(|_, v| !v.is_empty());
    Ok(Some(flags))
}

#[derive(Serialize, Clone, Debug)]
pub struct PostView {
    pub pid: i32,
    pub tid: i32,
    pub number: i64,
    pub subject: String,
    pub icon: Option<String>,
    pub uid: i32,
    pub username: String,
    pub author: Option<AuthorInfo>,
    pub dateline: i64,
    pub message: String,
    pub visible: i16,
    pub edited: Option<(String, i64, String, i32)>,
    pub ip: Option<String>,
    pub attachments: Vec<render::AttachmentInfo>,
    pub reactions: Vec<(String, String, i64, bool)>,
    pub can_edit: bool,
    pub can_delete: bool,
    pub can_quote: bool,
    pub can_report: bool,
    pub can_warn: bool,
    pub can_react: bool,
    pub ignored: bool,
    pub is_first: bool,
    pub has_history: bool,
}

pub async fn load_thread(ctx: &Ctx, tid: i32) -> AppResult<Thread> {
    sqlx::query_as::<_, Thread>(&format!(
        "SELECT {} FROM threads WHERE tid = $1",
        crate::models::THREAD_COLUMNS
    ))
    .bind(tid)
    .fetch_optional(&ctx.app.db)
    .await?
    .ok_or_else(|| AppError::not_found("thread"))
}

/// Load a thread and verify the viewer may see it. Returns (thread, forum, forum perms).
pub async fn check_thread(ctx: &Ctx, tid: i32) -> AppResult<(Thread, Forum, ForumPerms)> {
    let t = load_thread(ctx, tid).await?;
    let (forum, fp) = ctx.check_forum(t.fid)?;
    let forum = forum.clone();
    if !ctx.access().can_read_thread(t.fid, t.uid, ctx.uid()) {
        return Err(AppError::no_perm());
    }
    let states = ctx.visible_states(t.fid);
    let own = t.uid == ctx.uid() && ctx.uid() > 0;
    if !states.contains(&t.visible) && !(t.visible == 0 && own) {
        return Err(AppError::not_found("thread"));
    }
    Ok((t, forum, fp))
}

pub fn edit_allowed(
    ctx: &Ctx,
    post: &Post,
    thread: &Thread,
    fp: &ForumPerms,
    mp: &Option<ModPerms>,
) -> bool {
    if let Some(m) = mp
        && m.caneditposts
    {
        return true;
    }
    if ctx.uid() == 0
        || post.uid != ctx.uid()
        || !fp.caneditposts
        || thread.is_closed()
        || post.visible == -1
    {
        return false;
    }
    let limit = if ctx.perms.edittimelimit > 0 {
        ctx.perms.edittimelimit as i64
    } else {
        ctx.settings().int("edittimelimit")
    };
    limit == 0 || post.dateline + limit * 60 > now()
}

pub fn delete_allowed(
    ctx: &Ctx,
    post: &Post,
    thread: &Thread,
    fp: &ForumPerms,
    mp: &Option<ModPerms>,
) -> bool {
    if let Some(m) = mp {
        // Deleting a thread's first post deletes the thread, so it needs the thread permission.
        let allowed = if post.pid == thread.firstpost {
            m.candeletethreads || m.cansoftdeletethreads
        } else {
            m.candeleteposts || m.cansoftdeleteposts
        };
        if allowed {
            return true;
        }
    }
    if ctx.uid() == 0 || post.uid != ctx.uid() || thread.is_closed() || post.visible == -1 {
        return false;
    }
    if post.pid == thread.firstpost {
        fp.candeletethreads
    } else {
        fp.candeleteposts
    }
}

/// Build template data for a page of posts.
pub async fn build_postbits(
    ctx: &Ctx,
    thread: &Thread,
    fp: &ForumPerms,
    mp: &Option<ModPerms>,
    posts: Vec<Post>,
    start_number: i64,
    highlight: &[String],
) -> AppResult<Vec<PostView>> {
    let uids: Vec<i32> = posts.iter().map(|p| p.uid).collect();
    let pids: Vec<i32> = posts.iter().map(|p| p.pid).collect();
    let authors = render::load_authors(ctx, &uids).await?;
    let atts = render::load_attachments(ctx, &pids).await?;
    let s = ctx.settings();
    let reactions_enabled = s.bool("enablereactions");
    let rtypes = ctx.cache.reaction_types();
    let mut reactions: HashMap<i32, Vec<(String, i64, bool)>> = HashMap::new();
    if reactions_enabled && !pids.is_empty() {
        let rows: Vec<(i32, String, i64, bool)> = sqlx::query_as(
            "SELECT pid, kind, COUNT(*), bool_or(uid = $2) FROM reactions WHERE pid = ANY($1) GROUP BY pid, kind",
        )
        .bind(&pids)
        .bind(ctx.uid())
        .fetch_all(&ctx.app.db)
        .await?;
        for (pid, kind, n, mine) in rows {
            reactions.entry(pid).or_default().push((kind, n, mine));
        }
    }
    let history: Vec<i32> = if s.bool("keepedithistory") && !pids.is_empty() {
        sqlx::query_scalar("SELECT DISTINCT pid FROM post_edits WHERE pid = ANY($1)")
            .bind(&pids)
            .fetch_all(&ctx.app.db)
            .await?
    } else {
        vec![]
    };
    // editor names
    let edit_uids: Vec<i32> = posts
        .iter()
        .filter(|p| p.edituid > 0)
        .map(|p| p.edituid)
        .collect();
    let editors: HashMap<i32, String> = if edit_uids.is_empty() {
        HashMap::new()
    } else {
        sqlx::query_as::<_, (i32, String)>("SELECT uid, username FROM users WHERE uid = ANY($1)")
            .bind(&edit_uids)
            .fetch_all(&ctx.app.db)
            .await?
            .into_iter()
            .collect()
    };
    let thumbs = s.get("attachthumbnails").to_string();
    let ignore: Vec<i32> = ctx
        .user
        .as_ref()
        .map(|u| u.ignorelist.clone())
        .unwrap_or_default();
    let mut stale = Vec::new();
    let mut out = Vec::with_capacity(posts.len());
    let can_reply_ctx = ctx.uid() > 0;
    for (i, p) in posts.into_iter().enumerate() {
        let mut html = if p.visible == -1 && mp.as_ref().map(|m| !m.canviewdeleted).unwrap_or(true)
        {
            String::new()
        } else {
            render::post_html(ctx, &p, &mut stale)
        };
        let post_atts: Vec<render::AttachmentInfo> = atts
            .get(&p.pid)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|a| a.visible || mp.is_some() || p.uid == ctx.uid())
            .collect();
        let (h2, inline) = render::inline_attachments(&html, &post_atts, &thumbs);
        html = h2;
        if !highlight.is_empty() {
            html = crate::parser::highlight(&html, highlight);
        }
        let remaining: Vec<render::AttachmentInfo> = post_atts
            .into_iter()
            .filter(|a| !inline.contains(&a.aid))
            .collect();
        let show_edit = p.edituid > 0
            && s.bool("showeditedby")
            && (s.bool("showeditedbyadmin") || !ctx.cache.group(1).is_some_and(|_| false));
        let edited = if show_edit {
            Some((
                editors.get(&p.edituid).cloned().unwrap_or_default(),
                p.edittime,
                p.editreason.clone(),
                p.edituid,
            ))
        } else {
            None
        };
        let rx = reactions.remove(&p.pid).unwrap_or_default();
        let rx_view: Vec<(String, String, i64, bool)> = rtypes
            .iter()
            .filter_map(|(k, emoji)| {
                rx.iter()
                    .find(|r| &r.0 == k)
                    .map(|r| (k.clone(), emoji.clone(), r.1, r.2))
            })
            .collect();
        out.push(PostView {
            pid: p.pid,
            tid: p.tid,
            number: start_number + i as i64,
            subject: p.subject.clone(),
            icon: ctx.cache.icon(p.icon).map(|i| i.path.clone()),
            uid: p.uid,
            username: p.username.clone(),
            author: authors.get(&p.uid).cloned(),
            dateline: p.dateline,
            message: html,
            visible: p.visible,
            edited,
            ip: mp
                .as_ref()
                .filter(|m| m.canviewips)
                .map(|_| p.ipaddress.0.clone()),
            attachments: remaining,
            reactions: rx_view,
            can_edit: edit_allowed(ctx, &p, thread, fp, mp),
            can_delete: delete_allowed(ctx, &p, thread, fp, mp),
            can_quote: can_reply_ctx || fp.canpostreplys,
            can_report: ctx.uid() > 0 && p.uid != ctx.uid(),
            can_warn: ctx.perms.canwarnusers
                && p.uid > 0
                && p.uid != ctx.uid()
                && s.bool("enablewarningsystem"),
            can_react: reactions_enabled
                && ctx.perms.canreact
                && ctx.uid() > 0
                && p.uid != ctx.uid(),
            ignored: ignore.contains(&p.uid),
            is_first: p.pid == thread.firstpost,
            has_history: history.contains(&p.pid) && (mp.is_some() || p.uid == ctx.uid()),
        });
    }
    render::store_parsed(ctx, stale);
    Ok(out)
}

fn posts_per_page(ctx: &Ctx) -> i64 {
    ctx.user
        .as_ref()
        .map(|u| u.ppp as i64)
        .filter(|p| *p > 0)
        .unwrap_or_else(|| ctx.settings().int("postsperpage").max(1))
}

pub async fn showthread(
    ctx: Ctx,
    Path(seg): Path<String>,
    Query(q): Query<StQuery>,
) -> AppResult<Response> {
    let tid = leading_id(&seg).ok_or_else(|| AppError::not_found("thread"))?;
    let (thread, forum, fp) = check_thread(&ctx, tid).await?;
    if let Some(to) = thread.moved_to() {
        return Ok(Redirect::permanent(&url_thread(to as i64, None)).into_response());
    }
    ctx.set_location(thread.fid, tid);
    if forum.style > 0
        && (forum.overridestyle || ctx.user.as_ref().map(|u| u.style == 0).unwrap_or(true))
    {
        ctx.set_theme(forum.style);
    }
    let s = ctx.settings();
    let mp = ctx.mod_perms(thread.fid);
    let states = ctx.listed_states(thread.fid);
    let ppp = posts_per_page(&ctx);
    let mut total = thread.replies as i64 + 1;
    if states.contains(&0) {
        total += thread.unapprovedposts as i64;
    }
    if states.contains(&-1) {
        total += thread.deletedposts as i64;
    }
    // The author can see their own unapproved posts; count them too, or the last pages vanish.
    let own_unapproved = ctx.uid() > 0 && !states.contains(&0);
    if own_unapproved && thread.unapprovedposts > 0 {
        total += sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM posts WHERE tid = $1 AND visible = 0 AND uid = $2",
        )
        .bind(tid)
        .bind(ctx.uid())
        .fetch_one(&ctx.app.db)
        .await?;
    }
    let url = url_thread(tid as i64, Some(&thread.subject));
    let pagination = util::paginate(
        total,
        ppp,
        util::clamp_page(q.page),
        &format!("{url}?page={{page}}"),
    );
    let offset = (pagination.page - 1) * ppp;
    // Deep pages of huge threads: walk the (tid, dateline) index from the end.
    let reverse = !own_unapproved && offset > total / 2;
    let (order_sql, limit, off) = if reverse {
        let end = (offset + ppp).min(total);
        (
            "dateline DESC, pid DESC",
            (end - offset).max(0),
            (total - end).max(0),
        )
    } else {
        ("dateline, pid", ppp, offset)
    };
    let mut posts: Vec<Post> = sqlx::query_as(&format!(
        "SELECT {POST_COLUMNS} FROM posts WHERE tid = $1 AND (visible = ANY($2) OR ($5 AND visible = 0 AND uid = $6))
         ORDER BY {order_sql} LIMIT $3 OFFSET $4"
    ))
    .bind(tid)
    .bind(&states)
    .bind(limit)
    .bind(off)
    .bind(own_unapproved)
    .bind(ctx.uid())
    .fetch_all(&ctx.app.db)
    .await?;
    if reverse {
        posts.reverse();
    }
    let highlight: Vec<String> = q
        .highlight
        .as_deref()
        .map(|h| {
            h.split_whitespace()
                .map(|w| w.to_string())
                .take(10)
                .collect()
        })
        .unwrap_or_default();
    let last_pid = posts.last().map(|p| p.pid).unwrap_or(0);
    let postbits = build_postbits(&ctx, &thread, &fp, &mp, posts, offset + 1, &highlight).await?;

    // Poll
    let poll = if thread.poll > 0 {
        crate::routes::polls::load_poll_view(&ctx, &thread, &fp).await?
    } else {
        None
    };

    // Mark read + view count (batched). Speculative prefetches don't count as a visit.
    let prefetch = ctx.is_prefetch();
    if !prefetch {
        *ctx.app.thread_views.entry(tid).or_insert(0) += 1;
    }
    if ctx.uid() > 0 && !prefetch {
        let app = ctx.app.clone();
        let (uid, fid) = (ctx.uid(), thread.fid);
        let readcut = now() - s.int("threadreadcut").max(1) * 86400;
        tokio::spawn(async move {
            let t = now();
            let _ = sqlx::query("INSERT INTO threadsread (tid, uid, dateline) VALUES ($1, $2, $3) ON CONFLICT (uid, tid) DO UPDATE SET dateline = $3")
                .bind(tid)
                .bind(uid)
                .bind(t)
                .execute(&app.db)
                .await;
            // If no unread threads remain in the forum, mark the forum read.
            let fr: i64 =
                sqlx::query_scalar("SELECT dateline FROM forumsread WHERE uid = $1 AND fid = $2")
                    .bind(uid)
                    .bind(fid)
                    .fetch_optional(&app.db)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(0);
            let unread: Option<i32> = sqlx::query_scalar(
                "SELECT t.tid FROM threads t LEFT JOIN threadsread tr ON tr.tid = t.tid AND tr.uid = $2
                 WHERE t.fid = $1 AND t.visible = 1 AND t.lastpost > $3 AND (tr.dateline IS NULL OR tr.dateline < t.lastpost) LIMIT 1",
            )
            .bind(fid)
            .bind(uid)
            .bind(fr.max(readcut))
            .fetch_optional(&app.db)
            .await
            .ok()
            .flatten();
            if unread.is_none() {
                let _ = sqlx::query("INSERT INTO forumsread (fid, uid, dateline) VALUES ($1, $2, $3) ON CONFLICT (uid, fid) DO UPDATE SET dateline = $3")
                    .bind(fid)
                    .bind(uid)
                    .bind(t)
                    .execute(&app.db)
                    .await;
            }
        });
    }

    let subscription: Option<i16> = if ctx.uid() > 0 {
        sqlx::query_scalar(
            "SELECT notification FROM threadsubscriptions WHERE uid = $1 AND tid = $2",
        )
        .bind(ctx.uid())
        .bind(tid)
        .fetch_optional(&ctx.app.db)
        .await?
    } else {
        None
    };
    let my_rating: Option<i16> = if ctx.uid() > 0 && thread.numratings > 0 {
        sqlx::query_scalar("SELECT rating FROM threadratings WHERE tid = $1 AND uid = $2")
            .bind(tid)
            .bind(ctx.uid())
            .fetch_optional(&ctx.app.db)
            .await?
    } else {
        None
    };
    let similar = if s.bool("showsimilarthreads") {
        let rows = related_threads(&ctx, &thread).await;
        // Filter by the viewer's permissions (the cache is shared by everyone).
        rows.as_array()
            .map(|a| {
                a.iter()
                    .filter(|r| {
                        let f = r["fid"].as_i64().unwrap_or(0) as i32;
                        ctx.access().forum(f).is_ok_and(|a| a.threads == crate::domain::access::Threads::All)
                    })
                    .take(s.int("similarlimit").max(1) as usize)
                    .map(|r| {
                        let f = r["fid"].as_i64().unwrap_or(0);
                        minijinja::context! { url => url_thread(r["tid"].as_i64().unwrap_or(0), r["subject"].as_str()), subject => r["subject"].as_str(), replies => r["replies"].as_i64(),
                            lastpost => r["lastpost"].as_i64(), lastposter => r["lastposter"].as_str(), forum_name => ctx.cache.forum(f as i32).map(|x| x.name.clone()), forum_url => url_forum(f, None) }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };

    let can_reply = fp.canpostreplys
        && ctx.perms.canpostreplys
        && forum.open
        && (!thread.is_closed() || mp.as_ref().map(|m| m.canpostclosedthreads).unwrap_or(false))
        && (!fp.canonlyreplyownthreads || thread.uid == ctx.uid());
    let custom_tools: Vec<(i32, String, String)> = if mp.is_some() {
        sqlx::query_as::<_, (i32, String, String, Vec<i32>, Vec<i32>)>(
            "SELECT tid, name, type::text, forums, groups FROM modtools ORDER BY name",
        )
        .fetch_all(&ctx.app.db)
        .await?
        .into_iter()
        .filter(|(_, _, _, f, g)| {
            (f.is_empty() || f.iter().any(|x| forum.parentlist.contains(x)))
                && (g.is_empty() || g.iter().any(|x| ctx.groups.contains(x)))
        })
        .map(|(t, n, ty, _, _)| (t, n, ty))
        .collect()
    } else {
        vec![]
    };
    let draft = if ctx.uid() > 0 {
        sqlx::query_as::<_, (i32, String)>("SELECT did, message FROM drafts WHERE uid = $1 AND tid = $2 ORDER BY dateline DESC LIMIT 1")
            .bind(ctx.uid())
            .bind(tid)
            .fetch_optional(&ctx.app.db)
            .await?
    } else {
        None
    };
    let description = postbits
        .first()
        .map(|p| util::truncate_chars(&strip_tags(&p.message), 160))
        .unwrap_or_default();
    let captcha =
        if ctx.uid() == 0 && can_reply && s.bool("guestcaptcha") && s.get("captchaimage") == "1" {
            Some(crate::routes::captcha::new_captcha(&ctx).await?)
        } else {
            // A page with a (single-use) captcha must not be shared between guests.
            ctx.allow_guest_cache(&[format!("thread:{tid}")]);
            None
        };
    ctx.render(
        "showthread.html",
        minijinja::context! {
            title => &thread.subject,
            meta_description => description,
            thread => &thread,
            thread_url => &url,
            prefix_html => prefix_html(&ctx, thread.prefix),
            forum => &forum,
            forum_url => url_forum(forum.fid as i64, Some(&forum.name)),
            breadcrumb => breadcrumb(&ctx, forum.fid),
            staff_flags => staff_flags(&ctx, &postbits).await?,
            posts => postbits,
            pagination => pagination,
            poll => poll,
            fperms => &fp,
            modperms => &mp,
            is_mod => mp.is_some(),
            can_reply => can_reply,
            subscription => subscription,
            rating => if thread.numratings > 0 { thread.totalratings as f64 / thread.numratings as f64 } else { 0.0 },
            my_rating => my_rating,
            can_rate => forum.allowtratings && fp.canratethreads && ctx.uid() > 0 && my_rating.is_none() && thread.uid != ctx.uid() && s.bool("allowthreadratings"),
            similar => similar,
            forumjump => forum_jump(&ctx),
            last_pid => last_pid,
            custom_tools => custom_tools,
            smilies => crate::routes::posting::clickable_smilies(&ctx),
            draft => draft,
            quickreply => s.bool("quickreply") && ctx.user.as_ref().map(|u| u.showquickreply).unwrap_or(true),
            live => s.bool("livethreadupdates"),
            reaction_types => ctx.cache.reaction_types(),
            captcha => captcha,
        },
    )
    .await
}

/// How long a page waits for related threads that aren't cached yet. A slower lookup finishes
/// in the background and fills the cache for the next view.
const RELATED_WAIT: Duration = Duration::from_millis(150);

/// Related threads for `thread`, best first, for every viewer (filter by permission before
/// showing). Cached per thread; at most a few lookups run at once, so crawlers walking cold
/// threads can't pile work onto the database.
async fn related_threads(ctx: &Ctx, thread: &Thread) -> serde_json::Value {
    if let Some(v) = ctx.app.similar_cache.get(&thread.tid) {
        return v;
    }
    let app = ctx.app.clone();
    let (tid, fid, firstpost) = (thread.tid, thread.fid, thread.firstpost);
    let task = tokio::spawn(async move {
        let Ok(_permit) = app.related_sem.try_acquire() else {
            return None;
        };
        // Another view may have filled it while this one waited for the permit.
        if let Some(v) = app.similar_cache.get(&tid) {
            return Some(v);
        }
        let v = match find_related(&app.db, tid, fid, firstpost).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("related threads for {tid}: {e}");
                return None;
            }
        };
        app.similar_cache.insert(tid, v.clone());
        Some(v)
    });
    match tokio::time::timeout(RELATED_WAIT, task).await {
        Ok(Ok(Some(v))) => v,
        _ => serde_json::Value::Array(vec![]),
    }
}

/// Threads whose first post shares the most distinctive words with this thread's first post
/// (subject words first, then the most repeated words of the message), ranked by full-text
/// relevance with a bonus for the same forum; topped up with the forum's recently active threads.
pub async fn find_related(
    db: &sqlx::PgPool,
    tid: i32,
    fid: i32,
    firstpost: i32,
) -> AppResult<serde_json::Value> {
    const WANT: i64 = 20;
    let mut tx = db.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '2s'")
        .execute(&mut *tx)
        .await?;
    // The first post's lexemes are already stemmed with stop words removed; quoting them keeps
    // them verbatim in a 'simple' query.
    let query: Option<String> = sqlx::query_scalar(
        "SELECT string_agg(quote_literal(lexeme), ' | ') FROM (
             SELECT u.lexeme FROM posts p, unnest(p.search_tsv) u
             WHERE p.pid = $1 AND length(u.lexeme) > 2 AND u.lexeme !~ '^[0-9]+$'
             ORDER BY ('A' = ANY(u.weights)) DESC, array_length(u.positions, 1) DESC NULLS LAST, u.lexeme
             LIMIT 10) terms",
    )
    .bind(firstpost)
    .fetch_one(&mut *tx)
    .await?;
    let mut rows: Vec<(i32, String, i32, i64, String, i32)> = vec![];
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        rows = sqlx::query_as(
            "SELECT t.tid, t.subject, t.replies, t.lastpost, t.lastposter, t.fid
             FROM to_tsquery('simple', $1) q, posts p JOIN threads t ON t.firstpost = p.pid
             WHERE p.search_tsv @@ q AND t.tid <> $2 AND t.visible = 1 AND t.closed NOT LIKE 'moved|%'
             ORDER BY ts_rank(p.search_tsv, q) * CASE WHEN t.fid = $3 THEN 1.5 ELSE 1 END DESC, t.lastpost DESC
             LIMIT $4",
        )
        .bind(&q)
        .bind(tid)
        .bind(fid)
        .bind(WANT)
        .fetch_all(&mut *tx)
        .await
        .unwrap_or_default();
    }
    if (rows.len() as i64) < WANT {
        // A failed or slow search above aborted the transaction; the top-up gets its own.
        drop(tx);
        let more: Vec<(i32, String, i32, i64, String, i32)> = sqlx::query_as(
            "SELECT tid, subject, replies, lastpost, lastposter, fid FROM threads
             WHERE fid = $1 AND visible = 1 AND tid <> $2 AND closed NOT LIKE 'moved|%'
             ORDER BY lastpost DESC LIMIT $3",
        )
        .bind(fid)
        .bind(tid)
        .bind(WANT)
        .fetch_all(db)
        .await?;
        for r in more {
            if (rows.len() as i64) < WANT && !rows.iter().any(|x| x.0 == r.0) {
                rows.push(r);
            }
        }
    }
    Ok(serde_json::Value::Array(
        rows.into_iter()
            .map(|(t, sub, r, lp, lpn, f)| serde_json::json!({"tid": t, "subject": sub, "replies": r, "lastpost": lp, "lastposter": lpn, "fid": f}))
            .collect(),
    ))
}

fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// Redirect to the page containing a post.
pub async fn goto_post(ctx: Ctx, Path(pid): Path<i32>) -> AppResult<Response> {
    let p: Option<(i32, i64, i32)> =
        sqlx::query_as("SELECT tid, dateline, pid FROM posts WHERE pid = $1")
            .bind(pid)
            .fetch_optional(&ctx.app.db)
            .await?;
    let (tid, dateline, _) = p.ok_or_else(|| AppError::not_found("post"))?;
    let (thread, _, _) = check_thread(&ctx, tid).await?;
    let states = ctx.listed_states(thread.fid);
    let before: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM posts WHERE tid = $1 AND (visible = ANY($2) OR ($5 AND visible = 0 AND uid = $6))
         AND (dateline < $3 OR (dateline = $3 AND pid < $4))",
    )
    .bind(tid)
    .bind(&states)
    .bind(dateline)
    .bind(pid)
    .bind(ctx.uid() > 0 && !states.contains(&0))
    .bind(ctx.uid())
    .fetch_one(&ctx.app.db)
    .await?;
    let page = before / posts_per_page(&ctx) + 1;
    let url = url_thread(tid as i64, Some(&thread.subject));
    let to = if page > 1 {
        format!("{url}?page={page}#pid{pid}")
    } else {
        format!("{url}#pid{pid}")
    };
    Ok(Redirect::to(&to).into_response())
}

pub async fn lastpost(ctx: Ctx, Path(tid): Path<i32>) -> AppResult<Response> {
    let (thread, _, _) = check_thread(&ctx, tid).await?;
    if let Some(to) = thread.moved_to() {
        return Ok(Redirect::to(&format!("/thread/{to}/lastpost")).into_response());
    }
    let states = ctx.listed_states(thread.fid);
    let pid: Option<i32> = sqlx::query_scalar("SELECT pid FROM posts WHERE tid = $1 AND visible = ANY($2) ORDER BY dateline DESC, pid DESC LIMIT 1")
        .bind(tid)
        .bind(&states)
        .fetch_optional(&ctx.app.db)
        .await?;
    match pid {
        Some(p) => goto_post(ctx, Path(p)).await,
        None => Ok(Redirect::to(&url_thread(tid as i64, None)).into_response()),
    }
}

pub async fn newpost(ctx: Ctx, Path(tid): Path<i32>) -> AppResult<Response> {
    let (thread, _, _) = check_thread(&ctx, tid).await?;
    let since: i64 = if ctx.uid() > 0 {
        let r: Option<i64> =
            sqlx::query_scalar("SELECT dateline FROM threadsread WHERE uid = $1 AND tid = $2")
                .bind(ctx.uid())
                .bind(tid)
                .fetch_optional(&ctx.app.db)
                .await?;
        let fr: Option<i64> =
            sqlx::query_scalar("SELECT dateline FROM forumsread WHERE uid = $1 AND fid = $2")
                .bind(ctx.uid())
                .bind(thread.fid)
                .fetch_optional(&ctx.app.db)
                .await?;
        r.unwrap_or(0).max(fr.unwrap_or(0))
    } else {
        0
    };
    let states = ctx.listed_states(thread.fid);
    let pid: Option<i32> = sqlx::query_scalar(
        "SELECT pid FROM posts WHERE tid = $1 AND visible = ANY($2) AND dateline > $3 ORDER BY dateline, pid LIMIT 1",
    )
    .bind(tid)
    .bind(&states)
    .bind(since)
    .fetch_optional(&ctx.app.db)
    .await?;
    match pid {
        Some(p) => goto_post(ctx, Path(p)).await,
        None => lastpost(ctx, Path(tid)).await,
    }
}

pub async fn printthread(ctx: Ctx, Path(tid): Path<i32>) -> AppResult<Response> {
    let (thread, forum, fp) = check_thread(&ctx, tid).await?;
    let mp = ctx.mod_perms(thread.fid);
    let posts: Vec<Post> = sqlx::query_as(&format!(
        "SELECT {POST_COLUMNS} FROM posts WHERE tid = $1 AND visible = 1 ORDER BY dateline, pid LIMIT 2000"
    ))
    .bind(tid)
    .fetch_all(&ctx.app.db)
    .await?;
    let postbits = build_postbits(&ctx, &thread, &fp, &mp, posts, 1, &[]).await?;
    ctx.render("printthread.html", minijinja::context! { title => &thread.subject, thread => &thread, forum => &forum, posts => postbits }).await
}

/// HTML fragment of posts newer than `pid` (used for live updates and "load new replies").
pub async fn posts_since(ctx: Ctx, Path((tid, pid)): Path<(i32, i32)>) -> AppResult<Response> {
    let (thread, _, fp) = check_thread(&ctx, tid).await?;
    let mp = ctx.mod_perms(thread.fid);
    let states = ctx.listed_states(thread.fid);
    let after: Option<(i64,)> =
        sqlx::query_as("SELECT dateline FROM posts WHERE pid = $1 AND tid = $2")
            .bind(pid)
            .bind(tid)
            .fetch_optional(&ctx.app.db)
            .await?;
    let after_ts = after.map(|a| a.0).unwrap_or(0);
    let before: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM posts WHERE tid = $1 AND visible = ANY($2) AND (dateline < $3 OR (dateline = $3 AND pid <= $4))",
    )
    .bind(tid)
    .bind(&states)
    .bind(after_ts)
    .bind(pid)
    .fetch_one(&ctx.app.db)
    .await?;
    let posts: Vec<Post> = sqlx::query_as(&format!(
        "SELECT {POST_COLUMNS} FROM posts WHERE tid = $1 AND visible = ANY($2) AND (dateline > $3 OR (dateline = $3 AND pid > $4))
         ORDER BY dateline, pid LIMIT 50"
    ))
    .bind(tid)
    .bind(&states)
    .bind(after_ts)
    .bind(pid)
    .fetch_all(&ctx.app.db)
    .await?;
    let postbits = build_postbits(&ctx, &thread, &fp, &mp, posts, before + 1, &[]).await?;
    ctx.render(
        "posts_fragment.html",
        minijinja::context! { staff_flags => staff_flags(&ctx, &postbits).await?, posts => postbits, thread => &thread, is_mod => mp.is_some(), modperms => &mp, reaction_types => ctx.cache.reaction_types() },
    )
    .await
}

pub async fn edit_history(ctx: Ctx, Path(pid): Path<i32>) -> AppResult<Response> {
    let post: Post = sqlx::query_as(&format!("SELECT {POST_COLUMNS} FROM posts WHERE pid = $1"))
        .bind(pid)
        .fetch_optional(&ctx.app.db)
        .await?
        .ok_or_else(|| AppError::not_found("post"))?;
    let (thread, _, _) = check_thread(&ctx, post.tid).await?;
    if !(ctx.is_mod(thread.fid) || (post.uid == ctx.uid() && ctx.uid() > 0)) {
        return Err(AppError::no_perm());
    }
    let rows: Vec<(i32, i32, i64, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT e.peid, e.uid, e.dateline, e.subject, e.message, e.reason, u.username FROM post_edits e LEFT JOIN users u ON u.uid = e.uid
         WHERE e.pid = $1 ORDER BY e.dateline DESC",
    )
    .bind(pid)
    .fetch_all(&ctx.app.db)
    .await?;
    let opts = render::forum_parse_options(ctx.cache.forum(post.fid), None);
    let edits: Vec<_> = rows
        .into_iter()
        .map(|(peid, uid, dl, subject, msg, reason, uname)| {
            minijinja::context! { peid => peid, uid => uid, dateline => dl, subject => subject,
                html => render::parse_with(&ctx.cache, &ctx.app.plugins, &opts, &msg), raw => msg, reason => reason, username => uname }
        })
        .collect();
    ctx.render(
        "edit_history.html",
        minijinja::context! { title => "Edit History", post => &post, thread => &thread, edits => edits, thread_url => url_thread(thread.tid as i64, Some(&thread.subject)) },
    )
    .await
}

/// "Who posted?" — posters in a thread ranked by number of posts (MyBB misc.php?action=whoposted).
pub async fn whoposted(ctx: Ctx, Path(tid): Path<i32>) -> AppResult<Response> {
    let (thread, _, _) = check_thread(&ctx, tid).await?;
    let states = ctx.visible_states(thread.fid);
    let rows: Vec<(i32, String, i32, i32, i64)> = sqlx::query_as(
        "SELECT p.uid, COALESCE(u.username, max(p.username)), COALESCE(u.usergroup, 1), COALESCE(u.displaygroup, 0), COUNT(*)
         FROM posts p LEFT JOIN users u ON u.uid = p.uid AND p.uid > 0
         WHERE p.tid = $1 AND p.visible = ANY($2)
         GROUP BY p.uid, u.username, u.usergroup, u.displaygroup ORDER BY COUNT(*) DESC, 2 LIMIT 500",
    )
    .bind(tid)
    .bind(&states)
    .fetch_all(&ctx.app.db)
    .await?;
    let total: i64 = rows.iter().map(|r| r.4).sum();
    let list: Vec<_> = rows
        .into_iter()
        .map(|(uid, n, g, d, c)| minijinja::context! { uid => uid, formatted => ctx.cache.format_name(&n, g, d), posts => c })
        .collect();
    ctx.render(
        "whoposted.html",
        minijinja::context! { title => "Who Posted?", thread => thread, list => list, total => total },
    )
    .await
}

/// "Send thread to a friend" form (MyBB sendthread.php).
pub async fn sendthread_form(ctx: Ctx, Path(tid): Path<i32>) -> AppResult<Response> {
    ctx.require_login()?;
    if !ctx.perms.cansendemail {
        return Err(AppError::no_perm());
    }
    let (thread, _, _) = check_thread(&ctx, tid).await?;
    let subject = format!("Thought you might like: {}", thread.subject);
    ctx.render("sendthread.html", minijinja::context! { title => "Send Thread to a Friend", thread => thread, subject => subject }).await
}

#[derive(Deserialize)]
pub struct SendThreadForm {
    #[serde(default, deserialize_with = "crate::ctx::de::string")]
    pub email: String,
    #[serde(default, deserialize_with = "crate::ctx::de::string")]
    pub subject: String,
    #[serde(default, deserialize_with = "crate::ctx::de::string")]
    pub message: String,
}

pub async fn sendthread_submit(
    ctx: Ctx,
    Path(tid): Path<i32>,
    crate::ctx::CsrfForm(f): crate::ctx::CsrfForm<SendThreadForm>,
) -> AppResult<Response> {
    let me = ctx.require_login()?.clone();
    if !ctx.perms.cansendemail {
        return Err(AppError::no_perm());
    }
    let (thread, _, _) = check_thread(&ctx, tid).await?;
    let email = f.email.trim();
    if !util::valid_email(email) {
        return Err(AppError::user("Please enter a valid email address."));
    }
    if f.subject.trim().is_empty() {
        return Err(AppError::user("Please enter a subject."));
    }
    let sent: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM maillogs WHERE fromuid = $1 AND dateline > $2")
            .bind(me.uid)
            .bind(now() - 86400)
            .fetch_one(&ctx.app.db)
            .await?;
    let max = if ctx.perms.maxemails > 0 {
        ctx.perms.maxemails as i64
    } else {
        50
    };
    if sent >= max {
        return Err(AppError::user(format!(
            "You may only send {max} emails per day."
        )));
    }
    let s = ctx.settings();
    let link = format!(
        "{}{}",
        s.get("bburl").trim_end_matches('/'),
        url_thread(tid as i64, Some(&thread.subject))
    );
    let body = format!(
        "{}\n\n{link}\n\n------------------------------------------\nThis message was sent by {} via {} ({}).\n",
        f.message.trim(),
        me.username,
        s.get("bbname"),
        s.get("bburl"),
    );
    crate::mail::queue(&ctx.app, email, f.subject.trim(), &body).await;
    sqlx::query("INSERT INTO maillogs (subject, message, dateline, fromuid, fromemail, touid, toemail, tid, ipaddress, type) VALUES ($1, $2, $3, $4, $5, 0, $6, $7, $8, 2)")
        .bind(f.subject.trim())
        .bind(f.message.trim())
        .bind(now())
        .bind(me.uid)
        .bind(&me.email)
        .bind(email)
        .bind(tid)
        .bind(crate::util::IpText::from(&ctx.ip))
        .execute(&ctx.app.db)
        .await?;
    Ok(ctx.redirect(
        &url_thread(tid as i64, Some(&thread.subject)),
        "The thread has been sent.",
    ))
}
