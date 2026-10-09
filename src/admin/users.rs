//! User administration: search, add, edit, activate, merge, delete, admin permissions.

use crate::ctx::{CsrfForm, Ctx};
use crate::error::{AppError, AppResult};
use crate::models::User;
use crate::util::{self, now};
use axum::Router;
use axum::extract::{Path, Query};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use std::collections::HashMap;

pub fn router() -> Router<crate::app::App> {
    Router::new()
        .route("/users", get(list))
        .route("/users/new", get(new_form).post(new_save))
        .route("/users/awaiting", get(awaiting).post(awaiting_action))
        .route("/users/merge", get(merge_form).post(merge_save))
        .route("/users/{uid}", get(edit_form).post(edit_save))
        .route("/users/{uid}/delete", post(delete))
        .route("/users/{uid}/erase", post(erase))
        .route("/users/{uid}/ban", post(ban))
        .route("/users/{uid}/security", post(security))
        .route("/users/{uid}/activity", get(activity))
        .route("/adminperms", get(adminperms))
        .route(
            "/adminperms/{uid}",
            get(adminperms_edit).post(adminperms_save),
        )
}

#[derive(Deserialize, Default)]
pub struct ListQ {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub flag: String,
    // Old links used one box per field; they keep working.
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub ip: String,
    #[serde(default)]
    pub gid: i32,
    #[serde(default)]
    pub sort: String,
    pub page: Option<i64>,
}

/// Quick filters: (query value, label, WHERE clause). `$7` is "a week ago".
const FLAG_SQL: &[(&str, &str, &str)] = &[
    ("banned", "Banned", "EXISTS (SELECT 1 FROM banned b WHERE b.uid = users.uid)"),
    ("restricted", "Restricted", "(moderateposts OR suspendposting OR suspendsignature)"),
    ("unconfirmed", "Unconfirmed", "EXISTS (SELECT 1 FROM awaitingactivation a WHERE a.uid = users.uid AND a.type IN ('r','b'))"),
    ("mail", "Mail failing", "EXISTS (SELECT 1 FROM mailqueue m WHERE lower(m.mailto) = lower(users.email) AND m.attempts > 0)"),
    ("ip", "Shares an IP", "(lastip IS NOT NULL AND EXISTS (SELECT 1 FROM users o WHERE o.uid <> users.uid AND (o.lastip = users.lastip OR o.regip = users.lastip)))"),
    ("new", "New this week", "regdate > $7"),
];

pub async fn list(ctx: Ctx, Query(q): Query<ListQ>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "users");
    let col = match q.sort.as_str() {
        "username" => "lower(username) ASC",
        "postnum" => "postnum DESC",
        "lastactive" => "lastactive DESC",
        "reputation" => "reputation DESC",
        _ => "regdate DESC",
    };
    let found = FLAG_SQL.iter().find(|f| f.0 == q.flag);
    let flag = found.map(|f| f.0).unwrap_or("");
    let flag_sql = found.map(|f| f.2).unwrap_or("TRUE");
    let week_ago = crate::util::now() - 7 * 86_400;
    let where_sql = format!("($1 = '' OR username ILIKE '%' || $1 || '%') AND ($2 = '' OR email ILIKE '%' || $2 || '%')
        AND ($3 = '' OR host(regip) LIKE $3 || '%' OR host(lastip) LIKE $3 || '%') AND ($4 = 0 OR usergroup = $4 OR $4 = ANY(additionalgroups))
        AND ($5 = '' OR username ILIKE '%' || $5 || '%' OR email ILIKE '%' || $5 || '%' OR host(regip) LIKE $5 || '%' OR host(lastip) LIKE $5 || '%')
        AND $7::bigint IS NOT NULL AND {flag_sql}");
    // $6 (offset) is unused by the count; bind a dummy so $7 lines up.
    let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM users WHERE {where_sql} AND $6::bigint IS NOT NULL"))
        .bind(q.username.trim())
        .bind(q.email.trim())
        .bind(q.ip.trim())
        .bind(q.gid)
        .bind(q.q.trim())
        .bind(0i64)
        .bind(week_ago)
        .fetch_one(&ctx.app.db)
        .await?;
    let enc = |s: &str| {
        percent_encoding::utf8_percent_encode(s.trim(), percent_encoding::NON_ALPHANUMERIC)
            .to_string()
    };
    let mut qs = format!("q={}&gid={}&sort={}", enc(&q.q), q.gid, enc(&q.sort));
    for (k, v) in [("username", &q.username), ("email", &q.email), ("ip", &q.ip)] {
        if !v.trim().is_empty() {
            qs.push_str(&format!("&{k}={}", enc(v)));
        }
    }
    let pg = util::paginate(
        total,
        50,
        util::clamp_page(q.page),
        &format!("/admin/users?{qs}&flag={flag}&page={{page}}"),
    );
    let rows: Vec<(i32, String, String, String, i32, i32, i64, i64, i32)> = sqlx::query_as(&format!(
        "SELECT uid, username, email, avatar, usergroup, displaygroup, regdate, lastactive, postnum FROM users WHERE {where_sql} ORDER BY {col}, uid LIMIT 50 OFFSET $6"
    ))
    .bind(q.username.trim())
    .bind(q.email.trim())
    .bind(q.ip.trim())
    .bind(q.gid)
    .bind(q.q.trim())
    .bind((pg.page - 1) * 50)
    .bind(week_ago)
    .fetch_all(&ctx.app.db)
    .await?;
    let uids: Vec<i32> = rows.iter().map(|r| r.0).collect();
    let mut flags = crate::member_file::flags_for(&ctx, &uids, crate::member_file::View::of(&ctx)).await?;
    let users: Vec<_> = rows
        .into_iter()
        .map(|(uid, n, e, av, g, d, reg, last, posts)| minijinja::context! { uid => uid, username => &n, avatar => av, formatted => ctx.cache.format_name(&n, g, d), email => e, group => ctx.cache.group(g).map(|x| x.title.clone()), regdate => reg, lastactive => last, postnum => posts, flags => flags.remove(&uid).unwrap_or_default() })
        .collect();
    let groups: Vec<(i32, String)> = sorted_groups(&ctx);
    // Pill counts are board-wide: they ignore the search box and group filter.
    let counts = flag_counts(&ctx, week_ago).await?;
    let pills: Vec<_> = std::iter::once(("", "All", counts[0]))
        .chain(FLAG_SQL.iter().enumerate().map(|(i, f)| (f.0, f.1, counts[i + 1])))
        .map(|(k, label, n)| minijinja::context! { key => k, label => label, n => n })
        .collect();
    crate::admin::page(&ctx, "admin/users.html", "users", "Users", minijinja::context! { users => users, pagination => pg, q => minijinja::context!{ q => q.q, username => q.username, email => q.email, ip => q.ip, gid => q.gid, sort => q.sort, flag => flag }, qs => qs, pills => pills, groups => groups, total => total }).await
}

/// Board-wide counts for the filter pills: all, then one per `FLAG_SQL` entry.
async fn flag_counts(ctx: &Ctx, week_ago: i64) -> AppResult<Vec<i64>> {
    use sqlx::Row;
    let mut sql = String::from("SELECT COUNT(*)");
    for f in FLAG_SQL {
        sql.push_str(&format!(", COUNT(*) FILTER (WHERE {})", f.2.replace("$7", "$1")));
    }
    sql.push_str(" FROM users");
    let row = sqlx::query(&sql).bind(week_ago).fetch_one(&ctx.app.db).await?;
    Ok((0..=FLAG_SQL.len()).map(|i| row.get::<i64, _>(i)).collect())
}

pub fn sorted_groups(ctx: &Ctx) -> Vec<(i32, String)> {
    let mut v: Vec<_> = ctx
        .cache
        .groups
        .values()
        .map(|g| (g.gid, g.title.clone()))
        .collect();
    v.sort();
    v
}

pub async fn new_form(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "users");
    crate::admin::page(
        &ctx,
        "admin/user_new.html",
        "users",
        "Add User",
        minijinja::context! { groups => sorted_groups(&ctx) },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct AnyForm {
    #[serde(default, flatten)]
    pub fields: HashMap<String, serde_json::Value>,
}

fn s(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(a)) => {
            a.last().and_then(|x| x.as_str()).unwrap_or("").to_string()
        }
        _ => String::new(),
    }
}
fn b(v: Option<&serde_json::Value>) -> bool {
    matches!(s(v).as_str(), "1" | "on" | "true" | "yes")
}
fn i(v: Option<&serde_json::Value>) -> i32 {
    s(v).trim().parse().unwrap_or(0)
}
fn ints(v: Option<&serde_json::Value>) -> Vec<i32> {
    match v {
        Some(serde_json::Value::String(s)) => {
            s.split(',').filter_map(|x| x.trim().parse().ok()).collect()
        }
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().and_then(|s| s.parse().ok()))
            .collect(),
        _ => vec![],
    }
}

pub async fn new_save(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "users");
    let username = s(f.fields.get("username")).trim().to_string();
    let email = s(f.fields.get("email")).trim().to_string();
    let password = s(f.fields.get("password"));
    let gid = i(f.fields.get("usergroup")).max(1);
    crate::system::guard_group(&ctx.cache, 0, &[gid])?;
    if username.is_empty() || !crate::auth::valid_username_chars(&username) {
        return Err(AppError::user("Please enter a valid username."));
    }
    if !util::valid_email(&email) {
        return Err(AppError::user("Please enter a valid email address."));
    }
    if password.len() < 6 {
        return Err(AppError::user(
            "The password must be at least 6 characters.",
        ));
    }
    if ctx.cache.group(gid).is_some_and(|g| g.perms.0.cancp) && !is_full_admin(&ctx).await? {
        return Err(AppError::user(
            "Only an administrator without Admin Permissions restrictions can create administrators.",
        ));
    }
    let hash = crate::auth::hash_password(&password).await?;
    let uid: i32 = sqlx::query_scalar("INSERT INTO users (username, password, email, usergroup, regdate, lastactive, regip, pmfolders) VALUES ($1, $2, $3, $4, $5, 0, $6, '[]') RETURNING uid")
        .bind(&username)
        .bind(hash)
        .bind(&email)
        .bind(gid)
        .bind(now())
        .bind(crate::util::IpText::from(&ctx.ip))
        .fetch_one(&ctx.app.db)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(d) if d.is_unique_violation() => AppError::user("That username is already taken."),
            e => AppError::Db(e),
        })?;
    sqlx::query(
        "UPDATE counters SET numusers = numusers + 1, lastuid = $1, lastusername = $2 WHERE id = 1",
    )
    .bind(uid)
    .bind(&username)
    .execute(&ctx.app.db)
    .await?;
    ctx.app.stats_cache.invalidate_all();
    crate::admin::log(
        &ctx,
        "users",
        "Added user",
        serde_json::json!({"uid": uid, "username": username}),
    )
    .await;
    Ok(ctx.redirect(&format!("/admin/users/{uid}"), "The user has been created."))
}

async fn load(ctx: &Ctx, uid: i32) -> AppResult<User> {
    sqlx::query_as(&format!(
        "SELECT {} FROM users WHERE uid = $1",
        crate::models::USER_COLUMNS
    ))
    .bind(uid)
    .fetch_optional(&ctx.app.db)
    .await?
    .ok_or_else(|| AppError::not_found("user"))
}

/// Groups an account belongs to, primary first.
fn member_groups(u: &User) -> impl Iterator<Item = i32> + '_ {
    std::iter::once(u.usergroup).chain(u.additionalgroups.iter().copied())
}

/// True when any of the account's groups grants Admin CP access.
fn has_cancp(ctx: &Ctx, u: &User) -> bool {
    member_groups(u).any(|g| ctx.cache.group(g).is_some_and(|g| g.perms.0.cancp))
}

/// True when the account is an administrator or a (super) moderator.
fn has_staff_role(ctx: &Ctx, u: &User) -> bool {
    has_cancp(ctx, u)
        || member_groups(u).any(|g| {
            ctx.cache
                .group(g)
                .is_some_and(|g| g.perms.0.issupermod || g.perms.0.canmodcp)
        })
}

/// Whether the signed-in admin has no per-module restrictions (Admin Permissions). Only these
/// admins may touch other administrators' accounts or admin permissions.
async fn is_full_admin(ctx: &Ctx) -> AppResult<bool> {
    let perms: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT permissions FROM adminoptions WHERE uid = $1")
            .bind(ctx.uid())
            .fetch_optional(&ctx.app.db)
            .await?;
    let restricted = perms
        .as_ref()
        .and_then(|p| p.as_object())
        .is_some_and(|o| o.values().any(|v| v == &serde_json::Value::Bool(false)));
    Ok(!restricted)
}

/// A module-restricted admin may not act on another administrator, or on other staff when
/// `staff_too`, or it could take over the account (password, 2FA, groups) or remove it.
async fn guard_target(ctx: &Ctx, target: &User, staff_too: bool) -> AppResult<()> {
    // System's cosmetic fields are editable by any admin; its other actions are refused elsewhere.
    if target.uid == ctx.uid() || target.is_system {
        return Ok(());
    }
    let protected = if staff_too {
        has_staff_role(ctx, target)
    } else {
        has_cancp(ctx, target)
    };
    if protected && !is_full_admin(ctx).await? {
        return Err(AppError::user(
            "Only an administrator without Admin Permissions restrictions can do that to a staff account.",
        ));
    }
    Ok(())
}

/// Refuse putting `target` into an administrator group it is not already in, unless the
/// signed-in admin is unrestricted (otherwise a restricted admin could mint full admins).
async fn guard_admin_grant(ctx: &Ctx, target: &User, new_groups: &[i32]) -> AppResult<()> {
    let current: Vec<i32> = member_groups(target).collect();
    let grants_cancp = new_groups
        .iter()
        .any(|g| !current.contains(g) && ctx.cache.group(*g).is_some_and(|g| g.perms.0.cancp));
    if grants_cancp && !is_full_admin(ctx).await? {
        return Err(AppError::user(
            "Only an administrator without Admin Permissions restrictions can grant administrator access.",
        ));
    }
    Ok(())
}

#[derive(Deserialize, Default)]
pub struct EditQ {
    #[serde(default)]
    pub tab: String,
    #[serde(default)]
    pub r#type: String,
}

/// The member file: who this is and what's wrong at a glance, the actions, the timeline, and the
/// edit form and sign-in details on their own tabs.
pub async fn edit_form(ctx: Ctx, Path(uid): Path<i32>, Query(q): Query<EditQ>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "users");
    let user = load(&ctx, uid).await?;
    let view = crate::member_file::View::of(&ctx);
    let mf = crate::member_file::load(&ctx, &user, view).await?;
    let tab = match q.tab.as_str() {
        "details" | "ips" => q.tab.as_str(),
        _ => "timeline",
    };
    // The notes box always wants the notes; the timeline tab shows the chosen kinds.
    let events = crate::routes::modnotes::timeline(&ctx, &user, if tab == "timeline" { &q.r#type } else { "notes" }).await?;
    let notes = if tab == "timeline" && !q.r#type.is_empty() && q.r#type != "notes" {
        crate::routes::modnotes::timeline(&ctx, &user, "notes").await?
    } else {
        vec![]
    };
    let values: HashMap<String, String> =
        sqlx::query_as::<_, (i32, String)>("SELECT fid, value FROM userfields WHERE uid = $1")
            .bind(uid)
            .fetch_all(&ctx.app.db)
            .await?
            .into_iter()
            .map(|(f, v)| (f.to_string(), v))
            .collect();
    let (ips, logins): (Vec<(String, i64, Vec<(i32, String, bool)>)>, Vec<(String, i64, String, String)>) = if tab == "ips" {
        let rows: Vec<(String, i64)> = sqlx::query_as("SELECT host(ipaddress), MAX(dateline) FROM posts WHERE uid = $1 AND ipaddress IS NOT NULL GROUP BY ipaddress ORDER BY 2 DESC LIMIT 20").bind(uid).fetch_all(&ctx.app.db).await?;
        let mut ips = vec![];
        for (ip, when) in rows {
            let others: Vec<(i32, String, bool)> = sqlx::query_as(
                "SELECT o.uid, o.username, EXISTS (SELECT 1 FROM banned b WHERE b.uid = o.uid) FROM users o WHERE o.uid <> $1 AND (o.lastip = $2::inet OR o.regip = $2::inet) ORDER BY 3 DESC LIMIT 5",
            )
            .bind(uid)
            .bind(&ip)
            .fetch_all(&ctx.app.db)
            .await?;
            ips.push((ip, when, others));
        }
        let logins: Vec<(String, i64, String, String)> = sqlx::query_as(
            "SELECT token_hash, lastused, COALESCE(host(ip), ''), useragent FROM logins WHERE uid = $1 AND expires > $2 ORDER BY lastused DESC LIMIT 20",
        )
        .bind(uid)
        .bind(crate::util::now())
        .fetch_all(&ctx.app.db)
        .await?;
        let logins = logins.into_iter().map(|(_, used, ip, ua)| (String::new(), used, ip, crate::audit::device_label(&ua))).collect();
        (ips, logins)
    } else {
        (vec![], vec![])
    };
    let reasons = crate::member_file::reason_suggestions(&ctx.app.db).await?;
    let kinds: Vec<_> = crate::routes::modnotes::visible_kinds(&ctx);
    crate::admin::page(
        &ctx,
        "admin/user_edit.html",
        "users",
        &user.username,
        minijinja::context! {
            user => &user, mf => &mf, section_tab => tab, filter => &q.r#type, events => events, notes => if notes.is_empty() { None } else { Some(notes) },
            kinds => kinds, reasons => reasons,
            email => &user.email, regip => user.regip.to_string(), lastip => user.lastip.to_string(), groups => sorted_groups(&ctx),
            fields => ctx.cache.profilefields.to_vec(), values => values, ips => ips, logins => logins,
            has2fa => !user.totp_secret.is_empty(),
            passkeys => sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM passkeys WHERE uid = $1").bind(uid).fetch_one(&ctx.app.db).await?,
            can_write_notes => ctx.can(crate::domain::staff::Cap::WriteModNotes),
            can_warn => ctx.perms.canwarnusers,
        },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct SecurityForm {
    #[serde(default, deserialize_with = "crate::ctx::de::bool")]
    pub reset2fa: bool,
    #[serde(default, deserialize_with = "crate::ctx::de::bool")]
    pub removepasskeys: bool,
    #[serde(default, deserialize_with = "crate::ctx::de::bool")]
    pub logoutall: bool,
    #[serde(default, deserialize_with = "crate::ctx::de::bool")]
    pub sendreset: bool,
}

/// Sign-in help for a member: log them out everywhere, turn off two-factor, remove passkeys for
/// a lost device, or send them a password reset link.
pub async fn security(ctx: Ctx, Path(uid): Path<i32>, crate::ctx::CsrfForm(f): crate::ctx::CsrfForm<SecurityForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "users");
    let user = load(&ctx, uid).await?;
    crate::system::guard(&ctx.cache, uid, "signed out")?;
    guard_target(&ctx, &user, false).await?;
    let mut done = vec![];
    if f.reset2fa {
        sqlx::query("UPDATE users SET totp_secret = '' WHERE uid = $1").bind(uid).execute(&ctx.app.db).await?;
        crate::audit::log(&ctx, uid, "twofa_disabled", serde_json::json!({"via": "admin"})).await;
        done.push("two-factor is off");
    }
    if f.removepasskeys {
        let n = sqlx::query("DELETE FROM passkeys WHERE uid = $1").bind(uid).execute(&ctx.app.db).await?.rows_affected();
        if n > 0 {
            crate::audit::log(&ctx, uid, "passkey_removed", serde_json::json!({"all": n, "via": "admin"})).await;
            done.push("their passkeys are removed");
        }
    }
    if f.logoutall {
        crate::auth::destroy_all_logins(&ctx.app, uid, if uid == ctx.uid() { ctx.token_hash.as_deref() } else { None }).await?;
        done.push("they're signed out everywhere");
    }
    if f.sendreset {
        crate::usecase::accounts::request_password_reset(&ctx.app, &crate::audit::Actor::from_ctx(&ctx), &user.email).await?;
        done.push("a password reset link is on its way");
    }
    crate::admin::log(&ctx, "users", "Changed a member's sign-in", serde_json::json!({"uid": uid, "username": user.username, "done": done})).await;
    let msg = if done.is_empty() {
        "Nothing changed.".to_string()
    } else {
        let mut m = done.join(", ");
        m[..1].make_ascii_uppercase();
        format!("{m}.")
    };
    Ok(ctx.redirect(&format!("/admin/users/{uid}?tab=ips"), &msg))
}

pub async fn edit_save(
    ctx: Ctx,
    Path(uid): Path<i32>,
    CsrfForm(f): CsrfForm<AnyForm>,
) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "users");
    let user = load(&ctx, uid).await?;
    guard_target(&ctx, &user, false).await?;
    let fl = &f.fields;
    let username = s(fl.get("username")).trim().to_string();
    // System keeps its email, groups and password; only cosmetic fields are editable.
    let sys = user.is_system;
    let email = if sys {
        user.email.clone()
    } else {
        s(fl.get("email")).trim().to_string()
    };
    let gid = if sys {
        user.usergroup
    } else {
        i(fl.get("usergroup"))
    };
    if ctx.cache.group(gid).is_none() {
        return Err(AppError::user("Invalid primary group."));
    }
    if uid == ctx.uid()
        && !ctx
            .cache
            .group(gid)
            .map(|g| g.perms.0.cancp)
            .unwrap_or(false)
    {
        return Err(AppError::user(
            "You cannot remove your own administrator access.",
        ));
    }
    // Checked before the rename below so a refused edit changes nothing.
    let mut additional: Vec<i32> = ints(fl.get("additionalgroups"))
        .into_iter()
        .filter(|g| *g != gid && ctx.cache.group(*g).is_some())
        .collect();
    additional.sort();
    additional.dedup();
    if sys {
        additional = user.additionalgroups.clone();
    }
    let displaygroup = i(fl.get("displaygroup"));
    crate::system::guard_group(&ctx.cache, uid, &[gid, displaygroup])?;
    crate::system::guard_group(&ctx.cache, uid, &additional)?;
    guard_admin_grant(&ctx, &user, &[&[gid][..], &additional].concat()).await?;
    if !sys && !util::valid_email(&email) {
        return Err(AppError::user("Invalid email address."));
    }
    if username != user.username {
        if username.is_empty() || !crate::auth::valid_username_chars(&username) {
            return Err(AppError::user("Invalid username."));
        }
        let taken: Option<i32> = sqlx::query_scalar(
            "SELECT uid FROM users WHERE lower(username) = lower($1) AND uid <> $2",
        )
        .bind(&username)
        .bind(uid)
        .fetch_optional(&ctx.app.db)
        .await?;
        if taken.is_some() {
            return Err(AppError::user("That username is already taken."));
        }
        crate::routes::usercp::rename_user(&ctx.app, uid, &user.username, &username).await?;
    }
    sqlx::query(
        "UPDATE users SET email = $2, usergroup = $3, additionalgroups = $4, displaygroup = $5, usertitle = $6, website = $7, signature = $8,
            postnum = COALESCE($9, postnum), threadnum = COALESCE($10, threadnum), timezone = $11,
            avatar = CASE WHEN $12 THEN '' ELSE avatar END, avatartype = CASE WHEN $12 THEN '' ELSE avatartype END
         WHERE uid = $1",
    )
    .bind(uid)
    .bind(&email)
    .bind(gid)
    .bind(&additional)
    .bind(displaygroup)
    .bind(s(fl.get("usertitle")).trim())
    .bind(s(fl.get("website")).trim())
    .bind(s(fl.get("signature")).trim())
    // A form without the statistics fields keeps the stored counts instead of zeroing them.
    .bind(fl.get("postnum").map(|v| i(Some(v)).max(0)))
    .bind(fl.get("threadnum").map(|v| i(Some(v)).max(0)))
    .bind(s(fl.get("timezone")).trim())
    .bind(b(fl.get("removeavatar")))
    .execute(&ctx.app.db)
    .await?;
    let pw = s(fl.get("newpassword"));
    if !pw.is_empty() {
        crate::system::guard(&ctx.cache, uid, "given a password")?;
        if pw.len() < 6 {
            return Err(AppError::user(
                "The new password must be at least 6 characters.",
            ));
        }
        let h = crate::auth::hash_password(&pw).await?;
        sqlx::query("UPDATE users SET password = $2, loginattempts = 0, loginlockoutexpiry = 0 WHERE uid = $1").bind(uid).bind(h).execute(&ctx.app.db).await?;
        crate::auth::destroy_all_logins(
            &ctx.app,
            uid,
            if uid == ctx.uid() {
                ctx.token_hash.as_deref()
            } else {
                None
            },
        )
        .await?;
    }
    let mut errors = vec![];
    let vals = crate::routes::usercp::collect_profile_fields(&ctx, fl, &mut errors, false);
    crate::routes::usercp::save_profile_fields(&ctx.app.db, uid, &vals).await?;
    if b(fl.get("removepasskeys")) {
        let n = sqlx::query("DELETE FROM passkeys WHERE uid = $1")
            .bind(uid)
            .execute(&ctx.app.db)
            .await?
            .rows_affected();
        if n > 0 {
            crate::audit::log(
                &ctx,
                uid,
                "passkey_removed",
                serde_json::json!({"all": n, "via": "admin"}),
            )
            .await;
        }
    }
    if b(fl.get("logoutall")) {
        crate::auth::destroy_all_logins(
            &ctx.app,
            uid,
            if uid == ctx.uid() {
                ctx.token_hash.as_deref()
            } else {
                None
            },
        )
        .await?;
    }
    crate::admin::log(
        &ctx,
        "users",
        "Edited user",
        serde_json::json!({"uid": uid, "username": username}),
    )
    .await;
    crate::audit::log(&ctx, uid, "staff_edit", serde_json::json!({"via": "admin"})).await;
    Ok(ctx.redirect(&format!("/admin/users/{uid}"), "The user has been updated."))
}

/// Erase a member's personal data on request: delete the account and anonymize what stays,
/// whatever the Privacy settings say. The erasure log records that it happened, without the data.
pub async fn erase(
    ctx: Ctx,
    Path(uid): Path<i32>,
    CsrfForm(f): CsrfForm<AnyForm>,
) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "users");
    crate::system::guard(&ctx.cache, uid, "erased")?;
    if uid == ctx.uid() {
        return Err(AppError::user("You cannot erase your own account here."));
    }
    let user = load(&ctx, uid).await?;
    guard_target(&ctx, &user, true).await?;
    if s(f.fields.get("confirm")).trim() != user.username {
        return Err(AppError::user(
            "Type the member's username exactly to confirm the erasure.",
        ));
    }
    let keep_posts = b(f.fields.get("keepposts"));
    let reference: String = s(f.fields.get("reference"))
        .trim()
        .chars()
        .take(200)
        .collect();
    crate::routes::usercp::delete_user_with(&ctx.app, uid, !keep_posts, true).await?;
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO erasure_log (former_uid, performed_by, dateline, kept_posts, reference) VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(uid)
    .bind(ctx.uid())
    .bind(now())
    .bind(keep_posts)
    .bind(&reference)
    .fetch_one(&ctx.app.db)
    .await?;
    crate::admin::log(
        &ctx,
        "users",
        "Erased a member's personal data",
        serde_json::json!({"uid": uid, "erasure": id}),
    )
    .await;
    Ok(ctx.redirect(
        "/admin/users",
        "The member's personal data has been erased.",
    ))
}

pub async fn delete(
    ctx: Ctx,
    Path(uid): Path<i32>,
    CsrfForm(f): CsrfForm<AnyForm>,
) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "users");
    crate::system::guard(&ctx.cache, uid, "deleted")?;
    if uid == ctx.uid() {
        return Err(AppError::user("You cannot delete your own account here."));
    }
    let user = load(&ctx, uid).await?;
    guard_target(&ctx, &user, true).await?;
    let content = b(f.fields.get("deletecontent"));
    crate::routes::usercp::delete_user(&ctx.app, uid, content).await?;
    crate::admin::log(
        &ctx,
        "users",
        "Deleted user",
        serde_json::json!({"uid": uid, "username": user.username, "content": content}),
    )
    .await;
    Ok(ctx.redirect(
        "/admin/users",
        &format!("{} has been deleted.", user.username),
    ))
}

pub async fn ban(
    ctx: Ctx,
    Path(uid): Path<i32>,
    CsrfForm(f): CsrfForm<AnyForm>,
) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "bans");
    crate::system::guard(&ctx.cache, uid, "banned")?;
    let user = load(&ctx, uid).await?;
    if uid == ctx.uid() {
        return Err(AppError::user("You cannot ban yourself."));
    }
    guard_target(&ctx, &user, true).await?;
    let back = format!("/admin/users/{uid}");
    if b(f.fields.get("lift")) {
        crate::routes::modcp::lift_ban_for(&ctx.app, uid).await?;
        crate::audit::log(&ctx, uid, "unbanned", serde_json::Value::Null).await;
        return Ok(ctx.redirect(&back, "The ban has been lifted."));
    }
    let reason = s(f.fields.get("reason")).trim().to_string();
    let days = i(f.fields.get("days")) as i64;
    crate::routes::modcp::ban_user(&ctx.app, &user, 7, &reason, days, ctx.uid()).await?;
    let deleted = b(f.fields.get("deleteposts"));
    if deleted {
        crate::routes::usercp::delete_user_content(&ctx.app, uid).await?;
    }
    crate::admin::log(
        &ctx,
        "bans",
        "Banned user",
        serde_json::json!({"uid": uid, "username": user.username, "deleteposts": deleted}),
    )
    .await;
    crate::audit::log(&ctx, uid, "banned", serde_json::json!({"reason": reason, "days": days})).await;
    let msg = format!("{} has been banned.", user.username);
    // Deleted content can't come back, so only a plain ban offers an undo.
    if deleted {
        Ok(ctx.redirect(&back, &msg))
    } else {
        Ok(ctx.redirect_undo(&back, &msg, "/modcp/liftban", &[("uid", uid.to_string()), ("back", back.clone())]))
    }
}

pub async fn awaiting(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "users");
    let rows: Vec<(i32, String, String, i64, String, Option<String>)> = sqlx::query_as(
        "SELECT u.uid, u.username, u.email, u.regdate, COALESCE(host(u.regip), ''), (SELECT type FROM awaitingactivation a WHERE a.uid = u.uid ORDER BY aid DESC LIMIT 1)
         FROM users u WHERE u.usergroup = 5 ORDER BY u.regdate DESC LIMIT 500",
    )
    .fetch_all(&ctx.app.db)
    .await?;
    crate::admin::page(
        &ctx,
        "admin/awaiting.html",
        "users",
        "Awaiting Activation",
        minijinja::context! { rows => rows },
    )
    .await
}

pub async fn awaiting_action(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "users");
    let uids = ints(f.fields.get("uids"));
    match s(f.fields.get("action")).as_str() {
        "activate" => {
            let activated: Vec<(i32, String)> = sqlx::query_as("UPDATE users SET usergroup = 2 WHERE uid = ANY($1) AND usergroup = 5 RETURNING uid, username")
                .bind(&uids)
                .fetch_all(&ctx.app.db)
                .await?;
            if let Err(e) = crate::system::welcome(&ctx.app, None, &activated).await {
                tracing::warn!("{e:#}");
            }
            sqlx::query(
                "DELETE FROM awaitingactivation WHERE uid = ANY($1) AND type IN ('r', 'b')",
            )
            .bind(&uids)
            .execute(&ctx.app.db)
            .await?;
            let s = ctx.settings();
            let emails: Vec<(String, String)> =
                sqlx::query_as("SELECT username, email FROM users WHERE uid = ANY($1)")
                    .bind(&uids)
                    .fetch_all(&ctx.app.db)
                    .await?;
            for (name, email) in emails {
                crate::mail::queue(&ctx.app, &email, &format!("Your account at {} has been activated", s.get("bbname")), &format!("{name},\n\nYour account has been activated by an administrator. You can now log in:\n{}/member/login\n", s.get("bburl").trim_end_matches('/'))).await;
            }
        }
        "delete" => {
            // Only genuinely awaiting accounts, never the admin's own or another staff account
            // (an extra group can make a pending account staff).
            let pending: Vec<User> = sqlx::query_as(&format!(
                "SELECT {} FROM users WHERE uid = ANY($1) AND usergroup = 5 AND uid <> $2 AND NOT is_system",
                crate::models::USER_COLUMNS
            ))
            .bind(&uids)
            .bind(ctx.uid())
            .fetch_all(&ctx.app.db)
            .await?;
            for u in pending.iter().filter(|u| !has_staff_role(&ctx, u)) {
                crate::routes::usercp::delete_user(&ctx.app, u.uid, true).await?;
            }
        }
        _ => {}
    }
    crate::admin::log(
        &ctx,
        "users",
        "Processed awaiting activation",
        serde_json::json!({"uids": uids}),
    )
    .await;
    Ok(ctx.redirect(
        "/admin/users/awaiting",
        "The selected users have been processed.",
    ))
}

pub async fn merge_form(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "users");
    crate::admin::page(
        &ctx,
        "admin/user_merge.html",
        "users",
        "Merge Users",
        minijinja::context! {},
    )
    .await
}

pub async fn merge_save(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "users");
    let find = |name: String| {
        let db = ctx.app.db.clone();
        async move {
            sqlx::query_as::<_, (i32, String)>(
                "SELECT uid, username FROM users WHERE lower(username) = lower($1)",
            )
            .bind(name.trim())
            .fetch_optional(&db)
            .await
        }
    };
    let (src, src_name) = find(s(f.fields.get("source")))
        .await?
        .ok_or_else(|| AppError::user("Source user not found."))?;
    let (dst, dst_name) = find(s(f.fields.get("destination")))
        .await?
        .ok_or_else(|| AppError::user("Destination user not found."))?;
    crate::system::guard(&ctx.cache, src, "merged")?;
    crate::system::guard(&ctx.cache, dst, "merged into")?;
    if src == dst || src == ctx.uid() {
        return Err(AppError::user(
            "Choose two different users (and not yourself as the source).",
        ));
    }
    // The source account is deleted, so it needs the same protection as a plain delete.
    guard_target(&ctx, &load(&ctx, src).await?, true).await?;
    let mut tx = ctx.app.db.begin().await?;
    for sql in [
        "UPDATE posts SET uid = $2, username = $3 WHERE uid = $1",
        "UPDATE threads SET uid = $2, username = $3 WHERE uid = $1",
        "UPDATE threads SET lastposteruid = $2, lastposter = $3 WHERE lastposteruid = $1",
        "UPDATE forums SET lastposteruid = $2, lastposter = $3 WHERE lastposteruid = $1",
    ] {
        sqlx::query(sql)
            .bind(src)
            .bind(dst)
            .bind(&dst_name)
            .execute(&mut *tx)
            .await?;
    }
    for sql in [
        "UPDATE privatemessages SET uid = $2 WHERE uid = $1",
        "UPDATE privatemessages SET fromid = $2 WHERE fromid = $1",
        "UPDATE privatemessages SET toid = $2 WHERE toid = $1",
        "UPDATE reputation SET uid = $2 WHERE uid = $1",
        "UPDATE reputation SET adduid = $2 WHERE adduid = $1",
        "UPDATE attachments SET uid = $2 WHERE uid = $1",
        "UPDATE warnings SET uid = $2 WHERE uid = $1",
        "UPDATE events SET uid = $2 WHERE uid = $1",
        "UPDATE pollvotes SET uid = $2 WHERE uid = $1",
        "UPDATE system_authorship SET actor = $2 WHERE actor = $1",
        "DELETE FROM threadsubscriptions WHERE uid = $1 AND tid IN (SELECT tid FROM threadsubscriptions WHERE uid = $2)",
        "UPDATE threadsubscriptions SET uid = $2 WHERE uid = $1",
        "DELETE FROM reactions WHERE uid = $1 AND (pid, kind) IN (SELECT pid, kind FROM reactions WHERE uid = $2)",
        "UPDATE reactions SET uid = $2 WHERE uid = $1",
    ] {
        sqlx::query(sql)
            .bind(src)
            .bind(dst)
            .execute(&mut *tx)
            .await?;
    }
    // Recount in the same transaction: the moved content and the new totals land together.
    sqlx::query(
        "UPDATE users SET postnum = (SELECT COUNT(*) FROM posts p JOIN forums f ON f.fid = p.fid WHERE p.uid = $1 AND p.visible = 1 AND f.usepostcounts),
            threadnum = (SELECT COUNT(*) FROM threads WHERE uid = $1 AND visible = 1), reputation = (SELECT COALESCE(SUM(reputation), 0) FROM reputation WHERE uid = $1),
            totalpms = (SELECT COUNT(*) FROM privatemessages WHERE uid = $1), unreadpms = (SELECT COUNT(*) FROM privatemessages WHERE uid = $1 AND status = 0 AND folder NOT IN (2,3))
         WHERE uid = $1",
    )
    .bind(dst)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    // `delete_user` runs its own transactions, so it can only follow the commit. If it fails,
    // the content has already moved; say so rather than leaving the admin guessing.
    if let Err(e) = crate::routes::usercp::delete_user(&ctx.app, src, false).await {
        tracing::error!(
            "merge: moved {src_name} into {dst_name} but could not delete the source: {e}"
        );
        return Err(AppError::user(format!(
            "{src_name}'s content was moved to {dst_name}, but the old account could not be deleted. Delete it from its member page."
        )));
    }
    crate::admin::log(
        &ctx,
        "users",
        "Merged users",
        serde_json::json!({"source": src_name, "destination": dst_name}),
    )
    .await;
    Ok(ctx.redirect(
        &format!("/admin/users/{dst}"),
        &format!("{src_name} has been merged into {dst_name}."),
    ))
}

pub async fn adminperms(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "adminperms");
    let admin_groups: Vec<i32> = ctx
        .cache
        .groups
        .values()
        .filter(|g| g.perms.0.cancp)
        .map(|g| g.gid)
        .collect();
    let rows: Vec<(i32, String, Option<serde_json::Value>)> = sqlx::query_as(
        "SELECT u.uid, u.username, a.permissions FROM users u LEFT JOIN adminoptions a ON a.uid = u.uid WHERE u.usergroup = ANY($1) OR u.additionalgroups && $1 ORDER BY lower(u.username)",
    )
    .bind(&admin_groups)
    .fetch_all(&ctx.app.db)
    .await?;
    let list: Vec<_> = rows
        .into_iter()
        .map(|(uid, n, p)| minijinja::context! { uid => uid, username => n, restricted => p.as_ref().and_then(|x| x.as_object()).map(|o| o.values().any(|v| v == &serde_json::Value::Bool(false))).unwrap_or(false) })
        .collect();
    crate::admin::page(
        &ctx,
        "admin/adminperms.html",
        "users",
        "Admin Permissions",
        minijinja::context! { list => list },
    )
    .await
}

pub async fn adminperms_edit(ctx: Ctx, Path(uid): Path<i32>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "adminperms");
    let user = load(&ctx, uid).await?;
    let p: serde_json::Value =
        sqlx::query_scalar("SELECT permissions FROM adminoptions WHERE uid = $1")
            .bind(uid)
            .fetch_optional(&ctx.app.db)
            .await?
            .unwrap_or_else(|| serde_json::json!({}));
    let items: Vec<_> = crate::admin::MODULES.iter().map(|(k, t)| minijinja::context! { key => k, title => t, allowed => p.get(*k).and_then(|v| v.as_bool()).unwrap_or(true) }).collect();
    crate::admin::page(
        &ctx,
        "admin/adminperms_edit.html",
        "users",
        &format!("Admin Permissions: {}", user.username),
        minijinja::context! { user => &user, items => items },
    )
    .await
}

pub async fn adminperms_save(
    ctx: Ctx,
    Path(uid): Path<i32>,
    CsrfForm(f): CsrfForm<AnyForm>,
) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "adminperms");
    if uid == ctx.uid() {
        return Err(AppError::user(
            "You cannot restrict your own admin permissions.",
        ));
    }
    // Only an unrestricted admin may change another's permissions. That also guarantees the
    // change can never leave the board without an unrestricted administrator: the actor is one.
    if !is_full_admin(&ctx).await? {
        return Err(AppError::user(
            "Only an administrator without Admin Permissions restrictions can change admin permissions.",
        ));
    }
    let target = load(&ctx, uid).await?;
    if !has_cancp(&ctx, &target) {
        return Err(AppError::user("That member is not an administrator."));
    }
    let mut p = serde_json::json!({});
    for (k, _) in crate::admin::MODULES {
        p[*k] = serde_json::Value::Bool(b(f.fields.get(*k)));
    }
    sqlx::query("INSERT INTO adminoptions (uid, permissions) VALUES ($1, $2) ON CONFLICT (uid) DO UPDATE SET permissions = $2").bind(uid).bind(&p).execute(&ctx.app.db).await?;
    crate::admin::log(
        &ctx,
        "adminperms",
        "Updated admin permissions",
        serde_json::json!({"uid": uid}),
    )
    .await;
    Ok(ctx.redirect("/admin/adminperms", "Admin permissions saved."))
}

pub async fn activity(
    ctx: Ctx,
    Path(uid): Path<i32>,
    Query(q): Query<crate::routes::usercp::ActivityQuery>,
) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "users");
    let user = load(&ctx, uid).await?;
    // Same whitelist as the member's own activity page: `kind` goes into a link.
    let kind = match q.kind.as_str() {
        "security" | "account" | "staff" => q.kind.as_str(),
        _ => "",
    };
    let base = format!("/admin/users/{uid}/activity?kind={kind}&page={{page}}");
    let (events, pagination) =
        crate::routes::usercp::audit_rows(&ctx, uid, kind, q.page, &base, false).await?;
    crate::admin::page(
        &ctx,
        "admin/user_activity.html",
        "users",
        &format!("Account activity: {}", user.username),
        minijinja::context! { user => &user, events => events, pagination => pagination, kind => kind },
    )
    .await
}
