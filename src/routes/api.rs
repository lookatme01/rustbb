//! JSON REST API (v1). Authenticate with `Authorization: Bearer <token>` (obtain one from
//! `POST /api/v1/auth/token`) or with the browser session plus an `X-CSRF-Token` header for writes.
//! All permission checks are shared with the HTML front end.

use crate::app::App;
use crate::ctx::Ctx;
use crate::error::{AppError, AppResult};
use crate::models::{POST_COLUMNS, Post, Thread};
use crate::util::{self, now};
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

pub fn router() -> Router<App> {
    Router::new()
        .route("/", get(root))
        .route("/auth/token", post(token_create).delete(token_revoke))
        .route("/me", get(me))
        .route("/forums", get(forums))
        .route(
            "/forums/{fid}/threads",
            get(forum_threads).post(create_thread),
        )
        .route("/threads/{tid}", get(thread))
        .route("/threads/{tid}/posts", post(create_reply))
        .route("/posts/{pid}", get(post_get))
        .route("/users/{uid}", get(user))
        .route("/search", get(search))
        .route("/stats", get(stats))
        .route("/alerts", get(alerts))
}

fn ok(v: serde_json::Value) -> AppResult<Response> {
    Ok(Json(v).into_response())
}

async fn root() -> Response {
    Json(serde_json::json!({
        "name": "rbb", "version": env!("CARGO_PKG_VERSION"),
        "endpoints": ["POST /auth/token", "DELETE /auth/token", "GET /me", "GET /forums", "GET /forums/{fid}/threads", "POST /forums/{fid}/threads",
            "GET /threads/{tid}", "POST /threads/{tid}/posts", "GET /posts/{pid}", "GET /users/{uid}", "GET /search?q=", "GET /stats", "GET /alerts"],
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct TokenReq {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub code: String,
}

pub async fn token_create(ctx: Ctx, Json(r): Json<TokenReq>) -> AppResult<Response> {
    if !ctx.app.rate_check(&format!("apilogin:{}", ctx.ip), 10, 300) {
        return Err(AppError::RateLimited);
    }
    // Same checks as the web login: throttling, ban filters, lockout and failed-attempt
    // accounting, constant-time handling of unknown users.
    let user =
        match crate::routes::member::check_credentials(&ctx, &r.username, &r.password).await? {
            Ok(u) => u,
            Err(msg) => return Err(AppError::NoPermission(msg)),
        };
    if ctx
        .cache
        .group(user.usergroup)
        .map(|g| g.isbannedgroup)
        .unwrap_or(false)
    {
        return Err(AppError::NoPermission(
            crate::routes::member::LOGIN_FAILED.into(),
        ));
    }
    if !user.totp_secret.is_empty()
        && !crate::routes::member::totp_consume(&ctx.app, user.uid, &user.totp_secret, &r.code)
            .await?
    {
        crate::audit::log(
            &ctx,
            user.uid,
            "login_2fa_failed",
            serde_json::json!({"api": true}),
        )
        .await;
        return Err(AppError::NoPermission(
            "A valid two-factor code is required (field \"code\").".into(),
        ));
    }
    let token = util::random_token(48);
    let days = ctx.settings().int("loginsessionlength").max(1);
    sqlx::query("INSERT INTO logins (token_hash, uid, created, lastused, expires, ip, useragent, csrf) VALUES ($1, $2, $3, $3, $4, $5, 'api', $6)")
        .bind(util::sha256_hex(&token))
        .bind(user.uid)
        .bind(now())
        .bind(now() + days * 86400)
        .bind(&ctx.ip)
        .bind(util::random_token(32))
        .execute(&ctx.app.db)
        .await?;
    crate::audit::log(
        &ctx,
        user.uid,
        "api_token",
        serde_json::json!({"days": days}),
    )
    .await;
    ok(serde_json::json!({"token": token, "uid": user.uid, "expires": now() + days * 86400}))
}

pub async fn token_revoke(ctx: Ctx) -> AppResult<Response> {
    ctx.require_login()?;
    ctx.check_csrf("")?;
    if let Some(h) = &ctx.token_hash {
        sqlx::query("DELETE FROM logins WHERE token_hash = $1")
            .bind(h)
            .execute(&ctx.app.db)
            .await?;
    }
    ok(serde_json::json!({"ok": true}))
}

pub async fn me(ctx: Ctx) -> AppResult<Response> {
    let u = ctx.require_login()?;
    ok(serde_json::json!({
        "uid": u.uid, "username": u.username, "usergroup": u.usergroup, "postnum": u.postnum, "threadnum": u.threadnum, "reputation": u.reputation,
        "unreadpms": u.unreadpms, "unreadalerts": u.unreadalerts, "regdate": u.regdate, "avatar": u.avatar, "timezone": u.timezone,
    }))
}

pub async fn forums(ctx: Ctx) -> AppResult<Response> {
    let counters = crate::routes::index::load_counters(&ctx).await?;
    let list: Vec<serde_json::Value> = ctx
        .cache
        .forums
        .iter()
        .filter(|f| f.active && ctx.forum_perms(f.fid).canview)
        .map(|f| {
            let c = counters.get(&f.fid).cloned().unwrap_or_default();
            let fp = ctx.forum_perms(f.fid);
            serde_json::json!({
                "fid": f.fid, "pid": f.pid, "name": f.name, "description": f.description, "type": f.kind, "linkto": f.linkto,
                "threads": c.threads, "posts": c.posts, "open": f.open,
                "lastpost": if fp.canviewthreads && !fp.canonlyviewownthreads && !f.has_password() { serde_json::json!({"dateline": c.lastpost, "username": c.lastposter, "uid": c.lastposteruid, "tid": c.lastposttid, "subject": c.lastpostsubject}) } else { serde_json::Value::Null },
            })
        })
        .collect();
    ok(serde_json::json!({ "forums": list }))
}

#[derive(Deserialize, Default)]
pub struct PageQ {
    pub page: Option<i64>,
    pub per_page: Option<i64>,
}

fn thread_json(t: &Thread) -> serde_json::Value {
    serde_json::json!({
        "tid": t.tid, "fid": t.fid, "subject": t.subject, "prefix": t.prefix, "uid": t.uid, "username": t.username, "dateline": t.dateline,
        "lastpost": t.lastpost, "lastposter": t.lastposter, "lastposteruid": t.lastposteruid, "replies": t.replies, "views": t.views,
        "sticky": t.sticky, "closed": t.is_closed(), "poll": t.poll > 0, "visible": t.visible,
    })
}

pub async fn forum_threads(
    ctx: Ctx,
    Path(fid): Path<i32>,
    Query(q): Query<PageQ>,
) -> AppResult<Response> {
    let (_, fp) = ctx.check_forum(fid)?;
    if !fp.canviewthreads {
        return Err(AppError::no_perm());
    }
    let per = q.per_page.unwrap_or(25).clamp(1, 100);
    let page = util::clamp_page(q.page);
    let states = ctx.visible_states(fid);
    let own = if fp.canonlyviewownthreads && !ctx.is_mod(fid) {
        ctx.uid()
    } else {
        0
    };
    let threads: Vec<Thread> = sqlx::query_as(
        "SELECT * FROM threads WHERE fid = $1 AND visible = ANY($2) AND ($5 = 0 OR uid = $5) ORDER BY sticky DESC, lastpost DESC, tid DESC LIMIT $3 OFFSET $4",
    )
    .bind(fid)
    .bind(&states)
    .bind(per)
    .bind((page - 1) * per)
    .bind(own)
    .fetch_all(&ctx.app.db)
    .await?;
    ok(
        serde_json::json!({ "page": page, "per_page": per, "threads": threads.iter().map(thread_json).collect::<Vec<_>>() }),
    )
}

pub async fn thread(ctx: Ctx, Path(tid): Path<i32>, Query(q): Query<PageQ>) -> AppResult<Response> {
    let (t, _, _) = crate::routes::showthread::check_thread(&ctx, tid).await?;
    let per = q.per_page.unwrap_or(20).clamp(1, 100);
    let page = util::clamp_page(q.page);
    let states = ctx.visible_states(t.fid);
    let posts: Vec<Post> = sqlx::query_as(&format!("SELECT {POST_COLUMNS} FROM posts WHERE tid = $1 AND visible = ANY($2) ORDER BY dateline, pid LIMIT $3 OFFSET $4"))
        .bind(tid)
        .bind(&states)
        .bind(per)
        .bind((page - 1) * per)
        .fetch_all(&ctx.app.db)
        .await?;
    let mut stale = vec![];
    let list: Vec<serde_json::Value> = posts
        .iter()
        .map(|p| {
            serde_json::json!({"pid": p.pid, "uid": p.uid, "username": p.username, "dateline": p.dateline, "subject": p.subject, "message": p.message,
                "html": crate::render::post_html(&ctx, p, &mut stale), "edittime": p.edittime, "visible": p.visible})
        })
        .collect();
    crate::render::store_parsed(&ctx, stale);
    *ctx.app.thread_views.entry(tid).or_insert(0) += 1;
    ok(
        serde_json::json!({ "thread": thread_json(&t), "page": page, "per_page": per, "posts": list }),
    )
}

pub async fn post_get(ctx: Ctx, Path(pid): Path<i32>) -> AppResult<Response> {
    let p: Post = sqlx::query_as(&format!("SELECT {POST_COLUMNS} FROM posts WHERE pid = $1"))
        .bind(pid)
        .fetch_optional(&ctx.app.db)
        .await?
        .ok_or_else(|| AppError::not_found("post"))?;
    let (t, _, _) = crate::routes::showthread::check_thread(&ctx, p.tid).await?;
    if !ctx.visible_states(t.fid).contains(&p.visible) {
        return Err(AppError::not_found("post"));
    }
    let mut stale = vec![];
    let html = crate::render::post_html(&ctx, &p, &mut stale);
    crate::render::store_parsed(&ctx, stale);
    ok(
        serde_json::json!({"pid": p.pid, "tid": p.tid, "uid": p.uid, "username": p.username, "dateline": p.dateline, "subject": p.subject, "message": p.message, "html": html}),
    )
}

#[derive(Deserialize)]
pub struct NewThreadReq {
    pub subject: String,
    pub message: String,
    #[serde(default)]
    pub prefix: i32,
}

pub async fn create_thread(
    ctx: Ctx,
    Path(fid): Path<i32>,
    Json(r): Json<NewThreadReq>,
) -> AppResult<Response> {
    ctx.require_login()?;
    ctx.check_csrf("")?;
    let (forum, fp) = ctx.check_forum(fid)?;
    if forum.is_category()
        || !forum.linkto.is_empty()
        || !fp.canpostthreads
        || !ctx.perms.canpostthreads
        || (!forum.open && !ctx.is_mod(fid))
    {
        return Err(AppError::no_perm());
    }
    let input = crate::posting::PostInput {
        subject: r.subject,
        message: r.message,
        icon: 0,
        includesig: true,
        smilieoff: false,
        posthash: String::new(),
        replyto: 0,
        as_system: false,
    };
    crate::posting::validate(&ctx, &input, true)?;
    crate::posting::check_posting_allowed(&ctx).await?;
    let prefix = if r.prefix > 0
        && ctx
            .cache
            .prefixes_for(fid, &ctx.groups)
            .iter()
            .any(|p| p.pid == r.prefix)
    {
        r.prefix
    } else {
        0
    };
    let (tid, pid, visible) = crate::posting::create_thread(
        &ctx,
        fid,
        &input,
        &crate::posting::ThreadExtra {
            prefix,
            sticky: false,
            closed: false,
            poll: None,
        },
    )
    .await?;
    ok(serde_json::json!({"tid": tid, "pid": pid, "visible": visible}))
}

#[derive(Deserialize)]
pub struct ReplyReq {
    pub message: String,
    #[serde(default)]
    pub subject: String,
}

pub async fn create_reply(
    ctx: Ctx,
    Path(tid): Path<i32>,
    Json(r): Json<ReplyReq>,
) -> AppResult<Response> {
    ctx.require_login()?;
    ctx.check_csrf("")?;
    let (t, forum, fp) = crate::routes::showthread::check_thread(&ctx, tid).await?;
    let mp = ctx.mod_perms(t.fid);
    if !fp.canpostreplys
        || !ctx.perms.canpostreplys
        || ((t.is_closed() || !forum.open) && !mp.map(|m| m.canpostclosedthreads).unwrap_or(false))
    {
        return Err(AppError::no_perm());
    }
    let subject = if r.subject.trim().is_empty() {
        format!("RE: {}", t.subject)
    } else {
        r.subject
    };
    let input = crate::posting::PostInput {
        subject,
        message: r.message,
        icon: 0,
        includesig: true,
        smilieoff: false,
        posthash: String::new(),
        replyto: 0,
        as_system: false,
    };
    crate::posting::validate(&ctx, &input, false)?;
    crate::posting::check_posting_allowed(&ctx).await?;
    let (pid, visible, merged) =
        crate::posting::create_reply(&ctx, tid, t.fid, &input, None).await?;
    ok(serde_json::json!({"pid": pid, "visible": visible, "merged": merged}))
}

pub async fn user(ctx: Ctx, Path(uid): Path<i32>) -> AppResult<Response> {
    if !ctx.perms.canviewprofiles {
        return Err(AppError::no_perm());
    }
    let a = crate::render::load_authors(&ctx, &[uid])
        .await?
        .remove(&uid)
        .ok_or_else(|| AppError::not_found("user"))?;
    ok(serde_json::json!({
        "uid": a.uid, "username": a.username, "usertitle": a.usertitle, "avatar": a.avatar, "postnum": a.postnum, "threadnum": a.threadnum,
        "regdate": a.regdate, "reputation": a.reputation, "online": a.online, "group": a.grouptitle, "website": a.website,
    }))
}

#[derive(Deserialize, Default)]
pub struct SearchQ {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub limit: i64,
}

pub async fn search(ctx: Ctx, Query(q): Query<SearchQ>) -> AppResult<Response> {
    if !ctx.perms.cansearch {
        return Err(AppError::no_perm());
    }
    if q.q.trim().len() < ctx.settings().int("minsearchword").max(1) as usize {
        return Err(AppError::user("Search terms are too short."));
    }
    let key = if ctx.uid() > 0 {
        format!("apisearch:u{}", ctx.uid())
    } else {
        format!("apisearch:{}", ctx.ip)
    };
    if !ctx.app.rate_check(&key, 10, 60) {
        return Err(AppError::RateLimited);
    }
    let (fids, _) = crate::routes::search::searchable_forums(&ctx);
    let rows: Vec<(i32, i32, String, String, i64, f32)> = sqlx::query_as(
        "SELECT p.pid, p.tid, t.subject, p.username, p.dateline, ts_rank(p.search_tsv, websearch_to_tsquery('english', $1)) AS r
         FROM posts p JOIN threads t ON t.tid = p.tid WHERE p.search_tsv @@ websearch_to_tsquery('english', $1) AND p.fid = ANY($2) AND p.visible = 1 AND t.visible = 1
         ORDER BY r DESC, p.dateline DESC LIMIT $3",
    )
    .bind(q.q.trim())
    .bind(&fids)
    .bind(if q.limit > 0 { q.limit.min(100) } else { 25 })
    .fetch_all(&ctx.app.db)
    .await?;
    ok(
        serde_json::json!({ "results": rows.iter().map(|r| serde_json::json!({"pid": r.0, "tid": r.1, "subject": r.2, "username": r.3, "dateline": r.4, "rank": r.5})).collect::<Vec<_>>() }),
    )
}

pub async fn stats(ctx: Ctx) -> AppResult<Response> {
    let s = crate::routes::index::board_stats(&ctx).await?;
    ok(s)
}

pub async fn alerts(ctx: Ctx) -> AppResult<Response> {
    let me = ctx.require_login()?;
    let rows: Vec<(i64, String, i32, serde_json::Value, i64, bool, Option<String>)> = sqlx::query_as(
        "SELECT a.id, a.kind, a.object_id, a.extra, a.dateline, a.unread, u.username FROM alerts a LEFT JOIN users u ON u.uid = a.from_uid WHERE a.uid = $1 ORDER BY a.id DESC LIMIT 50",
    )
    .bind(me.uid)
    .fetch_all(&ctx.app.db)
    .await?;
    ok(serde_json::json!({ "alerts": rows.iter().map(|r| {
        let (text, url) = crate::routes::usercp::describe_alert(&r.1, &r.3, r.6.as_deref().unwrap_or("Someone"), r.2);
        serde_json::json!({"id": r.0, "kind": r.1, "text": text, "url": url, "dateline": r.4, "unread": r.5})
    }).collect::<Vec<_>>() }))
}
