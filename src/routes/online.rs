//! Who's Online.

use crate::ctx::Ctx;
use crate::error::{AppError, AppResult};
use crate::util::{escape_html, now};
use axum::response::Response;
use std::collections::HashMap;

/// Human-readable description of a location path (e.g. "Viewing thread X").
pub fn describe_location(ctx: &Ctx, loc: &str) -> String {
    let path = loc.split('?').next().unwrap_or(loc);
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let id = seg.get(1).and_then(|s| crate::util::leading_id(s));

    match (seg.first().copied().unwrap_or(""), id) {
        ("", _) | ("index.php", _) => "<a href=\"/\">Viewing the board index</a>".to_string(),
        ("forum", Some(fid)) => match ctx.cache.forum(fid) {
            Some(f) if ctx.access().can_see(fid) => format!(
                "Viewing forum <a href=\"/forum/{fid}\">{}</a>",
                escape_html(&f.name)
            ),
            _ => "Viewing a forum".into(),
        },
        ("thread", Some(_)) => "Reading a thread".into(),
        ("newthread", _) => "Posting a new thread".into(),
        ("newreply", _) => "Replying to a thread".into(),
        ("editpost", _) => "Editing a post".into(),
        ("user", _) => "Viewing a profile".into(),
        ("members", _) => "<a href=\"/members\">Viewing the member list</a>".into(),
        ("search", _) => "<a href=\"/search\">Searching</a>".into(),
        ("usercp", _) => "In the User Control Panel".into(),
        ("pm", _) => "Using private messaging".into(),
        ("modcp", _) => "In the Moderator Control Panel".into(),
        ("admin", _) => "In the Admin Control Panel".into(),
        ("online", _) => "<a href=\"/online\">Viewing Who's Online</a>".into(),
        ("calendar", _) => "<a href=\"/calendar\">Viewing the calendar</a>".into(),
        ("portal", _) => "<a href=\"/portal\">Viewing the portal</a>".into(),
        ("stats", _) => "<a href=\"/stats\">Viewing forum statistics</a>".into(),
        ("help", _) => "<a href=\"/help\">Reading help documents</a>".into(),
        ("member", _) if path.contains("register") => "Registering".into(),
        ("member", _) if path.contains("login") => "Logging in".into(),
        ("archive", _) => "Viewing the lite version".into(),
        _ => "Unknown location".into(),
    }
}

pub async fn online(ctx: Ctx) -> AppResult<Response> {
    if !ctx.perms.canviewonline {
        return Err(AppError::no_perm());
    }
    let s = ctx.settings();
    let cutoff = now() - s.int("wolcutoffmins").max(1) * 60;
    let order = if s.get("wolorder") == "username" {
        "u.username NULLS LAST, s.time DESC"
    } else {
        "s.time DESC"
    };
    let rows: Vec<(String, i32, String, i64, String, bool, String, i32, i32, Option<String>, Option<i32>, Option<i32>, Option<bool>)> = sqlx::query_as(&format!(
        "SELECT s.sid, s.uid, s.ip, s.time, s.location, s.anonymous, s.bot, s.location1, s.location2, u.username, u.usergroup, u.displaygroup, u.invisible
         FROM sessions s LEFT JOIN users u ON u.uid = s.uid WHERE s.time > $1 ORDER BY {order} LIMIT 1000"
    ))
    .bind(cutoff)
    .fetch_all(&ctx.app.db)
    .await?;
    // Thread titles for location2 (permission-checked).
    let tids: Vec<i32> = rows.iter().map(|r| r.8).filter(|t| *t > 0).collect();
    let threads: HashMap<i32, (String, i32)> = sqlx::query_as::<_, (i32, String, i32)>(
        "SELECT tid, subject, fid FROM threads WHERE tid = ANY($1)",
    )
    .bind(&tids)
    .fetch_all(&ctx.app.db)
    .await?
    .into_iter()
    .map(|(t, s, f)| (t, (s, f)))
    .collect();
    let mut seen_users = std::collections::HashSet::new();
    let mut list = vec![];
    // System is always online, though it has no session.
    let system: Option<(String, i32, i32)> =
        sqlx::query_as("SELECT username, usergroup, displaygroup FROM users WHERE is_system")
            .fetch_optional(&ctx.app.db)
            .await?;
    if let Some((name, g, d)) = system {
        let uid = ctx.cache.system_uid;
        list.push(minijinja::context! {
            who => format!("<a href=\"/user/{uid}\">{}</a>", ctx.cache.format_name(&name, g, d)),
            time => now(),
            location => crate::system::ONLINE_LOCATION,
            ip => None::<String>,
        });
    }
    for (_sid, uid, ip, time, loc, _anon, bot, _l1, l2, name, g, d, inv) in rows {
        if uid > 0 {
            if !seen_users.insert(uid) {
                continue;
            }
            if inv.unwrap_or(false) && !ctx.perms.canviewwolinvis && uid != ctx.uid() {
                continue;
            }
        }
        let mut location = describe_location(&ctx, &loc);
        if l2 > 0
            && let Some((subj, fid)) = threads.get(&l2)
            && ctx
                .access()
                .forum(*fid)
                .is_ok_and(|a| a.threads == crate::domain::access::Threads::All)
        {
            location = format!(
                "Reading thread <a href=\"/thread/{l2}\">{}</a>",
                escape_html(subj)
            );
        }
        let who = if !bot.is_empty() {
            format!("{} (search engine)", escape_html(&bot))
        } else if let Some(n) = name {
            format!(
                "<a href=\"/user/{uid}\">{}</a>{}",
                ctx.cache.format_name(&n, g.unwrap_or(2), d.unwrap_or(0)),
                if inv.unwrap_or(false) { "*" } else { "" }
            )
        } else {
            "Guest".into()
        };
        list.push(minijinja::context! { who => who, time => time, location => location, ip => if ctx.perms.canviewonlineips { Some(ip) } else { None } });
    }
    let summary = crate::routes::index::online_summary(&ctx).await?;
    ctx.render(
        "online.html",
        minijinja::context! { title => "Who's Online", list => list, summary => summary },
    )
    .await
}

pub async fn online_today(ctx: Ctx) -> AppResult<Response> {
    if !ctx.perms.canviewonline {
        return Err(AppError::no_perm());
    }
    let rows: Vec<(i32, String, i32, i32, i64, bool)> = sqlx::query_as(
        "SELECT uid, username, usergroup, displaygroup, lastactive, invisible FROM users WHERE lastactive > $1 ORDER BY lastactive DESC LIMIT 2000",
    )
    .bind(now() - 86400)
    .fetch_all(&ctx.app.db)
    .await?;
    let total = rows.len();
    let list: Vec<_> = rows
        .into_iter()
        .filter(|r| !r.5 || ctx.perms.canviewwolinvis || r.0 == ctx.uid())
        .map(|(uid, n, g, d, la, inv)| minijinja::context! { uid => uid, formatted => ctx.cache.format_name(&n, g, d), lastactive => la, invisible => inv })
        .collect();
    ctx.render(
        "online_today.html",
        minijinja::context! { title => "Online Today", list => list, total => total },
    )
    .await
}
