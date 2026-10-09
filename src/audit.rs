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
    ("passkey_added", "Added a passkey", "security"),
    ("passkey_removed", "Removed a passkey", "security"),
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
    ("restricted", "Restricted by staff", "staff"),
    ("restriction_lifted", "Restriction lifted by staff", "staff"),
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

/// The browser/client name in a user agent, if we recognise it. Order matters: Edge, Opera and
/// Chrome-on-iOS all also claim "Chrome" or "Safari".
fn browser_name(ua: &str) -> Option<&'static str> {
    Some(
        if ua.contains("Edg/") || ua.contains("EdgA/") || ua.contains("EdgiOS/") {
            "Edge"
        } else if ua.contains("OPR/") || ua.contains("Opera") {
            "Opera"
        } else if ua.contains("Firefox/") || ua.contains("FxiOS/") {
            "Firefox"
        } else if ua.contains("Chrome/") || ua.contains("CriOS/") {
            "Chrome"
        } else if ua.contains("Safari/") {
            "Safari"
        } else if ua.starts_with("curl/") {
            "curl"
        } else {
            return None;
        },
    )
}

fn os_name(ua: &str) -> Option<&'static str> {
    // iOS agents say "like Mac OS X", Android says "Linux", ChromeOS says "X11; CrOS"
    Some(if ua.contains("iPhone") {
        "iPhone"
    } else if ua.contains("iPad") {
        "iPad"
    } else if ua.contains("Android") {
        "Android"
    } else if ua.contains("CrOS") {
        "ChromeOS"
    } else if ua.contains("Mac OS X") {
        "macOS"
    } else if ua.contains("Windows") {
        "Windows"
    } else if ua.contains("Linux") || ua.contains("X11") {
        "Linux"
    } else {
        return None;
    })
}

/// A short, human browser/OS label from a user agent string.
pub fn device_label(ua: &str) -> String {
    let browser = match browser_name(ua) {
        Some(b) => b,
        None if ua.is_empty() => "Unknown client",
        None => "Other client",
    };
    match os_name(ua) {
        Some(os) if browser != "curl" => format!("{browser} on {os}"),
        _ => browser.to_string(),
    }
}

/// Like `device_label`, but an agent we can't place is shown as its raw string (cut to `max`
/// characters) rather than a vague "Other client".
pub fn ua_summary(ua: &str, max: usize) -> String {
    let ua = ua.trim();
    if browser_name(ua).is_some() {
        return device_label(ua);
    }
    if ua.chars().count() > max {
        format!("{}…", ua.chars().take(max).collect::<String>())
    } else if ua.is_empty() {
        "Unknown device".to_string()
    } else {
        ua.to_string()
    }
}

#[cfg(test)]
mod ua_tests {
    use super::*;

    const CHROME_MAC: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

    #[test]
    fn common_agents() {
        let cases = [
            (CHROME_MAC, "Chrome on macOS"),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.0.0",
                "Edge on Windows",
            ),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:121.0) Gecko/20100101 Firefox/121.0",
                "Firefox on Windows",
            ),
            (
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Safari/605.1.15",
                "Safari on macOS",
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Mobile/15E148 Safari/604.1",
                "Safari on iPhone",
            ),
            (
                "Mozilla/5.0 (iPad; CPU OS 17_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/120.0.0.0 Mobile/15E148 Safari/604.1",
                "Chrome on iPad",
            ),
            (
                "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36",
                "Chrome on Android",
            ),
            (
                "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0",
                "Firefox on Linux",
            ),
            (
                "Mozilla/5.0 (X11; CrOS x86_64 14541.0.0) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
                "Chrome on ChromeOS",
            ),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 OPR/106.0.0.0",
                "Opera on Windows",
            ),
            ("curl/8.4.0", "curl"),
        ];
        for (ua, want) in cases {
            assert_eq!(ua_summary(ua, 40), want, "{ua}");
            assert_eq!(device_label(ua), want, "{ua}");
        }
    }

    #[test]
    fn unknown_agents() {
        assert_eq!(ua_summary("", 40), "Unknown device");
        assert_eq!(ua_summary("MyBot", 40), "MyBot");
        assert_eq!(
            ua_summary(&"x".repeat(50), 10),
            format!("{}…", "x".repeat(10))
        );
        assert_eq!(device_label("MyBot"), "Other client");
    }
}
