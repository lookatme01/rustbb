//! Built-in CAPTCHA: random digits drawn as jittered stroke paths in an SVG (no text nodes to
//! scrape), with noise lines. Codes are single-use and expire after 30 minutes.

use crate::ctx::Ctx;
use crate::error::{AppError, AppResult};
use crate::util::{now, random_token};
use axum::extract::Path;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use rand::Rng;

pub async fn new_captcha(ctx: &Ctx) -> AppResult<String> {
    let hash = random_token(32);
    let code: String = {
        let mut rng = rand::thread_rng();
        (0..6)
            .map(|_| char::from(b'0' + rng.gen_range(0..10u8)))
            .collect()
    };
    sqlx::query("INSERT INTO captcha (imagehash, imagestring, dateline) VALUES ($1, $2, $3)")
        .bind(&hash)
        .bind(&code)
        .bind(now())
        .execute(&ctx.app.db)
        .await?;
    Ok(hash)
}

/// Verify and consume a captcha answer.
pub async fn check(ctx: &Ctx, hash: &str, answer: &str) -> AppResult<()> {
    let row: Option<(String,)> = sqlx::query_as(
        "DELETE FROM captcha WHERE imagehash = $1 AND dateline > $2 RETURNING imagestring",
    )
    .bind(hash)
    .bind(now() - 1800)
    .fetch_optional(&ctx.app.db)
    .await?;
    match row {
        Some((s,)) if s.eq_ignore_ascii_case(answer.trim()) => Ok(()),
        _ => Err(AppError::user(
            "The image verification code that you entered was incorrect. Please enter the code exactly how it appears in the image.",
        )),
    }
}

const SEGS: [[(f32, f32, f32, f32); 1]; 7] = [
    [(0.0, 0.0, 10.0, 0.0)],
    [(10.0, 0.0, 10.0, 10.0)],
    [(10.0, 10.0, 10.0, 20.0)],
    [(0.0, 20.0, 10.0, 20.0)],
    [(0.0, 10.0, 0.0, 20.0)],
    [(0.0, 0.0, 0.0, 10.0)],
    [(0.0, 10.0, 10.0, 10.0)],
];
const DIGITS: [&str; 10] = [
    "abcdef", "bc", "abged", "abgcd", "fgbc", "afgcd", "afgedc", "abc", "abcdefg", "abcdfg",
];

pub fn svg_for(code: &str) -> String {
    let mut rng = rand::thread_rng();
    let (w, h) = (220.0f32, 70.0f32);
    let mut s = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\"><rect width=\"100%\" height=\"100%\" fill=\"#f4f6f8\"/>"
    );
    for _ in 0..8 {
        s.push_str(&format!(
            "<path d=\"M{} {} Q{} {} {} {}\" stroke=\"hsl({},40%,60%)\" stroke-width=\"{:.1}\" fill=\"none\"/>",
            rng.gen_range(0.0..w), rng.gen_range(0.0..h), rng.gen_range(0.0..w), rng.gen_range(0.0..h), rng.gen_range(0.0..w), rng.gen_range(0.0..h),
            rng.gen_range(0..360), rng.gen_range(0.8..2.0)
        ));
    }
    for (i, ch) in code.chars().enumerate() {
        let d = ch.to_digit(10).unwrap_or(0) as usize;
        let ox = 18.0 + i as f32 * 33.0 + rng.gen_range(-3.0..3.0);
        let oy = 14.0 + rng.gen_range(-5.0..5.0);
        let sc = rng.gen_range(1.6..2.0);
        let rot = rng.gen_range(-15.0..15.0);
        let skew = rng.gen_range(-12.0..12.0);
        let mut path = String::new();
        for (si, name) in "abcdefg".chars().enumerate() {
            if DIGITS[d].contains(name) {
                let (x1, y1, x2, y2) = SEGS[si][0];
                let j = |v: f32, r: &mut rand::rngs::ThreadRng| v + r.gen_range(-1.2..1.2);
                path.push_str(&format!(
                    "M{:.1} {:.1} L{:.1} {:.1} ",
                    j(x1, &mut rng),
                    j(y1, &mut rng),
                    j(x2, &mut rng),
                    j(y2, &mut rng)
                ));
            }
        }
        s.push_str(&format!(
            "<g transform=\"translate({ox:.1} {oy:.1}) rotate({rot:.1} 5 10) skewX({skew:.1}) scale({sc:.2})\"><path d=\"{path}\" stroke=\"hsl({},55%,30%)\" stroke-width=\"2.4\" stroke-linecap=\"round\" fill=\"none\"/></g>",
            rng.gen_range(180..260)
        ));
    }
    for _ in 0..40 {
        s.push_str(&format!("<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"{:.1}\" fill=\"hsl({},30%,50%)\" opacity=\"0.5\"/>", rng.gen_range(0.0..w), rng.gen_range(0.0..h), rng.gen_range(0.5..1.8), rng.gen_range(0..360)));
    }
    s.push_str("</svg>");
    s
}

pub async fn image(ctx: Ctx, Path(hash): Path<String>) -> AppResult<Response> {
    let code: Option<String> =
        sqlx::query_scalar("SELECT imagestring FROM captcha WHERE imagehash = $1")
            .bind(&hash)
            .fetch_optional(&ctx.app.db)
            .await?;
    let code = code.ok_or_else(|| AppError::not_found("captcha"))?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        svg_for(&code),
    )
        .into_response())
}

pub async fn refresh(ctx: Ctx) -> AppResult<Response> {
    let h = new_captcha(&ctx).await?;
    Ok(axum::Json(serde_json::json!({"hash": h})).into_response())
}
