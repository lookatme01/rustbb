//! What staff (and members themselves) need to know about an account at a glance: flag chips,
//! check rows with a coloured dot, the headline numbers and any active restrictions. It is all
//! worked out from data rbb already keeps; nothing here writes.
//!
//! [`flags_for`] is the cheap batch version used by lists, hover cards and posts; [`load`] is the
//! full file for the admin and moderator user pages.

use crate::ctx::Ctx;
use crate::domain::staff::Cap;
use crate::error::AppResult;
use crate::models::User;
use crate::util::now;
use serde::Serialize;
use std::collections::HashMap;

/// A flag's or check row's colour. Red: the account is restricted. Orange: something to look at.
/// Blue: a staff role. Grey: plain information. Green: a check that passed.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Red,
    Orange,
    Blue,
    Grey,
    Green,
}

#[derive(Serialize, Clone, Debug)]
pub struct Flag {
    pub key: &'static str,
    pub level: Level,
    pub label: String,
}

/// One check row: a dot, a line of text and an optional second line and link.
#[derive(Serialize, Clone, Debug)]
pub struct Signal {
    pub group: &'static str,
    pub level: Level,
    pub text: String,
    pub detail: String,
    pub link: Option<String>,
}

/// What the viewer is allowed to see.
#[derive(Clone, Copy, Debug, Default)]
pub struct View {
    pub admin: bool,
    pub ip: bool,
    pub warnings: bool,
    pub reports: bool,
    pub notes: bool,
}

impl View {
    pub fn of(ctx: &Ctx) -> View {
        View {
            admin: ctx.is_admin(),
            ip: ctx.can(Cap::IpSearch),
            warnings: ctx.perms.canviewwarnlogs,
            reports: ctx.can(Cap::MemberReports) || ctx.staff().report_scope().posts_all,
            notes: ctx.can(Cap::ReadModNotes),
        }
    }
}

/// The facts behind the flags, one row per account.
#[derive(sqlx::FromRow, Clone, Debug, Default)]
pub struct Facts {
    pub uid: i32,
    pub usergroup: i32,
    pub regdate: i64,
    pub moderateposts: bool,
    pub moderationtime: i64,
    pub suspendposting: bool,
    pub suspensiontime: i64,
    pub suspendsignature: bool,
    pub suspendsigtime: i64,
    pub loginlockoutexpiry: i64,
    pub warningpoints: i32,
    pub away: bool,
    pub invisible: bool,
    pub coppauser: bool,
    pub has2fa: bool,
    pub is_system: bool,
    pub ban_lifted: Option<i64>,
    pub unconfirmed: bool,
    pub appeal: bool,
    pub mail_failing: bool,
    pub passkeys: i64,
    pub shared_ip: i64,
    pub shared_banned: i64,
    pub open_reports: i64,
}

const FACTS_SQL: &str = "SELECT u.uid, u.usergroup, u.regdate, u.moderateposts, u.moderationtime, u.suspendposting, u.suspensiontime,
        u.suspendsignature, u.suspendsigtime, u.loginlockoutexpiry, u.warningpoints, u.away, u.invisible, u.coppauser,
        u.totp_secret <> '' AS has2fa, u.is_system, b.lifted AS ban_lifted,
        EXISTS (SELECT 1 FROM awaitingactivation a WHERE a.uid = u.uid AND a.type IN ('r', 'b')) AS unconfirmed,
        EXISTS (SELECT 1 FROM ban_appeals ap WHERE ap.uid = u.uid AND ap.status = 0) AS appeal,
        EXISTS (SELECT 1 FROM mailqueue m WHERE lower(m.mailto) = lower(u.email) AND m.attempts > 0) AS mail_failing,
        (SELECT COUNT(*) FROM passkeys p WHERE p.uid = u.uid) AS passkeys,
        (SELECT COUNT(*) FROM users o WHERE o.uid <> u.uid AND (o.lastip = u.lastip OR o.regip = u.lastip OR o.lastip = u.regip OR o.regip = u.regip)) AS shared_ip,
        (SELECT COUNT(*) FROM users o JOIN banned ob ON ob.uid = o.uid WHERE o.uid <> u.uid AND (o.lastip = u.lastip OR o.regip = u.lastip OR o.lastip = u.regip OR o.regip = u.regip)) AS shared_banned,
        (SELECT COUNT(*) FROM reportedcontent r LEFT JOIN posts rp ON r.type = 'post' AND rp.pid = r.id
          WHERE r.reportstatus = 0 AND (rp.uid = u.uid OR (r.type <> 'post' AND r.id2 = u.uid))) AS open_reports
    FROM users u LEFT JOIN banned b ON b.uid = u.uid WHERE u.uid = ANY($1)";

pub async fn facts_for(db: &sqlx::PgPool, uids: &[i32]) -> AppResult<HashMap<i32, Facts>> {
    if uids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<Facts> = sqlx::query_as(FACTS_SQL).bind(uids).fetch_all(db).await?;
    Ok(rows.into_iter().map(|f| (f.uid, f)).collect())
}

/// Flag chips for several accounts at once.
pub async fn flags_for(ctx: &Ctx, uids: &[i32], view: View) -> AppResult<HashMap<i32, Vec<Flag>>> {
    let facts = facts_for(&ctx.app.db, uids).await?;
    let maxwarn = ctx.settings().int("maxwarningpoints").max(1);
    let t = now();
    Ok(facts
        .into_iter()
        .map(|(uid, f)| {
            let role = role_of(ctx, f.usergroup);
            (uid, flags(&f, view, t, maxwarn, role))
        })
        .collect())
}

/// "Admin" or "Moderator" from the account's primary group.
fn role_of(ctx: &Ctx, gid: i32) -> Option<&'static str> {
    let g = ctx.cache.group(gid)?;
    if g.perms.0.cancp {
        Some("Admin")
    } else if g.perms.0.issupermod || g.perms.0.canmodcp {
        Some("Moderator")
    } else {
        None
    }
}

/// "3d", "5mo", "2y": how long ago, compactly.
pub fn short_age(secs: i64) -> String {
    let days = secs.max(0) / 86_400;
    if days < 1 {
        "today".into()
    } else if days < 60 {
        format!("{days}d")
    } else if days < 730 {
        format!("{}mo", days / 30)
    } else {
        format!("{}y", days / 365)
    }
}

/// "· 3d left" for a restriction that ends at `until` (nothing for a permanent one).
fn left(until: i64, t: i64) -> String {
    if until <= 0 {
        String::new()
    } else {
        let days = ((until - t).max(0) + 86_399) / 86_400;
        format!(" · {days}d left")
    }
}

pub fn warn_pct(points: i32, maxwarn: i64) -> i64 {
    (points as i64 * 100 / maxwarn.max(1)).clamp(0, 100)
}

/// The flag chips for one account, most serious first.
pub fn flags(f: &Facts, view: View, t: i64, maxwarn: i64, role: Option<&str>) -> Vec<Flag> {
    let mut v = vec![];
    let mut add = |key, level, label: String| v.push(Flag { key, level, label });
    if f.is_system {
        add("system", Level::Grey, "System".into());
        return v;
    }
    if let Some(lifted) = f.ban_lifted {
        add(
            "banned",
            Level::Red,
            if lifted > 0 {
                format!("Banned{}", left(lifted, t))
            } else {
                "Banned · permanent".into()
            },
        );
    }
    if f.suspendposting {
        add("restricted", Level::Red, format!("Posting suspended{}", left(f.suspensiontime, t)));
    }
    if f.moderateposts {
        add("restricted", Level::Red, format!("Posts moderated{}", left(f.moderationtime, t)));
    }
    if f.suspendsignature {
        add("restricted", Level::Red, format!("Signature suspended{}", left(f.suspendsigtime, t)));
    }
    if f.loginlockoutexpiry > t {
        add("lockedout", Level::Red, "Locked out".into());
    }
    if f.appeal {
        add("appeal", Level::Orange, "Appeal pending".into());
    }
    if view.ip && f.shared_banned > 0 {
        add("ip", Level::Orange, "Shares IP with banned".into());
    }
    if view.admin && f.mail_failing {
        add("mail", Level::Orange, "Mail failing".into());
    }
    if f.unconfirmed {
        add("unconfirmed", Level::Orange, "Email unconfirmed".into());
    }
    if view.reports && f.open_reports > 0 {
        add(
            "reports",
            Level::Orange,
            format!("{} open report{}", f.open_reports, if f.open_reports == 1 { "" } else { "s" }),
        );
    }
    let pct = warn_pct(f.warningpoints, maxwarn);
    if view.warnings && pct > 0 {
        add("warned", Level::Orange, format!("Warning {pct}%"));
    }
    if let Some(r) = role {
        add("staff", Level::Blue, r.to_string());
    }
    if t - f.regdate < 7 * 86_400 {
        add("new", Level::Grey, "New".into());
    }
    if view.ip && f.shared_banned == 0 && f.shared_ip > 0 {
        add("ip", Level::Grey, "Shares an IP".into());
    }
    if view.admin {
        if f.has2fa {
            add("2fa", Level::Grey, "2FA".into());
        }
        if f.passkeys > 0 {
            add("passkey", Level::Grey, if f.passkeys == 1 { "Passkey".into() } else { format!("{} passkeys", f.passkeys) });
        }
        if f.invisible {
            add("invisible", Level::Grey, "Invisible".into());
        }
        if f.coppauser {
            add("coppa", Level::Grey, "COPPA".into());
        }
    }
    if f.away {
        add("away", Level::Grey, "Away".into());
    }
    v.sort_by_key(|f| f.level);
    v
}

/// A member worth a look on a dashboard: restricted, warned, locked out or reported, and active
/// in the past week.
#[derive(Serialize, Debug)]
pub struct Watched {
    pub uid: i32,
    pub username: String,
    pub formatted: String,
    pub avatar: String,
    pub lastactive: i64,
    pub flags: Vec<Flag>,
}

pub async fn members_to_watch(ctx: &Ctx, view: View, limit: i64) -> AppResult<Vec<Watched>> {
    let t = now();
    let rows: Vec<(i32, String, i32, i32, String, i64)> = sqlx::query_as(
        "SELECT u.uid, u.username, u.usergroup, u.displaygroup, u.avatar, u.lastactive FROM users u
         WHERE u.lastactive > $1 AND NOT u.is_system AND NOT EXISTS (SELECT 1 FROM banned b WHERE b.uid = u.uid)
           AND (u.moderateposts OR u.suspendposting OR u.suspendsignature OR u.warningpoints > 0 OR u.loginlockoutexpiry > $2
                OR EXISTS (SELECT 1 FROM reportedcontent r LEFT JOIN posts p ON r.type = 'post' AND p.pid = r.id
                           WHERE r.reportstatus = 0 AND (p.uid = u.uid OR (r.type <> 'post' AND r.id2 = u.uid))))
         ORDER BY u.lastactive DESC LIMIT $3",
    )
    .bind(t - 7 * 86_400)
    .bind(t)
    .bind(limit)
    .fetch_all(&ctx.app.db)
    .await?;
    let uids: Vec<i32> = rows.iter().map(|r| r.0).collect();
    let mut flags = flags_for(ctx, &uids, view).await?;
    Ok(rows
        .into_iter()
        .map(|(uid, username, g, d, avatar, lastactive)| Watched {
            formatted: ctx.cache.format_name(&username, g, d),
            flags: flags
                .remove(&uid)
                .unwrap_or_default()
                .into_iter()
                .filter(|f| matches!(f.level, Level::Red | Level::Orange))
                .collect(),
            uid,
            username,
            avatar,
            lastactive,
        })
        .collect())
}

#[derive(Serialize, Debug)]
pub struct Restriction {
    /// `posting`, `moderate` or `signature`: the value the restrict and lift forms post.
    pub kind: &'static str,
    pub label: String,
    pub until: i64,
}

pub fn restrictions(u: &User) -> Vec<Restriction> {
    let mut v = vec![];
    if u.moderateposts {
        v.push(Restriction { kind: "moderate", label: "Posts moderated".into(), until: u.moderationtime });
    }
    if u.suspendposting {
        v.push(Restriction { kind: "posting", label: "Posting suspended".into(), until: u.suspensiontime });
    }
    if u.suspendsignature {
        v.push(Restriction { kind: "signature", label: "Signature suspended".into(), until: u.suspendsigtime });
    }
    v
}

#[derive(Serialize, Debug)]
pub struct Stats {
    pub posts: i32,
    pub threads: i32,
    pub reputation: i32,
    pub warn_pct: i64,
    pub open_reports: i64,
    pub age: String,
}

#[derive(Serialize, Debug)]
pub struct Other {
    pub uid: i32,
    pub username: String,
    pub banned: bool,
}

#[derive(Serialize, Debug)]
pub struct PinnedNote {
    pub id: i64,
    pub note: String,
    pub author: String,
    pub created: i64,
}

/// Everything the member header and check rows show.
#[derive(Serialize, Debug)]
pub struct MemberFile {
    pub uid: i32,
    pub username: String,
    pub formatted: String,
    pub avatar: String,
    pub group: String,
    pub regdate: i64,
    pub lastactive: i64,
    pub lastip: String,
    pub device: String,
    pub stats: Stats,
    pub flags: Vec<Flag>,
    pub signals: Vec<Signal>,
    pub restrictions: Vec<Restriction>,
    pub ban: Option<(String, i64)>,
    pub others: Vec<Other>,
    pub pinned: Option<PinnedNote>,
    pub is_system: bool,
    /// For the ban preview: signed-in devices that a ban ends.
    pub active_logins: i64,
}

impl MemberFile {
    /// Check rows of one group (`identity`, `email`, `security`, `standing`).
    pub fn group(&self, g: &str) -> Vec<&Signal> {
        self.signals.iter().filter(|s| s.group == g).collect()
    }
}

/// The full member file for one account, as `view` may see it.
pub async fn load(ctx: &Ctx, u: &User, view: View) -> AppResult<MemberFile> {
    let db = &ctx.app.db;
    let t = now();
    let maxwarn = ctx.settings().int("maxwarningpoints").max(1);
    let f = facts_for(db, &[u.uid]).await?.remove(&u.uid).unwrap_or_default();
    let mut flags = flags(&f, view, t, maxwarn, role_of(ctx, u.usergroup));
    let mut signals: Vec<Signal> = vec![];
    let mut sig = |group, level, text: String, detail: String, link: Option<String>| {
        signals.push(Signal { group, level, text, detail, link })
    };

    // Identity
    let others: Vec<Other> = if view.ip {
        sqlx::query_as::<_, (i32, String, bool)>(
            "SELECT o.uid, o.username, EXISTS (SELECT 1 FROM banned b WHERE b.uid = o.uid) FROM users o
             WHERE o.uid <> $1 AND (o.lastip = $2::inet OR o.regip = $2::inet OR o.lastip = $3::inet OR o.regip = $3::inet)
             ORDER BY 3 DESC, o.lastactive DESC LIMIT 6",
        )
        .bind(u.uid)
        .bind(non_empty(&u.lastip))
        .bind(non_empty(&u.regip))
        .fetch_all(db)
        .await?
        .into_iter()
        .map(|(uid, username, banned)| Other { uid, username, banned })
        .collect()
    } else {
        vec![]
    };
    if !others.is_empty() {
        let banned = others.iter().filter(|o| o.banned).count();
        let n = f.shared_ip.max(others.len() as i64);
        sig(
            "identity",
            if banned > 0 { Level::Red } else { Level::Grey },
            format!(
                "Shares an IP address with {n} other account{}{}",
                if n == 1 { "" } else { "s" },
                if banned > 0 { format!(", {banned} of them banned") } else { String::new() }
            ),
            others
                .iter()
                .map(|o| if o.banned { format!("{} (banned)", o.username) } else { o.username.clone() })
                .collect::<Vec<_>>()
                .join(" · "),
            Some(format!("/modcp/ipsearch?ip={}", util_enc(non_empty(&u.lastip).or(non_empty(&u.regip)).unwrap_or_default()))),
        );
    }
    if view.admin {
        let spam: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(dateline) FROM spamlog WHERE (email <> '' AND lower(email) = lower($1)) OR (ipaddress IS NOT NULL AND ipaddress = $2::inet)",
        )
        .bind(&u.email)
        .bind(non_empty(&u.regip))
        .fetch_one(db)
        .await?;
        if let Some(when) = spam {
            flags.push(Flag { key: "spam", level: Level::Orange, label: "Spam log match".into() });
            sig(
                "identity",
                Level::Orange,
                "Matched the spam log".into(),
                format!("A sign-up with the same email or IP address was blocked on {}", ctx.fmt_date(when, "date")),
                Some("/admin/tools/spamlog".into()),
            );
        }
    }
    sig(
        "identity",
        Level::Grey,
        format!("Joined {}", ctx.fmt_date(u.regdate, "date")),
        if view.ip && !u.regip.is_empty() { format!("from {}", u.regip) } else { String::new() },
        None,
    );

    // Email (admins only)
    if view.admin && !u.is_system {
        sig(
            "email",
            if f.unconfirmed { Level::Orange } else { Level::Green },
            u.email.clone(),
            if f.unconfirmed { "Not confirmed yet".into() } else { "Confirmed".into() },
            None,
        );
        let pending: Option<String> = sqlx::query_scalar("SELECT misc FROM awaitingactivation WHERE uid = $1 AND type = 'e' ORDER BY aid DESC LIMIT 1")
            .bind(u.uid)
            .fetch_optional(db)
            .await?;
        if let Some(to) = pending {
            sig("email", Level::Grey, "Email change waiting for confirmation".into(), to, None);
        }
        let failing: Option<(i32, String)> = sqlx::query_as(
            "SELECT attempts, lasterror FROM mailqueue WHERE lower(mailto) = lower($1) AND attempts > 0 ORDER BY attempts DESC LIMIT 1",
        )
        .bind(&u.email)
        .fetch_optional(db)
        .await?;
        if let Some((attempts, err)) = failing {
            sig(
                "email",
                Level::Red,
                "Mail to this address is failing".into(),
                format!("{attempts} attempt{}{}", if attempts == 1 { "" } else { "s" }, if err.is_empty() { String::new() } else { format!(" · last error: {err}") }),
                Some("/admin/tools/mailerrors".into()),
            );
        }
    }

    // Security (admins only)
    let mut device = String::new();
    if view.admin && !u.is_system {
        if u.loginlockoutexpiry > t {
            sig(
                "security",
                Level::Red,
                format!("Locked out until {}", ctx.fmt_date(u.loginlockoutexpiry, "datetime")),
                format!("{} failed sign-in{}", u.loginattempts, if u.loginattempts == 1 { "" } else { "s" }),
                None,
            );
        }
        let has2fa = !u.totp_secret.is_empty();
        sig(
            "security",
            if has2fa || f.passkeys > 0 { Level::Green } else { Level::Grey },
            match (has2fa, f.passkeys) {
                (true, 0) => "Two-factor on".into(),
                (true, n) => format!("Two-factor on · {n} passkey{}", if n == 1 { "" } else { "s" }),
                (false, 0) => "No two-factor or passkey".into(),
                (false, n) => format!("{n} passkey{}", if n == 1 { "" } else { "s" }),
            },
            String::new(),
            None,
        );
        let logins: Vec<(String,)> = sqlx::query_as("SELECT useragent FROM logins WHERE uid = $1 AND expires > $2 ORDER BY lastused DESC")
            .bind(u.uid)
            .bind(t)
            .fetch_all(db)
            .await?;
        if let Some((ua,)) = logins.first() {
            device = crate::audit::device_label(ua);
        }
        let mut devices: Vec<String> = logins.iter().map(|(ua,)| crate::audit::device_label(ua)).collect();
        devices.dedup();
        sig(
            "security",
            Level::Grey,
            format!("{} active login{}", logins.len(), if logins.len() == 1 { "" } else { "s" }),
            devices.join(" · "),
            None,
        );
        let tokens: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_tokens WHERE uid = $1 AND revoked_at IS NULL AND expires_at > now()")
            .bind(u.uid)
            .fetch_one(db)
            .await?;
        if tokens > 0 {
            flags.push(Flag { key: "api", level: Level::Grey, label: "API tokens".into() });
            sig("security", Level::Grey, format!("{tokens} active API token{}", if tokens == 1 { "" } else { "s" }), String::new(), None);
        }
    }

    // Standing: what staff did to the account and what is still in force.
    let ban: Option<(String, i64)> = sqlx::query_as("SELECT reason, lifted FROM banned WHERE uid = $1")
        .bind(u.uid)
        .fetch_optional(db)
        .await?;
    if let Some((reason, lifted)) = &ban {
        sig(
            "standing",
            Level::Red,
            if *lifted > 0 { format!("Banned until {}", ctx.fmt_date(*lifted, "date")) } else { "Banned permanently".into() },
            reason.clone(),
            None,
        );
    }
    let restrictions = restrictions(u);
    for r in &restrictions {
        sig(
            "standing",
            Level::Red,
            if r.until > 0 { format!("{} until {}", r.label, ctx.fmt_date(r.until, "date")) } else { format!("{} until lifted", r.label) },
            String::new(),
            None,
        );
    }
    if view.reports && f.open_reports > 0 {
        sig(
            "standing",
            Level::Orange,
            format!("{} open report{} on their content", f.open_reports, if f.open_reports == 1 { "" } else { "s" }),
            String::new(),
            Some(format!("/modcp/member/{}?type=reports", u.uid)),
        );
    }
    let pct = warn_pct(u.warningpoints, maxwarn);
    if view.warnings && pct > 0 {
        let next: Option<(String, i64)> = sqlx::query_as(
            "SELECT title, expires FROM warnings WHERE uid = $1 AND NOT expired AND daterevoked = 0 ORDER BY CASE WHEN expires = 0 THEN 1 ELSE 0 END, expires LIMIT 1",
        )
        .bind(u.uid)
        .fetch_optional(db)
        .await?;
        sig(
            "standing",
            Level::Orange,
            format!("Warning level {pct}%"),
            next.map(|(title, exp)| if exp > 0 { format!("“{title}” expires {}", ctx.fmt_date(exp, "date")) } else { format!("“{title}” never expires") })
                .unwrap_or_default(),
            Some(format!("/warnings/{}", u.uid)),
        );
    }
    flags.sort_by_key(|f| f.level);

    let pinned = if view.notes {
        sqlx::query_as::<_, (i64, String, i64, Option<String>)>(
            "SELECT n.id, n.note, n.created, a.username FROM moderator_notes n LEFT JOIN users a ON a.uid = n.author
             WHERE n.uid = $1 AND n.pinned AND n.retracted_at = 0 LIMIT 1",
        )
        .bind(u.uid)
        .fetch_optional(db)
        .await?
        .map(|(id, note, created, author)| PinnedNote { id, note, created, author: author.unwrap_or_else(|| "Imported".into()) })
    } else {
        None
    };

    Ok(MemberFile {
        uid: u.uid,
        username: u.username.clone(),
        formatted: ctx.cache.format_name(&u.username, u.usergroup, u.displaygroup),
        avatar: u.avatar.clone(),
        group: ctx.cache.group(u.usergroup).map(|g| g.title.clone()).unwrap_or_default(),
        regdate: u.regdate,
        lastactive: u.lastactive,
        lastip: if view.ip { u.lastip.to_string() } else { String::new() },
        device,
        stats: Stats {
            posts: u.postnum,
            threads: u.threadnum,
            reputation: u.reputation,
            warn_pct: if view.warnings { pct } else { 0 },
            open_reports: if view.reports { f.open_reports } else { 0 },
            age: short_age(t - u.regdate),
        },
        flags,
        signals,
        restrictions,
        ban,
        others,
        pinned,
        is_system: u.is_system,
        active_logins: sqlx::query_scalar("SELECT COUNT(*) FROM logins WHERE uid = $1 AND expires > $2")
            .bind(u.uid)
            .bind(t)
            .fetch_one(db)
            .await?,
    })
}

/// Reasons staff typed before, most used first, plus the warning types: offered as suggestions
/// in the ban and restrict forms.
pub async fn reason_suggestions(db: &sqlx::PgPool) -> AppResult<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT r FROM (
            SELECT reason AS r, COUNT(*) AS n FROM banned WHERE reason <> '' GROUP BY reason
            UNION ALL
            SELECT details->>'reason', COUNT(*) FROM user_audit WHERE action = 'restricted' AND COALESCE(details->>'reason', '') <> '' AND dateline > $1 GROUP BY 1
            UNION ALL
            SELECT title, 0 FROM warningtypes
         ) s GROUP BY r ORDER BY SUM(n) DESC, r LIMIT 12",
    )
    .bind(now() - 365 * 86_400)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

fn non_empty(s: &str) -> Option<&str> {
    let s = s.trim();
    if s.is_empty() { None } else { Some(s) }
}

fn util_enc(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    fn admin() -> View {
        View { admin: true, ip: true, warnings: true, reports: true, notes: true }
    }

    fn keys(v: &[Flag]) -> Vec<&str> {
        v.iter().map(|f| f.key).collect()
    }

    #[test]
    fn a_quiet_old_account_has_no_flags() {
        let f = Facts { regdate: 0, ..Default::default() };
        assert!(flags(&f, admin(), 100 * DAY, 100, None).is_empty());
    }

    #[test]
    fn restrictions_come_first_and_show_time_left() {
        let t = 100 * DAY;
        let f = Facts { regdate: t - DAY, moderateposts: true, moderationtime: t + 3 * DAY, away: true, ..Default::default() };
        let v = flags(&f, admin(), t, 100, None);
        assert_eq!(keys(&v), ["restricted", "new", "away"]);
        assert_eq!(v[0].label, "Posts moderated · 3d left");
        assert_eq!(v[0].level, Level::Red);
    }

    #[test]
    fn a_permanent_ban_says_so() {
        let f = Facts { ban_lifted: Some(0), ..Default::default() };
        assert_eq!(flags(&f, admin(), 100 * DAY, 100, None)[0].label, "Banned · permanent");
    }

    #[test]
    fn moderators_do_not_see_admin_only_flags() {
        let f = Facts { mail_failing: true, has2fa: true, passkeys: 2, invisible: true, shared_banned: 1, shared_ip: 1, ..Default::default() };
        let modv = View { ip: false, warnings: true, reports: true, notes: true, admin: false };
        assert!(flags(&f, modv, 100 * DAY, 100, None).is_empty());
        let all = flags(&f, admin(), 100 * DAY, 100, None);
        assert_eq!(keys(&all), ["ip", "mail", "2fa", "passkey", "invisible"]);
    }

    #[test]
    fn sharing_an_ip_with_a_banned_account_is_orange_otherwise_grey() {
        let banned = Facts { shared_ip: 2, shared_banned: 1, ..Default::default() };
        assert_eq!(flags(&banned, admin(), 100 * DAY, 100, None)[0].level, Level::Orange);
        let plain = Facts { shared_ip: 2, ..Default::default() };
        assert_eq!(flags(&plain, admin(), 100 * DAY, 100, None)[0].level, Level::Grey);
    }

    #[test]
    fn the_system_account_is_only_system() {
        let f = Facts { is_system: true, moderateposts: true, ..Default::default() };
        assert_eq!(keys(&flags(&f, admin(), 0, 100, None)), ["system"]);
    }

    #[test]
    fn ages_are_compact() {
        assert_eq!(short_age(3 * DAY), "3d");
        assert_eq!(short_age(90 * DAY), "3mo");
        assert_eq!(short_age(800 * DAY), "2y");
        assert_eq!(short_age(10), "today");
    }
}
