//! Forum thread listing.

use crate::ctx::Ctx;
use crate::error::{AppError, AppResult};
use crate::models::Thread;
use crate::templates::{url_forum, url_thread};
use crate::util::{self, leading_id, now};
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Redirect, Response};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Deserialize, Default)]
pub struct FdQuery {
    pub page: Option<i64>,
    pub sortby: Option<String>,
    pub order: Option<String>,
    pub datecut: Option<i64>,
    pub prefix: Option<i32>,
}

#[derive(Serialize, Clone, Debug)]
pub struct ThreadRow {
    pub tid: i32,
    pub fid: i32,
    pub subject: String,
    pub url: String,
    pub prefix_html: String,
    pub icon: Option<String>,
    pub author_uid: i32,
    pub author: String,
    pub dateline: i64,
    pub replies: i32,
    pub views: i32,
    pub lastpost: i64,
    pub lastposter: String,
    pub lastposteruid: i32,
    pub sticky: bool,
    pub closed: bool,
    pub moved: Option<i32>,
    pub poll: bool,
    pub attachments: i32,
    pub rating: f64,
    pub numratings: i32,
    pub visible: i16,
    pub unread: bool,
    pub dot: bool,
    pub hot: bool,
    pub pages: Vec<(i64, String)>,
    pub lastpage: Option<String>,
    pub unapprovedposts: i32,
    pub preview: String,
    pub forum_name: String,
    pub author_avatar: String,
    pub lastposter_avatar: String,
    pub forum_url: String,
}

pub fn prefix_html(ctx: &Ctx, pid: i32) -> String {
    match ctx.cache.prefix(pid) {
        Some(p) if !p.displaystyle.is_empty() => p.displaystyle.clone(),
        Some(p) => format!(
            "<span class=\"thread_prefix\">{}</span>",
            util::escape_html(&p.prefix)
        ),
        None => String::new(),
    }
}

/// Decorate threads with unread/dot/hot/multipage info for listings.
pub async fn thread_rows(ctx: &Ctx, threads: Vec<Thread>) -> AppResult<Vec<ThreadRow>> {
    // Stored searches and subscriptions may outlive moderation or permission changes.
    let threads: Vec<Thread> = threads
        .into_iter()
        .filter(|t| {
            ctx.access().can_read_thread(t.fid, t.uid, ctx.uid())
                && (ctx.visible_states(t.fid).contains(&t.visible)
                    || (t.visible == 0 && ctx.uid() > 0 && t.uid == ctx.uid()))
        })
        .collect();
    let s = ctx.settings();
    let people: Vec<i32> = threads
        .iter()
        .flat_map(|t| [t.uid, t.lastposteruid])
        .collect();
    let avatars = crate::render::avatars(ctx, &people).await?;
    let tids: Vec<i32> = threads.iter().map(|t| t.tid).collect();
    let uid = ctx.uid();
    let mut read: HashMap<i32, i64> = HashMap::new();
    let mut dots: HashSet<i32> = HashSet::new();
    let mut forum_read: HashMap<i32, i64> = HashMap::new();
    if uid > 0 && !tids.is_empty() {
        read = sqlx::query_as::<_, (i32, i64)>(
            "SELECT tid, dateline FROM threadsread WHERE uid = $1 AND tid = ANY($2)",
        )
        .bind(uid)
        .bind(&tids)
        .fetch_all(&ctx.app.db)
        .await?
        .into_iter()
        .collect();
        forum_read = crate::routes::index::forums_read(ctx).await?;
        if s.bool("dotfolders") {
            dots = sqlx::query_scalar::<_, i32>(
                // One (uid, tid) index probe per listed thread; stops at the first match.
                "SELECT t FROM unnest($1::int[]) t CROSS JOIN LATERAL (SELECT 1 FROM posts p WHERE p.uid = $2 AND p.tid = t LIMIT 1) x",
            )
            .bind(&tids)
            .bind(uid)
            .fetch_all(&ctx.app.db)
            .await?
            .into_iter()
            .collect();
        }
    }
    let previews: HashMap<i32, String> = if s.bool("showthreadpreview") && !threads.is_empty() {
        let fps: Vec<i32> = threads.iter().map(|t| t.firstpost).collect();
        sqlx::query_as::<_, (i32, String)>(
            "SELECT tid, left(message, 400) FROM posts WHERE pid = ANY($1)",
        )
        .bind(&fps)
        .fetch_all(&ctx.app.db)
        .await?
        .into_iter()
        .map(|(t, m)| {
            (
                t,
                util::truncate_chars(&crate::parser::to_plaintext(&m), 200),
            )
        })
        .collect()
    } else {
        HashMap::new()
    };
    let readcut = now() - s.int("threadreadcut").max(1) * 86400;
    let ppp = ctx
        .user
        .as_ref()
        .map(|u| u.ppp as i64)
        .filter(|p| *p > 0)
        .unwrap_or_else(|| s.int("postsperpage").max(1));
    let (hot_r, hot_v) = (s.int("hottopic"), s.int("hottopicviews"));
    let mut out = Vec::with_capacity(threads.len());
    for t in threads {
        let url = url_thread(t.tid as i64, Some(&t.subject));
        let total_pages = ((t.replies as i64 + 1) + ppp - 1) / ppp;
        let mut pages = Vec::new();
        if total_pages > 1 {
            for p in 1..=total_pages.min(4) {
                pages.push((p, format!("{url}?page={p}")));
            }
        }
        let lastpage = (total_pages > 4).then(|| format!("{url}?page={total_pages}"));
        let unread = uid > 0
            && t.lastpost > readcut
            && t.lastpost > read.get(&t.tid).copied().unwrap_or(0)
            && t.lastpost > forum_read.get(&t.fid).copied().unwrap_or(0);
        let forum = ctx.cache.forum(t.fid);
        let (vtid, vsubject) = (t.tid, t.subject.clone());
        let _ = vtid;
        out.push(ThreadRow {
            tid: t.tid,
            fid: t.fid,
            url,
            prefix_html: prefix_html(ctx, t.prefix),
            icon: ctx.cache.icon(t.icon).map(|i| i.path.clone()),
            author_uid: t.uid,
            author: t.username.clone(),
            dateline: t.dateline,
            replies: t.replies,
            views: t.views + ctx.app.thread_views.get(&t.tid).map(|v| *v).unwrap_or(0),
            lastpost: t.lastpost,
            lastposter: t.lastposter.clone(),
            lastposteruid: t.lastposteruid,
            sticky: t.sticky,
            closed: t.is_closed(),
            moved: t.moved_to(),
            poll: t.poll > 0,
            attachments: t.attachmentcount,
            rating: if t.numratings > 0 {
                t.totalratings as f64 / t.numratings as f64
            } else {
                0.0
            },
            numratings: t.numratings,
            visible: t.visible,
            unread,
            dot: dots.contains(&t.tid),
            hot: t.replies as i64 >= hot_r || t.views as i64 >= hot_v,
            pages,
            lastpage,
            unapprovedposts: t.unapprovedposts,
            preview: previews.get(&t.tid).cloned().unwrap_or_default(),
            forum_name: forum.map(|f| f.name.clone()).unwrap_or_default(),
            author_avatar: avatars
                .get(&t.uid)
                .map(|a| a.to_string())
                .unwrap_or_default(),
            lastposter_avatar: avatars
                .get(&t.lastposteruid)
                .map(|a| a.to_string())
                .unwrap_or_default(),
            forum_url: url_forum(t.fid as i64, forum.map(|f| f.name.as_str())),
            subject: vsubject,
        });
    }
    Ok(out)
}

pub fn breadcrumb(ctx: &Ctx, fid: i32) -> Vec<(String, String)> {
    let mut v = Vec::new();
    if let Some(f) = ctx.cache.forum(fid) {
        for p in &f.parentlist {
            if let Some(pf) = ctx.cache.forum(*p) {
                v.push((pf.name.clone(), url_forum(pf.fid as i64, Some(&pf.name))));
            }
        }
    }
    v
}

/// Forum jump menu entries (fid, name, depth).
pub fn forum_jump(ctx: &Ctx) -> Vec<(i32, String, usize)> {
    ctx.cache
        .forums
        .iter()
        .filter(|f| f.showinjump && ctx.access().listed(f.fid))
        .map(|f| {
            (
                f.fid,
                f.name.clone(),
                ctx.cache.forum_depth.get(&f.fid).copied().unwrap_or(0),
            )
        })
        .collect()
}

pub async fn forumdisplay(
    ctx: Ctx,
    Path(seg): Path<String>,
    Query(q): Query<FdQuery>,
) -> AppResult<Response> {
    let fid = leading_id(&seg).ok_or_else(|| AppError::not_found("forum"))?;
    let (forum, fperms) = ctx.check_forum(fid)?;
    let forum = forum.clone();
    if !forum.linkto.is_empty() {
        return Ok(Redirect::to(&forum.linkto).into_response());
    }
    ctx.set_location(fid, 0);
    // A forum's theme applies when it overrides members' choices or the viewer chose none.
    if forum.style > 0
        && (forum.overridestyle || ctx.user.as_ref().map(|u| u.style == 0).unwrap_or(true))
    {
        ctx.set_theme(forum.style);
    }
    let s = ctx.settings();
    let mut subforums = crate::routes::index::build_tree(&ctx, fid, 1).await?;
    crate::routes::index::fill_avatars(&ctx, &mut subforums).await?;
    let mod_perms = ctx.mod_perms(fid);
    let is_mod = mod_perms.is_some();

    let mut threads_out = Vec::new();
    let mut pagination = util::Pagination::default();
    let mut announcements = Vec::new();
    let mut total: i64 = 0;
    let sortby = q
        .sortby
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if forum.defaultsortby.is_empty() {
                "lastpost".into()
            } else {
                forum.defaultsortby.clone()
            }
        });
    let order = q
        .order
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if forum.defaultsortorder.is_empty() {
                if sortby == "subject" || sortby == "starter" {
                    "asc".into()
                } else {
                    "desc".into()
                }
            } else {
                forum.defaultsortorder.clone()
            }
        });
    // Only known sort keys reach the pagination links; anything else sorts as `lastpost` anyway.
    let sortby = if matches!(
        sortby.as_str(),
        "subject" | "starter" | "started" | "replies" | "views" | "rating" | "lastpost"
    ) {
        sortby
    } else {
        "lastpost".to_string()
    };
    let order = if order == "asc" { "asc" } else { "desc" };
    let datecut = q
        .datecut
        .unwrap_or(forum.defaultdatecut as i64)
        .clamp(0, 100_000);

    if !forum.is_category() && fperms.canviewthreads {
        let tpp = ctx
            .user
            .as_ref()
            .map(|u| u.tpp as i64)
            .filter(|p| *p > 0)
            .unwrap_or_else(|| s.int("threadsperpage").max(1));
        let states = ctx.visible_states(fid);
        let sort_col = match sortby.as_str() {
            "subject" => "lower(subject)",
            "starter" => "lower(username)",
            "started" => "dateline",
            "replies" => "replies",
            "views" => "views",
            "rating" => {
                "(CASE WHEN numratings > 0 THEN totalratings::float / numratings ELSE 0 END)"
            }
            _ => "lastpost",
        };
        let dir = if order == "asc" { "ASC" } else { "DESC" };
        let own_only = fperms.canonlyviewownthreads && !is_mod;
        let prefix = q.prefix.unwrap_or(0);
        let simple = datecut <= 0 && !own_only && prefix == 0;
        total = if simple {
            let c: (i32, i32, i32) = sqlx::query_as(
                "SELECT threads, unapprovedthreads, deletedthreads FROM forums WHERE fid = $1",
            )
            .bind(fid)
            .fetch_one(&ctx.app.db)
            .await?;
            // Redirect stubs only affect the page count; a 30 s old count is fine.
            let key = format!("moved:{fid}");
            let redirects: i64 = match ctx.app.short_cache.get(&key).and_then(|v| v.as_i64()) {
                Some(n) => n,
                None => {
                    let n: i64 = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM threads WHERE fid = $1 AND closed LIKE 'moved|%'",
                    )
                    .bind(fid)
                    .fetch_one(&ctx.app.db)
                    .await?;
                    ctx.app.short_cache.insert(key, serde_json::json!(n));
                    n
                }
            };
            c.0 as i64
                + if states.contains(&0) { c.1 as i64 } else { 0 }
                + if states.contains(&-1) { c.2 as i64 } else { 0 }
                + redirects
        } else {
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM threads WHERE fid = $1 AND visible = ANY($2) AND ($3 = 0 OR uid = $3) AND ($4 = 0 OR lastpost >= $4) AND ($5 = 0 OR prefix = $5)",
            )
            .bind(fid)
            .bind(&states)
            .bind(if own_only { ctx.uid() } else { 0 })
            .bind(if datecut > 0 { now() - datecut * 86400 } else { 0 })
            .bind(prefix)
            .fetch_one(&ctx.app.db)
            .await?
        };
        let base = format!(
            "{}?sortby={}&order={}&datecut={}{}&page={{page}}",
            url_forum(fid as i64, Some(&forum.name)),
            sortby,
            order,
            datecut,
            if prefix > 0 {
                format!("&prefix={prefix}")
            } else {
                String::new()
            }
        );
        pagination = util::paginate(total, tpp, util::clamp_page(q.page), &base);
        let offset = (pagination.page - 1) * tpp;
        // Deep pages: scan from the far end of the index so OFFSET stays small.
        let reverse = offset > total / 2;
        let (order_sql, limit, off) = if reverse {
            let flip = if dir.eq_ignore_ascii_case("DESC") {
                "ASC"
            } else {
                "DESC"
            };
            let end = (offset + tpp).min(total);
            (
                format!("sticky ASC, {sort_col} {flip}, tid ASC"),
                (end - offset).max(0),
                (total - end).max(0),
            )
        } else {
            (
                format!("sticky DESC, {sort_col} {dir}, tid DESC"),
                tpp,
                offset,
            )
        };
        let mut threads: Vec<Thread> = sqlx::query_as(&format!(
            "SELECT {cols} FROM threads WHERE fid = $1 AND visible = ANY($2) AND ($3 = 0 OR uid = $3) AND ($4 = 0 OR lastpost >= $4)
               AND ($5 = 0 OR prefix = $5)
             ORDER BY {order_sql} LIMIT $6 OFFSET $7",
            cols = crate::models::THREAD_COLUMNS
        ))
        .bind(fid)
        .bind(&states)
        .bind(if own_only { ctx.uid() } else { 0 })
        .bind(if datecut > 0 { now() - datecut * 86400 } else { 0 })
        .bind(prefix)
        .bind(limit)
        .bind(off)
        .fetch_all(&ctx.app.db)
        .await?;
        if reverse {
            threads.reverse();
        }
        threads_out = thread_rows(&ctx, threads).await?;

        let limit = s.int("announcementlimit").max(0) as usize;
        let t = now();
        announcements = ctx
            .cache
            .announcements
            .iter()
            .filter(|a| (a.fid == -1 || forum.parentlist.contains(&a.fid)) && a.startdate <= t && (a.enddate == 0 || a.enddate > t))
            .take(limit)
            .map(|a| minijinja::context! { aid => a.aid, subject => &a.subject, startdate => a.startdate })
            .collect::<Vec<_>>();
    }

    let browsing = if s.bool("browsingthisforum") {
        // Who's here, refreshed at most every 30 s per forum (sessions are only flushed every
        // few seconds anyway); each viewer's permissions are still applied below.
        let key = format!("browsing:{fid}");
        let (rows, guests): (Vec<(i32, String, i32, i32, bool)>, i64) = match ctx
            .app
            .short_cache
            .get(&key)
            .and_then(|v| serde_json::from_value(v).ok())
        {
            Some(x) => x,
            None => {
                let cutoff = now() - s.int("wolcutoffmins").max(1) * 60;
                let rows: Vec<(i32, String, i32, i32, bool)> = sqlx::query_as(
                        "SELECT DISTINCT u.uid, u.username, u.usergroup, u.displaygroup, u.invisible FROM sessions s JOIN users u ON u.uid = s.uid
                         WHERE s.time > $1 AND s.location1 = $2 AND s.uid > 0 LIMIT 100",
                    )
                    .bind(cutoff)
                    .bind(fid)
                    .fetch_all(&ctx.app.db)
                    .await?;
                let guests: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM sessions WHERE time > $1 AND location1 = $2 AND uid = 0",
                )
                .bind(cutoff)
                .bind(fid)
                .fetch_one(&ctx.app.db)
                .await?;
                ctx.app
                    .short_cache
                    .insert(key, serde_json::json!([rows, guests]));
                (rows, guests)
            }
        };
        let users: Vec<(i32, String)> = rows
            .into_iter()
            .filter(|r| !r.4 || ctx.perms.canviewwolinvis || r.0 == ctx.uid())
            .map(|r| (r.0, ctx.cache.format_name(&r.1, r.2, r.3)))
            .collect();
        let mut users = users;
        if let Some(me) = &ctx.user
            && !users.iter().any(|u| u.0 == me.uid)
        {
            users.push((
                me.uid,
                ctx.cache
                    .format_name(&me.username, me.usergroup, me.displaygroup),
            ));
        }
        Some(minijinja::context! { users => users, guests => guests })
    } else {
        None
    };

    let subscribed = if ctx.uid() > 0 {
        sqlx::query_scalar::<_, i32>(
            "SELECT fsid FROM forumsubscriptions WHERE uid = $1 AND fid = $2",
        )
        .bind(ctx.uid())
        .bind(fid)
        .fetch_optional(&ctx.app.db)
        .await?
        .is_some()
    } else {
        false
    };
    let rules_html = if forum.rulestype > 0 && !forum.rules.is_empty() {
        crate::render::parse_with(
            &ctx.cache,
            &ctx.app.plugins,
            &Default::default(),
            &forum.rules,
        )
    } else {
        String::new()
    };
    let custom_tools: Vec<(i32, String)> = if is_mod {
        sqlx::query_as::<_, (i32, String, Vec<i32>, Vec<i32>)>(
            "SELECT tid, name, forums, groups FROM modtools WHERE type = 't' ORDER BY name",
        )
        .fetch_all(&ctx.app.db)
        .await?
        .into_iter()
        .filter(|(_, _, f, g)| {
            (f.is_empty() || f.iter().any(|x| forum.parentlist.contains(x)))
                && (g.is_empty() || g.iter().any(|x| ctx.groups.contains(x)))
        })
        .map(|(t, n, _, _)| (t, n))
        .collect()
    } else {
        vec![]
    };

    ctx.allow_guest_cache(&[format!("forum:{fid}")]);
    ctx.render(
        "forumdisplay.html",
        minijinja::context! {
            title => &forum.name,
            forum => &forum,
            forum_url => url_forum(fid as i64, Some(&forum.name)),
            breadcrumb => breadcrumb(&ctx, fid),
            subforums => subforums,
            threads => threads_out,
            announcements => announcements,
            pagination => pagination,
            total => total,
            fperms => &fperms,
            modperms => mod_perms,
            is_mod => is_mod,
            sortby => sortby,
            order => order,
            datecut => datecut,
            prefixes => ctx.cache.prefixes_for(fid, &[]),
            prefix => q.prefix.unwrap_or(0),
            browsing => browsing,
            subscribed => subscribed,
            rules_html => rules_html,
            forumjump => forum_jump(&ctx),
            custom_tools => custom_tools,
            can_post => fperms.canpostthreads && forum.open && !forum.is_category(),
        },
    )
    .await
}
