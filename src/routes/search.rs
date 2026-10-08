//! Search (Postgres full-text), "new posts", "today's posts", unread threads, user content.

use crate::ctx::{CsrfForm, Ctx, de};
use crate::error::{AppError, AppResult};
use crate::models::{POST_COLUMNS, Post, Thread};
use crate::routes::forumdisplay::thread_rows;
use crate::templates::url_thread;
use crate::util::{self, now};
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use std::collections::HashMap;

/// Forums the viewer may search/list: (all viewable fids, fids restricted to own threads).
/// Forums the viewer may search: (all threads, only their own threads).
pub fn searchable_forums(ctx: &Ctx) -> (Vec<i32>, Vec<i32>) {
    ctx.access()
        .readable(crate::domain::access::Purpose::Search)
}

/// Forums whose threads the viewer may read (feeds, portal, statistics): (all, own only).
pub fn readable_forums(ctx: &Ctx) -> (Vec<i32>, Vec<i32>) {
    ctx.access().readable(crate::domain::access::Purpose::Read)
}

pub async fn search_form(ctx: Ctx) -> AppResult<Response> {
    if !ctx.perms.cansearch {
        return Err(AppError::no_perm());
    }
    let p = SearchParams {
        matchusername: true,
        ..Default::default()
    };
    form_page(&ctx, &p, None).await
}

const NO_RESULTS: &str = "Sorry, but no results were returned using the query information you provided. Please redefine your search terms and try again.";

/// The search form, filled in with `p`. A failed search comes back here instead of an error page,
/// so the visitor can adjust the query without retyping it.
async fn form_page(ctx: &Ctx, p: &SearchParams, error: Option<String>) -> AppResult<Response> {
    let jump = crate::routes::forumdisplay::forum_jump(ctx);
    let empty = error.as_deref() == Some(NO_RESULTS);
    let errors: Vec<String> = error.into_iter().filter(|_| !empty).collect();
    let filtered = !p.author.trim().is_empty()
        || p.forums.iter().any(|f| *f > 0)
        || p.postdate > 0
        || p.numreplies > 0
        || p.postthread == "2"
        || p.showresults == "posts"
        || !matches!(p.sortby.as_str(), "" | "lastpost")
        || p.sortordr == "asc";
    ctx.render(
        "search.html",
        minijinja::context! {
            title => "Search", forums => jump, errors => errors, form => p, empty => empty,
            filtered => filtered,
        },
    )
    .await
}

/// Runs a search; validation failures and empty results re-render the form.
async fn search_or_form(ctx: &Ctx, p: &SearchParams) -> AppResult<Response> {
    match run_search(ctx, p).await {
        Err(AppError::User(msg)) => form_page(ctx, p, Some(msg)).await,
        r => r,
    }
}

#[derive(Deserialize, Default, Clone, serde::Serialize)]
pub struct SearchParams {
    #[serde(default, deserialize_with = "de::string")]
    pub keywords: String,
    #[serde(default, deserialize_with = "de::string")]
    pub author: String,
    #[serde(default, deserialize_with = "de::bool")]
    pub matchusername: bool,
    #[serde(default, deserialize_with = "de::string")]
    pub postthread: String, // "1" = titles and posts, "2" = titles only
    #[serde(default, deserialize_with = "de::vec_i32")]
    pub forums: Vec<i32>,
    #[serde(default, deserialize_with = "de::bool")]
    pub subforums: bool,
    #[serde(default, deserialize_with = "de::i64")]
    pub postdate: i64,
    #[serde(default, deserialize_with = "de::string")]
    pub pddir: String, // "1" newer, "0" older
    #[serde(default, deserialize_with = "de::string")]
    pub sortby: String,
    #[serde(default, deserialize_with = "de::string")]
    pub sortordr: String,
    #[serde(default, deserialize_with = "de::string")]
    pub showresults: String, // threads | posts
    #[serde(default, deserialize_with = "de::i64")]
    pub numreplies: i64,
    #[serde(default, deserialize_with = "de::i32")]
    pub prefix: i32,
}

async fn flood_check(ctx: &Ctx) -> AppResult<()> {
    if ctx.can(crate::domain::staff::Cap::PostingExempt) {
        return Ok(());
    }
    let secs = ctx.settings().int("searchfloodtime");
    if secs <= 0 {
        return Ok(());
    }
    let key = if ctx.uid() > 0 {
        format!("search:u{}", ctx.uid())
    } else {
        format!("search:{}", ctx.ip)
    };
    if !ctx.app.rate_check(&key, 1, secs) {
        return Err(AppError::user(format!(
            "You're searching a little fast. Please wait {secs} seconds between searches."
        )));
    }
    Ok(())
}

async fn store(
    ctx: &Ctx,
    kind: &str,
    ids: Vec<i32>,
    keywords: &str,
    params: serde_json::Value,
) -> AppResult<String> {
    let sid = util::random_token(24);
    sqlx::query("INSERT INTO searchlog (sid, uid, dateline, ipaddress, resulttype, ids, keywords, params) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)")
        .bind(&sid)
        .bind(ctx.uid())
        .bind(now())
        .bind(crate::util::IpText::from(&ctx.ip))
        .bind(kind)
        .bind(&ids)
        .bind(keywords)
        .bind(params)
        .execute(&ctx.app.db)
        .await?;
    Ok(sid)
}

fn sort_sql(sortby: &str, order: &str, threads: bool) -> String {
    let dir = if order == "asc" { "ASC" } else { "DESC" };
    let col = match (sortby, threads) {
        ("subject", true) => "lower(t.subject)",
        ("replies", true) => "t.replies",
        ("views", true) => "t.views",
        ("starter", true) => "lower(t.username)",
        ("forum", _) => "t.fid",
        ("dateline", true) => "t.dateline",
        ("subject", false) => "lower(p.subject)",
        ("starter", false) => "lower(p.username)",
        (_, true) => "t.lastpost",
        (_, false) => "p.dateline",
    };
    format!("{col} {dir}")
}

pub async fn run_search(ctx: &Ctx, p: &SearchParams) -> AppResult<Response> {
    let s = ctx.settings();
    let keywords = p.keywords.trim().to_string();
    let author = p.author.trim().to_string();
    let minw = s.int("minsearchword").max(1) as usize;
    if keywords.is_empty() && author.is_empty() {
        return Err(AppError::user(
            "You did not enter any search terms. At a minimum, you must enter either some search terms or a username to search by.",
        ));
    }
    if !keywords.is_empty()
        && !keywords.split_whitespace().any(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .chars()
                .count()
                >= minw
        })
    {
        return Err(AppError::user(format!(
            "One or more of your search terms were shorter than the minimum length ({minw} characters)."
        )));
    }
    // Identical searches by the same viewer within the cache window reuse the stored result set.
    let cache_key = format!(
        "search:{}:{}:{:?}:{}",
        ctx.uid(),
        if ctx.uid() == 0 { ctx.ip.as_str() } else { "" },
        ctx.groups,
        serde_json::to_string(p).unwrap_or_default()
    );
    match ctx.app.short_cache.get(&cache_key) {
        Some(serde_json::Value::String(sid)) => {
            return Ok(Redirect::to(&format!("/search/results/{sid}")).into_response());
        }
        // A search that just found nothing; repeating it shouldn't count against the flood limit.
        Some(serde_json::Value::Null) => return Err(AppError::user(NO_RESULTS)),
        _ => {}
    }
    flood_check(ctx).await?;
    let (mut fids, mut own_only) = searchable_forums(ctx);
    if !p.forums.is_empty() && !p.forums.contains(&0) {
        let mut wanted = p.forums.clone();
        if p.subforums {
            for f in &p.forums {
                wanted.extend(ctx.cache.descendants(*f));
            }
        }
        fids.retain(|f| wanted.contains(f));
        own_only.retain(|f| wanted.contains(f));
    }
    let author_uids: Vec<i32> = if author.is_empty() {
        vec![]
    } else if p.matchusername {
        sqlx::query_scalar("SELECT uid FROM users WHERE lower(username) = lower($1)")
            .bind(&author)
            .fetch_all(&ctx.app.db)
            .await?
    } else {
        sqlx::query_scalar("SELECT uid FROM users WHERE username ILIKE '%' || $1 || '%' LIMIT 200")
            .bind(&author)
            .fetch_all(&ctx.app.db)
            .await?
    };
    if !author.is_empty() && author_uids.is_empty() {
        ctx.app
            .short_cache
            .insert(cache_key, serde_json::Value::Null);
        return Err(AppError::user(NO_RESULTS));
    }
    let limit = s.int("searchhardlimit").max(50);
    let date_cond = if p.postdate > 0 {
        now().saturating_sub(p.postdate.saturating_mul(86400))
    } else {
        0
    };
    let newer = p.pddir != "0";
    let threads_mode = p.showresults != "posts";
    let titles_only = p.postthread == "2";
    let uid = ctx.uid();
    let is_mod_all = ctx.is_supermod();
    let states: Vec<i16> = if is_mod_all { vec![1, 0, -1] } else { vec![1] };

    let ids: Vec<i32> = if titles_only || (keywords.is_empty() && threads_mode) {
        // Thread subject search (trigram index) and/or thread starter filter.
        let words: Vec<String> = keywords
            .split_whitespace()
            .map(|w| format!("%{}%", w.replace('%', "\\%").replace('_', "\\_")))
            .collect();
        let order = sort_sql(&p.sortby, &p.sortordr, true);
        // Many `%word%` patterns are as expensive as full text: same concurrency cap and timeout.
        let _permit = ctx
            .app
            .search_sem
            .acquire()
            .await
            .map_err(|_| AppError::user("Search is temporarily unavailable."))?;
        let mut tx = ctx.app.db.begin().await?;
        sqlx::query("SET LOCAL statement_timeout = '5s'")
            .execute(&mut *tx)
            .await?;
        let words: Vec<String> = words.into_iter().take(10).collect();
        let res: Result<Vec<i32>, sqlx::Error> = sqlx::query_scalar(&format!(
            "SELECT t.tid FROM threads t WHERE (t.fid = ANY($1) OR (t.fid = ANY($2) AND t.uid = $3)) AND t.visible = ANY($4)
               AND t.closed NOT LIKE 'moved|%'
               AND (cardinality($5::text[]) = 0 OR t.subject ILIKE ALL($5))
               AND (cardinality($6::int[]) = 0 OR t.uid = ANY($6))
               AND ($7 = 0 OR ($8 AND t.lastpost >= $7) OR (NOT $8 AND t.lastpost < $7))
               AND ($9 = 0 OR t.replies >= $9) AND ($11 = 0 OR t.prefix = $11)
             ORDER BY {order} LIMIT $10"
        ))
        .bind(&fids)
        .bind(&own_only)
        .bind(uid)
        .bind(&states)
        .bind(&words)
        .bind(&author_uids)
        .bind(date_cond)
        .bind(newer)
        .bind(p.numreplies)
        .bind(limit)
        .bind(p.prefix)
        .fetch_all(&mut *tx)
        .await;
        match res {
            Ok(v) => {
                tx.commit().await?;
                v
            }
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("57014") => {
                return Err(AppError::user(
                    "Your search was too broad and took too long. Please use more specific search terms or narrow the forums searched.",
                ));
            }
            Err(e) => return Err(e.into()),
        }
    } else {
        // Full text over posts.
        let q = keywords.clone();
        let base = "FROM posts p JOIN threads t ON t.tid = p.tid
               WHERE (p.fid = ANY($1) OR (p.fid = ANY($2) AND t.uid = $3)) AND p.visible = ANY($4) AND t.visible = ANY($4)
               AND ($5 = '' OR p.search_tsv @@ websearch_to_tsquery('english', $5))
               AND (cardinality($6::int[]) = 0 OR p.uid = ANY($6))
               AND ($7 = 0 OR ($8 AND p.dateline >= $7) OR (NOT $8 AND p.dateline < $7))
               AND ($9 = 0 OR t.replies >= $9) AND ($11 = 0 OR t.prefix = $11)";
        // Cap concurrent full-text searches and bound their runtime so a burst of
        // expensive queries can never starve the connection pool.
        let _permit = ctx
            .app
            .search_sem
            .acquire()
            .await
            .map_err(|_| AppError::user("Search is temporarily unavailable."))?;
        let mut tx = ctx.app.db.begin().await?;
        sqlx::query("SET LOCAL statement_timeout = '5s'")
            .execute(&mut *tx)
            .await?;
        let res: Result<Vec<i32>, sqlx::Error> = if threads_mode {
            // Scan matching posts newest-first (backward pkey scan stops early),
            // then collapse to distinct threads preserving recency.
            let cands: Result<Vec<i32>, sqlx::Error> = sqlx::query_scalar(&format!(
                "SELECT p.tid {base} ORDER BY p.pid DESC LIMIT $10"
            ))
            .bind(&fids)
            .bind(&own_only)
            .bind(uid)
            .bind(&states)
            .bind(&q)
            .bind(&author_uids)
            .bind(date_cond)
            .bind(newer)
            .bind(p.numreplies)
            .bind((limit * 3).min(1500))
            .bind(p.prefix)
            .fetch_all(&mut *tx)
            .await;
            match cands {
                Ok(c) => {
                    let mut seen = std::collections::HashSet::new();
                    let tids: Vec<i32> = c
                        .into_iter()
                        .filter(|t| seen.insert(*t))
                        .take(limit as usize)
                        .collect();
                    if p.sortby.is_empty() || p.sortby == "lastpost" && p.sortordr != "asc" {
                        sqlx::query_scalar("SELECT tid FROM threads WHERE tid = ANY($1) ORDER BY lastpost DESC, tid DESC").bind(&tids).fetch_all(&mut *tx).await
                    } else {
                        let order = sort_sql(&p.sortby, &p.sortordr, true);
                        sqlx::query_scalar(&format!(
                            "SELECT t.tid FROM threads t WHERE t.tid = ANY($1) ORDER BY {order}"
                        ))
                        .bind(&tids)
                        .fetch_all(&mut *tx)
                        .await
                    }
                }
                Err(e) => Err(e),
            }
        } else {
            let order = if !q.is_empty() && p.sortby == "relevance" {
                "ts_rank(p.search_tsv, websearch_to_tsquery('english', $5)) DESC".to_string()
            } else if p.sortby.is_empty() || p.sortby == "dateline" && p.sortordr != "asc" {
                "p.pid DESC".to_string()
            } else {
                sort_sql(&p.sortby, &p.sortordr, false)
            };
            sqlx::query_scalar(&format!("SELECT p.pid {base} ORDER BY {order} LIMIT $10"))
                .bind(&fids)
                .bind(&own_only)
                .bind(uid)
                .bind(&states)
                .bind(&q)
                .bind(&author_uids)
                .bind(date_cond)
                .bind(newer)
                .bind(p.numreplies)
                .bind(limit)
                .bind(p.prefix)
                .fetch_all(&mut *tx)
                .await
        };
        match res {
            Ok(v) => {
                tx.commit().await?;
                v
            }
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("57014") => {
                return Err(AppError::user(
                    "Your search was too broad and took too long. Please use more specific search terms or narrow the forums searched.",
                ));
            }
            Err(e) => return Err(e.into()),
        }
    };
    if ids.is_empty() {
        ctx.app
            .short_cache
            .insert(cache_key, serde_json::Value::Null);
        return Err(AppError::user(NO_RESULTS));
    }
    let sid = store(
        ctx,
        if threads_mode || titles_only {
            "threads"
        } else {
            "posts"
        },
        ids,
        &keywords,
        serde_json::to_value(p).unwrap_or_default(),
    )
    .await?;
    ctx.app
        .short_cache
        .insert(cache_key, serde_json::Value::String(sid.clone()));
    Ok(Redirect::to(&format!("/search/results/{sid}")).into_response())
}

pub async fn do_search(ctx: Ctx, CsrfForm(p): CsrfForm<SearchParams>) -> AppResult<Response> {
    if !ctx.perms.cansearch {
        return Err(AppError::no_perm());
    }
    search_or_form(&ctx, &p).await
}

#[derive(Deserialize, Default)]
pub struct QuickQuery {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub fid: i32,
}

pub async fn quick(ctx: Ctx, Query(q): Query<QuickQuery>) -> AppResult<Response> {
    if !ctx.perms.cansearch {
        return Err(AppError::no_perm());
    }
    let p = SearchParams {
        keywords: q.q,
        showresults: "threads".into(),
        postthread: "1".into(),
        forums: if q.fid > 0 { vec![q.fid] } else { vec![] },
        subforums: true,
        ..Default::default()
    };
    search_or_form(&ctx, &p).await
}

#[derive(Deserialize, Default)]
pub struct PageQuery {
    pub page: Option<i64>,
}

pub async fn results(
    ctx: Ctx,
    Path(sid): Path<String>,
    Query(q): Query<PageQuery>,
) -> AppResult<Response> {
    let row: Option<(i32, String, Vec<i32>, String, i64, serde_json::Value)> = sqlx::query_as(
        "SELECT uid, resulttype, ids, keywords, dateline, params FROM searchlog WHERE sid = $1",
    )
    .bind(&sid)
    .fetch_optional(&ctx.app.db)
    .await?;
    let (owner, kind, ids, keywords, _dl, params) =
        row.ok_or_else(|| AppError::user("The search results have expired. Please search again."))?;
    // Thread listings (what's new, unread, ...) are stored with their title.
    let listing = params["listing"].as_str().map(str::to_string);
    let title = listing.clone().unwrap_or_else(|| "Search Results".into());
    if owner != ctx.uid() && owner != 0 {
        return Err(AppError::no_perm());
    }
    let per = ctx.settings().int("searchresultsperpage").max(5);
    let total = ids.len() as i64;
    let pg = util::paginate(
        total,
        per,
        util::clamp_page(q.page),
        &format!("/search/results/{sid}?page={{page}}"),
    );
    let slice: Vec<i32> = ids
        .iter()
        .skip(((pg.page - 1) * per) as usize)
        .take(per as usize)
        .copied()
        .collect();
    let highlight: Vec<String> = keywords
        .split_whitespace()
        .map(|w| w.trim_matches('"').to_string())
        .filter(|w| w.len() >= 2 && !w.starts_with('-'))
        .take(10)
        .collect();
    let hl_param = if highlight.is_empty() {
        String::new()
    } else {
        format!(
            "?highlight={}",
            percent_encoding::utf8_percent_encode(
                &highlight.join(" "),
                percent_encoding::NON_ALPHANUMERIC
            )
        )
    };
    if kind == "threads" {
        let threads: Vec<Thread> = sqlx::query_as(&format!(
            "SELECT {} FROM threads WHERE tid = ANY($1)",
            crate::models::THREAD_COLUMNS
        ))
        .bind(&slice)
        .fetch_all(&ctx.app.db)
        .await?;
        let mut by_id: HashMap<i32, Thread> = threads.into_iter().map(|t| (t.tid, t)).collect();
        let ordered: Vec<Thread> = slice
            .iter()
            .filter_map(|id| by_id.remove(id))
            .filter(|t| ctx.access().can_read_thread(t.fid, t.uid, ctx.uid()))
            .collect();
        let mut rows = thread_rows(&ctx, ordered).await?;
        for r in rows.iter_mut() {
            r.url = format!("{}{}", r.url, hl_param);
        }
        ctx.render("search_results.html", minijinja::context! { title => title, listing => listing, kind => "threads", threads => rows, pagination => pg, keywords => keywords, total => total }).await
    } else {
        let posts: Vec<Post> = sqlx::query_as(&format!(
            "SELECT {POST_COLUMNS} FROM posts WHERE pid = ANY($1)"
        ))
        .bind(&slice)
        .fetch_all(&ctx.app.db)
        .await?;
        let tids: Vec<i32> = posts.iter().map(|p| p.tid).collect();
        let thread_rows: Vec<(i32, String, i32, i32, i32, i16)> = sqlx::query_as(
            "SELECT tid, subject, replies, views, uid, visible FROM threads WHERE tid = ANY($1)",
        )
        .bind(&tids)
        .fetch_all(&ctx.app.db)
        .await?;
        let thread_authors: HashMap<i32, i32> = thread_rows.iter().map(|r| (r.0, r.4)).collect();
        let thread_states: HashMap<i32, i16> = thread_rows.iter().map(|r| (r.0, r.5)).collect();
        let subjects: HashMap<i32, (String, i32, i32)> = thread_rows
            .into_iter()
            .map(|(t, s, r, v, _, _)| (t, (s, r, v)))
            .collect();
        let mut by_id: HashMap<i32, Post> = posts.into_iter().map(|p| (p.pid, p)).collect();
        let mut stale = vec![];
        let mut out = vec![];
        for id in &slice {
            let Some(p) = by_id.remove(id) else { continue };
            // Search results are stored; check again what the viewer may read now.
            let author = thread_authors.get(&p.tid).copied().unwrap_or(0);
            if !ctx.access().can_read_thread(p.fid, author, ctx.uid())
                || !ctx.visible_states(p.fid).contains(&p.visible)
                || !thread_states
                    .get(&p.tid)
                    .is_some_and(|v| ctx.visible_states(p.fid).contains(v))
            {
                continue;
            }
            let html = crate::render::post_html(&ctx, &p, &mut stale);
            let text = util::truncate_chars(&strip(&html), 400);
            let snippet = crate::parser::highlight(&util::escape_html(&text), &highlight);
            let (tsub, replies, views) = subjects.get(&p.tid).cloned().unwrap_or_default();
            let forum = ctx.cache.forum(p.fid);
            out.push(minijinja::context! {
                pid => p.pid, tid => p.tid, subject => if p.subject.is_empty() { tsub.clone() } else { p.subject.clone() }, thread_subject => tsub.clone(),
                thread_url => url_thread(p.tid as i64, Some(&tsub)), uid => p.uid, username => &p.username, dateline => p.dateline,
                snippet => snippet, replies => replies, views => views, forum_name => forum.map(|f| f.name.clone()),
                forum_url => crate::templates::url_forum(p.fid as i64, forum.map(|f| f.name.as_str())), hl => &hl_param,
            });
        }
        crate::render::store_parsed(&ctx, stale);
        ctx.render("search_results.html", minijinja::context! { title => title, listing => listing, kind => "posts", posts => out, pagination => pg, keywords => keywords, total => total }).await
    }
}

fn strip(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let t = out
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// An empty thread listing: a friendly page instead of an error, with ways to keep reading.
async fn empty_listing(ctx: &Ctx, title: &str, message: &str) -> AppResult<Response> {
    let pg = util::paginate(0, 1, 1, "");
    ctx.render(
        "search_results.html",
        minijinja::context! {
            title => title, kind => "threads", threads => Vec::<String>::new(), pagination => pg,
            keywords => "", total => 0, empty => message, listing => true,
        },
    )
    .await
}

async fn thread_listing(
    ctx: &Ctx,
    cond: &str,
    arg: i64,
    (title, empty): (&str, &str),
) -> AppResult<Response> {
    let (fids, own_only) = searchable_forums(ctx);
    let limit = ctx.settings().int("searchhardlimit").max(50);
    let tids: Vec<i32> = sqlx::query_scalar(&format!(
        "SELECT t.tid FROM threads t WHERE (t.fid = ANY($1) OR (t.fid = ANY($2) AND t.uid = $3)) AND t.visible = 1 AND t.closed NOT LIKE 'moved|%' AND {cond}
         ORDER BY t.lastpost DESC LIMIT $5"
    ))
    .bind(&fids)
    .bind(&own_only)
    .bind(ctx.uid())
    .bind(arg)
    .bind(limit)
    .fetch_all(&ctx.app.db)
    .await?;
    if tids.is_empty() {
        return empty_listing(ctx, title, empty).await;
    }
    let sid = store(
        ctx,
        "threads",
        tids,
        "",
        serde_json::json!({ "listing": title }),
    )
    .await?;
    Ok(Redirect::to(&format!("/search/results/{sid}")).into_response())
}

pub async fn new_posts(ctx: Ctx) -> AppResult<Response> {
    let since = ctx
        .user
        .as_ref()
        .map(|u| u.lastvisit)
        .filter(|v| *v > 0)
        .unwrap_or_else(|| now() - 86400);
    thread_listing(
        &ctx,
        "t.lastpost > $4",
        since,
        ("What's new", "Nothing new since your last visit."),
    )
    .await
}

pub async fn today_posts(ctx: Ctx) -> AppResult<Response> {
    thread_listing(
        &ctx,
        "t.lastpost > $4",
        now() - 86400,
        ("Today's posts", "Nobody has posted in the last 24 hours."),
    )
    .await
}

pub async fn unanswered(ctx: Ctx) -> AppResult<Response> {
    thread_listing(
        &ctx,
        "t.replies = 0 AND t.dateline > $4",
        now() - 90 * 86400,
        (
            "Unanswered threads",
            "Every thread from the last three months has a reply.",
        ),
    )
    .await
}

pub async fn unread_threads(ctx: Ctx) -> AppResult<Response> {
    let me = ctx.require_login()?;
    let cut = now() - ctx.settings().int("threadreadcut").max(1) * 86400;
    let cond = format!(
        "t.lastpost > $4 AND NOT EXISTS (SELECT 1 FROM threadsread tr WHERE tr.tid = t.tid AND tr.uid = {uid} AND tr.dateline >= t.lastpost)
         AND NOT EXISTS (SELECT 1 FROM forumsread fr WHERE fr.fid = t.fid AND fr.uid = {uid} AND fr.dateline >= t.lastpost)",
        uid = me.uid
    );
    thread_listing(
        &ctx,
        &cond,
        cut,
        ("Unread threads", "You're all caught up."),
    )
    .await
}

pub async fn user_threads(ctx: Ctx, Path(uid): Path<i32>) -> AppResult<Response> {
    let (fids, own_only) = searchable_forums(&ctx);
    let tids: Vec<i32> = sqlx::query_scalar(
        "SELECT tid FROM threads WHERE uid = $1 AND (fid = ANY($2) OR (fid = ANY($3) AND uid = $4)) AND visible = 1 AND closed NOT LIKE 'moved|%' ORDER BY dateline DESC LIMIT $5",
    )
    .bind(uid)
    .bind(&fids)
    .bind(&own_only)
    .bind(ctx.uid())
    .bind(ctx.settings().int("searchhardlimit").max(50))
    .fetch_all(&ctx.app.db)
    .await?;
    if tids.is_empty() {
        return Err(AppError::user(
            "This member has not started any threads you can see.",
        ));
    }
    let sid = store(&ctx, "threads", tids, "", serde_json::json!({})).await?;
    Ok(Redirect::to(&format!("/search/results/{sid}")).into_response())
}

pub async fn user_posts(ctx: Ctx, Path(uid): Path<i32>) -> AppResult<Response> {
    let (fids, own_only) = searchable_forums(&ctx);
    let pids: Vec<i32> = sqlx::query_scalar(
        "SELECT p.pid FROM posts p JOIN threads t ON t.tid = p.tid WHERE p.uid = $1 AND (p.fid = ANY($2) OR (p.fid = ANY($3) AND t.uid = $4))
         AND p.visible = 1 AND t.visible = 1 ORDER BY p.dateline DESC LIMIT $5",
    )
    .bind(uid)
    .bind(&fids)
    .bind(&own_only)
    .bind(ctx.uid())
    .bind(ctx.settings().int("searchhardlimit").max(50))
    .fetch_all(&ctx.app.db)
    .await?;
    if pids.is_empty() {
        return Err(AppError::user("This member has no posts you can see."));
    }
    let sid = store(&ctx, "posts", pids, "", serde_json::json!({})).await?;
    Ok(Redirect::to(&format!("/search/results/{sid}")).into_response())
}
