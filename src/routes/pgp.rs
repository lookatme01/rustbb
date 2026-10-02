//! End-to-end identity for private messages: publishing OpenPGP keys, looking up contacts' keys,
//! and recording signed "I verified this person" statements. See `crate::pgp` for the checks.

use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::models::User;
use crate::pgp;
use crate::util::now;
use axum::Json;
use axum::Router;
use axum::extract::{Path, Query};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::FromRow;

pub fn router() -> Router<crate::app::App> {
    Router::new()
        .route("/pgp/challenge", get(challenge))
        .route("/pgp/me", get(me_json))
        .route("/pgp/key", post(publish))
        .route("/pgp/backup", post(backup))
        .route("/pgp/revoke", post(revoke))
        .route("/pgp/user/{uid}", get(user_json))
        .route("/pgp/lookup", get(lookup))
        .route("/pgp/verify", post(verify))
        .route("/pgp/unverify", post(unverify))
        .route("/user/{uid}/pgp.asc", get(public_key_file))
        .route("/usercp/pgp", get(settings_page))
        .route("/pm/verify/{uid}", get(verify_page))
}

/// Content-Security-Policy for pages that run OpenPGP.js: the default policy plus
/// `'wasm-unsafe-eval'`, which lets the Argon2 passphrase hashing run as WebAssembly. It allows
/// compiling WebAssembly only; JavaScript `eval` stays blocked.
pub const CSP: &str = "default-src 'self'; img-src * data: blob:; media-src * blob:; style-src 'self' 'unsafe-inline'; \
    script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; form-action 'self'; frame-ancestors 'self'; \
    base-uri 'self'; object-src 'none'; worker-src 'self' blob:";

pub fn with_csp(mut r: Response) -> Response {
    r.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    r
}

/// The board's identity in signed statements: the origin of the board URL setting, so that a
/// signature made on one board can't be replayed on another.
pub fn board_id(ctx: &Ctx) -> String {
    let u = ctx
        .settings()
        .get("bburl")
        .trim()
        .trim_end_matches('/')
        .to_string();
    url::Url::parse(&u)
        .map(|p| p.origin().ascii_serialization())
        .unwrap_or(u)
}

fn require(ctx: &Ctx) -> AppResult<User> {
    let me = ctx.require_login()?.clone();
    if !ctx.settings().bool("enablepms") {
        return Err(AppError::user(
            "Private messaging has been disabled by the administrator.",
        ));
    }
    if !ctx.perms.canusepms || ctx.cache.is_system(me.uid) {
        return Err(AppError::no_perm());
    }
    Ok(me)
}

#[derive(FromRow, Serialize, Clone, Debug)]
pub struct KeyRow {
    pub kid: i32,
    pub uid: i32,
    pub fingerprint: String,
    pub algorithm: String,
    pub armored: String,
    pub user_ids: sqlx::types::Json<Value>,
    pub enc_keyids: Vec<String>,
    pub key_created: i64,
    pub expires: i64,
    pub added: i64,
    pub status: i16,
    pub retired: i64,
    pub source: String,
    pub transition_from: String,
    pub transition_sig: String,
    #[serde(skip)]
    pub backup: String,
}

impl KeyRow {
    fn status_name(&self) -> &'static str {
        match self.status {
            0 if self.expires > 0 && self.expires <= now() => "expired",
            0 => "active",
            1 => "replaced",
            _ => "revoked",
        }
    }
    /// Public view of the key, as served to other members.
    pub fn public_json(&self) -> Value {
        json!({
            "fingerprint": self.fingerprint, "algorithm": self.algorithm, "armored": self.armored,
            "user_ids": self.user_ids.0, "created": self.key_created, "expires": self.expires,
            "added": self.added, "status": self.status_name(), "retired": self.retired,
            "source": self.source, "transition_from": self.transition_from,
            "transition_sig": self.transition_sig,
        })
    }
}

/// The member's current (active, unexpired) key.
pub async fn active_key(ctx: &Ctx, uid: i32) -> AppResult<Option<KeyRow>> {
    Ok(
        sqlx::query_as::<_, KeyRow>("SELECT * FROM pgp_keys WHERE uid = $1 AND status = 0")
            .bind(uid)
            .fetch_optional(&ctx.app.db)
            .await?
            .filter(|k| k.expires == 0 || k.expires > now()),
    )
}

/// Active keys for several members at once.
pub async fn active_keys(ctx: &Ctx, uids: &[i32]) -> AppResult<Vec<KeyRow>> {
    Ok(sqlx::query_as::<_, KeyRow>(
        "SELECT * FROM pgp_keys WHERE uid = ANY($1) AND status = 0 AND (expires = 0 OR expires > $2)",
    )
    .bind(uids)
    .bind(now())
    .fetch_all(&ctx.app.db)
    .await?)
}

async fn history(ctx: &Ctx, uid: i32) -> AppResult<Vec<KeyRow>> {
    Ok(sqlx::query_as(
        "SELECT * FROM pgp_keys WHERE uid = $1 ORDER BY added DESC, kid DESC LIMIT 50",
    )
    .bind(uid)
    .fetch_all(&ctx.app.db)
    .await?)
}

#[derive(FromRow, Serialize)]
struct VerificationRow {
    verifier: i32,
    subject: i32,
    verifier_fpr: String,
    subject_fpr: String,
    statement: String,
    signature: String,
    created: i64,
}

fn json_ok(v: Value) -> Response {
    let mut r = Json(v).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

// ------------------------------------------------------------------ API

async fn challenge(ctx: Ctx) -> AppResult<Response> {
    let me = require(&ctx)?;
    let c = pgp::make_challenge(&ctx.app.cfg.secret, me.uid, now());
    Ok(json_ok(
        json!({ "challenge": c, "board": board_id(&ctx), "uid": me.uid }),
    ))
}

async fn me_json(ctx: Ctx) -> AppResult<Response> {
    let me = require(&ctx)?;
    let hist = history(&ctx, me.uid).await?;
    let active = hist.iter().find(|k| k.status == 0);
    let verifications: Vec<Value> = sqlx::query_as::<_, (i32, String, String, String, String, i64, Option<String>, Option<String>)>(
        "SELECT v.subject, v.subject_fpr, v.verifier_fpr, v.statement, v.signature, v.created, u.username, k.fingerprint
         FROM pgp_verifications v JOIN users u ON u.uid = v.subject
         LEFT JOIN pgp_keys k ON k.uid = v.subject AND k.status = 0
         WHERE v.verifier = $1 ORDER BY lower(u.username)",
    )
    .bind(me.uid)
    .fetch_all(&ctx.app.db)
    .await?
    .into_iter()
    .map(|(s, sf, vf, st, sig, c, name, cur)| {
        json!({ "subject": s, "username": name, "subject_fpr": sf, "verifier_fpr": vf,
                "statement": st, "signature": sig, "created": c, "current_fpr": cur })
    })
    .collect();
    Ok(json_ok(json!({
        "board": board_id(&ctx), "uid": me.uid, "username": me.username,
        "key": active.map(|k| k.public_json()),
        "backup": active.map(|k| k.backup.clone()).unwrap_or_default(),
        "history": hist.iter().map(|k| k.public_json()).collect::<Vec<_>>(),
        "verifications": verifications,
    })))
}

#[derive(Deserialize, Default)]
pub struct PublishForm {
    #[serde(default, deserialize_with = "de::string")]
    pub public_key: String,
    #[serde(default, deserialize_with = "de::string")]
    pub challenge: String,
    #[serde(default, deserialize_with = "de::string")]
    pub proof: String,
    #[serde(default, deserialize_with = "de::string")]
    pub transition_sig: String,
    #[serde(default, deserialize_with = "de::string")]
    pub backup: String,
    #[serde(default, deserialize_with = "de::string")]
    pub source: String,
}

async fn publish(ctx: Ctx, CsrfForm(f): CsrfForm<PublishForm>) -> AppResult<Response> {
    let me = require(&ctx)?;
    if !ctx.app.rate_check(&format!("pgpkey:{}", me.uid), 10, 3600) {
        return Err(AppError::RateLimited);
    }
    let t = now();
    if !pgp::check_challenge(&ctx.app.cfg.secret, me.uid, &f.challenge, t) {
        return Err(AppError::user(
            "The key proof has expired. Please try again.",
        ));
    }
    let (key, info) = pgp::parse_public_key(&f.public_key, t).map_err(AppError::user)?;
    let board = board_id(&ctx);
    let statement = pgp::key_proof_statement(&board, me.uid, &info.fingerprint, &f.challenge);
    pgp::verify_detached(&key, &f.proof, statement.as_bytes()).map_err(|e| {
        AppError::user(format!(
            "Could not confirm you hold the private key for this public key: {e}"
        ))
    })?;
    if !f.backup.trim().is_empty() {
        pgp::check_backup(&f.backup, &info.fingerprint).map_err(AppError::user)?;
    }
    let taken: Option<i32> = sqlx::query_scalar(
        "SELECT uid FROM pgp_keys WHERE fingerprint = $1 AND status = 0 AND uid <> $2",
    )
    .bind(&info.fingerprint)
    .bind(me.uid)
    .fetch_optional(&ctx.app.db)
    .await?;
    if taken.is_some() {
        return Err(AppError::user(
            "That key is already the identity key of another account.",
        ));
    }
    let revoked: Option<i16> = sqlx::query_scalar(
        "SELECT status FROM pgp_keys WHERE uid = $1 AND fingerprint = $2 AND status = 2",
    )
    .bind(me.uid)
    .bind(&info.fingerprint)
    .fetch_optional(&ctx.app.db)
    .await?;
    if revoked.is_some() {
        return Err(AppError::user(
            "You revoked that key. Revoked keys can't be used again; please create a new one.",
        ));
    }
    let old = active_key(&ctx, me.uid).await?;
    let old_any: Option<KeyRow> =
        sqlx::query_as("SELECT * FROM pgp_keys WHERE uid = $1 AND status = 0")
            .bind(me.uid)
            .fetch_optional(&ctx.app.db)
            .await?;
    let source = if f.source == "imported" {
        "imported"
    } else {
        "generated"
    };

    // Re-publishing the same key (for example to add or remove the backup) just refreshes it.
    if old_any
        .as_ref()
        .is_some_and(|k| k.fingerprint == info.fingerprint)
    {
        sqlx::query("UPDATE pgp_keys SET armored = $3, user_ids = $4, enc_keyids = $5, expires = $6, backup = CASE WHEN $7 = '' THEN backup ELSE $7 END WHERE uid = $1 AND fingerprint = $2")
            .bind(me.uid)
            .bind(&info.fingerprint)
            .bind(&info.armored)
            .bind(json!(info.user_ids))
            .bind(&info.encryption_key_ids)
            .bind(info.expires)
            .bind(f.backup.trim())
            .execute(&ctx.app.db)
            .await?;
        return me_json(ctx).await;
    }

    // A signature from the outgoing key vouching for the new one, if the browser still had it.
    let mut transition = (String::new(), String::new());
    if let (Some(old), false) = (&old, f.transition_sig.trim().is_empty()) {
        let (old_key, _) = pgp::parse_public_key(&old.armored, t).map_err(AppError::user)?;
        let st = pgp::key_transition_statement(&board, me.uid, &old.fingerprint, &info.fingerprint);
        pgp::verify_detached(&old_key, &f.transition_sig, st.as_bytes()).map_err(|e| {
            AppError::user(format!(
                "The signature from your previous key is invalid: {e}"
            ))
        })?;
        transition = (old.fingerprint.clone(), f.transition_sig.trim().to_string());
    }

    let mut tx = ctx.app.db.begin().await?;
    sqlx::query(
        "UPDATE pgp_keys SET status = 1, retired = $2, backup = '' WHERE uid = $1 AND status = 0",
    )
    .bind(me.uid)
    .bind(t)
    .execute(&mut *tx)
    .await?;
    // A key that was previously replaced can come back (for example after restoring a backup).
    sqlx::query("DELETE FROM pgp_keys WHERE uid = $1 AND fingerprint = $2 AND status = 1")
        .bind(me.uid)
        .bind(&info.fingerprint)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO pgp_keys (uid, fingerprint, algorithm, armored, user_ids, enc_keyids, key_created, expires, added, source, transition_from, transition_sig, backup)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
    )
    .bind(me.uid)
    .bind(&info.fingerprint)
    .bind(&info.algorithm)
    .bind(&info.armored)
    .bind(json!(info.user_ids))
    .bind(&info.encryption_key_ids)
    .bind(info.created)
    .bind(info.expires)
    .bind(t)
    .bind(source)
    .bind(&transition.0)
    .bind(&transition.1)
    .bind(f.backup.trim())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    crate::audit::log(
        &ctx,
        me.uid,
        if old_any.is_some() { "pgp_key_replaced" } else { "pgp_key_added" },
        json!({ "fingerprint": info.fingerprint, "previous": old_any.as_ref().map(|k| k.fingerprint.clone()) }),
    )
    .await;
    if old_any.is_some() {
        notify_verifiers(&ctx, me.uid, "pgp_keychange").await?;
    }
    me_json(ctx).await
}

/// Tell everyone who verified this member that the key they verified is no longer current.
async fn notify_verifiers(ctx: &Ctx, uid: i32, kind: &str) -> AppResult<()> {
    let verifiers: Vec<i32> =
        sqlx::query_scalar("SELECT verifier FROM pgp_verifications WHERE subject = $1")
            .bind(uid)
            .fetch_all(&ctx.app.db)
            .await?;
    for v in verifiers {
        crate::notify::alert(&ctx.app, v, uid, kind, uid, json!({})).await;
    }
    Ok(())
}

#[derive(Deserialize, Default)]
pub struct BackupForm {
    #[serde(default, deserialize_with = "de::string")]
    pub backup: String,
}

async fn backup(ctx: Ctx, CsrfForm(f): CsrfForm<BackupForm>) -> AppResult<Response> {
    let me = require(&ctx)?;
    let key = active_key(&ctx, me.uid)
        .await?
        .ok_or_else(|| AppError::user("You don't have an active key."))?;
    let b = f.backup.trim();
    if !b.is_empty() {
        pgp::check_backup(b, &key.fingerprint).map_err(AppError::user)?;
    }
    sqlx::query("UPDATE pgp_keys SET backup = $2 WHERE kid = $1")
        .bind(key.kid)
        .bind(b)
        .execute(&ctx.app.db)
        .await?;
    crate::audit::log(
        &ctx,
        me.uid,
        if b.is_empty() {
            "pgp_backup_removed"
        } else {
            "pgp_backup_saved"
        },
        json!({ "fingerprint": key.fingerprint }),
    )
    .await;
    me_json(ctx).await
}

#[derive(Deserialize, Default)]
pub struct RevokeForm {
    #[serde(default, deserialize_with = "de::string")]
    pub password: String,
}

async fn revoke(ctx: Ctx, CsrfForm(f): CsrfForm<RevokeForm>) -> AppResult<Response> {
    let me = require(&ctx)?;
    crate::routes::usercp::reauth_throttle(&ctx, me.uid)?;
    if !crate::auth::verify_password(&f.password, &me.password).await {
        return Err(AppError::user("The password you entered is incorrect."));
    }
    let key: KeyRow = sqlx::query_as("SELECT * FROM pgp_keys WHERE uid = $1 AND status = 0")
        .bind(me.uid)
        .fetch_optional(&ctx.app.db)
        .await?
        .ok_or_else(|| AppError::user("You don't have an active key."))?;
    sqlx::query("UPDATE pgp_keys SET status = 2, retired = $2, backup = '' WHERE kid = $1")
        .bind(key.kid)
        .bind(now())
        .execute(&ctx.app.db)
        .await?;
    crate::audit::log(
        &ctx,
        me.uid,
        "pgp_key_revoked",
        json!({ "fingerprint": key.fingerprint }),
    )
    .await;
    notify_verifiers(&ctx, me.uid, "pgp_keyrevoked").await?;
    me_json(ctx).await
}

async fn can_view_user(ctx: &Ctx, uid: i32) -> AppResult<(String, bool)> {
    let row: Option<(String, bool)> =
        sqlx::query_as("SELECT username, is_system FROM users WHERE uid = $1")
            .bind(uid)
            .fetch_optional(&ctx.app.db)
            .await?;
    row.ok_or_else(|| AppError::not_found("member"))
}

async fn my_verification(ctx: &Ctx, me: i32, subject: i32) -> AppResult<Option<VerificationRow>> {
    Ok(sqlx::query_as(
        "SELECT verifier, subject, verifier_fpr, subject_fpr, statement, signature, created FROM pgp_verifications WHERE verifier = $1 AND subject = $2",
    )
    .bind(me)
    .bind(subject)
    .fetch_optional(&ctx.app.db)
    .await?)
}

async fn user_json(ctx: Ctx, Path(uid): Path<i32>) -> AppResult<Response> {
    let me = require(&ctx)?;
    let (username, _) = can_view_user(&ctx, uid).await?;
    let hist = history(&ctx, uid).await?;
    let active = hist
        .iter()
        .find(|k| k.status == 0 && (k.expires == 0 || k.expires > now()));
    let v = my_verification(&ctx, me.uid, uid).await?;
    Ok(json_ok(json!({
        "uid": uid, "username": username,
        "key": active.map(|k| k.public_json()),
        "history": hist.iter().map(|k| k.public_json()).collect::<Vec<_>>(),
        "verification": v,
    })))
}

#[derive(Deserialize, Default)]
pub struct LookupQuery {
    #[serde(default)]
    pub names: String,
    #[serde(default)]
    pub uids: String,
}

/// Keys for the recipients typed into the compose form (by name) or for known uids.
async fn lookup(ctx: Ctx, Query(q): Query<LookupQuery>) -> AppResult<Response> {
    let me = require(&ctx)?;
    let names: Vec<String> = q
        .names
        .split([',', ';'])
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .take(50)
        .collect();
    let uids: Vec<i32> = q
        .uids
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .take(50)
        .collect();
    let users: Vec<(i32, String, bool)> = sqlx::query_as(
        "SELECT uid, username, is_system FROM users WHERE lower(username) = ANY($1) OR uid = ANY($2)",
    )
    .bind(&names)
    .bind(&uids)
    .fetch_all(&ctx.app.db)
    .await?;
    let ids: Vec<i32> = users.iter().map(|u| u.0).collect();
    let keys = active_keys(&ctx, &ids).await?;
    let vers: Vec<VerificationRow> = sqlx::query_as(
        "SELECT verifier, subject, verifier_fpr, subject_fpr, statement, signature, created FROM pgp_verifications WHERE verifier = $1 AND subject = ANY($2)",
    )
    .bind(me.uid)
    .bind(&ids)
    .fetch_all(&ctx.app.db)
    .await?;
    let out: Vec<Value> = users
        .iter()
        .map(|(uid, name, system)| {
            json!({
                "uid": uid, "username": name, "system": system,
                "key": keys.iter().find(|k| k.uid == *uid).map(|k| k.public_json()),
                "verification": vers.iter().find(|v| v.subject == *uid),
            })
        })
        .collect();
    Ok(json_ok(
        json!({ "users": out, "missing": names.iter().filter(|n| !users.iter().any(|u| u.1.to_lowercase() == **n)).collect::<Vec<_>>() }),
    ))
}

#[derive(Deserialize, Default)]
pub struct VerifyForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub subject: i32,
    #[serde(default, deserialize_with = "de::string")]
    pub statement: String,
    #[serde(default, deserialize_with = "de::string")]
    pub signature: String,
}

async fn verify(ctx: Ctx, CsrfForm(f): CsrfForm<VerifyForm>) -> AppResult<Response> {
    let me = require(&ctx)?;
    if f.subject == me.uid {
        return Err(AppError::user("You can't verify yourself."));
    }
    let mine = active_key(&ctx, me.uid)
        .await?
        .ok_or_else(|| AppError::user("Set up your own key before verifying others."))?;
    let theirs = active_key(&ctx, f.subject)
        .await?
        .ok_or_else(|| AppError::user("That member has no active key."))?;
    let ts: i64 = f
        .statement
        .lines()
        .find_map(|l| l.strip_prefix("ts: "))
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| AppError::user("The verification statement is malformed."))?;
    if (ts - now()).abs() > pgp::MAX_CLOCK_SKEW {
        return Err(AppError::user(
            "Your device's clock seems to be wrong. Please correct it and try again.",
        ));
    }
    let expected = pgp::verification_statement(
        &board_id(&ctx),
        me.uid,
        &mine.fingerprint,
        f.subject,
        &theirs.fingerprint,
        ts,
    );
    if expected != f.statement {
        return Err(AppError::user(
            "Their key changed while you were verifying. Please compare the safety numbers again.",
        ));
    }
    let (key, _) = pgp::parse_public_key(&mine.armored, now()).map_err(AppError::user)?;
    pgp::verify_detached(&key, &f.signature, expected.as_bytes()).map_err(AppError::user)?;
    sqlx::query(
        "INSERT INTO pgp_verifications (verifier, subject, verifier_fpr, subject_fpr, statement, signature, created)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (verifier, subject) DO UPDATE SET verifier_fpr = $3, subject_fpr = $4, statement = $5, signature = $6, created = $7",
    )
    .bind(me.uid)
    .bind(f.subject)
    .bind(&mine.fingerprint)
    .bind(&theirs.fingerprint)
    .bind(&expected)
    .bind(f.signature.trim())
    .bind(ts)
    .execute(&ctx.app.db)
    .await?;
    user_json(ctx, Path(f.subject)).await
}

#[derive(Deserialize, Default)]
pub struct UnverifyForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub subject: i32,
}

async fn unverify(ctx: Ctx, CsrfForm(f): CsrfForm<UnverifyForm>) -> AppResult<Response> {
    let me = require(&ctx)?;
    sqlx::query("DELETE FROM pgp_verifications WHERE verifier = $1 AND subject = $2")
        .bind(me.uid)
        .bind(f.subject)
        .execute(&ctx.app.db)
        .await?;
    user_json(ctx, Path(f.subject)).await
}

/// A member's current public key as a file, for use with other OpenPGP software.
async fn public_key_file(ctx: Ctx, Path(uid): Path<i32>) -> AppResult<Response> {
    if !ctx.perms.canviewprofiles {
        return Err(AppError::no_perm());
    }
    let key = active_key(&ctx, uid)
        .await?
        .ok_or_else(|| AppError::not_found("key"))?;
    Ok((
        [
            (
                header::CONTENT_TYPE,
                "application/pgp-keys; charset=utf-8".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!(
                    "inline; filename=\"{}.asc\"",
                    &key.fingerprint[key.fingerprint.len() - 16..]
                ),
            ),
        ],
        key.armored,
    )
        .into_response())
}

// ------------------------------------------------------------------ pages

async fn settings_page(ctx: Ctx) -> AppResult<Response> {
    let me = require(&ctx)?;
    if !ctx.perms.canusercp {
        return Err(AppError::no_perm());
    }
    let r = ctx
        .render(
            "usercp/pgp.html",
            minijinja::context! {
                title => "Encryption & identity", ucp_active => "pgp",
                breadcrumb => vec![("User Control Panel".to_string(), "/usercp".to_string())],
                board => board_id(&ctx), uid => me.uid,
            },
        )
        .await?;
    Ok(with_csp(r))
}

async fn verify_page(ctx: Ctx, Path(uid): Path<i32>) -> AppResult<Response> {
    let me = require(&ctx)?;
    let (_, system) = can_view_user(&ctx, uid).await?;
    if system {
        return Err(AppError::user(
            "The System account doesn't have an identity key.",
        ));
    }
    let authors = crate::render::load_authors(&ctx, &[me.uid, uid]).await?;
    let r = ctx
        .render(
            "pm/verify.html",
            minijinja::context! {
                title => "Verify identity",
                breadcrumb => vec![("Private Messages".to_string(), "/pm".to_string())],
                folders => crate::routes::private::folder_list(&me),
                me_author => authors.get(&me.uid), them => authors.get(&uid),
                them_uid => uid, board => board_id(&ctx), self_view => uid == me.uid,
            },
        )
        .await?;
    Ok(with_csp(r))
}
