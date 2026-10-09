//! User Control Panel.

use crate::auth;
use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::models::{ProfileField, User};
use crate::templates::url_thread;
use crate::util::{self, now};
use axum::extract::{Multipart, Query};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use std::collections::HashMap;

pub fn router() -> Router<crate::app::App> {
    Router::new()
        .route("/usercp", get(home))
        .route("/usercp/profile", get(profile_form).post(profile_save))
        .route(
            "/usercp/signature",
            get(signature_form).post(signature_save),
        )
        .route(
            "/usercp/avatar",
            get(avatar_form).post(axum::handler::Handler::layer(
                avatar_save,
                crate::routes::upload_limit(),
            )),
        )
        .route("/usercp/email", get(email_form).post(email_save))
        .route("/usercp/password", get(password_form).post(password_save))
        .route("/usercp/username", get(username_form).post(username_save))
        .route("/usercp/options", get(options_form).post(options_save))
        .route(
            "/usercp/subscriptions",
            get(subscriptions).post(subscriptions_action),
        )
        .route(
            "/usercp/forumsubscriptions",
            get(forum_subscriptions).post(forum_subscriptions_action),
        )
        .route("/usercp/drafts", get(drafts).post(drafts_delete))
        .route(
            "/usercp/attachments",
            get(attachments).post(attachments_delete),
        )
        .route("/usercp/lists", get(lists))
        .route("/usercp/lists/add", post(lists_add))
        .route("/usercp/notepad", get(notepad).post(notepad_save))
        .route("/usercp/badges", get(badges).post(badges_save))
        .route("/usercp/alerts", get(alerts))
        .route("/usercp/alerts/read", post(alerts_read))
        .route("/usercp/alerts/count", get(alerts_count))
        .route("/usercp/security", get(security))
        .route("/usercp/activity", get(activity))
        .route("/usercp/security/2fa", post(twofa_save))
        .route("/usercp/security/revoke", post(revoke_login))
        .route(
            "/usercp/usergroups",
            get(usergroups).post(usergroups_action),
        )
        .route("/usercp/delete", get(delete_form).post(delete_account))
        .route("/usercp/export", get(export))
}

/// Throttle password re-entry on sensitive User CP forms, so a hijacked session can't be used
/// to guess the account password.
pub(crate) async fn reauth_throttle(ctx: &Ctx, uid: i32) -> AppResult<()> {
    if ctx.app.throttle(&format!("reauth:{uid}"), 10, 600).await {
        Ok(())
    } else {
        Err(AppError::RateLimited)
    }
}

async fn require_ucp(ctx: &Ctx) -> AppResult<User> {
    let u = ctx.require_login()?.clone();
    if !ctx.perms.canusercp {
        return Err(AppError::no_perm());
    }
    Ok(u)
}

async fn page(
    ctx: &Ctx,
    name: &str,
    active: &str,
    title: &str,
    extra: minijinja::Value,
) -> AppResult<Response> {
    let base = minijinja::context! { title => title, ucp_active => active, breadcrumb => vec![("User Control Panel".to_string(), "/usercp".to_string())] };
    ctx.render(name, minijinja::value::merge_maps([base, extra]))
        .await
}

pub async fn home(ctx: Ctx) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let threads: Vec<crate::models::Thread> = sqlx::query_as(
        &format!("SELECT {} FROM threadsubscriptions s JOIN threads t ON t.tid = s.tid WHERE s.uid = $1 AND t.visible = 1 ORDER BY t.lastpost DESC LIMIT 10", crate::models::THREAD_COLUMNS_T.as_str()),
    )
    .bind(me.uid)
    .fetch_all(&ctx.app.db)
    .await?;
    let threads: Vec<_> = threads
        .into_iter()
        .filter(|t| ctx.access().can_read_thread(t.fid, t.uid, ctx.uid()))
        .collect();
    let rows = crate::routes::forumdisplay::thread_rows(&ctx, threads).await?;
    let latest: Vec<crate::models::Thread> = sqlx::query_as(&format!(
        "SELECT {} FROM threads WHERE uid = $1 AND visible = 1 ORDER BY lastpost DESC LIMIT 5",
        crate::models::THREAD_COLUMNS
    ))
    .bind(me.uid)
    .fetch_all(&ctx.app.db)
    .await?;
    let latest = crate::routes::forumdisplay::thread_rows(&ctx, latest).await?;
    let (checks, attention) = account_checks(&ctx, &me).await?;
    let restricted: Vec<String> = crate::member_file::restrictions(&me)
        .into_iter()
        .map(|r| {
            let until = if r.until > 0 {
                format!("until {}", ctx.fmt_date(r.until, "date"))
            } else {
                "until a moderator lifts it".to_string()
            };
            match r.kind {
                "moderate" => format!("A moderator checks your posts before they appear, {until}."),
                "posting" => format!("You can't post {until}."),
                _ => format!("Your signature is hidden {until}."),
            }
        })
        .collect();
    page(
        &ctx,
        "usercp/home.html",
        "home",
        "User Control Panel",
        minijinja::context! { user => &me, subscribed => rows, latest => latest, checks => checks, attention => attention, restricted => restricted },
    )
    .await
}

/// The "Account check" rows on the overview, and how many of them want attention.
async fn account_checks(
    ctx: &Ctx,
    me: &User,
) -> AppResult<(Vec<crate::member_file::Signal>, usize)> {
    use crate::member_file::{Level, Signal};
    let db = &ctx.app.db;
    let mut v = vec![];
    let unconfirmed: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM awaitingactivation WHERE uid = $1 AND type = 'r')",
    )
    .bind(me.uid)
    .fetch_one(db)
    .await?;
    v.push(if unconfirmed {
        Signal {
            group: "self",
            level: Level::Orange,
            text: "Confirm your email address".into(),
            detail: format!("We sent a link to {}.", me.email),
            link: Some("/usercp/email".into()),
        }
    } else {
        Signal {
            group: "self",
            level: Level::Green,
            text: "Email confirmed".into(),
            detail: me.email.clone(),
            link: Some("/usercp/email".into()),
        }
    });
    let passkeys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM passkeys WHERE uid = $1")
        .bind(me.uid)
        .fetch_one(db)
        .await?;
    let has_totp = !me.totp_secret.is_empty();
    v.push(if has_totp || passkeys > 0 {
        let text = match (has_totp, passkeys) {
            (true, 0) => "Two-factor on".to_string(),
            (true, n) => format!("Two-factor on, {n} {}", if n == 1 { "passkey" } else { "passkeys" }),
            (false, n) => format!("{n} {}", if n == 1 { "passkey" } else { "passkeys" }),
        };
        Signal { group: "self", level: Level::Green, text, detail: String::new(), link: Some("/usercp/security".into()) }
    } else {
        Signal { group: "self", level: Level::Orange, text: "No passkey or two-factor yet".into(), detail: "A passkey lets you sign in with your fingerprint or face, and stops a stolen password from working.".into(), link: Some("/usercp/security".into()) }
    });
    if ctx.settings().bool("enablewarningsystem") && me.warningpoints > 0 {
        let pct =
            crate::member_file::warn_pct(me.warningpoints, ctx.settings().int("maxwarningpoints"));
        let next: Option<(String, i64)> = sqlx::query_as(
            "SELECT title, expires FROM warnings WHERE uid = $1 AND expired = FALSE AND daterevoked = 0 AND expires > 0 ORDER BY expires LIMIT 1",
        )
        .bind(me.uid)
        .fetch_optional(db)
        .await?;
        let detail = next
            .map(|(t, e)| format!("\u{201c}{t}\u{201d} expires {}", ctx.fmt_date(e, "date")))
            .unwrap_or_default();
        v.push(Signal {
            group: "self",
            level: Level::Orange,
            text: format!("Warning level {pct}%"),
            detail,
            link: Some(format!("/warnings/{}", me.uid)),
        });
    }
    let uas: Vec<String> = sqlx::query_scalar(
        "SELECT useragent FROM logins WHERE uid = $1 AND expires > $2 ORDER BY lastused DESC",
    )
    .bind(me.uid)
    .bind(crate::util::now())
    .fetch_all(db)
    .await?;
    if !uas.is_empty() {
        let mut labels: Vec<String> = vec![];
        for ua in &uas {
            let l = crate::audit::device_label(ua);
            if !labels.contains(&l) {
                labels.push(l);
            }
        }
        v.push(Signal {
            group: "self",
            level: Level::Grey,
            text: format!(
                "Signed in on {} {}",
                uas.len(),
                if uas.len() == 1 { "device" } else { "devices" }
            ),
            detail: labels.join(" \u{b7} "),
            link: Some("/usercp/security".into()),
        });
    }
    let attention = v
        .iter()
        .filter(|s| matches!(s.level, Level::Orange | Level::Red))
        .count();
    Ok((v, attention))
}

// ---------------------------------------------------------------- profile

pub fn validate_profile_field(f: &ProfileField, v: &str) -> Result<String, String> {
    let v = v.trim().to_string();
    if f.required && v.is_empty() {
        return Err(format!("{} is required.", f.name));
    }
    if f.maxlength > 0 && v.chars().count() > f.maxlength as usize {
        return Err(format!(
            "{} must be at most {} characters.",
            f.name, f.maxlength
        ));
    }
    if !v.is_empty()
        && matches!(
            f.kind.as_str(),
            "select" | "radio" | "multiselect" | "checkbox"
        )
    {
        let opts: Vec<&str> = f.options.lines().map(|l| l.trim()).collect();
        if !v.lines().all(|x| opts.contains(&x.trim())) {
            return Err(format!("Invalid choice for {}.", f.name));
        }
    }
    if !v.is_empty()
        && !f.regex.is_empty()
        && let Ok(re) = regex::Regex::new(&f.regex)
        && !re.is_match(&v)
    {
        return Err(format!("{} is not in the correct format.", f.name));
    }
    Ok(v)
}

fn field_editable(ctx: &Ctx, f: &ProfileField) -> bool {
    f.editableby.is_empty() || f.editableby.iter().any(|g| ctx.groups.contains(g))
}

async fn field_values(ctx: &Ctx, uid: i32) -> AppResult<HashMap<String, String>> {
    Ok(
        sqlx::query_as::<_, (i32, String)>("SELECT fid, value FROM userfields WHERE uid = $1")
            .bind(uid)
            .fetch_all(&ctx.app.db)
            .await?
            .into_iter()
            .map(|(f, v)| (f.to_string(), v))
            .collect(),
    )
}

pub async fn profile_form(ctx: Ctx) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let values = field_values(&ctx, me.uid).await?;
    let fields: Vec<&ProfileField> = ctx
        .cache
        .profilefields
        .iter()
        .filter(|f| field_editable(&ctx, f))
        .collect();
    let bd: Vec<String> = me.birthday.split('-').map(|s| s.to_string()).collect();
    page(
        &ctx,
        "usercp/profile.html",
        "profile",
        "Edit Profile",
        minijinja::context! { user => &me, fields => fields, values => values, errors => Vec::<String>::new(),
            bday => bd.first().cloned().unwrap_or_default(), bmonth => bd.get(1).cloned().unwrap_or_default(), byear => bd.get(2).cloned().unwrap_or_default(),
            can_title => ctx.perms.cancustomtitle, can_website => ctx.perms.canchangewebsite, allowaway => ctx.settings().bool("allowaway") },
    )
    .await
}

#[derive(Deserialize)]
pub struct ProfileForm {
    #[serde(default, deserialize_with = "de::string")]
    pub website: String,
    #[serde(default, deserialize_with = "de::i32")]
    pub bday: i32,
    #[serde(default, deserialize_with = "de::i32")]
    pub bmonth: i32,
    #[serde(default, deserialize_with = "de::i32")]
    pub byear: i32,
    #[serde(default, deserialize_with = "de::string")]
    pub birthdayprivacy: String,
    #[serde(default, deserialize_with = "de::string")]
    pub usertitle: String,
    #[serde(default, deserialize_with = "de::bool")]
    pub away: bool,
    #[serde(default, deserialize_with = "de::string")]
    pub awayreason: String,
    #[serde(default, deserialize_with = "de::string")]
    pub returndate: String,
    #[serde(default, flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

pub fn collect_profile_fields(
    ctx: &Ctx,
    extra: &HashMap<String, serde_json::Value>,
    errors: &mut Vec<String>,
    only_editable: bool,
) -> Vec<(i32, String)> {
    let mut out = vec![];
    for pf in ctx.cache.profilefields.iter() {
        if only_editable && !field_editable(ctx, pf) {
            continue;
        }
        let key = format!("profile_fields[{}]", pf.fid);
        let val = match extra.get(&key) {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        match validate_profile_field(pf, &val) {
            Ok(v) => out.push((pf.fid, v)),
            Err(e) => errors.push(e),
        }
    }
    out
}

pub async fn save_profile_fields(
    db: &sqlx::PgPool,
    uid: i32,
    vals: &[(i32, String)],
) -> AppResult<()> {
    for (fid, v) in vals {
        sqlx::query("INSERT INTO userfields (uid, fid, value) VALUES ($1, $2, $3) ON CONFLICT (uid, fid) DO UPDATE SET value = $3")
            .bind(uid)
            .bind(fid)
            .bind(v)
            .execute(db)
            .await?;
    }
    Ok(())
}

pub async fn profile_save(ctx: Ctx, CsrfForm(f): CsrfForm<ProfileForm>) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let mut errors = vec![];
    let website = f.website.trim().to_string();
    if !website.is_empty() && !(website.starts_with("http://") || website.starts_with("https://")) {
        errors.push("Your website must start with http:// or https://".to_string());
    }
    let birthday = if f.bday > 0 && f.bmonth > 0 {
        if f.bday > 31
            || f.bmonth > 12
            || (f.byear != 0 && (f.byear < 1900 || f.byear as i64 > 2100))
        {
            errors.push("Invalid birthday.".into());
            String::new()
        } else if f.byear > 0 {
            format!("{}-{}-{}", f.bday, f.bmonth, f.byear)
        } else {
            format!("{}-{}-", f.bday, f.bmonth)
        }
    } else {
        String::new()
    };
    let privacy = if matches!(f.birthdayprivacy.as_str(), "all" | "none" | "age") {
        f.birthdayprivacy.clone()
    } else {
        "all".into()
    };
    let title_max = ctx.settings().int("customtitlemaxlength").max(1) as usize;
    let usertitle = if ctx.perms.cancustomtitle {
        f.usertitle.trim().chars().take(title_max).collect()
    } else {
        me.usertitle.clone()
    };
    let vals = collect_profile_fields(&ctx, &f.extra, &mut errors, true);
    if !errors.is_empty() {
        let values = field_values(&ctx, me.uid).await?;
        let fields: Vec<&ProfileField> = ctx
            .cache
            .profilefields
            .iter()
            .filter(|x| field_editable(&ctx, x))
            .collect();
        return page(&ctx, "usercp/profile.html", "profile", "Edit Profile", minijinja::context! { user => &me, fields => fields, values => values, errors => errors, can_title => ctx.perms.cancustomtitle, can_website => ctx.perms.canchangewebsite, allowaway => ctx.settings().bool("allowaway") }).await;
    }
    let away = f.away && ctx.settings().bool("allowaway");
    sqlx::query(
        "UPDATE users SET website = $2, birthday = $3, birthdayprivacy = $4, usertitle = $5, away = $6, awayreason = $7, returndate = $8,
            awaydate = CASE WHEN $6 AND NOT away THEN $9 ELSE awaydate END WHERE uid = $1",
    )
    .bind(me.uid)
    .bind(if ctx.perms.canchangewebsite { website } else { me.website.clone() })
    .bind(birthday)
    .bind(privacy)
    .bind(usertitle)
    .bind(away)
    .bind(f.awayreason.chars().take(200).collect::<String>())
    .bind(f.returndate.chars().take(20).collect::<String>())
    .bind(now())
    .execute(&ctx.app.db)
    .await?;
    save_profile_fields(&ctx.app.db, me.uid, &vals).await?;
    crate::audit::log(&ctx, ctx.uid(), "profile_updated", serde_json::Value::Null).await;
    Ok(ctx.redirect("/usercp/profile", "Your profile has been updated."))
}

// ---------------------------------------------------------------- signature

pub async fn signature_form(ctx: Ctx) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let preview = crate::render::signature_html(&ctx, me.uid, &me.signature);
    page(&ctx, "usercp/signature.html", "signature", "Edit Signature", minijinja::context! { signature => &me.signature, preview => preview, errors => Vec::<String>::new(), allowed => sig_allowed(&ctx, &me) }).await
}

fn sig_allowed(ctx: &Ctx, me: &User) -> bool {
    ctx.perms.canusesig
        && me.postnum >= ctx.perms.canusesigxposts
        && !(me.suspendsignature && (me.suspendsigtime == 0 || me.suspendsigtime > now()))
}

#[derive(Deserialize)]
pub struct SigForm {
    #[serde(default, deserialize_with = "de::string")]
    pub signature: String,
    #[serde(default, deserialize_with = "de::string")]
    pub preview: String,
}

pub async fn signature_save(ctx: Ctx, CsrfForm(f): CsrfForm<SigForm>) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    if !sig_allowed(&ctx, &me) {
        return Err(AppError::user("You are not allowed to use a signature."));
    }
    let s = ctx.settings();
    let mut errors = vec![];
    let sig = f.signature.trim().to_string();
    let len = s.int("siglength") as usize;
    if len > 0 && sig.chars().count() > len {
        errors.push(format!(
            "Your signature is too long (maximum {len} characters)."
        ));
    }
    let lines = s.int("maxsiglines") as usize;
    if lines > 0 && sig.lines().count() > lines {
        errors.push(format!(
            "Your signature has too many lines (maximum {lines})."
        ));
    }
    if !f.preview.is_empty() || !errors.is_empty() {
        let preview = crate::render::signature_html(&ctx, me.uid, &sig);
        return page(&ctx, "usercp/signature.html", "signature", "Edit Signature", minijinja::context! { signature => sig, preview => preview, errors => errors, allowed => true }).await;
    }
    sqlx::query("UPDATE users SET signature = $2 WHERE uid = $1")
        .bind(me.uid)
        .bind(&sig)
        .execute(&ctx.app.db)
        .await?;
    crate::audit::log(
        &ctx,
        ctx.uid(),
        "signature_changed",
        serde_json::Value::Null,
    )
    .await;
    Ok(ctx.redirect("/usercp/signature", "Your signature has been updated."))
}

// ---------------------------------------------------------------- avatar

pub async fn avatar_form(ctx: Ctx) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let s = ctx.settings();
    page(
        &ctx,
        "usercp/avatar.html",
        "avatar",
        "Change Avatar",
        minijinja::context! { user => &me, can_upload => ctx.perms.canuploadavatars, remote => s.bool("allowremoteavatars"), gravatar => s.bool("allowgravatar"), maxsize => s.int("avatarsize"), maxdims => s.get("maxavatardims") },
    )
    .await
}

fn max_dims(ctx: &Ctx) -> (u32, u32) {
    ctx.settings()
        .get("maxavatardims")
        .split_once('x')
        .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
        .unwrap_or((100, 100))
}

pub async fn avatar_save(ctx: Ctx, mut mp: Multipart) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let mut fields: HashMap<String, String> = HashMap::new();
    let mut file: Option<crate::infra::uploads::Spooled> = None;
    let max_kb = ctx.settings().int("avatarsize").max(1) as u64;
    while let Some(field) = mp
        .next_field()
        .await
        .map_err(|e| AppError::user(format!("Upload failed: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            // Don't spool anything to disk for a forged request: the form puts the token before
            // the file, so it has arrived by now (a header token works too).
            ctx.check_csrf(fields.get("my_post_key").map(String::as_str).unwrap_or(""))?;
            // Streamed to disk and cut off at the avatar size limit.
            match crate::infra::uploads::spool(&ctx.app.cfg.upload_dir, field, max_kb * 1024).await
            {
                Ok(f) => file = Some(f),
                Err(crate::infra::uploads::SpoolError::Empty) => {}
                Err(crate::infra::uploads::SpoolError::TooLarge { .. }) => {
                    return Err(AppError::user(format!(
                        "The avatar is too large. Maximum size is {max_kb} KB."
                    )));
                }
                Err(e) => return Err(e.into()),
            }
        } else {
            fields.insert(name, field.text().await.unwrap_or_default());
        }
    }
    ctx.check_csrf(fields.get("my_post_key").map(String::as_str).unwrap_or(""))?;
    let action = fields.get("action").cloned().unwrap_or_default();
    let storage = ctx.app.storage.clone();
    let remove_old = |old: String| {
        let storage = storage.clone();
        async move {
            if let Some(name) = old.strip_prefix("/uploads/avatars/")
                && !name.contains('/')
                && !name.contains("..")
                && let Err(e) = storage.delete(&format!("avatars/{name}")).await
            {
                tracing::warn!("removing old avatar failed: {e:#}");
            }
        }
    };
    match action.as_str() {
        "remove" => {
            ctx.app.avatar_cache.invalidate(&me.uid);
            sqlx::query("UPDATE users SET avatar = '', avatartype = '', avatardimensions = '' WHERE uid = $1").bind(me.uid).execute(&ctx.app.db).await?;
            remove_old(me.avatar.clone()).await;
            Ok(ctx.redirect("/usercp/avatar", "Your avatar has been removed."))
        }
        "gravatar" => {
            if !ctx.settings().bool("allowgravatar") {
                return Err(AppError::no_perm());
            }
            let hash = util::sha256_hex(&me.email.trim().to_lowercase());
            let (w, _) = max_dims(&ctx);
            let url = format!("https://www.gravatar.com/avatar/{hash}?s={w}&d=identicon");
            ctx.app.avatar_cache.invalidate(&me.uid);
            sqlx::query("UPDATE users SET avatar = $2, avatartype = 'gravatar', avatardimensions = $3 WHERE uid = $1")
                .bind(me.uid)
                .bind(url)
                .bind(format!("{w}|{w}"))
                .execute(&ctx.app.db)
                .await?;
            remove_old(me.avatar.clone()).await;
            Ok(ctx.redirect("/usercp/avatar", "Your avatar now uses Gravatar."))
        }
        "remote" => {
            if !ctx.settings().bool("allowremoteavatars") {
                return Err(AppError::no_perm());
            }
            let url = fields
                .get("url")
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            if !url.starts_with("https://") && !url.starts_with("http://")
                || crate::parser::safe_url(&url).is_none()
            {
                return Err(AppError::user("Please enter a valid http(s) image URL."));
            }
            let (w, h) = max_dims(&ctx);
            ctx.app.avatar_cache.invalidate(&me.uid);
            sqlx::query("UPDATE users SET avatar = $2, avatartype = 'remote', avatardimensions = $3 WHERE uid = $1")
                .bind(me.uid)
                .bind(&url)
                .bind(format!("{w}|{h}"))
                .execute(&ctx.app.db)
                .await?;
            remove_old(me.avatar.clone()).await;
            Ok(ctx.redirect("/usercp/avatar", "Your avatar has been updated."))
        }
        _ => {
            if !ctx.perms.canuploadavatars {
                return Err(AppError::no_perm());
            }
            let upload = file.ok_or_else(|| AppError::user("Please choose an image to upload."))?;
            let (mw, mh) = max_dims(&ctx);
            let png = crate::infra::uploads::with_image(upload.path().to_path_buf(), move |img| {
                let img = if img.width() > mw || img.height() > mh {
                    img.resize(mw, mh, image::imageops::FilterType::Lanczos3)
                } else {
                    img
                };
                Ok((crate::infra::uploads::png(&img)?, img.width(), img.height()))
            })
            .await
            .map_err(AppError::User)?;
            drop(upload);
            let name = format!("avatar_{}_{}.png", me.uid, util::random_token(8));
            ctx.app
                .storage
                .put_bytes(&format!("avatars/{name}"), png.0.into())
                .await
                .map_err(AppError::Other)?;
            ctx.app.avatar_cache.invalidate(&me.uid);
            sqlx::query("UPDATE users SET avatar = $2, avatartype = 'upload', avatardimensions = $3 WHERE uid = $1")
                .bind(me.uid)
                .bind(format!("/uploads/avatars/{name}"))
                .bind(format!("{}|{}", png.1, png.2))
                .execute(&ctx.app.db)
                .await?;
            remove_old(me.avatar.clone()).await;
            crate::audit::log(&ctx, ctx.uid(), "avatar_changed", serde_json::Value::Null).await;
            Ok(ctx.redirect("/usercp/avatar", "Your avatar has been uploaded."))
        }
    }
}

// ---------------------------------------------------------------- email / password / username

pub async fn email_form(ctx: Ctx) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    page(
        &ctx,
        "usercp/email.html",
        "email",
        "Change Email",
        minijinja::context! { email => &me.email, errors => Vec::<String>::new() },
    )
    .await
}

#[derive(Deserialize)]
pub struct EmailChange {
    #[serde(default, deserialize_with = "de::string")]
    pub password: String,
    #[serde(default, deserialize_with = "de::string")]
    pub email: String,
    #[serde(default, deserialize_with = "de::string")]
    pub email2: String,
}

pub async fn email_save(ctx: Ctx, CsrfForm(f): CsrfForm<EmailChange>) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let email = f.email.trim().to_string();
    let mut errors = vec![];
    reauth_throttle(&ctx, me.uid).await?;
    if !auth::verify_password(&f.password, &me.password).await {
        errors.push("The password you entered is incorrect.".to_string());
    }
    if !util::valid_email(&email) {
        errors.push("The email address you entered is invalid.".into());
    } else if email != f.email2.trim() {
        errors.push("The email addresses do not match.".into());
    } else if auth::is_filtered(&ctx.app, 3, &email).await? {
        errors.push("That email address is banned.".into());
    } else if !ctx.settings().bool("allowmultipleemails") {
        let taken: Option<i32> = sqlx::query_scalar(
            "SELECT uid FROM users WHERE lower(email) = lower($1) AND uid <> $2 LIMIT 1",
        )
        .bind(&email)
        .bind(me.uid)
        .fetch_optional(&ctx.app.db)
        .await?;
        if taken.is_some() {
            errors.push("That email address is already in use.".into());
        }
    }
    if !errors.is_empty() {
        return page(
            &ctx,
            "usercp/email.html",
            "email",
            "Change Email",
            minijinja::context! { email => &me.email, errors => errors },
        )
        .await;
    }
    let s = ctx.settings();
    if matches!(s.get("regtype"), "verify" | "both") && !ctx.is_admin() {
        crate::usecase::accounts::request_email_change(
            &ctx.app,
            &crate::audit::Actor::from_ctx(&ctx),
            me.uid,
            &me.username,
            &email,
        )
        .await?;
        return Ok(ctx.redirect(
            "/usercp/email",
            "A confirmation link has been sent to your new email address.",
        ));
    }
    sqlx::query("UPDATE users SET email = $2 WHERE uid = $1")
        .bind(me.uid)
        .bind(&email)
        .execute(&ctx.app.db)
        .await?;
    crate::audit::log(&ctx, ctx.uid(), "email_changed", serde_json::Value::Null).await;
    Ok(ctx.redirect("/usercp/email", "Your email address has been changed."))
}

pub async fn password_form(ctx: Ctx) -> AppResult<Response> {
    require_ucp(&ctx).await?;
    page(
        &ctx,
        "usercp/password.html",
        "password",
        "Change Password",
        minijinja::context! { errors => Vec::<String>::new() },
    )
    .await
}

#[derive(Deserialize)]
pub struct PwChange {
    #[serde(default, deserialize_with = "de::string")]
    pub oldpassword: String,
    #[serde(default, deserialize_with = "de::string")]
    pub password: String,
    #[serde(default, deserialize_with = "de::string")]
    pub password2: String,
}

pub async fn password_save(ctx: Ctx, CsrfForm(f): CsrfForm<PwChange>) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let mut errors = vec![];
    reauth_throttle(&ctx, me.uid).await?;
    if !auth::verify_password(&f.oldpassword, &me.password).await {
        errors.push("Your current password is incorrect.".to_string());
    }
    if let Some(e) = auth::password_strength_error(&ctx, &f.password, &me.username) {
        errors.push(e);
    } else if f.password != f.password2 {
        errors.push("The new passwords do not match.".into());
    }
    if !errors.is_empty() {
        return page(
            &ctx,
            "usercp/password.html",
            "password",
            "Change Password",
            minijinja::context! { errors => errors },
        )
        .await;
    }
    let h = auth::hash_password(&f.password).await?;
    sqlx::query("UPDATE users SET password = $2 WHERE uid = $1")
        .bind(me.uid)
        .bind(h)
        .execute(&ctx.app.db)
        .await?;
    auth::destroy_all_logins(&ctx.app, me.uid, ctx.token_hash.as_deref()).await?;
    auth::rotate_login(&ctx).await?;
    crate::audit::log(&ctx, ctx.uid(), "password_changed", serde_json::Value::Null).await;
    Ok(ctx.redirect(
        "/usercp/password",
        "Your password has been changed. Other devices have been logged out.",
    ))
}

pub async fn username_form(ctx: Ctx) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    if !ctx.perms.canchangename {
        return Err(AppError::no_perm());
    }
    page(
        &ctx,
        "usercp/username.html",
        "username",
        "Change Username",
        minijinja::context! { username => &me.username, errors => Vec::<String>::new() },
    )
    .await
}

#[derive(Deserialize)]
pub struct NameChange {
    #[serde(default, deserialize_with = "de::string")]
    pub username: String,
    #[serde(default, deserialize_with = "de::string")]
    pub password: String,
}

/// Rename a user everywhere their name is denormalized.
pub async fn rename_user(app: &crate::app::App, uid: i32, old: &str, new: &str) -> AppResult<()> {
    let mut tx = app.db.begin().await?;
    // The unique index on lower(username) is what really guards against two people racing for a
    // name; the callers' "taken" lookups are only the friendly early answer.
    match sqlx::query("UPDATE users SET username = $2 WHERE uid = $1")
        .bind(uid)
        .bind(new)
        .execute(&mut *tx)
        .await
    {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(AppError::user("That username is already taken."));
        }
        Err(e) => return Err(e.into()),
    }
    sqlx::query("UPDATE posts SET username = $2 WHERE uid = $1")
        .bind(uid)
        .bind(new)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE threads SET username = $2 WHERE uid = $1")
        .bind(uid)
        .bind(new)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE threads SET lastposter = $2 WHERE lastposteruid = $1")
        .bind(uid)
        .bind(new)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE forums SET lastposter = $2 WHERE lastposteruid = $1")
        .bind(uid)
        .bind(new)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE counters SET lastusername = $2 WHERE lastuid = $1")
        .bind(uid)
        .bind(new)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let _ = old;
    Ok(())
}

pub async fn username_save(ctx: Ctx, CsrfForm(f): CsrfForm<NameChange>) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    if !ctx.perms.canchangename {
        return Err(AppError::no_perm());
    }
    let new = f.username.trim().to_string();
    let s = ctx.settings();
    let mut errors = vec![];
    reauth_throttle(&ctx, me.uid).await?;
    if !auth::verify_password(&f.password, &me.password).await {
        errors.push("Your password is incorrect.".to_string());
    }
    let (minl, maxl) = (
        s.int("minnamelength") as usize,
        s.int("maxnamelength") as usize,
    );
    if new.chars().count() < minl || new.chars().count() > maxl || !auth::valid_username_chars(&new)
    {
        errors.push("That username is not valid.".into());
    }
    let taken: Option<i32> =
        sqlx::query_scalar("SELECT uid FROM users WHERE lower(username) = lower($1) AND uid <> $2")
            .bind(&new)
            .bind(me.uid)
            .fetch_optional(&ctx.app.db)
            .await?;
    if taken.is_some() {
        errors.push("That username is already taken.".into());
    }
    if auth::is_filtered(&ctx.app, 2, &new).await? {
        errors.push("That username is banned.".into());
    }
    if !errors.is_empty() {
        return page(
            &ctx,
            "usercp/username.html",
            "username",
            "Change Username",
            minijinja::context! { username => &me.username, errors => errors },
        )
        .await;
    }
    rename_user(&ctx.app, me.uid, &me.username, &new).await?;
    crate::audit::log(&ctx, ctx.uid(), "username_changed", serde_json::Value::Null).await;
    Ok(ctx.redirect("/usercp", "Your username has been changed."))
}

// ---------------------------------------------------------------- options

pub async fn options_form(ctx: Ctx) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let s = ctx.settings();
    let tpp: Vec<i64> = s
        .get("usertppoptions")
        .split(',')
        .filter_map(|x| x.trim().parse().ok())
        .collect();
    let ppp: Vec<i64> = s
        .get("userppoptions")
        .split(',')
        .filter_map(|x| x.trim().parse().ok())
        .collect();
    page(
        &ctx,
        "usercp/options.html",
        "options",
        "Edit Options",
        minijinja::context! { user => &me, tpp_options => tpp, ppp_options => ppp, timezones => crate::routes::member::timezones(), can_invisible => ctx.perms.canbeinvisible },
    )
    .await
}

#[derive(Deserialize)]
pub struct OptionsForm {
    #[serde(default, deserialize_with = "de::bool")]
    pub invisible: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub allownotices: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub hideemail: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub receivepms: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub receivefrombuddy: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub pmnotice: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub pmnotify: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub showsigs: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub showavatars: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub showimages: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub showvideos: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub showquickreply: bool,
    #[serde(default, deserialize_with = "de::i32")]
    pub subscriptionmethod: i32,
    #[serde(default, deserialize_with = "de::i32")]
    pub tpp: i32,
    #[serde(default, deserialize_with = "de::i32")]
    pub ppp: i32,
    #[serde(default, deserialize_with = "de::string")]
    pub timezone: String,
    #[serde(default, deserialize_with = "de::string")]
    pub dateformat: String,
    #[serde(default, deserialize_with = "de::string")]
    pub timeformat: String,
    #[serde(default, deserialize_with = "de::string")]
    pub colormode: String,
}

pub async fn options_save(ctx: Ctx, CsrfForm(f): CsrfForm<OptionsForm>) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let tz = if f.timezone.parse::<chrono_tz::Tz>().is_ok() {
        f.timezone.clone()
    } else {
        String::new()
    };
    let fmt_ok = |s: &str| {
        s.len() <= 40
            && !s.contains(['<', '>'])
            && chrono::format::StrftimeItems::new(s)
                .all(|i| !matches!(i, chrono::format::Item::Error))
    };
    let dateformat = if fmt_ok(&f.dateformat) {
        f.dateformat.clone()
    } else {
        String::new()
    };
    let timeformat = if fmt_ok(&f.timeformat) {
        f.timeformat.clone()
    } else {
        String::new()
    };
    let colormode = if matches!(f.colormode.as_str(), "light" | "dark") {
        f.colormode.clone()
    } else {
        "auto".into()
    };
    sqlx::query(
        "UPDATE users SET invisible = $2, allownotices = $3, hideemail = $4, receivepms = $5, receivefrombuddy = $6, pmnotice = $7,
            pmnotify = $8, showsigs = $9, showavatars = $10, showimages = $11, showvideos = $12, showquickreply = $13,
            subscriptionmethod = $14, tpp = $15, ppp = $16, timezone = $17, dateformat = $18, timeformat = $19, colormode = $20
         WHERE uid = $1",
    )
    .bind(me.uid)
    .bind(f.invisible && ctx.perms.canbeinvisible)
    .bind(f.allownotices)
    .bind(f.hideemail)
    .bind(f.receivepms)
    .bind(f.receivefrombuddy)
    .bind(f.pmnotice)
    .bind(f.pmnotify)
    .bind(f.showsigs)
    .bind(f.showavatars)
    .bind(f.showimages)
    .bind(f.showvideos)
    .bind(f.showquickreply)
    .bind(f.subscriptionmethod.clamp(0, 3) as i16)
    .bind(f.tpp.clamp(0, 100) as i16)
    .bind(f.ppp.clamp(0, 100) as i16)
    .bind(tz)
    .bind(dateformat)
    .bind(timeformat)
    .bind(colormode)
    .execute(&ctx.app.db)
    .await?;
    crate::audit::log(&ctx, ctx.uid(), "options_changed", serde_json::Value::Null).await;
    Ok(ctx.redirect("/usercp/options", "Your options have been saved."))
}

// ---------------------------------------------------------------- subscriptions

pub async fn subscriptions(
    ctx: Ctx,
    Query(q): Query<crate::routes::forumdisplay::FdQuery>,
) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM threadsubscriptions WHERE uid = $1")
        .bind(me.uid)
        .fetch_one(&ctx.app.db)
        .await?;
    let per = 25;
    let pg = util::paginate(
        total,
        per,
        util::clamp_page(q.page),
        "/usercp/subscriptions?page={page}",
    );
    let rows: Vec<(crate::models::Thread, i16)> = {
        let threads: Vec<crate::models::Thread> = sqlx::query_as(
            &format!("SELECT {} FROM threadsubscriptions s JOIN threads t ON t.tid = s.tid WHERE s.uid = $1 ORDER BY t.lastpost DESC LIMIT $2 OFFSET $3", crate::models::THREAD_COLUMNS_T.as_str()),
        )
        .bind(me.uid)
        .bind(per)
        .bind((pg.page - 1) * per)
        .fetch_all(&ctx.app.db)
        .await?;
        // Only this page's threads, not every subscription the member has ever made.
        let tids: Vec<i32> = threads.iter().map(|t| t.tid).collect();
        let notif: HashMap<i32, i16> = sqlx::query_as::<_, (i32, i16)>(
            "SELECT tid, notification FROM threadsubscriptions WHERE uid = $1 AND tid = ANY($2)",
        )
        .bind(me.uid)
        .bind(&tids)
        .fetch_all(&ctx.app.db)
        .await?
        .into_iter()
        .collect();
        threads
            .into_iter()
            .map(|t| {
                let n = notif.get(&t.tid).copied().unwrap_or(0);
                (t, n)
            })
            .collect()
    };
    let notif: HashMap<i32, i16> = rows.iter().map(|(t, n)| (t.tid, *n)).collect();
    let subscribed: Vec<i32> = rows.iter().map(|(t, _)| t.tid).collect();
    let threads = crate::routes::forumdisplay::thread_rows(
        &ctx,
        rows.into_iter()
            .map(|r| r.0)
            .filter(|t| ctx.access().can_read_thread(t.fid, t.uid, ctx.uid()))
            .collect(),
    )
    .await?;
    // Threads the member can no longer see (moved, unapproved, deleted, permissions changed)
    // are listed without their details, so they can still be unsubscribed from.
    let unavailable: Vec<i32> = subscribed
        .into_iter()
        .filter(|tid| !threads.iter().any(|t| t.tid == *tid))
        .collect();
    page(&ctx, "usercp/subscriptions.html", "subscriptions", "Thread Subscriptions", minijinja::context! { threads => threads, unavailable => unavailable, notif => notif.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<HashMap<_, _>>(), pagination => pg }).await
}

#[derive(Deserialize)]
pub struct SubAction {
    #[serde(default, deserialize_with = "de::vec_i32")]
    pub tids: Vec<i32>,
    #[serde(default, deserialize_with = "de::string")]
    pub action: String,
}

pub async fn subscriptions_action(
    ctx: Ctx,
    CsrfForm(f): CsrfForm<SubAction>,
) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    match f.action.as_str() {
        "delete" => {
            sqlx::query("DELETE FROM threadsubscriptions WHERE uid = $1 AND tid = ANY($2)")
                .bind(me.uid)
                .bind(&f.tids)
                .execute(&ctx.app.db)
                .await?;
        }
        "deleteall" => {
            sqlx::query("DELETE FROM threadsubscriptions WHERE uid = $1")
                .bind(me.uid)
                .execute(&ctx.app.db)
                .await?;
        }
        a if a.starts_with("notify") => {
            let n: i16 = a.trim_start_matches("notify").parse().unwrap_or(0);
            sqlx::query(
                "UPDATE threadsubscriptions SET notification = $3 WHERE uid = $1 AND tid = ANY($2)",
            )
            .bind(me.uid)
            .bind(&f.tids)
            .bind(n.clamp(0, 2))
            .execute(&ctx.app.db)
            .await?;
        }
        _ => {}
    }
    Ok(ctx.redirect(
        "/usercp/subscriptions",
        "Your subscriptions have been updated.",
    ))
}

pub async fn forum_subscriptions(ctx: Ctx) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let fids: Vec<i32> = sqlx::query_scalar("SELECT fid FROM forumsubscriptions WHERE uid = $1")
        .bind(me.uid)
        .fetch_all(&ctx.app.db)
        .await?;
    let forums: Vec<_> = fids
        .iter()
        .filter_map(|f| ctx.cache.forum(*f))
        .map(|f| minijinja::context! { fid => f.fid, name => &f.name, url => crate::templates::url_forum(f.fid as i64, Some(&f.name)) })
        .collect();
    page(
        &ctx,
        "usercp/forumsubs.html",
        "forumsubscriptions",
        "Forum Subscriptions",
        minijinja::context! { forums => forums },
    )
    .await
}

#[derive(Deserialize)]
pub struct FidForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub fid: i32,
}

pub async fn forum_subscriptions_action(
    ctx: Ctx,
    CsrfForm(f): CsrfForm<FidForm>,
) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    sqlx::query("DELETE FROM forumsubscriptions WHERE uid = $1 AND fid = $2")
        .bind(me.uid)
        .bind(f.fid)
        .execute(&ctx.app.db)
        .await?;
    Ok(ctx.redirect("/usercp/forumsubscriptions", "Unsubscribed."))
}

// ---------------------------------------------------------------- drafts / attachments

pub async fn drafts(ctx: Ctx) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let rows: Vec<(i32, i32, i32, String, i64, Option<String>)> = sqlx::query_as(
        "SELECT d.did, d.fid, d.tid, d.subject, d.dateline, t.subject FROM drafts d LEFT JOIN threads t ON t.tid = d.tid WHERE d.uid = $1 ORDER BY d.dateline DESC",
    )
    .bind(me.uid)
    .fetch_all(&ctx.app.db)
    .await?;
    let list: Vec<_> = rows
        .into_iter()
        .map(|(did, fid, tid, subject, dl, tsub)| {
            let url = if tid > 0 { format!("/newreply/{tid}?did={did}") } else { format!("/newthread/{fid}?did={did}") };
            let context = if tid > 0 { format!("Reply to: {}", tsub.unwrap_or_default()) } else { format!("New thread in {}", ctx.cache.forum(fid).map(|f| f.name.clone()).unwrap_or_default()) };
            minijinja::context! { did => did, subject => if subject.is_empty() { "(no subject)".to_string() } else { subject }, dateline => dl, url => url, context => context }
        })
        .collect();
    page(
        &ctx,
        "usercp/drafts.html",
        "drafts",
        "Saved Drafts",
        minijinja::context! { drafts => list },
    )
    .await
}

#[derive(Deserialize)]
pub struct IdsForm {
    #[serde(default, deserialize_with = "de::vec_i32")]
    pub ids: Vec<i32>,
}

pub async fn drafts_delete(ctx: Ctx, CsrfForm(f): CsrfForm<IdsForm>) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    sqlx::query("DELETE FROM drafts WHERE uid = $1 AND did = ANY($2)")
        .bind(me.uid)
        .bind(&f.ids)
        .execute(&ctx.app.db)
        .await?;
    Ok(ctx.redirect("/usercp/drafts", "The selected drafts have been deleted."))
}

pub async fn attachments(ctx: Ctx) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let rows: Vec<(i32, String, i64, i32, i64, i32, Option<i32>, Option<String>)> = sqlx::query_as(
        "SELECT a.aid, a.filename, a.filesize, a.downloads, a.dateuploaded, a.pid, p.tid, t.subject FROM attachments a
         LEFT JOIN posts p ON p.pid = a.pid LEFT JOIN threads t ON t.tid = p.tid WHERE a.uid = $1 ORDER BY a.dateuploaded DESC LIMIT 500",
    )
    .bind(me.uid)
    .fetch_all(&ctx.app.db)
    .await?;
    let used: i64 = rows.iter().map(|r| r.2).sum();
    let quota = ctx.perms.attachquota as i64 * 1024;
    let list: Vec<_> = rows
        .into_iter()
        .map(|(aid, name, size, dl, date, pid, tid, subj)| minijinja::context! { aid => aid, filename => name, size => size, downloads => dl, dateline => date, pid => pid, tid => tid, subject => subj })
        .collect();
    page(&ctx, "usercp/attachments.html", "attachments", "Manage Attachments", minijinja::context! { list => list, used => used, quota => quota, percent => if quota > 0 { used * 100 / quota } else { 0 } }).await
}

pub async fn attachments_delete(ctx: Ctx, CsrfForm(f): CsrfForm<IdsForm>) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let mut uow = crate::usecase::Uow::begin(&ctx.app).await?;
    let files: Vec<(String, String, i32)> = sqlx::query_as(
        "DELETE FROM attachments WHERE uid = $1 AND aid = ANY($2) RETURNING attachname, thumbnail, pid",
    )
    .bind(me.uid)
    .bind(&f.ids)
    .fetch_all(uow.conn())
    .await?;
    sqlx::query("UPDATE posts SET parser_rev = -1 WHERE pid = ANY($1)")
        .bind(files.iter().map(|f| f.2).collect::<Vec<_>>())
        .execute(uow.conn())
        .await?;
    let paths: Vec<String> = files
        .into_iter()
        .flat_map(|(a, t, _)| [a, t])
        .filter(|p| !p.is_empty())
        .collect();
    if !paths.is_empty() {
        uow.job(crate::infra::outbox::Job::DeleteFiles { paths });
    }
    uow.commit(&ctx.app).await?;
    Ok(ctx.redirect(
        "/usercp/attachments",
        "The selected attachments have been deleted.",
    ))
}

// ---------------------------------------------------------------- buddy / ignore lists

pub async fn lists(ctx: Ctx) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let load = |ids: Vec<i32>| {
        let ctx = ctx.clone();
        async move {
            let rows: Vec<(i32, String, i32, i32)> = sqlx::query_as("SELECT uid, username, usergroup, displaygroup FROM users WHERE uid = ANY($1) ORDER BY lower(username)")
                .bind(&ids)
                .fetch_all(&ctx.app.db)
                .await?;
            Ok::<_, AppError>(rows.into_iter().map(|(u, n, g, d)| minijinja::context! { uid => u, username => &n, formatted => ctx.cache.format_name(&n, g, d) }).collect::<Vec<_>>())
        }
    };
    let buddies = load(me.buddylist.clone()).await?;
    let ignored = load(me.ignorelist.clone()).await?;
    let requests: Vec<(i32, i32, String)> = sqlx::query_as("SELECT b.id, u.uid, u.username FROM buddyrequests b JOIN users u ON u.uid = b.uid WHERE b.touid = $1").bind(me.uid).fetch_all(&ctx.app.db).await?;
    page(
        &ctx,
        "usercp/lists.html",
        "lists",
        "Friends and Ignore List",
        minijinja::context! { buddies => buddies, ignored => ignored, requests => requests },
    )
    .await
}

#[derive(Deserialize)]
pub struct ListAdd {
    #[serde(default, deserialize_with = "de::string")]
    pub list: String,
    #[serde(default, deserialize_with = "de::i32")]
    pub uid: i32,
    #[serde(default, deserialize_with = "de::string")]
    pub usernames: String,
}

/// Most usernames accepted in one "add to list" request.
const MAX_LIST_NAMES: usize = 20;

pub async fn lists_add(ctx: Ctx, CsrfForm(f): CsrfForm<ListAdd>) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let mut uids: Vec<i32> = vec![];
    if f.uid > 0 {
        uids.push(f.uid);
    }
    if !f.usernames.trim().is_empty() {
        let names: Vec<String> = f
            .usernames
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        if names.len() > MAX_LIST_NAMES {
            return Err(AppError::user(format!(
                "You can add at most {MAX_LIST_NAMES} names at a time."
            )));
        }
        uids.extend(
            sqlx::query_scalar::<_, i32>("SELECT uid FROM users WHERE lower(username) = ANY($1)")
                .bind(&names)
                .fetch_all(&ctx.app.db)
                .await?,
        );
    }
    uids.retain(|u| *u != me.uid);
    uids.sort_unstable();
    uids.dedup();
    let (col, add) = match f.list.as_str() {
        "buddy" => ("buddylist", true),
        "ignore" => ("ignorelist", true),
        "removebuddy" => ("buddylist", false),
        "removeignore" => ("ignorelist", false),
        _ => return Err(AppError::user("Unknown list.")),
    };
    if add {
        // `uid` comes straight from the form: keep only members that exist, so nobody
        // is alerted for (or stored as) an id that is not an account.
        uids = sqlx::query_scalar("SELECT uid FROM users WHERE uid = ANY($1)")
            .bind(&uids)
            .fetch_all(&ctx.app.db)
            .await?;
        sqlx::query(&format!(
            "UPDATE users SET {col} = (SELECT ARRAY(SELECT DISTINCT unnest({col} || $2::int[]))) WHERE uid = $1"
        ))
        .bind(me.uid)
        .bind(&uids)
        .execute(&ctx.app.db)
        .await?;
        // Adding a friend removes them from the ignore list and vice versa.
        let other = if col == "buddylist" {
            "ignorelist"
        } else {
            "buddylist"
        };
        sqlx::query(&format!("UPDATE users SET {other} = (SELECT ARRAY(SELECT unnest({other}) EXCEPT SELECT unnest($2::int[]))) WHERE uid = $1"))
            .bind(me.uid)
            .bind(&uids)
            .execute(&ctx.app.db)
            .await?;
        if col == "buddylist" {
            // Only people who weren't already friends: re-adding must not re-alert them.
            for u in uids.iter().filter(|u| !me.buddylist.contains(u)) {
                crate::notify::alert(
                    &ctx.app,
                    *u,
                    me.uid,
                    "buddy",
                    me.uid,
                    serde_json::json!({"poster": me.username}),
                )
                .await;
            }
        }
    } else {
        sqlx::query(&format!("UPDATE users SET {col} = (SELECT ARRAY(SELECT unnest({col}) EXCEPT SELECT unnest($2::int[]))) WHERE uid = $1"))
            .bind(me.uid)
            .bind(&uids)
            .execute(&ctx.app.db)
            .await?;
    }
    let to = crate::routes::misc_back(&ctx, "/usercp/lists");
    Ok(ctx.redirect(&to, "Your lists have been updated."))
}

// ---------------------------------------------------------------- notepad / alerts

pub async fn notepad(ctx: Ctx) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    page(
        &ctx,
        "usercp/notepad.html",
        "notepad",
        "Personal Notepad",
        minijinja::context! { notepad => &me.notepad },
    )
    .await
}

#[derive(Deserialize)]
pub struct NotepadForm {
    #[serde(default, deserialize_with = "de::string")]
    pub notepad: String,
}

pub async fn notepad_save(ctx: Ctx, CsrfForm(f): CsrfForm<NotepadForm>) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    sqlx::query("UPDATE users SET notepad = $2 WHERE uid = $1")
        .bind(me.uid)
        .bind(f.notepad.chars().take(60000).collect::<String>())
        .execute(&ctx.app.db)
        .await?;
    Ok(ctx.redirect("/usercp/notepad", "Your notepad has been saved."))
}

pub async fn badges(ctx: Ctx) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let list = crate::badges::of_member_all(&ctx.app.db, &ctx.cache, me.uid).await?;
    // Posts show the first few shown badges (same default as postbit.html).
    let on_posts = match ctx.cache.settings.get("badgespostbit").trim() {
        "" => 3,
        n => n.parse::<usize>().unwrap_or(0),
    };
    let mut shown = 0;
    let list: Vec<_> = list
        .iter()
        .map(|b| {
            shown += usize::from(!b.hidden);
            minijinja::context! { b => b, on_posts => !b.hidden && shown <= on_posts }
        })
        .collect();
    page(
        &ctx,
        "usercp/badges.html",
        "badges",
        "Your Badges",
        minijinja::context! { badges => list, on_posts => on_posts },
    )
    .await
}

#[derive(Deserialize)]
pub struct BadgesForm {
    /// Every badge, top to bottom, as the page showed them.
    #[serde(default, deserialize_with = "de::vec_i32")]
    pub order: Vec<i32>,
    #[serde(default, deserialize_with = "de::vec_i32")]
    pub shown: Vec<i32>,
    /// "up:<bid>", "down:<bid>" or "reset" (the board's order); empty just saves.
    #[serde(default, deserialize_with = "de::string")]
    pub action: String,
}

pub async fn badges_save(ctx: Ctx, CsrfForm(f): CsrfForm<BadgesForm>) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let current: Vec<i32> = crate::badges::of_member_all(&ctx.app.db, &ctx.cache, me.uid)
        .await?
        .iter()
        .map(|b| b.bid)
        .collect();
    // Only the badges the page showed: one earned (or re-enabled) since then had no Show box, so
    // it's left as it is rather than hidden, and keeps following the arranged ones.
    let current: Vec<i32> = current
        .into_iter()
        .filter(|bid| f.order.contains(bid))
        .collect();
    // The order the member saw, limited to badges they still have.
    let mut order: Vec<i32> = Vec::with_capacity(current.len());
    for bid in &f.order {
        if current.contains(bid) && !order.contains(bid) {
            order.push(*bid);
        }
    }
    let step = |dir: &str| {
        f.action
            .strip_prefix(dir)
            .and_then(|b| b.parse::<i32>().ok())
            .and_then(|bid| order.iter().position(|b| *b == bid))
    };
    let swap = match (step("up:"), step("down:")) {
        (Some(i), _) if i > 0 => Some((i - 1, i)),
        (_, Some(i)) if i + 1 < order.len() => Some((i, i + 1)),
        _ => None,
    };
    if let Some((a, b)) = swap {
        order.swap(a, b);
    }
    let order = if f.action == "reset" {
        Some(&[][..])
    } else {
        (order != current).then_some(&order[..])
    };
    crate::badges::arrange(&ctx.app.db, me.uid, &current, &f.shown, order).await?;
    let msg = if f.action.is_empty() {
        "Your badges have been saved."
    } else {
        ""
    };
    Ok(ctx.redirect("/usercp/badges", msg))
}

pub fn describe_alert(
    kind: &str,
    extra: &serde_json::Value,
    from: &str,
    object_id: i32,
) -> (String, String) {
    let subject = extra["subject"].as_str().unwrap_or("");
    let tid = extra["tid"].as_i64().unwrap_or(0);
    match kind {
        "quoted" => (
            format!("{from} quoted you in “{subject}”"),
            format!("/post/{object_id}"),
        ),
        "mention" => (
            format!("{from} mentioned you in “{subject}”"),
            format!("/post/{object_id}"),
        ),
        "subscribed_thread" => (
            format!("{from} replied to “{subject}”"),
            format!("/post/{object_id}"),
        ),
        "subscribed_forum" => (
            format!(
                "{from} started “{subject}” in {}",
                extra["forum"].as_str().unwrap_or("")
            ),
            format!("/thread/{tid}"),
        ),
        "reaction" => (
            format!("{from} reacted to your post in “{subject}”"),
            format!("/post/{object_id}"),
        ),
        "pm" => (
            format!("{from} sent you a private message: “{subject}”"),
            format!("/pm/read/{object_id}"),
        ),
        "reputation" => (
            format!(
                "{from} rated you ({})",
                extra["reputation"].as_i64().unwrap_or(0)
            ),
            format!("/reputation/{object_id}"),
        ),
        "pgp_keychange" => (
            format!(
                "{from} has a new encryption key. Verify them again before trusting their messages."
            ),
            format!("/pm/verify/{object_id}"),
        ),
        "pgp_keyrevoked" => (
            format!("{from} revoked the encryption key you verified."),
            format!("/pm/verify/{object_id}"),
        ),
        "buddy" => (
            format!("{from} added you as a friend"),
            format!("/user/{object_id}"),
        ),
        "warning" => (
            "You have received a warning".to_string(),
            format!("/warning/{object_id}"),
        ),
        "badge" => (
            format!(
                "You earned the badge “{}”",
                extra["badge"].as_str().unwrap_or("")
            ),
            format!("/badges/{object_id}"),
        ),
        "thread_moved" => (
            format!("Your thread “{subject}” was moved"),
            url_thread(tid, None),
        ),
        _ => (format!("{kind} from {from}"), "/usercp/alerts".into()),
    }
}

pub async fn alerts(
    ctx: Ctx,
    Query(q): Query<crate::routes::forumdisplay::FdQuery>,
) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM alerts WHERE uid = $1")
        .bind(me.uid)
        .fetch_one(&ctx.app.db)
        .await?;
    let pg = util::paginate(
        total,
        30,
        util::clamp_page(q.page),
        "/usercp/alerts?page={page}",
    );
    #[allow(clippy::type_complexity)]
    let rows: Vec<(i64, String, i32, serde_json::Value, i64, bool, Option<String>, Option<i32>)> = sqlx::query_as(
        "SELECT a.id, a.kind, a.object_id, a.extra, a.dateline, a.unread, u.username, u.uid FROM alerts a LEFT JOIN users u ON u.uid = a.from_uid
         WHERE a.uid = $1 ORDER BY a.id DESC LIMIT 30 OFFSET $2",
    )
    .bind(me.uid)
    .bind((pg.page - 1) * 30)
    .fetch_all(&ctx.app.db)
    .await?;
    let from_uids: Vec<i32> = rows.iter().filter_map(|r| r.7).collect();
    let faces = crate::render::avatars(&ctx, &from_uids).await?;
    let list: Vec<_> = rows
        .into_iter()
        .map(|(id, kind, oid, extra, dl, unread, from, from_uid)| {
            let (text, url) = describe_alert(&kind, &extra, from.as_deref().unwrap_or("Someone"), oid);
            let avatar = from_uid.and_then(|u| faces.get(&u)).map(|a| a.to_string()).unwrap_or_default();
            minijinja::context! { id => id, kind => kind, text => text, url => url, dateline => dl, unread => unread, from => from, from_uid => from_uid, avatar => avatar }
        })
        .collect();
    // Viewing the list marks everything read.
    if me.unreadalerts > 0 {
        sqlx::query("UPDATE alerts SET unread = FALSE WHERE uid = $1 AND unread")
            .bind(me.uid)
            .execute(&ctx.app.db)
            .await?;
        sqlx::query("UPDATE users SET unreadalerts = 0 WHERE uid = $1")
            .bind(me.uid)
            .execute(&ctx.app.db)
            .await?;
    }
    page(
        &ctx,
        "usercp/alerts.html",
        "alerts",
        "Alerts",
        minijinja::context! { alerts => list, pagination => pg },
    )
    .await
}

pub async fn alerts_read(
    ctx: Ctx,
    CsrfForm(_): CsrfForm<crate::routes::misc::Empty>,
) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    sqlx::query("DELETE FROM alerts WHERE uid = $1")
        .bind(me.uid)
        .execute(&ctx.app.db)
        .await?;
    sqlx::query("UPDATE users SET unreadalerts = 0 WHERE uid = $1")
        .bind(me.uid)
        .execute(&ctx.app.db)
        .await?;
    Ok(ctx.redirect("/usercp/alerts", "Your alerts have been cleared."))
}

pub async fn alerts_count(ctx: Ctx) -> AppResult<Response> {
    let me = ctx.require_login()?;
    Ok(Json(serde_json::json!({"alerts": me.unreadalerts, "pms": me.unreadpms})).into_response())
}

// ---------------------------------------------------------------- security

fn totp_for(secret: &str, account: &str, issuer: &str) -> Option<totp_rs::TOTP> {
    let bytes = totp_rs::Secret::Encoded(secret.to_string())
        .to_bytes()
        .ok()?;
    totp_rs::TOTP::new(
        totp_rs::Algorithm::SHA1,
        6,
        1,
        30,
        bytes,
        Some(issuer.to_string()),
        account.to_string(),
    )
    .ok()
}

pub async fn security(ctx: Ctx) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let logins: Vec<(String, i64, i64, String, String)> =
        sqlx::query_as("SELECT token_hash, created, lastused, COALESCE(host(ip), ''), useragent FROM logins WHERE uid = $1 AND expires > $2 ORDER BY created DESC")
            .bind(me.uid)
            .bind(now())
            .fetch_all(&ctx.app.db)
            .await?;
    let sessions: Vec<_> = logins
        .into_iter()
        .map(|(h, c, _l, ip, ua)| minijinja::context! { id => h.get(..16).unwrap_or(&h).to_string(), current => Some(&h) == ctx.token_hash.as_ref(), created => c, ip => ip, useragent => ua })
        .collect();
    let tokens: Vec<(i64, String, Vec<String>, i64, Option<i64>, i64)> = sqlx::query_as(
        "SELECT id, name, scopes, EXTRACT(EPOCH FROM created_at)::bigint, EXTRACT(EPOCH FROM last_used_at)::bigint,
                EXTRACT(EPOCH FROM expires_at)::bigint
         FROM api_tokens WHERE uid = $1 AND revoked_at IS NULL AND expires_at > now() ORDER BY id DESC",
    )
    .bind(me.uid)
    .fetch_all(&ctx.app.db)
    .await?;
    let api_tokens: Vec<_> = tokens
        .into_iter()
        .map(|(id, name, scopes, created, last, expires)| minijinja::context! { id => id, name => name, scopes => scopes, created => created, last_used => last, expires => expires })
        .collect();
    let keys: Vec<(i32, String, i64, i64)> = sqlx::query_as(
        "SELECT id, name, created, last_used FROM passkeys WHERE uid = $1 ORDER BY id",
    )
    .bind(me.uid)
    .fetch_all(&ctx.app.db)
    .await?;
    let passkeys: Vec<_> = keys
        .into_iter()
        .map(|(id, name, created, last_used)| minijinja::context! { id => id, name => name, created => created, last_used => last_used })
        .collect();
    let passkeys_unavailable = crate::passkeys::available(&ctx.app).err();
    let enabled = !me.totp_secret.is_empty();
    let (secret, qr, uri) = if enabled {
        (String::new(), String::new(), String::new())
    } else {
        let secret = totp_rs::Secret::generate_secret().to_encoded().to_string();
        let issuer = ctx.settings().get("bbname").replace(':', "");
        let t = totp_for(&secret, &me.username.replace(':', ""), &issuer);
        let uri = t.as_ref().map(|t| t.get_url()).unwrap_or_default();
        let qr = qrcode::QrCode::new(uri.as_bytes())
            .map(|c| {
                c.render::<qrcode::render::svg::Color>()
                    .min_dimensions(180, 180)
                    .dark_color(qrcode::render::svg::Color("#1d2530"))
                    .light_color(qrcode::render::svg::Color("#ffffff"))
                    .build()
            })
            .unwrap_or_default();
        (secret, qr, uri)
    };
    page(&ctx, "usercp/security.html", "security", "Security", minijinja::context! { enabled => enabled, secret => secret, qr => qr, uri => uri, sessions => sessions, api_tokens => api_tokens, passkeys => passkeys, passkeys_unavailable => passkeys_unavailable, passkeys_max => crate::passkeys::MAX_PER_MEMBER }).await
}

#[derive(Deserialize)]
pub struct TwoFaForm {
    #[serde(default, deserialize_with = "de::string")]
    pub secret: String,
    #[serde(default, deserialize_with = "de::string")]
    pub code: String,
    #[serde(default, deserialize_with = "de::string")]
    pub password: String,
    #[serde(default, deserialize_with = "de::bool")]
    pub disable: bool,
}

pub async fn twofa_save(ctx: Ctx, CsrfForm(f): CsrfForm<TwoFaForm>) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    reauth_throttle(&ctx, me.uid).await?;
    if !auth::verify_password(&f.password, &me.password).await {
        return Err(AppError::user("Your password is incorrect."));
    }
    if f.disable {
        if !crate::routes::member::totp_consume(&ctx.app, me.uid, &me.totp_secret, &f.code).await? {
            return Err(AppError::user("The authentication code is incorrect."));
        }
        crate::audit::log(&ctx, me.uid, "twofa_disabled", serde_json::Value::Null).await;
        sqlx::query("UPDATE users SET totp_secret = '' WHERE uid = $1")
            .bind(me.uid)
            .execute(&ctx.app.db)
            .await?;
        return Ok(ctx.redirect(
            "/usercp/security",
            "Two-factor authentication has been disabled.",
        ));
    }
    let Some(step) = crate::routes::member::totp_step(&f.secret, &f.code) else {
        return Err(AppError::user(
            "The authentication code is incorrect. Make sure your device's clock is correct and try again.",
        ));
    };
    sqlx::query("UPDATE users SET totp_secret = $2, totp_last_step = $3 WHERE uid = $1")
        .bind(me.uid)
        .bind(&f.secret)
        .bind(step)
        .execute(&ctx.app.db)
        .await?;
    auth::destroy_all_logins(&ctx.app, me.uid, ctx.token_hash.as_deref()).await?;
    auth::rotate_login(&ctx).await?;
    crate::audit::log(&ctx, me.uid, "twofa_enabled", serde_json::Value::Null).await;
    Ok(ctx.redirect(
        "/usercp/security",
        "Two-factor authentication is now enabled.",
    ))
}

#[derive(Deserialize)]
pub struct RevokeForm {
    #[serde(default, deserialize_with = "de::string")]
    pub id: String,
}

pub async fn revoke_login(ctx: Ctx, CsrfForm(f): CsrfForm<RevokeForm>) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    if let Some(id) =
        f.id.strip_prefix("api:")
            .and_then(|i| i.parse::<i64>().ok())
    {
        sqlx::query("UPDATE api_tokens SET revoked_at = now() WHERE id = $1 AND uid = $2 AND revoked_at IS NULL")
            .bind(id)
            .bind(me.uid)
            .execute(&ctx.app.db)
            .await?;
    } else if f.id == "all" {
        auth::destroy_all_logins(&ctx.app, me.uid, ctx.token_hash.as_deref()).await?;
    } else if f.id.len() == 16 && f.id.chars().all(|c| c.is_ascii_hexdigit()) {
        sqlx::query("DELETE FROM logins WHERE uid = $1 AND token_hash LIKE $2 || '%'")
            .bind(me.uid)
            .bind(&f.id)
            .execute(&ctx.app.db)
            .await?;
    }
    crate::audit::log(&ctx, ctx.uid(), "session_revoked", serde_json::Value::Null).await;
    Ok(ctx.redirect("/usercp/security", "The session has been logged out."))
}

// ---------------------------------------------------------------- usergroups

pub async fn usergroups(ctx: Ctx) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let joinable: Vec<_> = ctx
        .cache
        .groups
        .values()
        .filter(|g| g.kind == 3 || g.kind == 4)
        .map(|g| minijinja::context! { gid => g.gid, title => &g.title, description => &g.description, kind => g.kind, member => me.all_groups().contains(&g.gid), primary => me.usergroup == g.gid })
        .collect();
    let pending: Vec<i32> = sqlx::query_scalar("SELECT gid FROM joinrequests WHERE uid = $1")
        .bind(me.uid)
        .fetch_all(&ctx.app.db)
        .await?;
    let led: Vec<(i32, bool, bool)> = sqlx::query_as(
        "SELECT gid, canmanagemembers, canmanagerequests FROM groupleaders WHERE uid = $1",
    )
    .bind(me.uid)
    .fetch_all(&ctx.app.db)
    .await?;
    let mut leading = vec![];
    for (gid, can_members, can_requests) in led {
        let members: Vec<(i32, String)> = sqlx::query_as("SELECT uid, username FROM users WHERE usergroup = $1 OR $1 = ANY(additionalgroups) ORDER BY lower(username) LIMIT 500")
            .bind(gid)
            .fetch_all(&ctx.app.db)
            .await?;
        let requests: Vec<(i32, i32, String, String)> = sqlx::query_as("SELECT r.rid, u.uid, u.username, r.reason FROM joinrequests r JOIN users u ON u.uid = r.uid WHERE r.gid = $1 AND NOT r.invite")
            .bind(gid)
            .fetch_all(&ctx.app.db)
            .await?;
        leading.push(minijinja::context! { gid => gid, title => ctx.cache.group(gid).map(|g| g.title.clone()), members => members, requests => requests, can_members => can_members, can_requests => can_requests });
    }
    let displaygroups: Vec<(i32, String)> = me
        .all_groups()
        .into_iter()
        .filter_map(|g| {
            ctx.cache
                .group(g)
                .filter(|gr| gr.perms.0.candisplaygroup)
                .map(|gr| (g, gr.title.clone()))
        })
        .collect();
    page(
        &ctx,
        "usercp/usergroups.html",
        "usergroups",
        "Group Memberships",
        minijinja::context! { joinable => joinable, pending => pending, leading => leading, displaygroups => displaygroups, displaygroup => me.display_group() },
    )
    .await
}

#[derive(Deserialize)]
pub struct GroupAction {
    #[serde(default, deserialize_with = "de::string")]
    pub action: String,
    #[serde(default, deserialize_with = "de::i32")]
    pub gid: i32,
    #[serde(default, deserialize_with = "de::i32")]
    pub uid: i32,
    #[serde(default, deserialize_with = "de::i32")]
    pub rid: i32,
    #[serde(default, deserialize_with = "de::string")]
    pub reason: String,
    #[serde(default, deserialize_with = "de::string")]
    pub username: String,
}

pub async fn add_to_group(db: &sqlx::PgPool, uid: i32, gid: i32) -> AppResult<()> {
    sqlx::query("UPDATE users SET additionalgroups = array_append(additionalgroups, $2) WHERE uid = $1 AND usergroup <> $2 AND NOT ($2 = ANY(additionalgroups))")
        .bind(uid)
        .bind(gid)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn remove_from_group(db: &sqlx::PgPool, uid: i32, gid: i32) -> AppResult<()> {
    sqlx::query("UPDATE users SET additionalgroups = array_remove(additionalgroups, $2), displaygroup = CASE WHEN displaygroup = $2 THEN 0 ELSE displaygroup END WHERE uid = $1")
        .bind(uid)
        .bind(gid)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn usergroups_action(
    ctx: Ctx,
    CsrfForm(f): CsrfForm<GroupAction>,
) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let group = ctx.cache.group(f.gid).cloned();
    let leader: Option<(bool, bool)> = sqlx::query_as(
        "SELECT canmanagemembers, canmanagerequests FROM groupleaders WHERE uid = $1 AND gid = $2",
    )
    .bind(me.uid)
    .bind(f.gid)
    .fetch_optional(&ctx.app.db)
    .await?;
    // Leaders run custom and public groups (kinds 2-4). The built-in groups (kind 1: staff,
    // members, ...) are admin-assigned, so a leader row on one grants nothing here.
    let leader = leader.filter(|_| group.as_ref().is_some_and(|g| g.kind != 1));
    let msg = match f.action.as_str() {
        "join" => {
            let g = group.ok_or_else(|| AppError::not_found("group"))?;
            match g.kind {
                3 => {
                    add_to_group(&ctx.app.db, me.uid, g.gid).await?;
                    crate::audit::log(
                        &ctx,
                        me.uid,
                        "group_joined",
                        serde_json::json!({"gid": g.gid, "title": g.title}),
                    )
                    .await;
                    "You have joined the group."
                }
                4 => {
                    sqlx::query("INSERT INTO joinrequests (uid, gid, reason, dateline) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING")
                        .bind(me.uid)
                        .bind(g.gid)
                        .bind(f.reason.chars().take(250).collect::<String>())
                        .bind(now())
                        .execute(&ctx.app.db)
                        .await?;
                    "Your request to join has been sent to the group leaders."
                }
                _ => return Err(AppError::no_perm()),
            }
        }
        "leave" => {
            if me.usergroup == f.gid {
                return Err(AppError::user("You cannot leave your primary group."));
            }
            // Only publicly joinable groups can be left; staff-assigned groups stay put.
            if !group
                .as_ref()
                .map(|g| g.kind == 3 || g.kind == 4)
                .unwrap_or(false)
            {
                return Err(AppError::no_perm());
            }
            remove_from_group(&ctx.app.db, me.uid, f.gid).await?;
            crate::audit::log(
                &ctx,
                me.uid,
                "group_left",
                serde_json::json!({"gid": f.gid}),
            )
            .await;
            "You have left the group."
        }
        "display" => {
            if !me.all_groups().contains(&f.gid)
                || !group.map(|g| g.perms.0.candisplaygroup).unwrap_or(false)
            {
                return Err(AppError::no_perm());
            }
            sqlx::query("UPDATE users SET displaygroup = $2 WHERE uid = $1")
                .bind(me.uid)
                .bind(f.gid)
                .execute(&ctx.app.db)
                .await?;
            "Your display group has been changed."
        }
        "accept" | "decline" => {
            if !leader.map(|l| l.1).unwrap_or(false) {
                return Err(AppError::no_perm());
            }
            let req: Option<(i32, i32)> = sqlx::query_as(
                "DELETE FROM joinrequests WHERE rid = $1 AND gid = $2 AND NOT invite RETURNING uid, gid",
            )
            .bind(f.rid)
            .bind(f.gid)
            .fetch_optional(&ctx.app.db)
            .await?;
            if let (Some((uid, gid)), "accept") = (req, f.action.as_str()) {
                add_to_group(&ctx.app.db, uid, gid).await?;
            }
            "The request has been processed."
        }
        "kick" => {
            if !leader.map(|l| l.0).unwrap_or(false) {
                return Err(AppError::no_perm());
            }
            remove_from_group(&ctx.app.db, f.uid, f.gid).await?;
            "The member has been removed from the group."
        }
        "add" => {
            if !leader.map(|l| l.0).unwrap_or(false) {
                return Err(AppError::no_perm());
            }
            let uid: i32 =
                sqlx::query_scalar("SELECT uid FROM users WHERE lower(username) = lower($1)")
                    .bind(f.username.trim())
                    .fetch_optional(&ctx.app.db)
                    .await?
                    .ok_or_else(|| AppError::not_found("user"))?;
            add_to_group(&ctx.app.db, uid, f.gid).await?;
            "The member has been added to the group."
        }
        _ => return Err(AppError::user("Unknown action.")),
    };
    Ok(ctx.redirect("/usercp/usergroups", msg))
}

// ---------------------------------------------------------------- account deletion / export

pub async fn delete_form(ctx: Ctx) -> AppResult<Response> {
    require_ucp(&ctx).await?;
    if !ctx.settings().bool("allowaccountdeletion") {
        return Err(AppError::no_perm());
    }
    page(
        &ctx,
        "usercp/delete.html",
        "delete",
        "Delete Account",
        minijinja::context! {},
    )
    .await
}

#[derive(Deserialize)]
pub struct DeleteForm {
    #[serde(default, deserialize_with = "de::string")]
    pub password: String,
    #[serde(default, deserialize_with = "de::bool")]
    pub deleteposts: bool,
}

/// Delete a user account. Posts are kept (attributed to a guest with the old name) unless requested.
/// Permanently delete all threads and posts made by a user.
pub async fn delete_user_content(app: &crate::app::App, uid: i32) -> AppResult<()> {
    let tids: Vec<i32> = sqlx::query_scalar("SELECT tid FROM threads WHERE uid = $1")
        .bind(uid)
        .fetch_all(&app.db)
        .await?;
    for chunk in tids.chunks(200) {
        crate::ops::delete_threads(app, chunk).await?;
    }
    let pids: Vec<i32> = sqlx::query_scalar("SELECT pid FROM posts WHERE uid = $1")
        .bind(uid)
        .fetch_all(&app.db)
        .await?;
    for chunk in pids.chunks(500) {
        crate::ops::delete_posts(app, chunk).await?;
    }
    Ok(())
}

pub async fn delete_user(app: &crate::app::App, uid: i32, delete_posts: bool) -> AppResult<()> {
    let anonymize = app.cache().settings.bool("privacy_anonymize_deleted");
    delete_user_with(app, uid, delete_posts, anonymize).await
}

/// Delete an account. With `anonymize`, what the member leaves behind (kept posts, votes,
/// messages in other inboxes, mail and search logs) loses their name and IP addresses.
pub async fn delete_user_with(
    app: &crate::app::App,
    uid: i32,
    delete_posts: bool,
    anonymize: bool,
) -> AppResult<()> {
    crate::system::guard(&app.cache(), uid, "deleted")?;
    if delete_posts {
        delete_user_content(app, uid).await?;
    }
    let mut tx = app.db.begin().await?;
    if anonymize {
        let name = app
            .cache()
            .settings
            .get("privacy_deleted_name")
            .trim()
            .to_string();
        crate::privacy::anonymize_member(
            &mut tx,
            uid,
            if name.is_empty() {
                "Former member"
            } else {
                &name
            },
        )
        .await?;
    }
    sqlx::query("UPDATE posts SET uid = 0 WHERE uid = $1")
        .bind(uid)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE threads SET uid = 0 WHERE uid = $1")
        .bind(uid)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE threads SET lastposteruid = 0 WHERE lastposteruid = $1")
        .bind(uid)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE forums SET lastposteruid = 0 WHERE lastposteruid = $1")
        .bind(uid)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM privatemessages WHERE uid = $1")
        .bind(uid)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM moderators WHERE id = $1 AND NOT isgroup")
        .bind(uid)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE uid = $1")
        .bind(uid)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE users SET buddylist = array_remove(buddylist, $1), ignorelist = array_remove(ignorelist, $1) WHERE $1 = ANY(buddylist) OR $1 = ANY(ignorelist)").bind(uid).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM users WHERE uid = $1")
        .bind(uid)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE counters SET numusers = GREATEST(numusers - 1, 0) WHERE id = 1")
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE counters SET lastuid = COALESCE((SELECT MAX(uid) FROM users), 0) WHERE id = 1 AND lastuid = $1").bind(uid).execute(&mut *tx).await?;
    sqlx::query("UPDATE counters SET lastusername = COALESCE((SELECT username FROM users WHERE uid = counters.lastuid), '') WHERE id = 1").execute(&mut *tx).await?;
    tx.commit().await?;
    app.stats_cache.invalidate_all();
    Ok(())
}

pub async fn delete_account(ctx: Ctx, CsrfForm(f): CsrfForm<DeleteForm>) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    if !ctx.settings().bool("allowaccountdeletion") || me.usergroup == 4 {
        return Err(AppError::user(
            "This account cannot be deleted from the User CP.",
        ));
    }
    reauth_throttle(&ctx, me.uid).await?;
    if !auth::verify_password(&f.password, &me.password).await {
        return Err(AppError::user("Your password is incorrect."));
    }
    delete_user(&ctx.app, me.uid, f.deleteposts).await?;
    ctx.clear_cookie(crate::ctx::AUTH_COOKIE);
    Ok(ctx.redirect("/", "Your account has been deleted."))
}

/// Audit log actions that are sign-ins (or attempts); the rest of the log is account activity.
const SIGN_IN_ACTIONS: &[&str] = &[
    "login",
    "login_failed",
    "login_locked",
    "login_2fa_failed",
    "logout",
    "session_revoked",
];

const EXPORT_README: &str = "\
Your account data from this board. Times are UTC.

user/              profile, IP addresses, sign-ins, devices, account activity
posts/             your posts
private_messages/  one file per folder (encrypted messages stay encrypted)
";

/// A unix timestamp as an ISO 8601 UTC string; null when unset.
fn iso(ts: i64) -> serde_json::Value {
    match chrono::DateTime::from_timestamp(ts, 0) {
        Some(d) if ts > 0 => d.to_rfc3339().into(),
        _ => serde_json::Value::Null,
    }
}

/// A file name for a private message folder: lowercase, `[a-z0-9_]` only.
fn folder_file(id: i32, name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let slug = slug.trim_matches('_');
    if id >= 5 || slug.is_empty() {
        format!("folder_{id}_{slug}")
            .trim_end_matches('_')
            .to_string()
    } else {
        slug.to_string()
    }
}

fn zip_files(files: &[(String, Vec<u8>)]) -> anyhow::Result<Vec<u8>> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, body) in files {
        w.start_file(name.as_str(), opts)?;
        std::io::Write::write_all(&mut w, body)?;
    }
    Ok(w.finish()?.into_inner())
}

/// Download a copy of the account's data (GDPR data portability): a zip of JSON files for the
/// account itself (profile, IP addresses, sign-ins), its posts and its private messages.
pub async fn export(ctx: Ctx) -> AppResult<Response> {
    use serde_json::json;
    let me = require_ucp(&ctx).await?;
    crate::audit::log(&ctx, me.uid, "data_exported", serde_json::Value::Null).await;
    let db = &ctx.app.db;
    let mut files: Vec<(String, serde_json::Value)> = Vec::new();

    // Profile.
    let fields = field_values(&ctx, me.uid).await?;
    let passkeys: Vec<(String, i64, i64)> =
        sqlx::query_as("SELECT name, created, last_used FROM passkeys WHERE uid = $1 ORDER BY id")
            .bind(me.uid)
            .fetch_all(db)
            .await?;
    let badges: Vec<(String, i64)> = sqlx::query_as(
        "SELECT b.name, ub.dateline FROM user_badges ub JOIN badges b ON b.bid = ub.bid WHERE ub.uid = $1 ORDER BY ub.dateline",
    )
    .bind(me.uid)
    .fetch_all(db)
    .await?;
    files.push(("user/profile.json".into(), json!({
        "uid": me.uid, "username": me.username, "email": me.email, "usertitle": me.usertitle,
        "registered": iso(me.regdate), "last_active": iso(me.lastactive),
        "website": me.website, "birthday": me.birthday, "signature": me.signature, "timezone": me.timezone,
        "notepad": me.notepad, "posts": me.postnum, "threads": me.threadnum, "reputation": me.reputation,
        "two_factor_enabled": !me.totp_secret.is_empty(),
        "profile_fields": fields,
        "passkeys": passkeys.iter().map(|p| json!({"name": p.0, "created": iso(p.1), "last_used": iso(p.2)})).collect::<Vec<_>>(),
        "badges": badges.iter().map(|b| json!({"name": b.0, "earned": iso(b.1)})).collect::<Vec<_>>(),
    })));

    // IP addresses: everywhere the board recorded one for this account. Audit entries made by
    // staff carry the staff member's address, and received messages the sender's, so both are left out.
    let ips: Vec<(String, String, Option<i64>, Option<i64>, i64)> = sqlx::query_as(
        "SELECT host(ip), source, MIN(d) FILTER (WHERE d > 0), MAX(d) FILTER (WHERE d > 0), COUNT(*) FROM (
             SELECT regip AS ip, 'registration' AS source, regdate AS d FROM users WHERE uid = $1
             UNION ALL SELECT lastip, 'most recent sign-in', lastactive FROM users WHERE uid = $1
             UNION ALL SELECT ipaddress, action, dateline FROM user_audit WHERE uid = $1 AND actor_uid IS NULL
             UNION ALL SELECT ip, 'signed-in device', lastused FROM logins WHERE uid = $1
             UNION ALL SELECT ipaddress, 'post', dateline FROM posts WHERE uid = $1
             UNION ALL SELECT ipaddress, 'private message', dateline FROM privatemessages WHERE uid = $1 AND fromid = $1
             UNION ALL SELECT ipaddress, 'poll vote', dateline FROM pollvotes WHERE uid = $1
             UNION ALL SELECT ipaddress, 'thread rating', 0 FROM threadratings WHERE uid = $1
         ) s WHERE ip IS NOT NULL GROUP BY 1, 2 ORDER BY 1, 2",
    )
    .bind(me.uid)
    .fetch_all(db)
    .await?;
    let mut by_ip: Vec<(String, Option<i64>, Option<i64>, Vec<serde_json::Value>)> = Vec::new();
    for (ip, source, first, last, count) in ips {
        let source = match crate::audit::ACTIONS.iter().find(|a| a.0 == source) {
            Some(a) => a.1.to_string(),
            None => source,
        };
        if by_ip.last().is_none_or(|e| e.0 != ip) {
            by_ip.push((ip, None, None, Vec::new()));
        }
        let e = by_ip.last_mut().expect("pushed above");
        e.1 = match (e.1, first) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        e.2 = e.2.max(last);
        e.3.push(json!({"source": source, "times": count, "first_seen": iso(first.unwrap_or(0)), "last_seen": iso(last.unwrap_or(0))}));
    }
    by_ip.sort_by_key(|e| std::cmp::Reverse(e.2));
    files.push((
        "user/ip_addresses.json".into(),
        by_ip
            .into_iter()
            .map(|(ip, first, last, seen)| json!({"address": ip, "first_seen": iso(first.unwrap_or(0)), "last_seen": iso(last.unwrap_or(0)), "seen_in": seen}))
            .collect(),
    ));

    // The account audit log, split into sign-ins and everything else. Capped so a very old
    // account can't make this one request build an unbounded zip in memory: the newest entries
    // are kept, then put back in chronological order.
    let mut audit: Vec<(i64, String, Option<String>, String, Option<i32>, Option<String>, serde_json::Value)> = sqlx::query_as(
        "SELECT a.dateline, a.action, host(a.ipaddress), a.useragent, a.actor_uid, u.username, a.details
         FROM user_audit a LEFT JOIN users u ON u.uid = a.actor_uid WHERE a.uid = $1 ORDER BY a.id DESC LIMIT 50000",
    )
    .bind(me.uid)
    .fetch_all(db)
    .await?;
    audit.reverse();
    let (mut sign_ins, mut activity) = (Vec::new(), Vec::new());
    for (dateline, action, ip, ua, actor, actor_name, details) in audit {
        let (label, category) = crate::audit::describe(&action);
        let mut e = json!({"time": iso(dateline), "action": action, "description": label});
        if actor.is_some() {
            e["by_staff"] = json!(actor_name.unwrap_or_else(|| "a former staff member".into()));
        } else {
            e["ip_address"] = json!(ip);
            e["device"] = json!(crate::audit::device_label(&ua));
            e["user_agent"] = json!(ua);
        }
        if SIGN_IN_ACTIONS.contains(&e["action"].as_str().unwrap_or("")) {
            sign_ins.push(e);
        } else {
            e["category"] = json!(category);
            if details.as_object().is_some_and(|d| !d.is_empty()) {
                e["details"] = details;
            }
            activity.push(e);
        }
    }
    files.push(("user/sign_ins.json".into(), sign_ins.into()));
    files.push(("user/account_activity.json".into(), activity.into()));

    let devices: Vec<(i64, i64, i64, Option<String>, String)> = sqlx::query_as(
        "SELECT created, lastused, expires, host(ip), useragent FROM logins WHERE uid = $1 AND expires > $2 ORDER BY lastused DESC",
    )
    .bind(me.uid)
    .bind(now())
    .fetch_all(db)
    .await?;
    files.push((
        "user/devices.json".into(),
        devices
            .iter()
            .map(|d| json!({"signed_in": iso(d.0), "last_used": iso(d.1), "expires": iso(d.2), "ip_address": d.3, "device": crate::audit::device_label(&d.4), "user_agent": d.4}))
            .collect(),
    ));

    // Posts.
    let posts: Vec<(i32, i32, String, String, String, String, i64, i64, Option<String>, i16)> = sqlx::query_as(
        "SELECT p.pid, p.tid, t.subject, COALESCE(f.name, ''), p.subject, p.message, p.dateline, p.edittime, host(p.ipaddress), p.visible
         FROM posts p JOIN threads t ON t.tid = p.tid LEFT JOIN forums f ON f.fid = p.fid
         WHERE p.uid = $1 ORDER BY p.pid",
    )
    .bind(me.uid)
    .fetch_all(db)
    .await?;
    files.push((
        "posts/posts.json".into(),
        posts
            .iter()
            .map(|p| json!({
                "pid": p.0, "tid": p.1, "thread": p.2, "forum": p.3, "subject": p.4, "message": p.5,
                "posted": iso(p.6), "edited": iso(p.7), "ip_address": p.8,
                "status": match p.9 { 1 => "visible", 0 => "awaiting approval", _ => "deleted" },
            }))
            .collect(),
    ));

    // Private messages, one file per folder. Blind-copy recipients are only shown on your own copies.
    let pms: Vec<(i32, i32, i32, Option<String>, sqlx::types::Json<serde_json::Value>, String, String, i64, i16, i64, i16, Option<String>)> = sqlx::query_as(
        "SELECT p.pmid, p.folder, p.fromid, u.username, p.recipients, p.subject, p.message, p.dateline, p.status, p.readtime, p.pgp,
                CASE WHEN p.fromid = p.uid THEN host(p.ipaddress) END
         FROM privatemessages p LEFT JOIN users u ON u.uid = p.fromid WHERE p.uid = $1 ORDER BY p.pmid",
    )
    .bind(me.uid)
    .fetch_all(db)
    .await?;
    let ids = |v: &serde_json::Value| -> Vec<i32> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_i64().map(|x| x as i32))
                    .collect()
            })
            .unwrap_or_default()
    };
    let all_ids: Vec<i32> = pms
        .iter()
        .flat_map(|p| [ids(&p.4.0["to"]), ids(&p.4.0["bcc"])].concat())
        .collect();
    let names: HashMap<i32, String> =
        sqlx::query_as("SELECT uid, username FROM users WHERE uid = ANY($1)")
            .bind(&all_ids)
            .fetch_all(db)
            .await?
            .into_iter()
            .collect();
    let name_list = |v: &serde_json::Value| -> Vec<String> {
        ids(v)
            .iter()
            .map(|id| {
                names
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| format!("deleted member #{id}"))
            })
            .collect()
    };
    // Messages in a custom folder that has since been removed still get a file.
    let mut folders = crate::routes::private::folder_list(&me);
    for p in &pms {
        if !folders.iter().any(|f| f.0 == p.1) {
            folders.push((p.1, format!("Folder {}", p.1)));
        }
    }
    for (folder, folder_name) in folders {
        let msgs: Vec<serde_json::Value> = pms
            .iter()
            .filter(|p| p.1 == folder)
            .map(|p| {
                let mine = p.2 == me.uid;
                let mut m = json!({
                    "pmid": p.0, "from": p.3.clone().unwrap_or_else(|| if p.2 == 0 { "board".into() } else { format!("deleted member #{}", p.2) }),
                    "to": name_list(&p.4.0["to"]), "subject": p.5, "message": p.6, "sent": iso(p.7),
                    "status": match p.8 { 0 => "unread", 3 => "replied", 4 => "forwarded", _ => "read" },
                    "encrypted": p.10 == 2, "signed": p.10 == 1,
                });
                if mine {
                    m["bcc"] = json!(name_list(&p.4.0["bcc"]));
                    m["ip_address"] = json!(p.11);
                }
                if p.9 > 0 {
                    m["read"] = iso(p.9);
                }
                m
            })
            .collect();
        files.push((
            format!(
                "private_messages/{}.json",
                folder_file(folder, &folder_name)
            ),
            json!({"folder": folder_name, "messages": msgs}),
        ));
    }

    let mut out: Vec<(String, Vec<u8>)> =
        vec![("README.txt".into(), EXPORT_README.as_bytes().to_vec())];
    out.extend(
        files
            .into_iter()
            .map(|(name, v)| (name, serde_json::to_vec_pretty(&v).unwrap_or_default())),
    );
    let zip = tokio::task::spawn_blocking(move || zip_files(&out))
        .await
        .map_err(anyhow::Error::from)??;
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/zip".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"rbb-data-{}.zip\"", me.uid),
            ),
            (axum::http::header::CACHE_CONTROL, "no-store".to_string()),
        ],
        zip,
    )
        .into_response())
}

#[derive(Deserialize, Default)]
pub struct ActivityQuery {
    pub page: Option<i64>,
    #[serde(default)]
    pub kind: String,
}

/// Rows of the account audit log shaped for templates (shared with the Admin CP view). Staff
/// actions carry the staff member's IP address and browser, so `own_view` blanks both for the
/// account owner.
pub async fn audit_rows(
    ctx: &Ctx,
    uid: i32,
    kind: &str,
    page: Option<i64>,
    base: &str,
    own_view: bool,
) -> AppResult<(Vec<minijinja::Value>, util::Pagination)> {
    let actions: Vec<&str> = crate::audit::ACTIONS
        .iter()
        .filter(|a| kind.is_empty() || a.2 == kind)
        .map(|a| a.0)
        .collect();
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM user_audit WHERE uid = $1 AND ($2 = '' OR action = ANY($3))",
    )
    .bind(uid)
    .bind(kind)
    .bind(&actions)
    .fetch_one(&ctx.app.db)
    .await?;
    let per = 30;
    let pagination = util::paginate(total, per, util::clamp_page(page), base);
    let rows: Vec<(i64, String, String, String, i32, serde_json::Value, Option<String>)> = sqlx::query_as(
        "SELECT a.dateline, a.action, COALESCE(host(a.ipaddress), ''), a.useragent, COALESCE(a.actor_uid, 0), a.details, u.username
         FROM user_audit a LEFT JOIN users u ON u.uid = a.actor_uid AND a.actor_uid > 0
         WHERE a.uid = $1 AND ($2 = '' OR a.action = ANY($3)) ORDER BY a.id DESC LIMIT $4 OFFSET $5",
    )
    .bind(uid)
    .bind(kind)
    .bind(&actions)
    .bind(per)
    .bind((pagination.page - 1) * per)
    .fetch_all(&ctx.app.db)
    .await?;
    let events = rows
        .into_iter()
        .map(|(dateline, action, ip, ua, actor, details, actor_name)| {
            let (label, category) = crate::audit::describe(&action);
            let device = crate::audit::device_label(&ua);
            let (ip, ua, device) = if own_view && (actor > 0 || category == "staff") {
                (String::new(), String::new(), String::new())
            } else {
                (ip, ua, device)
            };
            minijinja::context! {
                dateline => dateline, action => action, label => label, category => category, ip => ip,
                device => device, useragent => ua, actor => actor, actor_name => actor_name,
                details => details,
            }
        })
        .collect();
    Ok((events, pagination))
}

pub async fn activity(ctx: Ctx, Query(q): Query<ActivityQuery>) -> AppResult<Response> {
    let me = require_ucp(&ctx).await?;
    let kind = match q.kind.as_str() {
        "security" | "account" | "staff" => q.kind.clone(),
        _ => String::new(),
    };
    let base = format!("/usercp/activity?kind={kind}&page={{page}}");
    let (events, pagination) = audit_rows(&ctx, me.uid, &kind, q.page, &base, true).await?;
    let failed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_audit WHERE uid = $1 AND action IN ('login_failed', 'login_locked', 'login_2fa_failed') AND dateline > $2")
        .bind(me.uid)
        .bind(now() - 7 * 86400)
        .fetch_one(&ctx.app.db)
        .await?;
    page(&ctx, "usercp/activity.html", "activity", "Account activity", minijinja::context! { events => events, pagination => pagination, kind => kind, failed_week => failed }).await
}
