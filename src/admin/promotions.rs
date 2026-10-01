//! Group promotions: automatically move users between groups based on activity.

use crate::app::App;
use crate::ctx::{CsrfForm, Ctx};
use crate::error::{AppError, AppResult};
use crate::util::now;
use axum::Router;
use axum::extract::Query;
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use std::collections::HashMap;

pub fn router() -> Router<App> {
    Router::new()
        .route("/promotions", get(list))
        .route("/promotions/edit", get(edit_form).post(edit_save))
        .route("/promotions/delete", post(delete))
        .route("/promotions/logs", get(logs))
}

/// (key, label, SQL expression)
const REQS: &[(&str, &str, &str)] = &[
    ("posts", "Post count", "postnum"),
    ("threads", "Thread count", "threadnum"),
    (
        "registered_days",
        "Days registered",
        "(EXTRACT(EPOCH FROM now())::bigint - regdate) / 86400",
    ),
    ("reputation", "Reputation", "reputation"),
    ("referrals", "Referrals", "referrals"),
    ("warnings", "Warning points", "warningpoints"),
    ("timeonline_hours", "Hours online", "timeonline / 3600"),
];

fn op_sql(op: &str) -> &'static str {
    match op {
        ">" => ">",
        "<" => "<",
        "<=" => "<=",
        "=" => "=",
        "!=" => "<>",
        _ => ">=",
    }
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

pub async fn list(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "promotions");
    let rows: Vec<(i32, String, String, bool, i32, i64)> =
        sqlx::query_as("SELECT pid, title, description, enabled, newusergroup, lastrun FROM promotions ORDER BY pid").fetch_all(&ctx.app.db).await?;
    let list: Vec<_> = rows
        .into_iter()
        .map(|r| minijinja::context! { pid => r.0, title => r.1, description => r.2, enabled => r.3, newgroup => ctx.cache.group(r.4).map(|g| g.title.clone()), lastrun => r.5 })
        .collect();
    crate::admin::page(
        &ctx,
        "admin/promotions.html",
        "users",
        "Group Promotions",
        minijinja::context! { list => list },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct PidQ {
    pub pid: Option<i32>,
}

pub async fn edit_form(ctx: Ctx, Query(q): Query<PidQ>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "promotions");
    let row: Option<(i32, String, String, bool, bool, serde_json::Value, Vec<i32>, i32, String)> = match q.pid {
        Some(pid) => sqlx::query_as("SELECT pid, title, description, enabled, logging, requirements, originalusergroup, newusergroup, usergrouptype FROM promotions WHERE pid = $1")
            .bind(pid)
            .fetch_optional(&ctx.app.db)
            .await?,
        None => None,
    };
    let reqs = row
        .as_ref()
        .map(|r| r.5.clone())
        .unwrap_or_else(|| serde_json::json!({}));
    let req_items: Vec<_> = REQS
        .iter()
        .map(|(k, label, _)| minijinja::context! { key => k, label => label, enabled => reqs.get(*k).is_some(), op => reqs[*k][0].as_str().unwrap_or(">="), value => reqs[*k][1].as_i64().unwrap_or(0) })
        .collect();
    crate::admin::page(
        &ctx,
        "admin/promotion_edit.html",
        "users",
        if row.is_some() { "Edit Promotion" } else { "Add Promotion" },
        minijinja::context! { p => row, reqs => req_items, groups => crate::admin::users::sorted_groups(&ctx) },
    )
    .await
}

pub async fn edit_save(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "promotions");
    let fl = &f.fields;
    let pid: i32 = s(fl.get("pid")).parse().unwrap_or(0);
    let title = s(fl.get("title")).trim().to_string();
    if title.is_empty() {
        return Err(AppError::user("Please enter a title."));
    }
    let mut reqs = serde_json::json!({});
    for (k, _, _) in REQS {
        if s(fl.get(&format!("req_{k}"))) == "1" {
            reqs[*k] = serde_json::json!([
                s(fl.get(&format!("op_{k}"))),
                s(fl.get(&format!("val_{k}"))).parse::<i64>().unwrap_or(0)
            ]);
        }
    }
    if reqs.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        return Err(AppError::user("Choose at least one requirement."));
    }
    let orig: Vec<i32> = match fl.get("originalusergroup") {
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().and_then(|s| s.parse().ok()))
            .collect(),
        Some(serde_json::Value::String(x)) => {
            x.split(',').filter_map(|y| y.trim().parse().ok()).collect()
        }
        _ => vec![],
    };
    let newg: i32 = s(fl.get("newusergroup")).parse().unwrap_or(0);
    crate::system::guard_group(&ctx.cache, 0, &[newg])?;
    if ctx.cache.group(newg).is_none() {
        return Err(AppError::user("Choose the group to promote to."));
    }
    let gtype = if s(fl.get("usergrouptype")) == "secondary" {
        "secondary"
    } else {
        "primary"
    };
    if pid > 0 {
        sqlx::query("UPDATE promotions SET title = $2, description = $3, enabled = $4, logging = $5, requirements = $6, originalusergroup = $7, newusergroup = $8, usergrouptype = $9 WHERE pid = $1")
            .bind(pid)
            .bind(&title)
            .bind(s(fl.get("description")))
            .bind(s(fl.get("enabled")) == "1")
            .bind(s(fl.get("logging")) == "1")
            .bind(&reqs)
            .bind(&orig)
            .bind(newg)
            .bind(gtype)
            .execute(&ctx.app.db)
            .await?;
    } else {
        sqlx::query("INSERT INTO promotions (title, description, enabled, logging, requirements, originalusergroup, newusergroup, usergrouptype) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
            .bind(&title)
            .bind(s(fl.get("description")))
            .bind(s(fl.get("enabled")) == "1")
            .bind(s(fl.get("logging")) == "1")
            .bind(&reqs)
            .bind(&orig)
            .bind(newg)
            .bind(gtype)
            .execute(&ctx.app.db)
            .await?;
    }
    crate::admin::log(
        &ctx,
        "promotions",
        "Saved promotion",
        serde_json::json!({"title": title}),
    )
    .await;
    Ok(ctx.redirect("/admin/promotions", "The promotion has been saved."))
}

pub async fn delete(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "promotions");
    sqlx::query("DELETE FROM promotions WHERE pid = $1")
        .bind(s(f.fields.get("pid")).parse::<i32>().unwrap_or(0))
        .execute(&ctx.app.db)
        .await?;
    Ok(ctx.redirect("/admin/promotions", "The promotion has been deleted."))
}

pub async fn logs(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "promotions");
    let rows: Vec<(String, i32, Option<String>, String, i32, i64, String)> = sqlx::query_as(
        "SELECT p.title, l.uid, u.username, l.oldusergroup, l.newusergroup, l.dateline, l.type FROM promotionlogs l LEFT JOIN promotions p ON p.pid = l.pid LEFT JOIN users u ON u.uid = l.uid ORDER BY l.plid DESC LIMIT 200",
    )
    .fetch_all(&ctx.app.db)
    .await?;
    let rows: Vec<_> = rows.into_iter().map(|r| minijinja::context! { title => r.0, uid => r.1, username => r.2, old => r.3, new => ctx.cache.group(r.4).map(|g| g.title.clone()), dateline => r.5, kind => r.6 }).collect();
    crate::admin::page(
        &ctx,
        "admin/promotion_logs.html",
        "users",
        "Promotion Logs",
        minijinja::context! { rows => rows },
    )
    .await
}

/// Task: apply all enabled promotions.
pub async fn run_promotions(app: &App) -> anyhow::Result<String> {
    let promos: Vec<(i32, bool, serde_json::Value, Vec<i32>, i32, String)> =
        sqlx::query_as("SELECT pid, logging, requirements, originalusergroup, newusergroup, usergrouptype FROM promotions WHERE enabled").fetch_all(&app.db).await?;
    let mut total = 0;
    for (pid, logging, reqs, orig, newg, gtype) in promos {
        let mut conds = vec![];
        for (k, _, expr) in REQS {
            if let Some(r) = reqs.get(*k) {
                let op = op_sql(r[0].as_str().unwrap_or(">="));
                let v = r[1].as_i64().unwrap_or(0);
                conds.push(format!("{expr} {op} {v}"));
            }
        }
        if conds.is_empty() {
            continue;
        }
        let in_group = if orig.is_empty() {
            "TRUE".to_string()
        } else {
            "(usergroup = ANY($1) OR additionalgroups && $1)".to_string()
        };
        let not_already = if gtype == "primary" {
            "usergroup <> $2"
        } else {
            "NOT ($2 = ANY(additionalgroups)) AND usergroup <> $2"
        };
        let sql = format!(
            "SELECT uid, usergroup, additionalgroups FROM users WHERE {in_group} AND {not_already} AND usergroup NOT IN (1, 5, 7) AND NOT is_system AND {} LIMIT 5000",
            conds.join(" AND ")
        );
        let users: Vec<(i32, i32, Vec<i32>)> = sqlx::query_as(&sql)
            .bind(&orig)
            .bind(newg)
            .fetch_all(&app.db)
            .await?;
        for (uid, oldg, _) in &users {
            if gtype == "primary" {
                sqlx::query("UPDATE users SET usergroup = $2, displaygroup = 0 WHERE uid = $1")
                    .bind(uid)
                    .bind(newg)
                    .execute(&app.db)
                    .await?;
            } else {
                crate::routes::usercp::add_to_group(&app.db, *uid, newg)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            if logging {
                sqlx::query("INSERT INTO promotionlogs (pid, uid, oldusergroup, newusergroup, dateline, type) VALUES ($1, $2, $3, $4, $5, $6)")
                    .bind(pid)
                    .bind(uid)
                    .bind(oldg.to_string())
                    .bind(newg)
                    .bind(now())
                    .bind(&gtype)
                    .execute(&app.db)
                    .await?;
            }
        }
        total += users.len();
        sqlx::query("UPDATE promotions SET lastrun = $2 WHERE pid = $1")
            .bind(pid)
            .bind(now())
            .execute(&app.db)
            .await?;
    }
    Ok(format!("promoted {total} users"))
}
