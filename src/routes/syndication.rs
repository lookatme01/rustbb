//! RSS 2.0 / Atom feeds, XML sitemap and robots.txt.

use crate::ctx::Ctx;
use crate::error::{AppError, AppResult};
use crate::util::escape_html;
use axum::extract::Query;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

#[derive(Deserialize, Default)]
pub struct FeedQuery {
    #[serde(default)]
    pub fid: String,
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub limit: i64,
}

fn rfc2822(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .unwrap_or_default()
        .to_rfc2822()
}
fn rfc3339(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .unwrap_or_default()
        .to_rfc3339()
}

pub async fn feed(ctx: Ctx, Query(q): Query<FeedQuery>) -> AppResult<Response> {
    let s = ctx.settings();
    if !s.bool("enablesyndication") {
        return Err(AppError::not_found("feed"));
    }
    let (mut fids, _) = crate::routes::search::searchable_forums(&ctx);
    let wanted: Vec<i32> = q
        .fid
        .split(',')
        .filter_map(|x| x.trim().parse().ok())
        .collect();
    if !wanted.is_empty() {
        let mut all = wanted.clone();
        for w in &wanted {
            all.extend(ctx.cache.descendants(*w));
        }
        fids.retain(|f| all.contains(f));
    }
    let limit = if q.limit > 0 {
        q.limit.min(50)
    } else {
        s.int("syndicationitems").max(1)
    };
    let atom = q.r#type == "atom";
    let ctype = if atom {
        "application/atom+xml; charset=utf-8"
    } else {
        "application/rss+xml; charset=utf-8"
    };
    // Feeds depend only on the visible forum set, so identical viewers share one rendering.
    let key = format!("feed:{atom}:{limit}:{fids:?}");
    if let Some(serde_json::Value::String(body)) = ctx.app.short_cache.get(&key) {
        return Ok((
            [
                (header::CONTENT_TYPE, ctype),
                (header::CACHE_CONTROL, "public, max-age=300"),
            ],
            body,
        )
            .into_response());
    }
    // Top-N per forum via the (fid, dateline) index, then merge; avoids sorting every thread.
    let rows: Vec<(i32, String, String, i64, i32, String)> = sqlx::query_as(
        "SELECT t.tid, t.subject, t.username, t.dateline, t.fid, COALESCE(p.message, '')
         FROM unnest($1::int[]) f(fid)
         CROSS JOIN LATERAL (SELECT tid, subject, username, dateline, fid, firstpost FROM threads
             WHERE fid = f.fid AND visible = 1 AND closed NOT LIKE 'moved|%' ORDER BY dateline DESC LIMIT $2) t
         LEFT JOIN posts p ON p.pid = t.firstpost
         ORDER BY t.dateline DESC LIMIT $2",
    )
    .bind(&fids)
    .bind(limit)
    .fetch_all(&ctx.app.db)
    .await?;
    let bburl = s.get("bburl").trim_end_matches('/').to_string();
    let bbname = escape_html(s.get("bbname"));
    let opts = crate::parser::ParseOptions {
        allow_videocode: false,
        ..Default::default()
    };
    let mut out = String::with_capacity(8192);
    if atom {
        out.push_str(&format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<feed xmlns=\"http://www.w3.org/2005/Atom\">\n<title>{bbname}</title>\n<link href=\"{bburl}/\"/>\n<link rel=\"self\" href=\"{bburl}/syndication?type=atom\"/>\n<id>{bburl}/</id>\n<updated>{}</updated>\n",
            rfc3339(rows.first().map(|r| r.3).unwrap_or(0))
        ));
    } else {
        out.push_str(&format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<rss version=\"2.0\" xmlns:atom=\"http://www.w3.org/2005/Atom\">\n<channel>\n<title>{bbname}</title>\n<link>{bburl}/</link>\n<description>Latest threads from {bbname}</description>\n<atom:link href=\"{bburl}/syndication\" rel=\"self\" type=\"application/rss+xml\"/>\n"
        ));
    }
    for (tid, subject, username, dateline, fid, message) in rows {
        let html = crate::render::parse_with(
            &ctx.cache,
            &ctx.app.plugins,
            &opts,
            &crate::util::truncate_chars(&message, 4000),
        );
        let link = format!("{bburl}/thread/{tid}");
        let forum = ctx
            .cache
            .forum(fid)
            .map(|f| escape_html(&f.name))
            .unwrap_or_default();
        if atom {
            out.push_str(&format!(
                "<entry><title>{}</title><link href=\"{link}\"/><id>{link}</id><updated>{}</updated><author><name>{}</name></author><category term=\"{forum}\"/><content type=\"html\">{}</content></entry>\n",
                escape_html(&subject), rfc3339(dateline), escape_html(&username), escape_html(&html)
            ));
        } else {
            out.push_str(&format!(
                "<item><title>{}</title><link>{link}</link><guid isPermaLink=\"true\">{link}</guid><pubDate>{}</pubDate><dc:creator xmlns:dc=\"http://purl.org/dc/elements/1.1/\">{}</dc:creator><category>{forum}</category><description>{}</description></item>\n",
                escape_html(&subject), rfc2822(dateline), escape_html(&username), escape_html(&html)
            ));
        }
    }
    out.push_str(if atom {
        "</feed>\n"
    } else {
        "</channel>\n</rss>\n"
    });
    ctx.app
        .short_cache
        .insert(key, serde_json::Value::String(out.clone()));
    Ok((
        [
            (header::CONTENT_TYPE, ctype),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        out,
    )
        .into_response())
}

pub async fn sitemap(ctx: Ctx) -> AppResult<Response> {
    let s = ctx.settings();
    let bburl = s.get("bburl").trim_end_matches('/').to_string();
    let (fids, _) = crate::routes::search::searchable_forums(&ctx);
    let threads: Vec<(i32, String, i64)> = sqlx::query_as(
        "SELECT tid, subject, lastpost FROM threads WHERE fid = ANY($1) AND visible = 1 AND closed NOT LIKE 'moved|%' ORDER BY lastpost DESC LIMIT 45000",
    )
    .bind(&fids)
    .fetch_all(&ctx.app.db)
    .await?;
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
    );
    out.push_str(&format!(
        "<url><loc>{bburl}/</loc><changefreq>hourly</changefreq></url>\n"
    ));
    for f in &fids {
        if let Some(forum) = ctx.cache.forum(*f) {
            out.push_str(&format!(
                "<url><loc>{bburl}{}</loc><changefreq>hourly</changefreq></url>\n",
                escape_html(&crate::templates::url_forum(*f as i64, Some(&forum.name)))
            ));
        }
    }
    for (tid, subject, lp) in threads {
        out.push_str(&format!(
            "<url><loc>{bburl}{}</loc><lastmod>{}</lastmod></url>\n",
            escape_html(&crate::templates::url_thread(tid as i64, Some(&subject))),
            chrono::DateTime::from_timestamp(lp, 0)
                .unwrap_or_default()
                .format("%Y-%m-%d")
        ));
    }
    out.push_str("</urlset>\n");
    Ok((
        [
            (header::CONTENT_TYPE, "application/xml; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        out,
    )
        .into_response())
}

pub async fn robots(ctx: Ctx) -> AppResult<Response> {
    let bburl = ctx
        .settings()
        .get("bburl")
        .trim_end_matches('/')
        .to_string();
    let body = format!(
        "User-agent: *\nDisallow: /admin\nDisallow: /modcp\nDisallow: /usercp\nDisallow: /pm\nDisallow: /search\nDisallow: /newreply\nDisallow: /newthread\nDisallow: /member/\nDisallow: /report\nDisallow: /attachment/\nDisallow: /captcha/\nSitemap: {bburl}/sitemap.xml\n"
    );
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response())
}
