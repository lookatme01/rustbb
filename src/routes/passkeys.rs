//! Passkey endpoints: adding and removing them in the User CP, and signing in with one.
//! The browser side is `static/js/passkeys.js`; the ceremonies are in `crate::passkeys`.

use crate::app::App;
use crate::auth;
use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use axum::Json;
use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Deserialize;

pub fn router() -> Router<App> {
    Router::new()
        .route("/usercp/passkeys/begin", post(add_begin))
        .route("/usercp/passkeys/finish", post(add_finish))
        .route("/usercp/passkeys/remove", post(remove))
        .route("/member/login/passkey/begin", post(sign_in_begin))
        .route("/member/login/passkey/finish", post(sign_in_finish))
}

/// JSON for the page script: the value, or `{"error": message}`.
fn json(r: AppResult<serde_json::Value>) -> Response {
    match r {
        Ok(v) => Json(v).into_response(),
        Err(e) => {
            if matches!(e, AppError::Db(_) | AppError::Other(_)) {
                tracing::error!(error = ?e, "passkey request failed");
            }
            let status = match e.status() {
                StatusCode::INTERNAL_SERVER_ERROR => StatusCode::INTERNAL_SERVER_ERROR,
                s => s,
            };
            (
                status,
                Json(serde_json::json!({"error": e.public_message()})),
            )
                .into_response()
        }
    }
}

/// The message shown on the page the script navigates to next.
fn flash(ctx: &Ctx, msg: &str) {
    ctx.add_cookie(crate::ctx::FLASH_COOKIE, msg, Some(60), false);
}

fn member(ctx: &Ctx) -> AppResult<crate::models::User> {
    let u = ctx.require_login()?.clone();
    if !ctx.perms.canusercp {
        return Err(AppError::no_perm());
    }
    Ok(u)
}

#[derive(Deserialize)]
pub struct PasswordForm {
    #[serde(default, deserialize_with = "de::string")]
    pub password: String,
}

/// Adding a passkey needs the password: a stolen session alone must not be able to plant a
/// lasting way back into the account.
pub async fn add_begin(ctx: Ctx, CsrfForm(f): CsrfForm<PasswordForm>) -> Response {
    json(
        async {
            let me = member(&ctx)?;
            if !ctx
                .app
                .throttle(&format!("passkey-add:{}", me.uid), 10, 600)
                .await
            {
                return Err(AppError::RateLimited);
            }
            if !auth::verify_password(&f.password, &me.password).await {
                return Err(AppError::user("Your password is incorrect."));
            }
            crate::passkeys::begin_registration(&ctx.app, me.uid, &me.username).await
        }
        .await,
    )
}

#[derive(Deserialize)]
pub struct FinishAddForm {
    #[serde(default, deserialize_with = "de::string")]
    pub credential: String,
    #[serde(default, deserialize_with = "de::string")]
    pub name: String,
}

pub async fn add_finish(ctx: Ctx, CsrfForm(f): CsrfForm<FinishAddForm>) -> Response {
    json(
        async {
            let me = member(&ctx)?;
            crate::passkeys::finish_registration(&ctx.app, me.uid, &f.credential, &f.name).await?;
            crate::audit::log(
                &ctx,
                me.uid,
                "passkey_added",
                serde_json::json!({"name": f.name.trim()}),
            )
            .await;
            flash(
                &ctx,
                "Your passkey has been added. You can now sign in with it.",
            );
            Ok(serde_json::json!({"redirect": "/usercp/security"}))
        }
        .await,
    )
}

#[derive(Deserialize)]
pub struct RemoveForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub id: i32,
}

pub async fn remove(ctx: Ctx, CsrfForm(f): CsrfForm<RemoveForm>) -> AppResult<Response> {
    let me = member(&ctx)?;
    let name: Option<String> =
        sqlx::query_scalar("DELETE FROM passkeys WHERE id = $1 AND uid = $2 RETURNING name")
            .bind(f.id)
            .bind(me.uid)
            .fetch_optional(&ctx.app.db)
            .await?;
    if let Some(name) = name {
        crate::audit::log(
            &ctx,
            me.uid,
            "passkey_removed",
            serde_json::json!({"name": name}),
        )
        .await;
    }
    Ok(ctx.redirect(
        "/usercp/security",
        "The passkey has been removed. Also delete it from your device or password manager.",
    ))
}

/// The same gates as signing in with a password, minus the password.
async fn sign_in_allowed(ctx: &Ctx) -> AppResult<()> {
    if !ctx
        .app
        .throttle(&format!("login:{}", ctx.ip), 20, 300)
        .await
    {
        return Err(AppError::user(
            "Too many login attempts. Please wait a few minutes and try again.",
        ));
    }
    if auth::is_filtered(&ctx.app, 1, &ctx.ip).await? {
        return Err(AppError::user(
            "Your IP address has been banned from this board.",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct Empty {}

pub async fn sign_in_begin(ctx: Ctx, CsrfForm(_): CsrfForm<Empty>) -> Response {
    json(
        async {
            if ctx.logged_in() {
                return Err(AppError::user("You are already signed in."));
            }
            sign_in_allowed(&ctx).await?;
            crate::passkeys::begin_sign_in(&ctx.app).await
        }
        .await,
    )
}

#[derive(Deserialize)]
pub struct FinishSignInForm {
    #[serde(default, deserialize_with = "de::string")]
    pub credential: String,
    #[serde(default, deserialize_with = "de::bool")]
    pub remember: bool,
    #[serde(default, deserialize_with = "de::string")]
    pub return_to: String,
}

/// A passkey is both factors at once (the device, unlocked by the member), so it skips the
/// two-factor code step.
pub async fn sign_in_finish(ctx: Ctx, CsrfForm(f): CsrfForm<FinishSignInForm>) -> Response {
    json(
        async {
            if ctx.logged_in() {
                return Err(AppError::user("You are already signed in."));
            }
            let uid = crate::passkeys::finish_sign_in(&ctx.app, &f.credential).await?;
            auth::create_login(&ctx, uid, f.remember).await?;
            crate::audit::log(
                &ctx,
                uid,
                "login",
                serde_json::json!({"remember": f.remember, "passkey": true}),
            )
            .await;
            let username: String = sqlx::query_scalar("SELECT username FROM users WHERE uid = $1")
                .bind(uid)
                .fetch_one(&ctx.app.db)
                .await?;
            flash(
                &ctx,
                &format!("You have successfully been logged in. Welcome back, {username}."),
            );
            let to = if f.return_to.starts_with("/member/") {
                "/"
            } else {
                crate::ctx::safe_redirect(&f.return_to)
            };
            Ok(serde_json::json!({"redirect": to}))
        }
        .await,
    )
}
