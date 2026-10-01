//! Thread polls.

use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::models::Thread;
use crate::perms::ForumPerms;
use crate::routes::showthread::check_thread;
use crate::templates::url_thread;
use crate::util::now;
use axum::extract::Path;
use axum::response::Response;
use serde::Deserialize;

#[derive(sqlx::FromRow, Clone)]
struct PollRow {
    pid: i32,
    question: String,
    options: Vec<String>,
    votes: Vec<i32>,
    numvotes: i32,
    timeout: i64,
    closed: bool,
    multiple: bool,
    public: bool,
    maxoptions: i32,
}

async fn load(ctx: &Ctx, tid: i32) -> AppResult<Option<PollRow>> {
    Ok(sqlx::query_as("SELECT pid, question, options, votes, numvotes, timeout, closed, multiple, public, maxoptions FROM polls WHERE tid = $1 ORDER BY pid LIMIT 1")
        .bind(tid)
        .fetch_optional(&ctx.app.db)
        .await?)
}

async fn my_votes(ctx: &Ctx, pid: i32) -> AppResult<Vec<i32>> {
    if ctx.uid() == 0 {
        return Ok(vec![]);
    }
    Ok(
        sqlx::query_scalar("SELECT voteoption FROM pollvotes WHERE pid = $1 AND uid = $2")
            .bind(pid)
            .bind(ctx.uid())
            .fetch_all(&ctx.app.db)
            .await?,
    )
}

pub async fn load_poll_view(
    ctx: &Ctx,
    t: &Thread,
    fp: &ForumPerms,
) -> AppResult<Option<serde_json::Value>> {
    let Some(p) = load(ctx, t.tid).await? else {
        return Ok(None);
    };
    build_view(ctx, t, fp, p, false).await.map(Some)
}

async fn build_view(
    ctx: &Ctx,
    t: &Thread,
    fp: &ForumPerms,
    p: PollRow,
    force_results: bool,
) -> AppResult<serde_json::Value> {
    let mine = my_votes(ctx, p.pid).await?;
    let closed = p.closed || (p.timeout > 0 && p.timeout < now()) || t.is_closed();
    let can_vote = !force_results
        && ctx.uid() > 0
        && mine.is_empty()
        && !closed
        && fp.canvotepolls
        && ctx.perms.canvotepolls;
    let voters: Vec<(i32, String)> = if p.public {
        sqlx::query_as("SELECT v.voteoption, u.username FROM pollvotes v JOIN users u ON u.uid = v.uid WHERE v.pid = $1 ORDER BY u.username").bind(p.pid).fetch_all(&ctx.app.db).await?
    } else {
        vec![]
    };
    let total: i32 = p.votes.iter().sum::<i32>().max(1);
    let opts_parse = crate::parser::ParseOptions {
        allow_imgcode: false,
        allow_videocode: false,
        ..Default::default()
    };
    let options: Vec<serde_json::Value> = p
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let v = p.votes.get(i).copied().unwrap_or(0);
            let n = (i + 1) as i32;
            serde_json::json!({
                "html": crate::render::parse_with(&ctx.cache, &ctx.app.plugins, &opts_parse, o),
                "votes": v,
                "percent": (v as f64 * 100.0 / total as f64).round() as i64,
                "mine": mine.contains(&n),
                "voters": voters.iter().filter(|x| x.0 == n).map(|x| x.1.clone()).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(serde_json::json!({
        "pid": p.pid, "question": p.question, "options": options, "numvotes": p.numvotes, "closed": closed, "timeout": p.timeout,
        "multiple": p.multiple, "public": p.public, "maxoptions": p.maxoptions, "can_vote": can_vote,
        "can_undo": !mine.is_empty() && !closed && ctx.perms.canundovotes,
    }))
}

#[derive(Deserialize, Default)]
pub struct VoteForm {
    #[serde(default, deserialize_with = "de::vec_i32")]
    pub option: Vec<i32>,
}

pub async fn vote(
    ctx: Ctx,
    Path(tid): Path<i32>,
    CsrfForm(f): CsrfForm<VoteForm>,
) -> AppResult<Response> {
    let me = ctx.require_login()?.clone();
    let (t, _, fp) = check_thread(&ctx, tid).await?;
    ctx.write_scope(vec![format!("thread:{tid}")]);
    let p = load(&ctx, tid)
        .await?
        .ok_or_else(|| AppError::not_found("poll"))?;
    if !fp.canvotepolls || !ctx.perms.canvotepolls {
        return Err(AppError::no_perm());
    }
    if p.closed || (p.timeout > 0 && p.timeout < now()) || t.is_closed() {
        return Err(AppError::user("This poll is closed."));
    }
    let mut opts: Vec<i32> = f
        .option
        .into_iter()
        .filter(|o| *o >= 1 && *o as usize <= p.options.len())
        .collect();
    opts.sort();
    opts.dedup();
    if opts.is_empty() {
        return Err(AppError::user("Please choose an option to vote for."));
    }
    if !p.multiple {
        opts.truncate(1);
    } else if p.maxoptions > 0 && opts.len() > p.maxoptions as usize {
        return Err(AppError::user(format!(
            "You can choose at most {} options.",
            p.maxoptions
        )));
    }
    let mut tx = ctx.app.db.begin().await?;
    sqlx::query("SELECT pid FROM polls WHERE pid = $1 FOR UPDATE")
        .bind(p.pid)
        .execute(&mut *tx)
        .await?;
    let voted: Option<i32> =
        sqlx::query_scalar("SELECT vid FROM pollvotes WHERE pid = $1 AND uid = $2 LIMIT 1")
            .bind(p.pid)
            .bind(me.uid)
            .fetch_optional(&mut *tx)
            .await?;
    if voted.is_some() {
        return Err(AppError::user("You have already voted in this poll."));
    }
    for o in &opts {
        sqlx::query("INSERT INTO pollvotes (pid, uid, voteoption, dateline, ipaddress) VALUES ($1, $2, $3, $4, $5)").bind(p.pid).bind(me.uid).bind(o).bind(now()).bind(&ctx.ip).execute(&mut *tx).await?;
        sqlx::query("UPDATE polls SET votes[$2] = votes[$2] + 1 WHERE pid = $1")
            .bind(p.pid)
            .bind(o)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE polls SET numvotes = numvotes + 1 WHERE pid = $1")
        .bind(p.pid)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(ctx.redirect(
        &url_thread(tid as i64, Some(&t.subject)),
        "Thank you for voting.",
    ))
}

#[derive(Deserialize, Default)]
pub struct Empty {}

pub async fn undo_vote(
    ctx: Ctx,
    Path(tid): Path<i32>,
    CsrfForm(_): CsrfForm<Empty>,
) -> AppResult<Response> {
    let me = ctx.require_login()?.clone();
    let (t, _, _) = check_thread(&ctx, tid).await?;
    if !ctx.perms.canundovotes {
        return Err(AppError::no_perm());
    }
    let p = load(&ctx, tid)
        .await?
        .ok_or_else(|| AppError::not_found("poll"))?;
    let mut tx = ctx.app.db.begin().await?;
    let removed: Vec<i32> = sqlx::query_scalar(
        "DELETE FROM pollvotes WHERE pid = $1 AND uid = $2 RETURNING voteoption",
    )
    .bind(p.pid)
    .bind(me.uid)
    .fetch_all(&mut *tx)
    .await?;
    for o in &removed {
        sqlx::query("UPDATE polls SET votes[$2] = GREATEST(votes[$2] - 1, 0) WHERE pid = $1")
            .bind(p.pid)
            .bind(o)
            .execute(&mut *tx)
            .await?;
    }
    if !removed.is_empty() {
        sqlx::query("UPDATE polls SET numvotes = GREATEST(numvotes - 1, 0) WHERE pid = $1")
            .bind(p.pid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(ctx.redirect(
        &url_thread(tid as i64, Some(&t.subject)),
        "Your vote has been removed.",
    ))
}

pub async fn results(ctx: Ctx, Path(tid): Path<i32>) -> AppResult<Response> {
    let (t, forum, fp) = check_thread(&ctx, tid).await?;
    let p = load(&ctx, tid)
        .await?
        .ok_or_else(|| AppError::not_found("poll"))?;
    let view = build_view(&ctx, &t, &fp, p, true).await?;
    ctx.render(
        "poll_results.html",
        minijinja::context! { title => "Poll Results", poll => view, thread => &t, thread_url => url_thread(tid as i64, Some(&t.subject)), breadcrumb => crate::routes::forumdisplay::breadcrumb(&ctx, forum.fid) },
    )
    .await
}

fn can_manage(ctx: &Ctx, t: &Thread) -> bool {
    ctx.mod_perms(t.fid)
        .map(|m| m.canmanagepolls)
        .unwrap_or(false)
}

pub async fn edit_form(ctx: Ctx, Path(tid): Path<i32>) -> AppResult<Response> {
    let (t, _, _) = check_thread(&ctx, tid).await?;
    if !can_manage(&ctx, &t) {
        return Err(AppError::no_perm());
    }
    let p = load(&ctx, tid)
        .await?
        .ok_or_else(|| AppError::not_found("poll"))?;
    ctx.render(
        "poll_form.html",
        minijinja::context! { title => "Edit Poll", edit => true, thread => &t, question => p.question, options => p.options.join("\n"), votes => p.votes, multiple => p.multiple, public => p.public, closed => p.closed, timeout => if p.timeout > 0 { (p.timeout - now()).max(0) / 86400 + 1 } else { 0 }, maxoptions => p.maxoptions },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct PollForm {
    #[serde(default, deserialize_with = "de::string")]
    pub question: String,
    #[serde(default, deserialize_with = "de::string")]
    pub options: String,
    #[serde(default, deserialize_with = "de::bool")]
    pub multiple: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub public: bool,
    #[serde(default, deserialize_with = "de::bool")]
    pub closed: bool,
    #[serde(default, deserialize_with = "de::i64")]
    pub timeout: i64,
    #[serde(default, deserialize_with = "de::i32")]
    pub maxoptions: i32,
}

fn parse_options(ctx: &Ctx, raw: &str) -> AppResult<Vec<String>> {
    let maxlen = ctx.settings().int("polloptionlimit").max(10) as usize;
    let opts: Vec<String> = raw
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.chars().take(maxlen).collect())
        .collect();
    let max = ctx.settings().int("maxpolloptions").max(2) as usize;
    if opts.len() < 2 {
        return Err(AppError::user("A poll needs at least two options."));
    }
    if opts.len() > max {
        return Err(AppError::user(format!(
            "A poll can have at most {max} options."
        )));
    }
    Ok(opts)
}

pub async fn edit_save(
    ctx: Ctx,
    Path(tid): Path<i32>,
    CsrfForm(f): CsrfForm<PollForm>,
) -> AppResult<Response> {
    let (t, _, _) = check_thread(&ctx, tid).await?;
    if !can_manage(&ctx, &t) {
        return Err(AppError::no_perm());
    }
    let p = load(&ctx, tid)
        .await?
        .ok_or_else(|| AppError::not_found("poll"))?;
    let opts = parse_options(&ctx, &f.options)?;
    // Keep vote counts for options that still exist at the same position.
    let votes: Vec<i32> = (0..opts.len())
        .map(|i| {
            if p.options.get(i) == opts.get(i) {
                p.votes.get(i).copied().unwrap_or(0)
            } else {
                0
            }
        })
        .collect();
    sqlx::query("DELETE FROM pollvotes WHERE pid = $1 AND voteoption > $2")
        .bind(p.pid)
        .bind(opts.len() as i32)
        .execute(&ctx.app.db)
        .await?;
    sqlx::query("UPDATE polls SET question = $2, options = $3, votes = $4, multiple = $5, public = $6, closed = $7, timeout = $8, maxoptions = $9 WHERE pid = $1")
        .bind(p.pid)
        .bind(f.question.trim())
        .bind(&opts)
        .bind(&votes)
        .bind(f.multiple)
        .bind(f.public)
        .bind(f.closed)
        .bind(if f.timeout > 0 { now() + f.timeout * 86400 } else { 0 })
        .bind(f.maxoptions.max(0))
        .execute(&ctx.app.db)
        .await?;
    crate::ops::log_moderator_action(
        &ctx.app,
        ctx.uid(),
        &ctx.ip,
        t.fid,
        tid,
        0,
        "Poll edited",
        serde_json::json!({}),
    )
    .await;
    Ok(ctx.redirect(
        &url_thread(tid as i64, Some(&t.subject)),
        "The poll has been updated.",
    ))
}

pub async fn new_form(ctx: Ctx, Path(tid): Path<i32>) -> AppResult<Response> {
    let (t, _, fp) = check_thread(&ctx, tid).await?;
    if t.poll > 0 {
        return Err(AppError::user("This thread already has a poll."));
    }
    if !((t.uid == ctx.uid() && ctx.uid() > 0 && fp.canpostpolls && ctx.perms.canpostpolls)
        || can_manage(&ctx, &t))
    {
        return Err(AppError::no_perm());
    }
    ctx.render("poll_form.html", minijinja::context! { title => "Post a Poll", edit => false, thread => &t, question => "", options => "", timeout => 0 }).await
}

pub async fn new_save(
    ctx: Ctx,
    Path(tid): Path<i32>,
    CsrfForm(f): CsrfForm<PollForm>,
) -> AppResult<Response> {
    let (t, _, fp) = check_thread(&ctx, tid).await?;
    if t.poll > 0 {
        return Err(AppError::user("This thread already has a poll."));
    }
    if !((t.uid == ctx.uid() && ctx.uid() > 0 && fp.canpostpolls && ctx.perms.canpostpolls)
        || can_manage(&ctx, &t))
    {
        return Err(AppError::no_perm());
    }
    if f.question.trim().is_empty() {
        return Err(AppError::user("Please enter a poll question."));
    }
    let opts = parse_options(&ctx, &f.options)?;
    let votes = vec![0i32; opts.len()];
    let pid: i32 = sqlx::query_scalar(
        "INSERT INTO polls (tid, question, dateline, options, votes, timeout, multiple, public, maxoptions) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING pid",
    )
    .bind(tid)
    .bind(f.question.trim())
    .bind(now())
    .bind(&opts)
    .bind(&votes)
    .bind(if f.timeout > 0 { now() + f.timeout * 86400 } else { 0 })
    .bind(f.multiple)
    .bind(f.public)
    .bind(f.maxoptions.max(0))
    .fetch_one(&ctx.app.db)
    .await?;
    sqlx::query("UPDATE threads SET poll = $2 WHERE tid = $1")
        .bind(tid)
        .bind(pid)
        .execute(&ctx.app.db)
        .await?;
    Ok(ctx.redirect(
        &url_thread(tid as i64, Some(&t.subject)),
        "Your poll has been added.",
    ))
}
