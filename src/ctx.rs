//! Per-request context: who the viewer is, their permissions, theme, CSRF token, and helpers
//! for rendering pages. Built once per request by `context_middleware`.

use crate::app::{Activity, App};
use crate::cache::Cache;
use crate::error::{AppError, AppResult, ErrorMarker};
use crate::models::User;
use crate::perms::{ForumPerms, GroupPerms, ModPerms};
use crate::util::{self, now};
use axum::extract::{ConnectInfo, FromRequest, FromRequestParts, Request};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header, request::Parts};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use chrono_tz::Tz;
use minijinja::Value;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Row};
use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

pub const AUTH_COOKIE: &str = "rbb_auth";
pub const SID_COOKIE: &str = "rbb_sid";
pub const FLASH_COOKIE: &str = "rbb_flash";

pub struct CtxInner {
    pub app: App,
    pub cache: Arc<Cache>,
    pub user: Option<User>,
    pub groups: Vec<i32>,
    pub perms: GroupPerms,
    pub ip: String,
    pub sid: String,
    pub csrf: String,
    pub token_hash: Option<String>,
    pub acp_verified: i64,
    pub theme: AtomicI32,
    pub tz: Tz,
    pub lang: String,
    pub datefmt: String,
    pub timefmt: String,
    pub path: String,
    pub query: String,
    pub method: String,
    pub useragent: String,
    pub bot: Option<&'static str>,
    pub is_api: bool,
    /// Authenticated with an `Authorization: Bearer` token (no ambient cookies → CSRF not applicable).
    pub bearer: bool,
    pub flash: Option<String>,
    pub cookies: Mutex<Vec<String>>,
    pub location: (AtomicI32, AtomicI32),
    pub headers: HeaderMap,
    /// Guest page cache key when this request may be served from / stored in the cache.
    pub guest_cache: Option<String>,
    /// Set by handlers whose guest output is safe to cache (`allow_guest_cache`).
    pub guest_cacheable: std::sync::atomic::AtomicBool,
    /// What a cacheable page depends on (`board`, `forum:<fid>`, `thread:<tid>`).
    pub guest_cache_tags: Mutex<Vec<String>>,
    /// What a write changed, if the handler knows (`None` = unknown: clear all guest pages).
    pub write_scope: Mutex<Option<Vec<String>>>,
    /// The viewer's forum read markers, loaded at most once per request.
    pub forums_read: tokio::sync::OnceCell<std::collections::HashMap<i32, i64>>,
}

#[derive(Clone)]
pub struct Ctx(pub Arc<CtxInner>);

impl Deref for Ctx {
    type Target = CtxInner;
    fn deref(&self) -> &CtxInner {
        &self.0
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Ctx {
    type Rejection = AppError;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Ctx>()
            .cloned()
            .ok_or_else(|| AppError::Other(anyhow::anyhow!("context middleware not installed")))
    }
}

#[derive(Serialize)]
struct ViewerInfo<'a> {
    uid: i32,
    username: &'a str,
    formatted: String,
    avatar: String,
    usergroup: i32,
    unreadpms: i32,
    unreadalerts: i32,
    lastvisit: i64,
    postnum: i32,
    colormode: &'a str,
    showsigs: bool,
    showavatars: bool,
    showquickreply: bool,
    invisible: bool,
    away: bool,
}

pub fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>, trust_proxy: bool) -> String {
    if trust_proxy {
        // The trusted proxy appends the address it saw to the end of X-Forwarded-For; anything
        // before it was supplied by the client and can be forged. Use the last entry.
        let xff: Vec<&str> = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        if let Some(last) = xff.last().and_then(|v| v.rsplit(',').next()) {
            let ip = last.trim();
            if ip.parse::<std::net::IpAddr>().is_ok() {
                return ip.to_string();
            }
        }
        if let Some(v) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
            if v.parse::<std::net::IpAddr>().is_ok() {
                return v.to_string();
            }
        }
    }
    peer.map(|p| p.ip().to_string())
        .unwrap_or_else(|| "0.0.0.0".into())
}

pub fn get_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    for v in headers.get_all(header::COOKIE) {
        let Ok(s) = v.to_str() else { continue };
        for part in s.split(';') {
            if let Some((k, val)) = part.trim().split_once('=') {
                if k == name {
                    return Some(
                        percent_encoding::percent_decode_str(val)
                            .decode_utf8_lossy()
                            .into_owned(),
                    );
                }
            }
        }
    }
    None
}

impl CtxInner {
    pub fn uid(&self) -> i32 {
        self.user.as_ref().map(|u| u.uid).unwrap_or(0)
    }
    pub fn username(&self) -> &str {
        self.user
            .as_ref()
            .map(|u| u.username.as_str())
            .unwrap_or("Guest")
    }
    pub fn logged_in(&self) -> bool {
        self.user.is_some()
    }
    pub fn settings(&self) -> &crate::settings::Settings {
        &self.cache.settings
    }
    pub fn me(&self) -> AppResult<&User> {
        self.user.as_ref().ok_or(AppError::LoginRequired)
    }
    pub fn is_admin(&self) -> bool {
        self.perms.cancp
    }
    pub fn is_supermod(&self) -> bool {
        self.perms.issupermod || self.perms.cancp
    }
    pub fn forum_perms(&self, fid: i32) -> ForumPerms {
        self.cache.forum_perms(&self.groups, fid)
    }
    pub fn mod_perms(&self, fid: i32) -> Option<ModPerms> {
        self.cache
            .mod_perms(self.uid(), &self.groups, &self.perms, fid)
    }
    pub fn is_mod(&self, fid: i32) -> bool {
        self.mod_perms(fid).is_some()
    }
    pub fn is_any_mod(&self) -> bool {
        self.cache
            .is_any_moderator(self.uid(), &self.groups, &self.perms)
    }
    pub fn theme_id(&self) -> i32 {
        self.theme.load(Ordering::Relaxed)
    }
    pub fn set_theme(&self, tid: i32) {
        if tid > 0 && self.cache.theme(tid).is_some() {
            self.theme.store(tid, Ordering::Relaxed);
        }
    }
    pub fn set_location(&self, fid: i32, tid: i32) {
        self.location.0.store(fid, Ordering::Relaxed);
        self.location.1.store(tid, Ordering::Relaxed);
    }
    pub fn add_cookie(&self, name: &str, value: &str, max_age: Option<i64>, http_only: bool) {
        let mut c = format!(
            "{name}={}; Path=/; SameSite=Lax",
            percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC)
        );
        if let Some(a) = max_age {
            c.push_str(&format!("; Max-Age={a}"));
        }
        if http_only {
            c.push_str("; HttpOnly");
        }
        if self.app.cfg.secure_cookies {
            c.push_str("; Secure");
        }
        self.cookies.lock().unwrap().push(c);
    }
    pub fn clear_cookie(&self, name: &str) {
        self.add_cookie(name, "", Some(0), true);
    }

    /// Verify a CSRF token submitted with a form (`my_post_key`, as in MyBB) or header.
    pub fn check_csrf(&self, token: &str) -> AppResult<()> {
        if self.bearer {
            return Ok(());
        }
        let hdr = self
            .headers
            .get("x-csrf-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if (!token.is_empty() && util::ct_eq(token, &self.csrf))
            || (!hdr.is_empty() && util::ct_eq(hdr, &self.csrf))
        {
            Ok(())
        } else {
            Err(AppError::Csrf)
        }
    }

    pub fn require_login(&self) -> AppResult<&User> {
        self.me()
    }

    /// Check the viewer can see a forum (active, permissions, password). Returns its perms.
    pub fn check_forum(&self, fid: i32) -> AppResult<(&crate::models::Forum, ForumPerms)> {
        let forum = self
            .cache
            .forum(fid)
            .ok_or_else(|| AppError::not_found("forum"))?;
        let perms = self.forum_perms(fid);
        if !perms.canview || (!forum.active && !self.is_mod(fid)) {
            return Err(AppError::no_perm());
        }
        // Password-protected forums (and their children).
        for pfid in &forum.parentlist {
            if let Some(pf) = self.cache.forum(*pfid) {
                if pf.has_password() && !self.is_mod(*pfid) {
                    let cookie =
                        get_cookie(&self.headers, &format!("forumpass_{pfid}")).unwrap_or_default();
                    let expect = util::hmac_hex(
                        &self.app.cfg.secret,
                        &format!("forumpass:{pfid}:{}", pf.password),
                    );
                    if !util::ct_eq(&cookie, &expect) {
                        return Err(AppError::User(format!("__forumpass__{pfid}")));
                    }
                }
            }
        }
        Ok((forum, perms))
    }

    /// Which `visible` states of threads/posts the viewer may *see the content of* in a forum.
    /// Only moderators get unapproved (0) or soft-deleted (-1) content.
    pub fn visible_states(&self, fid: i32) -> Vec<i16> {
        match self.mod_perms(fid) {
            Some(m) => {
                let mut v = vec![1];
                if m.canviewunapprove {
                    v.push(0);
                }
                if m.canviewdeleted {
                    v.push(-1);
                }
                v
            }
            None => vec![1],
        }
    }

    /// States of the posts *listed* in a thread the viewer can open: `visible_states` plus
    /// soft-deleted posts for members with "can view deletion notices", which are shown as a
    /// "This post was deleted." placeholder without their content (see `build_postbits`).
    /// Only for post listings and their page arithmetic, never to decide access to content.
    pub fn listed_states(&self, fid: i32) -> Vec<i16> {
        let mut v = self.visible_states(fid);
        if !v.contains(&-1) && self.mod_perms(fid).is_none() && self.forum_perms(fid).canviewdeletionnotice {
            v.push(-1);
        }
        v
    }

    pub fn fmt_date(&self, ts: i64, style: &str) -> String {
        util::format_date(ts, self.tz, &self.datefmt, &self.timefmt, style)
    }

    pub fn format_user(&self, username: &str, usergroup: i32, displaygroup: i32) -> String {
        self.cache.format_name(username, usergroup, displaygroup)
    }

    /// A browser speculative prefetch/prerender (`Sec-Purpose: prefetch`): serve the page but
    /// skip visit side effects such as marking threads read or counting views.
    /// Mark this page as the same for every guest, so its HTML may be cached and reused, and
    /// say what it shows so writes elsewhere don't evict it.
    pub fn allow_guest_cache(&self, tags: &[String]) {
        if self.guest_cache.is_some() {
            self.guest_cacheable.store(true, Ordering::Relaxed);
            *self.guest_cache_tags.lock().unwrap() = tags.to_vec();
        }
    }

    /// Declare what this write changed; other cached guest pages stay valid. `&[]` = nothing
    /// guests can see (read markers, subscriptions…).
    pub fn write_scope(&self, tags: Vec<String>) {
        *self.write_scope.lock().unwrap() = Some(tags);
    }

    pub fn is_prefetch(&self) -> bool {
        self.headers
            .get("sec-purpose")
            .or_else(|| self.headers.get("purpose"))
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("prefetch"))
            .unwrap_or(false)
    }

    fn viewer(&self) -> Option<ViewerInfo<'_>> {
        self.user.as_ref().map(|u| ViewerInfo {
            uid: u.uid,
            username: &u.username,
            formatted: self
                .cache
                .format_name(&u.username, u.usergroup, u.displaygroup),
            avatar: avatar_url(&u.avatar, &u.email, &self.cache),
            usergroup: u.usergroup,
            unreadpms: u.unreadpms,
            unreadalerts: u.unreadalerts,
            lastvisit: u.lastvisit,
            postnum: u.postnum,
            colormode: &u.colormode,
            showsigs: u.showsigs,
            showavatars: u.showavatars,
            showquickreply: u.showquickreply,
            invisible: u.invisible,
            away: u.away,
        })
    }

    /// Render a page template with the standard context merged in.
    pub async fn render(&self, name: &str, page: Value) -> AppResult<Response> {
        self.render_status(StatusCode::OK, name, page).await
    }

    pub async fn render_status(
        &self,
        status: StatusCode,
        name: &str,
        page: Value,
    ) -> AppResult<Response> {
        let base = self.base_context().await;
        // minijinja: the LAST map wins on duplicate keys, so page values go last.
        let merged = minijinja::value::merge_maps([base, page]);
        let theme = self.theme_id();
        let t0 = std::time::Instant::now();
        let html = self.app.tpl.render(theme, name, merged)?;
        crate::debugbar::record_render(name, t0.elapsed().as_secs_f64() * 1000.0);
        let mut resp = (status, Html(html)).into_response();
        resp.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-cache"),
        );
        Ok(resp)
    }

    pub async fn base_context(&self) -> Value {
        let s = &self.cache.settings;
        let theme = self.cache.theme(self.theme_id());
        let (modq, reports, appeals) = self.mod_notice_counts().await;
        let _ = s;
        let bb = self.cache.bb.clone();
        minijinja::context! {
            _tz => self.tz.name(),
            _df => &self.datefmt,
            _tf => &self.timefmt,
            _lang => &self.lang,
            bb => bb,
            me => self.viewer(),
            perms => &self.perms,
            csrf => if self.guest_cache.is_some() { crate::pagecache::CSRF_SLOT } else { self.csrf.as_str() },
            theme => theme.map(|t| minijinja::context!{ tid => t.tid, name => &t.name, props => &t.properties.0 }),
            stylesheet_version => theme.and_then(|t| self.cache.theme_versions.get(&t.tid).cloned()),
            brand => theme.and_then(|t| self.cache.theme_look.get(&t.tid)).map(|l| l.0.as_str()).filter(|b| !b.is_empty()),
            theme_css => theme.and_then(|t| self.cache.theme_look.get(&t.tid)).map(|l| l.1).unwrap_or(false),
            is_admin => self.perms.cancp,
            is_mod => self.is_any_mod(),
            can_modcp => self.perms.canmodcp || self.is_any_mod(),
            modqueue_count => modq,
            report_count => reports,
            appeal_count => appeals,
            path => &self.path,
            current_url => if self.query.is_empty() { self.path.clone() } else { format!("{}?{}", self.path, self.query) },
            flash => &self.flash,
            languages => LANGUAGES.clone(),
            themes => self.cache.themes.iter().filter(|t| t.allowedgroups.is_empty() || t.allowedgroups.iter().any(|g| self.groups.contains(g))).map(|t| (t.tid, t.name.clone())).collect::<Vec<_>>(),
            now => now(),
            bot => self.bot,
            colormode => self.user.as_ref().map(|u| u.colormode.clone()).or_else(|| get_cookie(&self.headers, "rbb_colormode")).filter(|m| m != "auto").or_else(|| theme.and_then(|t| t.properties.0.get("colormode").and_then(|v| v.as_str()).map(|s| s.to_string()))).unwrap_or_else(|| "auto".into()),
        }
    }

    /// (unapproved content count, open reports count, pending ban appeals) shown to moderators.
    async fn mod_notice_counts(&self) -> (i64, i64, i64) {
        let uid = self.uid();
        if uid == 0 || !(self.perms.canmodcp || self.is_any_mod()) {
            return (0, 0, 0);
        }
        if let Some(v) = self.app.mod_counts.get(&uid) {
            return v;
        }
        let forums = self.cache.moderated_forums(uid, &self.groups, &self.perms);
        let res: (i64, i64) = match &forums {
            None => sqlx::query_as(
                "SELECT (SELECT COALESCE(SUM(unapprovedthreads + unapprovedposts), 0)::bigint FROM forums),
                        (SELECT COUNT(*) FROM reportedcontent WHERE reportstatus = 0)",
            )
            .fetch_one(&self.app.db)
            .await
            .unwrap_or((0, 0)),
            Some(f) => sqlx::query_as(
                "SELECT (SELECT COALESCE(SUM(unapprovedthreads + unapprovedposts), 0)::bigint FROM forums WHERE fid = ANY($1)),
                        (SELECT COUNT(*) FROM reportedcontent WHERE reportstatus = 0 AND (type <> 'post' OR id3 = ANY($1)))",
            )
            .bind(f)
            .fetch_one(&self.app.db)
            .await
            .unwrap_or((0, 0)),
        };
        let appeals: i64 = if self.perms.canbanusers {
            sqlx::query_scalar("SELECT COUNT(*) FROM ban_appeals WHERE status = 0").fetch_one(&self.app.db).await.unwrap_or(0)
        } else {
            0
        };
        let v = (res.0, res.1, appeals);
        self.app.mod_counts.insert(uid, v);
        v
    }

    /// Redirect with a flash message shown on the next page (MyBB's "redirect" page equivalent).
    pub fn redirect(&self, to: &str, msg: &str) -> Response {
        if !msg.is_empty() {
            self.add_cookie(FLASH_COOKIE, msg, Some(60), false);
        }
        Redirect::to(safe_redirect(to)).into_response()
    }

    pub async fn error_page(&self, status: StatusCode, msg: &str, login: bool) -> Response {
        let is_forumpass = msg
            .strip_prefix("__forumpass__")
            .and_then(|s| s.parse::<i32>().ok());
        if let Some(fid) = is_forumpass {
            let forum = self
                .cache
                .forum(fid)
                .map(|f| f.name.clone())
                .unwrap_or_default();
            return match self
                .render_status(StatusCode::FORBIDDEN, "forum_password.html", minijinja::context! { fid => fid, forum_name => forum, return_to => &self.path })
                .await
            {
                Ok(r) => r,
                Err(_) => (StatusCode::FORBIDDEN, "Password required").into_response(),
            };
        }
        let show_login = login || (status == StatusCode::FORBIDDEN && !self.logged_in());
        match self
            .render_status(status, "error.html", minijinja::context! { message => msg, show_login => show_login, return_to => &self.path })
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("failed rendering error page: {e:?}");
                (status, msg.to_string()).into_response()
            }
        }
    }
}

/// Only allow local redirects (prevents open redirect abuse). Browsers strip tabs and newlines
/// from URLs and treat `\` like `/`, so `/<TAB>/evil.com` or `/\evil.com` would leave the site:
/// reject control characters, whitespace and backslashes outright.
pub fn safe_redirect(to: &str) -> &str {
    let local = to.starts_with('/')
        && !to.starts_with("//")
        && !to
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '\\');
    if local { to } else { "/" }
}

pub fn avatar_url(avatar: &str, _email: &str, _cache: &Cache) -> String {
    avatar.to_string()
}

async fn load_user(app: &App, token_hash: &str) -> Option<(User, String, i64)> {
    let row = sqlx::query(
        "SELECT u.*, l.csrf AS login_csrf, l.acp_verified AS login_acp FROM logins l JOIN users u ON u.uid = l.uid
         WHERE l.token_hash = $1 AND l.expires > $2",
    )
    .bind(token_hash)
    .bind(now())
    .fetch_optional(&app.db)
    .await
    .ok()??;
    let user = User::from_row(&row).ok()?;
    Some((user, row.get("login_csrf"), row.get("login_acp")))
}

/// Middleware building the request context, enforcing board-closed / rate limits, rendering
/// themed error pages and recording "who's online" activity.
pub async fn context_middleware(
    axum::extract::State(app): axum::extract::State<App>,
    mut req: Request,
    next: Next,
) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let headers = req.headers().clone();
    let ip = client_ip(&headers, peer, app.cfg.trust_proxy);
    let cache = app.cache();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let method = req.method().to_string();
    let is_api = path.starts_with("/api/");

    // Global per-IP rate limit.
    let rl = cache.settings.int("ratelimit_requests") as u32;
    if !app.rate_check(&format!("req:{ip}"), rl, 60) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "30")],
            "Too many requests",
        )
            .into_response();
    }

    let useragent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect::<String>();
    let bot = util::detect_bot(&useragent);

    let mut user = None;
    let mut csrf = String::new();
    let mut token_hash = None;
    let mut acp_verified = 0;
    let mut new_cookies = Vec::new();
    // API clients may authenticate with a bearer token (same login tokens).
    let bearer_tok = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string());
    let mut bearer = false;
    if let Some(tok) = bearer_tok
        .clone()
        .or_else(|| get_cookie(&headers, AUTH_COOKIE))
    {
        let h = util::sha256_hex(&tok);
        if let Some((u, c, acp)) = load_user(&app, &h).await {
            user = Some(u);
            csrf = c;
            acp_verified = acp;
            token_hash = Some(h);
            bearer = bearer_tok.is_some();
        }
    }
    let had_sid = get_cookie(&headers, SID_COOKIE)
        .filter(|s| s.len() == 32 && s.chars().all(|c| c.is_ascii_alphanumeric()));
    let sid = match had_sid.clone() {
        Some(s) => s,
        None => {
            let s = util::random_token(32);
            if bot.is_none() {
                new_cookies.push(format!(
                    "{SID_COOKIE}={s}; Path=/; SameSite=Lax; HttpOnly; Max-Age=31536000{}",
                    if app.cfg.secure_cookies {
                        "; Secure"
                    } else {
                        ""
                    }
                ));
            }
            s
        }
    };
    if user.is_none() {
        csrf = util::hmac_hex(&app.cfg.secret, &format!("csrf:{sid}"))[..32].to_string();
    }

    let (groups, perms) = match &user {
        Some(u) => {
            let g = u.all_groups();
            let p = cache.group_perms(&g);
            (g, p)
        }
        None => (vec![1], cache.group_perms(&[1])),
    };

    let s = &cache.settings;
    let tzname = user
        .as_ref()
        .map(|u| u.timezone.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| s.get("timezone").to_string());
    let lang = user
        .as_ref()
        .map(|u| u.language.clone())
        .filter(|l| !l.is_empty())
        .or_else(|| get_cookie(&headers, "rbb_lang"))
        .or_else(|| {
            headers
                .get(header::ACCEPT_LANGUAGE)
                .and_then(|v| v.to_str().ok())
                .and_then(crate::i18n::negotiate)
        })
        .unwrap_or_default();
    let mut theme = cache.default_theme();
    let chosen = user
        .as_ref()
        .map(|u| u.style)
        .filter(|s| *s > 0)
        .or_else(|| get_cookie(&headers, "rbb_theme").and_then(|t| t.parse().ok()));
    if let Some(t) = chosen.and_then(|t| cache.theme(t)) {
        if t.allowedgroups.is_empty() || t.allowedgroups.iter().any(|g| groups.contains(g)) {
            theme = t.tid;
        }
    }
    let flash = get_cookie(&headers, FLASH_COOKIE)
        .filter(|f| !f.is_empty())
        .map(|f| f.chars().take(300).collect());
    if flash.is_some() {
        new_cookies.push(format!("{FLASH_COOKIE}=; Path=/; Max-Age=0"));
    }

    let datefmt = user
        .as_ref()
        .map(|u| u.dateformat.clone())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| s.get("dateformat").to_string());
    let timefmt = user
        .as_ref()
        .map(|u| u.timeformat.clone())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| s.get("timeformat").to_string());

    // Guests without per-visitor state can share cached pages.
    let guest_cache = (method == "GET"
        && user.is_none()
        && !is_api
        && flash.is_none()
        && app.page_cache.enabled()
        && !path.starts_with("/static")
        && !path.starts_with("/live")
        && !path.starts_with("/pgp/")
        && !headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|c| c.contains("forumpass_")))
    .then(|| {
        let colormode = get_cookie(&headers, "rbb_colormode").unwrap_or_default();
        crate::pagecache::key(&path, &query, theme, &lang, &colormode, bot.is_some())
    });

    let ctx = Ctx(Arc::new(CtxInner {
        app: app.clone(),
        cache: cache.clone(),
        user,
        groups,
        perms,
        ip: ip.clone(),
        sid: sid.clone(),
        csrf,
        token_hash,
        acp_verified,
        theme: AtomicI32::new(theme),
        tz: util::parse_tz(&tzname),
        lang,
        datefmt,
        timefmt,
        path: path.clone(),
        query: query.clone(),
        method: method.clone(),
        useragent: useragent.clone(),
        bot,
        is_api,
        bearer,
        flash,
        cookies: Mutex::new(new_cookies),
        location: (AtomicI32::new(0), AtomicI32::new(0)),
        headers,
        guest_cache,
        guest_cacheable: std::sync::atomic::AtomicBool::new(false),
        guest_cache_tags: Mutex::new(vec![]),
        write_scope: Mutex::new(None),
        forums_read: tokio::sync::OnceCell::new(),
    }));
    req.extensions_mut().insert(ctx.clone());
    let cache_epoch = app.page_cache.epoch();
    let profile = (ctx.perms.cancp
        && !is_api
        && method == "GET"
        && cache.settings.bool("debugpanel")
        && !path.starts_with("/live")
        && !path.starts_with("/pgp/"))
    .then(crate::debugbar::new_profile);

    // Board closed: only users allowed to view a closed board (and the login page) get through.
    let closed = cache.settings.bool("boardclosed")
        && !ctx.perms.canviewboardclosed
        && !path.starts_with("/member/login")
        && !path.starts_with("/captcha");
    let mut resp = if closed {
        let msg = cache.settings.get("boardclosed_reason").to_string();
        match ctx
            .render_status(
                StatusCode::SERVICE_UNAVAILABLE,
                "board_closed.html",
                minijinja::context! { reason => msg },
            )
            .await
        {
            Ok(r) => r,
            Err(e) => e.into_response(),
        }
    } else if ctx
        .user
        .as_ref()
        .map(|u| {
            cache
                .group(u.usergroup)
                .map(|g| g.isbannedgroup)
                .unwrap_or(false)
        })
        .unwrap_or(false)
        && !path.starts_with("/member/logout")
        && !path.starts_with("/member/appeal")
        && !path.starts_with("/static")
    {
        banned_page(&ctx).await
    } else if let Some(hit) = ctx.guest_cache.as_deref().and_then(|k| app.page_cache.get(k)) {
        // Served from the guest page cache: replay the handler's side effects that matter.
        ctx.set_location(hit.fid, hit.tid);
        if hit.tid > 0 && !ctx.is_prefetch() {
            *app.thread_views.entry(hit.tid).or_insert(0) += 1;
        }
        let mut r = (StatusCode::OK, crate::pagecache::personalize(&hit.html, &ctx.csrf)).into_response();
        let h = r.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
        h.insert("x-rbb-cache", HeaderValue::from_static("hit"));
        r
    } else if let Some(p) = profile.clone() {
        crate::debugbar::PROFILE.scope(p, next.run(req)).await
    } else {
        next.run(req).await
    };

    // Themed error pages.
    if let Some(ErrorMarker(msg, status, login)) = resp.extensions().get::<ErrorMarker>().cloned() {
        if !is_api && !path.starts_with("/pgp/") {
            resp = ctx.error_page(status, &msg, login).await;
        } else {
            resp = (status, axum::Json(serde_json::json!({"error": msg}))).into_response();
        }
    }

    if let Some(p) = &profile {
        resp = inject_debug(&app, resp, p, &method, &path, &query).await;
    }

    // Guest HTML: remember cacheable pages, and put this visitor's CSRF token in.
    if let Some(key) = &ctx.guest_cache {
        let is_html = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/html"));
        if is_html && resp.headers().get("x-rbb-cache").is_none() {
            let (mut parts, body) = resp.into_parts();
            let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
            if parts.status == StatusCode::OK && ctx.guest_cacheable.load(Ordering::Relaxed) {
                app.page_cache.put(
                    key.clone(),
                    crate::pagecache::Entry {
                        html: bytes.clone(),
                        tags: app.page_cache.snapshot(&ctx.guest_cache_tags.lock().unwrap()),
                        fid: ctx.location.0.load(Ordering::Relaxed),
                        tid: ctx.location.1.load(Ordering::Relaxed),
                    },
                    cache_epoch,
                );
                parts.headers.insert("x-rbb-cache", HeaderValue::from_static("miss"));
            }
            parts.headers.remove(header::CONTENT_LENGTH);
            resp = Response::from_parts(parts, axum::body::Body::from(crate::pagecache::personalize(&bytes, &ctx.csrf)));
        }
    }

    // Any successful write can change what guests see: just what it touched if the handler
    // said so, otherwise everything.
    if method != "GET" && method != "HEAD" && resp.status().as_u16() < 400 && !crate::pagecache::write_is_private(&path) {
        match ctx.write_scope.lock().unwrap().take() {
            Some(tags) => app.content_changed_tags(tags),
            None => app.content_changed(),
        }
    }

    for c in ctx.cookies.lock().unwrap().drain(..) {
        if let Ok(v) = HeaderValue::from_str(&c) {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
    }

    // Record activity for Who's Online (batched; flushed every few seconds).
    if method == "GET" && !is_api && resp.status().is_success() && !path.starts_with("/live") && !path.starts_with("/pgp/") && !ctx.is_prefetch() {
        let location = if query.is_empty() {
            path
        } else {
            format!("{path}?{query}")
        };
        // Clients that don't keep cookies (bots, scripts) are tracked per IP so they can't flood
        // the sessions table with one row per request.
        let key = if let Some(b) = bot {
            format!("bot={b}")
        } else if had_sid.is_none() && ctx.uid() == 0 {
            format!("ip={}", util::sha256_hex(&ip)[..24].to_string())
        } else {
            sid
        };
        app.activity.insert(
            key,
            Activity {
                uid: ctx.uid(),
                ip,
                time: now(),
                location: location.chars().take(250).collect(),
                useragent,
                anonymous: ctx.user.as_ref().map(|u| u.invisible).unwrap_or(false),
                location1: ctx.location.0.load(Ordering::Relaxed),
                location2: ctx.location.1.load(Ordering::Relaxed),
                bot: bot.unwrap_or("").to_string(),
            },
        );
    }
    resp
}

async fn banned_page(ctx: &Ctx) -> Response {
    let ban: Option<(String, String, i64)> =
        sqlx::query_as("SELECT reason, bantime, lifted FROM banned WHERE uid = $1")
            .bind(ctx.uid())
            .fetch_optional(&ctx.app.db)
            .await
            .ok()
            .flatten();
    let (reason, lifted) = ban.map(|b| (b.0, b.2)).unwrap_or_default();
    let appeal = crate::routes::appeals::banned_page_context(ctx).await.unwrap_or_default();
    match ctx
        .render_status(
            StatusCode::FORBIDDEN,
            "banned.html",
            minijinja::context! { reason => reason, lifted => lifted, appeal => appeal },
        )
        .await
    {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

/// Language packs never change while running; build their template value once.
static LANGUAGES: std::sync::LazyLock<Value> =
    std::sync::LazyLock::new(|| Value::from_serialize(crate::i18n::available()));

/// Form extractor that also verifies the CSRF token (`my_post_key` field or X-CSRF-Token header).
pub struct CsrfForm<T>(pub T);

#[derive(Deserialize)]
struct KeyOnly {
    #[serde(default)]
    my_post_key: String,
}

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for CsrfForm<T> {
    type Rejection = AppError;
    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let ctx = req
            .extensions()
            .get::<Ctx>()
            .cloned()
            .ok_or(AppError::Csrf)?;
        let bytes = axum::body::Bytes::from_request(req, state)
            .await
            .map_err(|_| AppError::user("Request body too large or invalid."))?;
        let key: KeyOnly = serde_html_form_parse(&bytes).unwrap_or(KeyOnly {
            my_post_key: String::new(),
        });
        ctx.check_csrf(&key.my_post_key)?;
        let v: T = serde_html_form_parse(&bytes)
            .map_err(|e| AppError::user(format!("Invalid form submission: {e}")))?;
        Ok(CsrfForm(v))
    }
}

/// Parse urlencoded bodies, supporting repeated keys (`a=1&a=2`) for Vec fields and `a[]=` style.
pub fn serde_html_form_parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    // Collect pairs; group repeated keys into JSON arrays so Vec<T> fields work.
    let mut map: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    let mut multi: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (k, v) in url::form_urlencoded::parse(bytes) {
        let (key, is_arr) = match k.strip_suffix("[]") {
            Some(base) => (base.to_string(), true),
            None => (k.to_string(), false),
        };
        let val = serde_json::Value::String(v.into_owned());
        match map.get_mut(&key) {
            Some(serde_json::Value::Array(a)) => a.push(val),
            Some(existing) => {
                let prev = existing.take();
                *existing = serde_json::Value::Array(vec![prev, val]);
                multi.insert(key);
            }
            None => {
                if is_arr {
                    map.insert(key, serde_json::Value::Array(vec![val]));
                } else {
                    map.insert(key, val);
                }
            }
        }
    }
    let _ = multi;
    serde_json::from_value(serde_json::Value::Object(map)).map_err(|e| e.to_string())
}

/// Lenient deserializers for form fields ("1"/"on"/"yes" booleans, numeric strings, single-or-many lists).
pub mod de {
    use serde::{Deserialize, Deserializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        Many(Vec<serde_json::Value>),
        One(serde_json::Value),
    }

    fn val_to_string(v: &serde_json::Value) -> String {
        match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => String::new(),
            o => o.to_string(),
        }
    }

    pub fn bool<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
        let v = OneOrMany::deserialize(d)?;
        let s = match v {
            OneOrMany::Many(a) => a.last().map(val_to_string).unwrap_or_default(),
            OneOrMany::One(v) => val_to_string(&v),
        };
        Ok(matches!(s.as_str(), "1" | "on" | "yes" | "true"))
    }

    pub fn i64<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
        let v = OneOrMany::deserialize(d)?;
        let s = match v {
            OneOrMany::Many(a) => a.last().map(val_to_string).unwrap_or_default(),
            OneOrMany::One(v) => val_to_string(&v),
        };
        Ok(s.trim().parse().unwrap_or(0))
    }

    pub fn i32<'de, D: Deserializer<'de>>(d: D) -> Result<i32, D::Error> {
        Ok(i64(d)?.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
    }

    pub fn string<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
        let v = OneOrMany::deserialize(d)?;
        Ok(match v {
            OneOrMany::Many(a) => a.last().map(val_to_string).unwrap_or_default(),
            OneOrMany::One(v) => val_to_string(&v),
        })
    }

    pub fn vec_i32<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<i32>, D::Error> {
        let v = OneOrMany::deserialize(d)?;
        let items = match v {
            OneOrMany::Many(a) => a,
            OneOrMany::One(v) => vec![v],
        };
        Ok(items
            .iter()
            .flat_map(|v| {
                val_to_string(v)
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect::<Vec<i32>>()
            })
            .collect())
    }

    pub fn vec_string<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
        let v = OneOrMany::deserialize(d)?;
        Ok(match v {
            OneOrMany::Many(a) => a.iter().map(val_to_string).collect(),
            OneOrMany::One(v) => vec![val_to_string(&v)],
        })
    }
}

/// Replace the layout's debug placeholder with the profiler panel (admin HTML pages only).
async fn inject_debug(app: &App, resp: Response, p: &crate::debugbar::Handle, method: &str, path: &str, query: &str) -> Response {
    let is_html = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("text/html"))
        .unwrap_or(false);
    if !is_html {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    let bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return Response::from_parts(parts, axum::body::Body::empty()),
    };
    let html = String::from_utf8_lossy(&bytes);
    if !html.contains(crate::debugbar::PLACEHOLDER) {
        return Response::from_parts(parts, axum::body::Body::from(bytes));
    }
    let extra = crate::debugbar::Extra {
        route: format!("{method} {path}{}{query}", if query.is_empty() { "" } else { "?" }),
        status: parts.status.as_u16(),
        bytes: bytes.len(),
        pool_size: app.db.size(),
        pool_idle: app.db.num_idle(),
        node: app.node_id.clone(),
        uptime: crate::routes::member::format_duration(util::now() - app.started),
        cache_hint: format!("parser rev {}", app.cache().parser_rev),
    };
    let panel = crate::debugbar::render(&p.lock().unwrap(), &extra);
    let out = html.replacen(crate::debugbar::PLACEHOLDER, &panel, 1);
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, axum::body::Body::from(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_redirect_blocks_offsite() {
        for bad in [
            "//evil.example",
            "/\t/evil.example",
            "/\n/evil.example",
            "/\r/evil.example",
            "/\\evil.example",
            "/ /evil.example",
            "https://evil.example",
            "javascript:alert(1)",
            "",
        ] {
            assert_eq!(safe_redirect(bad), "/", "{bad:?} must not redirect off-site");
        }
        assert_eq!(safe_redirect("/thread/5?page=2#pid9"), "/thread/5?page=2#pid9");
    }

    #[test]
    fn client_ip_uses_proxy_appended_address() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_static("6.6.6.6, 203.0.113.9"));
        let peer: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        assert_eq!(client_ip(&h, Some(peer), true), "203.0.113.9");
        assert_eq!(client_ip(&h, Some(peer), false), "10.0.0.1");
    }
}
