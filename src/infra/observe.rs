//! Operational visibility: request metrics, health endpoints and a background sampler.
//!
//! * `/livez` — the process is up (no dependencies checked; restart only if this fails).
//! * `/readyz` — this node should get traffic: the database answers within a short timeout and
//!   the node is not shutting down. Load balancers stop routing to a node as soon as it starts
//!   draining.
//! * `/metrics` — Prometheus text format, served on the admin listener (`RBB_ADMIN_LISTEN`),
//!   not on the public port.
//!
//! Request metrics are labelled by route template (`/thread/{tid}`), method and status class,
//! never by raw path. Query counts and durations come from a sample of requests
//! (`RBB_QUERY_SAMPLE_RATE`, default 1%), because recording every statement costs more than
//! the queries it measures.

use crate::app::App;
use crate::infra::metrics;
use axum::extract::{MatchedPath, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::time::{Duration, Instant};

pub fn describe_all() {
    for (n, h) in [
        (
            "rbb_http_requests_total",
            "HTTP requests by route template, method and status class.",
        ),
        (
            "rbb_http_request_seconds",
            "HTTP request latency by route template.",
        ),
        ("rbb_http_in_flight", "HTTP requests being handled."),
        (
            "rbb_db_queries_per_request",
            "Statements per request (sampled requests).",
        ),
        (
            "rbb_db_query_seconds",
            "Statement latency (sampled requests).",
        ),
        ("rbb_db_pool_connections", "Open database connections."),
        ("rbb_db_pool_idle", "Idle database connections."),
        (
            "rbb_db_pool_acquire_seconds",
            "Time to get a connection from the pool (probe).",
        ),
        (
            "rbb_runtime_lag_seconds",
            "Worst scheduling delay of the async runtime over the last interval.",
        ),
        ("rbb_outbox_pending", "Outbox jobs waiting to run."),
        ("rbb_outbox_dead", "Outbox jobs that failed permanently."),
        (
            "rbb_outbox_oldest_seconds",
            "Age of the oldest pending outbox job.",
        ),
        ("rbb_mail_pending", "Emails waiting to be sent."),
        ("rbb_mail_dead", "Emails that failed permanently."),
        (
            "rbb_mail_oldest_seconds",
            "Age of the oldest pending email.",
        ),
        (
            "rbb_live_streams",
            "Open live-update (SSE) streams on this node.",
        ),
        (
            "rbb_page_cache_entries",
            "Guest pages in this node's page cache.",
        ),
        (
            "rbb_page_cache_requests_total",
            "Guest page cache lookups by result.",
        ),
        (
            "rbb_ratelimited_total",
            "Requests refused by a rate limit, by limit.",
        ),
        ("rbb_build_info", "Build information."),
    ] {
        metrics::describe(n, h);
    }
    metrics::gauge_with(
        "rbb_build_info",
        &[("version", env!("CARGO_PKG_VERSION"))],
        1.0,
    );
}

fn status_class(s: StatusCode) -> &'static str {
    match s.as_u16() {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    }
}

/// Record request count, latency and (for a sample) database work per route.
pub async fn http_metrics(State(app): State<App>, req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".into());
    // Live streams stay open for minutes; their latency is not a request latency.
    if route == "/live" {
        return next.run(req).await;
    }
    let method = req.method().as_str().to_owned();
    let start = Instant::now();
    metrics::gauge_add("rbb_http_in_flight", &[], 1.0);
    let sample = app.cfg.query_sample_rate > 0.0
        && !crate::debugbar::active()
        && rand::random::<f64>() < app.cfg.query_sample_rate;
    let (resp, profile) = if sample {
        let p = crate::debugbar::new_profile();
        let r = crate::debugbar::PROFILE
            .scope(p.clone(), next.run(req))
            .await;
        (r, Some(p))
    } else {
        (next.run(req).await, None)
    };
    metrics::gauge_add("rbb_http_in_flight", &[], -1.0);
    let secs = start.elapsed().as_secs_f64();
    let status = status_class(resp.status());
    metrics::counter_with(
        "rbb_http_requests_total",
        &[("route", &route), ("method", &method), ("status", status)],
        1,
    );
    metrics::observe("rbb_http_request_seconds", &[("route", &route)], secs);
    if let Some(p) = profile {
        let p = p.lock().unwrap();
        metrics::observe_in(
            "rbb_db_queries_per_request",
            &[("route", &route)],
            p.queries.len() as f64,
            metrics::COUNTS,
        );
        for q in &p.queries {
            metrics::observe("rbb_db_query_seconds", &[], q.ms / 1000.0);
        }
    }
    resp
}

pub async fn livez() -> Response {
    (StatusCode::OK, "ok").into_response()
}

pub async fn readyz(State(app): State<App>) -> Response {
    if *app.shutdown.borrow() {
        return (StatusCode::SERVICE_UNAVAILABLE, "shutting down").into_response();
    }
    let probe = tokio::time::timeout(
        Duration::from_secs(2),
        sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&app.db),
    )
    .await;
    match probe {
        Ok(Ok(_)) => (StatusCode::OK, "ready").into_response(),
        Ok(Err(e)) => {
            // Don't reveal connection strings or server details to unauthenticated callers.
            tracing::warn!("readiness check failed: {e}");
            (StatusCode::SERVICE_UNAVAILABLE, "database unavailable").into_response()
        }
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "database slow").into_response(),
    }
}

pub async fn metrics_handler() -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        metrics::render(),
    )
        .into_response()
}

/// Sample pool, queue, stream and runtime gauges until shutdown.
pub fn spawn_sampler(app: App) {
    // Runtime lag: how late a 100 ms timer fires is how long ready tasks wait for a thread.
    let a = app.clone();
    tokio::spawn(async move {
        let mut worst = 0f64;
        let mut since = Instant::now();
        let mut stop = a.shutdown.subscribe();
        loop {
            let t0 = Instant::now();
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                _ = stop.wait_for(|s| *s) => break,
            }
            worst = worst.max((t0.elapsed().as_secs_f64() - 0.1).max(0.0));
            if since.elapsed() >= Duration::from_secs(10) {
                metrics::gauge("rbb_runtime_lag_seconds", worst);
                worst = 0.0;
                since = Instant::now();
            }
        }
    });
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        let mut stop = app.shutdown.subscribe();
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = stop.wait_for(|s| *s) => break,
            }
            metrics::gauge("rbb_db_pool_connections", app.db.size() as f64);
            metrics::gauge("rbb_db_pool_idle", app.db.num_idle() as f64);
            let t0 = Instant::now();
            if let Ok(Ok(c)) = tokio::time::timeout(Duration::from_secs(5), app.db.acquire()).await
            {
                metrics::observe(
                    "rbb_db_pool_acquire_seconds",
                    &[],
                    t0.elapsed().as_secs_f64(),
                );
                drop(c);
            }
            metrics::gauge("rbb_live_streams", app.live.subscribers() as f64);
            metrics::gauge("rbb_page_cache_entries", app.page_cache.len() as f64);
            if let Ok((p, d, age)) = crate::infra::outbox::stats(&app.db).await {
                metrics::gauge("rbb_outbox_pending", p as f64);
                metrics::gauge("rbb_outbox_dead", d as f64);
                metrics::gauge("rbb_outbox_oldest_seconds", age);
            }
            if let Ok((p, d, age)) = crate::mail::stats(&app.db).await {
                metrics::gauge("rbb_mail_pending", p as f64);
                metrics::gauge("rbb_mail_dead", d as f64);
                metrics::gauge("rbb_mail_oldest_seconds", age);
            }
        }
    });
}

/// `rbb healthcheck`: GET a health URL with a plain TCP connection (no curl in the image).
/// Returns the process exit code: 0 when the endpoint answers 200.
pub async fn healthcheck(url: &str) -> i32 {
    match probe(url).await {
        Ok(200) => 0,
        Ok(code) => {
            eprintln!("{url}: HTTP {code}");
            1
        }
        Err(e) => {
            eprintln!("{url}: {e}");
            1
        }
    }
}

async fn probe(url: &str) -> anyhow::Result<u16> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let u = url::Url::parse(url)?;
    anyhow::ensure!(u.scheme() == "http", "only http:// URLs are supported");
    let host = u.host_str().ok_or_else(|| anyhow::anyhow!("no host"))?;
    let port = u.port().unwrap_or(80);
    let path = match u.query() {
        Some(q) => format!("{}?{q}", u.path()),
        None => u.path().to_string(),
    };
    let fut = async {
        let mut s = tokio::net::TcpStream::connect((host, port)).await?;
        s.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: rbb-healthcheck\r\n\r\n")
                .as_bytes(),
        )
        .await?;
        let mut buf = vec![0u8; 64];
        let n = s.read(&mut buf).await?;
        let line = String::from_utf8_lossy(&buf[..n]);
        let code = line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| anyhow::anyhow!("not an HTTP response"))?;
        anyhow::Ok(code)
    };
    tokio::time::timeout(Duration::from_secs(3), fut)
        .await
        .map_err(|_| anyhow::anyhow!("timed out"))?
}
