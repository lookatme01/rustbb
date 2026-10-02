//! Password hashing, login tokens and ban filters.

use crate::app::App;
use crate::ctx::{AUTH_COOKIE, CtxInner};
use crate::error::{AppError, AppResult};
use crate::util::{self, now};
use argon2::Argon2;
use argon2::password_hash::{
    PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng,
};

fn argon() -> Argon2<'static> {
    // OWASP recommended minimum: m=19 MiB, t=2, p=1.
    let params = argon2::Params::new(19 * 1024, 2, 1, None).expect("argon2 params");
    Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
}

/// At most a quarter of the CPU cores (min. 2) hash passwords at once. A burst of log-ins (everyone reconnecting
/// after a restart, a credential-stuffing attempt) then queues instead of taking every core and
/// 19 MiB per hash, so page views stay fast.
static HASHING: std::sync::LazyLock<tokio::sync::Semaphore> = std::sync::LazyLock::new(|| {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    tokio::sync::Semaphore::new((cores / 4).max(2))
});

pub async fn hash_password(pw: &str) -> AppResult<String> {
    let pw = pw.to_string();
    let _permit = HASHING
        .acquire()
        .await
        .map_err(|e| AppError::Other(e.into()))?;
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut OsRng);
        argon()
            .hash_password(pw.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| AppError::Other(anyhow::anyhow!("hash: {e}")))
    })
    .await
    .map_err(|e| AppError::Other(e.into()))?
}

/// Verify a password. Supports imported MyBB hashes (`mybb$<salt>$<md5>`), which are upgraded
/// to argon2 on successful login by the caller (`needs_rehash`).
pub async fn verify_password(pw: &str, stored: &str) -> bool {
    if let Some(rest) = stored.strip_prefix("mybb$") {
        let Some((salt, hash)) = rest.split_once('$') else {
            return false;
        };
        let md5 = |s: &str| format!("{:x}", md5_compute(s.as_bytes()));
        let calc = md5(&format!("{}{}", md5(salt), md5(pw)));
        return util::ct_eq(&calc, hash);
    }
    let (pw, stored) = (pw.to_string(), stored.to_string());
    let Ok(_permit) = HASHING.acquire().await else {
        return false;
    };
    tokio::task::spawn_blocking(move || match PasswordHash::new(&stored) {
        Ok(h) => argon().verify_password(pw.as_bytes(), &h).is_ok(),
        Err(_) => false,
    })
    .await
    .unwrap_or(false)
}

pub fn needs_rehash(stored: &str) -> bool {
    stored.starts_with("mybb$")
}

/// Minimal MD5 (only used to verify legacy imported MyBB passwords).
fn md5_compute(input: &[u8]) -> Md5Digest {
    let s: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64)
        .map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32)
        .collect();
    let (mut a0, mut b0, mut c0, mut d0) =
        (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    let mut msg = input.to_vec();
    let bitlen = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_le_bytes());
    for chunk in msg.chunks(64) {
        let m: Vec<u32> = (0..16)
            .map(|i| {
                u32::from_le_bytes([
                    chunk[i * 4],
                    chunk[i * 4 + 1],
                    chunk[i * 4 + 2],
                    chunk[i * 4 + 3],
                ])
            })
            .collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (mut f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(s[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..].copy_from_slice(&d0.to_le_bytes());
    Md5Digest(out)
}

struct Md5Digest([u8; 16]);
impl std::fmt::LowerHex for Md5Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// Create a persistent login and set the auth cookie.
pub async fn create_login(ctx: &CtxInner, uid: i32, remember: bool) -> AppResult<String> {
    let token = util::random_token(48);
    let days = ctx.settings().int("loginsessionlength").max(1);
    let expires = if remember {
        now() + days * 86400
    } else {
        now() + 86400
    };
    sqlx::query(
        "INSERT INTO logins (token_hash, uid, created, lastused, expires, ip, useragent, csrf) VALUES ($1, $2, $3, $3, $4, $5, $6, $7)",
    )
    .bind(util::sha256_hex(&token))
    .bind(uid)
    .bind(now())
    .bind(expires)
    .bind(&ctx.ip)
    .bind(&ctx.useragent)
    .bind(util::random_token(32))
    .execute(&ctx.app.db)
    .await?;
    ctx.add_cookie(
        AUTH_COOKIE,
        &token,
        if remember { Some(days * 86400) } else { None },
        true,
    );
    sqlx::query("UPDATE users SET loginattempts = 0, lastip = $2 WHERE uid = $1")
        .bind(uid)
        .bind(&ctx.ip)
        .execute(&ctx.app.db)
        .await?;
    // Rotate the guest session id so a pre-login sid can't be fixated.
    ctx.add_cookie(
        crate::ctx::SID_COOKIE,
        &util::random_token(32),
        Some(31536000),
        true,
    );
    Ok(token)
}

/// Replace the current sign-in token with a fresh one, keeping the session itself (and its CSRF
/// token, so forms open in other tabs still work), after a privilege change (password or 2FA
/// change, Admin CP verification): a token captured before the change stops working.
pub async fn rotate_login(ctx: &CtxInner) -> AppResult<()> {
    let Some(old) = &ctx.token_hash else {
        return Ok(());
    };
    let token = util::random_token(48);
    let expires: Option<i64> = sqlx::query_scalar(
        "UPDATE logins SET token_hash = $2, lastused = $3 WHERE token_hash = $1 RETURNING expires",
    )
    .bind(old)
    .bind(util::sha256_hex(&token))
    .bind(now())
    .fetch_optional(&ctx.app.db)
    .await?;
    if let Some(exp) = expires {
        ctx.add_cookie(AUTH_COOKIE, &token, Some((exp - now()).max(60)), true);
    }
    Ok(())
}

pub async fn destroy_login(ctx: &CtxInner) -> AppResult<()> {
    if let Some(h) = &ctx.token_hash {
        sqlx::query("DELETE FROM logins WHERE token_hash = $1")
            .bind(h)
            .execute(&ctx.app.db)
            .await?;
    }
    ctx.clear_cookie(AUTH_COOKIE);
    Ok(())
}

/// Invalidate all logins of a user (password change, ban, etc.), optionally keeping one.
/// Invalidate all browser sessions of a user (optionally keeping one) and revoke all of their
/// API tokens: after a password change, 2FA change or "log out everywhere".
pub async fn destroy_all_logins(app: &App, uid: i32, keep: Option<&str>) -> AppResult<()> {
    sqlx::query("DELETE FROM logins WHERE uid = $1 AND token_hash <> $2")
        .bind(uid)
        .bind(keep.unwrap_or(""))
        .execute(&app.db)
        .await?;
    sqlx::query("UPDATE api_tokens SET revoked_at = now() WHERE uid = $1 AND revoked_at IS NULL")
        .bind(uid)
        .execute(&app.db)
        .await?;
    Ok(())
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let p = regex::escape(&pattern.to_lowercase()).replace(r"\*", ".*");
    regex::Regex::new(&format!("^{p}$"))
        .map(|r| r.is_match(&value.to_lowercase()))
        .unwrap_or(false)
}

/// Check ban filters: type 1 = IP, 2 = username, 3 = email.
pub async fn is_filtered(app: &App, kind: i16, value: &str) -> AppResult<bool> {
    let filters: Vec<(i32, String)> =
        sqlx::query_as("SELECT fid, filter FROM banfilters WHERE type = $1")
            .bind(kind)
            .fetch_all(&app.db)
            .await?;
    for (fid, f) in filters {
        let hit = if kind == 1 {
            ip_matches(&f, value)
        } else if kind == 3 && !f.contains('@') && !f.contains('*') {
            // bare domain bans: "example.com"
            value
                .to_lowercase()
                .ends_with(&format!("@{}", f.to_lowercase()))
                || wildcard_match(&f, value)
        } else {
            wildcard_match(&f, value)
        };
        if hit {
            let _ = sqlx::query("UPDATE banfilters SET lastuse = $2 WHERE fid = $1")
                .bind(fid)
                .bind(now())
                .execute(&app.db)
                .await;
            return Ok(true);
        }
    }
    Ok(false)
}

/// IP filters support wildcards (`192.168.*`) and CIDR (`10.0.0.0/8`).
pub fn ip_matches(filter: &str, ip: &str) -> bool {
    if let Some((net, bits)) = filter.split_once('/') {
        let (Ok(net), Ok(bits), Ok(ip)) = (
            net.parse::<std::net::IpAddr>(),
            bits.parse::<u32>(),
            ip.parse::<std::net::IpAddr>(),
        ) else {
            return false;
        };
        return match (net, ip) {
            (std::net::IpAddr::V4(n), std::net::IpAddr::V4(i)) => {
                let mask = if bits == 0 {
                    0
                } else {
                    u32::MAX << (32 - bits.min(32))
                };
                u32::from(n) & mask == u32::from(i) & mask
            }
            (std::net::IpAddr::V6(n), std::net::IpAddr::V6(i)) => {
                let mask = if bits == 0 {
                    0
                } else {
                    u128::MAX << (128 - bits.min(128))
                };
                u128::from(n) & mask == u128::from(i) & mask
            }
            _ => false,
        };
    }
    wildcard_match(filter, ip)
}

pub fn valid_username_chars(name: &str) -> bool {
    !name
        .chars()
        .any(|c| c.is_control() || "<>&\\\"';,".contains(c))
        && name.trim() == name
        && !name.contains("  ")
}

pub fn password_strength_error(ctx: &CtxInner, pw: &str, username: &str) -> Option<String> {
    let s = ctx.settings();
    let min = s.int("minpasswordlength").max(6) as usize;
    if pw.chars().count() < min {
        return Some(format!(
            "Your password must be at least {min} characters long."
        ));
    }
    if pw.len() > 200 {
        return Some("Your password is too long.".into());
    }
    if s.bool("requirecomplexpasswords")
        && !(pw.chars().any(|c| c.is_uppercase())
            && pw.chars().any(|c| c.is_lowercase())
            && pw.chars().any(|c| c.is_ascii_digit()))
    {
        return Some(
            "Your password must contain an upper case letter, a lower case letter and a number."
                .into(),
        );
    }
    if !username.is_empty() && pw.to_lowercase() == username.to_lowercase() {
        return Some("Your password cannot be the same as your username.".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn md5_vectors() {
        assert_eq!(
            format!("{:x}", md5_compute(b"")),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
        assert_eq!(
            format!(
                "{:x}",
                md5_compute(b"The quick brown fox jumps over the lazy dog")
            ),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }
    #[test]
    fn ip_filters() {
        assert!(ip_matches("10.0.0.0/8", "10.2.3.4"));
        assert!(!ip_matches("10.0.0.0/8", "11.2.3.4"));
        assert!(ip_matches("192.168.*", "192.168.1.1"));
    }
}
