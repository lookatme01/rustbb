//! Small shared helpers: time, randomness, escaping, slugs, pagination.

use chrono::{DateTime, Datelike, TimeZone, Utc};
use chrono_tz::Tz;
use rand::Rng;
use serde::Serialize;
use sha2::{Digest, Sha256};

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn random_token(len: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

pub fn hmac_hex(secret: &str, msg: &str) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(msg.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time string comparison.
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// URL slug for SEO-friendly links: "Hello, World!" -> "hello-world".
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut dash = false;
    for c in s.chars().flat_map(|c| c.to_lowercase()) {
        if c.is_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.len() > 60 {
            break;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Parse the leading integer of a path segment like `12-some-slug`.
pub fn leading_id(seg: &str) -> Option<i32> {
    let digits: String = seg.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

pub fn parse_tz(name: &str) -> Tz {
    name.parse::<Tz>().unwrap_or(chrono_tz::UTC)
}

pub fn to_local(ts: i64, tz: Tz) -> DateTime<Tz> {
    tz.from_utc_datetime(
        &DateTime::<Utc>::from_timestamp(ts, 0)
            .unwrap_or_default()
            .naive_utc(),
    )
}

/// Format a timestamp. `style`: "relative" (Today/Yesterday/x minutes ago), "date", "time", "datetime".
pub fn format_date(ts: i64, tz: Tz, datefmt: &str, timefmt: &str, style: &str) -> String {
    if ts <= 0 {
        return "Never".into();
    }
    let dt = to_local(ts, tz);
    match style {
        "date" => dt.format(datefmt).to_string(),
        "time" => dt.format(timefmt).to_string(),
        "datetime" => format!("{}, {}", dt.format(datefmt), dt.format(timefmt)),
        "iso" => dt.to_rfc3339(),
        _ => {
            let n = now();
            let diff = n - ts;
            if (0..60).contains(&diff) {
                return if diff <= 1 {
                    "1 second ago".into()
                } else {
                    format!("{diff} seconds ago")
                };
            }
            if (60..3600).contains(&diff) {
                let m = diff / 60;
                return if m == 1 {
                    "1 minute ago".into()
                } else {
                    format!("{m} minutes ago")
                };
            }
            let today = to_local(n, tz).date_naive();
            let d = dt.date_naive();
            if d == today {
                if diff < 3 * 3600 && diff >= 0 {
                    let h = diff / 3600;
                    return if h == 1 {
                        "1 hour ago".into()
                    } else {
                        format!("{h} hours ago")
                    };
                }
                format!("Today, {}", dt.format(timefmt))
            } else if today.pred_opt() == Some(d) {
                format!("Yesterday, {}", dt.format(timefmt))
            } else {
                format!("{}, {}", dt.format(datefmt), dt.format(timefmt))
            }
        }
    }
}

pub fn age_from_birthday(b: &str, tz: Tz) -> Option<i32> {
    let parts: Vec<i32> = b.split('-').filter_map(|p| p.parse().ok()).collect();
    if parts.len() != 3 {
        return None;
    }
    let (d, m, y) = (parts[0], parts[1], parts[2]);
    let today = to_local(now(), tz).date_naive();
    let mut age = today.year() - y;
    if (today.month() as i32, today.day() as i32) < (m, d) {
        age -= 1;
    }
    (0..150).contains(&age).then_some(age)
}

pub fn format_bytes(n: i64) -> String {
    let n = n as f64;
    if n < 1024.0 {
        format!("{n} bytes")
    } else if n < 1024.0 * 1024.0 {
        format!("{:.1} KB", n / 1024.0)
    } else if n < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB", n / 1024.0 / 1024.0)
    } else {
        format!("{:.2} GB", n / 1024.0 / 1024.0 / 1024.0)
    }
}

pub fn format_number(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

#[derive(Serialize, Debug, Clone)]
pub struct PageLink {
    pub page: i64,
    pub url: String,
    pub current: bool,
    pub gap: bool,
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct Pagination {
    pub page: i64,
    pub pages: i64,
    pub prev: Option<String>,
    pub next: Option<String>,
    pub links: Vec<PageLink>,
    pub base: String,
}

/// Build MyBB-style multipage navigation. `base` must contain `{page}`.
pub fn paginate(total: i64, per_page: i64, page: i64, base: &str) -> Pagination {
    let per_page = per_page.max(1);
    let pages = ((total + per_page - 1) / per_page).max(1);
    let page = page.clamp(1, pages);
    let url = |p: i64| {
        if p == 1 {
            base.replace("?page={page}", "")
                .replace("&page={page}", "")
                .replace("{page}", "1")
        } else {
            base.replace("{page}", &p.to_string())
        }
    };
    let mut links = Vec::new();
    if pages > 1 {
        let window = 2;
        let mut last = 0;
        for p in 1..=pages {
            if p == 1 || p == pages || (p - page).abs() <= window {
                if last != 0 && p - last > 1 {
                    links.push(PageLink {
                        page: 0,
                        url: String::new(),
                        current: false,
                        gap: true,
                    });
                }
                links.push(PageLink {
                    page: p,
                    url: url(p),
                    current: p == page,
                    gap: false,
                });
                last = p;
            }
        }
    }
    Pagination {
        page,
        pages,
        prev: (page > 1).then(|| url(page - 1)),
        next: (page < pages).then(|| url(page + 1)),
        links,
        base: base.to_string(),
    }
}

pub fn clamp_page(p: Option<i64>) -> i64 {
    p.unwrap_or(1).max(1)
}

/// Very small user-agent bot detector used for "Who's Online" spider listing.
pub fn detect_bot(ua: &str) -> Option<&'static str> {
    let l = ua.to_ascii_lowercase();
    const BOTS: &[(&str, &str)] = &[
        ("googlebot", "Google"),
        ("bingbot", "Bing"),
        ("yandex", "Yandex"),
        ("baiduspider", "Baidu"),
        ("duckduckbot", "DuckDuckGo"),
        ("applebot", "Apple"),
        ("facebookexternalhit", "Facebook"),
        ("twitterbot", "Twitter"),
        ("ahrefsbot", "Ahrefs"),
        ("semrushbot", "Semrush"),
        ("gptbot", "OpenAI"),
        ("claudebot", "Anthropic"),
        ("bot", "Generic Bot"),
        ("spider", "Generic Spider"),
        ("crawler", "Generic Crawler"),
    ];
    BOTS.iter().find(|(k, _)| l.contains(k)).map(|(_, n)| *n)
}

pub fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

pub fn valid_email(e: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^[^\s@<>()\[\],;:]+@[^\s@<>()\[\],;:]+\.[A-Za-z]{2,}$").unwrap()
    });
    e.len() <= 254 && RE.is_match(e)
}

/// Decode an uploaded image with bounded dimensions and memory. A few-KB PNG can declare
/// 60000×60000 pixels; decoding it unbounded would allocate gigabytes (decompression bomb).
pub fn decode_image(data: &[u8]) -> image::ImageResult<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    reader.decode()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slug() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(leading_id("12-hello"), Some(12));
        assert_eq!(leading_id("x"), None);
    }
    #[test]
    fn image_bomb_rejected() {
        // A tiny PNG header claiming 60000x60000 pixels must be refused before allocating.
        let img = image::RgbImage::new(1, 1);
        let mut png = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let mut bytes = png.into_inner();
        // IHDR width/height live at bytes 16..24; patch them (CRC mismatch is irrelevant: the
        // dimension limit is checked first, and either way decoding must fail).
        bytes[16..20].copy_from_slice(&60000u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&60000u32.to_be_bytes());
        assert!(decode_image(&bytes).is_err());
        let ok = image::RgbImage::new(64, 64);
        let mut png = std::io::Cursor::new(Vec::new());
        ok.write_to(&mut png, image::ImageFormat::Png).unwrap();
        assert!(decode_image(&png.into_inner()).is_ok());
    }

    #[test]
    fn pages() {
        let p = paginate(100, 10, 5, "/f?page={page}");
        assert_eq!(p.pages, 10);
        assert_eq!(p.prev.as_deref(), Some("/f?page=4"));
        assert_eq!(paginate(5, 10, 1, "/f?page={page}").links.len(), 0);
        assert_eq!(format_number(1234567), "1,234,567");
    }
}
