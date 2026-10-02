//! rbb — a fast, scalable forum engine inspired by MyBB.

mod admin;
mod app;
mod assets;
mod audit;
mod automod;
mod auth;
mod cache;
mod config;
mod ctx;
mod debugbar;
mod doctor;
mod error;
mod i18n;
mod import;
mod install;
mod mail;
mod models;
mod notify;
mod ops;
mod parser;
mod pagecache;
mod perms;
mod pgp;
mod plugins;
mod posting;
mod privacy;
mod render;
mod routes;
mod seed;
mod settings;
mod system;
mod tasks;
mod templates;
mod util;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use clap::{Parser, Subcommand};
use sqlx::postgres::PgPoolOptions;
use std::net::SocketAddr;
use std::time::Duration;
use tower_http::compression::CompressionLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

#[derive(Parser)]
#[command(name = "rbb", version, about = "rbb forum server")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the web server (default). Applies pending migrations first.
    Serve,
    /// Apply database migrations.
    Migrate,
    /// Create default board data and an administrator account.
    Install {
        #[arg(long, default_value = "admin")]
        admin_user: String,
        #[arg(long, env = "RBB_ADMIN_PASSWORD")]
        admin_password: String,
        #[arg(long, default_value = "admin@example.com")]
        admin_email: String,
        #[arg(long, default_value = "rbb Community Forums")]
        board_name: String,
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        board_url: String,
    },
    /// Generate a large synthetic board for load testing.
    Seed {
        #[arg(long, default_value_t = 10_000)]
        users: i64,
        #[arg(long, default_value_t = 50_000)]
        threads: i64,
        #[arg(long, default_value_t = 1_000_000)]
        posts: i64,
    },
    /// Check configuration, database, board and plugins, and explain how to fix problems.
    Doctor {
        /// Treat warnings as failures (non-zero exit code).
        #[arg(long)]
        strict: bool,
    },
    /// Recount all denormalized counters.
    Recount,
    /// Verify denormalized counters against the data (exit code 1 on mismatch).
    Check,
    /// Import a MyBB database (MySQL dump converted, or direct connection) — see docs.
    ImportMybb {
        /// MySQL connection URL of the MyBB database, e.g. mysql://user:pass@host/mybb
        #[arg(long)]
        mysql_url: String,
        #[arg(long, default_value = "mybb_")]
        prefix: String,
        /// Where MyBB's uploads/ directory was copied, relative to RBB_UPLOAD_DIR.
        #[arg(long, default_value = "mybb")]
        uploads_prefix: String,
        /// Confirm that all existing users, forums and posts in the rbb database will be replaced.
        #[arg(long)]
        yes: bool,
    },
}

async fn static_file(
    Path(path): Path<String>,
    axum::extract::RawQuery(q): axum::extract::RawQuery,
    headers: axum::http::HeaderMap,
) -> Response {
    assets::serve(&path, q.as_deref(), &headers).await
}

async fn theme_css(State(app): State<app::App>, Path(tid): Path<String>) -> Response {
    let tid: i32 = tid.trim_end_matches(".css").parse().unwrap_or(0);
    let cache = app.cache();
    // Child themes inherit their parents' stylesheets.
    let mut chain = Vec::new();
    let mut cur = cache.theme(tid);
    while let Some(t) = cur {
        chain.push(t.stylesheet.clone());
        cur = if t.pid > 0 && chain.len() < 16 {
            cache.theme(t.pid)
        } else {
            None
        };
    }
    chain.reverse();
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        chain.join("\n"),
    )
        .into_response()
}

async fn health(State(app): State<app::App>) -> Response {
    match sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&app.db)
        .await
    {
        Ok(_) => (StatusCode::OK, "ok").into_response(),
        Err(e) => {
            // Don't reveal connection strings or server details to unauthenticated callers.
            tracing::warn!("health check failed: {e}");
            (StatusCode::SERVICE_UNAVAILABLE, "database unavailable").into_response()
        }
    }
}

async fn connect(cfg: &config::Config) -> anyhow::Result<sqlx::PgPool> {
    Ok(PgPoolOptions::new()
        .max_connections(cfg.db_max_connections)
        .min_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(300))
        // sqlx pings every connection before handing it out by default: an extra database round
        // trip for every query. Broken connections are detected when a query fails and dropped
        // anyway, and recycling connections every 30 minutes keeps them from going stale.
        .test_before_acquire(false)
        .max_lifetime(Duration::from_secs(1800))
        .connect(&cfg.database_url)
        .await
        .map_err(|e| doctor::hinted(e.into()))?)
}

async fn migrate(db: &sqlx::PgPool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations").run(db).await.map_err(|e| doctor::hinted(e.into()))?;
    Ok(())
}

pub fn build_router(app: app::App) -> Router {
    let csp = "default-src 'self'; img-src * data: blob:; media-src *; style-src 'self' 'unsafe-inline'; script-src 'self'; \
               frame-src https://www.youtube-nocookie.com https://player.vimeo.com https://www.dailymotion.com https://player.twitch.tv; \
               connect-src 'self'; form-action 'self'; frame-ancestors 'self'; base-uri 'self'; object-src 'none'";
    let dynamic = routes::router().layer(axum::middleware::from_fn_with_state(
        app.clone(),
        ctx::context_middleware,
    ));
    Router::new()
        .route("/static/{*path}", get(static_file))
        .route("/css/theme/{tid}", get(theme_css))
        .route("/healthz", get(health))
        .nest_service(
            "/uploads/avatars",
            tower_http::services::ServeDir::new(format!("{}/avatars", app.cfg.upload_dir)),
        )
        .nest_service(
            "/uploads/banners",
            tower_http::services::ServeDir::new(format!("{}/banners", app.cfg.upload_dir)),
        )
        .merge(dynamic)
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("SAMEORIGIN"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("strict-origin-when-cross-origin"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(csp),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static(
                "camera=(), microphone=(), geolocation=(), payment=(), usb=(), interest-cohort=()",
            ),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::STRICT_TRANSPORT_SECURITY,
            // Only meaningful (and only sent) when the board is served over HTTPS.
            app.cfg
                .secure_cookies
                .then(|| HeaderValue::from_static("max-age=31536000; includeSubDomains")),
        ))
        // Dynamic pages: a fast compression level. Level 3 is within a few percent of the default
        // (6) in size at a fraction of the CPU, and compression was the largest single cost of a
        // logged-in page view. Static assets are pre-compressed at maximum level separately.
        .layer(CompressionLayer::new().quality(tower_http::CompressionLevel::Precise(3)))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(60),
        ))
        .layer(tower_http::catch_panic::CatchPanicLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(app)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutdown signal received");
}

async fn serve(cfg: config::Config) -> anyhow::Result<()> {
    let db = connect(&cfg).await?;
    migrate(&db).await?;
    let installed: Option<i32> = sqlx::query_scalar("SELECT gid FROM usergroups LIMIT 1")
        .fetch_optional(&db)
        .await?;
    if installed.is_none() {
        // A fixed default password would be a known credential on every fresh install.
        let password = util::random_token(20);
        tracing::warn!(
            "board is not installed — created administrator 'admin' with password '{password}' (shown once; change it after logging in)"
        );
        install::install(
            &db,
            "admin",
            &password,
            "admin@example.com",
            "rbb Community Forums",
            &format!("http://{}", cfg.listen),
        )
        .await?;
    }
    install::upgrade(&db).await?;
    tokio::fs::create_dir_all(format!("{}/avatars", cfg.upload_dir)).await?;
    tokio::fs::create_dir_all(format!("{}/attachments", cfg.upload_dir)).await?;
    let app = app::AppState::new(cfg.clone(), db).await?;
    app::spawn_listener(app.clone());
    app::spawn_flushers(app.clone());
    // Compress static assets and compile every template now, not on the first visitor's request.
    if cfg.dev_templates.is_none() {
        assets::precompress_all();
    }
    app.tpl.warm(app.cache().default_theme());
    mail::spawn_worker(app.clone());
    if cfg.run_tasks {
        tasks::spawn_scheduler(app.clone());
    }
    let router = build_router(app.clone());
    let listener = tokio::net::TcpListener::bind(cfg.listen).await?;
    tracing::info!("rbb listening on http://{}", cfg.listen);
    // On SIGTERM/Ctrl-C: stop accepting, end live streams (browsers reconnect to the next
    // instance), give in-flight requests 10 s, then exit even if some are still open.
    let signalled = {
        let app = app.clone();
        async move {
            shutdown_signal().await;
            let _ = app.shutdown.send(true);
        }
    };
    let server = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(signalled);
    let mut stopping = app.shutdown.subscribe();
    tokio::select! {
        r = server => r?,
        _ = async {
            let _ = stopping.wait_for(|s| *s).await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        } => tracing::warn!("requests still open 10 s after shutdown began; exiting anyway"),
    }
    // Persist buffered activity before exiting.
    let _ = app::flush_activity(&app).await;
    let _ = app::flush_views(&app).await;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let json_logs = std::env::var("RBB_LOG_JSON")
        .map(|v| v == "true")
        .unwrap_or(false);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "rbb=info,tower_http=warn,sqlx=warn".into());
    use tracing_subscriber::prelude::*;
    let fmt_layer = if json_logs {
        tracing_subscriber::fmt::layer().json().with_filter(filter).boxed()
    } else {
        tracing_subscriber::fmt::layer().with_filter(filter).boxed()
    };
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(debugbar::QueryLayer.with_filter(debugbar::filter()))
        .init();
    let cli = Cli::parse();
    // Before loading the configuration: doctor reports configuration problems instead of exiting.
    if let Some(Cmd::Doctor { strict }) = cli.cmd {
        std::process::exit(doctor::run(strict).await);
    }
    let cfg = config::Config::from_env()?;
    match cli.cmd.unwrap_or(Cmd::Serve) {
        Cmd::Serve => serve(cfg).await,
        Cmd::Migrate => {
            let db = connect(&cfg).await?;
            migrate(&db).await?;
            println!("migrations applied");
            Ok(())
        }
        Cmd::Install {
            admin_user,
            admin_password,
            admin_email,
            board_name,
            board_url,
        } => {
            let db = connect(&cfg).await?;
            migrate(&db).await?;
            install::install(
                &db,
                &admin_user,
                &admin_password,
                &admin_email,
                &board_name,
                &board_url,
            )
            .await?;
            println!("board installed; administrator '{admin_user}' created");
            Ok(())
        }
        Cmd::Seed {
            users,
            threads,
            posts,
        } => {
            let db = connect(&cfg).await?;
            migrate(&db).await?;
            seed::seed(&db, users, threads, posts).await
        }
        Cmd::Recount => {
            let db = connect(&cfg).await?;
            let app = app::AppState::new(cfg, db).await?;
            ops::rebuild_all_counters(&app)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("counters rebuilt");
            Ok(())
        }
        Cmd::Doctor { .. } => unreachable!("handled before the configuration is loaded"),
        Cmd::Check => {
            let db = connect(&cfg).await?;
            let problems = ops::check_counters(&db).await?;
            if problems.is_empty() {
                println!("all counters are consistent");
                Ok(())
            } else {
                for p in &problems {
                    println!("{p}");
                }
                std::process::exit(1)
            }
        }
        Cmd::ImportMybb { mysql_url, prefix, uploads_prefix, yes } => {
            if !yes {
                anyhow::bail!("importing replaces all users, forums and posts in this database; re-run with --yes to continue");
            }
            let db = connect(&cfg).await?;
            migrate(&db).await?;
            if sqlx::query_scalar::<_, i32>("SELECT gid FROM usergroups LIMIT 1").fetch_optional(&db).await?.is_none() {
                install::install(&db, "admin", &util::random_token(16), "admin@example.com", "rbb Community Forums", "http://127.0.0.1:8080").await?;
            }
            // Load with foreign-key triggers off (when permitted); orphans are repaired afterwards.
            let is_super: bool = sqlx::query_scalar("SELECT rolsuper FROM pg_roles WHERE rolname = current_user").fetch_one(&db).await?;
            let load_db = if is_super {
                PgPoolOptions::new()
                    .max_connections(2)
                    .after_connect(|c, _| {
                        Box::pin(async move {
                            sqlx::query("SET session_replication_role = replica").execute(c).await?;
                            Ok(())
                        })
                    })
                    .connect(&cfg.database_url)
                    .await?
            } else {
                db.clone()
            };
            let mut imp = import::Importer::connect(&mysql_url, load_db.clone(), &prefix).await?;
            imp.run(&uploads_prefix).await?;
            load_db.close().await;
            install::upgrade(&db).await?;
            println!("rebuilding counters…");
            let app = app::AppState::new(cfg, db).await?;
            ops::rebuild_all_counters(&app).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            app.bump_parser_rev().await?;
            println!("import complete. MyBB passwords work as-is and are upgraded on first login.");
            Ok(())
        }
    }
}
