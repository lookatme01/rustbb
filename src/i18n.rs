//! gettext-style translations: templates call `t("English text")`; language packs in `lang/*.json`
//! map English source strings to translations. Missing keys fall back to English.

use rust_embed::RustEmbed;
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(RustEmbed)]
#[folder = "lang/"]
struct LangFiles;

pub struct Language {
    pub code: String,
    pub name: String,
    pub strings: HashMap<String, String>,
}

static LANGS: LazyLock<HashMap<String, Language>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    for f in LangFiles::iter() {
        let Some(code) = f.strip_suffix(".json") else {
            continue;
        };
        let Some(data) = LangFiles::get(&f) else {
            continue;
        };
        let Ok(mut map) = serde_json::from_slice::<HashMap<String, String>>(&data.data) else {
            tracing::warn!("invalid language pack {f}");
            continue;
        };
        let name = map.remove("_name").unwrap_or_else(|| code.to_string());
        m.insert(
            code.to_string(),
            Language {
                code: code.to_string(),
                name,
                strings: map,
            },
        );
    }
    m
});

pub fn translate(lang: &str, key: &str) -> String {
    if lang.is_empty() || lang == "en" {
        return key.to_string();
    }
    LANGS
        .get(lang)
        .and_then(|l| l.strings.get(key))
        .cloned()
        .unwrap_or_else(|| key.to_string())
}

/// (code, name) of installed languages, English first.
pub fn available() -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = LANGS
        .values()
        .map(|l| (l.code.clone(), l.name.clone()))
        .collect();
    v.sort();
    v.insert(0, ("en".into(), "English".into()));
    v
}

/// Pick a language from an Accept-Language header.
pub fn negotiate(accept: &str) -> Option<String> {
    for part in accept.split(',') {
        let code = part
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let base = code.split('-').next().unwrap_or("");
        if base == "en" {
            return None;
        }
        if LANGS.contains_key(base) {
            return Some(base.to_string());
        }
    }
    None
}
