//! Per-request profiling for administrators.
//!
//! sqlx emits a `sqlx::query` tracing event for every statement. While a request is being
//! profiled, a task-local collector is in scope and [`QueryLayer`] records those events into
//! it; the filter only enables the events when a collector exists, so ordinary requests pay
//! nothing. The context middleware then injects a summary panel into the HTML page.

use crate::util::escape_html;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer};

/// Marker the layout leaves where the panel goes.
pub const PLACEHOLDER: &str = "<!--rbb:debug-->";

#[derive(Clone, Debug)]
pub struct QueryRec {
    pub sql: String,
    pub ms: f64,
    pub rows: u64,
    /// Milliseconds since the request started.
    pub at: f64,
}

#[derive(Debug)]
pub struct Profile {
    pub start: Instant,
    pub queries: Vec<QueryRec>,
    pub render_ms: f64,
    pub template: String,
}

pub type Handle = Arc<Mutex<Profile>>;

tokio::task_local! {
    pub static PROFILE: Handle;
}

pub fn new_profile() -> Handle {
    Arc::new(Mutex::new(Profile {
        start: Instant::now(),
        queries: Vec::new(),
        render_ms: 0.0,
        template: String::new(),
    }))
}

pub fn active() -> bool {
    PROFILE.try_with(|_| ()).is_ok()
}

/// Record how long a template took to render.
pub fn record_render(name: &str, ms: f64) {
    let _ = PROFILE.try_with(|p| {
        let mut p = p.lock().unwrap();
        p.render_ms += ms;
        if p.template.is_empty() {
            p.template = name.to_string();
        }
    });
}

#[derive(Default)]
struct QueryVisitor {
    sql: Option<String>,
    summary: Option<String>,
    secs: Option<f64>,
    rows_returned: u64,
    rows_affected: u64,
}

impl Visit for QueryVisitor {
    fn record_str(&mut self, f: &Field, v: &str) {
        match f.name() {
            "db.statement" => self.sql = Some(v.to_string()),
            "summary" => self.summary = Some(v.to_string()),
            _ => {}
        }
    }
    fn record_f64(&mut self, f: &Field, v: f64) {
        if f.name() == "elapsed_secs" {
            self.secs = Some(v);
        }
    }
    fn record_u64(&mut self, f: &Field, v: u64) {
        match f.name() {
            "rows_returned" => self.rows_returned = v,
            "rows_affected" => self.rows_affected = v,
            _ => {}
        }
    }
    fn record_i64(&mut self, f: &Field, v: i64) {
        self.record_u64(f, v.max(0) as u64)
    }
    fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
        match f.name() {
            "db.statement" if self.sql.is_none() => {
                self.sql = Some(format!("{v:?}").trim_matches('"').replace("\\n", "\n"))
            }
            "summary" if self.summary.is_none() => {
                self.summary = Some(format!("{v:?}").trim_matches('"').to_string())
            }
            _ => {}
        }
    }
}

pub struct QueryLayer;

impl<S: Subscriber> Layer<S> for QueryLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != "sqlx::query" {
            return;
        }
        let _ = PROFILE.try_with(|p| {
            let mut v = QueryVisitor::default();
            event.record(&mut v);
            let sql = v
                .sql
                .filter(|s| !s.trim().is_empty())
                .or(v.summary)
                .unwrap_or_default();
            let mut p = p.lock().unwrap();
            let at = p.start.elapsed().as_secs_f64() * 1000.0;
            let ms = v.secs.unwrap_or(0.0) * 1000.0;
            p.queries.push(QueryRec {
                sql: normalize(&sql),
                ms,
                rows: v.rows_returned.max(v.rows_affected),
                at: at - ms,
            });
        });
    }
}

/// Only enable sqlx statement events while a request is being profiled.
pub fn filter<S>() -> tracing_subscriber::filter::DynFilterFn<
    S,
    impl Fn(&tracing::Metadata<'_>, &Context<'_, S>) -> bool,
> {
    tracing_subscriber::filter::DynFilterFn::new(|meta, _| {
        meta.target() == "sqlx::query" && active()
    })
}

fn normalize(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub struct Extra {
    pub route: String,
    pub status: u16,
    pub bytes: usize,
    pub pool_size: u32,
    pub pool_idle: usize,
    pub node: String,
    pub uptime: String,
    pub cache_hint: String,
}

/// Build the panel HTML. Styling lives in rbb.css (`.devbar`).
pub fn render(p: &Profile, x: &Extra) -> String {
    let total = p.start.elapsed().as_secs_f64() * 1000.0;
    let db: f64 = p.queries.iter().map(|q| q.ms).sum::<f64>().max(0.0);
    let mut dup: HashMap<&str, usize> = HashMap::new();
    for q in &p.queries {
        *dup.entry(q.sql.as_str()).or_default() += 1;
    }
    let repeated: Vec<(&str, usize)> = {
        let mut v: Vec<_> = dup
            .iter()
            .filter(|(_, n)| **n > 1)
            .map(|(s, n)| (*s, *n))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v
    };
    let slowest = p.queries.iter().map(|q| q.ms).fold(0.0, f64::max);
    let grade = if total < 50.0 {
        "fast"
    } else if total < 200.0 {
        "ok"
    } else {
        "slow"
    };
    let mut h = String::with_capacity(4096);
    let _ = write!(
        h,
        r#"<details class="devbar devbar-{grade}" id="devbar"><summary><span class="devbar-pill"><b>{total:.1}</b> ms</span><span class="devbar-pill"><b>{nq}</b> {qword} · {db:.1} ms</span><span class="devbar-pill">render <b>{render:.1}</b> ms</span><span class="devbar-pill">{kb:.1} KB</span>{warn}<span class="devbar-route">{method_route}</span></summary><div class="devbar-body">"#,
        nq = p.queries.len(),
        qword = if p.queries.len() == 1 {
            "query"
        } else {
            "queries"
        },
        render = p.render_ms,
        kb = x.bytes as f64 / 1024.0,
        warn = if repeated.is_empty() {
            String::new()
        } else {
            format!(
                r#"<span class="devbar-pill devbar-warn">{} repeated</span>"#,
                repeated.len()
            )
        },
        method_route = escape_html(&x.route),
    );
    let _ = write!(
        h,
        r#"<dl class="devbar-facts"><div><dt>Status</dt><dd>{}</dd></div><div><dt>Template</dt><dd>{}</dd></div><div><dt>Handler time</dt><dd>{:.1} ms</dd></div><div><dt>Slowest query</dt><dd>{:.2} ms</dd></div><div><dt>DB pool</dt><dd>{} open · {} idle</dd></div><div><dt>Node</dt><dd>{}</dd></div><div><dt>Uptime</dt><dd>{}</dd></div><div><dt>Version</dt><dd>{}</dd></div><div><dt>Cache</dt><dd>{}</dd></div></dl>"#,
        x.status,
        escape_html(if p.template.is_empty() {
            "—"
        } else {
            &p.template
        }),
        (total - p.render_ms).max(0.0),
        slowest,
        x.pool_size,
        x.pool_idle,
        escape_html(&x.node),
        escape_html(&x.uptime),
        env!("CARGO_PKG_VERSION"),
        escape_html(&x.cache_hint),
    );
    if !repeated.is_empty() {
        h.push_str(r#"<p class="devbar-note">Repeated statements often mean an N+1 pattern:</p><ul class="devbar-dups">"#);
        for (sql, n) in repeated.iter().take(5) {
            let _ = write!(
                h,
                "<li><b>×{n}</b> <code>{}</code></li>",
                escape_html(&truncate(sql, 160))
            );
        }
        h.push_str("</ul>");
    }
    h.push_str(r#"<table class="devbar-q"><thead><tr><th>#</th><th>at</th><th>ms</th><th>rows</th><th>statement</th></tr></thead><tbody>"#);
    for (i, q) in p.queries.iter().enumerate() {
        let hot = if q.ms >= 10.0 { " class=\"hot\"" } else { "" };
        let _ = write!(
            h,
            "<tr{hot}><td>{}</td><td>{:.1}</td><td>{:.2}</td><td>{}</td><td><code>{}</code></td></tr>",
            i + 1,
            q.at.max(0.0),
            q.ms,
            q.rows,
            escape_html(&truncate(&q.sql, 1200))
        );
    }
    if p.queries.is_empty() {
        h.push_str(r#"<tr><td colspan="5">No database queries: this page was served from memory.</td></tr>"#);
    }
    h.push_str("</tbody></table><p class=\"devbar-note\">Visible to administrators only. Turn it off under Settings → General → Admin debug panel. Session and permission lookups that run before the page handler aren't listed.</p></div></details>");
    h
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}
