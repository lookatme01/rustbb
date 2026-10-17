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

/// Whether `nonce` solves the proof-of-work for `hash`: SHA-256 of "<hash>:<nonce>" must start
/// with at least `bits` zero bits. The nonce is a plain decimal number, as the page produces.
pub fn pow_valid(hash: &str, nonce: &str, bits: u32) -> bool {
    use sha2::{Digest, Sha256};
    if nonce.is_empty() || nonce.len() > 12 || !nonce.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let digest = Sha256::digest(format!("{hash}:{nonce}").as_bytes());
    u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]).leading_zeros() >= bits
}

/// Verify and consume a captcha answer.
///
/// With proof-of-work on, the page appends its nonce to the typed digits ("123456~48151"); that is
/// checked first, without touching the captcha, so a missing or wrong nonce costs nothing.
pub async fn check(ctx: &Ctx, hash: &str, answer: &str) -> AppResult<()> {
    let (answer, nonce) = answer.split_once('~').unwrap_or((answer, ""));
    let s = ctx.settings();
    if s.bool("captchapow") {
        let bits = s.int("captchapowbits").clamp(8, 24) as u32;
        if !pow_valid(hash, nonce, bits) {
            return Err(AppError::user(
                "Your browser did not complete the security check. Please enable JavaScript, reload the page and try again.",
            ));
        }
    }
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

/// A distorted PNG of the code.
///
/// It used to be an SVG, which handed a bot every digit as a handful of stroke coordinates it
/// could read straight from the markup. Pixels leave it nothing but the picture: the digits are
/// drawn with random rotation, skew and jitter, crossed by curves of the same colour (so a
/// colour threshold does not separate them), then the whole image is warped by sine waves and
/// speckled. This stops trivial scripts, not a determined solver: a board that needs more
/// than that should use proof-of-work or an external captcha service.
pub fn png_for(code: &str) -> Vec<u8> {
    use rand::Rng;
    const W: u32 = 220;
    const H: u32 = 70;
    let mut rng = rand::thread_rng();
    let mut ink = vec![0f32; (W * H) as usize];

    // A line with round ends, anti-aliased by distance.
    fn stroke(ink: &mut [f32], (x1, y1): (f32, f32), (x2, y2): (f32, f32), r: f32) {
        let (min_x, max_x) = (
            (x1.min(x2) - r - 1.0).floor().max(0.0),
            (x1.max(x2) + r + 1.0).ceil().min(W as f32 - 1.0),
        );
        let (min_y, max_y) = (
            (y1.min(y2) - r - 1.0).floor().max(0.0),
            (y1.max(y2) + r + 1.0).ceil().min(H as f32 - 1.0),
        );
        let (dx, dy) = (x2 - x1, y2 - y1);
        let len2 = (dx * dx + dy * dy).max(1e-6);
        for py in min_y as u32..=max_y as u32 {
            for px in min_x as u32..=max_x as u32 {
                let (fx, fy) = (px as f32 + 0.5, py as f32 + 0.5);
                let t = (((fx - x1) * dx + (fy - y1) * dy) / len2).clamp(0.0, 1.0);
                let d = ((fx - (x1 + t * dx)).powi(2) + (fy - (y1 + t * dy)).powi(2)).sqrt();
                let cover = (r + 0.5 - d).clamp(0.0, 1.0);
                let cell = &mut ink[(py * W + px) as usize];
                *cell = cell.max(cover);
            }
        }
    }

    for (i, ch) in code.chars().enumerate() {
        let d = ch.to_digit(10).unwrap_or(0) as usize;
        let scale = rng.gen_range(1.9..2.35);
        let rot: f32 = rng.gen_range(-0.16..0.16);
        let skew: f32 = rng.gen_range(-0.16..0.16);
        let (cx, cy) = (
            26.0 + i as f32 * 33.0 + rng.gen_range(-3.0..3.0),
            H as f32 / 2.0 + rng.gen_range(-4.0..4.0),
        );
        let radius = rng.gen_range(2.3..2.9);
        let place = |(x, y): (f32, f32), rng: &mut rand::rngs::ThreadRng| {
            let (x, y) = (
                x - 5.0 + rng.gen_range(-0.35..0.35),
                y - 10.0 + rng.gen_range(-0.35..0.35),
            );
            let x = x + skew * y;
            (
                cx + scale * (x * rot.cos() - y * rot.sin()),
                cy + scale * (x * rot.sin() + y * rot.cos()),
            )
        };
        for (si, name) in "abcdefg".chars().enumerate() {
            if DIGITS[d].contains(name) {
                let (x1, y1, x2, y2) = SEGS[si][0];
                let (a, b) = (place((x1, y1), &mut rng), place((x2, y2), &mut rng));
                stroke(&mut ink, a, b, radius);
            }
        }
    }
    // Curves of the same ink across the whole image.
    let mut decoys = vec![0f32; (W * H) as usize];
    for _ in 0..4 {
        let (y0, amp, freq, phase): (f32, f32, f32, f32) = (
            rng.gen_range(8.0..(H as f32 - 8.0)),
            rng.gen_range(5.0..16.0),
            rng.gen_range(0.02..0.06),
            rng.gen_range(0.0..std::f32::consts::TAU),
        );
        let r = rng.gen_range(0.5..0.9);
        let mut prev = (0.0f32, y0 + amp * phase.sin());
        for step in 1..=44 {
            let x = step as f32 * (W as f32 / 44.0);
            let cur = (x, y0 + amp * (x * freq + phase).sin());
            stroke(&mut decoys, prev, cur, r);
            prev = cur;
        }
    }

    // The curves are the same colour as the digits but only ~60% as strong, so a person sees them
    // as background while a plain threshold still catches them along with the digits.
    for (i, d) in decoys.iter().enumerate() {
        ink[i] = ink[i].max(d * 0.6);
    }
    // Warp with two sine waves, sample the ink, and colour it.
    let (a1, f1, p1) = (
        rng.gen_range(0.8..1.6),
        rng.gen_range(0.05..0.10),
        rng.gen_range(0.0..std::f32::consts::TAU),
    );
    let (a2, f2, p2) = (
        rng.gen_range(1.2..2.2),
        rng.gen_range(0.03..0.07),
        rng.gen_range(0.0..std::f32::consts::TAU),
    );
    let ink_at = |x: f32, y: f32| -> f32 {
        let (xi, yi) = (x.round(), y.round());
        if xi < 0.0 || yi < 0.0 || xi >= W as f32 || yi >= H as f32 {
            0.0
        } else {
            ink[(yi as u32 * W + xi as u32) as usize]
        }
    };
    let hue: [f32; 3] = [
        rng.gen_range(10.0..70.0),
        rng.gen_range(20.0..80.0),
        rng.gen_range(70.0..130.0),
    ];
    let mut img = image::RgbImage::new(W, H);
    for y in 0..H {
        for x in 0..W {
            let (sx, sy) = (
                x as f32 + a1 * (y as f32 * f1 + p1).sin(),
                y as f32 + a2 * (x as f32 * f2 + p2).sin(),
            );
            let cover = ink_at(sx, sy);
            let n: f32 = rng.gen_range(-9.0..9.0);
            let bg = [236.0 + n, 238.0 + n, 242.0 + n];
            let px =
                [0, 1, 2].map(|c| (bg[c] * (1.0 - cover) + hue[c] * cover).clamp(0.0, 255.0) as u8);
            img.put_pixel(x, y, image::Rgb(px));
        }
    }
    for _ in 0..220 {
        let (x, y) = (rng.gen_range(0..W), rng.gen_range(0..H));
        let v: u8 = if rng.gen_bool(0.5) {
            rng.gen_range(40..110)
        } else {
            rng.gen_range(200..255)
        };
        img.put_pixel(x, y, image::Rgb([v, v, v.saturating_add(10)]));
    }
    crate::infra::uploads::png(&image::DynamicImage::ImageRgb8(img)).unwrap_or_default()
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
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        png_for(&code),
    )
        .into_response())
}

pub async fn refresh(ctx: Ctx) -> AppResult<Response> {
    // Every call writes a row that stays until the hourly cleanup, and it needs no sign-in.
    if !ctx
        .app
        .throttle(
            &format!("captcha-refresh:{}", crate::util::ip_bucket(&ctx.ip)),
            30,
            300,
        )
        .await
    {
        return Err(AppError::RateLimited);
    }
    let h = new_captcha(&ctx).await?;
    Ok(axum::Json(serde_json::json!({"hash": h})).into_response())
}

#[cfg(test)]
mod tests {
    use super::png_for;

    #[test]
    fn the_image_is_a_real_png_of_the_right_size_that_differs_every_time() {
        let a = png_for("482915");
        let b = png_for("482915");
        let img = image::load_from_memory(&a)
            .expect("a decodable PNG")
            .to_rgb8();
        assert_eq!(img.dimensions(), (220, 70));
        assert_ne!(a, b, "every render is distorted differently");
        assert!(a.len() < 80_000, "{} bytes", a.len());
        // There is ink on it: a good number of clearly dark pixels, but nowhere near all.
        let dark = img
            .pixels()
            .filter(|p| p.0.iter().all(|c| *c < 150))
            .count();
        assert!((600..8000).contains(&dark), "{dark} dark pixels");
        // The code is not recoverable from the bytes as text.
        assert!(!a.windows(6).any(|w| w == b"482915"));
        // Set RBB_CAPTCHA_SAMPLES=dir to look at a few by eye.
        if let Ok(dir) = std::env::var("RBB_CAPTCHA_SAMPLES") {
            for (i, code) in ["482915", "073268", "915304", "260781"].iter().enumerate() {
                std::fs::write(format!("{dir}/captcha{i}.png"), png_for(code)).unwrap();
            }
        }
    }
}
