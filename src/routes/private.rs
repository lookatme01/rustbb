//! Private messaging: folders, compose (to/bcc), read receipts, tracking, quotas.

use crate::app::{App, LiveEvent};
use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::models::User;
use crate::util::{self, now};
use axum::Router;
use axum::extract::{Path, Query};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use sqlx::FromRow;

pub fn router() -> Router<App> {
    Router::new()
        .route("/pm", get(folder))
        .route("/pm/send", get(compose).post(send))
        .route("/pm/read/{pmid}", get(read))
        .route("/pm/action", post(action))
        .route("/pm/folders", get(folders).post(folders_save))
        .route("/pm/tracking", get(tracking).post(tracking_action))
}

const BUILTIN: &[(i32, &str)] = &[
    (1, "Inbox"),
    (2, "Sent Items"),
    (3, "Drafts"),
    (4, "Trash Can"),
];

pub(crate) fn folder_list(me: &User) -> Vec<(i32, String)> {
    let mut v: Vec<(i32, String)> = BUILTIN.iter().map(|(i, n)| (*i, n.to_string())).collect();
    for f in me.pmfolders.0.iter() {
        if f.id >= 5 {
            v.push((f.id, f.name.clone()));
        }
    }
    v
}

fn require_pm(ctx: &Ctx) -> AppResult<User> {
    let me = ctx.require_login()?.clone();
    if !ctx.settings().bool("enablepms") {
        return Err(AppError::user(
            "Private messaging has been disabled by the administrator.",
        ));
    }
    if !ctx.perms.canusepms {
        return Err(AppError::no_perm());
    }
    Ok(me)
}

#[derive(FromRow, serde::Serialize, Clone)]
struct PmRow {
    pmid: i32,
    uid: i32,
    toid: i32,
    fromid: i32,
    recipients: sqlx::types::Json<serde_json::Value>,
    folder: i32,
    subject: String,
    icon: i32,
    message: String,
    dateline: i64,
    status: i16,
    statustime: i64,
    includesig: bool,
    smilieoff: bool,
    receipt: i16,
    readtime: i64,
    pgp: i16,
    pgp_fpr: String,
    pgp_payload: String,
    pgp_sig: String,
}

#[derive(Deserialize, Default)]
pub struct FolderQuery {
    pub folder: Option<i32>,
    pub page: Option<i64>,
    pub q: Option<String>,
}

async fn usage(ctx: &Ctx, uid: i32) -> AppResult<(i64, i64)> {
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM privatemessages WHERE uid = $1")
        .bind(uid)
        .fetch_one(&ctx.app.db)
        .await?;
    Ok((total, ctx.perms.pmquota as i64))
}

pub async fn folder(ctx: Ctx, Query(q): Query<FolderQuery>) -> AppResult<Response> {
    let me = require_pm(&ctx)?;
    let fid = q.folder.unwrap_or(1);
    let folders = folder_list(&me);
    let fname = folders
        .iter()
        .find(|f| f.0 == fid)
        .map(|f| f.1.clone())
        .ok_or_else(|| AppError::not_found("folder"))?;
    let per = ctx.settings().int("pmsperpage").max(5);
    let search = q.q.clone().unwrap_or_default();
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM privatemessages WHERE uid = $1 AND folder = $2 AND ($3 = '' OR subject ILIKE '%' || $3 || '%' OR (pgp <> 2 AND message ILIKE '%' || $3 || '%'))")
        .bind(me.uid)
        .bind(fid)
        .bind(&search)
        .fetch_one(&ctx.app.db)
        .await?;
    let pg = util::paginate(
        total,
        per,
        util::clamp_page(q.page),
        &format!(
            "/pm?folder={fid}&q={}&page={{page}}",
            percent_encoding::utf8_percent_encode(&search, percent_encoding::NON_ALPHANUMERIC)
        ),
    );
    let rows: Vec<(i32, String, i64, i16, i32, i32, Option<String>, Option<i32>, Option<i32>, serde_json::Value, i16)> = sqlx::query_as(
        "SELECT p.pmid, p.subject, p.dateline, p.status, p.fromid, p.toid, u.username, u.usergroup, u.displaygroup, p.recipients, p.pgp
         FROM privatemessages p LEFT JOIN users u ON u.uid = CASE WHEN p.folder = 2 OR p.folder = 3 THEN p.toid ELSE p.fromid END
         WHERE p.uid = $1 AND p.folder = $2 AND ($5 = '' OR p.subject ILIKE '%' || $5 || '%' OR (p.pgp <> 2 AND p.message ILIKE '%' || $5 || '%'))
         ORDER BY p.dateline DESC LIMIT $3 OFFSET $4",
    )
    .bind(me.uid)
    .bind(fid)
    .bind(per)
    .bind((pg.page - 1) * per)
    .bind(&search)
    .fetch_all(&ctx.app.db)
    .await?;
    let list: Vec<_> = rows
        .into_iter()
        .map(|(pmid, subject, dl, status, fromid, toid, name, g, d, rec, pgp)| {
            let other = match name {
                Some(n) => ctx.cache.format_name(&n, g.unwrap_or(2), d.unwrap_or(0)),
                None => if fromid == 0 && fid != 2 { format!("{} (system)", util::escape_html(ctx.settings().get("bbname"))) } else { "Unknown".into() },
            };
            let multi = rec["to"].as_array().map(|a| a.len()).unwrap_or(0) > 1;
            minijinja::context! { pmid => pmid, subject => subject, dateline => dl, status => status, other => other, other_uid => if fid == 2 || fid == 3 { toid } else { fromid }, multi => multi, pgp => pgp }
        })
        .collect();
    let (used, quota) = usage(&ctx, me.uid).await?;
    ctx.render(
        "pm/folder.html",
        minijinja::context! { title => fname.clone(), breadcrumb => vec![("Private Messages".to_string(), "/pm".to_string())], folder => fid, folder_name => fname, folders => folders, messages => list, pagination => pg, used => used, quota => quota, search => search },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct ComposeQuery {
    pub uid: Option<i32>,
    pub pmid: Option<i32>,
    pub mode: Option<String>,
    pub to: Option<String>,
}

async fn compose_page(
    ctx: &Ctx,
    me: &User,
    to: String,
    bcc: String,
    subject: String,
    message: String,
    pmid: i32,
    errors: Vec<String>,
    preview: Option<String>,
    pgp: PgpCompose,
    as_system: bool,
) -> AppResult<Response> {
    let r = ctx
        .render(
            "pm/compose.html",
            minijinja::context! {
                title => "Compose Message", breadcrumb => vec![("Private Messages".to_string(), "/pm".to_string())],
                form => minijinja::context!{ to => to, bcc => bcc, subject => subject, message => message, pmid => pmid, pgp_mode => pgp.mode, as_system => as_system },
                can_send_as_system => ctx.perms.canpostassystem,
                errors => errors, preview => preview, folders => folder_list(me), smilies => crate::routes::posting::clickable_smilies(ctx),
                can_track => ctx.perms.cantrackpms, max_recipients => ctx.perms.maxpmrecipients,
                pgp_source => pgp.source, board => crate::routes::pgp::board_id(ctx), uid => me.uid,
            },
        )
        .await?;
    Ok(crate::routes::pgp::with_csp(r))
}

/// End-to-end state carried into the compose form.
#[derive(Default)]
struct PgpCompose {
    /// "", "sign" or "encrypt": what the sender had chosen (kept across preview and errors).
    mode: String,
    /// An encrypted message the browser must decrypt to fill the form: a reply/forward quote,
    /// a draft, or the sender's own message coming back with errors.
    source: Option<serde_json::Value>,
}

pub async fn compose(ctx: Ctx, Query(q): Query<ComposeQuery>) -> AppResult<Response> {
    let me = require_pm(&ctx)?;
    if !ctx.perms.cansendpms {
        return Err(AppError::no_perm());
    }
    let mut to = q.to.clone().unwrap_or_default();
    let (mut subject, mut message) = (String::new(), String::new());
    if let Some(uid) = q.uid {
        if let Some(n) =
            sqlx::query_scalar::<_, String>("SELECT username FROM users WHERE uid = $1")
                .bind(uid)
                .fetch_optional(&ctx.app.db)
                .await?
        {
            to = n;
        }
    }
    let mut pmid = 0;
    let mut pgp_compose = PgpCompose::default();
    if let Some(id) = q.pmid {
        let pm: Option<PmRow> =
            sqlx::query_as("SELECT * FROM privatemessages WHERE pmid = $1 AND uid = $2")
                .bind(id)
                .bind(me.uid)
                .fetch_optional(&ctx.app.db)
                .await?;
        if let Some(pm) = pm {
            let from_name: String = sqlx::query_scalar("SELECT username FROM users WHERE uid = $1")
                .bind(pm.fromid)
                .fetch_optional(&ctx.app.db)
                .await?
                .unwrap_or_else(|| "System".into());
            if pm.pgp == 2 {
                // The server can't quote what it can't read: the browser decrypts and quotes.
                let mode = match q.mode.as_deref() {
                    Some("forward") => "forward",
                    Some("replyall") => "replyall",
                    Some("draft") if pm.folder == 3 => "draft",
                    _ => "reply",
                };
                pgp_compose = PgpCompose {
                    mode: "encrypt".into(),
                    source: Some(serde_json::json!({ "mode": mode, "armored": pm.message, "from": from_name, "fromid": pm.fromid })),
                };
            }
            match q.mode.as_deref() {
                Some("forward") => {
                    subject = format!("Fw: {}", pm.subject.trim_start_matches("Fw: "));
                    message = if pm.pgp == 2 { String::new() } else { format!("\n\n[quote='{from_name}']\n{}\n[/quote]", pm.message) };
                }
                Some("replyall") => {
                    subject = format!("Re: {}", pm.subject.trim_start_matches("Re: "));
                    let ids: Vec<i32> = pm.recipients.0["to"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_i64().map(|x| x as i32))
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut names: Vec<String> = sqlx::query_scalar(
                        "SELECT username FROM users WHERE uid = ANY($1) AND uid <> $2",
                    )
                    .bind(&ids)
                    .bind(me.uid)
                    .fetch_all(&ctx.app.db)
                    .await?;
                    if !ctx.cache.is_system(pm.fromid) {
                        names.insert(0, from_name.clone());
                    }
                    names.dedup();
                    to = names.join(", ");
                    message = if pm.pgp == 2 { String::new() } else { format!("[quote='{from_name}']\n{}\n[/quote]\n", pm.message) };
                }
                Some("draft") if pm.folder == 3 => {
                    subject = pm.subject.clone();
                    message = if pm.pgp == 2 { String::new() } else { pm.message.clone() };
                    let ids: Vec<i32> = pm.recipients.0["to"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_i64().map(|x| x as i32))
                                .collect()
                        })
                        .unwrap_or_default();
                    let names: Vec<String> =
                        sqlx::query_scalar("SELECT username FROM users WHERE uid = ANY($1)")
                            .bind(&ids)
                            .fetch_all(&ctx.app.db)
                            .await?;
                    to = names.join(", ");
                    pmid = pm.pmid;
                }
                _ => {
                    subject = format!("Re: {}", pm.subject.trim_start_matches("Re: "));
                    // System messages are automated: there is nobody to reply to.
                    to = if ctx.cache.is_system(pm.fromid) { String::new() } else { from_name.clone() };
                    message = if pm.pgp == 2 { String::new() } else { format!("[quote='{from_name}']\n{}\n[/quote]\n", pm.message) };
                }
            }
        }
    }
    compose_page(
        &ctx,
        &me,
        to,
        String::new(),
        subject,
        message,
        pmid,
        vec![],
        None,
        pgp_compose,
        false,
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct SendForm {
    #[serde(default, deserialize_with = "de::string")]
    pub to: String,
    #[serde(default, deserialize_with = "de::string")]
    pub bcc: String,
    #[serde(default, deserialize_with = "de::string")]
    pub subject: String,
    #[serde(default, deserialize_with = "de::string")]
    pub message: String,
    #[serde(default, deserialize_with = "de::bool")]
    pub savecopy: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub receipt: bool,
    #[serde(default, deserialize_with = "de::string")]
    pub preview: String,
    #[serde(default, deserialize_with = "de::string")]
    pub savedraft: String,
    #[serde(default, deserialize_with = "de::i32")]
    pub pmid: i32,
    #[serde(default, deserialize_with = "de::bool")]
    pub smilieoff: bool,
    /// "", "sign" or "encrypt" (see `crate::pgp`).
    #[serde(default, deserialize_with = "de::string")]
    pub pgp_mode: String,
    /// Signed messages: the canonical JSON payload and a detached signature over it.
    #[serde(default, deserialize_with = "de::string")]
    pub pgp_payload: String,
    #[serde(default, deserialize_with = "de::string")]
    pub pgp_sig: String,
    /// Send from the System account (needs `canpostassystem`).
    #[serde(default, deserialize_with = "de::bool")]
    pub as_system: bool,
}

/// What gets stored for a signed or encrypted message.
#[derive(Default)]
struct PgpStored {
    level: i16,
    fpr: String,
    payload: String,
    sig: String,
    body: String,
}

/// Check a signed or encrypted submission against the sender's key and the recipients.
async fn check_pgp(
    ctx: &Ctx,
    me: &User,
    f: &SendForm,
    recipients: &[Recipient],
    to_ids: &[i32],
    draft: bool,
) -> Result<PgpStored, String> {
    use crate::pgp;
    let mine = crate::routes::pgp::active_key(ctx, me.uid)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("You need an encryption key to sign or encrypt messages. Set one up under User CP → Encryption & identity.")?;
    if !f.bcc.trim().is_empty() {
        return Err("BCC can't be used with signed or encrypted messages; add everyone to “To”.".into());
    }
    match f.pgp_mode.as_str() {
        "sign" if draft => Ok(PgpStored { body: pgp::normalize_body(&f.message), ..Default::default() }),
        "sign" => {
            let p: pgp::MessagePayload = serde_json::from_str(&f.pgp_payload)
                .map_err(|_| "The message signature is malformed.".to_string())?;
            let mut to = to_ids.to_vec();
            to.sort_unstable();
            let now = now();
            let problem = if p.v != 1 || p.t != "rbb-pm" {
                Some("unsupported format")
            } else if p.board != crate::routes::pgp::board_id(ctx) {
                Some("signed for a different board")
            } else if p.from != me.uid || p.fpr != mine.fingerprint {
                Some("not signed with your current key")
            } else if p.to != to {
                Some("the recipients differ")
            } else if p.subject != pgp::normalize_subject(&f.subject) {
                Some("the subject differs")
            } else if p.body != pgp::normalize_body(&f.message) {
                Some("the message text differs")
            } else if (p.ts - now).abs() > pgp::MAX_CLOCK_SKEW {
                Some("your device's clock is off by more than ten minutes")
            } else {
                None
            };
            if let Some(why) = problem {
                return Err(format!("The signature doesn't match this message ({why}). Please try again."));
            }
            let (key, _) = pgp::parse_public_key(&mine.armored, now)?;
            pgp::verify_detached(&key, &f.pgp_sig, f.pgp_payload.as_bytes())
                .map_err(|e| format!("The message signature is invalid: {e}"))?;
            Ok(PgpStored {
                level: 1,
                fpr: mine.fingerprint,
                payload: f.pgp_payload.clone(),
                sig: f.pgp_sig.trim().to_string(),
                body: p.body,
            })
        }
        "encrypt" => {
            let entries = pgp::encrypted_recipients(&f.message)?;
            let addressed = |ids: &[String]| entries.iter().any(|e| pgp::recipient_matches(e, ids));
            if !addressed(&mine.enc_keyids) {
                return Err("The message isn't encrypted to your own key, so you couldn't read your copy.".into());
            }
            if !draft {
                let uids: Vec<i32> = recipients.iter().map(|r| r.uid).collect();
                let keys = crate::routes::pgp::active_keys(ctx, &uids).await.map_err(|e| e.to_string())?;
                for r in recipients {
                    match keys.iter().find(|k| k.uid == r.uid) {
                        None => return Err(format!("{} doesn't have an encryption key, so they couldn't read an encrypted message.", r.username)),
                        Some(k) if !addressed(&k.enc_keyids) => {
                            return Err(format!("{}'s key changed while you were writing. Please send again.", r.username));
                        }
                        _ => {}
                    }
                }
            }
            Ok(PgpStored { level: 2, fpr: mine.fingerprint, body: f.message.trim().to_string(), ..Default::default() })
        }
        _ => Err("Unknown message protection.".into()),
    }
}

fn parse_names(s: &str) -> Vec<String> {
    let mut v: Vec<String> = s
        .split([',', ';'])
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect();
    v.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    v
}

#[derive(FromRow)]
struct Recipient {
    uid: i32,
    username: String,
    email: String,
    usergroup: i32,
    additionalgroups: Vec<i32>,
    receivepms: bool,
    receivefrombuddy: bool,
    buddylist: Vec<i32>,
    ignorelist: Vec<i32>,
    pmnotify: bool,
}

pub async fn send(ctx: Ctx, CsrfForm(f): CsrfForm<SendForm>) -> AppResult<Response> {
    let me = require_pm(&ctx)?;
    if !ctx.perms.cansendpms {
        return Err(AppError::no_perm());
    }
    let s = ctx.settings().clone();
    let mut errors: Vec<String> = vec![];
    // Who the message is from: the member, or System for staff allowed to speak as it.
    let (sender_uid, sender_name) = if f.as_system {
        if !ctx.perms.canpostassystem {
            return Err(AppError::no_perm());
        }
        if !f.pgp_mode.is_empty() {
            errors.push("Messages sent as System can't be signed or encrypted.".into());
        }
        if !f.savedraft.is_empty() {
            errors.push("Messages sent as System can't be saved as drafts. Untick “Send as System” to save a draft.".into());
        }
        if f.preview.is_empty() && f.savedraft.is_empty() && !ctx.app.rate_check(&format!("pmassystem:{}", me.uid), 60, 3600) {
            return Err(AppError::RateLimited);
        }
        crate::system::identity(&ctx.app).await?
    } else {
        (me.uid, me.username.clone())
    };
    let to_names = parse_names(&f.to);
    let bcc_names = parse_names(&f.bcc);
    let all_names: Vec<String> = to_names
        .iter()
        .chain(bcc_names.iter())
        .map(|n| n.to_lowercase())
        .collect();
    let max = ctx.perms.maxpmrecipients;
    if all_names.is_empty() && f.savedraft.is_empty() {
        errors.push("Please enter at least one recipient.".into());
    }
    if max > 0 && all_names.len() > max as usize {
        errors.push(format!(
            "You can send a message to at most {max} recipients."
        ));
    }
    if f.subject.trim().is_empty() {
        errors.push("Please enter a subject.".into());
    }
    if f.message.trim().is_empty() {
        errors.push("Please enter a message.".into());
    }
    let recipients: Vec<Recipient> = sqlx::query_as(
        "SELECT uid, username, email, usergroup, additionalgroups, receivepms, receivefrombuddy, buddylist, ignorelist, pmnotify FROM users WHERE lower(username) = ANY($1)",
    )
    .bind(&all_names)
    .fetch_all(&ctx.app.db)
    .await?;
    for n in &all_names {
        if !recipients.iter().any(|r| r.username.to_lowercase() == *n) {
            errors.push(format!("The user “{n}” does not exist."));
        }
    }
    if f.savedraft.is_empty() {
        for r in &recipients {
            let mut groups = vec![r.usergroup];
            groups.extend(&r.additionalgroups);
            let rperms = ctx.cache.group_perms(&groups);
            let override_ = ctx.perms.canoverridepm;
            if ctx.cache.is_system(r.uid) {
                errors.push(format!("{} cannot receive private messages.", r.username));
            } else if !rperms.canusepms && !override_ {
                errors.push(format!("{} cannot receive private messages.", r.username));
            } else if (!r.receivepms || r.ignorelist.contains(&sender_uid)) && !override_ {
                errors.push(format!(
                    "{} has chosen not to receive private messages from you.",
                    r.username
                ));
            } else if r.receivefrombuddy && !r.buddylist.contains(&sender_uid) && !override_ {
                errors.push(format!(
                    "{} only accepts private messages from their friends.",
                    r.username
                ));
            } else if rperms.pmquota > 0 && !override_ {
                let n: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM privatemessages WHERE uid = $1")
                        .bind(r.uid)
                        .fetch_one(&ctx.app.db)
                        .await?;
                if n >= rperms.pmquota as i64 {
                    errors.push(format!(
                        "{}'s private message folders are full.",
                        r.username
                    ));
                }
            }
        }
        let flood = s.int("pmfloodsecs");
        if flood > 0 && !ctx.is_any_mod() {
            let last: Option<i64> = sqlx::query_scalar(
                "SELECT MAX(dateline) FROM privatemessages WHERE fromid = $1 AND folder <> 3",
            )
            .bind(me.uid)
            .fetch_one(&ctx.app.db)
            .await?;
            if let Some(l) = last {
                if now() - l < flood {
                    errors.push(format!(
                        "Please wait {} more seconds before sending another message.",
                        flood - (now() - l)
                    ));
                }
            }
        }
    }
    let to_ids: Vec<i32> = recipients
        .iter()
        .filter(|r| to_names.iter().any(|n| n.eq_ignore_ascii_case(&r.username)))
        .map(|r| r.uid)
        .collect();
    let mut stored = PgpStored { body: f.message.clone(), ..Default::default() };
    if !f.pgp_mode.is_empty() && f.preview.is_empty() && errors.is_empty() {
        match check_pgp(&ctx, &me, &f, &recipients, &to_ids, !f.savedraft.is_empty()).await {
            Ok(x) => stored = x,
            Err(e) => errors.push(e),
        }
    }
    let opts = crate::parser::ParseOptions {
        allow_mycode: s.bool("pmsallowmycode"),
        allow_smilies: s.bool("pmsallowsmilies") && !f.smilieoff,
        allow_imgcode: s.bool("pmsallowimgcode"),
        allow_videocode: s.bool("pmsallowvideocode"),
        me_username: Some(me.username.clone()),
        ..Default::default()
    };
    if !f.preview.is_empty() || !errors.is_empty() {
        // An encrypted body can't go back into the form as-is: the browser decrypts it again.
        let encrypted = f.pgp_mode == "encrypt" && f.message.trim_start().starts_with("-----BEGIN PGP MESSAGE-----");
        let pgp_compose = PgpCompose {
            mode: f.pgp_mode.clone(),
            source: encrypted.then(|| serde_json::json!({ "mode": "resume", "armored": f.message })),
        };
        let preview = if errors.is_empty() && !encrypted {
            Some(crate::render::parse_with(
                &ctx.cache,
                &ctx.app.plugins,
                &opts,
                &f.message,
            ))
        } else {
            None
        };
        return compose_page(
            &ctx,
            &me,
            f.to.clone(),
            f.bcc.clone(),
            f.subject.clone(),
            if encrypted { String::new() } else { f.message.clone() },
            f.pmid,
            errors,
            preview,
            pgp_compose,
            f.as_system,
        )
        .await;
    }
    let bcc_ids: Vec<i32> = recipients
        .iter()
        .filter(|r| !to_ids.contains(&r.uid))
        .map(|r| r.uid)
        .collect();
    let rec_json = serde_json::json!({"to": to_ids, "bcc": bcc_ids});
    let subject: String = f.subject.trim().chars().take(120).collect();
    let t = now();
    if !f.savedraft.is_empty() {
        if f.pmid > 0 {
            sqlx::query("UPDATE privatemessages SET subject = $3, message = $4, recipients = $5, dateline = $6, pgp = $7, pgp_fpr = $8 WHERE pmid = $1 AND uid = $2 AND folder = 3")
                .bind(f.pmid)
                .bind(me.uid)
                .bind(&subject)
                .bind(&stored.body)
                .bind(&rec_json)
                .bind(t)
                .bind(stored.level)
                .bind(&stored.fpr)
                .execute(&ctx.app.db)
                .await?;
        } else {
            sqlx::query("INSERT INTO privatemessages (uid, toid, fromid, recipients, folder, subject, message, dateline, status, ipaddress, pgp, pgp_fpr) VALUES ($1, 0, $1, $2, 3, $3, $4, $5, 1, $6, $7, $8)")
                .bind(me.uid)
                .bind(&rec_json)
                .bind(&subject)
                .bind(&stored.body)
                .bind(t)
                .bind(&ctx.ip)
                .bind(stored.level)
                .bind(&stored.fpr)
                .execute(&ctx.app.db)
                .await?;
        }
        return Ok(ctx.redirect("/pm?folder=3", "Your message has been saved as a draft."));
    }
    let receipt: i16 = if f.receipt && ctx.perms.cantrackpms && !f.as_system {
        1
    } else {
        0
    };
    let mut tx = ctx.app.db.begin().await?;
    let mut rec_pmids: Vec<(i32, i32)> = vec![];
    for r in &recipients {
        let pmid: i32 = sqlx::query_scalar(
            "INSERT INTO privatemessages (uid, toid, fromid, recipients, folder, subject, message, dateline, status, receipt, smilieoff, ipaddress, pgp, pgp_fpr, pgp_payload, pgp_sig)
             VALUES ($1, $1, $2, $3, 1, $4, $5, $6, 0, $7, $8, $9, $10, $11, $12, $13) RETURNING pmid",
        )
        .bind(r.uid)
        .bind(sender_uid)
        .bind(serde_json::json!({"to": to_ids}))
        .bind(&subject)
        .bind(&stored.body)
        .bind(t)
        .bind(receipt)
        .bind(f.smilieoff)
        .bind(if f.as_system { "" } else { ctx.ip.as_str() })
        .bind(stored.level)
        .bind(&stored.fpr)
        .bind(&stored.payload)
        .bind(&stored.sig)
        .fetch_one(&mut *tx)
        .await?;
        rec_pmids.push((r.uid, pmid));
        if f.as_system {
            let summary = format!("to {}: {subject}", r.username);
            crate::system::record(
                &mut tx,
                crate::system::Authorship { kind: "pm", ref_id: pmid, actor: me.uid, actor_name: &me.username, ip: &ctx.ip, summary: &summary },
            )
            .await?;
        }
        sqlx::query(
            "UPDATE users SET unreadpms = unreadpms + 1, totalpms = totalpms + 1 WHERE uid = $1",
        )
        .bind(r.uid)
        .execute(&mut *tx)
        .await?;
    }
    // System has no Sent folder, and the staff member's copy would show System as its sender.
    if f.savecopy && !f.as_system {
        sqlx::query(
            "INSERT INTO privatemessages (uid, toid, fromid, recipients, folder, subject, message, dateline, status, receipt, smilieoff, ipaddress, pgp, pgp_fpr, pgp_payload, pgp_sig)
             VALUES ($1, $2, $1, $3, 2, $4, $5, $6, 1, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(me.uid)
        .bind(to_ids.first().copied().unwrap_or(0))
        .bind(&rec_json)
        .bind(&subject)
        .bind(&stored.body)
        .bind(t)
        .bind(receipt)
        .bind(f.smilieoff)
        .bind(&ctx.ip)
        .bind(stored.level)
        .bind(&stored.fpr)
        .bind(&stored.payload)
        .bind(&stored.sig)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE users SET totalpms = totalpms + 1 WHERE uid = $1")
            .bind(me.uid)
            .execute(&mut *tx)
            .await?;
    }
    if f.pmid > 0 {
        sqlx::query("DELETE FROM privatemessages WHERE pmid = $1 AND uid = $2 AND folder = 3")
            .bind(f.pmid)
            .bind(me.uid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    let bburl = s.get("bburl").trim_end_matches('/').to_string();
    for r in &recipients {
        let pmid = rec_pmids
            .iter()
            .find(|x| x.0 == r.uid)
            .map(|x| x.1)
            .unwrap_or(0);
        crate::notify::alert(
            &ctx.app,
            r.uid,
            sender_uid,
            "pm",
            pmid,
            serde_json::json!({"subject": subject}),
        )
        .await;
        ctx.app
            .publish_all(LiveEvent {
                kind: "pm",
                tid: 0,
                uid: r.uid,
                data: serde_json::json!({"from": sender_name}),
            })
            .await;
        if r.pmnotify {
            let body = format!(
                "{},\n\n{} has sent you a new private message on {} titled \"{}\".\n\nTo read it, log in and visit:\n{}/pm\n\nYou can turn off these notifications in the User CP options.\n",
                r.username,
                sender_name,
                s.get("bbname"),
                subject,
                bburl
            );
            crate::mail::queue(
                &ctx.app,
                &r.email,
                &format!("New Private Message at {}", s.get("bbname")),
                &body,
            )
            .await;
        }
    }
    Ok(ctx.redirect("/pm", "Your message has been sent."))
}

/// Send an automated private message from the System account. Members can't reply to it.
pub async fn send_system_pm(app: &App, uid: i32, subject: &str, msg: &str) -> AppResult<()> {
    sqlx::query("INSERT INTO privatemessages (uid, toid, fromid, recipients, folder, subject, message, dateline, status) VALUES ($1, $1, $6, $2, 1, $3, $4, $5, 0)")
        .bind(uid)
        .bind(serde_json::json!({"to": [uid]}))
        .bind(subject)
        .bind(msg)
        .bind(now())
        .bind(app.cache().system_uid)
        .execute(&app.db)
        .await?;
    sqlx::query(
        "UPDATE users SET unreadpms = unreadpms + 1, totalpms = totalpms + 1 WHERE uid = $1",
    )
    .bind(uid)
    .execute(&app.db)
    .await?;
    Ok(())
}

pub async fn read(ctx: Ctx, Path(pmid): Path<i32>) -> AppResult<Response> {
    let me = require_pm(&ctx)?;
    let pm: PmRow = sqlx::query_as("SELECT * FROM privatemessages WHERE pmid = $1 AND uid = $2")
        .bind(pmid)
        .bind(me.uid)
        .fetch_optional(&ctx.app.db)
        .await?
        .ok_or_else(|| AppError::not_found("message"))?;
    if pm.folder == 3 {
        return Ok(ctx.redirect(&format!("/pm/send?pmid={pmid}&mode=draft"), ""));
    }
    if pm.status == 0 && pm.folder != 2 {
        sqlx::query(
            "UPDATE privatemessages SET status = 1, statustime = $2, readtime = $2 WHERE pmid = $1",
        )
        .bind(pmid)
        .bind(now())
        .execute(&ctx.app.db)
        .await?;
        sqlx::query("UPDATE users SET unreadpms = GREATEST(unreadpms - 1, 0) WHERE uid = $1")
            .bind(me.uid)
            .execute(&ctx.app.db)
            .await?;
        // Read receipt: mark the sender's copy.
        if pm.receipt == 1 {
            sqlx::query("UPDATE privatemessages SET receipt = 2, readtime = $3 WHERE fromid = $1 AND folder = 2 AND dateline = $2 AND receipt = 1")
                .bind(pm.fromid)
                .bind(pm.dateline)
                .bind(now())
                .execute(&ctx.app.db)
                .await?;
        }
    }
    let s = ctx.settings();
    let opts = crate::parser::ParseOptions {
        allow_mycode: s.bool("pmsallowmycode"),
        allow_smilies: s.bool("pmsallowsmilies") && !pm.smilieoff,
        allow_imgcode: s.bool("pmsallowimgcode"),
        allow_videocode: s.bool("pmsallowvideocode"),
        ..Default::default()
    };
    // Encrypted bodies are decrypted and rendered by the recipient's browser.
    let html = if pm.pgp == 2 {
        String::new()
    } else {
        crate::render::parse_with(&ctx.cache, &ctx.app.plugins, &opts, &pm.message)
    };
    let author = if pm.fromid > 0 {
        crate::render::load_authors(&ctx, &[pm.fromid])
            .await?
            .remove(&pm.fromid)
    } else {
        None
    };
    let to_ids: Vec<i32> = pm.recipients.0["to"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_i64().map(|x| x as i32))
                .collect()
        })
        .unwrap_or_default();
    let to_names: Vec<(i32, String)> =
        sqlx::query_as("SELECT uid, username FROM users WHERE uid = ANY($1)")
            .bind(&to_ids)
            .fetch_all(&ctx.app.db)
            .await?;
    // Everything the browser needs to check the signature itself.
    let pgp = (pm.pgp > 0).then(|| {
        serde_json::json!({
            "level": pm.pgp, "fpr": pm.pgp_fpr, "payload": pm.pgp_payload, "sig": pm.pgp_sig,
            "armored": if pm.pgp == 2 { pm.message.as_str() } else { "" },
            "body": if pm.pgp == 1 { pm.message.as_str() } else { "" },
            "from": pm.fromid, "to": to_ids, "subject": pm.subject, "dateline": pm.dateline,
            "sent": pm.folder == 2, "pmid": pm.pmid,
            "allow_mycode": s.bool("pmsallowmycode"),
        })
    });
    // Senders who have a key but didn't sign: worth a quiet note.
    let sender_has_key = pm.pgp == 0
        && pm.fromid > 0
        && crate::routes::pgp::active_key(&ctx, pm.fromid).await?.is_some();
    let r = ctx
        .render(
            "pm/read.html",
            minijinja::context! {
                title => &pm.subject, breadcrumb => vec![("Private Messages".to_string(), "/pm".to_string())],
                pm => &pm, html => html, author => author, to => to_names, folders => folder_list(&me),
                system_name => s.get("bbname"), pgp => pgp, sender_has_key => sender_has_key,
                from_system => pm.fromid == 0 || ctx.cache.is_system(pm.fromid), can_contact => s.bool("contact"),
                board => crate::routes::pgp::board_id(&ctx), uid => me.uid,
            },
        )
        .await?;
    Ok(crate::routes::pgp::with_csp(r))
}

#[derive(Deserialize, Default)]
pub struct ActionForm {
    #[serde(default, deserialize_with = "de::vec_i32")]
    pub pmids: Vec<i32>,
    #[serde(default, deserialize_with = "de::string")]
    pub action: String,
    #[serde(default, deserialize_with = "de::i32")]
    pub folder: i32,
    #[serde(default, deserialize_with = "de::i32")]
    pub target: i32,
}

async fn recount_pms(ctx: &Ctx, uid: i32) -> AppResult<()> {
    sqlx::query(
        "UPDATE users SET unreadpms = (SELECT COUNT(*) FROM privatemessages WHERE uid = $1 AND status = 0 AND folder <> 2 AND folder <> 3),
            totalpms = (SELECT COUNT(*) FROM privatemessages WHERE uid = $1) WHERE uid = $1",
    )
    .bind(uid)
    .execute(&ctx.app.db)
    .await?;
    Ok(())
}

pub async fn action(ctx: Ctx, CsrfForm(f): CsrfForm<ActionForm>) -> AppResult<Response> {
    let me = require_pm(&ctx)?;
    let back = format!("/pm?folder={}", if f.folder > 0 { f.folder } else { 1 });
    match f.action.as_str() {
        "delete" => {
            // Messages in the trash are deleted permanently; elsewhere they go to the trash.
            sqlx::query(
                "DELETE FROM privatemessages WHERE uid = $1 AND pmid = ANY($2) AND folder = 4",
            )
            .bind(me.uid)
            .bind(&f.pmids)
            .execute(&ctx.app.db)
            .await?;
            sqlx::query("UPDATE privatemessages SET folder = 4, deletetime = $3 WHERE uid = $1 AND pmid = ANY($2)").bind(me.uid).bind(&f.pmids).bind(now()).execute(&ctx.app.db).await?;
        }
        "move" => {
            if !folder_list(&me).iter().any(|x| x.0 == f.target) || f.target == 3 {
                return Err(AppError::user("Invalid folder."));
            }
            sqlx::query("UPDATE privatemessages SET folder = $3 WHERE uid = $1 AND pmid = ANY($2)")
                .bind(me.uid)
                .bind(&f.pmids)
                .bind(f.target)
                .execute(&ctx.app.db)
                .await?;
        }
        "read" => {
            sqlx::query("UPDATE privatemessages SET status = 1, readtime = $3 WHERE uid = $1 AND pmid = ANY($2) AND status = 0").bind(me.uid).bind(&f.pmids).bind(now()).execute(&ctx.app.db).await?;
        }
        "unread" => {
            sqlx::query("UPDATE privatemessages SET status = 0 WHERE uid = $1 AND pmid = ANY($2)")
                .bind(me.uid)
                .bind(&f.pmids)
                .execute(&ctx.app.db)
                .await?;
        }
        "emptytrash" => {
            sqlx::query("DELETE FROM privatemessages WHERE uid = $1 AND folder = 4")
                .bind(me.uid)
                .execute(&ctx.app.db)
                .await?;
        }
        _ => return Err(AppError::user("Unknown action.")),
    }
    recount_pms(&ctx, me.uid).await?;
    Ok(ctx.redirect(&back, "Your messages have been updated."))
}

pub async fn folders(ctx: Ctx) -> AppResult<Response> {
    let me = require_pm(&ctx)?;
    let custom: Vec<(i32, String)> = me
        .pmfolders
        .0
        .iter()
        .filter(|f| f.id >= 5)
        .map(|f| (f.id, f.name.clone()))
        .collect();
    ctx.render("pm/folders.html", minijinja::context! { title => "Manage Folders", breadcrumb => vec![("Private Messages".to_string(), "/pm".to_string())], custom => custom, folders => folder_list(&me) }).await
}

#[derive(Deserialize, Default)]
pub struct FoldersForm {
    #[serde(default, flatten)]
    pub fields: std::collections::HashMap<String, serde_json::Value>,
}

pub async fn folders_save(ctx: Ctx, CsrfForm(f): CsrfForm<FoldersForm>) -> AppResult<Response> {
    let me = require_pm(&ctx)?;
    let mut list: Vec<crate::models::PmFolder> = vec![];
    let mut max_id = me
        .pmfolders
        .0
        .iter()
        .map(|f| f.id)
        .max()
        .unwrap_or(4)
        .max(4);
    for existing in me.pmfolders.0.iter() {
        let key = format!("folder_{}", existing.id);
        let name = f
            .fields
            .get(&key)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| existing.name.clone());
        if name.is_empty() {
            // Deleting a folder moves its messages to the inbox.
            sqlx::query("UPDATE privatemessages SET folder = 1 WHERE uid = $1 AND folder = $2")
                .bind(me.uid)
                .bind(existing.id)
                .execute(&ctx.app.db)
                .await?;
        } else {
            list.push(crate::models::PmFolder {
                id: existing.id,
                name: name.chars().take(40).collect(),
            });
        }
    }
    if let Some(new) = f
        .fields
        .get("newfolder")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        if list.len() < 30 {
            max_id += 1;
            list.push(crate::models::PmFolder {
                id: max_id,
                name: new.chars().take(40).collect(),
            });
        }
    }
    sqlx::query("UPDATE users SET pmfolders = $2 WHERE uid = $1")
        .bind(me.uid)
        .bind(serde_json::to_value(&list).unwrap())
        .execute(&ctx.app.db)
        .await?;
    Ok(ctx.redirect("/pm/folders", "Your folders have been updated."))
}

pub async fn tracking(ctx: Ctx) -> AppResult<Response> {
    let me = require_pm(&ctx)?;
    if !ctx.perms.cantrackpms {
        return Err(AppError::no_perm());
    }
    let rows: Vec<(i32, String, i64, i16, i64, Option<String>, i32)> = sqlx::query_as(
        "SELECT p.pmid, p.subject, p.dateline, p.receipt, p.readtime, u.username, p.toid FROM privatemessages p LEFT JOIN users u ON u.uid = p.toid
         WHERE p.uid = $1 AND p.folder = 2 AND p.receipt > 0 ORDER BY p.dateline DESC LIMIT 200",
    )
    .bind(me.uid)
    .fetch_all(&ctx.app.db)
    .await?;
    let (read, unread): (Vec<_>, Vec<_>) = rows.into_iter().partition(|r| r.3 == 2);
    ctx.render("pm/tracking.html", minijinja::context! { title => "Message Tracking", breadcrumb => vec![("Private Messages".to_string(), "/pm".to_string())], read => read, unread => unread, folders => folder_list(&me) }).await
}

pub async fn tracking_action(ctx: Ctx, CsrfForm(f): CsrfForm<ActionForm>) -> AppResult<Response> {
    let me = require_pm(&ctx)?;
    // "Stop tracking" and cancelling unread messages (deletes the recipient copy if still unread).
    if f.action == "cancel" {
        for pmid in &f.pmids {
            let row: Option<(i32, i64)> = sqlx::query_as("SELECT toid, dateline FROM privatemessages WHERE pmid = $1 AND uid = $2 AND folder = 2").bind(pmid).bind(me.uid).fetch_optional(&ctx.app.db).await?;
            if let Some((toid, dl)) = row {
                sqlx::query("DELETE FROM privatemessages WHERE uid = $1 AND fromid = $2 AND dateline = $3 AND status = 0").bind(toid).bind(me.uid).bind(dl).execute(&ctx.app.db).await?;
                sqlx::query(
                    "UPDATE users SET unreadpms = (SELECT COUNT(*) FROM privatemessages WHERE uid = $1 AND status = 0 AND folder NOT IN (2,3)), totalpms = (SELECT COUNT(*) FROM privatemessages WHERE uid = $1) WHERE uid = $1",
                )
                .bind(toid)
                .execute(&ctx.app.db)
                .await?;
            }
        }
    }
    sqlx::query("UPDATE privatemessages SET receipt = 0 WHERE uid = $1 AND pmid = ANY($2)")
        .bind(me.uid)
        .bind(&f.pmids)
        .execute(&ctx.app.db)
        .await?;
    Ok(ctx.redirect("/pm/tracking", "Tracking updated."))
}
