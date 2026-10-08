//! Staff actions on a member from the Admin CP and Mod CP member pages: restrict posting and lift
//! it again, pin a staff note to the top of the member file, and the hover card shown over member
//! names in the panels.

use crate::app::App;
use crate::ctx::{CsrfForm, Ctx, de};
use crate::domain::staff::Cap;
use crate::error::{AppError, AppResult};
use crate::member_file::{self, View};
use crate::models::User;
use crate::util::now;
use axum::Router;
use axum::extract::Path;
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;

pub fn router() -> Router<App> {
    Router::new()
        .route("/modcp/member/{uid}/restrict", post(restrict))
        .route("/modcp/member/{uid}/lift", post(lift))
        .route("/modcp/member/{uid}/card", get(card))
        .route("/modcp/notes/{id}/pin", post(pin))
}

/// Where to go after an action: back to the member page it came from, else the Mod CP one.
fn back_to(back: &str, uid: i32) -> String {
    crate::routes::modcp::member_back(back, uid).unwrap_or_else(|| format!("/modcp/member/{uid}"))
}

async fn load(ctx: &Ctx, uid: i32) -> AppResult<User> {
    sqlx::query_as(&format!("SELECT {} FROM users WHERE uid = $1", crate::models::USER_COLUMNS))
        .bind(uid)
        .fetch_optional(&ctx.app.db)
        .await?
        .ok_or_else(|| AppError::not_found("member"))
}

/// Admins, and moderators who may edit profiles, may restrict members they outrank.
fn may_restrict(ctx: &Ctx, target: &User) -> AppResult<()> {
    ctx.require_login()?;
    let staff = ctx.is_admin() || (ctx.can(Cap::ModCp) && ctx.perms.caneditprofiles);
    if !staff {
        return Err(AppError::no_perm());
    }
    if target.is_system {
        return Err(AppError::user("The System account can't be restricted."));
    }
    if !crate::routes::modcp::can_act_on(ctx, target) || target.uid == ctx.uid() {
        return Err(AppError::user("You can't restrict this member."));
    }
    Ok(())
}

/// The columns behind each kind of restriction: (flag, until, label).
fn columns(kind: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match kind {
        "moderate" => Some(("moderateposts", "moderationtime", "Their posts now wait for approval")),
        "posting" => Some(("suspendposting", "suspensiontime", "Their posting is suspended")),
        "signature" => Some(("suspendsignature", "suspendsigtime", "Their signature is suspended")),
        _ => None,
    }
}

#[derive(Deserialize, Default)]
pub struct RestrictForm {
    #[serde(default, deserialize_with = "de::string")]
    pub kind: String,
    #[serde(default, deserialize_with = "de::i64")]
    pub days: i64,
    #[serde(default, deserialize_with = "de::string")]
    pub reason: String,
    #[serde(default, deserialize_with = "de::string")]
    pub back: String,
}

pub async fn restrict(ctx: Ctx, Path(uid): Path<i32>, CsrfForm(f): CsrfForm<RestrictForm>) -> AppResult<Response> {
    let user = load(&ctx, uid).await?;
    may_restrict(&ctx, &user)?;
    let (flag, until_col, done) = columns(&f.kind).ok_or_else(|| AppError::user("Choose a restriction."))?;
    let days = f.days.clamp(0, 3650);
    let until = if days > 0 { now() + days * 86_400 } else { 0 };
    sqlx::query(&format!("UPDATE users SET {flag} = TRUE, {until_col} = $2 WHERE uid = $1"))
        .bind(uid)
        .bind(until)
        .execute(&ctx.app.db)
        .await?;
    let reason: String = f.reason.trim().chars().take(300).collect();
    crate::ops::log_moderator_action(
        &ctx.app,
        ctx.uid(),
        &ctx.ip,
        0,
        0,
        0,
        "Restricted member",
        serde_json::json!({"uid": uid, "username": user.username, "kind": f.kind, "days": days, "reason": reason}),
    )
    .await;
    crate::audit::log(&ctx, uid, "restricted", serde_json::json!({"kind": f.kind, "days": days, "until": until, "reason": reason})).await;
    let back = back_to(&f.back, uid);
    let msg = if days > 0 {
        format!("{done} for {days} day{}.", if days == 1 { "" } else { "s" })
    } else {
        format!("{done} until someone lifts it.")
    };
    Ok(ctx.redirect_undo(
        &back,
        &msg,
        &format!("/modcp/member/{uid}/lift"),
        &[("kind", f.kind.clone()), ("back", back.clone()), ("undo", "1".into())],
    ))
}

#[derive(Deserialize, Default)]
pub struct LiftForm {
    #[serde(default, deserialize_with = "de::string")]
    pub kind: String,
    #[serde(default, deserialize_with = "de::string")]
    pub back: String,
    #[serde(default, deserialize_with = "de::bool")]
    pub undo: bool,
}

pub async fn lift(ctx: Ctx, Path(uid): Path<i32>, CsrfForm(f): CsrfForm<LiftForm>) -> AppResult<Response> {
    let user = load(&ctx, uid).await?;
    may_restrict(&ctx, &user)?;
    let (flag, until_col, _) = columns(&f.kind).ok_or_else(|| AppError::user("Choose a restriction."))?;
    sqlx::query(&format!("UPDATE users SET {flag} = FALSE, {until_col} = 0 WHERE uid = $1"))
        .bind(uid)
        .execute(&ctx.app.db)
        .await?;
    crate::ops::log_moderator_action(
        &ctx.app,
        ctx.uid(),
        &ctx.ip,
        0,
        0,
        0,
        if f.undo { "Undid a restriction" } else { "Lifted restriction" },
        serde_json::json!({"uid": uid, "username": user.username, "kind": f.kind}),
    )
    .await;
    crate::audit::log(&ctx, uid, "restriction_lifted", serde_json::json!({"kind": f.kind, "undo": f.undo})).await;
    Ok(ctx.redirect(&back_to(&f.back, uid), if f.undo { "Undone." } else { "The restriction has been lifted." }))
}

#[derive(Deserialize, Default)]
pub struct PinForm {
    #[serde(default, deserialize_with = "de::string")]
    pub back: String,
}

/// Pin a note to the top of the member file (or unpin it). One note per member is pinned.
pub async fn pin(ctx: Ctx, Path(id): Path<i64>, CsrfForm(f): CsrfForm<PinForm>) -> AppResult<Response> {
    ctx.require_login()?;
    if !ctx.can(Cap::WriteModNotes) {
        return Err(AppError::no_perm());
    }
    let note: (i32, bool, i64) = sqlx::query_as("SELECT uid, pinned, retracted_at FROM moderator_notes WHERE id = $1")
        .bind(id)
        .fetch_optional(&ctx.app.db)
        .await?
        .ok_or_else(|| AppError::not_found("note"))?;
    let (uid, pinned, retracted) = note;
    if retracted > 0 {
        return Err(AppError::user("A retracted note can't be pinned."));
    }
    let mut tx = ctx.app.db.begin().await?;
    sqlx::query("UPDATE moderator_notes SET pinned = FALSE WHERE uid = $1 AND pinned").bind(uid).execute(&mut *tx).await?;
    if !pinned {
        sqlx::query("UPDATE moderator_notes SET pinned = TRUE WHERE id = $1").bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(ctx.redirect(&back_to(&f.back, uid), if pinned { "The note has been unpinned." } else { "The note is pinned to the top of the member file." }))
}

/// The hover card: a small member file, as an HTML fragment for the panels' script.
pub async fn card(ctx: Ctx, Path(uid): Path<i32>) -> AppResult<Response> {
    ctx.require_login()?;
    if !ctx.can(Cap::ModCp) {
        return Err(AppError::no_perm());
    }
    let user = load(&ctx, uid).await?;
    let mf = member_file::load(&ctx, &user, View::of(&ctx)).await?;
    ctx.render("member_card.html", minijinja::context! { mf => mf }).await
}
