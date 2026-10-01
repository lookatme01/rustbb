//! Plugin system: Rhai scripts in the plugins directory can hook named events, mirroring
//! MyBB's `$plugins->add_hook()`. Each script may define functions named after hooks, e.g.
//!
//! ```rhai
//! fn post_created(data) { log("new post " + data.pid); }
//! fn parse_message(html) { html.replace(":rust:", "🦀") }
//! ```
//!
//! Hooks run in a sandboxed engine with operation limits; filter hooks (`parse_message`)
//! receive and return a value.

use rhai::{AST, Dynamic, Engine};
use std::sync::Arc;

#[derive(Clone)]
pub struct Plugin {
    pub name: String,
    pub file: String,
    pub info: serde_json::Value,
    pub ast: Arc<AST>,
    pub hooks: Vec<String>,
}

#[derive(Clone, Default)]
pub struct Plugins {
    pub list: Arc<Vec<Plugin>>,
    engine: Option<Arc<Engine>>,
}

fn engine() -> Engine {
    let mut e = Engine::new();
    e.set_max_operations(200_000);
    e.set_max_call_levels(32);
    e.set_max_string_size(1_000_000);
    e.set_max_array_size(10_000);
    e.set_max_map_size(10_000);
    e.on_print(|s| tracing::info!(target: "plugin", "{s}"));
    e.register_fn("log", |s: &str| tracing::info!(target: "plugin", "{s}"));
    e
}

impl Plugins {
    pub fn load(dir: &str) -> Plugins {
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
                            e.call_fn::<Dynamic>(&mut rhai::Scope::new(), &ast, "info", ())
                                .ok()
                                .and_then(|d| {
                                    rhai::serde::from_dynamic::<serde_json::Value>(&d).ok()
                                })
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
        }
    }

    /// Fire-and-forget action hook.
    pub fn run_hook(&self, hook: &str, data: serde_json::Value) {
        let Some(e) = &self.engine else { return };
        for p in self
            .list
            .iter()
            .filter(|p| p.hooks.iter().any(|h| h == hook))
        {
            let arg = rhai::serde::to_dynamic(&data).unwrap_or_default();
            if let Err(err) = e.call_fn::<Dynamic>(&mut rhai::Scope::new(), &p.ast, hook, (arg,)) {
                tracing::warn!("plugin {} hook {hook} failed: {err}", p.name);
            }
        }
    }

    /// Filter hook for strings (e.g. `parse_message`): each plugin transforms the value.
    pub fn filter_string(&self, hook: &str, mut value: String) -> String {
        let Some(e) = &self.engine else { return value };
        for p in self
            .list
            .iter()
            .filter(|p| p.hooks.iter().any(|h| h == hook))
        {
            match e.call_fn::<String>(&mut rhai::Scope::new(), &p.ast, hook, (value.clone(),)) {
                Ok(v) => value = v,
                Err(err) => tracing::warn!("plugin {} filter {hook} failed: {err}", p.name),
            }
        }
        value
    }

    pub fn has_hook(&self, hook: &str) -> bool {
        self.list.iter().any(|p| p.hooks.iter().any(|h| h == hook))
    }
}
