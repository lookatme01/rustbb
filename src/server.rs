//! Process assembly: database connection, router, runtime roles and the `serve` entry point.

use crate::infra::observe;
use crate::{app, assets, config, ctx, doctor, install, mail, routes, tasks};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use sqlx::postgres::PgPoolOptions;
use std::net::SocketAddr;
use std::time::Duration;
use tower_http::compression::CompressionLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

async fn static_file(
    Path(path): Path<String>,
    axum::extract::RawQuery(q): axum::extract::RawQuery,
    headers: axum::http::HeaderMap,
) -> Response {
    assets::serve(&path, q.as_deref(), &headers).await
}

/// Request bodies larger than this are refused unless a route allows more.
pub const DEFAULT_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// An avatar or banner from object storage (immutable: cached for a year).
async fn stored_image(
    State(app): State<app::App>,
    Path((kind, name)): Path<(String, String)>,
) -> Response {
    if !matches!(kind.as_str(), "avatars" | "banners") || name.contains("..") || name.contains('/')
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    match app.storage.get(&format!("{kind}/{name}")).await {
        Ok(Some(d)) => {
            let ctype = mime_guess::from_path(&name).first_or_octet_stream();
            (
                [
                    (header::CONTENT_TYPE, ctype.essence_str().to_string()),
                    (
                        header::CACHE_CONTROL,
                        "public, max-age=31536000, immutable".into(),
                    ),
                    (header::CONTENT_LENGTH, d.size.to_string()),
                ],
                axum::body::Body::from_stream(d.stream),
            )
                .into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::warn!("reading {kind}/{name} from storage failed: {e:#}");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
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

pub async fn connect(cfg: &config::Config) -> anyhow::Result<sqlx::PgPool> {
    PgPoolOptions::new()
        .max_connections(cfg.db_max_connections)
        .min_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(300))
        // sqlx pings every connection before handing it out by default: an extra database round
        // trip for every query. Broken connections are detected when a query fails and dropped
        // anyway, and recycling connections every 30 minutes keeps them from going stale.
        .test_before_acquire(false)
        .max_lifetime(Duration::from_secs(1800))
        .connect_with(connect_options(cfg)?)
        .await
        .map_err(|e| doctor::hinted(e.into()))
}

fn connect_options(cfg: &config::Config) -> anyhow::Result<sqlx::postgres::PgConnectOptions> {
    use sqlx::ConnectOptions;
    let o: sqlx::postgres::PgConnectOptions = cfg.database_url.parse()?;
    // Statements are not logged one by one (that costs more than they do), slow ones are.
    Ok(o.log_statements(tracing::log::LevelFilter::Trace)
        .log_slow_statements(
            tracing::log::LevelFilter::Warn,
            Duration::from_millis(cfg.slow_query_ms),
        )
        .application_name("rbb"))
}

pub async fn migrate(db: &sqlx::PgPool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations")
        .run(db)
        .await
        .map_err(|e| doctor::hinted(e.into()))?;
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
    // Avatars and banners: straight from the directory when stored locally, streamed from the
    // object store otherwise.
    let uploads: Router<app::App> = if app.storage.is_local() {
        Router::new()
            .nest_service(
                "/uploads/avatars",
                tower_http::services::ServeDir::new(format!("{}/avatars", app.cfg.upload_dir)),
            )
            .nest_service(
                "/uploads/banners",
                tower_http::services::ServeDir::new(format!("{}/banners", app.cfg.upload_dir)),
            )
    } else {
        Router::new().route("/uploads/{kind}/{name}", get(stored_image))
    };
    Router::new()
        .route("/static/{*path}", get(static_file))
        .route("/css/theme/{tid}", get(theme_css))
        .route("/livez", get(observe::livez))
        .route("/readyz", get(observe::readyz))
        // Older name for the readiness check.
        .route("/healthz", get(observe::readyz))
        .merge(uploads)
        .merge(dynamic)
        .route_layer(axum::middleware::from_fn_with_state(
            app.clone(),
            observe::http_metrics,
        ))
        // Small bodies by default; the upload routes raise it for themselves (see `routes`).
        .layer(axum::extract::DefaultBodyLimit::max(DEFAULT_BODY_LIMIT))
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
        .layer(
            // One span per request. Only the path goes in it: query strings can carry one-time
            // codes and tokens, and headers carry cookies, so neither is ever logged.
            TraceLayer::new_for_http()
                .make_span_with(|req: &axum::http::Request<_>| {
                    let id = req
                        .headers()
                        .get("x-request-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    tracing::info_span!("request", method = %req.method(), path = %req.uri().path(), request_id = %id)
                })
                .on_request(())
                .on_response(
                    |resp: &Response, latency: Duration, _span: &tracing::Span| {
                        if resp.status().is_server_error() {
                            tracing::error!(status = resp.status().as_u16(), ms = latency.as_millis() as u64, "request failed");
                        } else {
                            tracing::debug!(status = resp.status().as_u16(), ms = latency.as_millis() as u64, "request");
                        }
                    },
                )
                .on_failure(()),
        )
        // A request id for every request (kept from a trusted proxy if it set one), echoed back
        // in the response and attached to the request's log span.
        .layer(tower_http::request_id::PropagateRequestIdLayer::x_request_id())
        .layer(tower_http::request_id::SetRequestIdLayer::x_request_id(
            tower_http::request_id::MakeRequestUuid,
        ))
        .with_state(app)
}

/// The internal admin listener: metrics and health checks, never exposed publicly.
fn admin_router(app: app::App) -> Router {
    Router::new()
        .route("/metrics", get(observe::metrics_handler))
        .route("/livez", get(observe::livez))
        .route("/readyz", get(observe::readyz))
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

/// Whether the board has been installed (`rbb install`).
pub async fn is_installed(db: &sqlx::PgPool) -> anyhow::Result<bool> {
    let has_table: bool = sqlx::query_scalar("SELECT to_regclass('public.usergroups') IS NOT NULL")
        .fetch_one(db)
        .await?;
    if !has_table {
        return Ok(false);
    }
    Ok(
        sqlx::query_scalar::<_, i32>("SELECT gid FROM usergroups LIMIT 1")
            .fetch_optional(db)
            .await?
            .is_some(),
    )
}

/// Run this process's roles until it is told to stop.
pub async fn serve(cfg: config::Config) -> anyhow::Result<()> {
    let roles = cfg.roles;
    let db = connect(&cfg).await?;
    if cfg.migrate_on_start {
        migrate(&db).await?;
    }
    if !is_installed(&db).await? {
        // Never create an administrator implicitly: that would put a credential in the logs
        // (or make a predictable one). Installing is an explicit step.
        anyhow::bail!(
            "the board is not installed. Run `rbb install --admin-user NAME --admin-email ADDRESS` \
             with RBB_ADMIN_PASSWORD set (or --admin-password), then start the server again"
        );
    }
    if cfg.migrate_on_start {
        install::upgrade(&db).await?;
    }
    tokio::fs::create_dir_all(format!("{}/avatars", cfg.upload_dir)).await?;
    tokio::fs::create_dir_all(format!("{}/attachments", cfg.upload_dir)).await?;
    observe::describe_all();
    let app = app::AppState::new(cfg.clone(), db).await?;
    // Every role follows cache invalidations and wake-ups from other nodes.
    app::spawn_listener(app.clone());
    observe::spawn_sampler(app.clone());
    let mut background: Vec<tokio::task::JoinHandle<()>> = vec![];
    if roles.web {
        app::spawn_flushers(app.clone());
        // Compress static assets and compile every template now, not on the first visitor's request.
        if cfg.dev_templates.is_none() {
            assets::precompress_all();
        }
        app.tpl.warm(app.cache().default_theme());
    }
    if roles.worker {
        background.push(mail::spawn_worker(app.clone()));
        background.push(crate::infra::outbox::spawn_worker(app.clone()));
    }
    if roles.scheduler && cfg.run_tasks {
        background.push(tasks::spawn_scheduler(app.clone()));
    }
    tracing::info!(
        web = roles.web,
        worker = roles.worker,
        scheduler = roles.scheduler,
        node = %app.node_id,
        "rbb {} starting",
        env!("CARGO_PKG_VERSION")
    );
    if let Some(addr) = cfg.admin_listen {
        let l = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!("admin endpoints on http://{addr} (/metrics, /livez, /readyz)");
        let r = admin_router(app.clone());
        let mut stop = app.shutdown.subscribe();
        tokio::spawn(async move {
            let _ = axum::serve(l, r)
                .with_graceful_shutdown(async move {
                    let _ = stop.wait_for(|s| *s).await;
                })
                .await;
        });
    }
    // On SIGTERM/Ctrl-C: report not ready (load balancers stop sending traffic), keep serving
    // for the drain period, then stop accepting, end live streams (browsers reconnect to
    // another node), and give in-flight requests and background batches time to finish.
    let signalled = {
        let app = app.clone();
        let drain = Duration::from_secs(cfg.shutdown_drain_secs);
        async move {
            shutdown_signal().await;
            let _ = app.shutdown.send(true);
            if !drain.is_zero() {
                tracing::info!("draining for {drain:?}");
                tokio::time::sleep(drain).await;
            }
        }
    };
    if roles.web {
        let router = build_router(app.clone());
        let listener = tokio::net::TcpListener::bind(cfg.listen).await?;
        tracing::info!("rbb listening on http://{}", cfg.listen);
        let server = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(signalled);
        let mut stopping = app.shutdown.subscribe();
        let grace = Duration::from_secs(cfg.shutdown_drain_secs + 15);
        tokio::select! {
            r = server => r?,
            _ = async {
                let _ = stopping.wait_for(|s| *s).await;
                tokio::time::sleep(grace).await;
            } => tracing::warn!("requests still open {grace:?} after shutdown began; exiting anyway"),
        }
    } else {
        signalled.await;
    }
    // Background loops stop between batches; give the current ones time to finish.
    let wait = futures::future::join_all(background);
    if tokio::time::timeout(Duration::from_secs(30), wait)
        .await
        .is_err()
    {
        tracing::warn!("background work still running 30 s after shutdown; exiting anyway");
    }
    if roles.web {
        // Persist buffered activity before exiting.
        if let Err(e) = app::flush_activity(&app).await {
            tracing::warn!("final activity flush failed: {e:#}");
        }
        if let Err(e) = app::flush_views(&app).await {
            tracing::warn!("final view flush failed: {e:#}");
        }
    }
    app.db.close().await;
    tracing::info!("stopped");
    Ok(())
}
