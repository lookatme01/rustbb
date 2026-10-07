//! Reporting posts, profiles, reputation comments and private messages to moderators.

use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::util::now;
use axum::extract::Query;
use axum::response::Response;
use serde::Deserialize;

#[derive(Deserialize, Default)]
pub struct ReportQuery {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub id: i32,
}

/// Resolve (id, id2, id3, description, end-to-end encrypted) for the reported object, checking
/// visibility. Only a private message can be encrypted.
async fn target(ctx: &Ctx, kind: &str, id: i32) -> AppResult<(i32, i32, i32, String, bool)> {
    match kind {
        "post" => {
            let (tid, fid, visible, subject): (i32, i32, i16, String) = sqlx::query_as("SELECT p.tid, p.fid, p.visible, t.subject FROM posts p JOIN threads t ON t.tid = p.tid WHERE p.pid = $1")
                .bind(id)
                .fetch_optional(&ctx.app.db)
                .await?
                .ok_or_else(|| AppError::not_found("post"))?;
            crate::routes::showthread::check_thread(ctx, tid).await?;
            if !ctx.visible_states(fid).contains(&visible) {
                return Err(AppError::not_found("post"));
            }
            Ok((id, tid, fid, format!("a post in “{subject}”"), false))
        }
        "profile" => {
            let (name, g): (String, i32) =
                sqlx::query_as("SELECT username, usergroup FROM users WHERE uid = $1")
                    .bind(id)
                    .fetch_optional(&ctx.app.db)
                    .await?
                    .ok_or_else(|| AppError::not_found("user"))?;
            if !ctx
                .cache
                .group(g)
                .map(|g| g.perms.0.canbereported)
                .unwrap_or(true)
            {
                return Err(AppError::user("This user cannot be reported."));
            }
            Ok((id, id, 0, format!("the profile of {name}"), false))
        }
        "reputation" => {
            let uid: i32 = sqlx::query_scalar("SELECT uid FROM reputation WHERE rid = $1")
                .bind(id)
                .fetch_optional(&ctx.app.db)
                .await?
                .ok_or_else(|| AppError::not_found("rating"))?;
            Ok((id, uid, 0, "a reputation comment".into(), false))
        }
        "pm" => {
            let (from, pgp): (i32, i16) = sqlx::query_as(
                "SELECT fromid, pgp FROM privatemessages WHERE pmid = $1 AND uid = $2",
            )
            .bind(id)
            .bind(ctx.uid())
            .fetch_optional(&ctx.app.db)
            .await?
            .ok_or_else(|| AppError::not_found("message"))?;
            if from == 0 || ctx.cache.is_system(from) {
                return Err(AppError::user(
                    "Automated messages from the System account can't be reported.",
                ));
            }
            Ok((id, from, 0, "a private message".into(), pgp == 2))
        }
        _ => Err(AppError::user("Unknown content type.")),
    }
}

pub async fn form(ctx: Ctx, Query(q): Query<ReportQuery>) -> AppResult<Response> {
    ctx.require_login()?;
    let (_, _, _, desc, encrypted) = target(&ctx, &q.r#type, q.id).await?;
    let reasons: Vec<_> = ctx
        .cache
        .reportreasons
        .iter()
        .filter(|r| r.appliesto == "all" || r.appliesto.split(',').any(|a| a.trim() == q.r#type))
        .cloned()
        .collect();
    ctx.render("report.html", minijinja::context! { title => "Report Content", kind => q.r#type, id => q.id, desc => desc, encrypted => encrypted, reasons => reasons }).await
}

#[derive(Deserialize, Default)]
pub struct ReportForm {
    #[serde(default, deserialize_with = "de::string")]
    pub r#type: String,
    #[serde(default, deserialize_with = "de::i32")]
    pub id: i32,
    #[serde(default, deserialize_with = "de::i32")]
    pub reason: i32,
    #[serde(default, deserialize_with = "de::string")]
    pub comment: String,
}

pub async fn submit(ctx: Ctx, CsrfForm(f): CsrfForm<ReportForm>) -> AppResult<Response> {
    let me = ctx.require_login()?.clone();
    if !ctx
        .app
        .throttle(&format!("report:{}", me.uid), 20, 3600)
        .await
    {
        return Err(AppError::RateLimited);
    }
    let (id, id2, id3, _, encrypted) = target(&ctx, &f.r#type, f.id).await?;
    let reason = ctx
        .cache
        .reportreasons
        .iter()
        .find(|r| r.rid == f.reason)
        .cloned()
        .ok_or_else(|| AppError::user("Please choose a reason."))?;
    let comment: String = f.comment.trim().chars().take(500).collect();
    if reason.extra && comment.is_empty() {
        return Err(AppError::user("Please describe the problem."));
    }
    // One open report per piece of content (a unique index enforces it): the first report
    // opens it, later ones join it — atomically, so simultaneous reports cannot open two.
    // A private-message report also freezes a copy of the message, in the same transaction, so
    // moderators can still read it after the reporter deletes it or their account.
    let mut tx = ctx.app.db.begin().await?;
    let joined: Option<i32> = sqlx::query_scalar(
        "INSERT INTO reportedcontent (id, id2, id3, uid, reasonid, reason, type, reports, reporters, dateline, lastreport)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 1, ARRAY[$4], $8, $8)
         ON CONFLICT (type, id) WHERE reportstatus = 0 DO UPDATE
            SET reports = reportedcontent.reports + 1,
                reporters = array_append(reportedcontent.reporters, $4),
                lastreport = $8
            WHERE NOT ($4 = ANY(reportedcontent.reporters))
         RETURNING rid",
    )
    .bind(id)
    .bind(id2)
    .bind(id3)
    .bind(me.uid)
    .bind(reason.rid)
    .bind(&comment)
    .bind(&f.r#type)
    .bind(now())
    .fetch_optional(&mut *tx)
    .await?;
    if let (Some(rid), "pm") = (joined, f.r#type.as_str()) {
        // Ciphertext is useless to the server, so an encrypted message keeps no body.
        sqlx::query(
            "INSERT INTO report_pm_snapshots (rid, fromid, subject, message, sent, smilieoff, encrypted)
             SELECT $1, fromid, subject, CASE WHEN $3 THEN '' ELSE message END, dateline, smilieoff, $3
             FROM privatemessages WHERE pmid = $2 AND uid = $4
             ON CONFLICT (rid) DO NOTHING",
        )
        .bind(rid)
        .bind(id)
        .bind(encrypted)
        .bind(me.uid)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    if joined.is_none() {
        return Ok(ctx.redirect("/", "You have already reported this content. Thank you."));
    }
    ctx.app.mod_counts.invalidate_all();
    let back = if f.r#type == "post" {
        format!("/post/{id}")
    } else {
        "/".to_string()
    };
    Ok(ctx.redirect(
        &back,
        "Thank you — the content has been reported to the moderators.",
    ))
}
