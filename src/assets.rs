//! Static assets: embedded in the binary, fingerprinted, and pre-compressed.
//!
//! * Templates link assets through `asset("css/rbb.css")`, which appends a content hash. Requests
//!   carrying the current hash are cached by browsers for a year (`immutable`), so repeat visits
//!   never revalidate CSS or JS; a new build changes the hash and the URL.
//! * Text assets are compressed once (Brotli at maximum quality, and gzip) in the background at
//!   start-up instead of on every request, which is both faster to serve and smaller on the wire.
//! * `If-None-Match` is honoured, so unversioned requests revalidate with a 304.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use rust_embed::RustEmbed;
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, LazyLock};

#[derive(RustEmbed)]
#[folder = "static/"]
pub struct StaticFiles;

struct Compressed {
    br: Bytes,
    gz: Bytes,
}

/// Content hash (first 8 bytes of SHA-256, hex) of every embedded file.
static VERSIONS: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
    StaticFiles::iter()
        .filter_map(|p| {
            StaticFiles::get(&p)
                .map(|f| (p.to_string(), hex::encode(&f.metadata.sha256_hash()[..8])))
        })
        .collect()
});

static COMPRESSED: LazyLock<dashmap::DashMap<String, Arc<Compressed>>> =
    LazyLock::new(dashmap::DashMap::new);

fn compressible(path: &str) -> bool {
    matches!(
        path.rsplit('.').next().unwrap_or(""),
        "css" | "js" | "mjs" | "svg" | "json" | "txt" | "map" | "xml" | "html"
    )
}

/// `/static/<path>?v=<hash>` for use in templates.
pub fn url(path: &str) -> String {
    let path = path.trim_start_matches('/').trim_start_matches("static/");
    match VERSIONS.get(path) {
        Some(v) => format!("/static/{path}?v={v}"),
        None => format!("/static/{path}"),
    }
}

/// Compress every text asset in the background. Until a file is done it is served uncompressed
/// here and compressed on the fly by the response compression layer.
pub fn precompress_all() {
    tokio::task::spawn_blocking(|| {
        for path in StaticFiles::iter() {
            if !compressible(&path) {
                continue;
            }
            let Some(f) = StaticFiles::get(&path) else {
                continue;
            };
            if f.data.len() < 512 {
                continue;
            }
            let mut br = Vec::with_capacity(f.data.len() / 3);
            {
                let mut w = brotli::CompressorWriter::new(&mut br, 16 * 1024, 11, 22);
                let _ = w.write_all(&f.data);
            }
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
            let _ = gz.write_all(&f.data);
            let gz = gz.finish().unwrap_or_default();
            COMPRESSED.insert(
                path.to_string(),
                Arc::new(Compressed {
                    br: br.into(),
                    gz: gz.into(),
                }),
            );
        }
    });
}

fn accepts(headers: &HeaderMap, enc: &str) -> bool {
    headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|e| e.split(';').next().is_some_and(|n| n.trim() == enc))
        })
}

pub async fn serve(path: &str, query: Option<&str>, headers: &HeaderMap) -> Response {
    let Some(f) = StaticFiles::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let version = VERSIONS.get(path).cloned().unwrap_or_default();
    let etag = format!("\"{version}\"");
    let versioned = query
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("v=")))
        .is_some_and(|v| v == version);
    let cache = if versioned {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600, stale-while-revalidate=86400"
    };
    let mut resp = if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|t| t.trim().trim_start_matches("W/") == etag)
        }) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        let (body, encoding) = match COMPRESSED.get(path) {
            Some(c) if accepts(headers, "br") => (Body::from(c.br.clone()), Some("br")),
            Some(c) if accepts(headers, "gzip") => (Body::from(c.gz.clone()), Some("gzip")),
            _ => (Body::from(f.data.into_owned()), None),
        };
        let mut r = (StatusCode::OK, body).into_response();
        let h = r.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(mime.as_ref())
                .unwrap_or(HeaderValue::from_static("application/octet-stream")),
        );
        if let Some(e) = encoding {
            h.insert(header::CONTENT_ENCODING, HeaderValue::from_static(e));
        }
        r
    };
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"0\"")),
    );
    if compressible(path) {
        h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_carry_content_hashes() {
        let u = url("css/rbb.css");
        assert!(u.starts_with("/static/css/rbb.css?v="), "{u}");
        assert_eq!(u.len(), "/static/css/rbb.css?v=".len() + 16);
        assert_eq!(url("/static/css/rbb.css"), u);
        assert_eq!(url("nope.css"), "/static/nope.css");
    }

    #[test]
    fn parses_accept_encoding() {
        let mut h = HeaderMap::new();
        h.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("gzip, deflate, br;q=1.0"),
        );
        assert!(accepts(&h, "br") && accepts(&h, "gzip") && !accepts(&h, "zstd"));
    }
}
