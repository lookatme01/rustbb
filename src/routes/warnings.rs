//! Warning system: warn users with points that expire; warning levels trigger automatic
//! actions (moderate posts, suspend posting, temporary ban).

use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::models::User;
use crate::util::now;
use axum::extract::{Path, Query};
use axum::response::Response;
use serde::Deserialize;

fn enabled(ctx: &Ctx) -> AppResult<()> {
    if !ctx.settings().bool("enablewarningsystem") {
        return Err(AppError::user("The warning system is disabled."));
    }
    Ok(())
}

async fn load_user(ctx: &Ctx, uid: i32) -> AppResult<User> {
    sqlx::query_as(&format!(
        "SELECT {} FROM users WHERE uid = $1",
        crate::models::USER_COLUMNS
    ))
    .bind(uid)
    .fetch_optional(&ctx.app.db)
    .await?
    .ok_or_else(|| AppError::not_found("user"))
}

pub async fn list(ctx: Ctx, Path(uid): Path<i32>) -> AppResult<Response> {
    enabled(&ctx)?;
    if !(ctx.perms.canwarnusers
        || ctx.perms.canviewwarnlogs
        || (uid == ctx.uid() && ctx.settings().bool("canviewownwarning")))
    {
        return Err(AppError::no_perm());
    }
    let user = load_user(&ctx, uid).await?;
    let rows: Vec<(i32, String, i32, i64, i64, bool, i64, i32, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT w.wid, w.title, w.points, w.dateline, w.expires, w.expired, w.daterevoked, w.pid, i.username, t.subject FROM warnings w
         LEFT JOIN users i ON i.uid = w.issuedby LEFT JOIN posts p ON p.pid = w.pid AND w.pid > 0 LEFT JOIN threads t ON t.tid = p.tid
         WHERE w.uid = $1 ORDER BY w.dateline DESC",
    )
    .bind(uid)
    .fetch_all(&ctx.app.db)
    .await?;
    let max = ctx.settings().int("maxwarningpoints").max(1);
    ctx.render(
        "warnings.html",
        minijinja::context! { title => format!("Warnings for {}", user.username), user => &user, warnings => rows, level => (user.warningpoints as i64 * 100 / max).min(100), can_warn => ctx.perms.canwarnusers && uid != ctx.uid() },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct WarnQuery {
    pub pid: Option<i32>,
}

pub async fn warn_form(
    ctx: Ctx,
    Path(uid): Path<i32>,
    Query(q): Query<WarnQuery>,
) -> AppResult<Response> {
    enabled(&ctx)?;
    if !ctx.perms.canwarnusers {
        return Err(AppError::no_perm());
    }
    let user = load_user(&ctx, uid).await?;
    let types: Vec<(i32, String, i32, i64)> = sqlx::query_as(
        "SELECT tid, title, points, expirationtime FROM warningtypes ORDER BY title",
    )
    .fetch_all(&ctx.app.db)
    .await?;
    let post: Option<(i32, String, String)> = match q.pid {
        Some(pid) => sqlx::query_as("SELECT p.pid, t.subject, p.message FROM posts p JOIN threads t ON t.tid = p.tid WHERE p.pid = $1 AND p.uid = $2").bind(pid).bind(uid).fetch_optional(&ctx.app.db).await?,
        None => None,
    };
    let max = ctx.settings().int("maxwarningpoints").max(1);
    ctx.render(
        "warn.html",
        minijinja::context! { title => format!("Warn {}", user.username), user => &user, types => types, post => post, custom => ctx.settings().bool("allowcustomwarnings"), level => (user.warningpoints as i64 * 100 / max).min(100) },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct WarnForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub warningtype: i32,
    #[serde(default, deserialize_with = "de::string")]
    pub custom_reason: String,
    #[serde(default, deserialize_with = "de::i32")]
    pub custom_points: i32,
    #[serde(default, deserialize_with = "de::i64")]
    pub expires_days: i64,
    #[serde(default, deserialize_with = "de::string")]
    pub notes: String,
    #[serde(default, deserialize_with = "de::i32")]
    pub pid: i32,
    #[serde(default, deserialize_with = "de::bool")]
    pub sendpm: bool,
    #[serde(default, deserialize_with = "de::string")]
    pub pm_message: String,
}

pub async fn warn_submit(
    ctx: Ctx,
    Path(uid): Path<i32>,
    CsrfForm(f): CsrfForm<WarnForm>,
) -> AppResult<Response> {
    enabled(&ctx)?;
    if !ctx.perms.canwarnusers {
        return Err(AppError::no_perm());
    }
    let me = ctx.me()?.clone();
    let user = load_user(&ctx, uid).await?;
    if uid == me.uid {
        return Err(AppError::user("You cannot warn yourself."));
    }
    let tperms = ctx.cache.group_perms(&user.all_groups());
    if !tperms.canreceivewarnings || tperms.cancp {
        return Err(AppError::user("This user cannot receive warnings."));
    }
    if ctx.perms.maxwarningsday > 0 {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM warnings WHERE issuedby = $1 AND dateline > $2",
        )
        .bind(me.uid)
        .bind(now() - 86400)
        .fetch_one(&ctx.app.db)
        .await?;
        if n >= ctx.perms.maxwarningsday as i64 {
            return Err(AppError::user(
                "You have reached your warning limit for today.",
            ));
        }
    }
    let (title, points, expires) = if f.warningtype > 0 {
        let (t, p, e): (String, i32, i64) =
            sqlx::query_as("SELECT title, points, expirationtime FROM warningtypes WHERE tid = $1")
                .bind(f.warningtype)
                .fetch_optional(&ctx.app.db)
                .await?
                .ok_or_else(|| AppError::not_found("warning type"))?;
        (t, p, if e > 0 { now() + e } else { 0 })
    } else {
        if !ctx.settings().bool("allowcustomwarnings") {
            return Err(AppError::user("Please choose a warning type."));
        }
        if f.custom_reason.trim().is_empty() || f.custom_points <= 0 {
            return Err(AppError::user(
                "Please enter a reason and points for the custom warning.",
            ));
        }
        (
            f.custom_reason.trim().to_string(),
            f.custom_points
                .min(ctx.settings().int("maxwarningpoints") as i32),
            if f.expires_days > 0 {
                now() + f.expires_days.min(36_500) * 86400
            } else {
                0
            },
        )
    };
    if f.pid == 0 && !ctx.settings().bool("allowwarningsnopost") {
        return Err(AppError::user("Warnings must be tied to a post."));
    }
    let wid: i32 = sqlx::query_scalar("INSERT INTO warnings (uid, tid, pid, title, points, dateline, issuedby, expires, notes) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING wid")
        .bind(uid)
        .bind(f.warningtype)
        .bind(f.pid)
        .bind(&title)
        .bind(points)
        .bind(now())
        .bind(me.uid)
        .bind(expires)
        .bind(f.notes.trim())
        .fetch_one(&ctx.app.db)
        .await?;
    let newpoints: i32 = sqlx::query_scalar("UPDATE users SET warningpoints = warningpoints + $2 WHERE uid = $1 RETURNING warningpoints").bind(uid).bind(points).fetch_one(&ctx.app.db).await?;
    // Warning levels.
    let max = ctx.settings().int("maxwarningpoints").max(1);
    let pct = (newpoints as i64 * 100 / max) as i32;
    let level: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT action FROM warninglevels WHERE percentage <= $1 ORDER BY percentage DESC LIMIT 1",
    )
    .bind(pct)
    .fetch_optional(&ctx.app.db)
    .await?;
    let mut applied = String::new();
    if let Some(a) = level {
        let length = a["length"].as_i64().unwrap_or(0);
        let until = if length > 0 { now() + length } else { 0 };
        match a["type"].as_str().unwrap_or("") {
            "ban" => {
                let gid = a["usergroup"].as_i64().unwrap_or(7) as i32;
                crate::routes::modcp::ban_user(
                    &ctx.app,
                    &user,
                    gid,
                    &format!("Warning level reached ({pct}%)"),
                    length / 86400,
                    me.uid,
                )
                .await?;
                applied = "The user has been banned automatically.".into();
            }
            "suspend" => {
                sqlx::query(
                    "UPDATE users SET suspendposting = TRUE, suspensiontime = $2 WHERE uid = $1",
                )
                .bind(uid)
                .bind(until)
                .execute(&ctx.app.db)
                .await?;
                applied = "The user's posting privileges have been suspended.".into();
            }
            "moderate" => {
                sqlx::query(
                    "UPDATE users SET moderateposts = TRUE, moderationtime = $2 WHERE uid = $1",
                )
                .bind(uid)
                .bind(until)
                .execute(&ctx.app.db)
                .await?;
                applied = "The user's posts will now be moderated.".into();
            }
            _ => {}
        }
    }
    if f.sendpm {
        let msg = if f.pm_message.trim().is_empty() {
            format!("You have received a warning: [b]{title}[/b] ({points} point(s)).")
        } else {
            f.pm_message.trim().to_string()
        };
        crate::routes::private::send_system_pm(
            &ctx.app,
            uid,
            &format!("You have been warned: {title}"),
            &msg,
        )
        .await?;
    }
    crate::notify::alert(
        &ctx.app,
        uid,
        me.uid,
        "warning",
        wid,
        serde_json::json!({"title": title}),
    )
    .await;
    crate::ops::log_moderator_action(&ctx.app, me.uid, &ctx.ip, 0, 0, f.pid, "Warned user", serde_json::json!({"uid": uid, "username": user.username, "points": points, "title": title})).await;
    crate::audit::log(
        &ctx,
        uid,
        "warned",
        serde_json::json!({"points": points, "wid": wid}),
    )
    .await;
    Ok(ctx.redirect(
        &format!("/warnings/{uid}"),
        &format!("The warning has been issued. {applied}"),
    ))
}

pub async fn view(ctx: Ctx, Path(wid): Path<i32>) -> AppResult<Response> {
    enabled(&ctx)?;
    let w: (i32, String, i32, i64, i64, bool, i64, i32, String, String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT w.uid, w.title, w.points, w.dateline, w.expires, w.expired, w.daterevoked, w.pid, w.notes, w.revokereason, i.username, r.username
         FROM warnings w LEFT JOIN users i ON i.uid = w.issuedby LEFT JOIN users r ON r.uid = w.revokedby WHERE w.wid = $1",
    )
    .bind(wid)
    .fetch_optional(&ctx.app.db)
    .await?
    .ok_or_else(|| AppError::not_found("warning"))?;
    let own = w.0 == ctx.uid() && ctx.settings().bool("canviewownwarning");
    if !(ctx.perms.canwarnusers || ctx.perms.canviewwarnlogs || own) {
        return Err(AppError::no_perm());
    }
    let username: String = sqlx::query_scalar("SELECT username FROM users WHERE uid = $1")
        .bind(w.0)
        .fetch_optional(&ctx.app.db)
        .await?
        .unwrap_or_default();
    ctx.render(
        "warning_view.html",
        minijinja::context! { title => "Warning", wid => wid, w => w, username => username, can_revoke => ctx.perms.canwarnusers && w.6 == 0 && !own, show_notes => ctx.perms.canwarnusers || ctx.perms.canviewwarnlogs },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct RevokeForm {
    #[serde(default, deserialize_with = "de::string")]
    pub reason: String,
}

pub async fn revoke(
    ctx: Ctx,
    Path(wid): Path<i32>,
    CsrfForm(f): CsrfForm<RevokeForm>,
) -> AppResult<Response> {
    enabled(&ctx)?;
    if !ctx.perms.canwarnusers {
        return Err(AppError::no_perm());
    }
    let row: Option<(i32, i32, bool)> = sqlx::query_as("UPDATE warnings SET daterevoked = $2, revokedby = $3, revokereason = $4 WHERE wid = $1 AND daterevoked = 0 RETURNING uid, points, expired")
        .bind(wid)
        .bind(now())
        .bind(ctx.uid())
        .bind(f.reason.trim())
        .fetch_optional(&ctx.app.db)
        .await?;
    if let Some((uid, points, expired)) = row {
        crate::audit::log(
            &ctx,
            uid,
            "warning_revoked",
            serde_json::json!({"wid": wid}),
        )
        .await;
        if !expired {
            sqlx::query(
                "UPDATE users SET warningpoints = GREATEST(warningpoints - $2, 0) WHERE uid = $1",
            )
            .bind(uid)
            .bind(points)
            .execute(&ctx.app.db)
            .await?;
        }
        crate::ops::log_moderator_action(
            &ctx.app,
            ctx.uid(),
            &ctx.ip,
            0,
            0,
            0,
            "Revoked warning",
            serde_json::json!({"wid": wid, "uid": uid}),
        )
        .await;
    }
    Ok(ctx.redirect(&format!("/warning/{wid}"), "The warning has been revoked."))
}
