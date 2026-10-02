//! Assorted small endpoints: read markers, subscriptions, ratings, reactions, help, contact,
//! preferences and legacy MyBB URL redirects.

use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::routes::showthread::check_thread;
use crate::templates::{url_forum, url_thread};
use crate::util::{self, now};
use axum::Json;
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize, Default)]
pub struct Empty {}

fn back(ctx: &Ctx, fallback: &str) -> String {
    crate::routes::misc_back(ctx, fallback)
}

pub async fn mark_forum_read(
    ctx: Ctx,
    Path(fid): Path<i32>,
    CsrfForm(_): CsrfForm<Empty>,
) -> AppResult<Response> {
    // Only this member's own state changes; cached guest pages stay valid.
    ctx.write_scope(vec![]);
    let me = ctx.require_login()?;
    let mut fids = vec![fid];
    fids.extend(ctx.cache.descendants(fid));
    let t = now();
    sqlx::query(
        "INSERT INTO forumsread (fid, uid, dateline) SELECT f, $2, $3 FROM UNNEST($1::int[]) f ON CONFLICT (uid, fid) DO UPDATE SET dateline = $3",
    )
    .bind(&fids)
    .bind(me.uid)
    .bind(t)
    .execute(&ctx.app.db)
    .await?;
    let name = ctx.cache.forum(fid).map(|f| f.name.clone());
    Ok(ctx.redirect(
        &url_forum(fid as i64, name.as_deref()),
        "The forum has been marked as read.",
    ))
}

pub async fn mark_all_read(ctx: Ctx, CsrfForm(_): CsrfForm<Empty>) -> AppResult<Response> {
    // Only this member's own state changes; cached guest pages stay valid.
    ctx.write_scope(vec![]);
    let me = ctx.require_login()?;
    let fids: Vec<i32> = ctx.cache.forums.iter().map(|f| f.fid).collect();
    sqlx::query(
        "INSERT INTO forumsread (fid, uid, dateline) SELECT f, $2, $3 FROM UNNEST($1::int[]) f ON CONFLICT (uid, fid) DO UPDATE SET dateline = $3",
    )
    .bind(&fids)
    .bind(me.uid)
    .bind(now())
    .execute(&ctx.app.db)
    .await?;
    sqlx::query("UPDATE users SET lastvisit = $2 WHERE uid = $1")
        .bind(me.uid)
        .bind(now())
        .execute(&ctx.app.db)
        .await?;
    Ok(ctx.redirect("/", "All the forums have been marked as read."))
}

#[derive(Deserialize)]
pub struct PasswordForm {
    #[serde(default, deserialize_with = "de::string")]
    pub password: String,
    #[serde(default, deserialize_with = "de::string")]
    pub return_to: String,
}

pub async fn forum_password(
    ctx: Ctx,
    Path(fid): Path<i32>,
    CsrfForm(f): CsrfForm<PasswordForm>,
) -> AppResult<Response> {
    if !ctx
        .app
        .throttle(&format!("forumpw:{}", ctx.ip), 10, 300)
        .await
    {
        return Err(AppError::RateLimited);
    }
    let forum = ctx
        .cache
        .forum(fid)
        .ok_or_else(|| AppError::not_found("forum"))?;
    if !forum.has_password()
        || f.password.is_empty()
        || !crate::auth::verify_password(&f.password, &forum.password).await
    {
        return Err(AppError::user("The password you entered is incorrect."));
    }
    let token = crate::ctx::forum_unlock_token(&ctx.app.cfg.secret, forum);
    ctx.add_cookie(&format!("forumpass_{fid}"), &token, Some(30 * 86400), true);
    let to = if f.return_to.starts_with('/') {
        f.return_to.clone()
    } else {
        url_forum(fid as i64, Some(&forum.name))
    };
    Ok(ctx.redirect(&to, ""))
}

pub async fn subscribe_forum(
    ctx: Ctx,
    Path(fid): Path<i32>,
    CsrfForm(_): CsrfForm<Empty>,
) -> AppResult<Response> {
    // Only this member's own state changes; cached guest pages stay valid.
    ctx.write_scope(vec![]);
    let me = ctx.require_login()?;
    ctx.check_forum(fid)?;
    let deleted = sqlx::query("DELETE FROM forumsubscriptions WHERE uid = $1 AND fid = $2")
        .bind(me.uid)
        .bind(fid)
        .execute(&ctx.app.db)
        .await?;
    let msg = if deleted.rows_affected() > 0 {
        "You have been unsubscribed from this forum."
    } else {
        sqlx::query(
            "INSERT INTO forumsubscriptions (fid, uid) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(fid)
        .bind(me.uid)
        .execute(&ctx.app.db)
        .await?;
        "You have subscribed to this forum. You will be notified of new threads."
    };
    let name = ctx.cache.forum(fid).map(|f| f.name.clone());
    Ok(ctx.redirect(&url_forum(fid as i64, name.as_deref()), msg))
}

#[derive(Deserialize)]
pub struct SubForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub notification: i32,
    #[serde(default, deserialize_with = "de::bool")]
    pub unsubscribe: bool,
}

pub async fn subscribe_thread(
    ctx: Ctx,
    Path(tid): Path<i32>,
    CsrfForm(f): CsrfForm<SubForm>,
) -> AppResult<Response> {
    // Only this member's own state changes; cached guest pages stay valid.
    ctx.write_scope(vec![]);
    let me = ctx.require_login()?;
    let (thread, _, _) = check_thread(&ctx, tid).await?;
    let msg = if f.unsubscribe {
        sqlx::query("DELETE FROM threadsubscriptions WHERE uid = $1 AND tid = $2")
            .bind(me.uid)
            .bind(tid)
            .execute(&ctx.app.db)
            .await?;
        "You have been unsubscribed from this thread."
    } else {
        sqlx::query(
            "INSERT INTO threadsubscriptions (uid, tid, notification, dateline) VALUES ($1, $2, $3, $4) ON CONFLICT (uid, tid) DO UPDATE SET notification = $3",
        )
        .bind(me.uid)
        .bind(tid)
        .bind(f.notification.clamp(0, 2) as i16)
        .bind(now())
        .execute(&ctx.app.db)
        .await?;
        "You have subscribed to this thread."
    };
    Ok(ctx.redirect(
        &back(&ctx, &url_thread(tid as i64, Some(&thread.subject))),
        msg,
    ))
}

#[derive(Deserialize)]
pub struct RateForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub rating: i32,
}

pub async fn rate_thread(
    ctx: Ctx,
    Path(tid): Path<i32>,
    CsrfForm(f): CsrfForm<RateForm>,
) -> AppResult<Response> {
    let me = ctx.require_login()?;
    let (thread, forum, fp) = check_thread(&ctx, tid).await?;
    if !forum.allowtratings || !fp.canratethreads || !ctx.settings().bool("allowthreadratings") {
        return Err(AppError::no_perm());
    }
    if thread.uid == me.uid {
        return Err(AppError::user("You cannot rate your own thread."));
    }
    if !(1..=5).contains(&f.rating) {
        return Err(AppError::user("Invalid rating."));
    }
    let exists: Option<i32> =
        sqlx::query_scalar("SELECT rid FROM threadratings WHERE tid = $1 AND uid = $2")
            .bind(tid)
            .bind(me.uid)
            .fetch_optional(&ctx.app.db)
            .await?;
    if exists.is_some() {
        return Err(AppError::user("You have already rated this thread."));
    }
    sqlx::query("INSERT INTO threadratings (tid, uid, rating, ipaddress) VALUES ($1, $2, $3, $4)")
        .bind(tid)
        .bind(me.uid)
        .bind(f.rating as i16)
        .bind(crate::util::IpText::from(&ctx.ip))
        .execute(&ctx.app.db)
        .await?;
    sqlx::query("UPDATE threads SET numratings = numratings + 1, totalratings = totalratings + $2 WHERE tid = $1").bind(tid).bind(f.rating).execute(&ctx.app.db).await?;
    Ok(ctx.redirect(
        &url_thread(tid as i64, Some(&thread.subject)),
        "Thank you for rating this thread.",
    ))
}

#[derive(Deserialize)]
pub struct ReactForm {
    #[serde(default, deserialize_with = "de::string")]
    pub kind: String,
}

pub async fn reaction_summary(ctx: &Ctx, pid: i32) -> AppResult<Vec<serde_json::Value>> {
    let rows: Vec<(String, i64, bool)> = sqlx::query_as(
        "SELECT kind, COUNT(*), bool_or(uid = $2) FROM reactions WHERE pid = $1 GROUP BY kind",
    )
    .bind(pid)
    .bind(ctx.uid())
    .fetch_all(&ctx.app.db)
    .await?;
    let types = ctx.cache.reaction_types();
    Ok(types
        .iter()
        .filter_map(|(k, e)| {
            rows.iter()
                .find(|r| &r.0 == k)
                .map(|r| serde_json::json!({"kind": k, "emoji": e, "count": r.1, "mine": r.2}))
        })
        .collect())
}

pub async fn react(
    ctx: Ctx,
    Path(pid): Path<i32>,
    CsrfForm(f): CsrfForm<ReactForm>,
) -> AppResult<Response> {
    let me = ctx.require_login()?.clone();
    if !ctx.settings().bool("enablereactions") || !ctx.perms.canreact {
        return Err(AppError::no_perm());
    }
    if !ctx.cache.reaction_types().iter().any(|(k, _)| *k == f.kind) {
        return Err(AppError::user("Unknown reaction."));
    }
    if !ctx.app.rate_check(&format!("react:{}", me.uid), 60, 60) {
        return Err(AppError::RateLimited);
    }
    let (tid, puid, visible): (i32, i32, i16) =
        sqlx::query_as("SELECT tid, uid, visible FROM posts WHERE pid = $1")
            .bind(pid)
            .fetch_optional(&ctx.app.db)
            .await?
            .ok_or_else(|| AppError::not_found("post"))?;
    let (thread, _, _) = check_thread(&ctx, tid).await?;
    ctx.write_scope(vec![format!("thread:{tid}")]);
    if visible != 1 || puid == me.uid {
        return Err(AppError::user("You cannot react to this post."));
    }
    let removed = sqlx::query("DELETE FROM reactions WHERE pid = $1 AND uid = $2 AND kind = $3")
        .bind(pid)
        .bind(me.uid)
        .bind(&f.kind)
        .execute(&ctx.app.db)
        .await?;
    if removed.rows_affected() == 0 {
        sqlx::query("INSERT INTO reactions (pid, uid, kind, dateline) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING")
            .bind(pid)
            .bind(me.uid)
            .bind(&f.kind)
            .bind(now())
            .execute(&ctx.app.db)
            .await?;
        crate::notify::alert(&ctx.app, puid, me.uid, "reaction", pid, serde_json::json!({"tid": tid, "subject": thread.subject, "kind": f.kind, "poster": me.username})).await;
    }
    let summary = reaction_summary(&ctx, pid).await?;
    if ctx
        .headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .map(|a| a.contains("json"))
        .unwrap_or(false)
    {
        Ok(Json(serde_json::json!({"reactions": summary})).into_response())
    } else {
        Ok(Redirect::to(&format!("/post/{pid}")).into_response())
    }
}

pub async fn reactions_list(ctx: Ctx, Path(pid): Path<i32>) -> AppResult<Response> {
    let tid: i32 = sqlx::query_scalar("SELECT tid FROM posts WHERE pid = $1")
        .bind(pid)
        .fetch_optional(&ctx.app.db)
        .await?
        .ok_or_else(|| AppError::not_found("post"))?;
    check_thread(&ctx, tid).await?;
    let rows: Vec<(String, i32, String, i32, i32, i64)> = sqlx::query_as(
        "SELECT r.kind, u.uid, u.username, u.usergroup, u.displaygroup, r.dateline FROM reactions r JOIN users u ON u.uid = r.uid WHERE r.pid = $1 ORDER BY r.dateline",
    )
    .bind(pid)
    .fetch_all(&ctx.app.db)
    .await?;
    let types: HashMap<String, String> = ctx.cache.reaction_types().into_iter().collect();
    let list: Vec<_> = rows
        .into_iter()
        .map(|(k, uid, n, g, d, dl)| minijinja::context! { emoji => types.get(&k).cloned().unwrap_or(k.clone()), kind => k, uid => uid, formatted => ctx.cache.format_name(&n, g, d), dateline => dl })
        .collect();
    ctx.render(
        "reactions.html",
        minijinja::context! { title => "Reactions", pid => pid, list => list },
    )
    .await
}

pub async fn announcement(ctx: Ctx, Path(aid): Path<i32>) -> AppResult<Response> {
    let a = ctx
        .cache
        .announcements
        .iter()
        .find(|a| a.aid == aid)
        .cloned()
        .ok_or_else(|| AppError::not_found("announcement"))?;
    if a.fid > 0 {
        ctx.check_forum(a.fid)?;
    }
    let author = crate::render::load_authors(&ctx, &[a.uid])
        .await?
        .remove(&a.uid);
    let opts = crate::parser::ParseOptions {
        allow_html: a.allowhtml,
        allow_mycode: a.allowmycode,
        allow_smilies: a.allowsmilies,
        ..Default::default()
    };
    let html = crate::render::parse_with(&ctx.cache, &ctx.app.plugins, &opts, &a.message);
    let breadcrumb = if a.fid > 0 {
        crate::routes::forumdisplay::breadcrumb(&ctx, a.fid)
    } else {
        vec![]
    };
    ctx.allow_guest_cache(&[]);
    ctx.render("announcement.html", minijinja::context! { title => &a.subject, a => &a, html => html, author => author, breadcrumb => breadcrumb }).await
}

pub async fn help(ctx: Ctx) -> AppResult<Response> {
    if !ctx.settings().bool("enablehelp") {
        return Err(AppError::not_found("page"));
    }
    let sections: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT sid, name, description FROM helpsections WHERE enabled ORDER BY disporder, sid",
    )
    .fetch_all(&ctx.app.db)
    .await?;
    let docs: Vec<(i32, i32, String, String)> = sqlx::query_as(
        "SELECT hid, sid, name, description FROM helpdocs WHERE enabled ORDER BY disporder, hid",
    )
    .fetch_all(&ctx.app.db)
    .await?;
    let out: Vec<_> = sections
        .into_iter()
        .map(|(sid, name, desc)| {
            let d: Vec<_> = docs
                .iter()
                .filter(|x| x.1 == sid)
                .map(|x| minijinja::context! { hid => x.0, name => &x.2, description => &x.3 })
                .collect();
            minijinja::context! { sid => sid, name => name, description => desc, docs => d }
        })
        .collect();
    ctx.allow_guest_cache(&[]);
    ctx.render(
        "help.html",
        minijinja::context! { title => "Help Documents", sections => out },
    )
    .await
}

pub async fn help_doc(ctx: Ctx, Path(hid): Path<i32>) -> AppResult<Response> {
    let doc: (String, String, String) = sqlx::query_as(
        "SELECT name, description, document FROM helpdocs WHERE hid = $1 AND enabled",
    )
    .bind(hid)
    .fetch_optional(&ctx.app.db)
    .await?
    .ok_or_else(|| AppError::not_found("help document"))?;
    let html = crate::render::parse_with(&ctx.cache, &ctx.app.plugins, &Default::default(), &doc.2);
    ctx.allow_guest_cache(&[]);
    ctx.render("page.html", minijinja::context! { title => doc.0, subtitle => doc.1, html => html, breadcrumb => vec![("Help".to_string(), "/help".to_string())] }).await
}

pub async fn rules(ctx: Ctx) -> AppResult<Response> {
    let html = crate::render::parse_with(
        &ctx.cache,
        &ctx.app.plugins,
        &Default::default(),
        ctx.settings().get("tos"),
    );
    ctx.allow_guest_cache(&[]);
    ctx.render(
        "page.html",
        minijinja::context! { title => "Forum Rules", html => html },
    )
    .await
}

pub async fn privacy(ctx: Ctx) -> AppResult<Response> {
    let text = ctx.settings().get("privacypolicy");
    if text.is_empty() {
        return Err(AppError::not_found("page"));
    }
    let summary = crate::privacy::retention_summary(&crate::privacy::Retention::from_settings(
        ctx.settings(),
    ));
    let text = text.replace("{retention}", &crate::parser::literal(&summary));
    let html = crate::render::parse_with(&ctx.cache, &ctx.app.plugins, &Default::default(), &text);
    ctx.render(
        "page.html",
        minijinja::context! { title => "Privacy Policy", html => html },
    )
    .await
}

fn contact_allowed(ctx: &Ctx) -> AppResult<()> {
    let s = ctx.settings();
    if !s.bool("contact") || (s.bool("contact_guests") && ctx.uid() == 0) {
        return Err(AppError::no_perm());
    }
    Ok(())
}

pub async fn contact_form(ctx: Ctx) -> AppResult<Response> {
    contact_allowed(&ctx)?;
    let captcha = if ctx.uid() == 0 && ctx.settings().get("captchaimage") == "1" {
        Some(crate::routes::captcha::new_captcha(&ctx).await?)
    } else {
        None
    };
    ctx.render("contact.html", minijinja::context! { title => "Contact Us", captcha => captcha, errors => Vec::<String>::new(), form => minijinja::context!{} }).await
}

#[derive(Deserialize)]
pub struct ContactForm {
    #[serde(default, deserialize_with = "de::string")]
    pub subject: String,
    #[serde(default, deserialize_with = "de::string")]
    pub message: String,
    #[serde(default, deserialize_with = "de::string")]
    pub email: String,
    #[serde(default, deserialize_with = "de::string")]
    pub captcha_hash: String,
    #[serde(default, deserialize_with = "de::string")]
    pub captcha: String,
}

pub async fn contact_submit(ctx: Ctx, CsrfForm(f): CsrfForm<ContactForm>) -> AppResult<Response> {
    contact_allowed(&ctx)?;
    if !ctx
        .app
        .throttle(&format!("contact:{}", ctx.ip), 3, 3600)
        .await
    {
        return Err(AppError::RateLimited);
    }
    let email = ctx
        .user
        .as_ref()
        .map(|u| u.email.clone())
        .unwrap_or_else(|| f.email.trim().to_string());
    let mut errors = vec![];
    if f.subject.trim().is_empty() || f.message.trim().len() < 10 {
        errors.push("Please enter a subject and a message of at least 10 characters.".to_string());
    }
    if !util::valid_email(&email) {
        errors.push("Please enter a valid email address so we can reply.".into());
    }
    if errors.is_empty()
        && ctx.uid() == 0
        && ctx.settings().get("captchaimage") == "1"
        && let Err(e) = crate::routes::captcha::check(&ctx, &f.captcha_hash, &f.captcha).await
    {
        errors.push(e.public_message());
    }
    if !errors.is_empty() {
        let captcha = if ctx.uid() == 0 && ctx.settings().get("captchaimage") == "1" {
            Some(crate::routes::captcha::new_captcha(&ctx).await?)
        } else {
            None
        };
        return ctx
            .render("contact.html", minijinja::context! { title => "Contact Us", captcha => captcha, errors => errors, form => minijinja::context!{ subject => &f.subject, message => &f.message, email => &f.email } })
            .await;
    }
    let s = ctx.settings();
    let to = if s.get("contactemail").is_empty() {
        s.get("adminemail").to_string()
    } else {
        s.get("contactemail").to_string()
    };
    let body = format!(
        "{}\n\n------------------------------------------\nSent via the contact form by {} <{}> (IP {}).\n",
        f.message.trim(),
        ctx.username(),
        email,
        ctx.ip
    );
    crate::mail::queue(
        &ctx.app,
        &to,
        &format!("[{}] {}", s.get("bbname"), f.subject.trim()),
        &body,
    )
    .await;
    if s.bool("mail_logging") {
        sqlx::query("INSERT INTO maillogs (subject, message, dateline, fromuid, fromemail, toemail, ipaddress, type) VALUES ($1, $2, $3, $4, $5, $6, $7, 3)")
            .bind(f.subject.trim())
            .bind(f.message.trim())
            .bind(now())
            .bind(ctx.uid())
            .bind(&email)
            .bind(&to)
            .bind(crate::util::IpText::from(&ctx.ip))
            .execute(&ctx.app.db)
            .await?;
    }
    Ok(ctx.redirect(
        "/",
        "Thank you, your message has been sent to the board staff.",
    ))
}

pub async fn smilies(ctx: Ctx) -> AppResult<Response> {
    ctx.render(
        "smilies.html",
        minijinja::context! { title => "Smilies", smilies => ctx.cache.smilies.to_vec() },
    )
    .await
}

pub async fn mycode_help(ctx: Ctx) -> AppResult<Response> {
    let examples = [
        ("Bold", "[b]bold text[/b]"),
        ("Italic", "[i]italic text[/i]"),
        ("Underline", "[u]underlined text[/u]"),
        ("Strikethrough", "[s]struck text[/s]"),
        ("Link", "[url=https://example.com]a link[/url]"),
        ("Email", "[email]someone@example.com[/email]"),
        ("Image", "[img]https://example.com/image.png[/img]"),
        ("Colour", "[color=red]red text[/color]"),
        ("Size", "[size=large]large text[/size]"),
        ("Font", "[font=Georgia]Georgia text[/font]"),
        ("Alignment", "[align=center]centered[/align]"),
        ("Quote", "[quote='Jane']quoted text[/quote]"),
        ("Code", "[code=rust]fn main() {}[/code]"),
        ("List", "[list]\n[*]first\n[*]second\n[/list]"),
        ("Numbered list", "[list=1]\n[*]first\n[*]second\n[/list]"),
        ("Spoiler", "[spoiler=Ending]hidden text[/spoiler]"),
        (
            "Video",
            "[video=youtube]https://www.youtube.com/watch?v=dQw4w9WgXcQ[/video]",
        ),
        ("Horizontal rule", "[hr]"),
        ("Mention", "@admin"),
        ("Action", "/me waves hello"),
    ];
    let custom: Vec<(String, String)> =
        sqlx::query_as("SELECT title, description FROM mycode WHERE active ORDER BY parseorder")
            .fetch_all(&ctx.app.db)
            .await?;
    let opts = crate::parser::ParseOptions {
        me_username: Some(ctx.username().to_string()),
        ..Default::default()
    };
    let rows: Vec<_> = examples
        .iter()
        .map(|(n, c)| minijinja::context! { name => n, code => c, html => crate::render::parse_with(&ctx.cache, &ctx.app.plugins, &opts, c) })
        .collect();
    ctx.render(
        "mycode.html",
        minijinja::context! { title => "MyCode reference", rows => rows, custom => custom },
    )
    .await
}

pub async fn buddy_popup(ctx: Ctx) -> AppResult<Response> {
    let me = ctx.require_login()?;
    let cutoff = now() - ctx.settings().int("wolcutoffmins").max(1) * 60;
    let rows: Vec<(i32, String, i32, i32, i64, bool)> = sqlx::query_as(
        "SELECT uid, username, usergroup, displaygroup, lastactive, invisible FROM users WHERE uid = ANY($1) ORDER BY lastactive DESC",
    )
    .bind(&me.buddylist)
    .fetch_all(&ctx.app.db)
    .await?;
    let list: Vec<_> = rows
        .into_iter()
        .map(|(u, n, g, d, la, inv)| minijinja::context! { uid => u, formatted => ctx.cache.format_name(&n, g, d), online => la > cutoff && (!inv || ctx.perms.canviewwolinvis), lastactive => la })
        .collect();
    ctx.render(
        "buddies.html",
        minijinja::context! { title => "Friends", list => list },
    )
    .await
}

#[derive(Deserialize)]
pub struct ThemeForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub tid: i32,
}

pub async fn set_theme(
    ctx: Ctx,
    Path(tid): Path<i32>,
    CsrfForm(f): CsrfForm<ThemeForm>,
) -> AppResult<Response> {
    let tid = if tid > 0 { tid } else { f.tid };
    let theme = ctx
        .cache
        .theme(tid)
        .ok_or_else(|| AppError::not_found("theme"))?;
    if !(theme.allowedgroups.is_empty()
        || theme.allowedgroups.iter().any(|g| ctx.groups.contains(g)))
    {
        return Err(AppError::no_perm());
    }
    if let Some(u) = &ctx.user {
        sqlx::query("UPDATE users SET style = $2 WHERE uid = $1")
            .bind(u.uid)
            .bind(tid)
            .execute(&ctx.app.db)
            .await?;
    } else {
        ctx.add_cookie("rbb_theme", &tid.to_string(), Some(365 * 86400), false);
    }
    Ok(Redirect::to(&back(&ctx, "/")).into_response())
}

#[derive(Deserialize)]
pub struct LangForm {
    #[serde(default, deserialize_with = "de::string")]
    pub code: String,
}

pub async fn set_lang(
    ctx: Ctx,
    Path(code): Path<String>,
    CsrfForm(f): CsrfForm<LangForm>,
) -> AppResult<Response> {
    let code = if code.len() >= 2 && code != "x" {
        code
    } else {
        f.code
    };
    if !crate::i18n::available().iter().any(|(c, _)| *c == code) {
        return Err(AppError::not_found("language"));
    }
    let stored = if code == "en" {
        String::new()
    } else {
        code.clone()
    };
    if let Some(u) = &ctx.user {
        sqlx::query("UPDATE users SET language = $2 WHERE uid = $1")
            .bind(u.uid)
            .bind(&stored)
            .execute(&ctx.app.db)
            .await?;
    }
    ctx.add_cookie("rbb_lang", &code, Some(365 * 86400), false);
    Ok(Redirect::to(&back(&ctx, "/")).into_response())
}

#[derive(Deserialize)]
pub struct ModeForm {
    #[serde(default, deserialize_with = "de::string")]
    pub mode: String,
}

pub async fn set_colormode(ctx: Ctx, CsrfForm(f): CsrfForm<ModeForm>) -> AppResult<Response> {
    let mode = match f.mode.as_str() {
        "light" | "dark" => f.mode.clone(),
        _ => "auto".to_string(),
    };
    if let Some(u) = &ctx.user {
        sqlx::query("UPDATE users SET colormode = $2 WHERE uid = $1")
            .bind(u.uid)
            .bind(&mode)
            .execute(&ctx.app.db)
            .await?;
    } else {
        ctx.add_cookie("rbb_colormode", &mode, Some(365 * 86400), false);
    }
    Ok(Redirect::to(&back(&ctx, "/")).into_response())
}

#[derive(Deserialize, Default)]
pub struct LegacyQuery {
    pub tid: Option<i32>,
    pub pid: Option<i32>,
    pub fid: Option<i32>,
    pub uid: Option<i32>,
    pub page: Option<i64>,
    pub action: Option<String>,
}

pub async fn legacy_showthread(Query(q): Query<LegacyQuery>) -> Response {
    match (q.pid, q.tid) {
        (Some(pid), _) => Redirect::permanent(&format!("/post/{pid}")).into_response(),
        (None, Some(tid)) => {
            let page = q
                .page
                .filter(|p| *p > 1)
                .map(|p| format!("?page={p}"))
                .unwrap_or_default();
            Redirect::permanent(&format!("/thread/{tid}{page}")).into_response()
        }
        _ => Redirect::permanent("/").into_response(),
    }
}

pub async fn legacy_forumdisplay(Query(q): Query<LegacyQuery>) -> Response {
    match q.fid {
        Some(fid) => Redirect::permanent(&format!("/forum/{fid}")).into_response(),
        None => Redirect::permanent("/").into_response(),
    }
}

pub async fn legacy_member(Query(q): Query<LegacyQuery>) -> Response {
    match (q.action.as_deref(), q.uid) {
        (Some("profile"), Some(uid)) => {
            Redirect::permanent(&format!("/user/{uid}")).into_response()
        }
        (Some("login"), _) => Redirect::permanent("/member/login").into_response(),
        (Some("register"), _) => Redirect::permanent("/member/register").into_response(),
        (Some("lostpw"), _) => Redirect::permanent("/member/lostpw").into_response(),
        _ => Redirect::permanent("/").into_response(),
    }
}
