//! Plugin system: Rhai scripts in the plugins directory can hook named events, mirroring
//! MyBB's `$plugins->add_hook()`. Each script may define functions named after hooks, e.g.
//!
//! ```rhai
//! fn post_created(data) { log("new post " + data.pid); }
//! fn parse_message(html) { html.replace(":rust:", "🦀") }
//! ```
//!
//! Hooks run in a sandboxed engine with operation, size and time limits. Action hooks run on
//! blocking threads (a few at a time), usually from the outbox after the change that triggered
//! them commits; filter hooks (`parse_message`) receive and return a value while a page renders.
//! A plugin that keeps failing or overrunning is switched off for a while (circuit breaker), and
//! the HTML filters produce is sanitized unless plugins are configured as fully trusted.

use dashmap::DashMap;
use rhai::{AST, Dynamic, Engine};
use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Time budget for one plugin's action hook.
const ACTION_BUDGET: Duration = Duration::from_secs(2);
/// Time budget for one plugin's filter (runs while a page is being rendered).
const FILTER_BUDGET: Duration = Duration::from_millis(100);
/// Consecutive failures (errors or overruns) that open a plugin's circuit breaker…
const BREAKER_THRESHOLD: u32 = 5;
/// …and how long it stays open (the plugin's hooks are skipped meanwhile).
const BREAKER_COOLDOWN: Duration = Duration::from_secs(60);
/// Action hooks running at once (each on a blocking thread).
const MAX_CONCURRENT_ACTIONS: usize = 2;
/// Plugin log lines are cut to this length.
const MAX_LOG_LINE: usize = 1000;

#[derive(Clone)]
pub struct Plugin {
    pub name: String,
    pub file: String,
    pub info: serde_json::Value,
    pub ast: Arc<AST>,
    pub hooks: Vec<String>,
}

#[derive(Default)]
struct Breaker {
    failures: u32,
    open_until: Option<Instant>,
}

/// Shared runtime limits for plugin execution.
struct Guard {
    actions: tokio::sync::Semaphore,
    breakers: DashMap<String, Breaker>,
}

impl Default for Guard {
    fn default() -> Self {
        Guard {
            actions: tokio::sync::Semaphore::new(MAX_CONCURRENT_ACTIONS),
            breakers: DashMap::new(),
        }
    }
}

impl Guard {
    fn allowed(&self, plugin: &str) -> bool {
        match self.breakers.get(plugin).and_then(|b| b.open_until) {
            Some(t) => Instant::now() >= t,
            None => true,
        }
    }

    fn record(&self, plugin: &str, hook: &str, ok: bool, elapsed: Duration) {
        let labels = [("plugin", plugin), ("hook", hook)];
        crate::infra::metrics::observe("rbb_plugin_hook_seconds", &labels, elapsed.as_secs_f64());
        let mut b = self.breakers.entry(plugin.to_string()).or_default();
        if ok {
            b.failures = 0;
            b.open_until = None;
            return;
        }
        crate::infra::metrics::counter_with("rbb_plugin_failures_total", &labels, 1);
        b.failures += 1;
        if b.failures >= BREAKER_THRESHOLD {
            tracing::error!(
                plugin,
                hook,
                "plugin keeps failing; skipping its hooks for {BREAKER_COOLDOWN:?}"
            );
            crate::infra::metrics::counter_with(
                "rbb_plugin_breaker_open_total",
                &[("plugin", plugin)],
                1,
            );
            b.open_until = Some(Instant::now() + BREAKER_COOLDOWN);
            b.failures = 0;
        }
    }
}

#[derive(Clone, Default)]
pub struct Plugins {
    pub list: Arc<Vec<Plugin>>,
    engine: Option<Arc<Engine>>,
    guard: Arc<Guard>,
    /// Plugins are fully trusted: their filter output is not sanitized (`RBB_PLUGINS_TRUSTED`).
    trusted: bool,
}

thread_local! {
    /// When the plugin call running on this thread must stop.
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Run `f` with a deadline enforced by the engine's progress callback.
fn with_deadline<T>(budget: Duration, f: impl FnOnce() -> T) -> T {
    DEADLINE.with(|d| d.set(Some(Instant::now() + budget)));
    let r = f();
    DEADLINE.with(|d| d.set(None));
    r
}

fn clip(s: &str) -> &str {
    match s.char_indices().nth(MAX_LOG_LINE) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn engine() -> Engine {
    let mut e = Engine::new();
    e.set_max_operations(200_000);
    e.set_max_call_levels(32);
    e.set_max_expr_depths(64, 32);
    e.set_max_string_size(1_000_000);
    e.set_max_array_size(10_000);
    e.set_max_map_size(10_000);
    e.on_progress(|ops| {
        if ops % 512 == 0
            && DEADLINE
                .with(|d| d.get())
                .is_some_and(|t| Instant::now() >= t)
        {
            Some(Dynamic::from("time budget exceeded"))
        } else {
            None
        }
    });
    e.on_print(|s| tracing::info!(target: "plugin", "{}", clip(s)));
    e.register_fn(
        "log",
        |s: &str| tracing::info!(target: "plugin", "{}", clip(s)),
    );
    e
}

/// Make plugin-produced post HTML safe: only the markup the MyCode parser itself produces.
pub fn sanitize_html(html: &str) -> String {
    use std::collections::{HashMap, HashSet};
    static CLEANER: std::sync::LazyLock<ammonia::Builder<'static>> =
        std::sync::LazyLock::new(|| {
            let tags: HashSet<&str> = [
                "a",
                "b",
                "strong",
                "i",
                "em",
                "u",
                "s",
                "del",
                "ins",
                "sub",
                "sup",
                "span",
                "div",
                "p",
                "br",
                "hr",
                "ol",
                "ul",
                "li",
                "blockquote",
                "cite",
                "pre",
                "code",
                "img",
                "iframe",
                "details",
                "summary",
                "mark",
                "time",
                "button",
                "table",
                "thead",
                "tbody",
                "tr",
                "th",
                "td",
                "h1",
                "h2",
                "h3",
                "h4",
                "h5",
                "h6",
            ]
            .into_iter()
            .collect();
            let mut attrs: HashMap<&str, HashSet<&str>> = HashMap::new();
            attrs.insert("a", ["href", "title", "target"].into_iter().collect());
            attrs.insert(
                "img",
                ["src", "alt", "title", "width", "height", "loading"]
                    .into_iter()
                    .collect(),
            );
            attrs.insert(
                "iframe",
                [
                    "src",
                    "loading",
                    "allowfullscreen",
                    "referrerpolicy",
                    "sandbox",
                    "width",
                    "height",
                ]
                .into_iter()
                .collect(),
            );
            attrs.insert("time", ["datetime", "data-ts"].into_iter().collect());
            attrs.insert("ol", ["type", "start"].into_iter().collect());
            attrs.insert("button", ["type"].into_iter().collect());
            let mut b = ammonia::Builder::default();
            b.tags(tags)
                .tag_attributes(attrs)
                .generic_attributes(["class", "style", "title"].into_iter().collect())
                .link_rel(Some("nofollow ugc noopener"))
                .url_schemes(["http", "https", "mailto"].into_iter().collect())
                .attribute_filter(|el, attr, value| match (el, attr) {
                    // Embeds only from the video hosts the parser itself uses.
                    ("iframe", "src") => [
                        "https://www.youtube-nocookie.com/",
                        "https://player.vimeo.com/",
                        "https://www.dailymotion.com/",
                        "https://player.twitch.tv/",
                    ]
                    .iter()
                    .any(|p| value.starts_with(p))
                    .then(|| value.into()),
                    ("iframe", "sandbox") => Some(
                        "allow-scripts allow-same-origin allow-presentation allow-popups".into(),
                    ),
                    (_, "style") => safe_style(value).map(Into::into),
                    _ => Some(value.into()),
                });
            b
        });
    CLEANER.clean(html).to_string()
}

/// Keep only the declarations the parser emits (colours, sizes, fonts, alignment).
fn safe_style(v: &str) -> Option<String> {
    let kept: Vec<String> = v
        .split(';')
        .filter_map(|d| {
            let (k, val) = d.split_once(':')?;
            let (k, val) = (k.trim().to_ascii_lowercase(), val.trim());
            let ok_prop = matches!(
                k.as_str(),
                "color" | "font-size" | "font-family" | "text-align"
            );
            let ok_val = !val.is_empty()
                && val.len() < 100
                && val
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || " #.,%-'\"".contains(c));
            (ok_prop && ok_val).then(|| format!("{k}: {val}"))
        })
        .collect();
    (!kept.is_empty()).then(|| kept.join("; "))
}

impl Plugins {
    pub fn load(dir: &str) -> Plugins {
        Self::load_with(dir, false)
    }

    /// Load plugins; `trusted` turns off sanitizing of their filter output.
    pub fn load_with(dir: &str, trusted: bool) -> Plugins {
        let e = engine();
        let mut list = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            let mut paths: Vec<_> = rd
                .flatten()
                .map(|d| d.path())
                .filter(|p| p.extension().map(|x| x == "rhai").unwrap_or(false))
                .collect();
            paths.sort();
            for p in paths {
                let file = p.display().to_string();
                match e.compile_file(p.clone()) {
                    Ok(ast) => {
                        let hooks: Vec<String> =
                            ast.iter_functions().map(|f| f.name.to_string()).collect();
                        let name = p
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let info = if hooks.iter().any(|h| h == "info") {
                            with_deadline(ACTION_BUDGET, || {
                                e.call_fn::<Dynamic>(&mut rhai::Scope::new(), &ast, "info", ())
                            })
                            .ok()
                            .and_then(|d| rhai::serde::from_dynamic::<serde_json::Value>(&d).ok())
                            .unwrap_or_default()
                        } else {
                            serde_json::json!({})
                        };
                        tracing::info!("loaded plugin {name} (hooks: {})", hooks.join(", "));
                        list.push(Plugin {
                            name,
                            file,
                            info,
                            ast: Arc::new(ast),
                            hooks,
                        });
                    }
                    Err(err) => tracing::error!("plugin {file} failed to compile: {err}"),
                }
            }
        }
        Plugins {
            list: Arc::new(list),
            engine: Some(Arc::new(e)),
            guard: Arc::new(Guard::default()),
            trusted,
        }
    }

    /// Compile every plugin script in `dir` without running it: (file, error if it doesn't compile).
    pub fn check_dir(dir: &str) -> Vec<(String, Option<String>)> {
        let e = engine();
        let mut out: Vec<(String, Option<String>)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|d| d.path())
            .filter(|p| p.extension().map(|x| x == "rhai").unwrap_or(false))
            .map(|p| {
                (
                    p.display().to_string(),
                    e.compile_file(p).err().map(|err| err.to_string()),
                )
            })
            .collect();
        out.sort();
        out
    }

    /// Fire-and-forget action hook: runs in the background, never on the caller's thread.
    pub fn run_hook(&self, hook: &str, data: serde_json::Value) {
        if !self.has_hook(hook) {
            return;
        }
        let me = self.clone();
        let hook = hook.to_string();
        tokio::spawn(async move {
            if let Err(e) = me.run_hook_async(&hook, data).await {
                tracing::warn!("{e:#}");
            }
        });
    }

    /// Run an action hook on a blocking thread (at most `MAX_CONCURRENT_ACTIONS` at once), each
    /// plugin within its time budget. Returns an error if any plugin failed, so a durable caller
    /// (the outbox) can retry; plugins whose breaker is open are skipped.
    pub async fn run_hook_async(&self, hook: &str, data: serde_json::Value) -> anyhow::Result<()> {
        let Some(e) = self.engine.clone() else {
            return Ok(());
        };
        let targets: Vec<Plugin> = self
            .list
            .iter()
            .filter(|p| p.hooks.iter().any(|h| h == hook))
            .cloned()
            .collect();
        if targets.is_empty() {
            return Ok(());
        }
        let _permit = self.guard.actions.acquire().await?;
        let guard = self.guard.clone();
        let hook = hook.to_string();
        let failed = tokio::task::spawn_blocking(move || {
            let mut failed = vec![];
            for p in targets {
                if !guard.allowed(&p.name) {
                    continue;
                }
                let arg = rhai::serde::to_dynamic(&data).unwrap_or_default();
                let t0 = Instant::now();
                let r = with_deadline(ACTION_BUDGET, || {
                    e.call_fn::<Dynamic>(&mut rhai::Scope::new(), &p.ast, &hook, (arg,))
                });
                guard.record(&p.name, &hook, r.is_ok(), t0.elapsed());
                if let Err(err) = r {
                    failed.push(format!("plugin {} hook {hook} failed: {err}", p.name));
                }
            }
            failed
        })
        .await?;
        if failed.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(failed.join("; ")))
        }
    }

    /// Filter hook for post HTML (`parse_message`): each plugin transforms the value within a
    /// small time budget. A plugin that fails, overruns or returns an oversized result is
    /// skipped (its input passes through). Unless plugins are trusted, the final HTML is
    /// sanitized down to the markup the parser itself produces.
    pub fn filter_string(&self, hook: &str, mut value: String) -> String {
        let Some(e) = &self.engine else { return value };
        let mut changed = false;
        for p in self
            .list
            .iter()
            .filter(|p| p.hooks.iter().any(|h| h == hook))
        {
            if !self.guard.allowed(&p.name) {
                continue;
            }
            let t0 = Instant::now();
            let limit = value.len() * 4 + 64 * 1024;
            let r = with_deadline(FILTER_BUDGET, || {
                e.call_fn::<String>(&mut rhai::Scope::new(), &p.ast, hook, (value.clone(),))
            });
            let r = match r {
                Ok(v) if v.len() > limit => Err(format!("output too large ({} bytes)", v.len())),
                Ok(v) => Ok(v),
                Err(err) => Err(err.to_string()),
            };
            self.guard.record(&p.name, hook, r.is_ok(), t0.elapsed());
            match r {
                Ok(v) => {
                    changed |= v != value;
                    value = v;
                }
                Err(err) => tracing::warn!("plugin {} filter {hook} failed: {err}", p.name),
            }
        }
        if changed && !self.trusted {
            sanitize_html(&value)
        } else {
            value
        }
    }

    pub fn has_hook(&self, hook: &str) -> bool {
        self.list.iter().any(|p| p.hooks.iter().any(|h| h == hook))
    }
}
