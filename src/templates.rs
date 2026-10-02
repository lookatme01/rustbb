//! Template engine setup. Default templates are embedded in the binary; admins can override any
//! template per theme (stored in the DB), and child themes inherit from parent themes.
//!
//! Template names are addressed as `"<theme id>/<name>"`; the theme prefix is carried through
//! `extends`/`include`/`import` so a theme override of `layout.html` applies everywhere.

use crate::cache::Cache;
use crate::util;
use arc_swap::ArcSwap;
use minijinja::value::{Kwargs, Value};
use minijinja::{Environment, Error, ErrorKind, State};
use rust_embed::RustEmbed;
use std::borrow::Cow;
use std::sync::Arc;

#[derive(RustEmbed)]
#[folder = "templates/"]
pub struct DefaultTemplates;

pub fn default_template(name: &str) -> Option<String> {
    DefaultTemplates::get(name).map(|f| String::from_utf8_lossy(&f.data).into_owned())
}

pub fn default_template_names() -> Vec<String> {
    let mut v: Vec<String> = DefaultTemplates::iter().map(|s| s.into_owned()).collect();
    v.sort();
    v
}

/// Resolve a template source for a theme, walking the theme inheritance chain.
pub fn resolve_source(
    cache: &Cache,
    theme: i32,
    name: &str,
    dev_dir: Option<&str>,
) -> Option<String> {
    let mut tid = theme;
    let mut guard = 0;
    while tid > 0 && guard < 16 {
        if let Some(s) = cache.templates.get(&(tid, name.to_string())) {
            return Some(s.clone());
        }
        tid = cache.theme(tid).map(|t| t.pid).unwrap_or(0);
        guard += 1;
    }
    if let Some(dir) = dev_dir
        && let Ok(s) = std::fs::read_to_string(format!("{dir}/{name}"))
    {
        return Some(s);
    }
    default_template(name)
}

pub fn build_env(cache: Arc<ArcSwap<Cache>>, dev_dir: Option<String>) -> Environment<'static> {
    let mut env = Environment::new();
    env.set_auto_escape_callback(|_| minijinja::AutoEscape::Html);
    env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
    env.set_trim_blocks(true);
    env.set_lstrip_blocks(true);
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Chainable);
    let c2 = cache.clone();
    env.set_loader(move |full: &str| {
        let (theme, name) = match full.split_once('/') {
            Some((t, n)) if t.chars().all(|c| c.is_ascii_digit()) => {
                (t.parse::<i32>().unwrap_or(0), n)
            }
            _ => (0, full),
        };
        let cache = c2.load();
        Ok(resolve_source(&cache, theme, name, dev_dir.as_deref()))
    });
    env.set_path_join_callback(join_path);

    env.add_filter("num", |v: Value| -> String {
        util::format_number(v.as_i64().unwrap_or(0))
    });
    env.add_filter("bytes", |v: Value| -> String {
        util::format_bytes(v.as_i64().unwrap_or(0))
    });
    env.add_filter("slug", |v: String| -> String { util::slugify(&v) });
    env.add_filter("truncate_chars", |v: String, n: Option<usize>| -> String {
        util::truncate_chars(&v, n.unwrap_or(50))
    });
    env.add_filter("urlenc", |v: String| -> String {
        percent_encoding::utf8_percent_encode(&v, percent_encoding::NON_ALPHANUMERIC).to_string()
    });
    env.add_filter("json", |v: Value| -> Result<Value, Error> {
        let s = serde_json::to_string(&v)
            .map_err(|e| Error::new(ErrorKind::InvalidOperation, e.to_string()))?;
        // safe for embedding inside <script>
        Ok(Value::from_safe_string(s.replace("</", "<\\/")))
    });
    env.add_filter("plural", |n: Value, one: String, many: String| -> String {
        if n.as_i64().unwrap_or(0) == 1 {
            one
        } else {
            many
        }
    });
    // Date formatting uses the viewer's timezone/format from the render context (`_tz`, `_df`, `_tf`).
    env.add_filter(
        "date",
        |state: &State, ts: Value, style: Option<String>| -> Value {
            let ts = ts.as_i64().unwrap_or(0);
            let tz = state
                .lookup("_tz")
                .and_then(|v| v.as_str().map(util::parse_tz))
                .unwrap_or(chrono_tz::UTC);
            let df = state
                .lookup("_df")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| "%m-%d-%Y".into());
            let tf = state
                .lookup("_tf")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| "%I:%M %p".into());
            let style = style.unwrap_or_else(|| "relative".into());
            let text = util::format_date(ts, tz, &df, &tf, &style);
            if style == "relative" && ts > 0 {
                let full = util::format_date(ts, tz, &df, &tf, "datetime");
                Value::from_safe_string(format!(
                    "<time datetime=\"{}\" title=\"{}\">{}</time>",
                    util::format_date(ts, tz, &df, &tf, "iso"),
                    util::escape_html(&full),
                    util::escape_html(&text)
                ))
            } else {
                Value::from(text)
            }
        },
    );
    env.add_function(
        "t",
        |state: &State, key: String, kwargs: Kwargs| -> Result<String, Error> {
            let lang = state
                .lookup("_lang")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_default();
            let mut s = crate::i18n::translate(&lang, &key);
            for k in kwargs.args() {
                let v: Value = kwargs.get(k)?;
                s = s.replace(&format!("{{{k}}}"), &v.to_string());
            }
            Ok(s)
        },
    );
    env.add_function("range_pages", |n: i64| -> Vec<i64> {
        (1..=n.max(0)).collect()
    });
    env.add_function("url_forum", |fid: i64, name: Option<String>| -> String {
        url_forum(fid, name.as_deref())
    });
    env.add_function(
        "url_thread",
        |tid: i64, subject: Option<String>| -> String { url_thread(tid, subject.as_deref()) },
    );
    env.add_function("url_user", |uid: i64, name: Option<String>| -> String {
        url_user(uid, name.as_deref())
    });
    env.add_function("now", util::now);
    env.add_function("asset", |path: String| -> String {
        crate::assets::url(&path)
    });
    env
}

fn join_path<'s>(name: &'s str, parent: &'s str) -> Cow<'s, str> {
    let has_theme = name
        .split_once('/')
        .map(|(t, _)| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(false);
    if has_theme {
        return Cow::Borrowed(name);
    }
    match parent.split_once('/') {
        Some((t, _)) if t.chars().all(|c| c.is_ascii_digit()) => Cow::Owned(format!("{t}/{name}")),
        _ => Cow::Borrowed(name),
    }
}

pub fn url_forum(fid: i64, name: Option<&str>) -> String {
    match name.map(util::slugify).filter(|s| !s.is_empty()) {
        Some(s) => format!("/forum/{fid}-{s}"),
        None => format!("/forum/{fid}"),
    }
}
pub fn url_thread(tid: i64, subject: Option<&str>) -> String {
    match subject.map(util::slugify).filter(|s| !s.is_empty()) {
        Some(s) => format!("/thread/{tid}-{s}"),
        None => format!("/thread/{tid}"),
    }
}
pub fn url_user(uid: i64, name: Option<&str>) -> String {
    match name.map(util::slugify).filter(|s| !s.is_empty()) {
        Some(s) => format!("/user/{uid}-{s}"),
        None => format!("/user/{uid}"),
    }
}

pub struct Templates {
    pub env: ArcSwap<Environment<'static>>,
    cache: Arc<ArcSwap<Cache>>,
    dev_dir: Option<String>,
}

impl Templates {
    pub fn new(cache: Arc<ArcSwap<Cache>>, dev_dir: Option<String>) -> Self {
        let env = build_env(cache.clone(), dev_dir.clone());
        Templates {
            env: ArcSwap::from_pointee(env),
            cache,
            dev_dir,
        }
    }
    /// Compile every page template for `theme` up front, so no visitor pays for compilation.
    pub fn warm(&self, theme: i32) {
        if self.dev_dir.is_some() {
            return;
        }
        let env = self.env.load();
        for name in default_template_names() {
            if name.ends_with(".html") {
                let _ = env.get_template(&format!("{theme}/{name}"));
            }
        }
    }

    /// Drop compiled templates (after theme/template edits).
    pub fn reset(&self) {
        self.env.store(Arc::new(build_env(
            self.cache.clone(),
            self.dev_dir.clone(),
        )));
    }
    pub fn render(&self, theme: i32, name: &str, ctx: Value) -> Result<String, Error> {
        if self.dev_dir.is_some() {
            // live-reload templates from disk in development
            let env = build_env(self.cache.clone(), self.dev_dir.clone());
            return env.get_template(&format!("{theme}/{name}"))?.render(ctx);
        }
        let env = self.env.load();
        env.get_template(&format!("{theme}/{name}"))?.render(ctx)
    }
}
