//! Integration-test harness: every test gets its own freshly migrated and installed PostgreSQL
//! database, dropped when the test ends.
//!
//! The server comes from `RBB_TEST_DATABASE_URL` (a URL whose database part is ignored), by
//! default the development cluster at `postgres://rbb@127.0.0.1:5433/postgres`. If that is not
//! set and the default server is unreachable, database tests are skipped with a message; when it
//! is set (as in CI), an unreachable server fails the test.

#![allow(dead_code)]

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use rbb::app::{App, AppState};
use rbb::config::Config;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Mutex;
use tower::ServiceExt;

const DEFAULT_SERVER: &str = "postgres://rbb@127.0.0.1:5433/postgres";

pub const ADMIN_PASSWORD: &str = "Admin-Passw0rd-for-tests";

fn server_url() -> (String, bool) {
    match std::env::var("RBB_TEST_DATABASE_URL") {
        Ok(u) => (u, true),
        Err(_) => (DEFAULT_SERVER.to_string(), false),
    }
}

/// A database that exists for the lifetime of this value.
pub struct TestDb {
    pub pool: PgPool,
    pub name: String,
    pub url: String,
    admin: PgConnectOptions,
}

static SWEPT: std::sync::Once = std::sync::Once::new();

impl TestDb {
    /// A new empty database with migrations applied, or `None` when no server is available
    /// and none was required.
    pub async fn new() -> Option<Self> {
        let (url, required) = server_url();
        let admin = PgConnectOptions::from_str(&url)
            .expect("RBB_TEST_DATABASE_URL is not a valid URL")
            .database("postgres")
            .disable_statement_logging();
        let mut conn = match admin.connect().await {
            Ok(c) => c,
            Err(e) if !required => {
                eprintln!("skipping database test: no PostgreSQL at {url} ({e})");
                return None;
            }
            Err(e) => panic!("cannot reach the test PostgreSQL server at {url}: {e}"),
        };
        // Databases left behind by an interrupted run. Ones still in use (other tests running in
        // parallel) refuse to drop, which is what we want.
        let mut sweep = false;
        SWEPT.call_once(|| sweep = true);
        if sweep {
            let stale: Vec<String> = sqlx::query_scalar(
                "SELECT datname FROM pg_database WHERE datname LIKE 'rbb_test_%'",
            )
            .fetch_all(&mut conn)
            .await
            .unwrap_or_default();
            for d in stale {
                let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS \"{d}\""))
                    .execute(&mut conn)
                    .await;
            }
        }
        let name = format!("rbb_test_{}", rbb::util::random_token(12).to_lowercase());
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&mut conn)
            .await
            .expect("create test database");
        let opts = admin.clone().database(&name);
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .connect_with(opts.clone())
            .await
            .expect("connect to test database");
        rbb::server::migrate(&pool)
            .await
            .expect("migrate test database");
        let url = opts.to_url_lossy().to_string();
        Some(TestDb {
            pool,
            name,
            url,
            admin,
        })
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        // Drop runs outside any async context guarantee: use a private runtime on a thread.
        let admin = self.admin.clone();
        let name = self.name.clone();
        let pool = self.pool.clone();
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                // Don't wait for checked-out connections (a failed test may leave some held by
                // tasks of its finished runtime): FORCE ends them.
                drop(pool);
                if let Ok(mut c) = admin.connect().await {
                    let _ =
                        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                            .execute(&mut c)
                            .await;
                }
            });
        })
        .join();
    }
}

pub fn test_config(database_url: &str) -> Config {
    Config::for_tests(database_url)
}

/// An installed board with its router, driven in-process without a socket.
pub struct TestApp {
    pub app: App,
    pub router: Router,
    pub db: TestDb,
}

impl TestApp {
    pub async fn new() -> Option<Self> {
        let db = TestDb::new().await?;
        rbb::install::install(
            &db.pool,
            "admin",
            ADMIN_PASSWORD,
            "admin@example.com",
            "Test Board",
            "http://127.0.0.1",
        )
        .await
        .expect("install");
        rbb::install::upgrade(&db.pool).await.expect("upgrade");
        let mut cfg = test_config(&db.url);
        // Each test app gets its own upload directory (tests run in parallel).
        cfg.upload_dir = std::env::temp_dir()
            .join(format!("rbb-test-uploads-{}", db.name))
            .to_string_lossy()
            .into_owned();
        tokio::fs::create_dir_all(format!("{}/attachments", cfg.upload_dir))
            .await
            .unwrap();
        let app = AppState::new(cfg, db.pool.clone())
            .await
            .expect("app state");
        let router = rbb::server::build_router(app.clone());
        Some(TestApp { app, router, db })
    }

    pub fn client(&self) -> Client {
        Client {
            router: self.router.clone(),
            cookies: Mutex::new(HashMap::new()),
            ip: "203.0.113.10".into(),
        }
    }

    /// Create an activated member directly in the database. Returns the uid.
    pub async fn create_user(&self, name: &str, password: &str) -> i32 {
        let hash = rbb::auth::hash_password(password).await.unwrap();
        let uid: i32 = sqlx::query_scalar(
            "INSERT INTO users (username, password, email, usergroup, regdate, lastactive, lastvisit, pmfolders)
             VALUES ($1, $2, $3, 2, 1, 1, 1, '[]') RETURNING uid",
        )
        .bind(name)
        .bind(hash)
        .bind(format!("{}@example.com", name.to_lowercase()))
        .fetch_one(&self.db.pool)
        .await
        .unwrap();
        uid
    }

    /// A client signed in as `uid` (a login row is created directly, skipping the form).
    pub async fn login_as(&self, uid: i32) -> Client {
        let token = rbb::util::random_token(48);
        let csrf = rbb::util::random_token(32);
        sqlx::query(
            "INSERT INTO logins (token_hash, uid, created, lastused, expires, ip, useragent, csrf)
             VALUES ($1, $2, 1, 1, 4102444800, '127.0.0.1', 'test', $3)",
        )
        .bind(rbb::util::sha256_hex(&token))
        .bind(uid)
        .bind(&csrf)
        .execute(&self.db.pool)
        .await
        .unwrap();
        let c = self.client();
        c.set_cookie(rbb::ctx::AUTH_COOKIE, &token);
        c.set_cookie("__csrf", &csrf);
        c
    }
}

pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl Response {
    pub fn location(&self) -> &str {
        self.headers
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }
}

/// A tiny browser: keeps cookies between requests.
pub struct Client {
    router: Router,
    cookies: Mutex<HashMap<String, String>>,
    pub ip: String,
}

impl Client {
    pub fn set_cookie(&self, k: &str, v: &str) {
        self.cookies.lock().unwrap().insert(k.into(), v.into());
    }

    pub fn cookie(&self, k: &str) -> Option<String> {
        self.cookies.lock().unwrap().get(k).cloned()
    }

    /// The CSRF token for signed-in clients made by `login_as`.
    pub fn csrf(&self) -> String {
        self.cookie("__csrf").unwrap_or_default()
    }

    fn cookie_header(&self) -> String {
        self.cookies
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| !k.starts_with("__"))
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub async fn request(&self, req: Request<Body>) -> Response {
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        for v in headers.get_all(header::SET_COOKIE) {
            let raw = v.to_str().unwrap_or("");
            let kv = raw.split(';').next().unwrap_or("");
            if let Some((k, val)) = kv.split_once('=') {
                let mut jar = self.cookies.lock().unwrap();
                if val.is_empty() {
                    jar.remove(k);
                } else {
                    let val = percent_encoding::percent_decode_str(val).decode_utf8_lossy();
                    jar.insert(k.to_string(), val.into_owned());
                }
            }
        }
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024)
            .await
            .unwrap();
        Response {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    fn builder(&self, method: &str, path: &str) -> axum::http::request::Builder {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("x-forwarded-for", &self.ip);
        let c = self.cookie_header();
        if !c.is_empty() {
            b = b.header(header::COOKIE, c);
        }
        b
    }

    pub async fn get(&self, path: &str) -> Response {
        self.request(self.builder("GET", path).body(Body::empty()).unwrap())
            .await
    }

    /// POST an urlencoded form; the CSRF token is added automatically for signed-in clients.
    pub async fn post_form(&self, path: &str, fields: &[(&str, &str)]) -> Response {
        let mut fields: Vec<(&str, &str)> = fields.to_vec();
        let csrf = self.csrf();
        if !csrf.is_empty() && !fields.iter().any(|(k, _)| *k == "my_post_key") {
            fields.push(("my_post_key", &csrf));
        }
        let body = serde_urlencoded::to_string(&fields).unwrap();
        self.request(
            self.builder("POST", path)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
    }

    pub async fn send_json(
        &self,
        method: &str,
        path: &str,
        json: serde_json::Value,
        extra: &[(&str, &str)],
    ) -> Response {
        let mut b = self
            .builder(method, path)
            .header(header::CONTENT_TYPE, "application/json");
        for (k, v) in extra {
            b = b.header(*k, *v);
        }
        self.request(b.body(Body::from(json.to_string())).unwrap())
            .await
    }
}

/// Extract the CSRF token (`my_post_key`) from a rendered form.
pub fn form_key(html: &str) -> String {
    let marker = "name=\"my_post_key\" value=\"";
    html.find(marker)
        .map(|i| {
            let rest = &html[i + marker.len()..];
            rest[..rest.find('"').unwrap_or(0)].to_string()
        })
        .unwrap_or_default()
}

/// Skip the rest of a test when no database is available.
#[macro_export]
macro_rules! test_app {
    () => {
        match common::TestApp::new().await {
            Some(a) => a,
            None => return,
        }
    };
}
