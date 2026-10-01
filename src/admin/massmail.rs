//! Mass mail: email or PM a filtered set of members, delivered in batches by a task.

use crate::app::App;
use crate::ctx::{CsrfForm, Ctx};
use crate::error::{AppError, AppResult};
use crate::util::now;
use axum::Router;
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use std::collections::HashMap;

pub fn router() -> Router<App> {
    Router::new()
        .route("/massmail", get(list))
        .route("/massmail/new", get(form).post(save))
        .route("/massmail/cancel", post(cancel))
}

#[derive(Deserialize, Default)]
pub struct AnyForm {
    #[serde(default, flatten)]
    pub fields: HashMap<String, serde_json::Value>,
}

fn s(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(a)) => {
            a.last().and_then(|x| x.as_str()).unwrap_or("").to_string()
        }
        _ => String::new(),
    }
}

const WHERE: &str = "(cardinality($1::int[]) = 0 OR usergroup = ANY($1) OR additionalgroups && $1) AND postnum >= $2 AND ($3 = 0 OR regdate <= $3) AND ($4 = FALSE OR allownotices) AND uid > $5";

pub async fn list(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "massmail");
    let rows: Vec<(i32, String, i16, i64, i16, i32, i32)> =
        sqlx::query_as("SELECT mid, subject, type, dateline, status, sentcount, totalcount FROM massemails ORDER BY mid DESC LIMIT 100").fetch_all(&ctx.app.db).await?;
    crate::admin::page(
        &ctx,
        "admin/massmail.html",
        "users",
        "Mass Mail",
        minijinja::context! { rows => rows },
    )
    .await
}

pub async fn form(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "massmail");
    crate::admin::page(
        &ctx,
        "admin/massmail_new.html",
        "users",
        "New Mass Mail",
        minijinja::context! { groups => crate::admin::users::sorted_groups(&ctx) },
    )
    .await
}

pub async fn save(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "massmail");
    let subject = s(f.fields.get("subject")).trim().to_string();
    let message = s(f.fields.get("message")).trim().to_string();
    if subject.is_empty() || message.is_empty() {
        return Err(AppError::user("Please enter a subject and message."));
    }
    let kind: i16 = if s(f.fields.get("type")) == "pm" {
        1
    } else {
        0
    };
    let groups: Vec<i32> = match f.fields.get("groups") {
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().and_then(|s| s.parse().ok()))
            .collect(),
        Some(serde_json::Value::String(x)) => {
            x.split(',').filter_map(|y| y.trim().parse().ok()).collect()
        }
        _ => vec![],
    };
    let minposts: i32 = s(f.fields.get("minposts")).parse().unwrap_or(0);
    let regdays: i64 = s(f.fields.get("regdays")).parse().unwrap_or(0);
    let respect = s(f.fields.get("respect")) == "1" || kind == 0;
    let cond = serde_json::json!({"groups": groups, "minposts": minposts, "regbefore": if regdays > 0 { now() - regdays * 86400 } else { 0 }, "respect": respect});
    let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM users WHERE {WHERE}"))
        .bind(&groups)
        .bind(minposts)
        .bind(cond["regbefore"].as_i64().unwrap_or(0))
        .bind(respect)
        .bind(0)
        .fetch_one(&ctx.app.db)
        .await?;
    sqlx::query("INSERT INTO massemails (uid, subject, message, type, dateline, senddate, status, totalcount, conditions) VALUES ($1, $2, $3, $4, $5, $5, 1, $6, $7)")
        .bind(ctx.uid())
        .bind(&subject)
        .bind(&message)
        .bind(kind)
        .bind(now())
        .bind(total as i32)
        .bind(&cond)
        .execute(&ctx.app.db)
        .await?;
    crate::admin::log(
        &ctx,
        "massmail",
        "Queued mass mail",
        serde_json::json!({"subject": subject, "recipients": total}),
    )
    .await;
    Ok(ctx.redirect(
        "/admin/massmail",
        &format!("The message has been queued for {total} members."),
    ))
}

pub async fn cancel(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "massmail");
    let mid: i32 = s(f.fields.get("mid")).parse().unwrap_or(0);
    sqlx::query("UPDATE massemails SET status = 3 WHERE mid = $1")
        .bind(mid)
        .execute(&ctx.app.db)
        .await?;
    Ok(ctx.redirect("/admin/massmail", "The mailing has been stopped."))
}

/// Task: send the next batch of queued mass mailings.
pub async fn run_batch(app: &App) -> anyhow::Result<String> {
    let job: Option<(i32, String, String, i16, serde_json::Value, i32)> =
        sqlx::query_as("SELECT mid, subject, message, type, conditions, lastuid FROM massemails WHERE status IN (1, 2) ORDER BY mid LIMIT 1").fetch_optional(&app.db).await?;
    let Some((mid, subject, message, kind, cond, lastuid)) = job else {
        return Ok("nothing queued".into());
    };
    let groups: Vec<i32> = cond["groups"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_i64().map(|v| v as i32))
                .collect()
        })
        .unwrap_or_default();
    let users: Vec<(i32, String, String)> = sqlx::query_as(&format!(
        "SELECT uid, username, email FROM users WHERE {WHERE} ORDER BY uid LIMIT 250"
    ))
    .bind(&groups)
    .bind(cond["minposts"].as_i64().unwrap_or(0) as i32)
    .bind(cond["regbefore"].as_i64().unwrap_or(0))
    .bind(cond["respect"].as_bool().unwrap_or(true))
    .bind(lastuid)
    .fetch_all(&app.db)
    .await?;
    if users.is_empty() {
        sqlx::query("UPDATE massemails SET status = 3 WHERE mid = $1")
            .bind(mid)
            .execute(&app.db)
            .await?;
        return Ok(format!("mass mail {mid} complete"));
    }
    let s = app.cache().settings.clone();
    for (uid, name, email) in &users {
        // PMs are MyCode: a member's name must not be able to add markup to them.
        let shown_name = if kind == 1 { crate::parser::literal(name) } else { name.clone() };
        let body = message
            .replace("{username}", &shown_name)
            .replace("{bbname}", s.get("bbname"))
            .replace("{bburl}", s.get("bburl"));
        if kind == 1 {
            let _ = crate::routes::private::send_system_pm(app, *uid, &subject, &body).await;
        } else {
            crate::mail::queue(app, email, &subject, &body).await;
        }
    }
    let last = users.last().map(|u| u.0).unwrap_or(lastuid);
    sqlx::query(
        "UPDATE massemails SET status = 2, lastuid = $2, sentcount = sentcount + $3 WHERE mid = $1",
    )
    .bind(mid)
    .bind(last)
    .bind(users.len() as i32)
    .execute(&app.db)
    .await?;
    Ok(format!("sent {} messages for mass mail {mid}", users.len()))
}
