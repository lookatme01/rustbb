//! Login, registration, activation, password recovery and profiles.

use crate::audit::Actor;
use crate::auth;
use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::models::User;
use crate::templates::url_user;
use crate::usecase::accounts::{self, Activation};
use crate::util::{self, now};
use axum::Json;
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

#[derive(Deserialize, Default)]
pub struct ReturnQuery {
    #[serde(default)]
    pub return_to: String,
}

pub async fn login_form(ctx: Ctx, Query(q): Query<ReturnQuery>) -> AppResult<Response> {
    if ctx.logged_in() {
        return Ok(Redirect::to("/").into_response());
    }
    ctx.render("login.html", minijinja::context! { title => "Login", return_to => q.return_to, errors => Vec::<String>::new(), username => "" }).await
}

#[derive(Deserialize)]
pub struct LoginForm {
    #[serde(default, deserialize_with = "de::string")]
    pub username: String,
    #[serde(default, deserialize_with = "de::string")]
    pub password: String,
    #[serde(default, deserialize_with = "de::bool")]
    pub remember: bool,
    #[serde(default, deserialize_with = "de::string")]
    pub return_to: String,
}

async fn find_login_user(ctx: &Ctx, name: &str) -> AppResult<Option<User>> {
    let method = ctx.settings().get("usernamemethod");
    let q = match method {
        "1" => &format!(
            "SELECT {} FROM users WHERE lower(email) = lower($1) ORDER BY uid LIMIT 1",
            crate::models::USER_COLUMNS
        ),
        "2" => &format!(
            "SELECT {} FROM users WHERE lower(username) = lower($1) OR lower(email) = lower($1) ORDER BY (lower(username) = lower($1)) DESC, uid LIMIT 1",
            crate::models::USER_COLUMNS
        ),
        _ => &format!(
            "SELECT {} FROM users WHERE lower(username) = lower($1)",
            crate::models::USER_COLUMNS
        ),
    };
    Ok(sqlx::query_as(q)
        .bind(name.trim())
        .fetch_optional(&ctx.app.db)
        .await?)
}

/// The single message for every failed sign-in, so responses never reveal whether an account
/// exists, which password was wrong, or that an account is locked.
pub const LOGIN_FAILED: &str = "You have entered an invalid username/password combination. After several failed attempts sign-in is paused for a few minutes; you can reset your password at any time.";

/// A real argon2id hash of a random, discarded password. Verifying against it makes unknown
/// usernames cost as much as real ones (no user enumeration by timing).
async fn dummy_hash() -> &'static str {
    static H: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    H.get_or_init(|| async {
        auth::hash_password(&util::random_token(32))
            .await
            .unwrap_or_default()
    })
    .await
}

/// Check a username/password pair for the web login and the API alike: per-IP and
/// per-account throttling, IP ban filters, account lockout, constant work for unknown users,
/// failed-attempt accounting and audit logging. Returns the user on success.
pub async fn check_credentials(
    ctx: &Ctx,
    username: &str,
    password: &str,
) -> AppResult<Result<User, String>> {
    let name_key = username.trim().to_lowercase();
    if !ctx
        .app
        .throttle(&format!("login:{}", ctx.ip), 20, 300)
        .await
        || !ctx
            .app
            .throttle(&format!("login-name:{name_key}"), 30, 900)
            .await
    {
        return Ok(Err(
            "Too many login attempts. Please wait a few minutes and try again.".into(),
        ));
    }
    if auth::is_filtered(&ctx.app, 1, &ctx.ip).await? {
        return Ok(Err(
            "Your IP address has been banned from this board.".into()
        ));
    }
    let s = ctx.settings();
    let user = find_login_user(ctx, username)
        .await?
        .filter(|u| !u.is_system);
    let dummy = dummy_hash().await;
    let stored = user.as_ref().map(|u| u.password.as_str()).unwrap_or(dummy);
    let pw_ok = auth::verify_password(password, stored).await;
    let Some(user) = user else {
        return Ok(Err(LOGIN_FAILED.into()));
    };
    let max_attempts = s.int("failedlogincount");
    let locked = max_attempts > 0 && user.loginlockoutexpiry > now();
    if locked || !pw_ok {
        if !pw_ok && !locked {
            let attempts = user.loginattempts + 1;
            let lock = if max_attempts > 0 && attempts as i64 >= max_attempts {
                now() + s.int("failedlogintime").max(1) * 60
            } else {
                0
            };
            sqlx::query(
                "UPDATE users SET loginattempts = $2, loginlockoutexpiry = $3 WHERE uid = $1",
            )
            .bind(user.uid)
            .bind(if lock > 0 { 0 } else { attempts })
            .bind(lock)
            .execute(&ctx.app.db)
            .await?;
            crate::audit::log(
                ctx,
                user.uid,
                if lock > 0 {
                    "login_locked"
                } else {
                    "login_failed"
                },
                serde_json::json!({"attempts": attempts}),
            )
            .await;
        }
        return Ok(Err(LOGIN_FAILED.into()));
    }
    Ok(Ok(user))
}

pub async fn login_submit(ctx: Ctx, CsrfForm(f): CsrfForm<LoginForm>) -> AppResult<Response> {
    let render_err = |msg: String| {
        let ctx = ctx.clone();
        let (u, r) = (f.username.clone(), f.return_to.clone());
        async move {
            ctx.render_status(
                axum::http::StatusCode::UNAUTHORIZED,
                "login.html",
                minijinja::context! { title => "Login", return_to => r, errors => vec![msg], username => u },
            )
            .await
        }
    };
    let s = ctx.settings();
    let user = match check_credentials(&ctx, &f.username, &f.password).await? {
        Ok(u) => u,
        Err(msg) => return render_err(msg).await,
    };
    if auth::needs_rehash(&user.password) {
        let h = auth::hash_password(&f.password).await?;
        sqlx::query("UPDATE users SET password = $2 WHERE uid = $1")
            .bind(user.uid)
            .bind(h)
            .execute(&ctx.app.db)
            .await?;
    }
    if user.usergroup == 5 && s.get("regtype") != "instant" {
        // awaiting activation users may log in but are restricted by group permissions
    }
    if !user.totp_secret.is_empty() {
        let exp = now() + 300;
        let token = format!("{}.{}.{}", user.uid, exp, f.remember as u8);
        let sig = util::hmac_hex(
            &ctx.app.cfg.secret,
            &format!("2fa:{token}:{}", user.password),
        );
        return ctx
            .render(
                "login_2fa.html",
                minijinja::context! { title => "Two-Factor Authentication", pending => format!("{token}.{sig}"), return_to => f.return_to, errors => Vec::<String>::new() },
            )
            .await;
    }
    auth::create_login(&ctx, user.uid, f.remember).await?;
    crate::audit::log(
        &ctx,
        user.uid,
        "login",
        serde_json::json!({"remember": f.remember}),
    )
    .await;
    let to = if f.return_to.starts_with('/') && !f.return_to.starts_with("/member/") {
        f.return_to.clone()
    } else {
        "/".into()
    };
    Ok(ctx.redirect(
        &to,
        &format!(
            "You have successfully been logged in. Welcome back, {}.",
            user.username
        ),
    ))
}

#[derive(Deserialize)]
pub struct TwoFaForm {
    #[serde(default, deserialize_with = "de::string")]
    pub pending: String,
    #[serde(default, deserialize_with = "de::string")]
    pub code: String,
    #[serde(default, deserialize_with = "de::string")]
    pub return_to: String,
}

/// The TOTP time step (±1 step of clock skew) that `code` is valid for, if any.
pub fn totp_step(secret: &str, code: &str) -> Option<i64> {
    let bytes = totp_rs::Secret::Encoded(secret.to_string())
        .to_bytes()
        .ok()?;
    let t = totp_rs::TOTP::new(
        totp_rs::Algorithm::SHA1,
        6,
        1,
        30,
        bytes,
        Some("rbb".into()),
        "user".into(),
    )
    .ok()?;
    let code = code.trim();
    let now = now().max(0) as u64;
    [now.saturating_sub(30), now, now + 30]
        .into_iter()
        .find(|ts| util::ct_eq(&t.generate(*ts), code))
        .map(|ts| (ts / 30) as i64)
}

pub fn totp_check(secret: &str, code: &str, _account: &str) -> bool {
    totp_step(secret, code).is_some()
}

/// Verify a user's two-factor code and burn it, so each code signs in at most once (an
/// intercepted or shoulder-surfed code can't be replayed). Attempts are limited per account.
pub async fn totp_consume(
    app: &crate::app::App,
    uid: i32,
    secret: &str,
    code: &str,
) -> AppResult<bool> {
    if !app.throttle(&format!("2fa-uid:{uid}"), 5, 300).await {
        return Err(AppError::RateLimited);
    }
    let Some(step) = totp_step(secret, code) else {
        return Ok(false);
    };
    let r =
        sqlx::query("UPDATE users SET totp_last_step = $2 WHERE uid = $1 AND totp_last_step < $2")
            .bind(uid)
            .bind(step)
            .execute(&app.db)
            .await?;
    Ok(r.rows_affected() == 1)
}

pub async fn login_2fa(ctx: Ctx, CsrfForm(f): CsrfForm<TwoFaForm>) -> AppResult<Response> {
    if !ctx.app.throttle(&format!("2fa:{}", ctx.ip), 10, 300).await {
        return Err(AppError::RateLimited);
    }
    let parts: Vec<&str> = f.pending.split('.').collect();
    if parts.len() != 4 {
        return Err(AppError::user(
            "Invalid login session. Please log in again.",
        ));
    }
    let (uid, exp, remember, sig) = (
        parts[0].parse::<i32>().unwrap_or(0),
        parts[1].parse::<i64>().unwrap_or(0),
        parts[2] == "1",
        parts[3],
    );
    if exp < now() {
        return Err(AppError::user(
            "Your login session expired. Please log in again.",
        ));
    }
    let user: User = sqlx::query_as(&format!(
        "SELECT {} FROM users WHERE uid = $1",
        crate::models::USER_COLUMNS
    ))
    .bind(uid)
    .fetch_optional(&ctx.app.db)
    .await?
    .ok_or(AppError::Csrf)?;
    let expect = util::hmac_hex(
        &ctx.app.cfg.secret,
        &format!("2fa:{}.{}.{}:{}", uid, exp, parts[2], user.password),
    );
    if !util::ct_eq(&expect, sig) {
        return Err(AppError::Csrf);
    }
    if !totp_consume(&ctx.app, uid, &user.totp_secret, &f.code).await? {
        crate::audit::log(&ctx, uid, "login_2fa_failed", serde_json::Value::Null).await;
        return ctx
            .render(
                "login_2fa.html",
                minijinja::context! { title => "Two-Factor Authentication", pending => f.pending, return_to => f.return_to, errors => vec!["The code you entered is incorrect."] },
            )
            .await;
    }
    auth::create_login(&ctx, uid, remember).await?;
    crate::audit::log(
        &ctx,
        uid,
        "login",
        serde_json::json!({"remember": remember, "twofa": true}),
    )
    .await;
    let to = if f.return_to.starts_with('/') {
        f.return_to.clone()
    } else {
        "/".into()
    };
    Ok(ctx.redirect(
        &to,
        &format!(
            "You have successfully been logged in. Welcome back, {}.",
            user.username
        ),
    ))
}

#[derive(Deserialize)]
pub struct Empty {}

pub async fn logout(ctx: Ctx, CsrfForm(_): CsrfForm<Empty>) -> AppResult<Response> {
    crate::audit::log(&ctx, ctx.uid(), "logout", serde_json::Value::Null).await;
    auth::destroy_login(&ctx).await?;
    if ctx.uid() > 0 {
        // Show the user as offline right away.
        let _ = sqlx::query("DELETE FROM sessions WHERE uid = $1")
            .bind(ctx.uid())
            .execute(&ctx.app.db)
            .await;
        let _ = sqlx::query("UPDATE users SET lastactive = $2, lastvisit = $2 WHERE uid = $1")
            .bind(ctx.uid())
            .bind(now())
            .execute(&ctx.app.db)
            .await;
    }
    Ok(ctx.redirect("/", "You have been logged out."))
}

// ---------------------------------------------------------------------------------------------
// Registration

#[derive(Deserialize, Default)]
pub struct RegisterForm {
    #[serde(default, deserialize_with = "de::string")]
    pub username: String,
    #[serde(default, deserialize_with = "de::string")]
    pub password: String,
    #[serde(default, deserialize_with = "de::string")]
    pub password2: String,
    #[serde(default, deserialize_with = "de::string")]
    pub email: String,
    #[serde(default, deserialize_with = "de::string")]
    pub email2: String,
    #[serde(default, deserialize_with = "de::string")]
    pub referrer: String,
    #[serde(default, deserialize_with = "de::string")]
    pub timezone: String,
    #[serde(default, deserialize_with = "de::bool")]
    pub agree: bool,
    #[serde(default, deserialize_with = "de::string")]
    pub captcha_hash: String,
    #[serde(default, deserialize_with = "de::string")]
    pub captcha: String,
    #[serde(default, deserialize_with = "de::i32")]
    pub question_id: i32,
    #[serde(default, deserialize_with = "de::string")]
    pub answer: String,
    /// Honeypot field; humans leave it empty.
    #[serde(default, deserialize_with = "de::string")]
    pub website: String,
    #[serde(default, deserialize_with = "de::string")]
    pub formtoken: String,
    #[serde(default, deserialize_with = "de::bool")]
    pub hideemail: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub receivepms: bool,
    #[serde(default, flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

async fn register_page(ctx: &Ctx, f: &RegisterForm, errors: Vec<String>) -> AppResult<Response> {
    let s = ctx.settings();
    let captcha = if s.get("captchaimage") == "1" {
        Some(crate::routes::captcha::new_captcha(ctx).await?)
    } else {
        None
    };
    let question: Option<(i32, String)> = if s.bool("securityquestion") {
        sqlx::query_as("SELECT qid, question FROM questions WHERE active ORDER BY random() LIMIT 1")
            .fetch_optional(&ctx.app.db)
            .await?
    } else {
        None
    };
    let fields: Vec<&crate::models::ProfileField> = ctx
        .cache
        .profilefields
        .iter()
        .filter(|p| p.registration || p.required)
        .collect();
    let token_time = now();
    let formtoken = format!(
        "{token_time}.{}",
        &util::hmac_hex(&ctx.app.cfg.secret, &format!("reg:{token_time}"))[..16]
    );
    let tos = crate::render::parse_with(
        &ctx.cache,
        &ctx.app.plugins,
        &Default::default(),
        s.get("tos"),
    );
    ctx.render(
        "register.html",
        minijinja::context! {
            title => "Register",
            form => minijinja::context! { username => &f.username, email => &f.email, email2 => &f.email2, referrer => &f.referrer, timezone => &f.timezone, hideemail => f.hideemail },
            errors => errors,
            captcha => captcha,
            question => question,
            fields => fields,
            formtoken => formtoken,
            tos => tos,
            honeypot => s.bool("honeypot"),
            timezones => timezones(),
            usereferrals => s.bool("usereferrals"),
        },
    )
    .await
}

pub fn timezones() -> Vec<&'static str> {
    chrono_tz::TZ_VARIANTS
        .iter()
        .map(|t| t.name())
        .filter(|n| n.contains('/') || *n == "UTC")
        .collect()
}

#[derive(Deserialize, Default)]
pub struct RefQuery {
    #[serde(default)]
    pub referrer: String,
}

pub async fn register_form(ctx: Ctx, Query(q): Query<RefQuery>) -> AppResult<Response> {
    if ctx.logged_in() {
        return Err(AppError::user("You are already registered and logged in."));
    }
    if ctx.settings().bool("disableregs") {
        return Err(AppError::user(
            "Sorry, but registration has been disabled by the administrator.",
        ));
    }
    let f = RegisterForm {
        referrer: q.referrer,
        receivepms: true,
        hideemail: true,
        ..Default::default()
    };
    register_page(&ctx, &f, vec![]).await
}

pub async fn register_submit(ctx: Ctx, CsrfForm(f): CsrfForm<RegisterForm>) -> AppResult<Response> {
    if ctx.logged_in() {
        return Err(AppError::user("You are already registered and logged in."));
    }
    let s = ctx.settings().clone();
    if s.bool("disableregs") {
        return Err(AppError::user(
            "Sorry, but registration has been disabled by the administrator.",
        ));
    }
    let mut errors: Vec<String> = Vec::new();
    let username = f.username.trim().to_string();
    let email = f.email.trim().to_string();

    // Anti-spam first (cheap and silent).
    let spam = |reason: &str| {
        let (u, e, ip, r) = (
            username.clone(),
            email.clone(),
            ctx.ip.clone(),
            reason.to_string(),
        );
        let db = ctx.app.db.clone();
        async move {
            let _ = sqlx::query("INSERT INTO spamlog (username, email, ipaddress, dateline, data) VALUES ($1, $2, $3, $4, $5)")
                .bind(u)
                .bind(e)
                .bind(ip)
                .bind(now())
                .bind(r)
                .execute(&db)
                .await;
        }
    };
    if s.bool("honeypot") && !f.website.is_empty() {
        spam("honeypot").await;
        return Err(AppError::user("Registration could not be completed."));
    }
    if let Some((ts, sig)) = f.formtoken.split_once('.') {
        let t: i64 = ts.parse().unwrap_or(0);
        let ok = util::ct_eq(
            &util::hmac_hex(&ctx.app.cfg.secret, &format!("reg:{t}"))[..16],
            sig,
        );
        if !ok || now() - t < s.int("minregtime") || now() - t > 86400 {
            spam("form timing").await;
            errors.push(
                "The registration form was submitted too quickly or has expired. Please try again."
                    .into(),
            );
        }
    } else {
        errors.push("Invalid registration form. Please try again.".into());
    }
    if auth::is_filtered(&ctx.app, 1, &ctx.ip).await? {
        return Err(AppError::user(
            "Your IP address has been banned from registering.",
        ));
    }
    let maxregs = s.int("maxregsbetweentime");
    if maxregs > 0 {
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE regip = $1 AND regdate > $2")
                .bind(&ctx.ip)
                .bind(now() - s.int("betweenregstime").max(1) * 3600)
                .fetch_one(&ctx.app.db)
                .await?;
        if n >= maxregs {
            errors.push(format!("Sorry, but you cannot register more than {maxregs} accounts from the same IP in {} hours.", s.int("betweenregstime")));
        }
    }

    let (minl, maxl) = (
        s.int("minnamelength").max(1) as usize,
        s.int("maxnamelength").max(3) as usize,
    );
    let ulen = username.chars().count();
    if ulen < minl || ulen > maxl {
        errors.push(format!(
            "Your username must be between {minl} and {maxl} characters long."
        ));
    } else if !auth::valid_username_chars(&username) {
        errors.push("The username you entered contains invalid characters.".into());
    } else {
        let exists: Option<i32> =
            sqlx::query_scalar("SELECT uid FROM users WHERE lower(username) = lower($1)")
                .bind(&username)
                .fetch_optional(&ctx.app.db)
                .await?;
        if exists.is_some() {
            errors.push(
                "The username you have chosen is already registered. Please choose another.".into(),
            );
        } else if auth::is_filtered(&ctx.app, 2, &username).await? {
            errors.push("The username you have chosen is banned. Please choose another.".into());
        }
    }
    if !util::valid_email(&email) {
        errors.push("The email address you entered is invalid.".into());
    } else if email != f.email2.trim() {
        errors.push("The email addresses you entered do not match.".into());
    } else {
        if auth::is_filtered(&ctx.app, 3, &email).await? {
            errors.push("The email address you entered is banned.".into());
        }
        if !s.bool("allowmultipleemails") {
            let exists: Option<i32> =
                sqlx::query_scalar("SELECT uid FROM users WHERE lower(email) = lower($1) LIMIT 1")
                    .bind(&email)
                    .fetch_optional(&ctx.app.db)
                    .await?;
            if exists.is_some() {
                errors.push(
                    "The email address you entered is already in use by another member.".into(),
                );
            }
        }
    }
    let randompass = s.get("regtype") == "randompass";
    let password = if randompass {
        util::random_token(12)
    } else {
        f.password.clone()
    };
    if !randompass {
        if let Some(e) = auth::password_strength_error(&ctx, &password, &username) {
            errors.push(e);
        } else if password != f.password2 {
            errors.push("The passwords you entered do not match.".into());
        }
    }
    if !f.agree {
        errors.push("You must agree to the forum rules to register.".into());
    }
    // Profile fields
    let mut field_values: Vec<(i32, String)> = Vec::new();
    for pf in ctx
        .cache
        .profilefields
        .iter()
        .filter(|p| p.registration || p.required)
    {
        let key = format!("profile_fields[{}]", pf.fid);
        let val = match f.extra.get(&key) {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        match crate::routes::usercp::validate_profile_field(pf, &val) {
            Ok(v) => field_values.push((pf.fid, v)),
            Err(e) => errors.push(e),
        }
    }
    let referrer_uid: i32 = if s.bool("usereferrals") && !f.referrer.trim().is_empty() {
        match sqlx::query_scalar::<_, i32>(
            "SELECT uid FROM users WHERE lower(username) = lower($1)",
        )
        .bind(f.referrer.trim())
        .fetch_optional(&ctx.app.db)
        .await?
        {
            Some(u) => u,
            None => {
                errors.push("The referrer you entered does not exist.".into());
                0
            }
        }
    } else {
        0
    };
    if errors.is_empty() && s.bool("securityquestion") {
        let ans: Option<String> =
            sqlx::query_scalar("SELECT answer FROM questions WHERE qid = $1 AND active")
                .bind(f.question_id)
                .fetch_optional(&ctx.app.db)
                .await?;
        let ok = ans
            .map(|a| {
                a.lines()
                    .any(|l| l.trim().eq_ignore_ascii_case(f.answer.trim()))
            })
            .unwrap_or(false);
        let _ = sqlx::query(&format!(
            "UPDATE questions SET shown = shown + 1, {} WHERE qid = $1",
            if ok {
                "correct = correct + 1"
            } else {
                "incorrect = incorrect + 1"
            }
        ))
        .bind(f.question_id)
        .execute(&ctx.app.db)
        .await;
        if !ok {
            errors.push("The answer to the security question is incorrect.".into());
        }
    }
    if errors.is_empty()
        && s.get("captchaimage") == "1"
        && let Err(e) = crate::routes::captcha::check(&ctx, &f.captcha_hash, &f.captcha).await
    {
        errors.push(e.public_message());
    }
    if !errors.is_empty() {
        return register_page(&ctx, &f, errors).await;
    }

    let activation = Activation::from_setting(s.get("regtype"));
    let password_hash = auth::hash_password(&password).await?;
    let timezone = if f.timezone.is_empty() || f.timezone.parse::<chrono_tz::Tz>().is_err() {
        String::new()
    } else {
        f.timezone.clone()
    };
    let account = accounts::NewAccount {
        username: username.clone(),
        email: email.clone(),
        password,
        password_hash,
        timezone,
        referrer_uid,
        hideemail: f.hideemail,
        fields: field_values,
        activation,
    };
    let uid = match accounts::register(&ctx.app, &Actor::from_ctx(&ctx), account).await? {
        Ok(uid) => uid,
        Err(accounts::RegisterError::UsernameTaken) => {
            return register_page(
                &ctx,
                &f,
                vec!["The username you have chosen is already registered.".into()],
            )
            .await;
        }
    };
    let bbname = s.get("bbname").to_string();
    match activation {
        Activation::Email { .. } => {
            auth::create_login(&ctx, uid, false).await?;
            Ok(ctx.redirect("/", "Thank you for registering. An activation email has been sent — please follow the link in it to activate your account."))
        }
        Activation::Admin => Ok(ctx.redirect("/", "Thank you for registering. Your account must be activated by an administrator before you can post.")),
        Activation::RandomPassword => Ok(ctx.redirect(
            "/member/login",
            "Thank you for registering. Your password has been emailed to you.",
        )),
        Activation::Instant => {
            auth::create_login(&ctx, uid, true).await?;
            Ok(ctx.redirect(
                "/",
                &format!(
                    "Thank you for registering on {bbname}, {username}. You are now logged in."
                ),
            ))
        }
    }
}

#[derive(Deserialize, Default)]
pub struct ActivateQuery {
    #[serde(default)]
    pub uid: i32,
    #[serde(default)]
    pub code: String,
}

pub async fn activate(ctx: Ctx, Query(q): Query<ActivateQuery>) -> AppResult<Response> {
    if q.uid == 0 || q.code.is_empty() {
        return ctx
            .render(
                "activate.html",
                minijinja::context! { title => "Activate Account" },
            )
            .await;
    }
    Ok(
        match accounts::activate(&ctx.app, &Actor::from_ctx(&ctx), q.uid, &q.code).await? {
            accounts::Activated::EmailChanged => {
                ctx.redirect("/usercp", "Your new email address has been confirmed.")
            }
            accounts::Activated::AwaitingAdmin => ctx.redirect("/", "Your email address has been verified. An administrator must now activate your account."),
            accounts::Activated::Active => {
                ctx.redirect("/", "Your account has been activated. Welcome!")
            }
        },
    )
}

#[derive(Deserialize)]
pub struct EmailOnly {
    #[serde(default, deserialize_with = "de::string")]
    pub email: String,
}

pub async fn resend_form(ctx: Ctx) -> AppResult<Response> {
    ctx.render(
        "resend_activation.html",
        minijinja::context! { title => "Resend Activation Email" },
    )
    .await
}

pub async fn resend_submit(ctx: Ctx, CsrfForm(f): CsrfForm<EmailOnly>) -> AppResult<Response> {
    if !ctx
        .app
        .throttle(&format!("resend:{}", ctx.ip), 3, 3600)
        .await
    {
        return Err(AppError::RateLimited);
    }
    accounts::resend_activation(&ctx.app, f.email.trim()).await?;
    Ok(ctx.redirect("/", "If an account awaiting activation exists for that email address, the activation email has been resent."))
}

pub async fn lostpw_form(ctx: Ctx) -> AppResult<Response> {
    ctx.render(
        "lostpw.html",
        minijinja::context! { title => "Lost Password Recovery" },
    )
    .await
}

pub async fn lostpw_submit(ctx: Ctx, CsrfForm(f): CsrfForm<EmailOnly>) -> AppResult<Response> {
    if !ctx
        .app
        .throttle(&format!("lostpw:{}", ctx.ip), 5, 3600)
        .await
    {
        return Err(AppError::RateLimited);
    }
    accounts::request_password_reset(&ctx.app, &Actor::from_ctx(&ctx), f.email.trim()).await?;
    Ok(ctx.redirect(
        "/",
        "If an account exists with that email address, a password reset link has been sent to it.",
    ))
}

pub async fn resetpw_form(ctx: Ctx, Query(q): Query<ActivateQuery>) -> AppResult<Response> {
    ctx.render("resetpw.html", minijinja::context! { title => "Reset Password", uid => q.uid, code => q.code, errors => Vec::<String>::new() }).await
}

#[derive(Deserialize)]
pub struct ResetForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub uid: i32,
    #[serde(default, deserialize_with = "de::string")]
    pub code: String,
    #[serde(default, deserialize_with = "de::string")]
    pub password: String,
    #[serde(default, deserialize_with = "de::string")]
    pub password2: String,
}

pub async fn resetpw_submit(ctx: Ctx, CsrfForm(f): CsrfForm<ResetForm>) -> AppResult<Response> {
    if !ctx
        .app
        .throttle(&format!("resetpw:{}", ctx.ip), 10, 3600)
        .await
        || !ctx
            .app
            .throttle(&format!("resetpw-uid:{}", f.uid), 10, 3600)
            .await
    {
        return Err(AppError::RateLimited);
    }
    let Some(username) = accounts::reset_code_valid(&ctx.app, f.uid, &f.code).await? else {
        return Err(AppError::user(
            "The password reset link is invalid or has expired. Please request a new one.",
        ));
    };
    let err = auth::password_strength_error(&ctx, &f.password, &username).or_else(|| {
        (f.password != f.password2).then(|| "The passwords you entered do not match.".to_string())
    });
    if let Some(e) = err {
        return ctx.render("resetpw.html", minijinja::context! { title => "Reset Password", uid => f.uid, code => f.code, errors => vec![e] }).await;
    }
    // Hash before the transaction: it is slow, and the code is only consumed if all succeeds.
    let h = auth::hash_password(&f.password).await?;
    accounts::reset_password(&ctx.app, &Actor::from_ctx(&ctx), f.uid, &f.code, &h).await?;
    Ok(ctx.redirect(
        "/member/login",
        "Your password has been reset. You can now log in with your new password.",
    ))
}

#[derive(Deserialize)]
pub struct NameQuery {
    #[serde(default)]
    pub username: String,
}

pub async fn check_username(ctx: Ctx, Query(q): Query<NameQuery>) -> AppResult<Response> {
    if !ctx.app.rate_check(&format!("checkname:{}", ctx.ip), 60, 60) {
        return Err(AppError::RateLimited);
    }
    let name = q.username.trim();
    let taken: Option<i32> =
        sqlx::query_scalar("SELECT uid FROM users WHERE lower(username) = lower($1)")
            .bind(name)
            .fetch_optional(&ctx.app.db)
            .await?;
    let valid = auth::valid_username_chars(name);
    Ok(
        Json(serde_json::json!({"available": taken.is_none() && valid, "valid": valid}))
            .into_response(),
    )
}

// ---------------------------------------------------------------------------------------------
// Profiles

pub async fn profile_by_name(ctx: Ctx, Path(name): Path<String>) -> AppResult<Response> {
    let uid: Option<i32> =
        sqlx::query_scalar("SELECT uid FROM users WHERE lower(username) = lower($1)")
            .bind(&name)
            .fetch_optional(&ctx.app.db)
            .await?;
    match uid {
        Some(u) => Ok(Redirect::to(&url_user(u as i64, Some(&name))).into_response()),
        None => Err(AppError::not_found("user")),
    }
}

pub async fn profile(ctx: Ctx, Path(seg): Path<String>) -> AppResult<Response> {
    let uid = util::leading_id(&seg).ok_or_else(|| AppError::not_found("user"))?;
    if !ctx.perms.canviewprofiles {
        return Err(AppError::no_perm());
    }
    let mut user: User = sqlx::query_as(&format!(
        "SELECT {} FROM users WHERE uid = $1",
        crate::models::USER_COLUMNS
    ))
    .bind(uid)
    .fetch_optional(&ctx.app.db)
    .await?
    .ok_or_else(|| AppError::not_found("user"))?;
    if user.is_system {
        // Always online: System has no session, so its activity is "now".
        user.lastactive = now();
    }
    let s = ctx.settings();
    let author = crate::render::load_authors(&ctx, &[uid])
        .await?
        .remove(&uid)
        .unwrap_or_default();
    let totals = crate::routes::index::board_stats(&ctx).await?;
    let total_posts = totals["posts"].as_i64().unwrap_or(0).max(1);
    let total_threads = totals["threads"].as_i64().unwrap_or(0).max(1);
    let days = ((now() - user.regdate) as f64 / 86400.0).max(1.0);
    let group_title = ctx
        .cache
        .group(user.display_group())
        .map(|g| g.title.clone())
        .unwrap_or_default();
    // Custom profile fields
    let values: Vec<(i32, String)> =
        sqlx::query_as("SELECT fid, value FROM userfields WHERE uid = $1")
            .bind(uid)
            .fetch_all(&ctx.app.db)
            .await?;
    let fields: Vec<(String, String)> = ctx
        .cache
        .profilefields
        .iter()
        .filter(|f| {
            f.profile
                && (f.viewableby.is_empty() || f.viewableby.iter().any(|g| ctx.groups.contains(g)))
        })
        .filter_map(|f| {
            let v = values
                .iter()
                .find(|(fid, _)| *fid == f.fid)
                .map(|(_, v)| v.clone())
                .filter(|v| !v.is_empty())?;
            let html = if f.allowmycode || f.allowsmilies {
                let opts = crate::parser::ParseOptions {
                    allow_mycode: f.allowmycode,
                    allow_smilies: f.allowsmilies,
                    allow_imgcode: false,
                    allow_videocode: false,
                    ..Default::default()
                };
                crate::render::parse_with(&ctx.cache, &ctx.app.plugins, &opts, &v)
            } else {
                util::escape_html(&v).replace('\n', "<br />")
            };
            Some((f.name.clone(), html))
        })
        .collect();
    // Most active forum
    let active_forum: Option<(i32, i64)> = sqlx::query_as("SELECT fid, COUNT(*) c FROM posts WHERE uid = $1 AND visible = 1 GROUP BY fid ORDER BY c DESC LIMIT 1")
        .bind(uid)
        .fetch_optional(&ctx.app.db)
        .await?;
    let active_forum = active_forum.and_then(|(fid, c)| {
        let f = ctx.cache.forum(fid)?;
        ctx.access().can_see(fid).then(|| minijinja::context! { fid => fid, name => &f.name, count => c, url => crate::templates::url_forum(fid as i64, Some(&f.name)) })
    });
    let online = author.online;
    let is_buddy = ctx
        .user
        .as_ref()
        .map(|u| u.buddylist.contains(&uid))
        .unwrap_or(false);
    let is_ignored = ctx
        .user
        .as_ref()
        .map(|u| u.ignorelist.contains(&uid))
        .unwrap_or(false);
    let lastvisit_visible = !user.invisible || ctx.perms.canviewwolinvis || uid == ctx.uid();
    let age = if user.birthdayprivacy == "all" {
        util::age_from_birthday(&user.birthday, ctx.tz)
    } else {
        None
    };
    let birthday = if user.birthdayprivacy == "none" || user.birthday.is_empty() {
        None
    } else {
        Some(user.birthday.clone())
    };
    let warnings_visible = s.bool("enablewarningsystem")
        && (ctx.perms.canwarnusers || (uid == ctx.uid() && s.bool("canviewownwarning")));
    let location: Option<String> = sqlx::query_scalar(
        "SELECT location FROM sessions WHERE uid = $1 ORDER BY time DESC LIMIT 1",
    )
    .bind(uid)
    .fetch_optional(&ctx.app.db)
    .await?;
    let is_mod_viewer = ctx.staff().is_moderator();
    // Identity key (end-to-end private messages), and whether the viewer has verified it.
    let pgp_key = if ctx.settings().bool("enablepms") {
        crate::routes::pgp::active_key(&ctx, user.uid).await?
    } else {
        None
    };
    let pgp_verified: bool = match (&pgp_key, ctx.uid()) {
        (Some(k), me) if me > 0 && me != user.uid => sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pgp_verifications WHERE verifier = $1 AND subject = $2 AND subject_fpr = $3)",
        )
        .bind(me)
        .bind(user.uid)
        .bind(&k.fingerprint)
        .fetch_one(&ctx.app.db)
        .await?,
        _ => false,
    };
    let pgp = pgp_key.map(|k| {
        let groups: Vec<String> = k.fingerprint.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
        minijinja::context! { fingerprint => k.fingerprint, groups => groups, algorithm => k.algorithm, added => k.added, verified => pgp_verified }
    });
    // Active ban: shown to everyone; who issued it only to staff.
    let ban: Option<(String, i64, i64, i32, Option<String>)> = sqlx::query_as(
        "SELECT b.reason, b.dateline, b.lifted, b.admin, a.username FROM banned b LEFT JOIN users a ON a.uid = b.admin
         WHERE b.uid = $1 AND (b.lifted = 0 OR b.lifted > $2)",
    )
    .bind(user.uid)
    .bind(now())
    .fetch_optional(&ctx.app.db)
    .await?;
    let is_staff = ctx.uid() > 0
        && (ctx.can(crate::domain::staff::Cap::ModCp) || ctx.can(crate::domain::staff::Cap::Ban));
    let ban = ban.map(|(reason, since, until, by_uid, by)| {
        let by = by.filter(|_| is_staff);
        minijinja::context! { reason => reason, since => since, until => until, by_uid => if by.is_some() { by_uid } else { 0 }, by => by }
    });
    let history_notes = if ctx.uid() > 0 && ctx.can(crate::domain::staff::Cap::ReadModNotes) {
        Some(crate::routes::modnotes::note_count(&ctx.app, user.uid).await?)
    } else {
        None
    };
    let system_activity = if user.is_system {
        Some(system_activity(&ctx, user.uid).await?)
    } else {
        None
    };
    let additional: Vec<String> = user
        .additionalgroups
        .iter()
        .filter_map(|g| ctx.cache.group(*g).map(|g| g.title.clone()))
        .collect();
    ctx.allow_guest_cache(&["board".to_string()]);
    ctx.render(
        "profile.html",
        minijinja::context! {
            title => format!("Profile of {}", user.username),
            user => &user,
            author => author,
            group_title => group_title,
            additional_groups => additional,
            pgp => pgp,
            posts_per_day => format!("{:.2}", user.postnum as f64 / days),
            posts_percent => format!("{:.2}", user.postnum as f64 * 100.0 / total_posts as f64),
            threads_per_day => format!("{:.2}", user.threadnum as f64 / days),
            threads_percent => format!("{:.2}", user.threadnum as f64 * 100.0 / total_threads as f64),
            fields => fields,
            active_forum => active_forum,
            online => online,
            is_buddy => is_buddy,
            is_ignored => is_ignored,
            lastvisit_visible => lastvisit_visible,
            age => age,
            birthday => birthday,
            warnings_visible => warnings_visible,
            warn_level => author_warn(&ctx, user.warningpoints),
            location => if user.is_system && ctx.perms.canviewonline { Some(crate::system::ONLINE_LOCATION.to_string()) } else if online && (ctx.perms.canviewonline) { location.map(|l| crate::routes::online::describe_location(&ctx, &l)) } else { None },
            can_email => !user.is_system && ctx.perms.cansendemail && (!user.hideemail || ctx.perms.cansendemailoverride) && ctx.uid() > 0,
            can_pm => ctx.perms.canusepms && ctx.settings().bool("enablepms") && user.receivepms && ctx.uid() > 0,
            can_rep => s.bool("enablereputation") && ctx.perms.cangivereputations && uid != ctx.uid() && ctx.uid() > 0,
            is_mod_viewer => is_mod_viewer,
            can_edit_profile => ctx.perms.caneditprofiles,
            can_ban => ctx.perms.canbanusers,
            can_warn => ctx.perms.canwarnusers && uid != ctx.uid() && s.bool("enablewarningsystem"),
            regip => if ctx.perms.canuseipsearch { Some(user.regip.clone()) } else { None },
            lastip => if ctx.perms.canuseipsearch { Some(user.lastip.clone()) } else { None },
            // System never makes requests, but it is always online.
            timeonline => format_duration(if user.is_system { now() - user.regdate } else { user.timeonline }),
            system_activity => system_activity,
            ban => ban,
            history_notes => history_notes,
        },
    )
    .await
}

fn author_warn(ctx: &Ctx, points: i32) -> i64 {
    (points as i64 * 100 / ctx.settings().int("maxwarningpoints").max(1)).min(100)
}

/// What the System account has done, for its profile. Counts are public; the latest moderator-log
/// entries only go to viewers who can read the moderator log.
async fn system_activity(ctx: &Ctx, uid: i32) -> AppResult<minijinja::Value> {
    let (quarantined, messages, closed, staff_posts): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM automod_actions WHERE status <> 'observed'),
                (SELECT COUNT(*) FROM privatemessages WHERE fromid = $1),
                (SELECT COUNT(*) FROM moderatorlog WHERE uid = $1 AND action LIKE 'Thread closed (no posts%'),
                (SELECT COUNT(*) FROM system_authorship WHERE kind IN ('thread', 'post', 'announcement'))",
    )
    .bind(uid)
    .fetch_one(&ctx.app.db)
    .await?;
    let can_see_log = ctx.uid() > 0 && ctx.can(crate::domain::staff::Cap::ModLog);
    let recent = if can_see_log {
        Some(crate::routes::modcp::load_logs(ctx, uid, 0, 10, 0).await?)
    } else {
        None
    };
    Ok(
        minijinja::context! { quarantined => quarantined, messages => messages, closed => closed, staff_posts => staff_posts, recent => recent },
    )
}

pub fn format_duration(secs: i64) -> String {
    let secs = secs.max(0);
    let (d, h, m) = (secs / 86400, (secs % 86400) / 3600, (secs % 3600) / 60);
    let mut parts = vec![];
    if d > 0 {
        parts.push(format!("{d} day{}", if d == 1 { "" } else { "s" }));
    }
    if h > 0 {
        parts.push(format!("{h} hour{}", if h == 1 { "" } else { "s" }));
    }
    if m > 0 || parts.is_empty() {
        parts.push(format!("{m} minute{}", if m == 1 { "" } else { "s" }));
    }
    parts.join(", ")
}

pub async fn referrals(ctx: Ctx, Path(uid): Path<i32>) -> AppResult<Response> {
    if !ctx.perms.canviewprofiles {
        return Err(AppError::no_perm());
    }
    let rows: Vec<(i32, String, i32, i32, i64)> = sqlx::query_as(
        "SELECT uid, username, usergroup, displaygroup, regdate FROM users WHERE referrer = $1 ORDER BY regdate DESC LIMIT 500",
    )
    .bind(uid)
    .fetch_all(&ctx.app.db)
    .await?;
    let list: Vec<_> = rows
        .into_iter()
        .map(|(u, n, g, d, r)| minijinja::context! { uid => u, formatted => ctx.cache.format_name(&n, g, d), username => n, regdate => r })
        .collect();
    let name: String = sqlx::query_scalar("SELECT username FROM users WHERE uid = $1")
        .bind(uid)
        .fetch_optional(&ctx.app.db)
        .await?
        .ok_or_else(|| AppError::not_found("user"))?;
    ctx.render("referrals.html", minijinja::context! { title => format!("Referrals of {name}"), username => name, uid => uid, list => list }).await
}

pub async fn email_form(ctx: Ctx, Path(uid): Path<i32>) -> AppResult<Response> {
    ctx.require_login()?;
    if !ctx.perms.cansendemail {
        return Err(AppError::no_perm());
    }
    let (username, hideemail): (String, bool) =
        sqlx::query_as("SELECT username, hideemail FROM users WHERE uid = $1")
            .bind(uid)
            .fetch_optional(&ctx.app.db)
            .await?
            .ok_or_else(|| AppError::not_found("user"))?;
    crate::system::guard(&ctx.cache, uid, "emailed")?;
    if hideemail && !ctx.perms.cansendemailoverride {
        return Err(AppError::user(
            "This user has chosen not to receive emails from other members.",
        ));
    }
    ctx.render("email_user.html", minijinja::context! { title => format!("Send Email to {username}"), username => username, uid => uid }).await
}

#[derive(Deserialize)]
pub struct EmailForm {
    #[serde(default, deserialize_with = "de::string")]
    pub subject: String,
    #[serde(default, deserialize_with = "de::string")]
    pub message: String,
}

pub async fn email_submit(
    ctx: Ctx,
    Path(uid): Path<i32>,
    CsrfForm(f): CsrfForm<EmailForm>,
) -> AppResult<Response> {
    let me = ctx.require_login()?.clone();
    if !ctx.perms.cansendemail {
        return Err(AppError::no_perm());
    }
    let (username, email, hideemail): (String, String, bool) =
        sqlx::query_as("SELECT username, email, hideemail FROM users WHERE uid = $1")
            .bind(uid)
            .fetch_optional(&ctx.app.db)
            .await?
            .ok_or_else(|| AppError::not_found("user"))?;
    crate::system::guard(&ctx.cache, uid, "emailed")?;
    if hideemail && !ctx.perms.cansendemailoverride {
        return Err(AppError::user(
            "This user has chosen not to receive emails from other members.",
        ));
    }
    if f.subject.trim().is_empty() || f.message.trim().is_empty() {
        return Err(AppError::user("Please enter both a subject and a message."));
    }
    // Enforced in memory too: the maillogs count below only works when mail logging is on.
    let daily = if ctx.perms.maxemails > 0 {
        ctx.perms.maxemails as u32
    } else {
        50
    };
    if !ctx
        .app
        .throttle(&format!("useremail:{}", me.uid), daily, 86400)
        .await
    {
        return Err(AppError::user(format!(
            "You may only send {daily} emails per day."
        )));
    }
    if ctx.perms.maxemails > 0 {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM maillogs WHERE fromuid = $1 AND dateline > $2",
        )
        .bind(me.uid)
        .bind(now() - 86400)
        .fetch_one(&ctx.app.db)
        .await?;
        if n >= ctx.perms.maxemails as i64 {
            return Err(AppError::user(format!(
                "You may only send {} emails per day.",
                ctx.perms.maxemails
            )));
        }
    }
    let s = ctx.settings();
    let body = format!(
        "{}\n\n------------------------------------------\nThis message was sent by {} via {} ({}). Reply by visiting their profile: {}/user/{}\n",
        f.message.trim(),
        me.username,
        s.get("bbname"),
        s.get("bburl"),
        s.get("bburl").trim_end_matches('/'),
        me.uid
    );
    crate::mail::queue(&ctx.app, &email, f.subject.trim(), &body).await;
    if s.bool("mail_logging") {
        sqlx::query("INSERT INTO maillogs (subject, message, dateline, fromuid, fromemail, touid, toemail, ipaddress, type) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 1)")
            .bind(f.subject.trim())
            .bind(f.message.trim())
            .bind(now())
            .bind(me.uid)
            .bind(&me.email)
            .bind(uid)
            .bind(&email)
            .bind(&ctx.ip)
            .execute(&ctx.app.db)
            .await?;
    }
    Ok(ctx.redirect(
        &url_user(uid as i64, Some(&username)),
        "Your email has been sent.",
    ))
}

#[cfg(test)]
mod duration_tests {
    use super::format_duration;

    #[test]
    fn zero_time_is_not_reported_as_hidden() {
        assert_eq!(format_duration(0), "0 minutes");
        assert_eq!(format_duration(-5), "0 minutes");
        assert_eq!(format_duration(90061), "1 day, 1 hour, 1 minute");
    }
}
