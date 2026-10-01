//! Reputation system.

use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::util::{self, now};
use axum::extract::{Path, Query};
use axum::response::Response;
use serde::Deserialize;

fn enabled(ctx: &Ctx) -> AppResult<()> {
    if !ctx.settings().bool("enablereputation") {
        return Err(AppError::user("The reputation system is disabled."));
    }
    Ok(())
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    pub page: Option<i64>,
    #[serde(default)]
    pub show: String,
}

pub async fn list(
    ctx: Ctx,
    Path(uid): Path<i32>,
    Query(q): Query<ListQuery>,
) -> AppResult<Response> {
    enabled(&ctx)?;
    let (username, usergroup, displaygroup, reputation): (String, i32, i32, i32) = sqlx::query_as(
        "SELECT username, usergroup, displaygroup, reputation FROM users WHERE uid = $1",
    )
    .bind(uid)
    .fetch_optional(&ctx.app.db)
    .await?
    .ok_or_else(|| AppError::not_found("user"))?;
    let cond = match q.show.as_str() {
        "positive" => "AND r.reputation > 0",
        "negative" => "AND r.reputation < 0",
        "neutral" => "AND r.reputation = 0",
        _ => "",
    };
    let total: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM reputation r WHERE r.uid = $1 {cond}"
    ))
    .bind(uid)
    .fetch_one(&ctx.app.db)
    .await?;
    let per = ctx.settings().int("repsperpage").max(5);
    let pg = util::paginate(
        total,
        per,
        util::clamp_page(q.page),
        &format!("/reputation/{uid}?show={}&page={{page}}", q.show),
    );
    let rows: Vec<(i32, i32, i32, i32, i64, String, Option<String>, Option<i32>, Option<i32>, Option<i32>, Option<String>)> = sqlx::query_as(&format!(
        "SELECT r.rid, r.adduid, r.pid, r.reputation, r.dateline, r.comments, u.username, u.usergroup, u.displaygroup, p.tid, t.subject
         FROM reputation r LEFT JOIN users u ON u.uid = r.adduid LEFT JOIN posts p ON p.pid = r.pid AND r.pid > 0 LEFT JOIN threads t ON t.tid = p.tid
         WHERE r.uid = $1 {cond} ORDER BY r.dateline DESC LIMIT $2 OFFSET $3"
    ))
    .bind(uid)
    .bind(per)
    .bind((pg.page - 1) * per)
    .fetch_all(&ctx.app.db)
    .await?;
    let opts = crate::parser::ParseOptions {
        allow_imgcode: false,
        allow_videocode: false,
        ..Default::default()
    };
    let list: Vec<_> = rows
        .into_iter()
        .map(|(rid, adduid, pid, rep, dl, comments, name, g, d, tid, subj)| {
            minijinja::context! {
                rid => rid, adduid => adduid, pid => pid, reputation => rep, dateline => dl,
                comments => crate::render::parse_with(&ctx.cache, &ctx.app.plugins, &opts, &comments),
                from => name.as_ref().map(|n| ctx.cache.format_name(n, g.unwrap_or(2), d.unwrap_or(0))), tid => tid, subject => subj,
                can_delete => (adduid == ctx.uid() && ctx.perms.candeletereputations) || ctx.is_supermod(),
            }
        })
        .collect();
    let stats: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*) FILTER (WHERE reputation > 0), COUNT(*) FILTER (WHERE reputation = 0), COUNT(*) FILTER (WHERE reputation < 0),
                COUNT(*) FILTER (WHERE reputation > 0 AND dateline > $2), COUNT(*) FILTER (WHERE reputation < 0 AND dateline > $2) FROM reputation WHERE uid = $1",
    )
    .bind(uid)
    .bind(now() - 30 * 86400)
    .fetch_one(&ctx.app.db)
    .await?;
    ctx.render(
        "reputation.html",
        minijinja::context! {
            title => format!("Reputation of {username}"), uid => uid, username => &username, formatted => ctx.cache.format_name(&username, usergroup, displaygroup),
            reputation => reputation, list => list, pagination => pg, show => q.show, stats => stats,
            can_give => ctx.uid() > 0 && ctx.uid() != uid && ctx.perms.cangivereputations,
        },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct AddQuery {
    pub pid: Option<i32>,
}

async fn check_can_give(
    ctx: &Ctx,
    uid: i32,
    pid: i32,
) -> AppResult<(String, Option<(i32, i32, String)>)> {
    enabled(ctx)?;
    let me = ctx.require_login()?;
    if !ctx.perms.cangivereputations {
        return Err(AppError::no_perm());
    }
    if me.uid == uid {
        return Err(AppError::user("You cannot add to your own reputation."));
    }
    let (username, g, ag): (String, i32, Vec<i32>) =
        sqlx::query_as("SELECT username, usergroup, additionalgroups FROM users WHERE uid = $1")
            .bind(uid)
            .fetch_optional(&ctx.app.db)
            .await?
            .ok_or_else(|| AppError::not_found("user"))?;
    let mut groups = vec![g];
    groups.extend(ag);
    if !ctx.cache.group_perms(&groups).usereputationsystem {
        return Err(AppError::user(
            "This user's group does not use the reputation system.",
        ));
    }
    let post = if pid > 0 && ctx.settings().bool("postrep") {
        let p: Option<(i32, i32, String)> = sqlx::query_as("SELECT p.tid, p.uid, t.subject FROM posts p JOIN threads t ON t.tid = p.tid WHERE p.pid = $1").bind(pid).fetch_optional(&ctx.app.db).await?;
        let p = p.ok_or_else(|| AppError::not_found("post"))?;
        if p.1 != uid {
            return Err(AppError::user("That post was not made by this user."));
        }
        crate::routes::showthread::check_thread(ctx, p.0).await?;
        Some(p)
    } else {
        None
    };
    Ok((username, post))
}

pub async fn add_form(
    ctx: Ctx,
    Path(uid): Path<i32>,
    Query(q): Query<AddQuery>,
) -> AppResult<Response> {
    let pid = q.pid.unwrap_or(0);
    let (username, post) = check_can_give(&ctx, uid, pid).await?;
    let existing: Option<(i32, String)> = sqlx::query_as(
        "SELECT reputation, comments FROM reputation WHERE uid = $1 AND adduid = $2 AND pid = $3",
    )
    .bind(uid)
    .bind(ctx.uid())
    .bind(if post.is_some() { pid } else { 0 })
    .fetch_optional(&ctx.app.db)
    .await?;
    let s = ctx.settings();
    let power = ctx.perms.reputationpower.max(1);
    let mut choices: Vec<(i32, String)> = vec![];
    if s.bool("posrep") {
        for i in (1..=power).rev() {
            choices.push((i, format!("Positive (+{i})")));
        }
    }
    if s.bool("neurep") {
        choices.push((0, "Neutral".into()));
    }
    if s.bool("negrep") {
        for i in 1..=power {
            choices.push((-i, format!("Negative (-{i})")));
        }
    }
    ctx.render(
        "reputation_add.html",
        minijinja::context! { title => format!("Rate {username}"), uid => uid, username => username, pid => if post.is_some() { pid } else { 0 }, post => post, existing => existing, choices => choices, maxlen => s.int("maxreplength") },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct AddForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub reputation: i32,
    #[serde(default, deserialize_with = "de::string")]
    pub comments: String,
    #[serde(default, deserialize_with = "de::i32")]
    pub pid: i32,
}

pub async fn add_submit(
    ctx: Ctx,
    Path(uid): Path<i32>,
    CsrfForm(f): CsrfForm<AddForm>,
) -> AppResult<Response> {
    let (username, post) = check_can_give(&ctx, uid, f.pid).await?;
    let me = ctx.me()?.clone();
    let s = ctx.settings();
    let power = ctx.perms.reputationpower.max(1);
    let rep = f.reputation;
    if rep.abs() > power
        || (rep > 0 && !s.bool("posrep"))
        || (rep < 0 && !s.bool("negrep"))
        || (rep == 0 && !s.bool("neurep"))
    {
        return Err(AppError::user("Invalid reputation value."));
    }
    let comments: String = f
        .comments
        .trim()
        .chars()
        .take(s.int("maxreplength").max(10) as usize)
        .collect();
    let pid = if post.is_some() { f.pid } else { 0 };
    let day = now() - 86400;
    let existing: Option<i32> = sqlx::query_scalar(
        "SELECT rid FROM reputation WHERE uid = $1 AND adduid = $2 AND pid = $3",
    )
    .bind(uid)
    .bind(me.uid)
    .bind(pid)
    .fetch_optional(&ctx.app.db)
    .await?;
    if existing.is_none() {
        if ctx.perms.maxreputationsday > 0 {
            let n: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM reputation WHERE adduid = $1 AND dateline > $2",
            )
            .bind(me.uid)
            .bind(day)
            .fetch_one(&ctx.app.db)
            .await?;
            if n >= ctx.perms.maxreputationsday as i64 {
                return Err(AppError::user(
                    "You have reached your reputation limit for today.",
                ));
            }
        }
        if ctx.perms.maxreputationsperuser > 0 {
            let n: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM reputation WHERE adduid = $1 AND uid = $2 AND dateline > $3",
            )
            .bind(me.uid)
            .bind(uid)
            .bind(day)
            .fetch_one(&ctx.app.db)
            .await?;
            if n >= ctx.perms.maxreputationsperuser as i64 {
                return Err(AppError::user(
                    "You have reached your reputation limit for this user today.",
                ));
            }
        }
        if pid == 0 && !s.bool("multirep") {
            // single overall rating per user: fall through to update below via existing check on pid 0
        }
    }
    match existing {
        Some(rid) => {
            sqlx::query("UPDATE reputation SET reputation = $2, comments = $3, dateline = $4 WHERE rid = $1").bind(rid).bind(rep).bind(&comments).bind(now()).execute(&ctx.app.db).await?;
        }
        None => {
            sqlx::query("INSERT INTO reputation (uid, adduid, pid, reputation, dateline, comments) VALUES ($1, $2, $3, $4, $5, $6)")
                .bind(uid)
                .bind(me.uid)
                .bind(pid)
                .bind(rep)
                .bind(now())
                .bind(&comments)
                .execute(&ctx.app.db)
                .await?;
        }
    }
    sqlx::query("UPDATE users SET reputation = (SELECT COALESCE(SUM(reputation), 0) FROM reputation WHERE uid = $1) WHERE uid = $1").bind(uid).execute(&ctx.app.db).await?;
    crate::notify::alert(
        &ctx.app,
        uid,
        me.uid,
        "reputation",
        uid,
        serde_json::json!({"reputation": rep, "poster": me.username}),
    )
    .await;
    Ok(ctx.redirect(
        &format!("/reputation/{uid}"),
        &format!("Your rating of {username} has been saved."),
    ))
}

#[derive(Deserialize, Default)]
pub struct Empty {}

pub async fn delete(
    ctx: Ctx,
    Path(rid): Path<i32>,
    CsrfForm(_): CsrfForm<Empty>,
) -> AppResult<Response> {
    let me = ctx.require_login()?;
    let (uid, adduid): (i32, i32) =
        sqlx::query_as("SELECT uid, adduid FROM reputation WHERE rid = $1")
            .bind(rid)
            .fetch_optional(&ctx.app.db)
            .await?
            .ok_or_else(|| AppError::not_found("rating"))?;
    if !((adduid == me.uid && ctx.perms.candeletereputations) || ctx.is_supermod()) {
        return Err(AppError::no_perm());
    }
    sqlx::query("DELETE FROM reputation WHERE rid = $1")
        .bind(rid)
        .execute(&ctx.app.db)
        .await?;
    sqlx::query("UPDATE users SET reputation = (SELECT COALESCE(SUM(reputation), 0) FROM reputation WHERE uid = $1) WHERE uid = $1").bind(uid).execute(&ctx.app.db).await?;
    Ok(ctx.redirect(
        &format!("/reputation/{uid}"),
        "The rating has been deleted.",
    ))
}
