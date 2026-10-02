//! Forum management: tree, add/edit/delete/reorder, per-group permissions, moderators.

use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::perms::{FORUM_PERM_META, ForumPerms, MOD_PERM_META, ModPerms};
use axum::Router;
use axum::extract::{Path, Query};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use std::collections::HashMap;

pub fn router() -> Router<crate::app::App> {
    Router::new()
        .route("/forums", get(tree))
        .route("/forums/order", post(save_order))
        .route("/forums/edit", get(edit_form).post(edit_save))
        .route("/forums/delete", post(delete))
        .route(
            "/forums/{fid}/permissions",
            get(perms_matrix).post(perms_matrix_save),
        )
        .route(
            "/forums/{fid}/permissions/{gid}",
            get(perms_edit).post(perms_edit_save),
        )
        .route(
            "/forums/{fid}/moderators",
            get(moderators).post(moderator_add),
        )
        .route(
            "/forums/moderators/{mid}",
            get(moderator_edit).post(moderator_save),
        )
        .route("/forums/moderators/{mid}/delete", post(moderator_delete))
}

pub async fn tree(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "forums");
    let counters = crate::routes::index::load_counters(&ctx).await?;
    let perm_counts: HashMap<i32, usize> =
        ctx.cache
            .forum_perms
            .keys()
            .fold(HashMap::new(), |mut m, (f, _)| {
                *m.entry(*f).or_insert(0) += 1;
                m
            });
    let forums: Vec<_> = ctx
        .cache
        .forums
        .iter()
        .map(|f| {
            let c = counters.get(&f.fid).cloned().unwrap_or_default();
            minijinja::context! { fid => f.fid, name => &f.name, description => &f.description, depth => ctx.cache.forum_depth.get(&f.fid).copied().unwrap_or(0),
                category => f.is_category(), disporder => f.disporder, active => f.active, open => f.open, threads => c.threads, posts => c.posts,
                custom_perms => perm_counts.get(&f.fid).copied().unwrap_or(0), mods => ctx.cache.moderators.iter().filter(|m| m.fid == f.fid).count(), linkto => &f.linkto }
        })
        .collect();
    crate::admin::page(
        &ctx,
        "admin/forums.html",
        "forums",
        "Forum Management",
        minijinja::context! { forums => forums },
    )
    .await
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
fn b(v: Option<&serde_json::Value>) -> bool {
    matches!(s(v).as_str(), "1" | "on" | "true" | "yes")
}
fn i(v: Option<&serde_json::Value>) -> i32 {
    s(v).trim().parse().unwrap_or(0)
}

pub async fn save_order(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "forums");
    for (k, v) in &f.fields {
        if let Some(fid) = k.strip_prefix("order_").and_then(|x| x.parse::<i32>().ok()) {
            sqlx::query("UPDATE forums SET disporder = $2 WHERE fid = $1")
                .bind(fid)
                .bind(i(Some(v)))
                .execute(&ctx.app.db)
                .await?;
        }
    }
    ctx.app.invalidate(&["forums"]).await?;
    Ok(ctx.redirect("/admin/forums", "The display order has been saved."))
}

#[derive(Deserialize, Default)]
pub struct EditQ {
    pub fid: Option<i32>,
    pub pid: Option<i32>,
    #[serde(default)]
    pub r#type: String,
}

pub async fn edit_form(ctx: Ctx, Query(q): Query<EditQ>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "forums");
    let forum = match q.fid {
        Some(fid) => Some(
            ctx.cache
                .forum(fid)
                .cloned()
                .ok_or_else(|| AppError::not_found("forum"))?,
        ),
        None => None,
    };
    let parents: Vec<(i32, String, usize)> = ctx
        .cache
        .forums
        .iter()
        .filter(|f| {
            forum
                .as_ref()
                .map(|x| !f.parentlist.contains(&x.fid))
                .unwrap_or(true)
        })
        .map(|f| {
            (
                f.fid,
                f.name.clone(),
                ctx.cache.forum_depth.get(&f.fid).copied().unwrap_or(0),
            )
        })
        .collect();
    let password_set = forum.as_ref().map(|f| f.has_password()).unwrap_or(false);
    crate::admin::page(
        &ctx,
        "admin/forum_edit.html",
        "forums",
        if forum.is_some() { "Edit Forum" } else { "Add Forum" },
        minijinja::context! { forum => forum, parents => parents, pid => q.pid.unwrap_or(0), kind => if q.r#type == "c" { "c" } else { "f" }, all_themes => ctx.cache.themes.to_vec(), password_set => password_set },
    )
    .await
}

pub async fn edit_save(ctx: Ctx, CsrfForm(form): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "forums");
    let fl = &form.fields;
    let fid = i(fl.get("fid"));
    let name = s(fl.get("name")).trim().to_string();
    if name.is_empty() {
        return Err(AppError::user("Please enter a forum name."));
    }
    let pid = i(fl.get("pid"));
    let kind = if s(fl.get("type")) == "c" { "c" } else { "f" };
    if fid > 0
        && pid > 0
        && let Some(p) = ctx.cache.forum(pid)
        && p.parentlist.contains(&fid)
    {
        return Err(AppError::user(
            "A forum cannot be moved into one of its own subforums.",
        ));
    }
    let password = s(fl.get("password"));
    let clear_pw = b(fl.get("clearpassword"));
    // Forum passwords are stored as Argon2id verifiers; hash before the transaction (it is slow).
    let password_hash = if !clear_pw && !password.is_empty() {
        Some(crate::auth::hash_password(&password).await?)
    } else {
        None
    };
    let db = &ctx.app.db;
    let mut tx = db.begin().await?;
    let fid = if fid > 0 {
        sqlx::query(
            "UPDATE forums SET name = $2, description = $3, linkto = $4, type = $5, pid = $6, disporder = $7, active = $8, open = $9,
                allowhtml = $10, allowmycode = $11, allowsmilies = $12, allowimgcode = $13, allowvideocode = $14, allowpicons = $15, allowtratings = $16,
                usepostcounts = $17, usethreadcounts = $18, requireprefix = $19, showinjump = $20, style = $21, overridestyle = $22,
                rulestype = $23, rulestitle = $24, rules = $25, defaultdatecut = $26, defaultsortby = $27, defaultsortorder = $28
             WHERE fid = $1",
        )
        .bind(fid)
        .bind(&name)
        .bind(s(fl.get("description")))
        .bind(s(fl.get("linkto")).trim())
        .bind(kind)
        .bind(pid)
        .bind(i(fl.get("disporder")))
        .bind(b(fl.get("active")))
        .bind(b(fl.get("open")))
        .bind(b(fl.get("allowhtml")))
        .bind(b(fl.get("allowmycode")))
        .bind(b(fl.get("allowsmilies")))
        .bind(b(fl.get("allowimgcode")))
        .bind(b(fl.get("allowvideocode")))
        .bind(b(fl.get("allowpicons")))
        .bind(b(fl.get("allowtratings")))
        .bind(b(fl.get("usepostcounts")))
        .bind(b(fl.get("usethreadcounts")))
        .bind(b(fl.get("requireprefix")))
        .bind(b(fl.get("showinjump")))
        .bind(i(fl.get("style")))
        .bind(b(fl.get("overridestyle")))
        .bind(i(fl.get("rulestype")) as i16)
        .bind(s(fl.get("rulestitle")))
        .bind(s(fl.get("rules")))
        .bind(i(fl.get("defaultdatecut")))
        .bind(s(fl.get("defaultsortby")))
        .bind(s(fl.get("defaultsortorder")))
        .execute(&mut *tx)
        .await?;
        fid
    } else {
        sqlx::query_scalar(
            "INSERT INTO forums (name, description, linkto, type, pid, disporder, active, open, allowhtml, allowmycode, allowsmilies, allowimgcode,
                allowvideocode, allowpicons, allowtratings, usepostcounts, usethreadcounts, requireprefix, showinjump, style, overridestyle,
                rulestype, rulestitle, rules, defaultdatecut, defaultsortby, defaultsortorder)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25,$26,$27) RETURNING fid",
        )
        .bind(&name)
        .bind(s(fl.get("description")))
        .bind(s(fl.get("linkto")).trim())
        .bind(kind)
        .bind(pid)
        .bind(i(fl.get("disporder")))
        .bind(b(fl.get("active")))
        .bind(b(fl.get("open")))
        .bind(b(fl.get("allowhtml")))
        .bind(b(fl.get("allowmycode")))
        .bind(b(fl.get("allowsmilies")))
        .bind(b(fl.get("allowimgcode")))
        .bind(b(fl.get("allowvideocode")))
        .bind(b(fl.get("allowpicons")))
        .bind(b(fl.get("allowtratings")))
        .bind(b(fl.get("usepostcounts")))
        .bind(b(fl.get("usethreadcounts")))
        .bind(b(fl.get("requireprefix")))
        .bind(b(fl.get("showinjump")))
        .bind(i(fl.get("style")))
        .bind(b(fl.get("overridestyle")))
        .bind(i(fl.get("rulestype")) as i16)
        .bind(s(fl.get("rulestitle")))
        .bind(s(fl.get("rules")))
        .bind(i(fl.get("defaultdatecut")))
        .bind(s(fl.get("defaultsortby")))
        .bind(s(fl.get("defaultsortorder")))
        .fetch_one(&mut *tx)
        .await?
    };
    // A new or removed password changes the version, which signs everyone out of the forum.
    if clear_pw {
        sqlx::query("UPDATE forums SET password = '', password_version = password_version + 1 WHERE fid = $1")
            .bind(fid)
            .execute(&mut *tx)
            .await?;
    } else if let Some(h) = &password_hash {
        sqlx::query("UPDATE forums SET password = $2, password_version = password_version + 1 WHERE fid = $1")
            .bind(fid)
            .bind(h)
            .execute(&mut *tx)
            .await?;
    }
    // Recompute parentlists for the whole tree (cheap: forums are few).
    let all: Vec<(i32, i32)> = sqlx::query_as("SELECT fid, pid FROM forums")
        .fetch_all(&mut *tx)
        .await?;
    let parent: HashMap<i32, i32> = all.iter().copied().collect();
    for (f, _) in &all {
        let mut chain = vec![*f];
        let mut cur = *f;
        let mut guard = 0;
        while let Some(p) = parent.get(&cur).copied().filter(|p| *p > 0) {
            chain.push(p);
            cur = p;
            guard += 1;
            if guard > 50 {
                break;
            }
        }
        chain.reverse();
        sqlx::query("UPDATE forums SET parentlist = $2 WHERE fid = $1")
            .bind(f)
            .bind(&chain)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    ctx.app
        .invalidate(&["forums", "forumperms", "moderators"])
        .await?;
    ctx.app.bump_parser_rev().await?;
    crate::admin::log(
        &ctx,
        "forums",
        "Saved forum",
        serde_json::json!({"fid": fid, "name": name}),
    )
    .await;
    Ok(ctx.redirect(
        "/admin/forums",
        &format!("The forum “{name}” has been saved."),
    ))
}

#[derive(Deserialize, Default)]
pub struct FidForm {
    #[serde(default, deserialize_with = "de::i32")]
    pub fid: i32,
}

pub async fn delete(ctx: Ctx, CsrfForm(f): CsrfForm<FidForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "forums");
    let forum = ctx
        .cache
        .forum(f.fid)
        .cloned()
        .ok_or_else(|| AppError::not_found("forum"))?;
    let mut fids = vec![f.fid];
    fids.extend(ctx.cache.descendants(f.fid));
    let app = ctx.app.clone();
    let fids2 = fids.clone();
    // Content removal can be large; do it in the background in batches.
    tokio::spawn(async move {
        loop {
            let tids: Vec<i32> =
                sqlx::query_scalar("SELECT tid FROM threads WHERE fid = ANY($1) LIMIT 500")
                    .bind(&fids2)
                    .fetch_all(&app.db)
                    .await
                    .unwrap_or_default();
            if tids.is_empty() {
                break;
            }
            if let Err(e) = crate::ops::delete_threads(&app, &tids).await {
                tracing::error!("forum deletion failed: {e}");
                break;
            }
        }
        let _ = sqlx::query("DELETE FROM forums WHERE fid = ANY($1)")
            .bind(&fids2)
            .execute(&app.db)
            .await;
        let _ = app
            .invalidate(&["forums", "forumperms", "moderators"])
            .await;
    });
    // Hide immediately.
    sqlx::query("UPDATE forums SET active = FALSE WHERE fid = ANY($1)")
        .bind(&fids)
        .execute(&ctx.app.db)
        .await?;
    ctx.app.invalidate(&["forums"]).await?;
    crate::admin::log(
        &ctx,
        "forums",
        "Deleted forum",
        serde_json::json!({"fid": f.fid, "name": forum.name}),
    )
    .await;
    Ok(ctx.redirect(
        "/admin/forums",
        &format!("“{}” and its subforums are being deleted.", forum.name),
    ))
}

// ---------------------------------------------------------------- permissions

const QUICK: &[(&str, &str)] = &[
    ("canview", "View"),
    ("canviewthreads", "Read threads"),
    ("canpostthreads", "Post threads"),
    ("canpostreplys", "Post replies"),
    ("canpostpolls", "Post polls"),
    ("canpostattachments", "Upload"),
    ("candlattachments", "Download"),
    ("modposts", "Moderate posts"),
    ("modthreads", "Moderate threads"),
];

pub async fn perms_matrix(ctx: Ctx, Path(fid): Path<i32>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "forums");
    let forum = ctx
        .cache
        .forum(fid)
        .cloned()
        .ok_or_else(|| AppError::not_found("forum"))?;
    let mut groups: Vec<_> = ctx.cache.groups.values().cloned().collect();
    groups.sort_by_key(|g| (g.disporder, g.gid));
    let rows: Vec<_> = groups
        .iter()
        .map(|g| {
            let custom = ctx.cache.forum_perms.get(&(fid, g.gid)).cloned();
            let inherited = custom.is_none();
            let eff = ctx.cache.forum_perms(&[g.gid], fid);
            let effj = serde_json::to_value(&eff).unwrap();
            let vals: Vec<(String, bool)> = QUICK.iter().map(|(k, _)| (k.to_string(), effj[*k].as_bool().unwrap_or(false))).collect();
            minijinja::context! { gid => g.gid, title => &g.title, custom => !inherited, vals => vals }
        })
        .collect();
    crate::admin::page(
        &ctx,
        "admin/forum_perms.html",
        "forums",
        &format!("Permissions: {}", forum.name),
        minijinja::context! { forum => &forum, rows => rows, quick => QUICK },
    )
    .await
}

pub async fn perms_matrix_save(
    ctx: Ctx,
    Path(fid): Path<i32>,
    CsrfForm(f): CsrfForm<AnyForm>,
) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "forums");
    ctx.cache
        .forum(fid)
        .ok_or_else(|| AppError::not_found("forum"))?;
    for g in ctx.cache.groups.values() {
        let custom = b(f.fields.get(&format!("custom_{}", g.gid)));
        if !custom {
            sqlx::query("DELETE FROM forumpermissions WHERE fid = $1 AND gid = $2")
                .bind(fid)
                .bind(g.gid)
                .execute(&ctx.app.db)
                .await?;
            continue;
        }
        // Start from existing custom perms (or the inherited effective perms) and apply the quick toggles.
        let mut p = ctx
            .cache
            .forum_perms
            .get(&(fid, g.gid))
            .cloned()
            .unwrap_or_else(|| ctx.cache.forum_perms(&[g.gid], fid));
        let mut j = serde_json::to_value(&p).unwrap();
        for (k, _) in QUICK {
            j[*k] = serde_json::Value::Bool(b(f.fields.get(&format!("p_{}_{}", g.gid, k))));
        }
        p = serde_json::from_value(j).unwrap_or(p);
        sqlx::query("INSERT INTO forumpermissions (fid, gid, perms) VALUES ($1, $2, $3) ON CONFLICT (fid, gid) DO UPDATE SET perms = $3")
            .bind(fid)
            .bind(g.gid)
            .bind(serde_json::to_value(&p).unwrap())
            .execute(&ctx.app.db)
            .await?;
    }
    ctx.app.invalidate(&["forumperms"]).await?;
    crate::admin::log(
        &ctx,
        "forums",
        "Updated forum permissions",
        serde_json::json!({"fid": fid}),
    )
    .await;
    Ok(ctx.redirect(
        &format!("/admin/forums/{fid}/permissions"),
        "The forum permissions have been saved.",
    ))
}

pub async fn perms_edit(ctx: Ctx, Path((fid, gid)): Path<(i32, i32)>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "forums");
    let forum = ctx
        .cache
        .forum(fid)
        .cloned()
        .ok_or_else(|| AppError::not_found("forum"))?;
    let group = ctx
        .cache
        .group(gid)
        .cloned()
        .ok_or_else(|| AppError::not_found("group"))?;
    let custom = ctx.cache.forum_perms.get(&(fid, gid)).cloned();
    let p = custom
        .clone()
        .unwrap_or_else(|| ctx.cache.forum_perms(&[gid], fid));
    let j = serde_json::to_value(&p).unwrap();
    let items: Vec<_> = FORUM_PERM_META.iter().map(|m| minijinja::context! { name => m.name, title => m.title, section => m.section, value => j[m.name].as_bool().unwrap_or(false) }).collect();
    crate::admin::page(&ctx, "admin/forum_perms_edit.html", "forums", &format!("{} in {}", group.title, forum.name), minijinja::context! { forum => &forum, group => &group, items => items, custom => custom.is_some() }).await
}

pub async fn perms_edit_save(
    ctx: Ctx,
    Path((fid, gid)): Path<(i32, i32)>,
    CsrfForm(f): CsrfForm<AnyForm>,
) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "forums");
    if b(f.fields.get("inherit")) {
        sqlx::query("DELETE FROM forumpermissions WHERE fid = $1 AND gid = $2")
            .bind(fid)
            .bind(gid)
            .execute(&ctx.app.db)
            .await?;
    } else {
        let mut j = serde_json::json!({});
        for m in FORUM_PERM_META {
            j[m.name] = serde_json::Value::Bool(b(f.fields.get(m.name)));
        }
        let p: ForumPerms = serde_json::from_value(j).map_err(|e| AppError::Other(e.into()))?;
        sqlx::query("INSERT INTO forumpermissions (fid, gid, perms) VALUES ($1, $2, $3) ON CONFLICT (fid, gid) DO UPDATE SET perms = $3")
            .bind(fid)
            .bind(gid)
            .bind(serde_json::to_value(&p).unwrap())
            .execute(&ctx.app.db)
            .await?;
    }
    ctx.app.invalidate(&["forumperms"]).await?;
    crate::admin::log(
        &ctx,
        "forums",
        "Updated forum permissions",
        serde_json::json!({"fid": fid, "gid": gid}),
    )
    .await;
    Ok(ctx.redirect(
        &format!("/admin/forums/{fid}/permissions"),
        "The permissions have been saved.",
    ))
}

// ---------------------------------------------------------------- moderators

pub async fn moderators(ctx: Ctx, Path(fid): Path<i32>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "forums");
    let forum = ctx
        .cache
        .forum(fid)
        .cloned()
        .ok_or_else(|| AppError::not_found("forum"))?;
    let mods: Vec<_> = ctx
        .cache
        .moderators
        .iter()
        .filter(|m| m.fid == fid)
        .cloned()
        .collect();
    let uids: Vec<i32> = mods.iter().filter(|m| !m.isgroup).map(|m| m.id).collect();
    let names: HashMap<i32, String> =
        sqlx::query_as::<_, (i32, String)>("SELECT uid, username FROM users WHERE uid = ANY($1)")
            .bind(&uids)
            .fetch_all(&ctx.app.db)
            .await?
            .into_iter()
            .collect();
    let list: Vec<_> = mods
        .iter()
        .map(|m| minijinja::context! { mid => m.mid, isgroup => m.isgroup, name => if m.isgroup { ctx.cache.group(m.id).map(|g| g.title.clone()).unwrap_or_default() } else { names.get(&m.id).cloned().unwrap_or_default() }, id => m.id })
        .collect();
    let groups: Vec<(i32, String)> = ctx
        .cache
        .groups
        .values()
        .map(|g| (g.gid, g.title.clone()))
        .collect();
    crate::admin::page(
        &ctx,
        "admin/moderators.html",
        "forums",
        &format!("Moderators: {}", forum.name),
        minijinja::context! { forum => &forum, list => list, groups => groups },
    )
    .await
}

pub async fn moderator_add(
    ctx: Ctx,
    Path(fid): Path<i32>,
    CsrfForm(f): CsrfForm<AnyForm>,
) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "forums");
    let username = s(f.fields.get("username"));
    let gid = i(f.fields.get("gid"));
    let (id, isgroup) = if !username.trim().is_empty() {
        let uid: i32 =
            sqlx::query_scalar("SELECT uid FROM users WHERE lower(username) = lower($1)")
                .bind(username.trim())
                .fetch_optional(&ctx.app.db)
                .await?
                .ok_or_else(|| AppError::not_found("user"))?;
        // Moderators join the Moderators group (6) as an additional group, like MyBB.
        sqlx::query("UPDATE users SET additionalgroups = array_append(additionalgroups, 6) WHERE uid = $1 AND usergroup NOT IN (3, 4, 6) AND NOT (6 = ANY(additionalgroups))").bind(uid).execute(&ctx.app.db).await?;
        (uid, false)
    } else if gid > 0 {
        (gid, true)
    } else {
        return Err(AppError::user("Enter a username or choose a group."));
    };
    sqlx::query("INSERT INTO moderators (fid, id, isgroup, perms) VALUES ($1, $2, $3, $4) ON CONFLICT (fid, id, isgroup) DO NOTHING")
        .bind(fid)
        .bind(id)
        .bind(isgroup)
        .bind(serde_json::to_value(ModPerms::default()).unwrap())
        .execute(&ctx.app.db)
        .await?;
    ctx.app.invalidate(&["moderators"]).await?;
    crate::admin::log(
        &ctx,
        "forums",
        "Added moderator",
        serde_json::json!({"fid": fid, "id": id, "isgroup": isgroup}),
    )
    .await;
    Ok(ctx.redirect(
        &format!("/admin/forums/{fid}/moderators"),
        "The moderator has been added.",
    ))
}

pub async fn moderator_edit(ctx: Ctx, Path(mid): Path<i32>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "forums");
    let m = ctx
        .cache
        .moderators
        .iter()
        .find(|m| m.mid == mid)
        .cloned()
        .ok_or_else(|| AppError::not_found("moderator"))?;
    let j = serde_json::to_value(&m.perms.0).unwrap();
    let items: Vec<_> = MOD_PERM_META.iter().map(|x| minijinja::context! { name => x.name, title => x.title, section => x.section, value => j[x.name].as_bool().unwrap_or(false) }).collect();
    crate::admin::page(
        &ctx,
        "admin/moderator_edit.html",
        "forums",
        "Moderator Permissions",
        minijinja::context! { mo => &m, items => items, forum => ctx.cache.forum(m.fid).cloned() },
    )
    .await
}

pub async fn moderator_save(
    ctx: Ctx,
    Path(mid): Path<i32>,
    CsrfForm(f): CsrfForm<AnyForm>,
) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "forums");
    let m = ctx
        .cache
        .moderators
        .iter()
        .find(|m| m.mid == mid)
        .cloned()
        .ok_or_else(|| AppError::not_found("moderator"))?;
    let mut j = serde_json::json!({});
    for x in MOD_PERM_META {
        j[x.name] = serde_json::Value::Bool(b(f.fields.get(x.name)));
    }
    sqlx::query("UPDATE moderators SET perms = $2 WHERE mid = $1")
        .bind(mid)
        .bind(j)
        .execute(&ctx.app.db)
        .await?;
    ctx.app.invalidate(&["moderators"]).await?;
    Ok(ctx.redirect(
        &format!("/admin/forums/{}/moderators", m.fid),
        "The moderator's permissions have been saved.",
    ))
}

pub async fn moderator_delete(
    ctx: Ctx,
    Path(mid): Path<i32>,
    CsrfForm(_): CsrfForm<AnyForm>,
) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "forums");
    let m = ctx
        .cache
        .moderators
        .iter()
        .find(|m| m.mid == mid)
        .cloned()
        .ok_or_else(|| AppError::not_found("moderator"))?;
    sqlx::query("DELETE FROM moderators WHERE mid = $1")
        .bind(mid)
        .execute(&ctx.app.db)
        .await?;
    if !m.isgroup {
        let still: Option<i32> =
            sqlx::query_scalar("SELECT mid FROM moderators WHERE id = $1 AND NOT isgroup LIMIT 1")
                .bind(m.id)
                .fetch_optional(&ctx.app.db)
                .await?;
        if still.is_none() {
            sqlx::query("UPDATE users SET additionalgroups = array_remove(additionalgroups, 6) WHERE uid = $1").bind(m.id).execute(&ctx.app.db).await?;
        }
    }
    ctx.app.invalidate(&["moderators"]).await?;
    Ok(ctx.redirect(
        &format!("/admin/forums/{}/moderators", m.fid),
        "The moderator has been removed.",
    ))
}
