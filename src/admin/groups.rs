//! User group management: permissions, display, leaders.

use crate::ctx::{CsrfForm, Ctx};
use crate::error::{AppError, AppResult};
use crate::perms::{GROUP_PERM_META, GroupPerms};
use axum::Router;
use axum::extract::{Path, Query};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use std::collections::HashMap;

pub fn router() -> Router<crate::app::App> {
    Router::new()
        .route("/groups", get(list))
        .route("/groups/edit", get(edit_form).post(edit_save))
        .route("/groups/delete", post(delete))
        .route("/groups/{gid}/leaders", get(leaders).post(leader_add))
        .route("/groups/{gid}/leaders/remove", post(leader_remove))
}

pub async fn list(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "groups");
    let counts: HashMap<i32, i64> =
        sqlx::query_as::<_, (i32, i64)>("SELECT usergroup, COUNT(*) FROM users GROUP BY usergroup")
            .fetch_all(&ctx.app.db)
            .await?
            .into_iter()
            .collect();
    let extra: HashMap<i32, i64> = sqlx::query_as::<_, (i32, i64)>(
        "SELECT g, COUNT(*) FROM users, unnest(additionalgroups) g GROUP BY g",
    )
    .fetch_all(&ctx.app.db)
    .await?
    .into_iter()
    .collect();
    let mut groups: Vec<_> = ctx.cache.groups.values().cloned().collect();
    groups.sort_by_key(|g| (g.disporder, g.gid));
    let rows: Vec<_> = groups
        .iter()
        .map(|g| minijinja::context! { gid => g.gid, title => &g.title, description => &g.description, kind => g.kind, styled => g.namestyle.replace("{username}", &crate::util::escape_html(&g.title)),
            primary => counts.get(&g.gid).copied().unwrap_or(0), secondary => extra.get(&g.gid).copied().unwrap_or(0) })
        .collect();
    crate::admin::page(
        &ctx,
        "admin/groups.html",
        "users",
        "User Groups",
        minijinja::context! { groups => rows },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct GidQ {
    pub gid: Option<i32>,
    pub copy: Option<i32>,
}

pub async fn edit_form(ctx: Ctx, Query(q): Query<GidQ>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "groups");
    let group = q.gid.and_then(|g| ctx.cache.group(g).cloned());
    let perms = group
        .as_ref()
        .map(|g| g.perms.0.clone())
        .or_else(|| {
            q.copy
                .and_then(|c| ctx.cache.group(c).map(|g| g.perms.0.clone()))
        })
        .unwrap_or_default();
    let j = serde_json::to_value(&perms).unwrap();
    let items: Vec<_> = GROUP_PERM_META
        .iter()
        .map(|m| minijinja::context! { name => m.name, title => m.title, section => m.section, is_bool => m.is_bool, value => if m.is_bool { j[m.name].as_bool().unwrap_or(false).to_string() } else { j[m.name].as_i64().unwrap_or(0).to_string() } })
        .collect();
    let sections: Vec<&str> = {
        let mut v: Vec<&str> = vec![];
        for m in GROUP_PERM_META {
            if !v.contains(&m.section) {
                v.push(m.section);
            }
        }
        v
    };
    crate::admin::page(&ctx, "admin/group_edit.html", "users", if group.is_some() { "Edit Group" } else { "Add Group" }, minijinja::context! { group => group, items => items, sections => sections, groups => crate::admin::users::sorted_groups(&ctx) }).await
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

pub async fn edit_save(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "groups");
    let fl = &f.fields;
    let gid: i32 = s(fl.get("gid")).parse().unwrap_or(0);
    let title = s(fl.get("title")).trim().to_string();
    if title.is_empty() {
        return Err(AppError::user("Please enter a title."));
    }
    let namestyle = {
        let n = s(fl.get("namestyle"));
        if n.contains("{username}") {
            n
        } else {
            "{username}".to_string()
        }
    };
    let mut j = serde_json::json!({});
    for m in GROUP_PERM_META {
        let raw = s(fl.get(m.name));
        j[m.name] = if m.is_bool {
            serde_json::Value::Bool(raw == "1")
        } else {
            serde_json::json!(raw.trim().parse::<i64>().unwrap_or(0))
        };
    }
    let perms: GroupPerms = serde_json::from_value(j).map_err(|e| AppError::Other(e.into()))?;
    // Protect against locking every admin out.
    if gid == 4 && !perms.cancp {
        return Err(AppError::user(
            "The Administrators group must keep Admin CP access.",
        ));
    }
    let kind: i16 = if gid > 0 && ctx.cache.group(gid).map(|g| g.kind == 1).unwrap_or(false) {
        1
    } else {
        s(fl.get("type")).parse::<i16>().unwrap_or(2).clamp(2, 4)
    };
    let stars: i16 = s(fl.get("stars")).parse().unwrap_or(0);
    let disporder: i32 = s(fl.get("disporder")).parse().unwrap_or(0);
    let isbanned = s(fl.get("isbannedgroup")) == "1";
    let pj = serde_json::to_value(&perms).unwrap();
    if gid > 0 {
        sqlx::query(
            "UPDATE usergroups SET title = $2, description = $3, namestyle = $4, usertitle = $5, stars = $6, starimage = $7, image = $8, disporder = $9, type = $10, isbannedgroup = $11, perms = $12 WHERE gid = $1",
        )
        .bind(gid)
        .bind(&title)
        .bind(s(fl.get("description")))
        .bind(&namestyle)
        .bind(s(fl.get("usertitle")))
        .bind(stars)
        .bind(s(fl.get("starimage")))
        .bind(s(fl.get("image")))
        .bind(disporder)
        .bind(kind)
        .bind(isbanned)
        .bind(&pj)
        .execute(&ctx.app.db)
        .await?;
    } else {
        sqlx::query("INSERT INTO usergroups (type, title, description, namestyle, usertitle, stars, starimage, image, disporder, isbannedgroup, perms) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
            .bind(kind)
            .bind(&title)
            .bind(s(fl.get("description")))
            .bind(&namestyle)
            .bind(s(fl.get("usertitle")))
            .bind(stars)
            .bind(s(fl.get("starimage")))
            .bind(s(fl.get("image")))
            .bind(disporder)
            .bind(isbanned)
            .bind(&pj)
            .execute(&ctx.app.db)
            .await?;
    }
    ctx.app.invalidate(&["groups"]).await?;
    crate::admin::log(
        &ctx,
        "groups",
        "Saved group",
        serde_json::json!({"gid": gid, "title": title}),
    )
    .await;
    Ok(ctx.redirect(
        "/admin/groups",
        &format!("The group “{title}” has been saved."),
    ))
}

pub async fn delete(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "groups");
    let gid: i32 = s(f.fields.get("gid")).parse().unwrap_or(0);
    let g = ctx
        .cache
        .group(gid)
        .cloned()
        .ok_or_else(|| AppError::not_found("group"))?;
    if g.kind == 1 {
        return Err(AppError::user("Default groups cannot be deleted."));
    }
    let mut tx = ctx.app.db.begin().await?;
    sqlx::query("UPDATE users SET usergroup = 2 WHERE usergroup = $1")
        .bind(gid)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE users SET additionalgroups = array_remove(additionalgroups, $1), displaygroup = CASE WHEN displaygroup = $1 THEN 0 ELSE displaygroup END WHERE additionalgroups @> ARRAY[$1]::int[] OR displaygroup = $1").bind(gid).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM moderators WHERE isgroup AND id = $1")
        .bind(gid)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM usergroups WHERE gid = $1")
        .bind(gid)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    ctx.app
        .invalidate(&["groups", "forumperms", "moderators"])
        .await?;
    crate::admin::log(
        &ctx,
        "groups",
        "Deleted group",
        serde_json::json!({"gid": gid, "title": g.title}),
    )
    .await;
    Ok(ctx.redirect(
        "/admin/groups",
        "The group has been deleted; its members were moved to Registered.",
    ))
}

pub async fn leaders(ctx: Ctx, Path(gid): Path<i32>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "groups");
    let g = ctx
        .cache
        .group(gid)
        .cloned()
        .ok_or_else(|| AppError::not_found("group"))?;
    let rows: Vec<(i32, i32, String, bool, bool, bool)> = sqlx::query_as(
        "SELECT l.lid, u.uid, u.username, l.canmanagemembers, l.canmanagerequests, l.caninvitemembers FROM groupleaders l JOIN users u ON u.uid = l.uid WHERE l.gid = $1",
    )
    .bind(gid)
    .fetch_all(&ctx.app.db)
    .await?;
    let requests: Vec<(i32, String, String, i64)> = sqlx::query_as("SELECT u.uid, u.username, r.reason, r.dateline FROM joinrequests r JOIN users u ON u.uid = r.uid WHERE r.gid = $1").bind(gid).fetch_all(&ctx.app.db).await?;
    crate::admin::page(
        &ctx,
        "admin/group_leaders.html",
        "users",
        &format!("Leaders: {}", g.title),
        minijinja::context! { group => &g, leaders => rows, requests => requests },
    )
    .await
}

pub async fn leader_add(
    ctx: Ctx,
    Path(gid): Path<i32>,
    CsrfForm(f): CsrfForm<AnyForm>,
) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "groups");
    let uid: i32 = sqlx::query_scalar("SELECT uid FROM users WHERE lower(username) = lower($1)")
        .bind(s(f.fields.get("username")).trim())
        .fetch_optional(&ctx.app.db)
        .await?
        .ok_or_else(|| AppError::not_found("user"))?;
    sqlx::query("INSERT INTO groupleaders (gid, uid, canmanagemembers, canmanagerequests, caninvitemembers) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (gid, uid) DO UPDATE SET canmanagemembers = $3, canmanagerequests = $4, caninvitemembers = $5")
        .bind(gid)
        .bind(uid)
        .bind(s(f.fields.get("canmanagemembers")) == "1")
        .bind(s(f.fields.get("canmanagerequests")) == "1")
        .bind(s(f.fields.get("caninvitemembers")) == "1")
        .execute(&ctx.app.db)
        .await?;
    crate::routes::usercp::add_to_group(&ctx.app.db, uid, gid).await?;
    Ok(ctx.redirect(
        &format!("/admin/groups/{gid}/leaders"),
        "The group leader has been added.",
    ))
}

pub async fn leader_remove(
    ctx: Ctx,
    Path(gid): Path<i32>,
    CsrfForm(f): CsrfForm<AnyForm>,
) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "groups");
    let lid: i32 = s(f.fields.get("lid")).parse().unwrap_or(0);
    sqlx::query("DELETE FROM groupleaders WHERE lid = $1 AND gid = $2")
        .bind(lid)
        .bind(gid)
        .execute(&ctx.app.db)
        .await?;
    Ok(ctx.redirect(
        &format!("/admin/groups/{gid}/leaders"),
        "The group leader has been removed.",
    ))
}
