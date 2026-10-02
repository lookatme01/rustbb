//! Report claiming, resolution notes and report history (Mod CP).
//!
//! Claims are advisory: they show who is handling a report so two moderators don't duplicate
//! work. Anyone allowed to manage a report can still resolve it; every step is recorded.

use crate::app::App;
use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::util::now;
use axum::Router;
use axum::extract::Path;
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;

const MAX_RESOLUTION_CHARS: usize = 1000;

pub fn router() -> Router<App> {
    Router::new()
        .route("/modcp/reports/{rid}", get(detail))
        .route("/modcp/reports/{rid}/claim", post(claim))
        .route("/modcp/reports/{rid}/resolve", post(resolve))
        .route("/modcp/reports/{rid}/reopen", post(reopen))
}

/// Access to some kind of report (see `domain::staff::ReportScope`).
pub fn require_reports(ctx: &Ctx) -> AppResult<()> {
    ctx.require_login()?;
    if !ctx.can(crate::domain::staff::Cap::ModCp) || !ctx.staff().report_scope().any() {
        return Err(AppError::no_perm());
    }
    Ok(())
}

pub async fn record_event(
    db: &sqlx::PgPool,
    rid: i32,
    uid: i32,
    action: &str,
    note: &str,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO report_events (rid, uid, action, note, dateline) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(rid)
    .bind(uid)
    .bind(action)
    .bind(note)
    .bind(now())
    .execute(db)
    .await?;
    Ok(())
}

pub fn event_label(action: &str, note: &str) -> String {
    match action {
        "claimed" => "Claimed".into(),
        "released" => "Released the claim".into(),
        "taken_over" => format!("Took over from {note}"),
        "resolved" => "Resolved".into(),
        "reopened" => "Reopened".into(),
        other => other.into(),
    }
}

#[derive(sqlx::FromRow)]
struct Report {
    rid: i32,
    id: i32,
    id2: i32,
    id3: i32,
    r#type: String,
    reportstatus: i16,
    reasonid: i32,
    reason: String,
    reports: i32,
    reporters: Vec<i32>,
    dateline: i64,
    lastreport: i64,
    claimed_by: i32,
    claimed_at: i64,
    resolved_by: i32,
    resolved_at: i64,
    resolution: String,
}

/// A report the viewer may manage: post reports are limited to the forums a moderator moderates.
async fn load(ctx: &Ctx, rid: i32) -> AppResult<Report> {
    require_reports(ctx)?;
    let r: Report = sqlx::query_as(
        "SELECT rid, id, id2, id3, type, reportstatus, reasonid, reason, reports, reporters, dateline, lastreport,
                COALESCE(claimed_by, 0) AS claimed_by, claimed_at, COALESCE(resolved_by, 0) AS resolved_by, resolved_at, resolution
         FROM reportedcontent WHERE rid = $1",
    )
    .bind(rid)
    .fetch_optional(&ctx.app.db)
    .await?
    .ok_or_else(|| AppError::not_found("report"))?;
    // Reports the viewer may not see do not exist for them (PM reports need their own
    // capability; post reports must be from a forum they moderate).
    if !ctx.staff().report_scope().allows(&r.r#type, r.id3) {
        return Err(AppError::not_found("report"));
    }
    Ok(r)
}

/// Who the report is about: the post's author, the profile owner, the rated member or the sender.
async fn target_uid(db: &sqlx::PgPool, r: &Report) -> i32 {
    if r.r#type == "post" {
        sqlx::query_scalar("SELECT uid FROM posts WHERE pid = $1")
            .bind(r.id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .unwrap_or(0)
    } else {
        r.id2
    }
}

async fn username(db: &sqlx::PgPool, uid: i32) -> String {
    if uid <= 0 {
        return String::new();
    }
    sqlx::query_scalar("SELECT username FROM users WHERE uid = $1")
        .bind(uid)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
}

pub async fn detail(ctx: Ctx, Path(rid): Path<i32>) -> AppResult<Response> {
    let r = load(&ctx, rid).await?;
    let db = &ctx.app.db;
    let target = target_uid(db, &r).await;
    let reporters: Vec<(i32, String)> =
        sqlx::query_as("SELECT uid, username FROM users WHERE uid = ANY($1) ORDER BY username")
            .bind(&r.reporters)
            .fetch_all(db)
            .await?;
    let events: Vec<(String, String, i64, Option<String>)> = sqlx::query_as(
        "SELECT e.action, e.note, e.dateline, u.username FROM report_events e LEFT JOIN users u ON u.uid = e.uid WHERE e.rid = $1 ORDER BY e.id",
    )
    .bind(rid)
    .fetch_all(db)
    .await?;
    let events: Vec<_> = events
        .into_iter()
        .map(|(action, note, when, who)| minijinja::context! {
            label => event_label(&action, &note), note => if action == "taken_over" { String::new() } else { note }, when => when, who => who,
        })
        .collect();
    let latest_note = if target > 0 {
        crate::routes::modnotes::latest_notes(&ctx.app, &[target])
            .await?
            .remove(&target)
    } else {
        None
    };
    let (link, what) = match r.r#type.as_str() {
        "post" => (Some(format!("/post/{}", r.id)), "Post".to_string()),
        "profile" => (Some(format!("/user/{}", r.id)), "Profile".to_string()),
        "reputation" => (
            Some(format!("/reputation/{}", r.id2)),
            "Reputation comment".to_string(),
        ),
        _ => (None, "Private message".to_string()),
    };
    let reason_title = ctx
        .cache
        .reportreasons
        .iter()
        .find(|x| x.rid == r.reasonid)
        .map(|x| x.title.clone())
        .unwrap_or_default();
    ctx.render(
        "modcp/report.html",
        minijinja::context! {
            title => format!("Report #{rid}"), mcp_active => "reports",
            breadcrumb => vec![("Mod CP".to_string(), "/modcp".to_string()), ("Reported content".to_string(), "/modcp/reports".to_string())],
            report => minijinja::context! {
                rid => r.rid, what => what, link => link, open => r.reportstatus == 0, reason_title => reason_title, comment => &r.reason,
                reports => r.reports, dateline => r.dateline, lastreport => r.lastreport,
                claimed_by => username(db, r.claimed_by).await, claimed_by_me => r.claimed_by == ctx.uid(), claimed => r.claimed_by > 0, claimed_at => r.claimed_at,
                resolved_by => username(db, r.resolved_by).await, resolved_at => r.resolved_at, resolution => &r.resolution,
            },
            target => minijinja::context! { uid => target, username => username(db, target).await },
            latest_note => latest_note, reporters => reporters, events => events,
        },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct ClaimForm {
    #[serde(default, deserialize_with = "de::string")]
    pub action: String,
}

pub async fn claim(
    ctx: Ctx,
    Path(rid): Path<i32>,
    CsrfForm(f): CsrfForm<ClaimForm>,
) -> AppResult<Response> {
    let r = load(&ctx, rid).await?;
    let db = &ctx.app.db;
    let me = ctx.uid();
    match f.action.as_str() {
        "claim" => {
            if r.claimed_by > 0 && r.claimed_by != me {
                return Err(AppError::user(format!(
                    "{} is already handling this report. Use “Take over” if you're taking it from them.",
                    username(db, r.claimed_by).await
                )));
            }
            if r.claimed_by != me {
                sqlx::query(
                    "UPDATE reportedcontent SET claimed_by = $2, claimed_at = $3 WHERE rid = $1",
                )
                .bind(rid)
                .bind(me)
                .bind(now())
                .execute(db)
                .await?;
                record_event(db, rid, me, "claimed", "").await?;
            }
        }
        "takeover" => {
            if r.claimed_by != me {
                let previous = username(db, r.claimed_by).await;
                sqlx::query(
                    "UPDATE reportedcontent SET claimed_by = $2, claimed_at = $3 WHERE rid = $1",
                )
                .bind(rid)
                .bind(me)
                .bind(now())
                .execute(db)
                .await?;
                record_event(
                    db,
                    rid,
                    me,
                    if r.claimed_by > 0 {
                        "taken_over"
                    } else {
                        "claimed"
                    },
                    &previous,
                )
                .await?;
            }
        }
        "release" => {
            if r.claimed_by == 0 {
                return Ok(ctx.redirect(&format!("/modcp/reports/{rid}"), ""));
            }
            if r.claimed_by != me && !ctx.is_admin() {
                return Err(AppError::NoPermission(
                    "Only the moderator handling this report (or an administrator) can release it."
                        .into(),
                ));
            }
            sqlx::query(
                "UPDATE reportedcontent SET claimed_by = NULL, claimed_at = 0 WHERE rid = $1",
            )
            .bind(rid)
            .execute(db)
            .await?;
            record_event(db, rid, me, "released", "").await?;
        }
        _ => return Err(AppError::user("Unknown action.")),
    }
    ctx.app.mod_counts.invalidate_all();
    Ok(ctx.redirect(&format!("/modcp/reports/{rid}"), ""))
}

#[derive(Deserialize, Default)]
pub struct ResolveForm {
    #[serde(default, deserialize_with = "de::string")]
    pub resolution: String,
}

pub async fn resolve(
    ctx: Ctx,
    Path(rid): Path<i32>,
    CsrfForm(f): CsrfForm<ResolveForm>,
) -> AppResult<Response> {
    let r = load(&ctx, rid).await?;
    let note: String = f
        .resolution
        .trim()
        .chars()
        .take(MAX_RESOLUTION_CHARS)
        .collect();
    if r.reportstatus == 0 {
        sqlx::query("UPDATE reportedcontent SET reportstatus = 1, resolved_by = $2, resolved_at = $3, resolution = $4 WHERE rid = $1")
            .bind(rid)
            .bind(ctx.uid())
            .bind(now())
            .bind(&note)
            .execute(&ctx.app.db)
            .await?;
        record_event(&ctx.app.db, rid, ctx.uid(), "resolved", &note).await?;
        ctx.app.mod_counts.invalidate_all();
    }
    Ok(ctx.redirect(
        &format!("/modcp/reports/{rid}"),
        "The report has been resolved.",
    ))
}

pub async fn reopen(
    ctx: Ctx,
    Path(rid): Path<i32>,
    CsrfForm(_f): CsrfForm<ResolveForm>,
) -> AppResult<Response> {
    let r = load(&ctx, rid).await?;
    if r.reportstatus != 0 {
        sqlx::query("UPDATE reportedcontent SET reportstatus = 0, resolved_by = NULL, resolved_at = 0, resolution = '' WHERE rid = $1")
            .bind(rid)
            .execute(&ctx.app.db)
            .await?;
        record_event(&ctx.app.db, rid, ctx.uid(), "reopened", "").await?;
        ctx.app.mod_counts.invalidate_all();
    }
    Ok(ctx.redirect(
        &format!("/modcp/reports/{rid}"),
        "The report has been reopened.",
    ))
}
