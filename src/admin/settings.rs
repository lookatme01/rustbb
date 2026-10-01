//! Board settings editor.

use crate::ctx::{CsrfForm, Ctx};
use crate::error::{AppError, AppResult};
use crate::settings::{DEFS, GROUPS};
use axum::Router;
use axum::extract::{Path, Query};
use axum::response::Response;
use axum::routing::get;
use serde::Deserialize;
use std::collections::HashMap;

pub fn router() -> Router<crate::app::App> {
    Router::new()
        .route("/settings", get(index))
        .route("/settings/{group}", get(edit).post(save))
}

#[derive(Deserialize, Default)]
pub struct SearchQ {
    #[serde(default)]
    pub q: String,
}

pub async fn index(ctx: Ctx, Query(q): Query<SearchQ>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "settings");
    let groups: Vec<_> = GROUPS
        .iter()
        .map(|g| minijinja::context! { name => g.name, title => g.title, description => g.description, count => DEFS.iter().filter(|d| d.group == g.name).count() })
        .collect();
    let needle = q.q.to_lowercase();
    let results: Vec<_> = if needle.len() >= 2 {
        DEFS.iter()
            .filter(|d| d.title.to_lowercase().contains(&needle) || d.description.to_lowercase().contains(&needle) || d.name.contains(&needle))
            .map(|d| minijinja::context! { group => d.group, title => d.title, description => d.description })
            .collect()
    } else {
        vec![]
    };
    crate::admin::page(
        &ctx,
        "admin/settings_index.html",
        "settings",
        "Board Settings",
        minijinja::context! { groups => groups, q => q.q, results => results },
    )
    .await
}

pub async fn edit(ctx: Ctx, Path(group): Path<String>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "settings");
    let g = GROUPS
        .iter()
        .find(|g| g.name == group)
        .ok_or_else(|| AppError::not_found("setting group"))?;
    let s = ctx.settings();
    let items: Vec<_> = DEFS
        .iter()
        .filter(|d| d.group == group)
        .map(|d| {
            let (kind, options): (&str, Vec<(String, String)>) = if let Some(opts) = d.kind.strip_prefix("select:") {
                ("select", opts.split(',').filter_map(|o| o.split_once('=').map(|(a, b)| (a.to_string(), b.to_string()))).collect())
            } else {
                (d.kind, vec![])
            };
            let value = if d.kind == "password" { String::new() } else { s.get(d.name).to_string() };
            minijinja::context! { name => d.name, title => d.title, description => d.description, kind => kind, options => options, value => value, default => d.default }
        })
        .collect();
    crate::admin::page(
        &ctx,
        "admin/settings_edit.html",
        "settings",
        g.title,
        minijinja::context! { group => g, items => items, groups => GROUPS },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct AnyForm {
    #[serde(default, flatten)]
    pub fields: HashMap<String, serde_json::Value>,
}

pub async fn save(
    ctx: Ctx,
    Path(group): Path<String>,
    CsrfForm(f): CsrfForm<AnyForm>,
) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "settings");
    let mut changed = vec![];
    for d in DEFS.iter().filter(|d| d.group == group) {
        let raw = match f.fields.get(d.name) {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(a)) => {
                a.last().and_then(|x| x.as_str()).unwrap_or("").to_string()
            }
            _ => String::new(),
        };
        let value = match d.kind {
            "yesno" => {
                if raw == "1" {
                    "1".to_string()
                } else {
                    "0".to_string()
                }
            }
            "numeric" => raw
                .trim()
                .parse::<i64>()
                .map(|n| n.to_string())
                .map_err(|_| AppError::user(format!("“{}” must be a number.", d.title)))?,
            "password" if raw.is_empty() => continue,
            k if k.starts_with("select:") => {
                if !k[7..]
                    .split(',')
                    .any(|o| o.split('=').next() == Some(raw.as_str()))
                {
                    return Err(AppError::user(format!("Invalid choice for “{}”.", d.title)));
                }
                raw
            }
            _ => raw.replace("\r\n", "\n"),
        };
        if d.name == "timezone" && value.parse::<chrono_tz::Tz>().is_err() {
            return Err(AppError::user(
                "The default timezone must be a valid IANA timezone name, e.g. Europe/London.",
            ));
        }
        if d.name == "bburl" && !(value.starts_with("http://") || value.starts_with("https://")) {
            return Err(AppError::user(
                "The board URL must start with http:// or https://.",
            ));
        }
        if ctx.settings().get(d.name) != value {
            changed.push(d.name);
        }
        sqlx::query("INSERT INTO settings (name, value) VALUES ($1, $2) ON CONFLICT (name) DO UPDATE SET value = $2").bind(d.name).bind(&value).execute(&ctx.app.db).await?;
    }
    ctx.app.invalidate(&["settings"]).await?;
    if changed
        .iter()
        .any(|c| c.starts_with("sig") || *c == "linknofollow" || *c == "enablementions")
    {
        ctx.app.bump_parser_rev().await?;
    }
    crate::admin::log(
        &ctx,
        "settings",
        "Updated settings",
        serde_json::json!({"group": group, "changed": changed}),
    )
    .await;
    Ok(ctx.redirect(
        &format!("/admin/settings/{group}"),
        "The settings have been updated.",
    ))
}
