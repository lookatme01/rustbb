//! Image proxy for pictures linked from other sites (`[img]`, remote avatars).
//!
//! Without a proxy, every reader's browser fetches remote images straight from wherever they are
//! hosted, which tells that host the reader's IP address and that they are reading the board (and
//! breaks on HTTPS boards when the image is plain HTTP). With a proxy, image URLs are rewritten to
//! `<base>/<hmac>/<hex url>` — the URL format of camo and go-camo — so readers only ever talk to
//! the proxy. The HMAC (SHA-1, keyed) stops the proxy from fetching URLs the board didn't sign.
//!
//! Two kinds of proxy:
//!
//! * **External** (recommended): camo or go-camo on a separate host. The board's own server never
//!   fetches anything, so a board hidden behind a DDoS-protection CDN keeps its origin address
//!   secret.
//! * **Built-in** (`/imgproxy/…`): this server fetches the images. Simple, but anyone can post an
//!   image hosted on a server they control and read this server's real IP address from its logs,
//!   which defeats a CDN's DDoS protection. Fetching is SSRF-hardened: only public addresses
//!   (checked after DNS resolution and on every redirect), only image formats recognised by their
//!   magic bytes (never SVG), size and time limits.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use hmac::{Hmac, Mac};
use std::borrow::Cow;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use crate::settings::Settings;

type HmacSha1 = Hmac<sha1::Sha1>;

/// Path prefix of the built-in proxy.
pub const BUILTIN_PREFIX: &str = "/imgproxy";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Builtin,
    External,
}

impl Mode {
    pub fn from_settings(s: &Settings) -> Mode {
        match s.get("imageproxy") {
            "builtin" => Mode::Builtin,
            "external" => Mode::External,
            _ => Mode::Off,
        }
    }
}

/// A configured proxy: how to rewrite image URLs.
#[derive(Debug)]
pub struct ImageProxy {
    pub mode: Mode,
    base: String,
    key: Vec<u8>,
    /// The board's own origin and the proxy's; images there are never proxied.
    own: Vec<url::Origin>,
}

impl ImageProxy {
    /// The proxy the settings describe, or `None` when it is off or incomplete.
    pub fn from_settings(s: &Settings) -> Option<ImageProxy> {
        let (mode, base, key) = match Mode::from_settings(s) {
            Mode::Off => return None,
            Mode::Builtin => (
                Mode::Builtin,
                BUILTIN_PREFIX.to_string(),
                s.get("imageproxy_builtin_key"),
            ),
            Mode::External => {
                let base = s.get("imageproxy_url").trim().trim_end_matches('/');
                if !valid_base(base) {
                    return None;
                }
                (Mode::External, base.to_string(), s.get("imageproxy_key"))
            }
        };
        if key.is_empty() {
            return None;
        }
        let own = [s.get("bburl"), base.as_str()]
            .iter()
            .filter_map(|u| url::Url::parse(u).ok())
            .map(|u| u.origin())
            .filter(url::Origin::is_tuple)
            .collect();
        Some(ImageProxy {
            mode,
            base,
            key: key.as_bytes().to_vec(),
            own,
        })
    }

    fn sign(&self, url: &str) -> String {
        let mut mac = HmacSha1::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(url.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    fn verify(&self, url: &str, sig_hex: &str) -> bool {
        let Ok(sig) = hex::decode(sig_hex) else {
            return false;
        };
        let mut mac = HmacSha1::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(url.as_bytes());
        mac.verify_slice(&sig).is_ok()
    }

    /// The proxied URL for `url`, or `None` when it should be left alone (not http(s), or
    /// already on this board or the proxy).
    pub fn url_for(&self, url: &str) -> Option<String> {
        let parsed = url::Url::parse(url).ok()?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return None;
        }
        parsed.host_str()?;
        if self.own.contains(&parsed.origin()) {
            return None;
        }
        Some(format!(
            "{}/{}/{}",
            self.base,
            self.sign(url),
            hex::encode(url.as_bytes())
        ))
    }

    /// Rewrite the `src` of every `<img>` in parsed post HTML.
    pub fn rewrite_html<'a>(&self, html: &'a str) -> Cow<'a, str> {
        static IMG_SRC: LazyLock<regex::Regex> =
            LazyLock::new(|| regex::Regex::new(r#"(?i)(<img\b[^>]*?\ssrc=")([^"]*)(")"#).unwrap());
        if !html.contains("<img") && !html.contains("<IMG") {
            return Cow::Borrowed(html);
        }
        IMG_SRC.replace_all(html, |c: &regex::Captures| {
            let src = unescape_attr(&c[2]);
            match self.url_for(&src) {
                Some(p) => format!("{}{}{}", &c[1], crate::util::escape_html(&p), &c[3]),
                None => c[0].to_string(),
            }
        })
    }
}

fn valid_base(base: &str) -> bool {
    url::Url::parse(base).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https")
            && u.host_str().is_some()
            && u.query().is_none()
            && !base.contains(['"', '<', '>', '\''])
    })
}

fn unescape_attr(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Template object carrying the board's proxy into templates (`_imgproxy`), for the `img` filter.
/// Deliberately prints as nothing, so the key can't end up in a page.
#[derive(Debug)]
pub struct TemplateProxy(pub Option<Arc<ImageProxy>>);

impl minijinja::value::Object for TemplateProxy {
    fn render(self: &Arc<Self>, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}

/// `{{ url|img }}`: the proxied URL of a remote image (avatars), or the URL unchanged.
pub fn img_filter(state: &minijinja::State, url: String) -> String {
    let Some(v) = state.lookup("_imgproxy") else {
        return url;
    };
    match v.downcast_object_ref::<TemplateProxy>() {
        Some(TemplateProxy(Some(p))) => p.url_for(&url).unwrap_or(url),
        _ => url,
    }
}

// ---------------------------------------------------------------------------------------------
// The built-in proxy.

/// Fetches running at once.
static FETCHES: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(16));
/// Recently fetched images (URL → bytes, type), up to 64 MB.
static RECENT: LazyLock<moka::sync::Cache<String, (Bytes, &'static str)>> = LazyLock::new(|| {
    moka::sync::Cache::builder()
        .weigher(|k: &String, v: &(Bytes, &'static str)| {
            (k.len() + v.0.len()).try_into().unwrap_or(u32::MAX)
        })
        .max_capacity(64 * 1024 * 1024)
        .time_to_live(Duration::from_secs(3600))
        .build()
});
const MAX_REDIRECTS: usize = 3;
/// Integration tests serve images from 127.0.0.1 on a random port. Never set outside tests.
#[doc(hidden)]
pub static ALLOW_LOOPBACK_FOR_TESTS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn loopback_allowed() -> bool {
    ALLOW_LOOPBACK_FOR_TESTS.load(std::sync::atomic::Ordering::Relaxed)
}
const PORTS: &[u16] = &[80, 443, 8080, 8443];

/// Whether this server may connect to `ip`: public unicast addresses only.
pub fn is_public(ip: IpAddr) -> bool {
    if ip.is_loopback() && loopback_allowed() {
        return true;
    }
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let s = v6.segments();
            // 6to4 and NAT64 embed an IPv4 address; Teredo hides its target.
            if s[0] == 0x2002 {
                return is_public_v4(Ipv4Addr::new(
                    (s[1] >> 8) as u8,
                    s[1] as u8,
                    (s[2] >> 8) as u8,
                    s[2] as u8,
                ));
            }
            if s[0] == 0x64 && s[1] == 0xff9b {
                return is_public_v4(Ipv4Addr::new(
                    (s[6] >> 8) as u8,
                    s[6] as u8,
                    (s[7] >> 8) as u8,
                    s[7] as u8,
                ));
            }
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link-local
                || (s[0] & 0xffc0) == 0xfec0 // site-local (deprecated)
                || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
                || (s[0] == 0x2001 && s[1] == 0) // Teredo
                || v6.to_ipv4().is_some()) // IPv4-compatible (deprecated)
        }
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || o[0] == 0
        || o[0] >= 240
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // carrier-grade NAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // IETF protocol assignments
        || (o[0] == 198 && (o[1] & 0xfe) == 18)) // benchmarking
}

/// Whether a URL may be fetched before its host is resolved (scheme, port, literal addresses).
fn allowed_target(u: &url::Url) -> bool {
    if !matches!(u.scheme(), "http" | "https") || !u.username().is_empty() {
        return false;
    }
    if !u
        .port_or_known_default()
        .is_some_and(|p| PORTS.contains(&p) || loopback_allowed())
    {
        return false;
    }
    match u.host() {
        Some(url::Host::Ipv4(ip)) => is_public(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => is_public(IpAddr::V6(ip)),
        Some(url::Host::Domain(d)) => !d.is_empty(),
        None => false,
    }
}

/// Resolves hostnames and drops every non-public address, so neither a hostname nor a redirect
/// can point the proxy at the internal network. The connection uses the checked addresses
/// directly, so DNS rebinding between the check and the connect isn't possible.
struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| is_public(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err("the image host has no public address".into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .dns_resolver(Arc::new(PublicOnly))
        .redirect(reqwest::redirect::Policy::custom(|a| {
            if a.previous().len() >= MAX_REDIRECTS {
                a.error("too many redirects")
            } else if !allowed_target(a.url()) {
                a.error("redirect to a forbidden address")
            } else {
                a.follow()
            }
        }))
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("rbb-image-proxy/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("image proxy HTTP client")
});

/// The image type from its first bytes. SVG and anything else unrecognised is refused: an SVG
/// served from the board's own origin could run script if opened directly.
pub fn sniff(b: &[u8]) -> Option<&'static str> {
    Some(if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if b.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        "image/gif"
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        "image/webp"
    } else if b.len() >= 12 && &b[4..8] == b"ftyp" && matches!(&b[8..12], b"avif" | b"avis") {
        "image/avif"
    } else if b.starts_with(b"BM") {
        "image/bmp"
    } else if b.starts_with(&[0, 0, 1, 0]) {
        "image/x-icon"
    } else {
        return None;
    })
}

#[derive(Debug)]
enum FetchError {
    Refused(&'static str),
    Upstream(String),
}

async fn fetch(url: &url::Url, max_bytes: usize) -> Result<(Bytes, &'static str), FetchError> {
    use futures::StreamExt;
    let resp = CLIENT
        .get(url.clone())
        .header(
            header::ACCEPT,
            "image/avif,image/webp,image/png,image/jpeg,image/gif,image/*;q=0.8",
        )
        .send()
        .await
        .map_err(|e| FetchError::Upstream(format!("{e:#}")))?;
    if !resp.status().is_success() {
        return Err(FetchError::Upstream(format!(
            "upstream answered {}",
            resp.status()
        )));
    }
    if resp
        .content_length()
        .is_some_and(|n| n as usize > max_bytes)
    {
        return Err(FetchError::Refused("too large"));
    }
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| FetchError::Upstream(e.to_string()))?;
        if body.len() + chunk.len() > max_bytes {
            return Err(FetchError::Refused("too large"));
        }
        body.extend_from_slice(&chunk);
    }
    let kind = sniff(&body).ok_or(FetchError::Refused("not an image"))?;
    Ok((Bytes::from(body), kind))
}

fn image_response(bytes: Bytes, kind: &'static str) -> Response {
    let mut r = Response::new(Body::from(bytes));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(kind));
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("inline"),
    );
    r
}

fn refuse(status: StatusCode, result: &'static str) -> Response {
    crate::infra::metrics::counter_with("rbb_imageproxy_requests_total", &[("result", result)], 1);
    let mut r = status.into_response();
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    r
}

/// `GET /imgproxy/{sig}/{hex url}`.
pub async fn handler(
    State(app): State<crate::app::App>,
    Path((sig, hex_url)): Path<(String, String)>,
) -> Response {
    let cache = app.cache();
    let Some(proxy) = cache
        .image_proxy
        .as_ref()
        .filter(|p| p.mode == Mode::Builtin)
    else {
        return refuse(StatusCode::NOT_FOUND, "disabled");
    };
    let Some(raw) = hex::decode(&hex_url)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
    else {
        return refuse(StatusCode::NOT_FOUND, "bad_url");
    };
    if !proxy.verify(&raw, &sig) {
        return refuse(StatusCode::FORBIDDEN, "bad_signature");
    }
    let Some(url) = url::Url::parse(&raw).ok().filter(allowed_target) else {
        return refuse(StatusCode::FORBIDDEN, "forbidden_target");
    };
    if let Some((bytes, kind)) = RECENT.get(&raw) {
        crate::infra::metrics::counter_with(
            "rbb_imageproxy_requests_total",
            &[("result", "cached")],
            1,
        );
        return image_response(bytes, kind);
    }
    let Ok(_permit) = tokio::time::timeout(Duration::from_secs(5), FETCHES.acquire()).await else {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "busy");
    };
    let max_bytes = (cache.settings.int("imageproxy_maxsize").clamp(16, 51_200) as usize) * 1024;
    let t0 = std::time::Instant::now();
    let res = fetch(&url, max_bytes).await;
    crate::infra::metrics::observe(
        "rbb_imageproxy_fetch_seconds",
        &[],
        t0.elapsed().as_secs_f64(),
    );
    match res {
        Ok((bytes, kind)) => {
            crate::infra::metrics::counter_with(
                "rbb_imageproxy_requests_total",
                &[("result", "fetched")],
                1,
            );
            RECENT.insert(raw, (bytes.clone(), kind));
            image_response(bytes, kind)
        }
        Err(FetchError::Refused(why)) => {
            tracing::debug!("image proxy refused {url}: {why}");
            refuse(StatusCode::UNPROCESSABLE_ENTITY, "refused")
        }
        Err(FetchError::Upstream(e)) => {
            tracing::debug!("image proxy fetch of {url} failed: {e}");
            refuse(StatusCode::BAD_GATEWAY, "upstream_error")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(pairs: &[(&str, &str)]) -> Settings {
        Settings::from_rows(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    fn external() -> ImageProxy {
        ImageProxy::from_settings(&settings(&[
            ("bburl", "https://forum.example.org"),
            ("imageproxy", "external"),
            ("imageproxy_url", "https://camo.example.net/"),
            ("imageproxy_key", "0x24FEEDFACEDEADBEEFCAFE"),
        ]))
        .unwrap()
    }

    #[test]
    fn signs_urls_like_camo() {
        // Reference value from camo's test suite format: HMAC-SHA1 over the URL, both hex.
        let p = external();
        let u = p.url_for("http://example.com/a.png").unwrap();
        let mut mac = HmacSha1::new_from_slice(b"0x24FEEDFACEDEADBEEFCAFE").unwrap();
        mac.update(b"http://example.com/a.png");
        let sig = hex::encode(mac.finalize().into_bytes());
        assert_eq!(
            u,
            format!(
                "https://camo.example.net/{sig}/{}",
                hex::encode("http://example.com/a.png")
            )
        );
        assert!(p.verify("http://example.com/a.png", &sig));
        assert!(!p.verify("http://example.com/b.png", &sig));
    }

    #[test]
    fn leaves_local_and_non_http_images_alone() {
        let p = external();
        assert_eq!(p.url_for("https://forum.example.org/uploads/x.png"), None);
        assert_eq!(p.url_for("https://camo.example.net/x/y"), None);
        assert_eq!(p.url_for("/static/images/star.svg"), None);
        assert_eq!(p.url_for("data:image/png;base64,AAAA"), None);
    }

    #[test]
    fn rewrites_img_tags_and_unescapes_their_urls() {
        let p = external();
        let html = r#"<p>hi <img src="http://example.com/a.png?x=1&amp;y=2" loading="lazy" alt="[Image: a]" /> <img class="smilie" src="/static/smilies/smile.png" /></p>"#;
        let out = p.rewrite_html(html);
        let want = p.url_for("http://example.com/a.png?x=1&y=2").unwrap();
        assert!(out.contains(&format!(r#"src="{want}""#)), "{out}");
        assert!(out.contains(r#"src="/static/smilies/smile.png""#), "{out}");
        assert!(matches!(
            p.rewrite_html("<p>no images</p>"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn off_or_incomplete_settings_mean_no_proxy() {
        assert!(ImageProxy::from_settings(&settings(&[("imageproxy", "off")])).is_none());
        assert!(
            ImageProxy::from_settings(&settings(&[
                ("imageproxy", "external"),
                ("imageproxy_key", "k")
            ]))
            .is_none()
        );
        assert!(
            ImageProxy::from_settings(&settings(&[
                ("imageproxy", "external"),
                ("imageproxy_url", "javascript:alert(1)"),
                ("imageproxy_key", "k")
            ]))
            .is_none()
        );
        assert!(ImageProxy::from_settings(&settings(&[("imageproxy", "builtin")])).is_none());
        let b = ImageProxy::from_settings(&settings(&[
            ("imageproxy", "builtin"),
            ("imageproxy_builtin_key", "secret"),
        ]))
        .unwrap();
        assert!(
            b.url_for("https://example.com/x.gif")
                .unwrap()
                .starts_with("/imgproxy/")
        );
    }

    #[test]
    fn only_public_addresses_are_allowed() {
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "198.18.0.1",
            "192.0.0.8",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
            "2002:a00:1::",
            "2001:db8::1",
            "2001::1",
        ] {
            assert!(!is_public(bad.parse().unwrap()), "{bad} should be refused");
        }
        for good in [
            "1.1.1.1",
            "93.184.216.34",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(is_public(good.parse().unwrap()), "{good} should be allowed");
        }
    }

    #[test]
    fn targets_are_checked_before_resolution() {
        let ok = |s: &str| allowed_target(&url::Url::parse(s).unwrap());
        assert!(ok("https://example.com/a.png"));
        assert!(ok("http://example.com:8080/a.png"));
        assert!(!ok("http://127.0.0.1/a.png"));
        assert!(!ok("http://[::1]/a.png"));
        assert!(!ok("http://example.com:22/a.png"));
        assert!(!ok("ftp://example.com/a.png"));
        assert!(!ok("file:///etc/passwd"));
        assert!(!ok("http://user:pw@example.com/a.png"));
    }

    #[test]
    fn sniffs_images_and_refuses_svg() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(
            sniff(b"<svg xmlns='http://www.w3.org/2000/svg'><script/></svg>"),
            None
        );
        assert_eq!(sniff(b"<html>"), None);
    }
}
