//! Account lifecycle: registration, activation, password reset and email change.
//!
//! One-time codes are stored as SHA-256 hashes, one per member and kind, and are consumed with
//! `DELETE … RETURNING` inside the transaction that acts on them, so two concurrent requests
//! with the same code cannot both succeed.

use super::Uow;
use crate::app::App;
use crate::audit::Actor;
use crate::error::{AppError, AppResult};
use crate::infra::outbox::Job;
use crate::util::{self, now};

/// How long a password reset link works.
pub const RESET_TTL_SECS: i64 = 86400;

/// What registration does once the account exists (the board's `regtype`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activation {
    /// Active at once.
    Instant,
    /// Emailed link (`verify`), then possibly an administrator too (`both`).
    Email { then_admin: bool },
    /// An administrator activates.
    Admin,
    /// Active at once; the generated password is emailed.
    RandomPassword,
}

impl Activation {
    pub fn from_setting(regtype: &str) -> Activation {
        match regtype {
            "verify" => Activation::Email { then_admin: false },
            "both" => Activation::Email { then_admin: true },
            "admin" => Activation::Admin,
            "randompass" => Activation::RandomPassword,
            _ => Activation::Instant,
        }
    }

    /// The usergroup a new account starts in: Registered (2) or Awaiting Activation (5).
    pub fn initial_group(self) -> i32 {
        match self {
            Activation::Instant | Activation::RandomPassword => 2,
            _ => 5,
        }
    }
}

pub struct NewAccount {
    pub username: String,
    pub email: String,
    /// The password, needed to email it for `RandomPassword`.
    pub password: String,
    pub password_hash: String,
    pub timezone: String,
    pub referrer_uid: i32,
    pub hideemail: bool,
    /// Custom profile field values (field id, value).
    pub fields: Vec<(i32, String)>,
    pub activation: Activation,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RegisterError {
    UsernameTaken,
}

/// Board name and URL for emails.
fn board(app: &App) -> (String, String) {
    let c = app.cache();
    (
        c.settings.get("bbname").to_string(),
        c.settings.get("bburl").trim_end_matches('/').to_string(),
    )
}

/// Store a fresh one-time code of `kind` for `uid` (replacing any older one). Returns the code.
async fn issue_code(
    uow: &mut Uow,
    uid: i32,
    kind: &str,
    misc: &str,
    len: usize,
) -> AppResult<String> {
    let code = util::random_token(len);
    sqlx::query(
        "INSERT INTO awaitingactivation (uid, dateline, code, type, misc) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (uid, type) DO UPDATE SET dateline = EXCLUDED.dateline, code = EXCLUDED.code, misc = EXCLUDED.misc",
    )
    .bind(uid)
    .bind(now())
    .bind(util::sha256_hex(&code))
    .bind(kind)
    .bind(misc)
    .execute(uow.conn())
    .await?;
    Ok(code)
}

fn activation_mail(username: &str, uid: i32, code: &str, bbname: &str, bburl: &str) -> String {
    format!(
        "{username},\n\nTo complete the registration process on {bbname}, you will need to go to the URL below in your web browser.\n\n{bburl}/member/activate?uid={uid}&code={code}\n\nIf the above link does not work correctly, go to\n{bburl}/member/activate\nand enter your username and activation code: {code}\n\nThank you,\n{bbname} Staff"
    )
}

/// Create an account with everything that belongs to it in one transaction: the user row,
/// profile fields, referral and board counters, the audit record, the activation code and email,
/// and (after commit) the welcome PM and `user_registered` hook.
pub async fn register(
    app: &App,
    actor: &Actor,
    a: NewAccount,
) -> AppResult<Result<i32, RegisterError>> {
    let group = a.activation.initial_group();
    let t = now();
    let mut uow = Uow::begin(app).await?;
    let uid: i32 = match sqlx::query_scalar(
        "INSERT INTO users (username, password, email, usergroup, regdate, lastactive, lastvisit, regip, lastip, timezone, referrer, hideemail, receivepms, pmfolders)
         VALUES ($1, $2, $3, $4, $5, $5, $5, $6, $6, $7, $8, $9, TRUE, '[]') RETURNING uid",
    )
    .bind(&a.username)
    .bind(&a.password_hash)
    .bind(&a.email)
    .bind(group)
    .bind(t)
    .bind(crate::util::IpText::from(&actor.ip))
    .bind(&a.timezone)
    .bind(a.referrer_uid)
    .bind(a.hideemail)
    .fetch_one(uow.conn())
    .await
    {
        Ok(u) => u,
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Ok(Err(RegisterError::UsernameTaken));
        }
        Err(e) => return Err(e.into()),
    };
    for (fid, v) in &a.fields {
        sqlx::query("INSERT INTO userfields (uid, fid, value) VALUES ($1, $2, $3)")
            .bind(uid)
            .bind(fid)
            .bind(v)
            .execute(uow.conn())
            .await?;
    }
    if a.referrer_uid > 0 {
        sqlx::query("UPDATE users SET referrals = referrals + 1 WHERE uid = $1")
            .bind(a.referrer_uid)
            .execute(uow.conn())
            .await?;
    }
    sqlx::query(
        "UPDATE counters SET numusers = numusers + 1, lastuid = $1, lastusername = $2 WHERE id = 1",
    )
    .bind(uid)
    .bind(&a.username)
    .execute(uow.conn())
    .await?;
    // The new member is the actor of their own registration.
    let me = Actor {
        uid,
        ..actor.clone()
    };
    uow.audit(
        &me,
        uid,
        "registered",
        serde_json::json!({"username": a.username}),
    )
    .await?;
    let (bbname, bburl) = board(app);
    match a.activation {
        Activation::Email { .. } => {
            let code = issue_code(&mut uow, uid, "r", "", 20).await?;
            let body = activation_mail(&a.username, uid, &code, &bbname, &bburl);
            uow.mail(&a.email, &format!("Account Activation at {bbname}"), &body)
                .await?;
        }
        Activation::Admin => {
            issue_code(&mut uow, uid, "b", "", 20).await?;
        }
        Activation::RandomPassword => {
            let body = format!(
                "{},\n\nThank you for registering on {bbname}. Your login details are:\n\nUsername: {}\nPassword: {}\n\nPlease change your password after logging in.\n\n{bburl}/member/login\n",
                a.username, a.username, a.password
            );
            uow.mail(&a.email, &format!("Your Password for {bbname}"), &body)
                .await?;
        }
        Activation::Instant => {}
    }
    uow.hook(
        "user_registered",
        serde_json::json!({"uid": uid, "username": a.username}),
    );
    if group == 2 {
        uow.job(Job::WelcomePm {
            members: vec![(uid, a.username.clone())],
        });
    }
    uow.commit(app).await?;
    app.stats_cache.invalidate(&"boardstats");
    Ok(Ok(uid))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Activated {
    /// The account is active.
    Active,
    /// The email address is confirmed; an administrator must still activate the account.
    AwaitingAdmin,
    /// A new email address was confirmed.
    EmailChanged,
}

/// Use an emailed activation or email-change code. The code is consumed in the same
/// transaction as its effect, so it works exactly once.
pub async fn activate(app: &App, actor: &Actor, uid: i32, code: &str) -> AppResult<Activated> {
    let mut uow = Uow::begin(app).await?;
    let row: Option<(String, String)> = sqlx::query_as(
        "DELETE FROM awaitingactivation WHERE uid = $1 AND code = $2 AND type IN ('r', 'e') RETURNING type, misc",
    )
    .bind(uid)
    .bind(util::sha256_hex(code))
    .fetch_optional(uow.conn())
    .await?;
    let Some((kind, misc)) = row else {
        return Err(AppError::user(
            "The activation code you entered is invalid.",
        ));
    };
    if kind == "e" {
        sqlx::query("UPDATE users SET email = $2 WHERE uid = $1")
            .bind(uid)
            .bind(&misc)
            .execute(uow.conn())
            .await?;
        uow.audit(
            actor,
            uid,
            "email_changed",
            serde_json::json!({"confirmed": true}),
        )
        .await?;
        uow.commit(app).await?;
        return Ok(Activated::EmailChanged);
    }
    if app.cache().settings.get("regtype") == "both" {
        issue_code(&mut uow, uid, "b", "", 20).await?;
        uow.commit(app).await?;
        return Ok(Activated::AwaitingAdmin);
    }
    let name: Option<String> = sqlx::query_scalar(
        "UPDATE users SET usergroup = 2 WHERE uid = $1 AND usergroup = 5 RETURNING username",
    )
    .bind(uid)
    .fetch_optional(uow.conn())
    .await?;
    uow.audit(actor, uid, "activated", serde_json::Value::Null)
        .await?;
    if let Some(name) = name {
        uow.job(Job::WelcomePm {
            members: vec![(uid, name)],
        });
    }
    uow.commit(app).await?;
    Ok(Activated::Active)
}

/// Email a fresh activation link to every account awaiting email activation at `email`.
/// (Codes are stored hashed, so the old one cannot be resent; it is replaced.)
pub async fn resend_activation(app: &App, email: &str) -> AppResult<()> {
    let mut uow = Uow::begin(app).await?;
    let rows: Vec<(i32, String)> = sqlx::query_as(
        "SELECT u.uid, u.username FROM users u JOIN awaitingactivation a ON a.uid = u.uid AND a.type = 'r'
         WHERE lower(u.email) = lower($1) AND u.usergroup = 5 FOR UPDATE OF a",
    )
    .bind(email)
    .fetch_all(uow.conn())
    .await?;
    let (bbname, bburl) = board(app);
    for (uid, username) in rows {
        let code = issue_code(&mut uow, uid, "r", "", 20).await?;
        let body = activation_mail(&username, uid, &code, &bbname, &bburl);
        uow.mail(email, &format!("Account Activation at {bbname}"), &body)
            .await?;
    }
    uow.commit(app).await
}

/// Send password reset links to the accounts at `email` (none is not an error, so the
/// response does not reveal which addresses are registered).
pub async fn request_password_reset(app: &App, actor: &Actor, email: &str) -> AppResult<()> {
    let mut uow = Uow::begin(app).await?;
    let users: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT uid, username, email FROM users WHERE lower(email) = lower($1) AND NOT is_system",
    )
    .bind(email)
    .fetch_all(uow.conn())
    .await?;
    let (bbname, bburl) = board(app);
    for (uid, username, to) in users {
        let code = issue_code(&mut uow, uid, "p", "", 30).await?;
        uow.audit(
            actor,
            uid,
            "password_reset_requested",
            serde_json::Value::Null,
        )
        .await?;
        let body = format!(
            "{username},\n\nSomeone (hopefully you) requested a password reset for your account at {bbname}.\n\nTo reset your password, visit the following link within 24 hours:\n\n{bburl}/member/resetpw?uid={uid}&code={code}\n\nIf you did not request this, you can ignore this email.\n"
        );
        uow.mail(&to, &format!("Password Reset at {bbname}"), &body)
            .await?;
    }
    uow.commit(app).await
}

/// Whether a reset link is (still) valid, for showing the form. Does not consume it.
pub async fn reset_code_valid(app: &App, uid: i32, code: &str) -> AppResult<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT u.username FROM awaitingactivation a JOIN users u ON u.uid = a.uid
         WHERE a.uid = $1 AND a.type = 'p' AND a.code = $2 AND a.dateline > $3",
    )
    .bind(uid)
    .bind(util::sha256_hex(code))
    .bind(now() - RESET_TTL_SECS)
    .fetch_optional(&app.db)
    .await?)
}

/// Set a new password with a reset code: the code is consumed, the password replaced, every
/// existing sign-in ended and the reset audited, all in one transaction.
/// `password_hash` must already be computed (hashing is slow and must not hold the transaction).
pub async fn reset_password(
    app: &App,
    actor: &Actor,
    uid: i32,
    code: &str,
    password_hash: &str,
) -> AppResult<()> {
    let mut uow = Uow::begin(app).await?;
    let consumed: Option<i64> = sqlx::query_scalar(
        "DELETE FROM awaitingactivation WHERE uid = $1 AND type = 'p' AND code = $2 RETURNING dateline",
    )
    .bind(uid)
    .bind(util::sha256_hex(code))
    .fetch_optional(uow.conn())
    .await?;
    if !consumed.is_some_and(|d| d > now() - RESET_TTL_SECS) {
        // An expired code is still consumed; rolling back keeps it for the cleanup task instead.
        return Err(AppError::user(
            "The password reset link is invalid or has expired. Please request a new one.",
        ));
    }
    sqlx::query(
        "UPDATE users SET password = $2, loginattempts = 0, loginlockoutexpiry = 0, session_version = session_version + 1 WHERE uid = $1",
    )
    .bind(uid)
    .bind(password_hash)
    .execute(uow.conn())
    .await?;
    sqlx::query("DELETE FROM logins WHERE uid = $1")
        .bind(uid)
        .execute(uow.conn())
        .await?;
    sqlx::query("UPDATE api_tokens SET revoked_at = now() WHERE uid = $1 AND revoked_at IS NULL")
        .bind(uid)
        .execute(uow.conn())
        .await?;
    uow.audit(actor, uid, "password_reset", serde_json::Value::Null)
        .await?;
    uow.commit(app).await
}

/// Start an email change that must be confirmed from the new address.
pub async fn request_email_change(
    app: &App,
    actor: &Actor,
    uid: i32,
    username: &str,
    new_email: &str,
) -> AppResult<()> {
    let mut uow = Uow::begin(app).await?;
    let code = issue_code(&mut uow, uid, "e", new_email, 20).await?;
    let (bbname, bburl) = board(app);
    let body = format!(
        "{username},\n\nYou asked to change your email address at {bbname}. Confirm the new address by visiting:\n\n{bburl}/member/activate?uid={uid}&code={code}\n"
    );
    uow.mail(
        new_email,
        &format!("Confirm your new email at {bbname}"),
        &body,
    )
    .await?;
    uow.audit(actor, uid, "email_change_requested", serde_json::json!({}))
        .await?;
    uow.commit(app).await
}
