//! Turning posts and users into template-ready data ("postbit"), with parse caching.

use crate::cache::Cache;
use crate::ctx::CtxInner;
use crate::error::AppResult;
use crate::models::{Forum, Post};
use crate::parser::{ParseOptions, Parser};
use crate::util::{self, now};
use serde::Serialize;
use sqlx::FromRow;
use std::collections::HashMap;
use std::sync::Arc;

pub fn forum_parse_options(forum: Option<&Forum>, me_username: Option<String>) -> ParseOptions {
    match forum {
        Some(f) => ParseOptions {
            allow_html: f.allowhtml,
            allow_mycode: f.allowmycode,
            allow_smilies: f.allowsmilies,
            allow_imgcode: f.allowimgcode,
            allow_videocode: f.allowvideocode,
            me_username,
            ..Default::default()
        },
        None => ParseOptions {
            me_username,
            ..Default::default()
        },
    }
}

pub fn parse_with(
    cache: &Cache,
    plugins: &crate::plugins::Plugins,
    opts: &ParseOptions,
    msg: &str,
) -> String {
    let mut html = Parser::new(&cache.parser, opts).parse(msg);
    if plugins.has_hook("parse_message") {
        html = plugins.filter_string("parse_message", html);
    }
    match &cache.image_proxy {
        Some(p) => p.rewrite_html(&html).into_owned(),
        None => html,
    }
}

/// Freshly parsed post HTML waiting to be written back, with a hash of the source it came from.
pub struct Parsed {
    pid: i32,
    html: String,
    source_hash: Vec<u8>,
}

fn source_hash(msg: &str) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(msg.as_bytes()).to_vec()
}

/// Parse a post message, using and refreshing the cached HTML where possible.
pub fn post_html(ctx: &CtxInner, post: &Post, stale: &mut Vec<Parsed>) -> String {
    let cache = &ctx.cache;
    let forum = cache.forum(post.fid);
    let viewer_restrict = ctx
        .user
        .as_ref()
        .map(|u| !u.showimages || !u.showvideos)
        .unwrap_or(false);
    if !viewer_restrict
        && post.parser_rev == cache.parser_rev
        && !post.message_html.is_empty()
        && !post.smilieoff
    {
        return post.message_html.clone();
    }
    // Parsed moments ago for another viewer, before the write-back landed?
    if !viewer_restrict
        && !post.smilieoff
        && let Some((rev, hash, html)) = ctx.app.parsed_cache.get(&post.pid)
    {
        // Same parser inputs and the same text (edits reset parser_rev but change the text).
        if rev == cache.parser_rev && *hash == *source_hash(&post.message) {
            return html.to_string();
        }
    }
    let mut opts = forum_parse_options(forum, Some(post.username.clone()));
    if post.smilieoff {
        opts.allow_smilies = false;
    }
    if let Some(u) = &ctx.user {
        if !u.showimages {
            opts.allow_imgcode = false;
        }
        if !u.showvideos {
            opts.allow_videocode = false;
        }
    }
    let html = parse_with(cache, &ctx.app.plugins, &opts, &post.message);
    if !viewer_restrict {
        let hash = source_hash(&post.message);
        if !post.smilieoff {
            ctx.app.parsed_cache.insert(
                post.pid,
                (
                    cache.parser_rev,
                    Arc::from(hash.as_slice()),
                    Arc::from(html.as_str()),
                ),
            );
        }
        stale.push(Parsed {
            pid: post.pid,
            html: html.clone(),
            source_hash: hash,
        });
    }
    html
}

/// Write freshly parsed HTML back to the posts table (in the background).
pub fn store_parsed(ctx: &CtxInner, stale: Vec<Parsed>) {
    if stale.is_empty() {
        return;
    }
    // Many visitors can open the same cold page at once: write each post back only once, and
    // only a few batches at a time, so warming the cache never starves page views of connections.
    let app = ctx.app.clone();
    let stale: Vec<Parsed> = stale
        .into_iter()
        .filter(|p| app.parse_inflight.insert(p.pid))
        .collect();
    if stale.is_empty() {
        return;
    }
    let rev = ctx.cache.parser_rev;
    tokio::spawn(async move {
        let pids: Vec<i32> = stale.iter().map(|p| p.pid).collect();
        let htmls: Vec<&str> = stale.iter().map(|p| p.html.as_str()).collect();
        let hashes: Vec<&[u8]> = stale.iter().map(|p| p.source_hash.as_slice()).collect();
        if let Ok(_permit) = app.parse_writeback.acquire().await {
            // Only if the post still has the text that was parsed: an edit made meanwhile wins.
            let _ = sqlx::query(
                "UPDATE posts SET message_html = d.h, parser_rev = $3 FROM UNNEST($1::int[], $2::text[], $4::bytea[]) AS d(pid, h, src)
                 WHERE posts.pid = d.pid AND sha256(convert_to(posts.message, 'UTF8')) = d.src",
            )
            .bind(&pids)
            .bind(&htmls)
            .bind(rev)
            .bind(&hashes)
            .execute(&app.db)
            .await;
        }
        for p in &pids {
            app.parse_inflight.remove(p);
        }
    });
}

#[derive(FromRow, Clone, Debug)]
pub struct AuthorRow {
    pub uid: i32,
    pub username: String,
    pub usergroup: i32,
    pub displaygroup: i32,
    pub additionalgroups: Vec<i32>,
    pub usertitle: String,
    pub avatar: String,
    pub avatardimensions: String,
    pub signature: String,
    pub postnum: i32,
    pub threadnum: i32,
    pub regdate: i64,
    pub reputation: i32,
    pub warningpoints: i32,
    pub lastactive: i64,
    pub invisible: bool,
    pub away: bool,
    pub website: String,
    pub suspendsignature: bool,
    pub suspendsigtime: i64,
    pub birthday: String,
    pub email: String,
    pub hideemail: bool,
    pub receivepms: bool,
}

pub const AUTHOR_COLS: &str = "uid, username, usergroup, displaygroup, additionalgroups, usertitle, avatar, avatardimensions, signature, postnum, threadnum, regdate, reputation, warningpoints, lastactive, invisible, away, website, suspendsignature, suspendsigtime, birthday, email, hideemail, receivepms";

#[derive(Serialize, Clone, Debug, Default)]
pub struct AuthorInfo {
    pub uid: i32,
    pub username: String,
    pub formatted: String,
    pub usertitle: String,
    pub stars: i32,
    pub starimage: String,
    pub groupimage: String,
    pub grouptitle: String,
    pub avatar: String,
    pub avatar_w: i32,
    pub avatar_h: i32,
    pub postnum: i32,
    pub threadnum: i32,
    pub regdate: i64,
    pub reputation: i32,
    pub warninglevel: i64,
    pub online: bool,
    pub away: bool,
    pub website: String,
    pub signature: String,
    pub fields: Vec<(String, String)>,
    /// Enabled badges in display order.
    pub badges: Vec<crate::badges::Shown>,
    pub can_pm: bool,
    pub can_email: bool,
}

/// Avatar URLs ("" = none) for many members at once: one indexed query for whoever isn't cached.
pub async fn avatars(ctx: &CtxInner, uids: &[i32]) -> AppResult<HashMap<i32, Arc<str>>> {
    let mut out = HashMap::with_capacity(uids.len());
    let mut missing: Vec<i32> = vec![];
    for &u in uids {
        if u <= 0 || out.contains_key(&u) {
            continue;
        }
        match ctx.app.avatar_cache.get(&u) {
            Some(a) => {
                out.insert(u, a);
            }
            None => missing.push(u),
        }
    }
    missing.sort_unstable();
    missing.dedup();
    if !missing.is_empty() {
        let rows: Vec<(i32, String)> =
            sqlx::query_as("SELECT uid, avatar FROM users WHERE uid = ANY($1)")
                .bind(&missing)
                .fetch_all(&ctx.app.db)
                .await?;
        for (u, a) in rows {
            let a: Arc<str> = Arc::from(a.as_str());
            ctx.app.avatar_cache.insert(u, a.clone());
            out.insert(u, a);
        }
    }
    Ok(out)
}

pub async fn load_authors(ctx: &CtxInner, uids: &[i32]) -> AppResult<HashMap<i32, AuthorInfo>> {
    let mut uids: Vec<i32> = uids.iter().copied().filter(|u| *u > 0).collect();
    uids.sort();
    uids.dedup();
    if uids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<AuthorRow> = sqlx::query_as(&format!(
        "SELECT {AUTHOR_COLS} FROM users WHERE uid = ANY($1)"
    ))
    .bind(&uids)
    .fetch_all(&ctx.app.db)
    .await?;
    // Postbit profile fields.
    let pf: Vec<&crate::models::ProfileField> = ctx
        .cache
        .profilefields
        .iter()
        .filter(|f| f.postbit)
        .collect();
    let mut fields: HashMap<i32, Vec<(String, String)>> = HashMap::new();
    if !pf.is_empty() {
        let fids: Vec<i32> = pf.iter().map(|f| f.fid).collect();
        let vals: Vec<(i32, i32, String)> = sqlx::query_as("SELECT uid, fid, value FROM userfields WHERE uid = ANY($1) AND fid = ANY($2) AND value <> ''")
            .bind(&uids)
            .bind(&fids)
            .fetch_all(&ctx.app.db)
            .await?;
        for (uid, fid, v) in vals {
            if let Some(f) = pf.iter().find(|f| f.fid == fid)
                && (f.viewableby.is_empty() || f.viewableby.iter().any(|g| ctx.groups.contains(g)))
            {
                fields.entry(uid).or_default().push((f.name.clone(), v));
            }
        }
    }
    let mut badges = crate::badges::of_members(&ctx.app.db, &ctx.cache, &uids).await?;
    let mut out = HashMap::new();
    for r in rows {
        let mut a = author_info(ctx, &r);
        a.fields = fields.remove(&r.uid).unwrap_or_default();
        a.badges = badges.remove(&r.uid).unwrap_or_default();
        out.insert(r.uid, a);
    }
    Ok(out)
}

pub fn author_info(ctx: &CtxInner, r: &AuthorRow) -> AuthorInfo {
    let cache = &ctx.cache;
    let s = &cache.settings;
    let dg = if r.displaygroup > 0 {
        r.displaygroup
    } else {
        r.usergroup
    };
    let group = cache.group(dg);
    let (mut usertitle, mut stars, mut starimage) = (String::new(), 0i32, String::new());
    if let Some(g) = group {
        stars = g.stars as i32;
        starimage = g.starimage.clone();
        if !g.usertitle.is_empty() {
            usertitle = g.usertitle.clone();
        }
    }
    if (usertitle.is_empty() || group.map(|g| g.usertitle.is_empty()).unwrap_or(true))
        && let Some(t) = cache.usertitle_for(r.postnum)
    {
        if usertitle.is_empty() {
            usertitle = t.title.clone();
        }
        if stars == 0 {
            stars = t.stars as i32;
            starimage = t.starimage.clone();
        }
    }
    if !r.usertitle.is_empty() {
        usertitle = r.usertitle.clone();
    }
    if starimage.is_empty() {
        starimage = "/static/images/star.svg".into();
    }
    let maxwarn = s.int("maxwarningpoints").max(1);
    let (aw, ah) = r
        .avatardimensions
        .split_once('|')
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((0, 0));
    let online = cache.is_system(r.uid)
        || !r.invisible && r.lastactive > now() - s.int("wolcutoffmins").max(1) * 60
        || (r.invisible
            && ctx.perms.canviewwolinvis
            && r.lastactive > now() - s.int("wolcutoffmins").max(1) * 60);
    let sig_allowed = !r.suspendsignature || (r.suspendsigtime > 0 && r.suspendsigtime < now());
    let show_sig = ctx.user.as_ref().map(|u| u.showsigs).unwrap_or(true);
    let signature = if sig_allowed && show_sig && !r.signature.is_empty() {
        signature_html(ctx, r.uid, &r.signature)
    } else {
        String::new()
    };
    AuthorInfo {
        uid: r.uid,
        username: r.username.clone(),
        formatted: cache.format_name(&r.username, r.usergroup, r.displaygroup),
        usertitle,
        stars,
        starimage,
        groupimage: group.map(|g| g.image.clone()).unwrap_or_default(),
        grouptitle: group.map(|g| g.title.clone()).unwrap_or_default(),
        avatar: r.avatar.clone(),
        avatar_w: aw,
        avatar_h: ah,
        postnum: r.postnum,
        threadnum: r.threadnum,
        regdate: r.regdate,
        reputation: r.reputation,
        warninglevel: (r.warningpoints as i64 * 100 / maxwarn).min(100),
        online,
        away: r.away,
        website: r.website.clone(),
        signature,
        fields: vec![],
        badges: vec![],
        can_pm: r.receivepms,
        can_email: !r.hideemail,
    }
}

static SIG_CACHE: std::sync::LazyLock<moka::sync::Cache<(i32, i32, u64), String>> =
    std::sync::LazyLock::new(|| {
        moka::sync::Cache::builder()
            .max_capacity(20_000)
            .time_to_live(std::time::Duration::from_secs(3600))
            .build()
    });

pub fn signature_html(ctx: &CtxInner, uid: i32, sig: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    sig.hash(&mut h);
    let key = (uid, ctx.cache.parser_rev, h.finish());
    if let Some(v) = SIG_CACHE.get(&key) {
        return v;
    }
    let s = &ctx.cache.settings;
    let opts = ParseOptions {
        allow_mycode: s.bool("sigmycode"),
        allow_smilies: s.bool("sigsmilies"),
        allow_imgcode: s.bool("sigimgcode"),
        allow_videocode: false,
        ..Default::default()
    };
    let html = parse_with(&ctx.cache, &ctx.app.plugins, &opts, sig);
    SIG_CACHE.insert(key, html.clone());
    html
}

#[derive(Serialize, Clone, Debug)]
pub struct AttachmentInfo {
    pub aid: i32,
    pub pid: i32,
    pub filename: String,
    pub filesize: i64,
    pub filetype: String,
    pub downloads: i32,
    pub is_image: bool,
    pub has_thumb: bool,
    pub visible: bool,
}

pub async fn load_attachments(
    ctx: &CtxInner,
    pids: &[i32],
) -> AppResult<HashMap<i32, Vec<AttachmentInfo>>> {
    if pids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i32, i32, String, i64, String, i32, String, bool)> = sqlx::query_as(
        "SELECT aid, pid, filename, filesize, filetype, downloads, thumbnail, visible FROM attachments WHERE pid = ANY($1) ORDER BY aid",
    )
    .bind(pids)
    .fetch_all(&ctx.app.db)
    .await?;
    let mut m: HashMap<i32, Vec<AttachmentInfo>> = HashMap::new();
    for (aid, pid, filename, filesize, filetype, downloads, thumb, visible) in rows {
        m.entry(pid).or_default().push(AttachmentInfo {
            aid,
            pid,
            is_image: filetype.starts_with("image/"),
            has_thumb: !thumb.is_empty(),
            filename,
            filesize,
            filetype,
            downloads,
            visible,
        });
    }
    Ok(m)
}

/// Replace `[attachment=N]` placeholders with inline attachment HTML; returns the ids used inline.
pub fn inline_attachments(
    html: &str,
    atts: &[AttachmentInfo],
    thumbs_mode: &str,
) -> (String, Vec<i32>) {
    if !html.contains("<!--attachment:") {
        return (html.to_string(), vec![]);
    }
    let mut used = vec![];
    let mut out = html.to_string();
    for a in atts {
        let marker = format!("<!--attachment:{}-->", a.aid);
        if out.contains(&marker) {
            used.push(a.aid);
            out = out.replace(&marker, &attachment_html(a, thumbs_mode));
        }
    }
    (out, used)
}

pub fn attachment_html(a: &AttachmentInfo, thumbs_mode: &str) -> String {
    let name = util::escape_html(&a.filename);
    if a.is_image && thumbs_mode != "download" {
        let src = if a.has_thumb && thumbs_mode == "yes" {
            format!("/attachment/{}?thumb=1", a.aid)
        } else {
            format!("/attachment/{}", a.aid)
        };
        format!(
            "<a href=\"/attachment/{0}\" target=\"_blank\" class=\"attachment_image\"><img src=\"{src}\" alt=\"{name}\" loading=\"lazy\" /></a>",
            a.aid
        )
    } else {
        format!(
            "<span class=\"attachment\"><a href=\"/attachment/{}\">{name}</a> <small>({}, {} downloads)</small></span>",
            a.aid,
            util::format_bytes(a.filesize),
            a.downloads
        )
    }
}
