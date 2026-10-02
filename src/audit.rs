//! Account audit log: a per-user record of security-relevant events that the account owner
//! can review in the User CP (sign-ins, failed attempts, credential changes, staff actions).

use crate::app::App;
use crate::ctx::Ctx;
use crate::util::now;
use serde_json::Value;

/// (action, description, category). Categories: `security`, `account`, `staff`.
pub const ACTIONS: &[(&str, &str, &str)] = &[
    ("login", "Signed in", "security"),
    (
        "login_failed",
        "Failed sign-in attempt (wrong password)",
        "security",
    ),
    (
        "login_locked",
        "Sign-in locked after too many failed attempts",
        "security",
    ),
    ("login_2fa_failed", "Failed two-factor code", "security"),
    ("logout", "Signed out", "security"),
    ("session_revoked", "Signed out a device", "security"),
    ("api_token", "Created an API token", "security"),
    ("password_changed", "Changed password", "security"),
    (
        "password_reset_requested",
        "Requested a password reset email",
        "security",
    ),
    (
        "password_reset",
        "Reset password using an emailed link",
        "security",
    ),
    (
        "twofa_enabled",
        "Turned on two-factor authentication",
        "security",
    ),
    (
        "twofa_disabled",
        "Turned off two-factor authentication",
        "security",
    ),
    (
        "pgp_key_added",
        "Set up an encryption key for private messages",
        "security",
    ),
    (
        "pgp_key_replaced",
        "Replaced the encryption key for private messages",
        "security",
    ),
    (
        "pgp_key_revoked",
        "Revoked the encryption key for private messages",
        "security",
    ),
    (
        "pgp_backup_saved",
        "Saved an encrypted backup of the private message key",
        "security",
    ),
    (
        "pgp_backup_removed",
        "Deleted the backup of the private message key",
        "security",
    ),
    ("email_changed", "Changed email address", "account"),
    (
        "email_change_requested",
        "Asked to change email address",
        "account",
    ),
    ("username_changed", "Changed username", "account"),
    ("registered", "Created the account", "account"),
    ("activated", "Activated the account", "account"),
    ("profile_updated", "Updated profile", "account"),
    ("avatar_changed", "Changed avatar", "account"),
    ("signature_changed", "Changed signature", "account"),
    ("options_changed", "Changed preferences", "account"),
    (
        "data_exported",
        "Downloaded a copy of account data",
        "account",
    ),
    ("group_joined", "Joined a usergroup", "account"),
    ("group_left", "Left a usergroup", "account"),
    ("banned", "Banned by staff", "staff"),
    ("unbanned", "Ban lifted by staff", "staff"),
    ("warned", "Received a warning", "staff"),
    ("warning_revoked", "Warning revoked by staff", "staff"),
    ("staff_edit", "Account edited by staff", "staff"),
    ("appeal_submitted", "Appealed a ban", "account"),
    ("appeal_accepted", "Ban appeal accepted by staff", "staff"),
    ("appeal_rejected", "Ban appeal rejected by staff", "staff"),
];

pub fn describe(action: &str) -> (&'static str, &'static str) {
    ACTIONS
        .iter()
        .find(|a| a.0 == action)
        .map(|a| (a.1, a.2))
        .unwrap_or(("Account event", "account"))
}

/// Who caused an audited event: the request's address and browser, and the signed-in member.
#[derive(Clone, Debug, Default)]
pub struct Actor {
    pub uid: i32,
    pub ip: String,
    pub useragent: String,
}

impl Actor {
    pub fn from_ctx(ctx: &crate::ctx::CtxInner) -> Actor {
        Actor {
            uid: ctx.uid(),
            ip: ctx.ip.clone(),
            useragent: ctx.useragent.clone(),
        }
    }

    /// Background work with no request behind it.
    pub fn system() -> Actor {
        Actor::default()
    }
}

/// Record an event for `uid` on `conn` (normally the transaction making the audited change, so
/// the record and the change commit together). Errors are returned: inside a transaction a failed
/// insert aborts it anyway.
pub async fn record(
    conn: &mut sqlx::PgConnection,
    actor: &Actor,
    uid: i32,
    action: &str,
    details: Value,
) -> sqlx::Result<()> {
    if uid <= 0 {
        return Ok(());
    }
    let details = if details.is_null() {
        serde_json::json!({})
    } else {
        details
    };
    let by: Option<i32> = (actor.uid > 0 && actor.uid != uid).then_some(actor.uid);
    sqlx::query(
        "INSERT INTO user_audit (uid, dateline, action, ipaddress, useragent, actor_uid, details) VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(uid)
    .bind(now())
    .bind(action)
    .bind(crate::util::IpText::from(&actor.ip))
    .bind(actor.useragent.chars().take(200).collect::<String>())
    .bind(by)
    .bind(details)
    .execute(conn)
    .await?;
    Ok(())
}

/// Record an event for `uid` on its own, outside any transaction (for events that are not part
/// of a change, such as failed sign-ins). Failures are logged, never surfaced.
pub async fn log(ctx: &Ctx, uid: i32, action: &str, details: Value) {
    log_actor(&ctx.app, &Actor::from_ctx(ctx), uid, action, details).await
}

async fn log_actor(app: &App, actor: &Actor, uid: i32, action: &str, details: Value) {
    let r = match app.db.acquire().await {
        Ok(mut c) => record(&mut c, actor, uid, action, details).await,
        Err(e) => Err(e),
    };
    if let Err(e) = r {
        tracing::warn!(error = %e, uid, action, "audit log write failed");
    }
}

/// A short, human browser/OS label from a user agent string.
pub fn device_label(ua: &str) -> String {
    let browser = if ua.contains("Edg/") {
        "Edge"
    } else if ua.contains("Firefox/") {
        "Firefox"
    } else if ua.contains("Chrome/") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else if ua.starts_with("curl/") {
        "curl"
    } else if ua.is_empty() {
        "Unknown client"
    } else {
        "Other client"
    };
    let os = if ua.contains("iPhone") || ua.contains("iPad") {
        " on iOS"
    } else if ua.contains("Android") {
        " on Android"
    } else if ua.contains("Mac OS X") {
        " on macOS"
    } else if ua.contains("Windows") {
        " on Windows"
    } else if ua.contains("Linux") {
        " on Linux"
    } else {
        ""
    };
    format!("{browser}{os}")
}
