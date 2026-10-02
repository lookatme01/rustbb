//! Process configuration from environment variables (optionally loaded from `.env`).

use std::net::SocketAddr;

/// What a process does. One binary serves every role; large boards run them as separate,
/// independently scaled processes so slow mail, media, plugins or maintenance cannot take
/// request-serving capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Roles {
    /// Serve HTTP.
    pub web: bool,
    /// Deliver mail and run outbox jobs.
    pub worker: bool,
    /// Run scheduled tasks.
    pub scheduler: bool,
}

impl Roles {
    pub const ALL: Roles = Roles {
        web: true,
        worker: true,
        scheduler: true,
    };

    /// `all`, or a comma-separated list of `web`, `worker`, `scheduler`.
    pub fn parse(s: &str) -> anyhow::Result<Roles> {
        let mut r = Roles {
            web: false,
            worker: false,
            scheduler: false,
        };
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match part {
                "all" => r = Roles::ALL,
                "web" => r.web = true,
                "worker" => r.worker = true,
                "scheduler" => r.scheduler = true,
                other => {
                    anyhow::bail!("unknown role {other:?} (use web, worker, scheduler or all)")
                }
            }
        }
        anyhow::ensure!(r.web || r.worker || r.scheduler, "no role given");
        Ok(r)
    }
}

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
    /// What this process does (`RBB_ROLE`).
    pub roles: Roles,
    /// Internal listener for `/metrics`, `/livez` and `/readyz` (`RBB_ADMIN_LISTEN`).
    pub admin_listen: Option<SocketAddr>,
    /// Apply pending migrations when starting (`RBB_MIGRATE_ON_START`, default true). With
    /// several nodes, turn it off and run `rbb migrate` once per deploy.
    pub migrate_on_start: bool,
    /// Fraction of requests whose database statements are measured (`RBB_QUERY_SAMPLE_RATE`).
    pub query_sample_rate: f64,
    /// Statements slower than this are logged with a warning (`RBB_SLOW_QUERY_MS`).
    pub slow_query_ms: u64,
    /// On shutdown, keep serving (while reporting not ready) this long so load balancers stop
    /// sending requests first (`RBB_SHUTDOWN_DRAIN_SECS`).
    pub shutdown_drain_secs: u64,
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
            roles: Roles {
                web: true,
                worker: false,
                scheduler: false,
            },
            admin_listen: None,
            migrate_on_start: true,
            query_sample_rate: 0.0,
            slow_query_ms: 1000,
            shutdown_drain_secs: 0,
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
            // Deprecated: use RBB_ROLE without `scheduler` instead.
            run_tasks: get("RBB_RUN_TASKS", "true") == "true",
            plugins_dir: get("RBB_PLUGINS_DIR", DEFAULT_PLUGINS_DIR),
            plugins_trusted: get("RBB_PLUGINS_TRUSTED", "false") == "true",
            dev_templates: std::env::var("RBB_DEV_TEMPLATES").ok(),
            page_cache_mb: get("RBB_PAGE_CACHE_MB", DEFAULT_PAGE_CACHE_MB).parse()?,
            roles: Roles::parse(&get("RBB_ROLE", "all"))?,
            admin_listen: match std::env::var("RBB_ADMIN_LISTEN") {
                Ok(v) if !v.is_empty() => Some(v.parse()?),
                _ => None,
            },
            migrate_on_start: get("RBB_MIGRATE_ON_START", "true") == "true",
            query_sample_rate: get("RBB_QUERY_SAMPLE_RATE", "0.01")
                .parse::<f64>()?
                .clamp(0.0, 1.0),
            slow_query_ms: get("RBB_SLOW_QUERY_MS", "250").parse()?,
            shutdown_drain_secs: get("RBB_SHUTDOWN_DRAIN_SECS", "0").parse()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_parse() {
        assert_eq!(Roles::parse("all").unwrap(), Roles::ALL);
        let r = Roles::parse("web, worker").unwrap();
        assert!(r.web && r.worker && !r.scheduler);
        assert!(Roles::parse("").is_err());
        assert!(Roles::parse("cron").is_err());
    }
}
