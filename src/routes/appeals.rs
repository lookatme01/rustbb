//! Ban appeals: banned members ask for a review from the banned page, and staff with ban rights
//! accept (lifting the ban) or reject it with a response.

use crate::app::App;
use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::util::now;
use axum::Router;
use axum::extract::Path;
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;

const MAX_TEXT_CHARS: usize = 2000;

pub fn router() -> Router<App> {
    Router::new()
        .route("/member/appeal", post(submit))
        .route("/modcp/appeals", get(queue))
        .route("/modcp/appeals/{id}", get(detail))
        .route("/modcp/appeals/{id}/decide", post(decide))
}

/// What the banned page offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppealState {
    /// Appeals are turned off in the board settings.
    Disabled,
    /// An appeal is waiting for staff.
    Pending,
    /// The member may submit an appeal now.
    CanAppeal,
    /// The last appeal against this ban was rejected; another is possible from `until`.
    Wait { until: i64 },
    /// The last appeal against this ban was rejected and only one is allowed per ban.
    Closed,
}

/// One earlier appeal: the ban it was against (that ban's start time), status and decision time.
#[derive(Debug, Clone)]
pub struct PastAppeal {
    pub ban_dateline: i64,
    pub status: i16,
    pub decided_at: i64,
}

pub const PENDING: i16 = 0;
pub const ACCEPTED: i16 = 1;
pub const REJECTED: i16 = 2;

/// Whether a member banned since `ban_dateline` may appeal now.
pub fn appeal_state(
    enabled: bool,
    ban_dateline: i64,
    past: &[PastAppeal],
    cooldown_days: i64,
    now: i64,
) -> AppealState {
    if !enabled {
        return AppealState::Disabled;
    }
    if past.iter().any(|a| a.status == PENDING) {
        return AppealState::Pending;
    }
    let last_rejection = past
        .iter()
        .filter(|a| a.ban_dateline == ban_dateline && a.status == REJECTED)
        .map(|a| a.decided_at)
        .max();
    match last_rejection {
        None => AppealState::CanAppeal,
        Some(_) if cooldown_days <= 0 => AppealState::Closed,
        Some(at) if now < at + cooldown_days * 86_400 => AppealState::Wait {
            until: at + cooldown_days * 86_400,
        },
        Some(_) => AppealState::CanAppeal,
    }
}

/// The member's current ban: (start time, reason, issued by). None when not banned.
async fn current_ban(db: &sqlx::PgPool, uid: i32) -> AppResult<Option<(i64, String, i32)>> {
    Ok(
        sqlx::query_as("SELECT dateline, reason, admin FROM banned WHERE uid = $1")
            .bind(uid)
            .fetch_optional(db)
            .await?,
    )
}

async fn state_for(ctx: &Ctx, uid: i32, ban_dateline: i64) -> AppResult<AppealState> {
    let past: Vec<(i64, i16, i64)> =
        sqlx::query_as("SELECT ban_dateline, status, decided_at FROM ban_appeals WHERE uid = $1")
            .bind(uid)
            .fetch_all(&ctx.app.db)
            .await?;
    let past: Vec<PastAppeal> = past
        .into_iter()
        .map(|(ban_dateline, status, decided_at)| PastAppeal {
            ban_dateline,
            status,
            decided_at,
        })
        .collect();
    let s = ctx.settings();
    Ok(appeal_state(
        s.bool("banappeals"),
        ban_dateline,
        &past,
        s.int("banappeal_cooldown_days"),
        now(),
    ))
}

/// What the banned page shows about appeals.
pub async fn banned_page_context(ctx: &Ctx) -> AppResult<minijinja::Value> {
    let Some((ban_dateline, _, _)) = current_ban(&ctx.app.db, ctx.uid()).await? else {
        return Ok(minijinja::Value::UNDEFINED);
    };
    let state = state_for(ctx, ctx.uid(), ban_dateline).await?;
    let last: Option<(i16, i64, i64, String)> = sqlx::query_as(
        "SELECT status, created, decided_at, response FROM ban_appeals WHERE uid = $1 AND ban_dateline = $2 ORDER BY id DESC LIMIT 1",
    )
    .bind(ctx.uid())
    .bind(ban_dateline)
    .fetch_optional(&ctx.app.db)
    .await?;
    let (name, until) = match state {
        AppealState::Disabled => ("disabled", 0),
        AppealState::Pending => ("pending", 0),
        AppealState::CanAppeal => ("can", 0),
        AppealState::Wait { until } => ("wait", until),
        AppealState::Closed => ("closed", 0),
    };
    Ok(minijinja::context! {
        state => name, until => until, max => MAX_TEXT_CHARS,
        last => last.map(|(status, created, decided_at, response)| minijinja::context! {
            rejected => status == REJECTED, created => created, decided_at => decided_at, response => response,
        }),
    })
}

#[derive(Deserialize, Default)]
pub struct AppealForm {
    #[serde(default, deserialize_with = "de::string")]
    pub statement: String,
}

pub async fn submit(ctx: Ctx, CsrfForm(f): CsrfForm<AppealForm>) -> AppResult<Response> {
    let me = ctx.require_login()?.clone();
    let Some((ban_dateline, reason, banned_by)) = current_ban(&ctx.app.db, me.uid).await? else {
        return Err(AppError::user("Your account isn't banned."));
    };
    match state_for(&ctx, me.uid, ban_dateline).await? {
        AppealState::CanAppeal => {}
        AppealState::Pending => {
            return Err(AppError::user("Your appeal is already waiting for staff."));
        }
        AppealState::Wait { until } => {
            return Err(AppError::user(format!(
                "You can appeal again from {}.",
                ctx.fmt_date(until, "datetime")
            )));
        }
        AppealState::Closed => return Err(AppError::user("This ban can't be appealed again.")),
        AppealState::Disabled => {
            return Err(AppError::user("Ban appeals are turned off on this board."));
        }
    }
    let statement = f.statement.trim();
    if statement.is_empty() {
        return Err(AppError::user(
            "Please explain why the ban should be reviewed.",
        ));
    }
    if statement.chars().count() > MAX_TEXT_CHARS {
        return Err(AppError::user(format!(
            "Please keep your appeal under {MAX_TEXT_CHARS} characters."
        )));
    }
    if !ctx.app.rate_check(&format!("appeal:{}", me.uid), 5, 3600) {
        return Err(AppError::RateLimited);
    }
    // The unique index allows one pending appeal per member, even with concurrent submissions.
    let inserted = sqlx::query(
        "INSERT INTO ban_appeals (uid, ban_dateline, ban_reason, banned_by, statement, created) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
    )
    .bind(me.uid)
    .bind(ban_dateline)
    .bind(&reason)
    .bind(banned_by)
    .bind(statement)
    .bind(now())
    .execute(&ctx.app.db)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Err(AppError::user("Your appeal is already waiting for staff."));
    }
    crate::audit::log(&ctx, me.uid, "appeal_submitted", serde_json::Value::Null).await;
    ctx.app.mod_counts.invalidate_all();
    Ok(ctx.redirect("/", "Your appeal has been sent to the staff."))
}

fn require_ban_rights(ctx: &Ctx) -> AppResult<()> {
    crate::routes::modcp::require_modcp(ctx)?;
    if !ctx.perms.canbanusers {
        return Err(AppError::no_perm());
    }
    Ok(())
}

type AppealRow = (
    i32,
    i32,
    String,
    i64,
    String,
    i32,
    String,
    i16,
    i64,
    i32,
    i64,
    String,
);
const APPEAL_COLS: &str = "a.id, a.uid, u.username, a.ban_dateline, a.ban_reason, a.banned_by, a.statement, a.status, a.created, a.decided_by, a.decided_at, a.response";

fn appeal_view(r: AppealRow, names: &std::collections::HashMap<i32, String>) -> minijinja::Value {
    let (
        id,
        uid,
        username,
        ban_dateline,
        ban_reason,
        banned_by,
        statement,
        status,
        created,
        decided_by,
        decided_at,
        response,
    ) = r;
    minijinja::context! {
        id => id, uid => uid, username => username, ban_dateline => ban_dateline, ban_reason => ban_reason,
        banned_by => names.get(&banned_by).cloned().unwrap_or_default(), statement => statement,
        status => match status { PENDING => "pending", ACCEPTED => "accepted", _ => "rejected" },
        created => created, decided_by => names.get(&decided_by).cloned().unwrap_or_default(), decided_at => decided_at, response => response,
    }
}

async fn names(
    db: &sqlx::PgPool,
    rows: &[AppealRow],
) -> AppResult<std::collections::HashMap<i32, String>> {
    let ids: Vec<i32> = rows
        .iter()
        .flat_map(|r| [r.5, r.9])
        .filter(|u| *u > 0)
        .collect();
    let rows: Vec<(i32, String)> =
        sqlx::query_as("SELECT uid, username FROM users WHERE uid = ANY($1)")
            .bind(&ids)
            .fetch_all(db)
            .await?;
    Ok(rows.into_iter().collect())
}

pub async fn queue(ctx: Ctx) -> AppResult<Response> {
    require_ban_rights(&ctx)?;
    let db = &ctx.app.db;
    let pending: Vec<AppealRow> = sqlx::query_as(&format!("SELECT {APPEAL_COLS} FROM ban_appeals a JOIN users u ON u.uid = a.uid WHERE a.status = 0 ORDER BY a.created"))
        .fetch_all(db)
        .await?;
    let decided: Vec<AppealRow> = sqlx::query_as(&format!("SELECT {APPEAL_COLS} FROM ban_appeals a JOIN users u ON u.uid = a.uid WHERE a.status <> 0 ORDER BY a.decided_at DESC LIMIT 50"))
        .fetch_all(db)
        .await?;
    let all: Vec<AppealRow> = pending.iter().chain(decided.iter()).cloned().collect();
    let n = names(db, &all).await?;
    ctx.render(
        "modcp/appeals.html",
        minijinja::context! {
            title => "Ban Appeals", mcp_active => "appeals", breadcrumb => vec![("Mod CP".to_string(), "/modcp".to_string())],
            pending => pending.into_iter().map(|r| appeal_view(r, &n)).collect::<Vec<_>>(),
            decided => decided.into_iter().map(|r| appeal_view(r, &n)).collect::<Vec<_>>(),
        },
    )
    .await
}

async fn load(ctx: &Ctx, id: i32) -> AppResult<AppealRow> {
    sqlx::query_as(&format!(
        "SELECT {APPEAL_COLS} FROM ban_appeals a JOIN users u ON u.uid = a.uid WHERE a.id = $1"
    ))
    .bind(id)
    .fetch_optional(&ctx.app.db)
    .await?
    .ok_or_else(|| AppError::not_found("appeal"))
}

pub async fn detail(ctx: Ctx, Path(id): Path<i32>) -> AppResult<Response> {
    require_ban_rights(&ctx)?;
    let row = load(&ctx, id).await?;
    let uid = row.1;
    let ban = current_ban(&ctx.app.db, uid).await?;
    let ban_lifts: Option<i64> = sqlx::query_scalar("SELECT lifted FROM banned WHERE uid = $1")
        .bind(uid)
        .fetch_optional(&ctx.app.db)
        .await?;
    let earlier: Vec<AppealRow> = sqlx::query_as(&format!("SELECT {APPEAL_COLS} FROM ban_appeals a JOIN users u ON u.uid = a.uid WHERE a.uid = $1 AND a.id <> $2 ORDER BY a.id DESC"))
        .bind(uid)
        .bind(id)
        .fetch_all(&ctx.app.db)
        .await?;
    let mut all = earlier.clone();
    all.push(row.clone());
    let n = names(&ctx.app.db, &all).await?;
    ctx.render(
        "modcp/appeal.html",
        minijinja::context! {
            title => format!("Ban appeal #{id}"), mcp_active => "appeals",
            breadcrumb => vec![("Mod CP".to_string(), "/modcp".to_string()), ("Ban appeals".to_string(), "/modcp/appeals".to_string())],
            appeal => appeal_view(row, &n), still_banned => ban.is_some(), ban_lifts => ban_lifts.unwrap_or(0),
            earlier => earlier.into_iter().map(|r| appeal_view(r, &n)).collect::<Vec<_>>(),
        },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct DecideForm {
    #[serde(default, deserialize_with = "de::string")]
    pub decision: String,
    #[serde(default, deserialize_with = "de::string")]
    pub response: String,
}

pub async fn decide(
    ctx: Ctx,
    Path(id): Path<i32>,
    CsrfForm(f): CsrfForm<DecideForm>,
) -> AppResult<Response> {
    require_ban_rights(&ctx)?;
    let row = load(&ctx, id).await?;
    let (uid, username, status) = (row.1, row.2.clone(), row.7);
    if status != PENDING {
        return Err(AppError::user("This appeal has already been decided."));
    }
    let accept = match f.decision.as_str() {
        "accept" => true,
        "reject" => false,
        _ => return Err(AppError::user("Choose accept or reject.")),
    };
    let response: String = f.response.trim().chars().take(MAX_TEXT_CHARS).collect();
    if !accept && response.is_empty() {
        return Err(AppError::user(
            "Please tell the member why the appeal was rejected.",
        ));
    }
    let still_banned = current_ban(&ctx.app.db, uid).await?.is_some();
    if accept && still_banned {
        // The same rank check as lifting a ban by hand.
        crate::routes::modcp::check_can_lift(&ctx, uid).await?;
    }
    let claimed = sqlx::query("UPDATE ban_appeals SET status = $2, decided_by = $3, decided_at = $4, response = $5 WHERE id = $1 AND status = 0")
        .bind(id)
        .bind(if accept { ACCEPTED } else { REJECTED })
        .bind(ctx.uid())
        .bind(now())
        .bind(&response)
        .execute(&ctx.app.db)
        .await?
        .rows_affected();
    if claimed == 0 {
        return Err(AppError::user("This appeal has already been decided."));
    }
    if accept && still_banned {
        crate::routes::modcp::lift_ban_logged(&ctx, uid).await?;
    }
    let action = if accept {
        "Accepted ban appeal"
    } else {
        "Rejected ban appeal"
    };
    crate::ops::log_moderator_action(
        &ctx.app,
        ctx.uid(),
        &ctx.ip,
        0,
        0,
        0,
        action,
        serde_json::json!({"uid": uid, "username": username, "subject": username, "appeal": id}),
    )
    .await;
    crate::audit::log(
        &ctx,
        uid,
        if accept {
            "appeal_accepted"
        } else {
            "appeal_rejected"
        },
        serde_json::json!({"response": response}),
    )
    .await;
    let (subject, intro) = if accept {
        (
            "Your ban appeal was accepted",
            "Your ban appeal was accepted and your account is active again.",
        )
    } else {
        (
            "Your ban appeal was rejected",
            "Your ban appeal was reviewed and rejected.",
        )
    };
    let body = if response.is_empty() {
        intro.to_string()
    } else {
        format!(
            "{intro}\n\nResponse from the staff:\n[quote]{}[/quote]",
            crate::parser::literal(&response)
        )
    };
    crate::routes::private::send_system_pm(&ctx.app, uid, subject, &body).await?;
    ctx.app.mod_counts.invalidate_all();
    Ok(ctx.redirect(
        "/modcp/appeals",
        if accept {
            "The appeal was accepted and the ban lifted."
        } else {
            "The appeal was rejected."
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BAN: i64 = 1_000_000;
    const DAY: i64 = 86_400;

    fn past(ban_dateline: i64, status: i16, decided_at: i64) -> PastAppeal {
        PastAppeal {
            ban_dateline,
            status,
            decided_at,
        }
    }

    #[test]
    fn disabled_board_setting_wins() {
        assert_eq!(
            appeal_state(false, BAN, &[], 30, BAN + DAY),
            AppealState::Disabled
        );
    }

    #[test]
    fn first_appeal_is_allowed() {
        assert_eq!(
            appeal_state(true, BAN, &[], 30, BAN + DAY),
            AppealState::CanAppeal
        );
    }

    #[test]
    fn pending_appeal_blocks_another() {
        assert_eq!(
            appeal_state(true, BAN, &[past(BAN, PENDING, 0)], 30, BAN + DAY),
            AppealState::Pending
        );
    }

    #[test]
    fn rejection_starts_the_cooldown() {
        let rejected = [past(BAN, REJECTED, BAN + DAY)];
        assert_eq!(
            appeal_state(true, BAN, &rejected, 30, BAN + 2 * DAY),
            AppealState::Wait {
                until: BAN + 31 * DAY
            }
        );
        assert_eq!(
            appeal_state(true, BAN, &rejected, 30, BAN + 31 * DAY),
            AppealState::CanAppeal
        );
    }

    #[test]
    fn zero_cooldown_means_one_appeal_per_ban() {
        let rejected = [past(BAN, REJECTED, BAN + DAY)];
        assert_eq!(
            appeal_state(true, BAN, &rejected, 0, BAN + 400 * DAY),
            AppealState::Closed
        );
    }

    #[test]
    fn appeals_against_earlier_bans_dont_count() {
        let old = [
            past(BAN - 100 * DAY, REJECTED, BAN - 99 * DAY),
            past(BAN - 50 * DAY, ACCEPTED, BAN - 49 * DAY),
        ];
        assert_eq!(
            appeal_state(true, BAN, &old, 0, BAN + DAY),
            AppealState::CanAppeal
        );
    }

    #[test]
    fn latest_rejection_decides_the_wait() {
        let two = [
            past(BAN, REJECTED, BAN + DAY),
            past(BAN, REJECTED, BAN + 40 * DAY),
        ];
        assert_eq!(
            appeal_state(true, BAN, &two, 30, BAN + 41 * DAY),
            AppealState::Wait {
                until: BAN + 70 * DAY
            }
        );
    }
}
