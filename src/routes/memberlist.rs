//! Member list and forum team page.

use crate::ctx::Ctx;
use crate::error::{AppError, AppResult};
use crate::util;
use axum::extract::Query;
use axum::response::Response;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize, Default)]
pub struct MlQuery {
    pub page: Option<i64>,
    #[serde(default)]
    pub sort: String,
    #[serde(default)]
    pub order: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub letter: String,
    #[serde(default)]
    pub website: String,
    #[serde(default)]
    pub gid: i32,
}

pub async fn memberlist(ctx: Ctx, Query(q): Query<MlQuery>) -> AppResult<Response> {
    let s = ctx.settings();
    if !s.bool("enablememberlist") || !ctx.perms.canviewmemberlist {
        return Err(AppError::no_perm());
    }
    let sort = if q.sort.is_empty() {
        s.get("default_mlsort").to_string()
    } else {
        q.sort.clone()
    };
    let col = match sort.as_str() {
        "username" => "lower(username)",
        "postnum" => "postnum",
        "threadnum" => "threadnum",
        "lastvisit" => "lastactive",
        "reputation" => "reputation",
        "referrals" => "referrals",
        _ => "regdate",
    };
    let order = if q.order == "asc" || (q.order.is_empty() && sort == "username") {
        "ASC"
    } else {
        "DESC"
    };
    // Groups hidden from the member list.
    let hidden: Vec<i32> = ctx
        .cache
        .groups
        .values()
        .filter(|g| !g.perms.0.showmemberlist)
        .map(|g| g.gid)
        .collect();
    let letter = q
        .letter
        .chars()
        .next()
        .filter(|c| c.is_ascii_alphabetic() || *c == '#')
        .map(|c| c.to_string())
        .unwrap_or_default();
    let where_sql = "NOT (usergroup = ANY($1)) AND ($2 = '' OR username ILIKE '%' || $2 || '%')
        AND ($3 = '' OR ($3 = '#' AND username !~ '^[A-Za-z]') OR lower(left(username, 1)) = lower($3))
        AND ($4 = '' OR website ILIKE '%' || $4 || '%') AND ($5 = 0 OR usergroup = $5 OR $5 = ANY(additionalgroups))";
    let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM users WHERE {where_sql}"))
        .bind(&hidden)
        .bind(q.username.trim())
        .bind(&letter)
        .bind(q.website.trim())
        .bind(q.gid)
        .fetch_one(&ctx.app.db)
        .await?;
    let per = s.int("membersperpage").max(5);
    let base = format!(
        "/members?sort={sort}&order={}&username={}&letter={}&gid={}&website={}&page={{page}}",
        order.to_lowercase(),
        percent_encoding::utf8_percent_encode(
            q.username.trim(),
            percent_encoding::NON_ALPHANUMERIC
        ),
        percent_encoding::utf8_percent_encode(&letter, percent_encoding::NON_ALPHANUMERIC),
        q.gid,
        percent_encoding::utf8_percent_encode(q.website.trim(), percent_encoding::NON_ALPHANUMERIC)
    );
    let pg = util::paginate(total, per, util::clamp_page(q.page), &base);
    let rows: Vec<(i32, String, i32, i32, String, i64, i64, i32, i32, i32, String, bool, String)> = sqlx::query_as(&format!(
        "SELECT uid, username, usergroup, displaygroup, avatar, regdate, lastactive, postnum, threadnum, reputation, usertitle, invisible, website FROM users WHERE {where_sql}
         ORDER BY {col} {order}, uid LIMIT $6 OFFSET $7"
    ))
    .bind(&hidden)
    .bind(q.username.trim())
    .bind(&letter)
    .bind(q.website.trim())
    .bind(q.gid)
    .bind(per)
    .bind((pg.page - 1) * per)
    .fetch_all(&ctx.app.db)
    .await?;
    let list: Vec<_> = rows
        .into_iter()
        .map(|(uid, name, g, d, avatar, reg, last, posts, threads, rep, title, inv, web)| {
            let t = if !title.is_empty() { title } else { ctx.cache.group(if d > 0 { d } else { g }).map(|x| x.usertitle.clone()).filter(|x| !x.is_empty()).or_else(|| ctx.cache.usertitle_for(posts).map(|x| x.title.clone())).unwrap_or_default() };
            minijinja::context! { uid => uid, username => &name, formatted => ctx.cache.format_name(&name, g, d), avatar => avatar, regdate => reg,
                lastactive => if inv && !ctx.perms.canviewwolinvis && uid != ctx.uid() { 0 } else { last }, postnum => posts, threadnum => threads, reputation => rep, usertitle => t, website => web }
        })
        .collect();
    let groups: Vec<(i32, String)> = ctx
        .cache
        .groups
        .values()
        .filter(|g| g.perms.0.showmemberlist && g.gid != 1)
        .map(|g| (g.gid, g.title.clone()))
        .collect();
    ctx.allow_guest_cache(&["board".to_string()]);
    ctx.render(
        "memberlist.html",
        minijinja::context! { title => "Member List", members => list, pagination => pg, sort => sort, order => order.to_lowercase(), username => q.username, website => q.website, letter => letter, groups => groups, gid => q.gid, total => total },
    )
    .await
}

pub async fn showteam(ctx: Ctx) -> AppResult<Response> {
    if !ctx.settings().bool("enableshowteam") {
        return Err(AppError::not_found("page"));
    }
    let team_groups: Vec<(i32, String)> = {
        let mut v: Vec<_> = ctx
            .cache
            .groups
            .values()
            .filter(|g| g.perms.0.showforumteam)
            .map(|g| (g.disporder, g.gid, g.title.clone()))
            .collect();
        v.sort();
        v.into_iter().map(|(_, g, t)| (g, t)).collect()
    };
    let gids: Vec<i32> = team_groups.iter().map(|(gid, _)| *gid).collect();
    type TeamMember = (i32, String, i32, i32, i64, bool);
    let rows: Vec<(i32, i32, String, i32, i32, i64, bool)> = sqlx::query_as(
        "SELECT g.gid, u.uid, u.username, u.usergroup, u.displaygroup, u.lastactive, u.invisible
         FROM unnest($1::int[]) AS g(gid)
         CROSS JOIN LATERAL (
             SELECT uid, username, usergroup, displaygroup, lastactive, invisible FROM users
             WHERE usergroup = g.gid OR g.gid = ANY(additionalgroups)
             ORDER BY lower(username), uid LIMIT 200
         ) u ORDER BY g.gid, lower(u.username), u.uid",
    )
    .bind(&gids)
    .fetch_all(&ctx.app.db)
    .await?;
    let mut by_group: HashMap<i32, Vec<TeamMember>> = HashMap::new();
    for (gid, uid, name, group, display, last, invisible) in rows {
        by_group
            .entry(gid)
            .or_default()
            .push((uid, name, group, display, last, invisible));
    }
    let mut sections = vec![];
    for (gid, title) in &team_groups {
        let rows = by_group.remove(gid).unwrap_or_default();
        let members: Vec<_> = rows
            .into_iter()
            .map(|(uid, n, g, d, la, inv)| minijinja::context! { uid => uid, formatted => ctx.cache.format_name(&n, g, d), lastactive => if inv && !ctx.perms.canviewwolinvis { 0 } else { la } })
            .collect();
        if !members.is_empty() {
            sections.push(minijinja::context! { title => title, members => members });
        }
    }
    // Forum moderators.
    let mods: Vec<(i32, String, i32, i32, i32)> = sqlx::query_as(
        "SELECT m.fid, u.username, u.uid, u.usergroup, u.displaygroup FROM moderators m JOIN users u ON u.uid = m.id WHERE NOT m.isgroup ORDER BY lower(u.username)",
    )
    .fetch_all(&ctx.app.db)
    .await?;
    let moderators: Vec<_> = mods
        .into_iter()
        .filter(|m| ctx.access().can_see(m.0))
        .map(|(fid, n, uid, g, d)| minijinja::context! { uid => uid, formatted => ctx.cache.format_name(&n, g, d), forum => ctx.cache.forum(fid).map(|f| f.name.clone()), fid => fid })
        .collect();
    ctx.allow_guest_cache(&["board".to_string()]);
    ctx.render("showteam.html", minijinja::context! { title => "Forum Team", sections => sections, moderators => moderators }).await
}
