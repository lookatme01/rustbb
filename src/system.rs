//! "System": the board's built-in bot account. It is a real `users` row (so it can author reports,
//! PMs and moderator-log entries like anyone else) in its own protected group with administrator
//! permissions. It never signs in, is always shown online, and cannot be deleted, banned, merged,
//! or moved out of its group — enforced by database triggers (migrations/0005_system_user.sql)
//! with friendlier checks in the app. Cosmetic fields (name, avatar, signature, title) remain
//! editable from the Admin CP.

use crate::app::App;
use crate::cache::Cache;
use crate::error::{AppError, AppResult};
use crate::perms::GroupPerms;
use crate::tasks::Ran;
use crate::util::now;
use sqlx::PgPool;

pub const DEFAULT_NAME: &str = "System";
/// Never a valid password hash, so `auth::verify_password` always fails.
const NO_PASSWORD: &str = "!";
/// `.invalid` is reserved (RFC 2606): mail to it can never be delivered.
const EMAIL: &str = "system@invalid";
/// What "Who's Online" and the profile say System is doing.
pub const ONLINE_LOCATION: &str = "Keeping the board running";
const NAMESTYLE: &str = "<span class=\"name-system\">{username}</span>";

/// Create the System group and account if they are missing. Idempotent; runs on every start
/// (via `install::upgrade`) and after a MyBB import replaces the users table.
pub async fn ensure(db: &PgPool) -> anyhow::Result<(i32, i32)> {
    let mut tx = db.begin().await?;
    // Serialize concurrent starts of several app nodes.
    sqlx::query("SELECT pg_advisory_xact_lock(424243)")
        .execute(&mut *tx)
        .await?;
    let gid: i32 = match sqlx::query_scalar("SELECT gid FROM usergroups WHERE is_system")
        .fetch_optional(&mut *tx)
        .await?
    {
        Some(g) => g,
        None => {
            let disporder: i32 =
                sqlx::query_scalar("SELECT COALESCE(MAX(disporder), 0) + 1 FROM usergroups")
                    .fetch_one(&mut *tx)
                    .await?;
            sqlx::query_scalar(
                "INSERT INTO usergroups (type, title, description, namestyle, usertitle, stars, starimage, disporder, isbannedgroup, perms, is_system)
                 VALUES (1, 'System', 'The built-in System account that runs automated board tasks.', $1, 'System', 0, '', $2, FALSE, $3, TRUE)
                 RETURNING gid",
            )
            .bind(NAMESTYLE)
            .bind(disporder)
            .bind(serde_json::to_value(GroupPerms::administrator())?)
            .fetch_one(&mut *tx)
            .await?
        }
    };
    let uid: i32 = match sqlx::query_scalar("SELECT uid FROM users WHERE is_system")
        .fetch_optional(&mut *tx)
        .await?
    {
        Some(u) => u,
        None => {
            let name = free_name(&mut tx).await?;
            let t = now();
            let uid: i32 = sqlx::query_scalar(
                "INSERT INTO users (username, password, email, usergroup, regdate, lastactive, lastvisit, hideemail, receivepms, allownotices, pmnotice, pmnotify, is_system)
                 VALUES ($1, $2, $3, $4, $5, $5, $5, TRUE, FALSE, FALSE, FALSE, FALSE, TRUE)
                 RETURNING uid",
            )
            .bind(&name)
            .bind(NO_PASSWORD)
            .bind(EMAIL)
            .bind(gid)
            .bind(t)
            .fetch_one(&mut *tx)
            .await?;
            // Counted as a member, but never announced as the newest one.
            sqlx::query("UPDATE counters SET numusers = numusers + 1 WHERE id = 1")
                .execute(&mut *tx)
                .await?;
            tracing::info!("created the System account “{name}” (uid {uid})");
            uid
        }
    };
    // Messages the board sent before System existed (or imported from MyBB) came from uid 0.
    sqlx::query("UPDATE privatemessages SET fromid = $1 WHERE fromid = 0")
        .bind(uid)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((uid, gid))
}

/// System's uid and current name (admins can rename it).
pub async fn identity(app: &App) -> AppResult<(i32, String)> {
    let uid = app.cache().system_uid;
    let name: Option<String> =
        sqlx::query_scalar("SELECT username FROM users WHERE uid = $1 AND is_system")
            .bind(uid)
            .fetch_optional(&app.db)
            .await?;
    name.map(|n| (uid, n))
        .ok_or_else(|| AppError::user("The System account is not available."))
}

/// What a staff member published as System. Written inside the content's own transaction, so
/// there is never System content without a record of who wrote it.
pub struct Authorship<'a> {
    pub kind: &'a str,
    pub ref_id: i32,
    pub actor: i32,
    pub actor_name: &'a str,
    pub ip: &'a str,
    pub summary: &'a str,
}

pub async fn record(conn: &mut sqlx::PgConnection, a: Authorship<'_>) -> AppResult<()> {
    sqlx::query("INSERT INTO system_authorship (kind, ref_id, actor, actor_name, ipaddress, dateline, summary) VALUES ($1, $2, $3, $4, $5, $6, $7)")
        .bind(a.kind)
        .bind(a.ref_id)
        .bind(a.actor)
        .bind(a.actor_name)
        .bind(crate::util::IpText::from(a.ip))
        .bind(now())
        .bind(a.summary.chars().take(200).collect::<String>())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// The welcome message for a member, from the board settings. `{username}` and `{boardname}`
/// are inserted as literal text, so a crafted username can't add MyCode to the message.
pub fn welcome_text(
    subject: &str,
    message: &str,
    username: &str,
    boardname: &str,
) -> (String, String) {
    let subject = subject
        .replace("{username}", username)
        .replace("{boardname}", boardname);
    let message = message
        .replace("{username}", &crate::parser::literal(username))
        .replace("{boardname}", &crate::parser::literal(boardname));
    (subject.chars().take(120).collect(), message)
}

/// Send System's welcome message to members whose accounts just became active (when enabled).
/// With delivery keys, members who already got it from an earlier attempt are skipped.
pub async fn welcome(
    app: &App,
    d: Option<crate::infra::outbox::Delivery<'_>>,
    members: &[(i32, String)],
) -> anyhow::Result<()> {
    let s = app.cache().settings.clone();
    if !s.bool("system_welcome_pm") || !s.bool("enablepms") {
        return Ok(());
    }
    let (subject, message) = (
        s.get("system_welcome_subject"),
        s.get("system_welcome_message"),
    );
    if subject.trim().is_empty() || message.trim().is_empty() {
        return Ok(());
    }
    for (uid, name) in members {
        let (sub, msg) = welcome_text(subject, message, name, s.get("bbname"));
        let key = d.map(|d| d.key("welcome_pm", *uid));
        crate::routes::private::deliver_system_pm(app, key.as_deref(), *uid, &sub, &msg)
            .await
            .map_err(|e| anyhow::anyhow!("welcome message to uid {uid} failed: {e}"))?;
    }
    Ok(())
}

/// "System", or "System 2", "System 3"… if a member already has the name.
async fn free_name(tx: &mut sqlx::PgConnection) -> anyhow::Result<String> {
    for n in 1.. {
        let name = if n == 1 {
            DEFAULT_NAME.to_string()
        } else {
            format!("{DEFAULT_NAME} {n}")
        };
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM users WHERE lower(username) = lower($1))",
        )
        .bind(&name)
        .fetch_one(&mut *tx)
        .await?;
        if !taken {
            return Ok(name);
        }
    }
    unreachable!()
}

/// Refuse `action` (e.g. "banned") on the System account with a user-facing error.
pub fn guard(cache: &Cache, uid: i32, action: &str) -> AppResult<()> {
    if cache.is_system(uid) {
        return Err(AppError::user(format!(
            "The System account cannot be {action}."
        )));
    }
    Ok(())
}

/// Refuse to place a regular member in the System group.
pub fn guard_group(cache: &Cache, uid: i32, gids: &[i32]) -> AppResult<()> {
    if cache.system_gid > 0 && !cache.is_system(uid) && gids.contains(&cache.system_gid) {
        return Err(AppError::user(
            "The System group is reserved for the System account.",
        ));
    }
    Ok(())
}

/// Forum IDs from the "close inactive threads in" setting. `None` means every forum; an empty
/// list means the setting is filled in but holds no valid ID, which must not widen to every forum.
fn autoclose_forums(setting: &str) -> Option<Vec<i32>> {
    if setting.trim().is_empty() {
        return None;
    }
    Some(
        setting
            .split(',')
            .filter_map(|x| x.trim().parse::<i32>().ok())
            .filter(|f| *f > 0)
            .collect(),
    )
}

/// Scheduled task: close threads with no new posts for `system_autoclose_days`, as System.
pub async fn autoclose(app: &App) -> anyhow::Result<Ran> {
    let cache = app.cache();
    let days = cache.settings.int("system_autoclose_days");
    if days <= 0 || cache.system_uid == 0 {
        return Ok(Ran::new(false, "turned off"));
    }
    let fids = match autoclose_forums(cache.settings.get("system_autoclose_forums")) {
        Some(f) if f.is_empty() => {
            // A misconfiguration: logged so the admin sees it.
            return Ok(Ran::new(
                true,
                "no valid forum IDs configured; nothing closed",
            ));
        }
        Some(f) => f,
        None => vec![],
    };
    let mut tx = app.db.begin().await?;
    let closed: Vec<(i32, i32, String)> = sqlx::query_as(
        "UPDATE threads SET closed = '1' WHERE tid IN (
            SELECT tid FROM threads
            WHERE visible = 1 AND NOT sticky AND closed = '' AND lastpost < $1
              AND (cardinality($2::int[]) = 0 OR fid = ANY($2))
            ORDER BY lastpost LIMIT 500 FOR UPDATE SKIP LOCKED)
         RETURNING tid, fid, subject",
    )
    .bind(now() - days * 86400)
    .bind(&fids)
    .fetch_all(&mut *tx)
    .await?;
    for (tid, fid, subject) in &closed {
        sqlx::query("INSERT INTO moderatorlog (uid, dateline, fid, tid, pid, action, data) VALUES ($1, $2, $3, $4, 0, $5, $6)")
            .bind(cache.system_uid)
            .bind(now())
            .bind(fid)
            .bind(tid)
            .bind(format!("Thread closed (no posts for {days} days)"))
            .bind(serde_json::json!({"automatic": true, "days": days, "subject": subject}))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    if !closed.is_empty() {
        app.invalidate(&["pagecache"]).await?;
    }
    Ok(Ran::new(
        !closed.is_empty(),
        format!("closed {} inactive threads", closed.len()),
    ))
}

/// Record, as System, that something a moderator set up ran out on its own (ban, suspension,
/// warning). `members` are (uid, username) pairs.
pub async fn log_expiries(app: &App, action: &str, members: &[(i32, String)]) {
    let cache = app.cache();
    if members.is_empty() || cache.system_uid == 0 || !cache.settings.bool("system_log_expiries") {
        return;
    }
    for (uid, name) in members {
        crate::ops::log_moderator_action(
            app,
            cache.system_uid,
            "",
            0,
            0,
            0,
            action,
            serde_json::json!({"uid": uid, "username": name, "subject": name, "automatic": true}),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_placeholders_are_literal() {
        let (sub, msg) = welcome_text(
            "Welcome to {boardname}, {username}!",
            "Hi {username}, enjoy [b]{boardname}[/b].",
            "[url=https://evil.example]x[/url]",
            "My Board",
        );
        assert_eq!(
            sub,
            "Welcome to My Board, [url=https://evil.example]x[/url]!"
        );
        assert!(
            msg.starts_with("Hi [noparse][[/noparse][noparse]url=https://evil.example]x[/noparse]")
        );
        assert!(msg.ends_with("enjoy [b][noparse]My Board[/noparse][/b]."));
    }

    #[test]
    fn autoclose_forum_list() {
        assert_eq!(autoclose_forums(""), None);
        assert_eq!(autoclose_forums("  "), None);
        assert_eq!(autoclose_forums("2, 5,x"), Some(vec![2, 5]));
        // A filled-in setting with nothing usable must not mean "every forum".
        assert_eq!(autoclose_forums("general"), Some(vec![]));
        assert_eq!(autoclose_forums("-1,0"), Some(vec![]));
    }
}
