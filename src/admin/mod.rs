//! Admin Control Panel.
//!
//! Access requires the `cancp` group permission plus a recent password re-confirmation
//! ("ACP verification", valid for one hour per login) and, optionally, TOTP 2FA.

pub mod automod;
pub mod crud;
pub mod forums;
pub mod groups;
pub mod massmail;
pub mod promotions;
pub mod settings;
pub mod themes;
pub mod tools;
pub mod users;

use crate::app::App;
use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::util::now;
use axum::Router;
use axum::extract::Query;
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;

pub fn router() -> Router<App> {
    Router::new()
        .route("/", get(dashboard))
        .route("/verify", get(verify_form).post(verify_submit))
        .route("/notes", post(save_notes))
        .merge(automod::router())
        .merge(settings::router())
        .merge(forums::router())
        .merge(users::router())
        .merge(groups::router())
        .merge(themes::router())
        .merge(tools::router())
        .merge(massmail::router())
        .merge(promotions::router())
        .merge(crud::router())
}

/// Modules an administrator can be restricted from (Admin Permissions).
pub static MODULES: &[(&str, &str)] = &[
    ("settings", "Board settings"),
    ("forums", "Forums & permissions"),
    ("users", "Users"),
    ("groups", "User groups & titles"),
    ("bans", "Banning"),
    ("themes", "Themes & templates"),
    (
        "content",
        "Posting configuration (smilies, icons, MyCode, filters…)",
    ),
    ("tools", "Tools & maintenance"),
    ("logs", "Logs"),
    ("massmail", "Mass mail"),
    ("promotions", "Promotions"),
    ("adminperms", "Admin permissions"),
];

/// Guard for every ACP page: admin, verified recently, allowed to use `module`.
pub async fn require_admin(ctx: &Ctx, module: &str) -> AppResult<()> {
    let me = ctx.require_login()?;
    if !ctx.perms.cancp {
        return Err(AppError::no_perm());
    }
    if ctx.settings().bool("acp2fa") && me.totp_secret.is_empty() {
        return Err(AppError::User("Two-factor authentication is required for the Admin CP. Enable it in User CP → Security first.".into()));
    }
    if ctx.acp_verified < now() - 3600 {
        return Err(AppError::User("__acpverify__".into()));
    }
    if !module.is_empty() {
        let perms: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT permissions FROM adminoptions WHERE uid = $1")
                .bind(me.uid)
                .fetch_optional(&ctx.app.db)
                .await?;
        if let Some(p) = perms
            && let Some(obj) = p.as_object()
            && !obj.is_empty()
            && obj.get(module).and_then(|v| v.as_bool()) == Some(false)
        {
            return Err(AppError::NoPermission(
                "You do not have permission to access this part of the Admin CP.".into(),
            ));
        }
    }
    Ok(())
}

/// Wrap a guard so that an unverified admin is sent to the password prompt.
#[allow(clippy::result_large_err)] // an axum response, returned as-is by handlers
pub async fn guard(ctx: &Ctx, module: &str) -> Result<(), Response> {
    match require_admin(ctx, module).await {
        Ok(()) => Ok(()),
        Err(AppError::User(m)) if m == "__acpverify__" => {
            let back = if ctx.query.is_empty() {
                ctx.path.clone()
            } else {
                format!("{}?{}", ctx.path, ctx.query)
            };
            Err(ctx.redirect(
                &format!(
                    "/admin/verify?return_to={}",
                    percent_encoding::utf8_percent_encode(
                        &back,
                        percent_encoding::NON_ALPHANUMERIC
                    )
                ),
                "",
            ))
        }
        Err(e) => Err(axum::response::IntoResponse::into_response(e)),
    }
}

/// Record an admin action.
pub async fn log(ctx: &Ctx, module: &str, action: &str, data: serde_json::Value) {
    let _ = sqlx::query("INSERT INTO adminlog (uid, ipaddress, dateline, module, action, data) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(ctx.uid())
        .bind(&ctx.ip)
        .bind(now())
        .bind(module)
        .bind(action)
        .bind(data)
        .execute(&ctx.app.db)
        .await;
}

/// Render an ACP page (adds the admin navigation context).
pub async fn page(
    ctx: &Ctx,
    template: &str,
    section: &str,
    title: &str,
    extra: minijinja::Value,
) -> AppResult<Response> {
    let base = minijinja::context! { title => title, acp_section => section, breadcrumb => vec![("Admin CP".to_string(), "/admin".to_string())], crud_tables => crud::nav() };
    ctx.render(template, minijinja::value::merge_maps([base, extra]))
        .await
}

macro_rules! acp_guard {
    ($ctx:expr, $module:expr) => {
        if let Err(r) = crate::admin::guard(&$ctx, $module).await {
            return Ok(r);
        }
    };
}
pub(crate) use acp_guard;

#[derive(Deserialize, Default)]
pub struct ReturnQ {
    #[serde(default)]
    pub return_to: String,
}

pub async fn verify_form(ctx: Ctx, Query(q): Query<ReturnQ>) -> AppResult<Response> {
    let me = ctx.require_login()?;
    if !ctx.perms.cancp {
        return Err(AppError::no_perm());
    }
    ctx.render("admin/verify.html", minijinja::context! { title => "Admin CP Login", return_to => q.return_to, needs_2fa => !me.totp_secret.is_empty(), error => "" }).await
}

#[derive(Deserialize, Default)]
pub struct VerifyForm {
    #[serde(default, deserialize_with = "de::string")]
    pub password: String,
    #[serde(default, deserialize_with = "de::string")]
    pub code: String,
    #[serde(default, deserialize_with = "de::string")]
    pub return_to: String,
}

pub async fn verify_submit(ctx: Ctx, CsrfForm(f): CsrfForm<VerifyForm>) -> AppResult<Response> {
    let me = ctx.require_login()?.clone();
    if !ctx.perms.cancp {
        return Err(AppError::no_perm());
    }
    if !ctx
        .app
        .throttle(&format!("acpverify:{}", me.uid), 10, 600)
        .await
    {
        return Err(AppError::RateLimited);
    }
    let pw_ok = crate::auth::verify_password(&f.password, &me.password).await;
    let code_ok = me.totp_secret.is_empty()
        || (pw_ok
            && crate::routes::member::totp_consume(&ctx.app, me.uid, &me.totp_secret, &f.code)
                .await?);
    if !pw_ok || !code_ok {
        log(&ctx, "home", "Failed Admin CP login", serde_json::json!({})).await;
        return ctx
            .render("admin/verify.html", minijinja::context! { title => "Admin CP Login", return_to => f.return_to, needs_2fa => !me.totp_secret.is_empty(), error => "The details you entered are incorrect." })
            .await;
    }
    if let Some(h) = &ctx.token_hash {
        sqlx::query("UPDATE logins SET acp_verified = $2 WHERE token_hash = $1")
            .bind(h)
            .bind(now())
            .execute(&ctx.app.db)
            .await?;
        crate::auth::rotate_login(&ctx).await?;
    }
    let to = if f.return_to.starts_with("/admin") {
        f.return_to.clone()
    } else {
        "/admin".into()
    };
    Ok(ctx.redirect(&to, ""))
}

pub async fn dashboard(ctx: Ctx) -> AppResult<Response> {
    acp_guard!(ctx, "");
    let db = &ctx.app.db;
    let base = crate::routes::index::board_stats(&ctx).await?;
    let t = now();
    let (posts_today, threads_today, users_today, awaiting, online): (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM posts WHERE dateline > $1), (SELECT COUNT(*) FROM threads WHERE dateline > $1),
                (SELECT COUNT(*) FROM users WHERE regdate > $1), (SELECT COUNT(*) FROM users WHERE usergroup = 5),
                (SELECT COUNT(*) FROM sessions WHERE time > $2)",
    )
    .bind(t - 86400)
    .bind(t - 900)
    .fetch_one(db)
    .await?;
    let (uthreads, uposts, reports, mailq): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COALESCE(SUM(unapprovedthreads), 0)::bigint FROM forums), (SELECT COALESCE(SUM(unapprovedposts), 0)::bigint FROM forums),
                (SELECT COUNT(*) FROM reportedcontent WHERE reportstatus = 0), (SELECT COUNT(*) FROM mailqueue)",
    )
    .fetch_one(db)
    .await?;
    let dbsize: i64 = sqlx::query_scalar("SELECT pg_database_size(current_database())")
        .fetch_one(db)
        .await?;
    let pgversion: String = sqlx::query_scalar("SHOW server_version")
        .fetch_one(db)
        .await?;
    let attach_size: i64 =
        sqlx::query_scalar("SELECT COALESCE(SUM(filesize), 0)::bigint FROM attachments")
            .fetch_one(db)
            .await?;
    let notes: String = sqlx::query_scalar("SELECT notes FROM adminoptions WHERE uid = $1")
        .bind(ctx.uid())
        .fetch_optional(db)
        .await?
        .unwrap_or_default();
    let logs: Vec<(i64, String, String, String, i64, Option<String>)> = sqlx::query_as(
        "SELECT l.id, l.module, l.action, l.ipaddress, l.dateline, u.username FROM adminlog l LEFT JOIN users u ON u.uid = l.uid ORDER BY l.id DESC LIMIT 10",
    )
    .fetch_all(db)
    .await?;
    let history: Vec<(i64, i32, i32, i32)> = sqlx::query_as("SELECT dateline, numusers, numthreads, numposts FROM stats ORDER BY dateline DESC LIMIT 14").fetch_all(db).await?;
    page(
        &ctx,
        "admin/dashboard.html",
        "home",
        "Dashboard",
        minijinja::context! {
            base => base, posts_today => posts_today, threads_today => threads_today, users_today => users_today, awaiting => awaiting, online => online,
            uthreads => uthreads, uposts => uposts, reports => reports, mailq => mailq, dbsize => dbsize, pgversion => pgversion, attach_size => attach_size,
            notes => notes, logs => logs, version => env!("CARGO_PKG_VERSION"), uptime => crate::routes::member::format_duration(t - ctx.app.started),
            history => history.into_iter().rev().collect::<Vec<_>>(), node => &ctx.app.node_id,
        },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct NotesForm {
    #[serde(default, deserialize_with = "de::string")]
    pub notes: String,
}

pub async fn save_notes(ctx: Ctx, CsrfForm(f): CsrfForm<NotesForm>) -> AppResult<Response> {
    acp_guard!(ctx, "");
    sqlx::query("INSERT INTO adminoptions (uid, notes) VALUES ($1, $2) ON CONFLICT (uid) DO UPDATE SET notes = $2").bind(ctx.uid()).bind(&f.notes).execute(&ctx.app.db).await?;
    Ok(ctx.redirect("/admin", "Your notes have been saved."))
}
