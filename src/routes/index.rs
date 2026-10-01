//! Board index.

use crate::ctx::Ctx;
use crate::error::AppResult;
use crate::models::ForumCounters;
use crate::templates::{url_forum, url_thread};
use crate::util::now;
use axum::response::Response;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Serialize, Clone, Debug, Default)]
pub struct LastPost {
    pub dateline: i64,
    pub username: String,
    pub uid: i32,
    pub tid: i32,
    pub subject: String,
    pub url: String,
    pub avatar: String,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct ForumNode {
    pub fid: i32,
    pub name: String,
    pub description: String,
    pub url: String,
    pub linkto: String,
    pub is_category: bool,
    pub open: bool,
    pub threads: i64,
    pub posts: i64,
    pub unapproved: i64,
    pub lastpost: Option<LastPost>,
    pub unread: bool,
    pub subforums: Vec<SubForum>,
    pub children: Vec<ForumNode>,
    pub moderators: Vec<(i32, String, bool)>,
    pub viewers: i64,
    pub password: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct SubForum {
    pub fid: i32,
    pub name: String,
    pub url: String,
    pub unread: bool,
}

pub async fn load_counters(ctx: &Ctx) -> AppResult<HashMap<i32, ForumCounters>> {
    let rows: Vec<ForumCounters> = sqlx::query_as(
        "SELECT fid, threads, posts, unapprovedthreads, unapprovedposts, deletedthreads, deletedposts, lastpost, lastposter, lastposteruid, lastposttid, lastpostsubject FROM forums",
    )
    .fetch_all(&ctx.app.db)
    .await?;
    Ok(rows.into_iter().map(|r| (r.fid, r)).collect())
}

/// The viewer's forum read markers (once per request: the forum tree and thread rows share it).
pub async fn forums_read(ctx: &Ctx) -> AppResult<HashMap<i32, i64>> {
    if ctx.uid() == 0 {
        return Ok(HashMap::new());
    }
    let m = ctx
        .forums_read
        .get_or_try_init(|| async {
            let rows: Vec<(i32, i64)> =
                sqlx::query_as("SELECT fid, dateline FROM forumsread WHERE uid = $1")
                    .bind(ctx.uid())
                    .fetch_all(&ctx.app.db)
                    .await?;
            Ok::<_, crate::error::AppError>(rows.into_iter().collect())
        })
        .await?;
    Ok(m.clone())
}

/// Build the viewable forum tree below `root` (0 = board index), with aggregated counters.
pub async fn build_tree(ctx: &Ctx, root: i32, depth_limit: usize) -> AppResult<Vec<ForumNode>> {
    let counters = load_counters(ctx).await?;
    let read = forums_read(ctx).await?;
    let viewers: HashMap<i32, i64> = if ctx.settings().bool("showforumviewing") {
        let cutoff = now() - ctx.settings().int("wolcutoffmins").max(1) * 60;
        sqlx::query_as::<_, (i32, i64)>("SELECT location1, COUNT(*) FROM sessions WHERE time > $1 AND location1 > 0 GROUP BY location1")
            .bind(cutoff)
            .fetch_all(&ctx.app.db)
            .await?
            .into_iter()
            .collect()
    } else {
        HashMap::new()
    };
    let mod_names = moderator_names(ctx).await?;
    Ok(build_level(
        ctx,
        root,
        0,
        depth_limit,
        &counters,
        &read,
        &viewers,
        &mod_names,
    ))
}

async fn moderator_names(ctx: &Ctx) -> AppResult<HashMap<i32, Vec<(i32, String, bool)>>> {
    let cache = &ctx.cache;
    if cache.moderators.is_empty() {
        return Ok(HashMap::new());
    }
    let uids: Vec<i32> = cache
        .moderators
        .iter()
        .filter(|m| !m.isgroup)
        .map(|m| m.id)
        .collect();
    let names: HashMap<i32, (String, i32, i32)> = sqlx::query_as::<_, (i32, String, i32, i32)>(
        "SELECT uid, username, usergroup, displaygroup FROM users WHERE uid = ANY($1)",
    )
    .bind(&uids)
    .fetch_all(&ctx.app.db)
    .await?
    .into_iter()
    .map(|(u, n, g, d)| (u, (n, g, d)))
    .collect();
    let mut m: HashMap<i32, Vec<(i32, String, bool)>> = HashMap::new();
    for md in cache.moderators.iter() {
        let entry = if md.isgroup {
            cache
                .group(md.id)
                .map(|g| (md.id, crate::util::escape_html(&g.title), true))
        } else {
            names
                .get(&md.id)
                .map(|(n, g, d)| (md.id, cache.format_name(n, *g, *d), false))
        };
        if let Some(e) = entry {
            m.entry(md.fid).or_default().push(e);
        }
    }
    Ok(m)
}

#[allow(clippy::too_many_arguments)]
fn build_level(
    ctx: &Ctx,
    pid: i32,
    depth: usize,
    depth_limit: usize,
    counters: &HashMap<i32, ForumCounters>,
    read: &HashMap<i32, i64>,
    viewers: &HashMap<i32, i64>,
    mods: &HashMap<i32, Vec<(i32, String, bool)>>,
) -> Vec<ForumNode> {
    let cache = &ctx.cache;
    let mut out = Vec::new();
    let readcut = now() - ctx.settings().int("threadreadcut").max(1) * 86400;
    for f in cache.children(pid) {
        if !f.active && !ctx.is_mod(f.fid) {
            continue;
        }
        let fp = ctx.forum_perms(f.fid);
        if !fp.canview {
            continue;
        }
        let mut node = ForumNode {
            fid: f.fid,
            name: f.name.clone(),
            description: f.description.clone(),
            url: url_forum(f.fid as i64, Some(&f.name)),
            linkto: f.linkto.clone(),
            is_category: f.is_category(),
            open: f.open,
            password: f.has_password(),
            moderators: mods.get(&f.fid).cloned().unwrap_or_default(),
            viewers: viewers.get(&f.fid).copied().unwrap_or(0),
            ..Default::default()
        };
        // Aggregate counters over this forum and all viewable descendants.
        let mut ids = vec![f.fid];
        ids.extend(
            cache
                .descendants(f.fid)
                .into_iter()
                .filter(|d| ctx.forum_perms(*d).canview),
        );
        let mut best: Option<&ForumCounters> = None;
        for id in &ids {
            let Some(c) = counters.get(id) else { continue };
            node.threads += c.threads as i64;
            node.posts += c.posts as i64;
            if ctx.is_mod(*id) {
                node.unapproved += (c.unapprovedthreads + c.unapprovedposts) as i64;
            }
            let p = ctx.forum_perms(*id);
            let pw = cache.forum(*id).map(|f| f.has_password()).unwrap_or(false);
            if p.canviewthreads && !p.canonlyviewownthreads && !pw {
                if best.map(|b| c.lastpost > b.lastpost).unwrap_or(true) && c.lastpost > 0 {
                    best = Some(c);
                }
            }
            if ctx.uid() > 0 && c.lastpost > readcut && c.lastposteruid != ctx.uid() {
                let r = read.get(id).copied().unwrap_or(0);
                if c.lastpost > r {
                    node.unread = true;
                }
            }
        }
        if let Some(b) = best {
            node.lastpost = Some(LastPost {
                dateline: b.lastpost,
                username: b.lastposter.clone(),
                uid: b.lastposteruid,
                tid: b.lastposttid,
                subject: b.lastpostsubject.clone(),
                url: format!("{}/lastpost", url_thread(b.lastposttid as i64, None)),
                avatar: String::new(),
            });
        }
        let subf_limit = ctx.settings().int("subforumsindex") as usize;
        if depth + 1 < depth_limit {
            node.children = build_level(
                ctx,
                f.fid,
                depth + 1,
                depth_limit,
                counters,
                read,
                viewers,
                mods,
            );
        } else if subf_limit > 0 {
            node.subforums = cache
                .children(f.fid)
                .filter(|s| s.active && ctx.forum_perms(s.fid).canview)
                .take(subf_limit)
                .map(|s| SubForum {
                    fid: s.fid,
                    name: s.name.clone(),
                    url: url_forum(s.fid as i64, Some(&s.name)),
                    unread: ctx.uid() > 0
                        && counters
                            .get(&s.fid)
                            .map(|c| {
                                c.lastpost > readcut
                                    && c.lastpost > read.get(&s.fid).copied().unwrap_or(0)
                                    && c.lastposteruid != ctx.uid()
                            })
                            .unwrap_or(false),
                })
                .collect();
        }
        out.push(node);
    }
    out
}

#[derive(Serialize, serde::Deserialize)]
pub struct OnlineSummary {
    pub members: Vec<OnlineUser>,
    pub num_members: i64,
    pub num_guests: i64,
    pub num_bots: i64,
    pub num_invisible: i64,
    pub bots: Vec<String>,
    pub total: i64,
    pub cutoff: i64,
}

#[derive(Serialize, serde::Deserialize, Clone)]
pub struct OnlineUser {
    pub uid: i32,
    pub formatted: String,
    pub invisible: bool,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub avatar: String,
}

/// Online summary, computed at most every 30 seconds per node (the sessions table can be large).
pub async fn online_summary(ctx: &Ctx) -> AppResult<OnlineSummary> {
    let key = if ctx.perms.canviewwolinvis {
        "online_all"
    } else {
        "online_public"
    }
    .to_string();
    let mut summary: OnlineSummary = match ctx
        .app
        .short_cache
        .get(&key)
        .and_then(|v| serde_json::from_value(v).ok())
    {
        Some(s) => s,
        None => {
            let s = compute_online_summary(ctx).await?;
            ctx.app
                .short_cache
                .insert(key, serde_json::to_value(&s).unwrap_or_default());
            s
        }
    };
    // The viewer is online even if the cached summary predates their arrival.
    if let Some(u) = &ctx.user {
        if !summary.members.iter().any(|m| m.uid == u.uid) {
            summary.members.push(OnlineUser {
                uid: u.uid,
                formatted: ctx
                    .cache
                    .format_name(&u.username, u.usergroup, u.displaygroup),
                invisible: u.invisible,
                username: u.username.clone(),
                avatar: String::new(),
            });
            summary.num_members += 1;
            summary.total += 1;
        }
    }
    // Faces for the first members shown.
    let shown: Vec<i32> = summary.members.iter().take(40).map(|m| m.uid).collect();
    let av = crate::render::avatars(ctx, &shown).await?;
    for m in summary.members.iter_mut().take(40) {
        m.avatar = av.get(&m.uid).map(|a| a.to_string()).unwrap_or_default();
    }
    Ok(summary)
}

/// Fill in last posters' avatars across a forum tree (one batched lookup).
pub async fn fill_avatars(ctx: &Ctx, nodes: &mut [ForumNode]) -> AppResult<()> {
    fn collect(nodes: &[ForumNode], out: &mut Vec<i32>) {
        for n in nodes {
            if let Some(l) = &n.lastpost {
                out.push(l.uid);
            }
            collect(&n.children, out);
        }
    }
    fn apply(nodes: &mut [ForumNode], av: &HashMap<i32, std::sync::Arc<str>>) {
        for n in nodes {
            if let Some(l) = &mut n.lastpost {
                l.avatar = av.get(&l.uid).map(|a| a.to_string()).unwrap_or_default();
            }
            apply(&mut n.children, av);
        }
    }
    let mut uids = vec![];
    collect(nodes, &mut uids);
    let av = crate::render::avatars(ctx, &uids).await?;
    apply(nodes, &av);
    Ok(())
}

async fn compute_online_summary(ctx: &Ctx) -> AppResult<OnlineSummary> {
    let mins = ctx.settings().int("wolcutoffmins").max(1);
    let cutoff = now() - mins * 60;
    let rows: Vec<(i32, bool, String)> = sqlx::query_as(
        "SELECT DISTINCT ON (uid, bot, CASE WHEN uid = 0 AND bot = '' THEN sid ELSE '' END) uid, anonymous, bot FROM sessions WHERE time > $1",
    )
    .bind(cutoff)
    .fetch_all(&ctx.app.db)
    .await?;
    let mut uids = Vec::new();
    let (mut guests, mut bots_n) = (0, 0);
    let mut bots = Vec::new();
    for (uid, _anon, bot) in &rows {
        if !bot.is_empty() {
            bots_n += 1;
            if !bots.contains(bot) {
                bots.push(bot.clone());
            }
        } else if *uid == 0 {
            guests += 1;
        } else if !uids.contains(uid) {
            uids.push(*uid);
        }
    }
    let users: Vec<(i32, String, i32, i32, bool)> = sqlx::query_as(
        // System is always listed (first) but never counted: it has no session.
        "SELECT uid, username, usergroup, displaygroup, invisible FROM users WHERE uid = ANY($1) OR is_system ORDER BY is_system DESC, lower(username)",
    )
    .bind(&uids)
    .fetch_all(&ctx.app.db)
    .await?;
    let mut members = Vec::new();
    let mut invisible = 0;
    for (uid, name, g, d, inv) in users {
        if inv {
            invisible += 1;
            if !ctx.perms.canviewwolinvis {
                continue;
            }
        }
        members.push(OnlineUser {
            uid,
            formatted: ctx.cache.format_name(&name, g, d),
            invisible: inv,
            username: name.clone(),
            avatar: String::new(),
        });
    }
    let num_members = uids.len() as i64;
    let total = num_members + guests + bots_n;
    Ok(OnlineSummary {
        members,
        num_members,
        num_guests: guests,
        num_bots: bots_n,
        num_invisible: invisible,
        bots,
        total,
        cutoff: mins,
    })
}

pub async fn board_stats(ctx: &Ctx) -> AppResult<serde_json::Value> {
    if let Some(v) = ctx.app.stats_cache.get("boardstats") {
        return Ok(v);
    }
    let (threads, posts): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(threads), 0)::bigint, COALESCE(SUM(posts), 0)::bigint FROM forums",
    )
    .fetch_one(&ctx.app.db)
    .await?;
    let c: (i32, i32, String, i32, i64) =
        sqlx::query_as("SELECT numusers, lastuid, lastusername, mostonline, mostonlinetime FROM counters WHERE id = 1").fetch_one(&ctx.app.db).await?;
    let v = serde_json::json!({
        "threads": threads, "posts": posts, "users": c.0, "lastuid": c.1, "lastusername": c.2,
        "mostonline": c.3, "mostonlinetime": c.4,
    });
    ctx.app.stats_cache.insert("boardstats", v.clone());
    Ok(v)
}

pub async fn todays_birthdays(ctx: &Ctx) -> AppResult<Vec<serde_json::Value>> {
    use chrono::Datelike;
    let today = crate::util::to_local(now(), ctx.tz);
    let key = format!("{}-{}-", today.day(), today.month());
    let ck = format!("bday:{key}");
    type Row = (i32, String, i32, i32, String, String);
    let cached: Option<Vec<Row>> = ctx
        .app
        .short_cache
        .get(&ck)
        .and_then(|v| serde_json::from_value(v).ok());
    let rows: Vec<Row> = if let Some(r) = cached {
        r
    } else {
        let rows: Vec<Row> = sqlx::query_as(
        "SELECT uid, username, usergroup, displaygroup, birthday, birthdayprivacy FROM users WHERE birthday LIKE $1 || '%' AND birthdayprivacy <> 'none' LIMIT 200",
    )
    .bind(&key)
    .fetch_all(&ctx.app.db)
    .await?;
        ctx.app
            .short_cache
            .insert(ck, serde_json::to_value(&rows).unwrap_or_default());
        rows
    };
    Ok(rows
        .into_iter()
        .filter(|(_, _, g, _, _, _)| ctx.cache.group(*g).map(|g| g.perms.0.showinbirthdaylist).unwrap_or(false))
        .map(|(uid, name, g, d, b, privacy)| {
            let age = if privacy == "all" { crate::util::age_from_birthday(&b, ctx.tz) } else { None };
            serde_json::json!({"uid": uid, "formatted": ctx.cache.format_name(&name, g, d), "age": age})
        })
        .collect())
}

pub async fn index(ctx: Ctx) -> AppResult<Response> {
    let s = ctx.settings();
    if !ctx.perms.canview {
        return Err(crate::error::AppError::no_perm());
    }
    let mut tree = build_tree(&ctx, 0, 2).await?;
    fill_avatars(&ctx, &mut tree).await?;
    let online = if s.bool("showwol") && ctx.perms.canviewonline {
        Some(online_summary(&ctx).await?)
    } else {
        None
    };
    let stats = if s.bool("showindexstats") {
        Some(board_stats(&ctx).await?)
    } else {
        None
    };
    if let (Some(o), Some(st)) = (&online, &stats) {
        if o.total > st["mostonline"].as_i64().unwrap_or(0) {
            let _ = sqlx::query("UPDATE counters SET mostonline = $1, mostonlinetime = $2 WHERE id = 1 AND mostonline < $1")
                .bind(o.total as i32)
                .bind(now())
                .execute(&ctx.app.db)
                .await;
            ctx.app.stats_cache.invalidate(&"boardstats");
        }
    }
    let birthdays = if s.bool("showbirthdays") {
        todays_birthdays(&ctx).await?
    } else {
        vec![]
    };
    ctx.allow_guest_cache(&vec!["board".to_string()]);
    ctx.render(
        "index.html",
        minijinja::context! {
            title => s.get("bbname"),
            forums => tree,
            online => online,
            stats => stats,
            birthdays => birthdays,
        },
    )
    .await
}
