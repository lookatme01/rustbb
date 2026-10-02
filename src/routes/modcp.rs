//! Moderator Control Panel: reports, moderation queue, logs, announcements, bans, profile
//! editing, IP search, warning logs and scheduled actions.

use crate::app::App;
use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::models::User;
use crate::templates::url_thread;
use crate::util::{self, now};
use axum::Router;
use axum::extract::{Path, Query};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;

pub fn router() -> Router<App> {
    Router::new()
        .route("/modcp", get(home))
        .route("/modcp/reports", get(reports).post(reports_action))
        .route("/modcp/modqueue", get(modqueue).post(modqueue_action))
        .route("/modcp/modlogs", get(modlogs))
        .route("/modcp/announcements", get(announcements))
        .route(
            "/modcp/announcements/edit",
            get(announcement_form).post(announcement_save),
        )
        .route("/modcp/announcements/delete", post(announcement_delete))
        .route("/modcp/finduser", get(finduser))
        .route(
            "/modcp/editprofile/{uid}",
            get(editprofile_form).post(editprofile_save),
        )
        .route("/modcp/banning", get(banning))
        .route("/modcp/ban", get(ban_form).post(ban_save))
        .route("/modcp/liftban", post(lift_ban))
        .route("/modcp/ipsearch", get(ipsearch))
        .route("/modcp/warninglogs", get(warninglogs))
        .route("/modcp/delayed", get(delayed))
}

pub(crate) fn require_modcp(ctx: &Ctx) -> AppResult<()> {
    ctx.require_login()?;
    if !(ctx.perms.canmodcp || ctx.is_any_mod()) {
        return Err(AppError::no_perm());
    }
    Ok(())
}

/// Forums the viewer moderates (None = all).
pub(crate) fn mod_fids(ctx: &Ctx) -> Option<Vec<i32>> {
    ctx.cache
        .moderated_forums(ctx.uid(), &ctx.groups, &ctx.perms)
}

async fn page(
    ctx: &Ctx,
    name: &str,
    active: &str,
    title: &str,
    extra: minijinja::Value,
) -> AppResult<Response> {
    let base = minijinja::context! { title => title, mcp_active => active, breadcrumb => vec![("Mod CP".to_string(), "/modcp".to_string())] };
    ctx.render(name, minijinja::value::merge_maps([base, extra]))
        .await
}

pub async fn home(ctx: Ctx) -> AppResult<Response> {
    require_modcp(&ctx)?;
    let fids = mod_fids(&ctx);
    let all = fids.is_none();
    let f = fids.unwrap_or_default();
    let (uthreads, uposts): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(unapprovedthreads), 0)::bigint, COALESCE(SUM(unapprovedposts), 0)::bigint FROM forums WHERE $1 OR fid = ANY($2)",
    )
    .bind(all)
    .bind(&f)
    .fetch_one(&ctx.app.db)
    .await?;
    let reports: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reportedcontent WHERE reportstatus = 0 AND ($1 OR type <> 'post' OR id3 = ANY($2))")
        .bind(all)
        .bind(&f)
        .fetch_one(&ctx.app.db)
        .await?;
    let logs = load_logs(&ctx, 0, 0, 10, 0).await?;
    page(&ctx, "modcp/home.html", "home", "Moderator Control Panel", minijinja::context! { uthreads => uthreads, uposts => uposts, reports => reports, logs => logs }).await
}

// ---------------------------------------------------------------- reports

#[derive(Deserialize, Default)]
pub struct ReportsQuery {
    pub page: Option<i64>,
    pub status: Option<i16>,
}

pub async fn reports(ctx: Ctx, Query(q): Query<ReportsQuery>) -> AppResult<Response> {
    crate::routes::modreports::require_reports(&ctx)?;
    let fids = mod_fids(&ctx);
    let all = fids.is_none();
    let f = fids.unwrap_or_default();
    let status = q.status.unwrap_or(0);
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reportedcontent WHERE reportstatus = $1 AND ($2 OR type <> 'post' OR id3 = ANY($3))")
        .bind(status)
        .bind(all)
        .bind(&f)
        .fetch_one(&ctx.app.db)
        .await?;
    let pg = util::paginate(
        total,
        25,
        util::clamp_page(q.page),
        &format!("/modcp/reports?status={status}&page={{page}}"),
    );
    let rows: Vec<(i32, i32, i32, i32, String, i32, String, i32, i64, i64, Option<String>, Option<String>, Option<String>, Option<String>, i32)> = sqlx::query_as(
        "SELECT r.rid, r.id, r.id2, r.id3, r.type, r.reports, r.reason, r.reasonid, r.dateline, r.lastreport, u.username, t.subject, ru.username, cu.username,
                COALESCE(CASE WHEN r.type = 'post' THEN (SELECT uid FROM posts WHERE pid = r.id) ELSE r.id2 END, 0)
         FROM reportedcontent r LEFT JOIN users u ON u.uid = r.uid
         LEFT JOIN users cu ON cu.uid = r.claimed_by AND r.claimed_by > 0
         LEFT JOIN threads t ON r.type = 'post' AND t.tid = r.id2
         LEFT JOIN users ru ON (r.type IN ('profile', 'reputation') AND ru.uid = r.id2) OR (r.type = 'post' AND ru.uid = (SELECT uid FROM posts WHERE pid = r.id))
         WHERE r.reportstatus = $1 AND ($2 OR r.type <> 'post' OR r.id3 = ANY($3))
         ORDER BY r.lastreport DESC LIMIT 25 OFFSET $4",
    )
    .bind(status)
    .bind(all)
    .bind(&f)
    .bind((pg.page - 1) * 25)
    .fetch_all(&ctx.app.db)
    .await?;
    let reasons = ctx.cache.reportreasons.clone();
    let target_uids: Vec<i32> = rows.iter().map(|r| r.14).filter(|u| *u > 0).collect();
    let notes = crate::routes::modnotes::latest_notes(&ctx.app, &target_uids).await?;
    let list: Vec<_> = rows
        .into_iter()
        .map(|(rid, id, id2, _id3, kind, n, reason, reasonid, dl, last, reporter, subject, target, claimer, target_uid)| {
            let reason_title = reasons.iter().find(|r| r.rid == reasonid).map(|r| r.title.clone()).unwrap_or_default();
            let link = match kind.as_str() {
                "post" => format!("/post/{id}"),
                "profile" => format!("/user/{id}"),
                "reputation" => format!("/reputation/{id2}"),
                "pm" => "#".to_string(),
                _ => "#".to_string(),
            };
            minijinja::context! { rid => rid, kind => kind, reports => n, reason => reason, reason_title => reason_title, dateline => dl, lastreport => last,
                reporter => reporter, subject => subject, target => target, link => link, claimer => claimer, target_uid => target_uid,
                latest_note => notes.get(&target_uid).cloned() }
        })
        .collect();
    page(
        &ctx,
        "modcp/reports.html",
        "reports",
        "Reported Content",
        minijinja::context! { reports => list, pagination => pg, status => status },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct IdsForm {
    #[serde(default, deserialize_with = "de::vec_i32")]
    pub ids: Vec<i32>,
    #[serde(default, deserialize_with = "de::string")]
    pub action: String,
}

pub async fn reports_action(ctx: Ctx, CsrfForm(f): CsrfForm<IdsForm>) -> AppResult<Response> {
    // Same permission as viewing the reports: Mod CP access alone isn't enough.
    crate::routes::modreports::require_reports(&ctx)?;
    let status: i16 = if f.action == "reopen" { 0 } else { 1 };
    let fids = mod_fids(&ctx);
    let changed: Vec<i32> = sqlx::query_scalar(
        "UPDATE reportedcontent SET reportstatus = $2,
            resolved_by = CASE WHEN $2 = 1 THEN $5 ELSE 0 END, resolved_at = CASE WHEN $2 = 1 THEN $6 ELSE 0 END,
            resolution = CASE WHEN $2 = 1 THEN resolution ELSE '' END
         WHERE rid = ANY($1) AND reportstatus <> $2 AND ($3 OR type <> 'post' OR id3 = ANY($4)) RETURNING rid",
    )
    .bind(&f.ids)
    .bind(status)
    .bind(fids.is_none())
    .bind(fids.unwrap_or_default())
    .bind(ctx.uid())
    .bind(now())
    .fetch_all(&ctx.app.db)
    .await?;
    for rid in changed {
        crate::routes::modreports::record_event(
            &ctx.app.db,
            rid,
            ctx.uid(),
            if status == 1 { "resolved" } else { "reopened" },
            "",
        )
        .await?;
    }
    ctx.app.mod_counts.invalidate_all();
    Ok(ctx.redirect("/modcp/reports", "The selected reports have been updated."))
}

// ---------------------------------------------------------------- moderation queue

#[derive(Deserialize, Default)]
pub struct QueueQuery {
    #[serde(default)]
    pub kind: String,
}

pub async fn modqueue(ctx: Ctx, Query(q): Query<QueueQuery>) -> AppResult<Response> {
    require_modcp(&ctx)?;
    let fids = mod_fids(&ctx);
    let all = fids.is_none();
    let f = fids.unwrap_or_default();
    let kind = if q.kind.is_empty() {
        "threads".to_string()
    } else {
        q.kind.clone()
    };
    let items: Vec<serde_json::Value> = match kind.as_str() {
        "posts" => {
            let rows: Vec<(i32, i32, String, String, i32, String, i64, i32)> = sqlx::query_as(
                "SELECT p.pid, p.tid, t.subject, p.message, p.uid, p.username, p.dateline, p.fid FROM posts p JOIN threads t ON t.tid = p.tid
                 WHERE p.visible = 0 AND p.pid <> t.firstpost AND ($1 OR p.fid = ANY($2)) ORDER BY p.dateline DESC LIMIT 100",
            )
            .bind(all)
            .bind(&f)
            .fetch_all(&ctx.app.db)
            .await?;
            rows.into_iter()
                .map(|(pid, tid, subj, msg, uid, name, dl, fid)| {
                    let html = crate::render::parse_with(&ctx.cache, &ctx.app.plugins, &crate::render::forum_parse_options(ctx.cache.forum(fid), None), &msg);
                    serde_json::json!({"id": pid, "tid": tid, "subject": subj, "html": html, "uid": uid, "username": name, "dateline": dl, "url": format!("/post/{pid}"), "forum": ctx.cache.forum(fid).map(|x| x.name.clone())})
                })
                .collect()
        }
        "attachments" => {
            let rows: Vec<(i32, String, i64, i32, i32, String, i64)> = sqlx::query_as(
                "SELECT a.aid, a.filename, a.filesize, a.pid, a.uid, COALESCE(u.username, ''), a.dateuploaded FROM attachments a
                 JOIN posts p ON p.pid = a.pid LEFT JOIN users u ON u.uid = a.uid WHERE NOT a.visible AND ($1 OR p.fid = ANY($2)) ORDER BY a.dateuploaded DESC LIMIT 100",
            )
            .bind(all)
            .bind(&f)
            .fetch_all(&ctx.app.db)
            .await?;
            rows.into_iter()
                .map(|(aid, name, size, pid, uid, uname, dl)| serde_json::json!({"id": aid, "subject": name, "size": size, "url": format!("/post/{pid}"), "uid": uid, "username": uname, "dateline": dl, "download": format!("/attachment/{aid}")}))
                .collect()
        }
        _ => {
            let rows: Vec<(i32, String, i32, String, i64, i32, String)> = sqlx::query_as(
                "SELECT t.tid, t.subject, t.uid, t.username, t.dateline, t.fid, COALESCE(p.message, '') FROM threads t LEFT JOIN posts p ON p.pid = t.firstpost
                 WHERE t.visible = 0 AND ($1 OR t.fid = ANY($2)) ORDER BY t.dateline DESC LIMIT 100",
            )
            .bind(all)
            .bind(&f)
            .fetch_all(&ctx.app.db)
            .await?;
            rows.into_iter()
                .map(|(tid, subj, uid, name, dl, fid, msg)| {
                    let html = crate::render::parse_with(&ctx.cache, &ctx.app.plugins, &crate::render::forum_parse_options(ctx.cache.forum(fid), None), &msg);
                    serde_json::json!({"id": tid, "subject": subj, "html": html, "uid": uid, "username": name, "dateline": dl, "url": url_thread(tid as i64, Some(&subj)), "forum": ctx.cache.forum(fid).map(|x| x.name.clone())})
                })
                .collect()
        }
    };
    page(
        &ctx,
        "modcp/modqueue.html",
        "modqueue",
        "Moderation Queue",
        minijinja::context! { kind => kind, items => items },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct QueueForm {
    #[serde(default, deserialize_with = "de::string")]
    pub kind: String,
    #[serde(default, deserialize_with = "de::vec_i32")]
    pub approve: Vec<i32>,
    #[serde(default, deserialize_with = "de::vec_i32")]
    pub delete: Vec<i32>,
}

pub async fn modqueue_action(ctx: Ctx, CsrfForm(f): CsrfForm<QueueForm>) -> AppResult<Response> {
    require_modcp(&ctx)?;
    let fids = mod_fids(&ctx);
    let all = fids.is_none();
    let fl = fids.unwrap_or_default();
    match f.kind.as_str() {
        "posts" => {
            let ok = |ids: Vec<i32>| {
                let ctx = ctx.clone();
                let fl = fl.clone();
                async move {
                    Ok::<Vec<i32>, AppError>(
                        sqlx::query_scalar(
                            "SELECT pid FROM posts WHERE pid = ANY($1) AND ($2 OR fid = ANY($3))",
                        )
                        .bind(&ids)
                        .bind(all)
                        .bind(&fl)
                        .fetch_all(&ctx.app.db)
                        .await?,
                    )
                }
            };
            let a = ok(f.approve.clone()).await?;
            let d = ok(f.delete.clone()).await?;
            if !a.is_empty() {
                crate::ops::set_posts_visibility(&ctx.app, &a, 1).await?;
            }
            if !d.is_empty() {
                crate::ops::delete_posts(&ctx.app, &d).await?;
            }
        }
        "attachments" => {
            sqlx::query("UPDATE attachments SET visible = TRUE WHERE aid = ANY($1)")
                .bind(&f.approve)
                .execute(&ctx.app.db)
                .await?;
            let files: Vec<(String, String)> = sqlx::query_as("DELETE FROM attachments WHERE aid = ANY($1) AND NOT visible RETURNING attachname, thumbnail").bind(&f.delete).fetch_all(&ctx.app.db).await?;
            for (a, t) in files {
                let _ = tokio::fs::remove_file(format!("{}/{a}", ctx.app.cfg.upload_dir)).await;
                if !t.is_empty() {
                    let _ = tokio::fs::remove_file(format!("{}/{t}", ctx.app.cfg.upload_dir)).await;
                }
            }
        }
        _ => {
            let ok = |ids: Vec<i32>| {
                let ctx = ctx.clone();
                let fl = fl.clone();
                async move {
                    Ok::<Vec<i32>, AppError>(
                        sqlx::query_scalar(
                            "SELECT tid FROM threads WHERE tid = ANY($1) AND ($2 OR fid = ANY($3))",
                        )
                        .bind(&ids)
                        .bind(all)
                        .bind(&fl)
                        .fetch_all(&ctx.app.db)
                        .await?,
                    )
                }
            };
            let a = ok(f.approve.clone()).await?;
            let d = ok(f.delete.clone()).await?;
            if !a.is_empty() {
                crate::ops::set_threads_visibility(&ctx.app, &a, 1).await?;
            }
            if !d.is_empty() {
                crate::ops::delete_threads(&ctx.app, &d).await?;
            }
        }
    }
    crate::ops::log_moderator_action(
        &ctx.app,
        ctx.uid(),
        &ctx.ip,
        0,
        0,
        0,
        "Moderation queue",
        serde_json::json!({"kind": f.kind, "approved": f.approve, "deleted": f.delete}),
    )
    .await;
    ctx.app.mod_counts.invalidate_all();
    Ok(ctx.redirect(
        &format!("/modcp/modqueue?kind={}", f.kind),
        "The moderation queue has been updated.",
    ))
}

// ---------------------------------------------------------------- logs

pub async fn load_logs(
    ctx: &Ctx,
    uid: i32,
    tid: i32,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<minijinja::Value>> {
    let rows: Vec<(i64, i32, i64, i32, i32, i32, String, serde_json::Value, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT l.id, l.uid, l.dateline, l.fid, l.tid, l.pid, l.action, l.data, l.ipaddress, u.username, t.subject FROM moderatorlog l
         LEFT JOIN users u ON u.uid = l.uid LEFT JOIN threads t ON t.tid = l.tid
         WHERE ($1 = 0 OR l.uid = $1) AND ($2 = 0 OR l.tid = $2) ORDER BY l.id DESC LIMIT $3 OFFSET $4",
    )
    .bind(uid)
    .bind(tid)
    .bind(limit)
    .bind(offset)
    .fetch_all(&ctx.app.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, uid, dl, fid, tid, pid, action, data, ip, uname, subj)| {
            minijinja::context! { id => id, uid => uid, dateline => dl, fid => fid, tid => tid, pid => pid, action => action, data => data.to_string(), ip => ip,
                username => uname, subject => subj.or_else(|| data.get("subject").and_then(|s| s.as_str()).map(|s| s.to_string())), forum => ctx.cache.forum(fid).map(|f| f.name.clone()) }
        })
        .collect())
}

#[derive(Deserialize, Default)]
pub struct LogQuery {
    pub page: Option<i64>,
    pub uid: Option<i32>,
    pub tid: Option<i32>,
}

pub async fn modlogs(ctx: Ctx, Query(q): Query<LogQuery>) -> AppResult<Response> {
    require_modcp(&ctx)?;
    if !ctx.perms.canviewmodlogs && !ctx.is_any_mod() {
        return Err(AppError::no_perm());
    }
    let (uid, tid) = (q.uid.unwrap_or(0), q.tid.unwrap_or(0));
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moderatorlog WHERE ($1 = 0 OR uid = $1) AND ($2 = 0 OR tid = $2)",
    )
    .bind(uid)
    .bind(tid)
    .fetch_one(&ctx.app.db)
    .await?;
    let pg = util::paginate(
        total,
        50,
        util::clamp_page(q.page),
        &format!("/modcp/modlogs?uid={uid}&tid={tid}&page={{page}}"),
    );
    let logs = load_logs(&ctx, uid, tid, 50, (pg.page - 1) * 50).await?;
    page(
        &ctx,
        "modcp/modlogs.html",
        "modlogs",
        "Moderator Logs",
        minijinja::context! { logs => logs, pagination => pg },
    )
    .await
}

// ---------------------------------------------------------------- announcements

fn can_announce(ctx: &Ctx, fid: i32) -> bool {
    if fid <= 0 {
        return ctx.perms.canmanageannounce || ctx.is_admin();
    }
    ctx.perms.canmanageannounce
        || ctx
            .mod_perms(fid)
            .map(|m| m.canmanageannouncements)
            .unwrap_or(false)
}

pub async fn announcements(ctx: Ctx) -> AppResult<Response> {
    require_modcp(&ctx)?;
    let list: Vec<_> = ctx
        .cache
        .announcements
        .iter()
        .filter(|a| can_announce(&ctx, a.fid))
        .map(|a| minijinja::context! { aid => a.aid, subject => &a.subject, fid => a.fid, forum => ctx.cache.forum(a.fid).map(|f| f.name.clone()), startdate => a.startdate, enddate => a.enddate, active => a.startdate <= now() && (a.enddate == 0 || a.enddate > now()) })
        .collect();
    page(
        &ctx,
        "modcp/announcements.html",
        "announcements",
        "Announcements",
        minijinja::context! { list => list, can_global => can_announce(&ctx, -1) },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct AnnQuery {
    pub aid: Option<i32>,
    pub fid: Option<i32>,
}

pub async fn announcement_form(ctx: Ctx, Query(q): Query<AnnQuery>) -> AppResult<Response> {
    require_modcp(&ctx)?;
    let a = q.aid.and_then(|id| {
        ctx.cache
            .announcements
            .iter()
            .find(|a| a.aid == id)
            .cloned()
    });
    if let Some(a) = &a {
        if !can_announce(&ctx, a.fid) {
            return Err(AppError::no_perm());
        }
    }
    let forums: Vec<(i32, String, usize)> = crate::routes::forumdisplay::forum_jump(&ctx)
        .into_iter()
        .filter(|f| can_announce(&ctx, f.0))
        .collect();
    let fmt = |ts: i64| {
        if ts > 0 {
            util::to_local(ts, ctx.tz)
                .format("%Y-%m-%dT%H:%M")
                .to_string()
        } else {
            String::new()
        }
    };
    page(
        &ctx,
        "modcp/announcement_form.html",
        "announcements",
        "Edit Announcement",
        minijinja::context! { a => &a, forums => forums, can_global => can_announce(&ctx, -1),
            can_post_as_system => ctx.perms.canpostassystem, by_system => a.as_ref().map(|x| ctx.cache.is_system(x.uid)).unwrap_or(false), start => fmt(a.as_ref().map(|x| x.startdate).unwrap_or(now())), end => fmt(a.as_ref().map(|x| x.enddate).unwrap_or(0)), fid => q.fid.unwrap_or(-1) },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct AnnForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub aid: i32,
    #[serde(default, deserialize_with = "de::i32")]
    pub fid: i32,
    #[serde(default, deserialize_with = "de::string")]
    pub subject: String,
    #[serde(default, deserialize_with = "de::string")]
    pub message: String,
    #[serde(default, deserialize_with = "de::string")]
    pub startdate: String,
    #[serde(default, deserialize_with = "de::string")]
    pub enddate: String,
    #[serde(default, deserialize_with = "de::bool")]
    pub allowhtml: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub allowmycode: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub allowsmilies: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub as_system: bool,
}

pub fn parse_local_datetime(ctx: &Ctx, s: &str) -> Option<i64> {
    use chrono::TimeZone;
    let dt = chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%dT%H:%M")
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
                .map(|d| d.and_hms_opt(0, 0, 0).unwrap())
        })
        .ok()?;
    ctx.tz
        .from_local_datetime(&dt)
        .earliest()
        .map(|d| d.timestamp())
}

pub async fn announcement_save(ctx: Ctx, CsrfForm(f): CsrfForm<AnnForm>) -> AppResult<Response> {
    require_modcp(&ctx)?;
    if !can_announce(&ctx, f.fid) {
        return Err(AppError::no_perm());
    }
    if f.subject.trim().is_empty() || f.message.trim().is_empty() {
        return Err(AppError::user("Please enter a subject and message."));
    }
    let start = parse_local_datetime(&ctx, &f.startdate).unwrap_or_else(now);
    let end = parse_local_datetime(&ctx, &f.enddate).unwrap_or(0);
    let allowhtml = f.allowhtml && ctx.is_admin();
    if f.as_system && !ctx.perms.canpostassystem {
        return Err(AppError::no_perm());
    }
    let system = if f.as_system {
        Some(crate::system::identity(&ctx.app).await?.0)
    } else {
        None
    };
    let mut tx = ctx.app.db.begin().await?;
    let aid = if f.aid > 0 {
        let existing = ctx
            .cache
            .announcements
            .iter()
            .find(|a| a.aid == f.aid)
            .ok_or_else(|| AppError::not_found("announcement"))?;
        if !can_announce(&ctx, existing.fid) {
            return Err(AppError::no_perm());
        }
        // Only staff allowed to speak as System decide whether it signs the announcement;
        // anyone else's edit keeps the current author.
        let author = match system {
            Some(sys) => sys,
            None if ctx.perms.canpostassystem && ctx.cache.is_system(existing.uid) => ctx.uid(),
            None => existing.uid,
        };
        sqlx::query("UPDATE announcements SET fid = $2, subject = $3, message = $4, startdate = $5, enddate = $6, allowhtml = $7, allowmycode = $8, allowsmilies = $9, uid = $10 WHERE aid = $1")
            .bind(f.aid)
            .bind(f.fid)
            .bind(f.subject.trim())
            .bind(f.message.trim())
            .bind(start)
            .bind(end)
            .bind(allowhtml)
            .bind(f.allowmycode)
            .bind(f.allowsmilies)
            .bind(author)
            .execute(&mut *tx)
            .await?;
        f.aid
    } else {
        sqlx::query_scalar("INSERT INTO announcements (fid, uid, subject, message, startdate, enddate, allowhtml, allowmycode, allowsmilies) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING aid")
            .bind(f.fid)
            .bind(system.unwrap_or(ctx.uid()))
            .bind(f.subject.trim())
            .bind(f.message.trim())
            .bind(start)
            .bind(end)
            .bind(allowhtml)
            .bind(f.allowmycode)
            .bind(f.allowsmilies)
            .fetch_one(&mut *tx)
            .await?
    };
    if system.is_some() {
        crate::system::record(
            &mut tx,
            crate::system::Authorship {
                kind: "announcement",
                ref_id: aid,
                actor: ctx.uid(),
                actor_name: ctx.username(),
                ip: &ctx.ip,
                summary: f.subject.trim(),
            },
        )
        .await?;
    }
    tx.commit().await?;
    ctx.app.invalidate(&["announcements"]).await?;
    Ok(ctx.redirect("/modcp/announcements", "The announcement has been saved."))
}

#[derive(Deserialize, Default)]
pub struct AidForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub aid: i32,
}

pub async fn announcement_delete(ctx: Ctx, CsrfForm(f): CsrfForm<AidForm>) -> AppResult<Response> {
    require_modcp(&ctx)?;
    let a = ctx
        .cache
        .announcements
        .iter()
        .find(|a| a.aid == f.aid)
        .ok_or_else(|| AppError::not_found("announcement"))?;
    if !can_announce(&ctx, a.fid) {
        return Err(AppError::no_perm());
    }
    sqlx::query("DELETE FROM announcements WHERE aid = $1")
        .bind(f.aid)
        .execute(&ctx.app.db)
        .await?;
    ctx.app.invalidate(&["announcements"]).await?;
    Ok(ctx.redirect("/modcp/announcements", "The announcement has been deleted."))
}

// ---------------------------------------------------------------- users

#[derive(Deserialize, Default)]
pub struct FindQuery {
    #[serde(default)]
    pub username: String,
    pub page: Option<i64>,
}

pub async fn finduser(ctx: Ctx, Query(q): Query<FindQuery>) -> AppResult<Response> {
    require_modcp(&ctx)?;
    let rows: Vec<(i32, String, i32, i32, String, i32, i64, i64)> = if q.username.trim().is_empty()
    {
        vec![]
    } else {
        sqlx::query_as("SELECT uid, username, usergroup, displaygroup, email, postnum, regdate, lastactive FROM users WHERE username ILIKE '%' || $1 || '%' ORDER BY lower(username) LIMIT 100")
            .bind(q.username.trim())
            .fetch_all(&ctx.app.db)
            .await?
    };
    let list: Vec<_> = rows
        .into_iter()
        .map(|(uid, name, g, d, _email, posts, reg, last)| minijinja::context! { uid => uid, username => &name, formatted => ctx.cache.format_name(&name, g, d), postnum => posts, regdate => reg, lastactive => last })
        .collect();
    page(
        &ctx,
        "modcp/finduser.html",
        "finduser",
        "Find Users",
        minijinja::context! { list => list, username => q.username },
    )
    .await
}

async fn load_user(ctx: &Ctx, uid: i32) -> AppResult<User> {
    sqlx::query_as("SELECT * FROM users WHERE uid = $1")
        .bind(uid)
        .fetch_optional(&ctx.app.db)
        .await?
        .ok_or_else(|| AppError::not_found("user"))
}

/// Moderators may not edit/ban users in groups with more power (admins, super mods).
fn can_act_on(ctx: &Ctx, target: &User) -> bool {
    let tp = ctx.cache.group_perms(&target.all_groups());
    if tp.cancp && !ctx.perms.cancp {
        return false;
    }
    if tp.issupermod && !(ctx.perms.issupermod || ctx.perms.cancp) {
        return false;
    }
    target.uid != ctx.uid() || ctx.is_admin()
}

pub async fn editprofile_form(ctx: Ctx, Path(uid): Path<i32>) -> AppResult<Response> {
    require_modcp(&ctx)?;
    if !ctx.perms.caneditprofiles {
        return Err(AppError::no_perm());
    }
    let user = load_user(&ctx, uid).await?;
    if !can_act_on(&ctx, &user) {
        return Err(AppError::user("You cannot edit this user's profile."));
    }
    let values: std::collections::HashMap<String, String> =
        sqlx::query_as::<_, (i32, String)>("SELECT fid, value FROM userfields WHERE uid = $1")
            .bind(uid)
            .fetch_all(&ctx.app.db)
            .await?
            .into_iter()
            .map(|(f, v)| (f.to_string(), v))
            .collect();
    let fields = ctx.cache.profilefields.to_vec();
    page(
        &ctx,
        "modcp/editprofile.html",
        "finduser",
        &format!("Edit Profile: {}", user.username),
        minijinja::context! { user => &user, fields => fields, values => values },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct EditProfileForm {
    #[serde(default, deserialize_with = "de::string")]
    pub usertitle: String,
    #[serde(default, deserialize_with = "de::string")]
    pub website: String,
    #[serde(default, deserialize_with = "de::string")]
    pub signature: String,
    #[serde(default, deserialize_with = "de::bool")]
    pub removeavatar: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub suspendposting: bool,
    #[serde(default, deserialize_with = "de::i64")]
    pub suspendposting_days: i64,
    #[serde(default, deserialize_with = "de::bool")]
    pub moderateposts: bool,
    #[serde(default, deserialize_with = "de::i64")]
    pub moderateposts_days: i64,
    #[serde(default, deserialize_with = "de::bool")]
    pub suspendsignature: bool,
    #[serde(default, deserialize_with = "de::i64")]
    pub suspendsignature_days: i64,
    #[serde(default, flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

pub async fn editprofile_save(
    ctx: Ctx,
    Path(uid): Path<i32>,
    CsrfForm(f): CsrfForm<EditProfileForm>,
) -> AppResult<Response> {
    require_modcp(&ctx)?;
    if !ctx.perms.caneditprofiles {
        return Err(AppError::no_perm());
    }
    let user = load_user(&ctx, uid).await?;
    if !can_act_on(&ctx, &user) {
        return Err(AppError::user("You cannot edit this user's profile."));
    }
    // The System account is never suspended or moderated.
    let sys = user.is_system;
    let until = |on: bool, days: i64| {
        if !sys && on && days > 0 {
            now() + days * 86400
        } else {
            0
        }
    };
    sqlx::query(
        "UPDATE users SET usertitle = $2, website = $3, signature = $4,
            suspendposting = $5, suspensiontime = $6, moderateposts = $7, moderationtime = $8, suspendsignature = $9, suspendsigtime = $10,
            avatar = CASE WHEN $11 THEN '' ELSE avatar END, avatartype = CASE WHEN $11 THEN '' ELSE avatartype END
         WHERE uid = $1",
    )
    .bind(uid)
    .bind(f.usertitle.trim())
    .bind(f.website.trim())
    .bind(f.signature.trim())
    .bind(!sys && f.suspendposting)
    .bind(until(f.suspendposting, f.suspendposting_days))
    .bind(!sys && f.moderateposts)
    .bind(until(f.moderateposts, f.moderateposts_days))
    .bind(!sys && f.suspendsignature)
    .bind(until(f.suspendsignature, f.suspendsignature_days))
    .bind(f.removeavatar)
    .execute(&ctx.app.db)
    .await?;
    let mut errors = vec![];
    let vals = crate::routes::usercp::collect_profile_fields(&ctx, &f.extra, &mut errors, false);
    if errors.is_empty() {
        crate::routes::usercp::save_profile_fields(&ctx.app.db, uid, &vals).await?;
    }
    crate::ops::log_moderator_action(
        &ctx.app,
        ctx.uid(),
        &ctx.ip,
        0,
        0,
        0,
        "Edited user profile",
        serde_json::json!({"uid": uid, "username": user.username}),
    )
    .await;
    crate::audit::log(&ctx, uid, "staff_edit", serde_json::json!({"via": "modcp"})).await;
    Ok(ctx.redirect(
        &format!("/modcp/editprofile/{uid}"),
        "The profile has been updated.",
    ))
}

// ---------------------------------------------------------------- bans

pub async fn banning(ctx: Ctx) -> AppResult<Response> {
    require_modcp(&ctx)?;
    if !ctx.perms.canbanusers {
        return Err(AppError::no_perm());
    }
    let rows: Vec<(i32, String, String, String, i64, i64, Option<String>)> = sqlx::query_as(
        "SELECT b.uid, u.username, b.reason, b.bantime, b.dateline, b.lifted, a.username FROM banned b JOIN users u ON u.uid = b.uid LEFT JOIN users a ON a.uid = b.admin ORDER BY b.dateline DESC LIMIT 500",
    )
    .fetch_all(&ctx.app.db)
    .await?;
    page(
        &ctx,
        "modcp/banning.html",
        "banning",
        "Banned Users",
        minijinja::context! { bans => rows },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct UidQuery {
    pub uid: Option<i32>,
}

pub async fn ban_form(ctx: Ctx, Query(q): Query<UidQuery>) -> AppResult<Response> {
    require_modcp(&ctx)?;
    if !ctx.perms.canbanusers {
        return Err(AppError::no_perm());
    }
    let user = match q.uid {
        Some(u) => Some(load_user(&ctx, u).await?),
        None => None,
    };
    let banned_groups: Vec<(i32, String)> = ctx
        .cache
        .groups
        .values()
        .filter(|g| g.isbannedgroup)
        .map(|g| (g.gid, g.title.clone()))
        .collect();
    page(
        &ctx,
        "modcp/ban.html",
        "banning",
        "Ban User",
        minijinja::context! { user => user, groups => banned_groups },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct BanForm {
    #[serde(default, deserialize_with = "de::string")]
    pub username: String,
    #[serde(default, deserialize_with = "de::string")]
    pub reason: String,
    #[serde(default, deserialize_with = "de::i64")]
    pub days: i64,
    #[serde(default, deserialize_with = "de::i32")]
    pub gid: i32,
    #[serde(default, deserialize_with = "de::bool")]
    pub deleteposts: bool,
}

/// Ban a user (moves them to a banned group; restored when the ban lifts).
pub async fn ban_user(
    app: &App,
    target: &User,
    gid: i32,
    reason: &str,
    days: i64,
    admin: i32,
) -> AppResult<()> {
    crate::system::guard(&app.cache(), target.uid, "banned")?;
    let lifted = if days > 0 { now() + days * 86400 } else { 0 };
    let bantime = if days > 0 {
        format!("{days}-0-0")
    } else {
        "---".to_string()
    };
    let (oldgroup, oldag, olddg) = if app
        .cache()
        .group(target.usergroup)
        .map(|g| g.isbannedgroup)
        .unwrap_or(false)
    {
        // already banned: keep the original groups
        sqlx::query_as::<_, (i32, Vec<i32>, i32)>(
            "SELECT oldgroup, oldadditionalgroups, olddisplaygroup FROM banned WHERE uid = $1",
        )
        .bind(target.uid)
        .fetch_optional(&app.db)
        .await?
        .unwrap_or((2, vec![], 0))
    } else {
        (
            target.usergroup,
            target.additionalgroups.clone(),
            target.displaygroup,
        )
    };
    let mut tx = app.db.begin().await?;
    sqlx::query(
        "INSERT INTO banned (uid, gid, oldgroup, oldadditionalgroups, olddisplaygroup, admin, dateline, bantime, lifted, reason) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (uid) DO UPDATE SET gid = $2, admin = $6, dateline = $7, bantime = $8, lifted = $9, reason = $10",
    )
    .bind(target.uid)
    .bind(gid)
    .bind(oldgroup)
    .bind(&oldag)
    .bind(olddg)
    .bind(admin)
    .bind(now())
    .bind(bantime)
    .bind(lifted)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE users SET usergroup = $2, additionalgroups = '{}', displaygroup = 0 WHERE uid = $1",
    )
    .bind(target.uid)
    .bind(gid)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM logins WHERE uid = $1")
        .bind(target.uid)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn ban_save(ctx: Ctx, CsrfForm(f): CsrfForm<BanForm>) -> AppResult<Response> {
    require_modcp(&ctx)?;
    if !ctx.perms.canbanusers {
        return Err(AppError::no_perm());
    }
    let uid: i32 = sqlx::query_scalar("SELECT uid FROM users WHERE lower(username) = lower($1)")
        .bind(f.username.trim())
        .fetch_optional(&ctx.app.db)
        .await?
        .ok_or_else(|| AppError::not_found("user"))?;
    let user = load_user(&ctx, uid).await?;
    if !can_act_on(&ctx, &user) || user.uid == ctx.uid() {
        return Err(AppError::user("You cannot ban this user."));
    }
    let gid = if ctx
        .cache
        .group(f.gid)
        .map(|g| g.isbannedgroup)
        .unwrap_or(false)
    {
        f.gid
    } else {
        7
    };
    ban_user(
        &ctx.app,
        &user,
        gid,
        f.reason.trim(),
        f.days.max(0),
        ctx.uid(),
    )
    .await?;
    if f.deleteposts {
        crate::routes::usercp::delete_user_content(&ctx.app, uid).await?;
    }
    crate::ops::log_moderator_action(&ctx.app, ctx.uid(), &ctx.ip, 0, 0, 0, "Banned user", serde_json::json!({"uid": uid, "username": user.username, "reason": f.reason, "days": f.days})).await;
    crate::audit::log(&ctx, uid, "banned", serde_json::json!({"reason": f.reason})).await;
    Ok(ctx.redirect(
        "/modcp/banning",
        &format!("{} has been banned.", user.username),
    ))
}

#[derive(Deserialize, Default)]
pub struct LiftForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub uid: i32,
}

pub async fn lift_ban_for(app: &App, uid: i32) -> AppResult<()> {
    let row: Option<(i32, Vec<i32>, i32)> =
        sqlx::query_as("DELETE FROM banned WHERE uid = $1 RETURNING oldgroup, oldadditionalgroups, olddisplaygroup").bind(uid).fetch_optional(&app.db).await?;
    if let Some((g, ag, dg)) = row {
        sqlx::query("UPDATE users SET usergroup = $2, additionalgroups = $3, displaygroup = $4 WHERE uid = $1").bind(uid).bind(g).bind(ag).bind(dg).execute(&app.db).await?;
    }
    Ok(())
}

/// Refuse to lift a ban on someone who would outrank the viewer once it's lifted: lifting
/// restores the groups they had before the ban, so judge them by those.
pub async fn check_can_lift(ctx: &Ctx, uid: i32) -> AppResult<()> {
    let old: Option<(i32, Vec<i32>)> =
        sqlx::query_as("SELECT oldgroup, oldadditionalgroups FROM banned WHERE uid = $1")
            .bind(uid)
            .fetch_optional(&ctx.app.db)
            .await?;
    if let Some((group, additional)) = old {
        let mut target = load_user(ctx, uid).await?;
        target.usergroup = group;
        target.additionalgroups = additional;
        if !can_act_on(ctx, &target) {
            return Err(AppError::user("You cannot lift this ban."));
        }
    }
    Ok(())
}

/// Lift a ban and record it in the moderator log and the member's audit log.
pub async fn lift_ban_logged(ctx: &Ctx, uid: i32) -> AppResult<()> {
    lift_ban_for(&ctx.app, uid).await?;
    crate::ops::log_moderator_action(
        &ctx.app,
        ctx.uid(),
        &ctx.ip,
        0,
        0,
        0,
        "Lifted ban",
        serde_json::json!({"uid": uid}),
    )
    .await;
    crate::audit::log(ctx, uid, "unbanned", serde_json::Value::Null).await;
    Ok(())
}

pub async fn lift_ban(ctx: Ctx, CsrfForm(f): CsrfForm<LiftForm>) -> AppResult<Response> {
    require_modcp(&ctx)?;
    if !ctx.perms.canbanusers {
        return Err(AppError::no_perm());
    }
    check_can_lift(&ctx, f.uid).await?;
    lift_ban_logged(&ctx, f.uid).await?;
    Ok(ctx.redirect("/modcp/banning", "The ban has been lifted."))
}

// ---------------------------------------------------------------- IP search

#[derive(Deserialize, Default)]
pub struct IpQuery {
    #[serde(default)]
    pub ip: String,
}

pub async fn ipsearch(ctx: Ctx, Query(q): Query<IpQuery>) -> AppResult<Response> {
    require_modcp(&ctx)?;
    if !ctx.perms.canuseipsearch {
        return Err(AppError::no_perm());
    }
    let ip = q.ip.trim().to_string();
    let (users, posts) = if ip.is_empty() {
        (vec![], vec![])
    } else {
        let pat = if ip.contains('*') {
            ip.replace('*', "%")
        } else {
            ip.clone()
        };
        let users: Vec<(i32, String, String, String)> = sqlx::query_as("SELECT uid, username, regip, lastip FROM users WHERE regip LIKE $1 OR lastip LIKE $1 ORDER BY uid LIMIT 200")
            .bind(&pat)
            .fetch_all(&ctx.app.db)
            .await?;
        let posts: Vec<(i32, i32, String, String, i64)> = sqlx::query_as("SELECT pid, uid, username, subject, dateline FROM posts WHERE ipaddress LIKE $1 ORDER BY pid DESC LIMIT 200")
            .bind(&pat)
            .fetch_all(&ctx.app.db)
            .await?;
        (users, posts)
    };
    page(
        &ctx,
        "modcp/ipsearch.html",
        "ipsearch",
        "IP Search",
        minijinja::context! { ip => ip, users => users, posts => posts },
    )
    .await
}

pub async fn warninglogs(ctx: Ctx, Query(q): Query<LogQuery>) -> AppResult<Response> {
    require_modcp(&ctx)?;
    if !ctx.perms.canviewwarnlogs {
        return Err(AppError::no_perm());
    }
    let rows: Vec<(i32, i32, String, String, i32, i64, i64, bool, i64, Option<String>)> = sqlx::query_as(
        "SELECT w.wid, w.uid, u.username, w.title, w.points, w.dateline, w.expires, w.expired, w.daterevoked, i.username FROM warnings w
         JOIN users u ON u.uid = w.uid LEFT JOIN users i ON i.uid = w.issuedby WHERE ($1 = 0 OR w.uid = $1) ORDER BY w.dateline DESC LIMIT 200",
    )
    .bind(q.uid.unwrap_or(0))
    .fetch_all(&ctx.app.db)
    .await?;
    page(
        &ctx,
        "modcp/warninglogs.html",
        "warninglogs",
        "Warning Logs",
        minijinja::context! { warnings => rows },
    )
    .await
}

pub async fn delayed(ctx: Ctx) -> AppResult<Response> {
    require_modcp(&ctx)?;
    let rows: Vec<(i32, String, i64, Vec<i32>, Option<String>)> = sqlx::query_as(
        "SELECT d.did, d.type, d.delaydateline, d.tids, u.username FROM delayedmoderation d LEFT JOIN users u ON u.uid = d.uid ORDER BY d.delaydateline",
    )
    .fetch_all(&ctx.app.db)
    .await?;
    page(
        &ctx,
        "modcp/delayed.html",
        "delayed",
        "Scheduled Moderation",
        minijinja::context! { rows => rows },
    )
    .await
}
