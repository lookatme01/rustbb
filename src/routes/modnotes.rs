//! Moderator notes about members and the member moderation history (Mod CP).
//!
//! Notes are append-only: the author (shortly after writing) or an administrator can retract one,
//! and a retracted note stays visible to staff, so nobody can quietly rewrite what was known.
//! The history page puts everything staff have done about a member on one timeline.

use crate::app::App;
use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::util::now;
use axum::Router;
use axum::extract::{Path, Query};
use axum::response::Response;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};

/// How long the author of a note may retract it.
pub const RETRACT_WINDOW_SECS: i64 = 15 * 60;
const MAX_NOTE_CHARS: usize = 2000;
/// Rows read from each source for the timeline.
const SOURCE_LIMIT: i64 = 200;

pub fn router() -> Router<App> {
    Router::new()
        .route("/modcp/member/{uid}", get(history))
        .route("/modcp/member/{uid}/notes", post(add_note))
        .route("/modcp/notes/{id}/retract", post(retract))
}

/// Whether `viewer` may retract a note written by `author` at `created`.
pub fn can_retract(author: i32, created: i64, viewer: i32, viewer_is_admin: bool, now: i64) -> bool {
    viewer_is_admin || (author > 0 && author == viewer && now - created <= RETRACT_WINDOW_SECS)
}

/// Staff who may read and write notes: anyone with Mod CP access.
fn require_staff(ctx: &Ctx) -> AppResult<()> {
    ctx.require_login()?;
    if ctx.perms.canmodcp || ctx.is_any_mod() {
        Ok(())
    } else {
        Err(AppError::no_perm())
    }
}

/// Live (not retracted) notes about a member, for the staff link on profiles.
pub async fn note_count(app: &App, uid: i32) -> AppResult<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM moderator_notes WHERE uid = $1 AND retracted_at = 0")
        .bind(uid)
        .fetch_one(&app.db)
        .await?)
}

/// The most recent live note about each of `uids` (for report and queue rows).
pub async fn latest_notes(app: &App, uids: &[i32]) -> AppResult<std::collections::HashMap<i32, String>> {
    let rows: Vec<(i32, String)> = sqlx::query_as(
        "SELECT DISTINCT ON (uid) uid, note FROM moderator_notes WHERE uid = ANY($1) AND retracted_at = 0 ORDER BY uid, id DESC",
    )
    .bind(uids)
    .fetch_all(&app.db)
    .await?;
    Ok(rows.into_iter().collect())
}

#[derive(Deserialize, Default)]
pub struct NoteForm {
    #[serde(default, deserialize_with = "de::string")]
    pub note: String,
}

pub async fn add_note(ctx: Ctx, Path(uid): Path<i32>, CsrfForm(f): CsrfForm<NoteForm>) -> AppResult<Response> {
    require_staff(&ctx)?;
    let note = f.note.trim();
    if note.is_empty() {
        return Err(AppError::user("Please write a note."));
    }
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err(AppError::user(format!("Notes can be at most {MAX_NOTE_CHARS} characters.")));
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE uid = $1)")
        .bind(uid)
        .fetch_one(&ctx.app.db)
        .await?;
    if !exists {
        return Err(AppError::not_found("member"));
    }
    sqlx::query("INSERT INTO moderator_notes (uid, author, note, created) VALUES ($1, $2, $3, $4)")
        .bind(uid)
        .bind(ctx.uid())
        .bind(note)
        .bind(now())
        .execute(&ctx.app.db)
        .await?;
    Ok(ctx.redirect(&format!("/modcp/member/{uid}"), "Your note has been added."))
}

pub async fn retract(ctx: Ctx, Path(id): Path<i64>, CsrfForm(_f): CsrfForm<NoteForm>) -> AppResult<Response> {
    require_staff(&ctx)?;
    let (uid, author, created, retracted): (i32, i32, i64, i64) =
        sqlx::query_as("SELECT uid, author, created, retracted_at FROM moderator_notes WHERE id = $1")
            .bind(id)
            .fetch_optional(&ctx.app.db)
            .await?
            .ok_or_else(|| AppError::not_found("note"))?;
    if retracted == 0 {
        if !can_retract(author, created, ctx.uid(), ctx.is_admin(), now()) {
            return Err(AppError::NoPermission(
                "Only the author (within 15 minutes) or an administrator can retract a note.".into(),
            ));
        }
        sqlx::query("UPDATE moderator_notes SET retracted_by = $2, retracted_at = $3 WHERE id = $1 AND retracted_at = 0")
            .bind(id)
            .bind(ctx.uid())
            .bind(now())
            .execute(&ctx.app.db)
            .await?;
    }
    Ok(ctx.redirect(&format!("/modcp/member/{uid}"), "The note has been retracted."))
}

/// One entry on the member's timeline.
#[derive(Serialize)]
struct Event {
    kind: &'static str,
    when: i64,
    title: String,
    detail: String,
    actor: Option<String>,
    link: Option<String>,
    /// Notes only.
    note_id: i64,
    retracted_by: Option<String>,
    can_retract: bool,
}

impl Event {
    fn new(kind: &'static str, when: i64, title: impl Into<String>) -> Self {
        Event { kind, when, title: title.into(), detail: String::new(), actor: None, link: None, note_id: 0, retracted_by: None, can_retract: false }
    }
}

#[derive(Deserialize, Default)]
pub struct HistoryQuery {
    #[serde(default)]
    pub r#type: String,
}

const KINDS: &[(&str, &str)] = &[
    ("notes", "Notes"),
    ("warnings", "Warnings"),
    ("bans", "Bans and account actions"),
    ("reports", "Reports"),
    ("moderation", "Moderation"),
    ("appeals", "Ban appeals"),
];

pub async fn history(ctx: Ctx, Path(uid): Path<i32>, Query(q): Query<HistoryQuery>) -> AppResult<Response> {
    require_staff(&ctx)?;
    let member: crate::models::User = sqlx::query_as("SELECT * FROM users WHERE uid = $1")
        .bind(uid)
        .fetch_optional(&ctx.app.db)
        .await?
        .ok_or_else(|| AppError::not_found("member"))?;
    let db = &ctx.app.db;
    let t = now();
    let want = |k: &str| q.r#type.is_empty() || q.r#type == k;
    let mut events: Vec<Event> = vec![];

    if want("notes") {
        let rows: Vec<(i64, i32, String, i64, i64, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT n.id, n.author, n.note, n.created, n.retracted_at, a.username, r.username
             FROM moderator_notes n LEFT JOIN users a ON a.uid = n.author LEFT JOIN users r ON r.uid = n.retracted_by
             WHERE n.uid = $1 ORDER BY n.id DESC LIMIT $2",
        )
        .bind(uid)
        .bind(SOURCE_LIMIT)
        .fetch_all(db)
        .await?;
        for (id, author, note, created, retracted_at, author_name, retracted_name) in rows {
            let mut e = Event::new("notes", created, "Note");
            e.detail = note;
            e.actor = Some(if author == 0 { "Imported".into() } else { author_name.unwrap_or_else(|| "Deleted member".into()) });
            e.note_id = id;
            if retracted_at > 0 {
                e.retracted_by = Some(retracted_name.unwrap_or_else(|| "Deleted member".into()));
            } else {
                e.can_retract = can_retract(author, created, ctx.uid(), ctx.is_admin(), t);
            }
            events.push(e);
        }
    }

    // Warning details follow the existing "can view warning logs" permission.
    let warnings_visible = ctx.perms.canviewwarnlogs;
    if want("warnings") && warnings_visible {
        let rows: Vec<(i32, String, i32, i64, i64, bool, i64, String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT w.wid, w.title, w.points, w.dateline, w.expires, w.expired, w.daterevoked, w.revokereason, i.username, rv.username
             FROM warnings w LEFT JOIN users i ON i.uid = w.issuedby LEFT JOIN users rv ON rv.uid = w.revokedby
             WHERE w.uid = $1 ORDER BY w.dateline DESC LIMIT $2",
        )
        .bind(uid)
        .bind(SOURCE_LIMIT)
        .fetch_all(db)
        .await?;
        for (_wid, title, points, dateline, expires, expired, revoked, revokereason, issuer, revoker) in rows {
            let mut e = Event::new("warnings", dateline, format!("Warned: {title} ({points} point{})", if points == 1 { "" } else { "s" }));
            e.actor = issuer;
            e.link = Some(format!("/warnings/{uid}"));
            e.detail = if revoked > 0 {
                format!("Revoked by {}: {revokereason}", revoker.unwrap_or_default())
            } else if expired {
                "Expired".into()
            } else if expires > 0 {
                format!("Expires {}", ctx.fmt_date(expires, "date"))
            } else {
                "Never expires".into()
            };
            events.push(e);
        }
    }

    if want("bans") {
        // Staff actions on the account from the audit log ("warned" comes from the warnings table).
        let actions: Vec<&str> = crate::audit::ACTIONS
            .iter()
            .filter(|a| a.2 == "staff" && a.0 != "warned" && !a.0.starts_with("appeal_") && (warnings_visible || a.0 != "warning_revoked"))
            .map(|a| a.0)
            .collect();
        let rows: Vec<(String, i64, serde_json::Value, Option<String>)> = sqlx::query_as(
            "SELECT a.action, a.dateline, a.details, u.username FROM user_audit a LEFT JOIN users u ON u.uid = a.actor_uid AND a.actor_uid > 0
             WHERE a.uid = $1 AND a.action = ANY($2) ORDER BY a.id DESC LIMIT $3",
        )
        .bind(uid)
        .bind(&actions)
        .bind(SOURCE_LIMIT)
        .fetch_all(db)
        .await?;
        for (action, dateline, details, actor) in rows {
            let mut e = Event::new("bans", dateline, crate::audit::describe(&action).0);
            e.actor = actor;
            if let Some(reason) = details.get("reason").and_then(|r| r.as_str()).filter(|r| !r.is_empty()) {
                e.detail = format!("Reason: {reason}");
            }
            events.push(e);
        }
    }

    if want("reports") {
        // Reports against the member, their content, ratings and messages; forum moderators only
        // see post reports from the forums they moderate, as on the reports page.
        let fids = crate::routes::modcp::mod_fids(&ctx);
        let rows: Vec<(i32, String, String, i32, i64, i32, i16, Option<String>, i32)> = sqlx::query_as(
            "SELECT r.rid, r.type, r.reason, r.reasonid, r.dateline, r.reports, r.reportstatus, u.username, r.id
             FROM reportedcontent r LEFT JOIN users u ON u.uid = r.uid
             WHERE ((r.type IN ('profile', 'reputation', 'pm') AND r.id2 = $1)
                    OR (r.type = 'post' AND r.id IN (SELECT pid FROM posts WHERE uid = $1)))
               AND ($2 OR r.type <> 'post' OR r.id3 = ANY($3))
             ORDER BY r.dateline DESC LIMIT $4",
        )
        .bind(uid)
        .bind(fids.is_none())
        .bind(fids.unwrap_or_default())
        .bind(SOURCE_LIMIT)
        .fetch_all(db)
        .await?;
        for (_rid, kind, comment, reasonid, dateline, count, status, reporter, id) in rows {
            let reason = ctx.cache.reportreasons.iter().find(|r| r.rid == reasonid).map(|r| r.title.clone()).unwrap_or_default();
            let what = match kind.as_str() { "post" => "post", "profile" => "profile", "reputation" => "reputation comment", _ => "private message" };
            let mut e = Event::new("reports", dateline, format!("Reported {what}: {reason}{}", if count > 1 { format!(" ({count} reports)") } else { String::new() }));
            e.detail = format!("{comment}{}", if status == 0 { " (open)" } else { " (closed)" }).trim().to_string();
            e.actor = reporter;
            if kind == "post" {
                e.link = Some(format!("/post/{id}"));
            }
            events.push(e);
        }
    }

    if want("moderation") {
        // Moderator-log entries about the member or their content. Ban lifts and warnings are
        // already on the timeline from the audit log and the warnings table.
        let rows: Vec<(String, i64, i32, i32, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT l.action, l.dateline, l.tid, l.pid, u.username, t.subject
             FROM moderatorlog l LEFT JOIN users u ON u.uid = l.uid LEFT JOIN threads t ON t.tid = l.tid
             WHERE (l.data->>'uid' = $1::text AND l.action NOT IN ('Lifted ban', 'Warned user', 'Banned user'))
                OR (l.pid > 0 AND l.pid IN (SELECT pid FROM posts WHERE uid = $1))
                OR (l.pid = 0 AND l.tid > 0 AND l.tid IN (SELECT tid FROM threads WHERE uid = $1))
             ORDER BY l.id DESC LIMIT $2",
        )
        .bind(uid)
        .bind(SOURCE_LIMIT)
        .fetch_all(db)
        .await?;
        for (action, dateline, tid, pid, actor, subject) in rows {
            let mut e = Event::new("moderation", dateline, action);
            e.actor = actor;
            e.detail = subject.unwrap_or_default();
            e.link = if pid > 0 { Some(format!("/post/{pid}")) } else if tid > 0 { Some(format!("/thread/{tid}")) } else { None };
            events.push(e);
        }
    }

    if want("appeals") {
        let rows: Vec<(i32, String, i16, i64, i64, String, Option<String>)> = sqlx::query_as(
            "SELECT a.id, a.statement, a.status, a.created, a.decided_at, a.response, d.username
             FROM ban_appeals a LEFT JOIN users d ON d.uid = a.decided_by WHERE a.uid = $1 ORDER BY a.id DESC LIMIT $2",
        )
        .bind(uid)
        .bind(SOURCE_LIMIT)
        .fetch_all(db)
        .await?;
        for (id, statement, status, created, decided_at, response, decider) in rows {
            let mut e = Event::new("appeals", created, "Ban appeal submitted");
            e.detail = statement;
            e.actor = Some(member.username.clone());
            e.link = Some(format!("/modcp/appeals/{id}"));
            events.push(e);
            if status != crate::routes::appeals::PENDING {
                let mut d = Event::new("appeals", decided_at,
                    if status == crate::routes::appeals::ACCEPTED { "Ban appeal accepted" } else { "Ban appeal rejected" });
                d.detail = response;
                d.actor = decider;
                d.link = Some(format!("/modcp/appeals/{id}"));
                events.push(d);
            }
        }
    }

    events.sort_by(|a, b| b.when.cmp(&a.when));
    let ban: Option<(String, i64)> = sqlx::query_as("SELECT reason, lifted FROM banned WHERE uid = $1")
        .bind(uid)
        .fetch_optional(db)
        .await?;
    let warn_pct = (member.warningpoints as i64 * 100 / ctx.settings().int("maxwarningpoints").max(1)).min(100);
    let kinds: Vec<_> = KINDS
        .iter()
        .filter(|k| k.0 != "warnings" || warnings_visible)
        .map(|k| minijinja::context! { key => k.0, label => k.1 })
        .collect();
    let group = ctx.cache.group(member.usergroup).map(|g| g.title.clone()).unwrap_or_default();
    ctx.render(
        "modcp/member.html",
        minijinja::context! {
            title => format!("Moderation history: {}", member.username),
            mcp_active => "finduser",
            breadcrumb => vec![("Mod CP".to_string(), "/modcp".to_string())],
            member => minijinja::context! {
                uid => member.uid, username => &member.username, group => group, regdate => member.regdate,
                postnum => member.postnum, warn_pct => warn_pct, warnings_visible => warnings_visible,
                suspendposting => member.suspendposting, moderateposts => member.moderateposts, suspendsignature => member.suspendsignature,
            },
            ban => ban.map(|(reason, lifted)| minijinja::context! { reason => reason, lifted => lifted }),
            events => events, kinds => kinds, filter => &q.r#type,
            can_ban => ctx.perms.canbanusers, can_warn => ctx.perms.canwarnusers,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn author_may_retract_within_the_window() {
        assert!(can_retract(5, 1_000, 5, false, 1_000 + RETRACT_WINDOW_SECS));
        assert!(!can_retract(5, 1_000, 5, false, 1_000 + RETRACT_WINDOW_SECS + 1));
    }

    #[test]
    fn other_staff_may_not_retract() {
        assert!(!can_retract(5, 1_000, 6, false, 1_001));
    }

    #[test]
    fn administrators_may_always_retract() {
        assert!(can_retract(5, 1_000, 6, true, 1_000 + 365 * 86_400));
        assert!(can_retract(0, 1_000, 6, true, 2_000), "including imported notes");
    }

    #[test]
    fn imported_notes_have_no_author_to_retract_them() {
        assert!(!can_retract(0, 1_000, 0, false, 1_001));
    }
}
