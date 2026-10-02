//! New thread / reply / edit / delete.

use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::models::{POST_COLUMNS, Post};
use crate::posting::{self, PollInput, PostInput, ThreadExtra};
use crate::routes::forumdisplay::breadcrumb;
use crate::routes::showthread::{check_thread, delete_allowed, edit_allowed};
use crate::templates::{url_forum, url_thread};
use crate::util::{self, now};
use axum::Json;
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Default, Debug)]
pub struct PostForm {
    #[serde(default, deserialize_with = "de::string")]
    pub subject: String,
    #[serde(default, deserialize_with = "de::string")]
    pub message: String,
    #[serde(default, deserialize_with = "de::i32")]
    pub icon: i32,
    #[serde(default, deserialize_with = "de::i32")]
    pub prefix: i32,
    #[serde(default, deserialize_with = "de::bool")]
    pub includesig: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub smilieoff: bool,
    #[serde(default, deserialize_with = "de::string")]
    pub posthash: String,
    #[serde(default, deserialize_with = "de::string")]
    pub preview: String,
    #[serde(default, deserialize_with = "de::string")]
    pub savedraft: String,
    #[serde(default, deserialize_with = "de::i32")]
    pub did: i32,
    #[serde(default, deserialize_with = "de::bool")]
    pub mod_sticky: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub mod_close: bool,
    #[serde(default, deserialize_with = "de::i32")]
    pub subscribe: i32,
    #[serde(default, deserialize_with = "de::bool")]
    pub postpoll: bool,
    #[serde(default, deserialize_with = "de::string")]
    pub poll_question: String,
    #[serde(default, deserialize_with = "de::string")]
    pub poll_options: String,
    #[serde(default, deserialize_with = "de::bool")]
    pub poll_multiple: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub poll_public: bool,
    #[serde(default, deserialize_with = "de::i64")]
    pub poll_timeout: i64,
    #[serde(default, deserialize_with = "de::i32")]
    pub poll_maxoptions: i32,
    #[serde(default, deserialize_with = "de::string")]
    pub captcha_hash: String,
    #[serde(default, deserialize_with = "de::string")]
    pub captcha: String,
    #[serde(default, deserialize_with = "de::string")]
    pub username: String,
    #[serde(default, deserialize_with = "de::i32")]
    pub replyto: i32,
    #[serde(default, deserialize_with = "de::string")]
    pub editreason: String,
    #[serde(default, deserialize_with = "de::bool")]
    pub silent: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub delete: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub harddelete: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub quickreply: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub as_system: bool,
}

#[derive(Serialize)]
pub struct SmilieView {
    pub find: String,
    pub image: String,
    pub name: String,
}

pub fn clickable_smilies(ctx: &Ctx) -> Vec<SmilieView> {
    ctx.cache
        .smilies
        .iter()
        .filter(|s| s.showclickable)
        .map(|s| SmilieView {
            find: s.find.lines().next().unwrap_or("").to_string(),
            image: s.image.clone(),
            name: s.name.clone(),
        })
        .collect()
}

async fn need_captcha(ctx: &Ctx) -> bool {
    ctx.uid() == 0
        && ctx.settings().bool("guestcaptcha")
        && ctx.settings().get("captchaimage") == "1"
}

fn preview_html(ctx: &Ctx, fid: i32, message: &str, smilieoff: bool) -> String {
    let mut opts =
        crate::render::forum_parse_options(ctx.cache.forum(fid), Some(ctx.username().to_string()));
    if smilieoff {
        opts.allow_smilies = false;
    }
    crate::render::parse_with(&ctx.cache, &ctx.app.plugins, &opts, message)
}

async fn attachments_for_hash(
    ctx: &Ctx,
    posthash: &str,
    pid: i32,
) -> AppResult<Vec<serde_json::Value>> {
    let rows: Vec<(i32, String, i64, String)> = sqlx::query_as(
        "SELECT aid, filename, filesize, filetype FROM attachments WHERE (posthash = $1 AND posthash <> '' AND uid = $3) OR (pid = $2 AND pid > 0) ORDER BY aid",
    )
    .bind(posthash)
    .bind(pid)
    .bind(ctx.uid())
    .fetch_all(&ctx.app.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(aid, name, size, ty)| serde_json::json!({"aid": aid, "filename": name, "size": util::format_bytes(size), "is_image": ty.starts_with("image/")}))
        .collect())
}

fn attach_allowed(ctx: &Ctx, fid: i32) -> bool {
    ctx.settings().bool("enableattachments")
        && ctx.forum_perms(fid).canpostattachments
        && ctx.perms.canpostattachments
        && ctx.uid() > 0
}

#[allow(clippy::too_many_arguments)]
async fn render_editor(
    ctx: &Ctx,
    mode: &str,
    fid: i32,
    tid: i32,
    pid: i32,
    form: &PostForm,
    errors: Vec<String>,
    preview: Option<String>,
    extra: minijinja::Value,
) -> AppResult<Response> {
    let forum = ctx
        .cache
        .forum(fid)
        .cloned()
        .ok_or_else(|| AppError::not_found("forum"))?;
    let posthash = if form.posthash.is_empty() {
        util::random_token(32)
    } else {
        form.posthash.clone()
    };
    let captcha = if need_captcha(ctx).await {
        Some(crate::routes::captcha::new_captcha(ctx).await?)
    } else {
        None
    };
    let is_mod = ctx.is_mod(fid);
    let attachments = attachments_for_hash(ctx, &posthash, pid).await?;
    let page = minijinja::context! {
        title => match mode { "newthread" => format!("Post New Thread in {}", forum.name), "newreply" => "Post Reply".to_string(), _ => "Edit Post".to_string() },
        mode => mode,
        forum => &forum,
        forum_url => url_forum(fid as i64, Some(&forum.name)),
        breadcrumb => breadcrumb(ctx, fid),
        tid => tid,
        pid => pid,
        form => minijinja::context! {
            subject => &form.subject, message => &form.message, icon => form.icon, prefix => form.prefix,
            includesig => form.includesig, smilieoff => form.smilieoff, posthash => posthash,
            mod_sticky => form.mod_sticky, mod_close => form.mod_close, subscribe => form.subscribe,
            postpoll => form.postpoll, poll_question => &form.poll_question, poll_options => &form.poll_options,
            poll_multiple => form.poll_multiple, poll_public => form.poll_public, poll_timeout => form.poll_timeout,
            username => &form.username, replyto => form.replyto, editreason => &form.editreason, did => form.did,
            as_system => form.as_system,
        },
        can_post_as_system => mode != "editpost" && ctx.uid() > 0 && ctx.perms.canpostassystem,
        errors => errors,
        preview => preview,
        icons => if forum.allowpicons { ctx.cache.icons.to_vec() } else { vec![] },
        prefixes => ctx.cache.prefixes_for(fid, &ctx.groups),
        smilies => clickable_smilies(ctx),
        captcha => captcha,
        is_mod => is_mod,
        can_attach => attach_allowed(ctx, fid),
        attachments => attachments,
        can_poll => mode == "newthread" && ctx.forum_perms(fid).canpostpolls && ctx.perms.canpostpolls,
        max_poll_options => ctx.settings().int("maxpolloptions"),
        can_draft => ctx.uid() > 0 && ctx.settings().bool("savedrafts"),
        max_attachments => ctx.settings().int("maxattachments"),
    };
    ctx.render("editor.html", minijinja::value::merge_maps([page, extra]))
        .await
}

fn default_form(ctx: &Ctx) -> PostForm {
    let subscribe = ctx
        .user
        .as_ref()
        .map(|u| u.subscriptionmethod as i32)
        .unwrap_or(0);
    PostForm {
        includesig: true,
        subscribe,
        ..Default::default()
    }
}

#[derive(Deserialize)]
pub struct DraftQuery {
    pub did: Option<i32>,
    pub pid: Option<i32>,
}

pub async fn newthread_form(
    ctx: Ctx,
    Path(fid): Path<i32>,
    Query(q): Query<DraftQuery>,
) -> AppResult<Response> {
    let (forum, fp) = ctx.check_forum(fid)?;
    if forum.is_category() || !forum.linkto.is_empty() {
        return Err(AppError::user("You cannot post threads in this forum."));
    }
    if !fp.canpostthreads || !ctx.perms.canpostthreads {
        return Err(AppError::no_perm());
    }
    if !forum.open && !ctx.is_mod(fid) {
        return Err(AppError::user("This forum is closed for new threads."));
    }
    ctx.set_location(fid, 0);
    let mut form = default_form(&ctx);
    if let Some(did) = q.did {
        if let Some((s, m)) = sqlx::query_as::<_, (String, String)>(
            "SELECT subject, message FROM drafts WHERE did = $1 AND uid = $2",
        )
        .bind(did)
        .bind(ctx.uid())
        .fetch_optional(&ctx.app.db)
        .await?
        {
            form.subject = s;
            form.message = m;
            form.did = did;
        }
    }
    render_editor(
        &ctx,
        "newthread",
        fid,
        0,
        0,
        &form,
        vec![],
        None,
        minijinja::context! {},
    )
    .await
}

async fn check_captcha_if_needed(ctx: &Ctx, form: &PostForm) -> AppResult<()> {
    if need_captcha(ctx).await {
        crate::routes::captcha::check(ctx, &form.captcha_hash, &form.captcha).await?;
    }
    Ok(())
}

fn parse_poll(ctx: &Ctx, form: &PostForm) -> AppResult<Option<PollInput>> {
    if !form.postpoll {
        return Ok(None);
    }
    let s = ctx.settings();
    let maxlen = s.int("polloptionlimit").max(10) as usize;
    let options: Vec<String> = form
        .poll_options
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.chars().take(maxlen).collect())
        .collect();
    if form.poll_question.trim().is_empty() {
        return Err(AppError::user("Please enter a poll question."));
    }
    if options.len() < 2 {
        return Err(AppError::user(
            "A poll needs at least two options (one per line).",
        ));
    }
    let max = s.int("maxpolloptions").max(2) as usize;
    if options.len() > max {
        return Err(AppError::user(format!(
            "A poll can have at most {max} options."
        )));
    }
    Ok(Some(PollInput {
        question: form.poll_question.trim().chars().take(250).collect(),
        options,
        multiple: form.poll_multiple,
        public: form.poll_public,
        timeout_days: form.poll_timeout.clamp(0, 3650),
        maxoptions: form.poll_maxoptions.max(0),
    }))
}

pub async fn newthread_submit(
    ctx: Ctx,
    Path(fid): Path<i32>,
    CsrfForm(form): CsrfForm<PostForm>,
) -> AppResult<Response> {
    let (forum, fp) = ctx.check_forum(fid)?;
    let forum = forum.clone();
    if forum.is_category()
        || !forum.linkto.is_empty()
        || !fp.canpostthreads
        || !ctx.perms.canpostthreads
    {
        return Err(AppError::no_perm());
    }
    if !forum.open && !ctx.is_mod(fid) {
        return Err(AppError::user("This forum is closed for new threads."));
    }
    if !form.savedraft.is_empty() && ctx.uid() > 0 {
        save_draft_row(&ctx, form.did, fid, 0, &form.subject, &form.message).await?;
        return Ok(ctx.redirect("/usercp/drafts", "Your draft has been saved."));
    }
    let input = PostInput {
        subject: form.subject.clone(),
        message: form.message.clone(),
        icon: form.icon,
        includesig: form.includesig,
        smilieoff: form.smilieoff,
        posthash: form.posthash.clone(),
        replyto: 0,
        as_system: form.as_system,
    };
    let mut errors = vec![];
    if let Err(e) = posting::validate(&ctx, &input, true) {
        errors.push(e.public_message());
    }
    if forum.requireprefix
        && form.prefix == 0
        && !ctx.cache.prefixes_for(fid, &ctx.groups).is_empty()
    {
        errors.push("You must select a thread prefix.".into());
    }
    if form.prefix > 0
        && !ctx
            .cache
            .prefixes_for(fid, &ctx.groups)
            .iter()
            .any(|p| p.pid == form.prefix)
    {
        errors.push("The selected prefix is not available in this forum.".into());
    }
    let poll = match parse_poll(&ctx, &form) {
        Ok(p) => p,
        Err(e) => {
            errors.push(e.public_message());
            None
        }
    };
    if !form.preview.is_empty() || !errors.is_empty() {
        let preview = if errors.is_empty() {
            Some(preview_html(&ctx, fid, &form.message, form.smilieoff))
        } else {
            None
        };
        return render_editor(
            &ctx,
            "newthread",
            fid,
            0,
            0,
            &form,
            errors,
            preview,
            minijinja::context! {},
        )
        .await;
    }
    if let Err(e) = check_captcha_if_needed(&ctx, &form).await {
        return render_editor(
            &ctx,
            "newthread",
            fid,
            0,
            0,
            &form,
            vec![e.public_message()],
            None,
            minijinja::context! {},
        )
        .await;
    }
    posting::check_posting_allowed(&ctx).await?;
    let is_mod = ctx.mod_perms(fid);
    let extra = ThreadExtra {
        prefix: form.prefix,
        sticky: form.mod_sticky
            && is_mod
                .as_ref()
                .map(|m| m.canstickunstickthreads)
                .unwrap_or(false),
        closed: form.mod_close
            && is_mod
                .as_ref()
                .map(|m| m.canopenclosethreads)
                .unwrap_or(false),
        poll,
    };
    let (tid, _pid, visible) = posting::create_thread(&ctx, fid, &input, &extra).await?;
    if form.did > 0 {
        let _ = sqlx::query("DELETE FROM drafts WHERE did = $1 AND uid = $2")
            .bind(form.did)
            .bind(ctx.uid())
            .execute(&ctx.app.db)
            .await;
    }
    apply_subscription(&ctx, tid, form.subscribe).await;
    ctx.write_scope(crate::pagecache::post_tags(&ctx.cache, fid, tid));
    if visible == 1 {
        Ok(ctx.redirect(
            &url_thread(tid as i64, Some(&form.subject)),
            "Thank you, your thread has been posted.",
        ))
    } else {
        Ok(ctx.redirect(
            &url_forum(fid as i64, Some(&forum.name)),
            "Thank you, your thread has been posted. It must be approved by a moderator before it is publicly visible.",
        ))
    }
}

async fn apply_subscription(ctx: &Ctx, tid: i32, method: i32) {
    if ctx.uid() == 0 {
        return;
    }
    if method > 0 {
        let notification = match method {
            2 => 1,
            3 => 2,
            _ => 0,
        };
        let _ = sqlx::query(
            "INSERT INTO threadsubscriptions (uid, tid, notification, dateline) VALUES ($1, $2, $3, $4)
             ON CONFLICT (uid, tid) DO UPDATE SET notification = $3",
        )
        .bind(ctx.uid())
        .bind(tid)
        .bind(notification as i16)
        .bind(now())
        .execute(&ctx.app.db)
        .await;
    }
}

pub fn build_quote(username: &str, pid: i32, dateline: i64, message: &str, depth: usize) -> String {
    let msg = match depth {
        0 => message.to_string(),
        1 => strip_all_quotes(message),
        d => crate::parser::limit_quote_depth(message, d - 1),
    };
    let name = username.replace('\'', "&#39;");
    format!(
        "[quote='{name}' pid='{pid}' dateline='{dateline}']\n{}\n[/quote]\n",
        msg.trim()
    )
}

fn strip_all_quotes(m: &str) -> String {
    crate::parser::limit_quote_depth(&format!("[quote]{m}[/quote]"), 1)
        .trim_start_matches("[quote]")
        .trim_end_matches("[/quote]")
        .to_string()
}

async fn quotes_for(ctx: &Ctx, tid: i32, pids: &[i32]) -> AppResult<String> {
    if pids.is_empty() {
        return Ok(String::new());
    }
    let rows: Vec<(i32, i32, String, i64, String, i16, i32)> = sqlx::query_as(
        "SELECT pid, tid, username, dateline, message, visible, fid FROM posts WHERE pid = ANY($1) ORDER BY dateline, pid",
    )
    .bind(pids)
    .fetch_all(&ctx.app.db)
    .await?;
    let depth = ctx.settings().int("maxquotedepth") as usize;
    let mut s = String::new();
    // Quoted pids come from the query string and the multiquote cookie, so each one must pass
    // the same checks as viewing its thread (forum password, "own threads only", thread
    // visibility), not just a forum-level view permission.
    let mut allowed: std::collections::HashMap<i32, bool> = std::collections::HashMap::new();
    for (pid, ptid, name, dl, msg, vis, _fid) in rows {
        if vis != 1 {
            continue;
        }
        let ok = match allowed.get(&ptid) {
            Some(ok) => *ok,
            None => {
                let ok = check_thread(ctx, ptid).await.is_ok();
                allowed.insert(ptid, ok);
                ok
            }
        };
        if !ok {
            continue;
        }
        let _ = tid;
        s.push_str(&build_quote(&name, pid, dl, &msg, depth));
        s.push('\n');
    }
    Ok(s)
}

pub async fn newreply_form(
    ctx: Ctx,
    Path(tid): Path<i32>,
    Query(q): Query<DraftQuery>,
) -> AppResult<Response> {
    let (thread, forum, fp) = check_thread(&ctx, tid).await?;
    check_can_reply(&ctx, &thread, &forum, &fp)?;
    ctx.set_location(thread.fid, tid);
    let mut form = default_form(&ctx);
    form.subject = format!("RE: {}", thread.subject);
    let mut pids: Vec<i32> = q.pid.into_iter().collect();
    if let Some(mq) = crate::ctx::get_cookie(&ctx.headers, "multiquote") {
        pids.extend(
            mq.split(',')
                .filter_map(|p| p.trim().parse::<i32>().ok())
                .take(50),
        );
        ctx.add_cookie("multiquote", "", Some(0), false);
    }
    pids.sort();
    pids.dedup();
    form.message = quotes_for(&ctx, tid, &pids).await?;
    form.replyto = q.pid.unwrap_or(0);
    if let Some(did) = q.did {
        if let Some((m,)) =
            sqlx::query_as::<_, (String,)>("SELECT message FROM drafts WHERE did = $1 AND uid = $2")
                .bind(did)
                .bind(ctx.uid())
                .fetch_optional(&ctx.app.db)
                .await?
        {
            form.message = m;
            form.did = did;
        }
    }
    let recent = recent_posts_for_review(&ctx, tid).await?;
    render_editor(&ctx, "newreply", thread.fid, tid, 0, &form, vec![], None, minijinja::context! { thread => &thread, thread_url => url_thread(tid as i64, Some(&thread.subject)), recent => recent }).await
}

async fn recent_posts_for_review(ctx: &Ctx, tid: i32) -> AppResult<Vec<serde_json::Value>> {
    let rows: Vec<(i32, String, i64, String, i32)> = sqlx::query_as(
        "SELECT pid, username, dateline, message, fid FROM posts WHERE tid = $1 AND visible = 1 ORDER BY dateline DESC, pid DESC LIMIT 5",
    )
    .bind(tid)
    .fetch_all(&ctx.app.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(pid, name, dl, msg, fid)| {
            serde_json::json!({"pid": pid, "username": name, "dateline": dl, "html": preview_html(ctx, fid, &msg, false)})
        })
        .collect())
}

fn check_can_reply(
    ctx: &Ctx,
    thread: &crate::models::Thread,
    forum: &crate::models::Forum,
    fp: &crate::perms::ForumPerms,
) -> AppResult<()> {
    if !fp.canpostreplys || !ctx.perms.canpostreplys {
        return Err(AppError::no_perm());
    }
    if fp.canonlyreplyownthreads && thread.uid != ctx.uid() {
        return Err(AppError::no_perm());
    }
    let mp = ctx.mod_perms(thread.fid);
    if (thread.is_closed() || !forum.open) && !mp.map(|m| m.canpostclosedthreads).unwrap_or(false) {
        return Err(AppError::user(
            "This thread is closed. You cannot post replies to it.",
        ));
    }
    if thread.visible == -1 {
        return Err(AppError::user("This thread has been deleted."));
    }
    Ok(())
}

pub async fn newreply_submit(
    ctx: Ctx,
    Path(tid): Path<i32>,
    CsrfForm(form): CsrfForm<PostForm>,
) -> AppResult<Response> {
    let (thread, forum, fp) = check_thread(&ctx, tid).await?;
    check_can_reply(&ctx, &thread, &forum, &fp)?;
    if !form.savedraft.is_empty() && ctx.uid() > 0 {
        save_draft_row(
            &ctx,
            form.did,
            thread.fid,
            tid,
            &form.subject,
            &form.message,
        )
        .await?;
        return Ok(ctx.redirect("/usercp/drafts", "Your draft has been saved."));
    }
    let subject = if form.subject.trim().is_empty() {
        format!("RE: {}", thread.subject)
    } else {
        form.subject.clone()
    };
    let input = PostInput {
        subject,
        message: form.message.clone(),
        icon: form.icon,
        includesig: form.includesig || form.quickreply,
        smilieoff: form.smilieoff,
        posthash: form.posthash.clone(),
        replyto: form.replyto,
        as_system: form.as_system,
    };
    let mut errors = vec![];
    if let Err(e) = posting::validate(&ctx, &input, false) {
        errors.push(e.public_message());
    }
    let extra = minijinja::context! { thread => &thread, thread_url => url_thread(tid as i64, Some(&thread.subject)) };
    if !form.preview.is_empty() || !errors.is_empty() {
        let preview = if errors.is_empty() {
            Some(preview_html(
                &ctx,
                thread.fid,
                &form.message,
                form.smilieoff,
            ))
        } else {
            None
        };
        return render_editor(
            &ctx, "newreply", thread.fid, tid, 0, &form, errors, preview, extra,
        )
        .await;
    }
    if let Err(e) = check_captcha_if_needed(&ctx, &form).await {
        return render_editor(
            &ctx,
            "newreply",
            thread.fid,
            tid,
            0,
            &form,
            vec![e.public_message()],
            None,
            extra,
        )
        .await;
    }
    if let Err(e) = posting::check_posting_allowed(&ctx).await {
        return render_editor(
            &ctx,
            "newreply",
            thread.fid,
            tid,
            0,
            &form,
            vec![e.public_message()],
            None,
            extra,
        )
        .await;
    }
    let guest_name = (ctx.uid() == 0).then_some(form.username.as_str());
    let (pid, visible, _merged) =
        posting::create_reply(&ctx, tid, thread.fid, &input, guest_name).await?;
    if form.did > 0 {
        let _ = sqlx::query("DELETE FROM drafts WHERE did = $1 AND uid = $2")
            .bind(form.did)
            .bind(ctx.uid())
            .execute(&ctx.app.db)
            .await;
    } else if ctx.uid() > 0 {
        let _ = sqlx::query("DELETE FROM drafts WHERE uid = $1 AND tid = $2")
            .bind(ctx.uid())
            .bind(tid)
            .execute(&ctx.app.db)
            .await;
    }
    apply_subscription(&ctx, tid, form.subscribe).await;
    // Moderator options on reply.
    if let Some(mp) = ctx.mod_perms(thread.fid) {
        if mp.canopenclosethreads && form.mod_close != thread.is_closed() && !form.quickreply {
            let _ = sqlx::query("UPDATE threads SET closed = $2 WHERE tid = $1")
                .bind(tid)
                .bind(if form.mod_close { "1" } else { "" })
                .execute(&ctx.app.db)
                .await;
        }
        if mp.canstickunstickthreads && form.mod_sticky != thread.sticky && !form.quickreply {
            let _ = sqlx::query("UPDATE threads SET sticky = $2 WHERE tid = $1")
                .bind(tid)
                .bind(form.mod_sticky)
                .execute(&ctx.app.db)
                .await;
        }
    }
    ctx.write_scope(crate::pagecache::post_tags(&ctx.cache, thread.fid, tid));
    if visible == 1 {
        Ok(ctx.redirect(&format!("/post/{pid}"), ""))
    } else {
        Ok(ctx.redirect(
            &url_thread(tid as i64, Some(&thread.subject)),
            "Thank you, your reply has been posted. It must be approved by a moderator before it is publicly visible.",
        ))
    }
}

async fn load_post(ctx: &Ctx, pid: i32) -> AppResult<Post> {
    sqlx::query_as(&format!("SELECT {POST_COLUMNS} FROM posts WHERE pid = $1"))
        .bind(pid)
        .fetch_optional(&ctx.app.db)
        .await?
        .ok_or_else(|| AppError::not_found("post"))
}

pub async fn editpost_form(ctx: Ctx, Path(pid): Path<i32>) -> AppResult<Response> {
    let post = load_post(&ctx, pid).await?;
    let (thread, _forum, fp) = check_thread(&ctx, post.tid).await?;
    let mp = ctx.mod_perms(thread.fid);
    if !edit_allowed(&ctx, &post, &thread, &fp, &mp) {
        return Err(AppError::user(
            "You do not have permission to edit this post, or the edit time limit has passed.",
        ));
    }
    let form = PostForm {
        subject: post.subject.clone(),
        message: post.message.clone(),
        icon: post.icon,
        prefix: thread.prefix,
        includesig: post.includesig,
        smilieoff: post.smilieoff,
        ..Default::default()
    };
    render_editor(
        &ctx,
        "editpost",
        thread.fid,
        thread.tid,
        pid,
        &form,
        vec![],
        None,
        minijinja::context! {
            thread => &thread, thread_url => url_thread(thread.tid as i64, Some(&thread.subject)),
            is_first => post.pid == thread.firstpost, can_delete => delete_allowed(&ctx, &post, &thread, &fp, &mp),
            post => &post,
        },
    )
    .await
}

pub async fn editpost_submit(
    ctx: Ctx,
    Path(pid): Path<i32>,
    CsrfForm(form): CsrfForm<PostForm>,
) -> AppResult<Response> {
    let post = load_post(&ctx, pid).await?;
    let (thread, _forum, fp) = check_thread(&ctx, post.tid).await?;
    let mp = ctx.mod_perms(thread.fid);
    if form.delete {
        return do_delete(&ctx, &post, &thread, &fp, &mp, form.harddelete).await;
    }
    if !edit_allowed(&ctx, &post, &thread, &fp, &mp) {
        return Err(AppError::user(
            "You do not have permission to edit this post, or the edit time limit has passed.",
        ));
    }
    let is_first = post.pid == thread.firstpost;
    let input = PostInput {
        subject: form.subject.clone(),
        message: form.message.clone(),
        icon: form.icon,
        includesig: form.includesig,
        smilieoff: form.smilieoff,
        posthash: String::new(),
        replyto: 0,
        as_system: false,
    };
    let extra = minijinja::context! {
        thread => &thread, thread_url => url_thread(thread.tid as i64, Some(&thread.subject)), is_first => is_first,
        can_delete => delete_allowed(&ctx, &post, &thread, &fp, &mp), post => &post,
    };
    let mut errors = vec![];
    if let Err(e) = posting::validate(&ctx, &input, is_first) {
        errors.push(e.public_message());
    }
    if !form.preview.is_empty() || !errors.is_empty() {
        let preview = if errors.is_empty() {
            Some(preview_html(
                &ctx,
                thread.fid,
                &form.message,
                form.smilieoff,
            ))
        } else {
            None
        };
        return render_editor(
            &ctx, "editpost", thread.fid, thread.tid, pid, &form, errors, preview, extra,
        )
        .await;
    }
    let silent = form.silent && mp.is_some();
    let reason: String = form.editreason.chars().take(150).collect();
    let moderated = posting::edit_post(
        &ctx,
        pid,
        &form.subject,
        &form.message,
        &reason,
        form.icon,
        form.includesig,
        form.smilieoff,
        silent,
    )
    .await?;
    if is_first && form.prefix != thread.prefix {
        let allowed = form.prefix == 0
            || ctx
                .cache
                .prefixes_for(thread.fid, &ctx.groups)
                .iter()
                .any(|p| p.pid == form.prefix);
        if allowed {
            sqlx::query("UPDATE threads SET prefix = $2 WHERE tid = $1")
                .bind(thread.tid)
                .bind(form.prefix)
                .execute(&ctx.app.db)
                .await?;
        }
    }
    if mp.is_some() && post.uid != ctx.uid() {
        crate::ops::log_moderator_action(
            &ctx.app,
            ctx.uid(),
            &ctx.ip,
            thread.fid,
            thread.tid,
            pid,
            "Edited post",
            serde_json::json!({}),
        )
        .await;
    }
    ctx.write_scope(crate::pagecache::post_tags(
        &ctx.cache, thread.fid, thread.tid,
    ));
    if moderated {
        Ok(ctx.redirect(
            &url_thread(thread.tid as i64, None),
            "Your edit has been saved and is awaiting moderator approval.",
        ))
    } else {
        Ok(ctx.redirect(&format!("/post/{pid}"), ""))
    }
}

async fn do_delete(
    ctx: &Ctx,
    post: &Post,
    thread: &crate::models::Thread,
    fp: &crate::perms::ForumPerms,
    mp: &Option<crate::perms::ModPerms>,
    hard_requested: bool,
) -> AppResult<Response> {
    if !delete_allowed(ctx, post, thread, fp, mp) {
        return Err(AppError::no_perm());
    }
    let is_first = post.pid == thread.firstpost;
    let can_hard = mp
        .as_ref()
        .map(|m| {
            if is_first {
                m.candeletethreads
            } else {
                m.candeleteposts
            }
        })
        .unwrap_or(false);
    let can_soft = mp
        .as_ref()
        .map(|m| {
            if is_first {
                m.cansoftdeletethreads
            } else {
                m.cansoftdeleteposts
            }
        })
        .unwrap_or(false);
    let soft_setting = ctx.settings().bool("soft_delete");
    let hard = if mp.is_some() {
        (hard_requested || !can_soft) && can_hard
    } else {
        !soft_setting
    };
    if is_first {
        if hard {
            crate::ops::delete_threads(&ctx.app, &[thread.tid]).await?;
        } else {
            crate::ops::set_threads_visibility(&ctx.app, &[thread.tid], -1).await?;
        }
    } else if hard {
        crate::ops::delete_posts(&ctx.app, &[post.pid]).await?;
    } else {
        crate::ops::set_posts_visibility(&ctx.app, &[post.pid], -1).await?;
    }
    if mp.is_some() {
        crate::ops::log_moderator_action(
            &ctx.app,
            ctx.uid(),
            &ctx.ip,
            thread.fid,
            thread.tid,
            post.pid,
            if is_first {
                if hard {
                    "Deleted thread"
                } else {
                    "Soft deleted thread"
                }
            } else if hard {
                "Deleted post"
            } else {
                "Soft deleted post"
            },
            serde_json::json!({"subject": thread.subject}),
        )
        .await;
    }
    ctx.app.mod_counts.invalidate_all();
    let forum = ctx.cache.forum(thread.fid);
    if is_first {
        Ok(ctx.redirect(
            &url_forum(thread.fid as i64, forum.map(|f| f.name.as_str())),
            "The thread has been deleted.",
        ))
    } else {
        Ok(ctx.redirect(
            &url_thread(thread.tid as i64, Some(&thread.subject)),
            "The post has been deleted.",
        ))
    }
}

#[derive(Deserialize)]
pub struct DeleteForm {
    #[serde(default, deserialize_with = "de::bool")]
    pub hard: bool,
}

pub async fn deletepost(
    ctx: Ctx,
    Path(pid): Path<i32>,
    CsrfForm(f): CsrfForm<DeleteForm>,
) -> AppResult<Response> {
    let post = load_post(&ctx, pid).await?;
    let (thread, _, fp) = check_thread(&ctx, post.tid).await?;
    let mp = ctx.mod_perms(thread.fid);
    do_delete(&ctx, &post, &thread, &fp, &mp, f.hard).await
}

pub async fn restorepost(
    ctx: Ctx,
    Path(pid): Path<i32>,
    CsrfForm(_f): CsrfForm<DeleteForm>,
) -> AppResult<Response> {
    let post = load_post(&ctx, pid).await?;
    let (thread, _, _) = check_thread(&ctx, post.tid).await?;
    let mp = ctx.mod_perms(thread.fid).ok_or_else(AppError::no_perm)?;
    let is_first = post.pid == thread.firstpost;
    if is_first {
        if !mp.canrestorethreads {
            return Err(AppError::no_perm());
        }
        crate::ops::set_threads_visibility(&ctx.app, &[thread.tid], 1).await?;
    } else {
        if !mp.canrestoreposts {
            return Err(AppError::no_perm());
        }
        crate::ops::set_posts_visibility(&ctx.app, &[pid], 1).await?;
    }
    crate::ops::log_moderator_action(
        &ctx.app,
        ctx.uid(),
        &ctx.ip,
        thread.fid,
        thread.tid,
        pid,
        "Restored post",
        serde_json::json!({}),
    )
    .await;
    Ok(ctx.redirect(&format!("/post/{pid}"), "The post has been restored."))
}

pub async fn quote_json(ctx: Ctx, Path(pid): Path<i32>) -> AppResult<Response> {
    let post = load_post(&ctx, pid).await?;
    let (_thread, _, _) = check_thread(&ctx, post.tid).await?;
    if post.visible != 1 {
        return Err(AppError::not_found("post"));
    }
    let depth = ctx.settings().int("maxquotedepth") as usize;
    Ok(Json(serde_json::json!({"quote": build_quote(&post.username, post.pid, post.dateline, &post.message, depth)})).into_response())
}

#[derive(Deserialize)]
pub struct PreviewForm {
    #[serde(default, deserialize_with = "de::string")]
    pub message: String,
    #[serde(default, deserialize_with = "de::i32")]
    pub fid: i32,
}

pub async fn preview_json(ctx: Ctx, CsrfForm(f): CsrfForm<PreviewForm>) -> AppResult<Response> {
    if !ctx.app.rate_check(&format!("preview:{}", ctx.ip), 60, 60) {
        return Err(AppError::RateLimited);
    }
    let html = preview_html(&ctx, f.fid, &f.message, false);
    Ok(Json(serde_json::json!({"html": html})).into_response())
}

#[derive(Deserialize)]
pub struct DraftForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub did: i32,
    #[serde(default, deserialize_with = "de::i32")]
    pub fid: i32,
    #[serde(default, deserialize_with = "de::i32")]
    pub tid: i32,
    #[serde(default, deserialize_with = "de::string")]
    pub subject: String,
    #[serde(default, deserialize_with = "de::string")]
    pub message: String,
}

async fn save_draft_row(
    ctx: &Ctx,
    did: i32,
    fid: i32,
    tid: i32,
    subject: &str,
    message: &str,
) -> AppResult<i32> {
    if !ctx.settings().bool("savedrafts") {
        return Err(AppError::user("Drafts are disabled."));
    }
    let uid = ctx.me()?.uid;
    if did > 0 {
        let r = sqlx::query("UPDATE drafts SET subject = $3, message = $4, dateline = $5 WHERE did = $1 AND uid = $2")
            .bind(did)
            .bind(uid)
            .bind(subject)
            .bind(message)
            .bind(now())
            .execute(&ctx.app.db)
            .await?;
        if r.rows_affected() > 0 {
            return Ok(did);
        }
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM drafts WHERE uid = $1")
        .bind(uid)
        .fetch_one(&ctx.app.db)
        .await?;
    if count >= 100 {
        return Err(AppError::user(
            "You have too many saved drafts. Please delete some first.",
        ));
    }
    Ok(sqlx::query_scalar("INSERT INTO drafts (uid, fid, tid, subject, message, dateline) VALUES ($1, $2, $3, $4, $5, $6) RETURNING did")
        .bind(uid)
        .bind(fid)
        .bind(tid)
        .bind(subject)
        .bind(message)
        .bind(now())
        .fetch_one(&ctx.app.db)
        .await?)
}

pub async fn save_draft(ctx: Ctx, CsrfForm(f): CsrfForm<DraftForm>) -> AppResult<Response> {
    let did = save_draft_row(&ctx, f.did, f.fid, f.tid, &f.subject, &f.message).await?;
    Ok(Json(serde_json::json!({"did": did})).into_response())
}
