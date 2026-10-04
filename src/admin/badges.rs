//! Badges: define them, see who has them, award and revoke them by hand.

use crate::admin::promotions::AnyForm;
use crate::app::App;
use crate::ctx::{CsrfForm, Ctx};
use crate::error::{AppError, AppResult};
use axum::Router;
use axum::extract::Query;
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;

pub fn router() -> Router<App> {
    Router::new()
        .route("/badges", get(list))
        .route("/badges/edit", get(edit_form).post(edit_save))
        .route("/badges/delete", post(delete))
        .route("/badges/holders", get(holders))
        .route("/badges/award", post(award))
        .route("/badges/revoke", post(revoke))
}

fn field(f: &AnyForm, k: &str) -> String {
    match f.fields.get(k) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(a)) => {
            a.last().and_then(|x| x.as_str()).unwrap_or("").to_string()
        }
        _ => String::new(),
    }
}

fn int(f: &AnyForm, k: &str) -> i32 {
    field(f, k).trim().parse().unwrap_or(0)
}

pub async fn list(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "badges");
    let counts: Vec<(i32, i64)> =
        sqlx::query_as("SELECT bid, COUNT(*) FROM user_badges GROUP BY bid")
            .fetch_all(&ctx.app.db)
            .await?;
    let counts: std::collections::HashMap<i32, i64> = counts.into_iter().collect();
    let list: Vec<_> = ctx
        .cache
        .badges
        .iter()
        .map(|b| minijinja::context! { b => b, rule => crate::badges::describe(b), holders => counts.get(&b.bid).copied().unwrap_or(0) })
        .collect();
    crate::admin::page(
        &ctx,
        "admin/badges.html",
        "users",
        "Badges",
        minijinja::context! { list => list },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct BidQ {
    pub bid: Option<i32>,
}

pub async fn edit_form(ctx: Ctx, Query(q): Query<BidQ>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "badges");
    let b = q.bid.and_then(|bid| ctx.cache.badge(bid).cloned());
    let reqs = b
        .as_ref()
        .map(|b| b.requirements.clone())
        .unwrap_or_else(|| serde_json::json!({}));
    let req_items: Vec<_> = crate::badges::requirements()
        .map(|(k, label, _)| minijinja::context! { key => k, label => label, enabled => reqs.get(*k).is_some(), op => reqs[*k][0].as_str().unwrap_or(">="), value => reqs[*k][1].as_i64().unwrap_or(0) })
        .collect();
    crate::admin::page(
        &ctx,
        "admin/badge_edit.html",
        "users",
        if b.is_some() { "Edit Badge" } else { "Add Badge" },
        minijinja::context! { b => b, reqs => req_items, icons => crate::badges::ICONS, colors => crate::badges::COLORS },
    )
    .await
}

pub async fn edit_save(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "badges");
    let bid = int(&f, "bid");
    let name = field(&f, "name").trim().to_string();
    if name.is_empty() {
        return Err(AppError::user("Please enter a name."));
    }
    let pick = |k: &str, options: &[(&str, &str)]| {
        let v = field(&f, k);
        options
            .iter()
            .find(|(o, _)| *o == v)
            .map(|(o, _)| o.to_string())
            .unwrap_or_else(|| options[0].0.to_string())
    };
    let icon = pick("icon", crate::badges::ICONS);
    let color = pick("color", crate::badges::COLORS);
    let mut reqs = serde_json::json!({});
    for (k, _, _) in crate::badges::requirements() {
        if field(&f, &format!("req_{k}")) == "1" {
            reqs[*k] = serde_json::json!([
                crate::admin::promotions::op_sql(&field(&f, &format!("op_{k}"))),
                field(&f, &format!("val_{k}"))
                    .trim()
                    .parse::<i64>()
                    .unwrap_or(0)
            ]);
        }
    }
    let q = if bid > 0 {
        sqlx::query(
            "UPDATE badges SET name = $2, description = $3, icon = $4, color = $5, requirements = $6, enabled = $7, disporder = $8 WHERE bid = $1",
        )
    } else {
        sqlx::query(
            "INSERT INTO badges (name, description, icon, color, requirements, enabled, disporder) VALUES ($2, $3, $4, $5, $6, $7, $8)",
        )
    };
    q.bind(bid)
        .bind(&name)
        .bind(field(&f, "description").trim())
        .bind(&icon)
        .bind(&color)
        .bind(&reqs)
        .bind(field(&f, "enabled") == "1")
        .bind(int(&f, "disporder"))
        .execute(&ctx.app.db)
        .await?;
    ctx.app.invalidate(&["badges"]).await?;
    crate::admin::log(
        &ctx,
        "badges",
        "Saved badge",
        serde_json::json!({"name": name}),
    )
    .await;
    Ok(ctx.redirect("/admin/badges", "The badge has been saved."))
}

pub async fn delete(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "badges");
    let bid = int(&f, "bid");
    let name: Option<String> =
        sqlx::query_scalar("DELETE FROM badges WHERE bid = $1 RETURNING name")
            .bind(bid)
            .fetch_optional(&ctx.app.db)
            .await?;
    ctx.app.invalidate(&["badges"]).await?;
    crate::admin::log(
        &ctx,
        "badges",
        "Deleted badge",
        serde_json::json!({"name": name}),
    )
    .await;
    Ok(ctx.redirect("/admin/badges", "The badge has been deleted."))
}

pub async fn holders(ctx: Ctx, Query(q): Query<BidQ>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "badges");
    let b = q
        .bid
        .and_then(|bid| ctx.cache.badge(bid).cloned())
        .ok_or_else(|| AppError::not_found("badge"))?;
    let rows: Vec<(i32, String, i64, bool, Option<String>, String, bool)> = sqlx::query_as(
        "SELECT ub.uid, u.username, ub.dateline, ub.manual, a.username, ub.reason, ub.hidden FROM user_badges ub
         JOIN users u ON u.uid = ub.uid LEFT JOIN users a ON a.uid = ub.awarded_by
         WHERE ub.bid = $1 ORDER BY ub.dateline DESC, ub.uid LIMIT 500",
    )
    .bind(b.bid)
    .fetch_all(&ctx.app.db)
    .await?;
    let rows: Vec<_> = rows
        .into_iter()
        .map(|r| minijinja::context! { uid => r.0, username => r.1, dateline => r.2, manual => r.3, by => r.4, reason => r.5, hidden => r.6 })
        .collect();
    crate::admin::page(
        &ctx,
        "admin/badge_holders.html",
        "users",
        &format!("Badge: {}", b.name),
        minijinja::context! { b => b, rule => crate::badges::describe(&b), rows => rows },
    )
    .await
}

pub async fn award(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "badges");
    let b = ctx
        .cache
        .badge(int(&f, "bid"))
        .cloned()
        .ok_or_else(|| AppError::not_found("badge"))?;
    let username = field(&f, "username").trim().to_string();
    let uid: Option<i32> =
        sqlx::query_scalar("SELECT uid FROM users WHERE lower(username) = lower($1)")
            .bind(&username)
            .fetch_optional(&ctx.app.db)
            .await?;
    let uid = uid.ok_or_else(|| AppError::user("There is no member with that username."))?;
    let reason: String = field(&f, "reason").trim().chars().take(500).collect();
    let back = format!("/admin/badges/holders?bid={}", b.bid);
    if !crate::badges::award(&ctx.app, uid, &b, ctx.uid(), &reason).await? {
        return Ok(ctx.redirect(
            &back,
            &format!("{username} already has this badge, or can't hold badges (guests, members awaiting activation, banned members and the System account can't)."),
        ));
    }
    crate::admin::log(
        &ctx,
        "badges",
        "Awarded badge",
        serde_json::json!({"badge": b.name, "uid": uid, "username": username, "reason": reason}),
    )
    .await;
    Ok(ctx.redirect(&back, &format!("{username} has been awarded the badge.")))
}

pub async fn revoke(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "badges");
    let (bid, uid) = (int(&f, "bid"), int(&f, "uid"));
    let name = ctx.cache.badge(bid).map(|b| b.name.clone());
    if crate::badges::revoke(&ctx.app.db, uid, bid).await? {
        crate::admin::log(
            &ctx,
            "badges",
            "Revoked badge",
            serde_json::json!({"badge": name, "uid": uid}),
        )
        .await;
    }
    Ok(ctx.redirect(
        &format!("/admin/badges/holders?bid={bid}"),
        "The badge has been revoked.",
    ))
}
