//! Public badge pages: every badge with what earns it, and who has one.

use crate::ctx::Ctx;
use crate::error::{AppError, AppResult};
use axum::extract::{Path, Query};
use axum::response::Response;
use serde::Deserialize;
use std::collections::HashMap;

pub async fn list(ctx: Ctx) -> AppResult<Response> {
    let counts: Vec<(i32, i64)> = sqlx::query_as(
        "SELECT ub.bid, COUNT(*) FROM user_badges ub JOIN users u ON u.uid = ub.uid
         WHERE u.usergroup <> 7 GROUP BY ub.bid",
    )
    .fetch_all(&ctx.app.db)
    .await?;
    let counts: HashMap<i32, i64> = counts.into_iter().collect();
    let mine: Vec<i32> = if ctx.uid() > 0 {
        sqlx::query_scalar("SELECT bid FROM user_badges WHERE uid = $1")
            .bind(ctx.uid())
            .fetch_all(&ctx.app.db)
            .await?
    } else {
        vec![]
    };
    let badges: Vec<_> = ctx
        .cache
        .badges
        .iter()
        .filter(|b| b.enabled)
        .map(|b| minijinja::context! { b => b, rule => crate::badges::describe(b), holders => counts.get(&b.bid).copied().unwrap_or(0), mine => mine.contains(&b.bid) })
        .collect();
    ctx.allow_guest_cache(&["board".to_string()]);
    ctx.render(
        "badges.html",
        minijinja::context! { title => "Badges", badges => badges },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct HoldersQ {
    /// Show members who earned it before this time (keyset by award time, then member id).
    pub before: Option<String>,
}

pub async fn holders(
    ctx: Ctx,
    Path(bid): Path<i32>,
    Query(q): Query<HoldersQ>,
) -> AppResult<Response> {
    if !ctx.perms.canviewprofiles {
        return Err(AppError::no_perm());
    }
    let b = ctx
        .cache
        .badge(bid)
        .filter(|b| b.enabled)
        .cloned()
        .ok_or_else(|| AppError::not_found("badge"))?;
    let (bt, bu) = q
        .before
        .as_deref()
        .and_then(|s| s.split_once('.'))
        .and_then(|(t, u)| Some((t.parse::<i64>().ok()?, u.parse::<i32>().ok()?)))
        .unwrap_or((i64::MAX, i32::MAX));
    const PER: usize = 50;
    let mut rows: Vec<(i32, String, i64)> = sqlx::query_as(
        "SELECT ub.uid, u.username, ub.dateline FROM user_badges ub JOIN users u ON u.uid = ub.uid
         WHERE ub.bid = $1 AND u.usergroup <> 7 AND (ub.dateline, ub.uid) < ($2, $3)
         ORDER BY ub.dateline DESC, ub.uid DESC LIMIT $4",
    )
    .bind(bid)
    .bind(bt)
    .bind(bu)
    .bind(PER as i64 + 1)
    .fetch_all(&ctx.app.db)
    .await?;
    let older = (rows.len() > PER).then(|| {
        rows.truncate(PER);
        let last = rows.last().unwrap();
        format!("/badges/{bid}?before={}.{}", last.2, last.0)
    });
    let avatars =
        crate::render::avatars(&ctx, &rows.iter().map(|r| r.0).collect::<Vec<_>>()).await?;
    let members: Vec<_> = rows
        .iter()
        .map(|r| minijinja::context! { uid => r.0, username => &r.1, dateline => r.2, avatar => avatars.get(&r.0).map(|a| a.to_string()).unwrap_or_default() })
        .collect();
    ctx.allow_guest_cache(&["board".to_string()]);
    ctx.render(
        "badge.html",
        minijinja::context! { title => format!("Badge: {}", b.name), b => b, rule => crate::badges::describe(&b), members => members, older => older, newest => q.before.is_some().then(|| format!("/badges/{bid}")) },
    )
    .await
}
