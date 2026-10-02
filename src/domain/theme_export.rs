//! Theme export files: parsing and validation of untrusted input for the theme importer.
//!
//! An import may only contain what an export does: a name, a flat object of display properties,
//! a stylesheet and templates that override built-in ones. Everything is size-limited, unknown
//! templates are dropped, and templates must compile, so a bad file is refused instead of
//! breaking pages.

use serde_json::Value;

pub const MAX_NAME: usize = 100;
pub const MAX_STYLESHEET: usize = 2 * 1024 * 1024;
pub const MAX_TEMPLATE: usize = 512 * 1024;
pub const MAX_PROPERTIES: usize = 200;
pub const MAX_PROPERTY_VALUE: usize = 10 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct ThemeExport {
    pub name: String,
    pub properties: serde_json::Map<String, Value>,
    pub stylesheet: String,
    /// (template name, source), only names of built-in templates.
    pub templates: Vec<(String, String)>,
}

/// Parse and validate an export. `known` says whether a template name exists.
pub fn parse(input: &str, known: impl Fn(&str) -> bool) -> Result<ThemeExport, String> {
    let data: Value =
        serde_json::from_str(input).map_err(|e| format!("Invalid theme file: {e}"))?;
    if data.get("rbb_theme").and_then(Value::as_i64) != Some(1) {
        return Err("This is not an rbb theme export.".into());
    }
    let name = data
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Imported theme")
        .trim();
    if name.is_empty() || name.chars().count() > MAX_NAME || name.chars().any(char::is_control) {
        return Err(format!(
            "The theme name must be 1 to {MAX_NAME} characters, without control characters."
        ));
    }
    let properties = match data.get("properties") {
        None | Some(Value::Null) => serde_json::Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err("Theme properties must be an object.".into()),
    };
    if properties.len() > MAX_PROPERTIES {
        return Err(format!(
            "A theme can have at most {MAX_PROPERTIES} properties."
        ));
    }
    for (k, v) in &properties {
        let ok = match v {
            Value::String(s) => s.len() <= MAX_PROPERTY_VALUE,
            Value::Bool(_) | Value::Number(_) | Value::Null => true,
            _ => false,
        };
        if !ok || k.len() > 100 {
            return Err(format!(
                "Theme property {:?} must be a short text, number or true/false.",
                k.chars().take(40).collect::<String>()
            ));
        }
    }
    let stylesheet = match data.get("stylesheet") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) if s.len() <= MAX_STYLESHEET => s.clone(),
        Some(Value::String(_)) => return Err("The stylesheet is too large.".into()),
        Some(_) => return Err("The stylesheet must be text.".into()),
    };
    let mut templates = vec![];
    if let Some(t) = data.get("templates") {
        let Some(map) = t.as_object() else {
            return Err("Templates must be an object of name → source.".into());
        };
        let env = minijinja::Environment::new();
        for (n, src) in map {
            if !known(n) {
                continue;
            }
            let Some(src) = src.as_str() else {
                return Err(format!("Template {n} must be text."));
            };
            if src.len() > MAX_TEMPLATE {
                return Err(format!("Template {n} is too large."));
            }
            env.template_from_str(src)
                .map_err(|e| format!("Template {n} does not compile: {e}"))?;
            templates.push((n.clone(), src.to_string()));
        }
    }
    templates.sort();
    Ok(ThemeExport {
        name: name.to_string(),
        properties,
        stylesheet,
        templates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(n: &str) -> bool {
        n == "index.html"
    }

    #[test]
    fn accepts_an_export() {
        let t = parse(
            r##"{"rbb_theme":1,"name":"Dusk","properties":{"brand":"#123"},"stylesheet":"a{}","templates":{"index.html":"{{ x }}","nope.html":"y"}}"##,
            known,
        )
        .unwrap();
        assert_eq!(t.name, "Dusk");
        assert_eq!(t.templates, vec![("index.html".into(), "{{ x }}".into())]);
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            "not json",
            r#"{"rbb_theme":2}"#,
            r#"{"rbb_theme":1,"name":""}"#,
            r#"{"rbb_theme":1,"properties":[1]}"#,
            r#"{"rbb_theme":1,"properties":{"a":{"nested":1}}}"#,
            r#"{"rbb_theme":1,"stylesheet":5}"#,
            r#"{"rbb_theme":1,"templates":{"index.html":"{% if %}"}}"#,
            r#"{"rbb_theme":1,"templates":["x"]}"#,
        ] {
            assert!(parse(bad, known).is_err(), "{bad}");
        }
    }
}
