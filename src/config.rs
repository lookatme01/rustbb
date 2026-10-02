//! Process configuration from environment variables (optionally loaded from `.env`).

use std::net::SocketAddr;

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub listen: SocketAddr,
    /// Secret used for CSRF tokens, captcha HMACs, signed URLs. Must be stable across nodes.
    pub secret: String,
    pub db_max_connections: u32,
    pub upload_dir: String,
    /// Trust `X-Forwarded-For` / `X-Real-IP` (enable only behind a reverse proxy).
    pub trust_proxy: bool,
    /// Serve cookies with the `Secure` attribute.
    pub secure_cookies: bool,
    pub run_tasks: bool,
    pub plugins_dir: String,
    /// Plugins are fully trusted: their HTML output is not sanitized.
    pub plugins_trusted: bool,
    pub dev_templates: Option<String>,
    /// Memory for the guest page cache in MiB (0 turns it off).
    pub page_cache_mb: u64,
}

pub const DEFAULT_DATABASE_URL: &str = "postgres://rbb@127.0.0.1:5433/rbb";
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8080";
pub const DEFAULT_DB_MAX_CONNECTIONS: &str = "32";
pub const DEFAULT_UPLOAD_DIR: &str = "uploads";
pub const DEFAULT_PLUGINS_DIR: &str = "plugins";
pub const DEFAULT_PAGE_CACHE_MB: &str = "64";

/// Placeholder secrets from the docs and examples.
pub const KNOWN_SECRETS: &[&str] = &[
    "change-me-to-a-long-random-string",
    "insecure-development-secret-change-me",
];

impl Config {
    /// Settings for tests: no background tasks, no page cache, a throwaway upload directory.
    pub fn for_tests(database_url: &str) -> Config {
        Config {
            database_url: database_url.to_string(),
            listen: "127.0.0.1:0".parse().unwrap(),
            secret: "integration-test-secret-0123456789abcdef".into(),
            db_max_connections: 16,
            upload_dir: std::env::temp_dir()
                .join(format!("rbb-test-uploads-{}", std::process::id()))
                .to_string_lossy()
                .into_owned(),
            trust_proxy: false,
            secure_cookies: false,
            run_tasks: false,
            plugins_dir: "/nonexistent-rbb-test-plugins".into(),
            plugins_trusted: false,
            dev_templates: None,
            page_cache_mb: 0,
        }
    }

    pub fn from_env() -> anyhow::Result<Self> {
        let get = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let secret = get("RBB_SECRET", "");
        // The secret signs CSRF tokens, captchas, 2FA challenges and forum-password cookies: a
        // guessable one lets anyone forge them. Refuse to run with an empty, example or short one.
        if secret.len() < 32 || KNOWN_SECRETS.contains(&secret.as_str()) {
            anyhow::bail!(
                "RBB_SECRET must be set to a random value of at least 32 characters \
                 (for example: `openssl rand -hex 32`)"
            );
        }
        Ok(Config {
            database_url: get("DATABASE_URL", DEFAULT_DATABASE_URL),
            listen: get("RBB_LISTEN", DEFAULT_LISTEN).parse()?,
            secret,
            db_max_connections: get("RBB_DB_MAX_CONNECTIONS", DEFAULT_DB_MAX_CONNECTIONS)
                .parse()?,
            upload_dir: get("RBB_UPLOAD_DIR", DEFAULT_UPLOAD_DIR),
            trust_proxy: get("RBB_TRUST_PROXY", "false") == "true",
            secure_cookies: get("RBB_SECURE_COOKIES", "false") == "true",
            run_tasks: get("RBB_RUN_TASKS", "true") == "true",
            plugins_dir: get("RBB_PLUGINS_DIR", DEFAULT_PLUGINS_DIR),
            plugins_trusted: get("RBB_PLUGINS_TRUSTED", "false") == "true",
            dev_templates: std::env::var("RBB_DEV_TEMPLATES").ok(),
            page_cache_mb: get("RBB_PAGE_CACHE_MB", DEFAULT_PAGE_CACHE_MB).parse()?,
        })
    }
}
