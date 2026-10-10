//! Themes (stylesheets, properties, inheritance) and per-theme template overrides.

use crate::ctx::{CsrfForm, Ctx};
use crate::error::{AppError, AppResult};
use crate::util::now;
use axum::Router;
use axum::extract::{Path, Query};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use std::collections::HashMap;

pub fn router() -> Router<crate::app::App> {
    Router::new()
        .route("/themes", get(list))
        .route("/themes/edit", get(edit_form).post(edit_save))
        .route(
            "/themes/{tid}/banner",
            post(banner_upload).layer(crate::routes::upload_limit()),
        )
        .route("/themes/default", post(set_default))
        .route("/themes/delete", post(delete))
        .route(
            "/themes/import",
            post(import).layer(axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024)),
        )
        .route("/themes/{tid}/export", get(export))
        .route("/themes/{tid}/templates", get(templates))
        .route(
            "/themes/{tid}/template",
            get(template_form).post(template_save),
        )
        .route("/themes/{tid}/template/revert", post(template_revert))
}

#[derive(Deserialize, Default)]
pub struct AnyForm {
    #[serde(default, flatten)]
    pub fields: HashMap<String, serde_json::Value>,
}

fn s(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(a)) => {
            a.last().and_then(|x| x.as_str()).unwrap_or("").to_string()
        }
        _ => String::new(),
    }
}

pub async fn list(ctx: Ctx) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "themes");
    let users: HashMap<i32, i64> = sqlx::query_as::<_, (i32, i64)>(
        "SELECT style, COUNT(*) FROM users WHERE style > 0 GROUP BY style",
    )
    .fetch_all(&ctx.app.db)
    .await?
    .into_iter()
    .collect();
    let overrides: HashMap<i32, usize> =
        ctx.cache
            .templates
            .keys()
            .fold(HashMap::new(), |mut m, (t, _)| {
                *m.entry(*t).or_insert(0) += 1;
                m
            });
    let themes: Vec<_> = ctx
        .cache
        .themes
        .iter()
        .map(|t| minijinja::context! { tid => t.tid, name => &t.name, def => t.def, parent => ctx.cache.theme(t.pid).map(|p| p.name.clone()), users => users.get(&t.tid).copied().unwrap_or(0), overrides => overrides.get(&t.tid).copied().unwrap_or(0) })
        .collect();
    crate::admin::page(
        &ctx,
        "admin/themes.html",
        "themes",
        "Themes",
        minijinja::context! { all_themes => themes },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct TidQ {
    pub tid: Option<i32>,
}

pub async fn edit_form(ctx: Ctx, Query(q): Query<TidQ>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "themes");
    let theme = q.tid.and_then(|t| ctx.cache.theme(t).cloned());
    let props = theme
        .as_ref()
        .map(|t| serde_json::to_string_pretty(&t.properties.0).unwrap_or_default())
        .unwrap_or_else(|| {
            "{\n  \"logo\": \"/static/images/logo.svg\",\n  \"colormode\": \"auto\"\n}".into()
        });
    crate::admin::page(
        &ctx,
        "admin/theme_edit.html",
        "themes",
        if theme.is_some() { "Edit Theme" } else { "Add Theme" },
        minijinja::context! { th => theme, props => props, all_themes => ctx.cache.themes.to_vec(), groups => crate::admin::users::sorted_groups(&ctx) },
    )
    .await
}

pub async fn edit_save(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "themes");
    let fl = &f.fields;
    let tid: i32 = s(fl.get("tid")).parse().unwrap_or(0);
    let name = s(fl.get("name")).trim().to_string();
    if name.is_empty() {
        return Err(AppError::user("Please enter a theme name."));
    }
    let pid: i32 = s(fl.get("pid")).parse().unwrap_or(0);
    if tid > 0 && pid == tid {
        return Err(AppError::user("A theme cannot be its own parent."));
    }
    let mut props: serde_json::Value = serde_json::from_str(&s(fl.get("properties")))
        .map_err(|e| AppError::user(format!("Properties must be valid JSON: {e}")))?;
    apply_branding(&mut props, fl)?;
    let css = s(fl.get("stylesheet"));
    if css.to_lowercase().contains("</style") {
        return Err(AppError::user("The stylesheet cannot contain </style>."));
    }
    let groups: Vec<i32> = match fl.get("allowedgroups") {
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().and_then(|s| s.parse().ok()))
            .collect(),
        Some(serde_json::Value::String(x)) => {
            x.split(',').filter_map(|y| y.trim().parse().ok()).collect()
        }
        _ => vec![],
    };
    if tid > 0 {
        sqlx::query("UPDATE themes SET name = $2, pid = $3, properties = $4, stylesheet = $5, allowedgroups = $6 WHERE tid = $1")
            .bind(tid)
            .bind(&name)
            .bind(pid)
            .bind(&props)
            .bind(&css)
            .bind(&groups)
            .execute(&ctx.app.db)
            .await?;
    } else {
        sqlx::query("INSERT INTO themes (name, pid, properties, stylesheet, allowedgroups) VALUES ($1, $2, $3, $4, $5)")
            .bind(&name)
            .bind(pid)
            .bind(&props)
            .bind(&css)
            .bind(&groups)
            .execute(&ctx.app.db)
            .await?;
    }
    ctx.app.invalidate(&["themes"]).await?;
    crate::admin::log(
        &ctx,
        "themes",
        "Saved theme",
        serde_json::json!({"tid": tid, "name": name}),
    )
    .await;
    Ok(ctx.redirect(
        "/admin/themes",
        &format!("The theme “{name}” has been saved."),
    ))
}

pub async fn set_default(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "themes");
    let tid: i32 = s(f.fields.get("tid")).parse().unwrap_or(0);
    ctx.cache
        .theme(tid)
        .ok_or_else(|| AppError::not_found("theme"))?;
    sqlx::query("UPDATE themes SET def = (tid = $1)")
        .bind(tid)
        .execute(&ctx.app.db)
        .await?;
    ctx.app.invalidate(&["themes"]).await?;
    Ok(ctx.redirect("/admin/themes", "The default theme has been changed."))
}

pub async fn delete(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "themes");
    let tid: i32 = s(f.fields.get("tid")).parse().unwrap_or(0);
    let t = ctx
        .cache
        .theme(tid)
        .cloned()
        .ok_or_else(|| AppError::not_found("theme"))?;
    if t.def {
        return Err(AppError::user("The default theme cannot be deleted."));
    }
    if ctx.cache.themes.iter().any(|x| x.pid == tid) {
        return Err(AppError::user(
            "This theme has child themes. Delete or re-parent them first.",
        ));
    }
    sqlx::query("UPDATE users SET style = 0 WHERE style = $1")
        .bind(tid)
        .execute(&ctx.app.db)
        .await?;
    sqlx::query("UPDATE forums SET style = 0 WHERE style = $1")
        .bind(tid)
        .execute(&ctx.app.db)
        .await?;
    sqlx::query("DELETE FROM themes WHERE tid = $1")
        .bind(tid)
        .execute(&ctx.app.db)
        .await?;
    ctx.app
        .invalidate(&["themes", "templates", "forums"])
        .await?;
    crate::admin::log(
        &ctx,
        "themes",
        "Deleted theme",
        serde_json::json!({"tid": tid, "name": t.name}),
    )
    .await;
    Ok(ctx.redirect("/admin/themes", "The theme has been deleted."))
}

pub async fn export(ctx: Ctx, Path(tid): Path<i32>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "themes");
    let t = ctx
        .cache
        .theme(tid)
        .cloned()
        .ok_or_else(|| AppError::not_found("theme"))?;
    let templates: HashMap<String, String> = ctx
        .cache
        .templates
        .iter()
        .filter(|((th, _), _)| *th == tid)
        .map(|((_, n), src)| (n.clone(), src.clone()))
        .collect();
    let data = serde_json::json!({"rbb_theme": 1, "name": t.name, "properties": t.properties.0, "stylesheet": t.stylesheet, "templates": templates});
    Ok((
        [
            (header::CONTENT_TYPE, "application/json".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!(
                    "attachment; filename=\"theme-{}.json\"",
                    crate::util::slugify(&t.name)
                ),
            ),
        ],
        serde_json::to_string_pretty(&data).unwrap_or_default(),
    )
        .into_response())
}

pub async fn import(ctx: Ctx, CsrfForm(f): CsrfForm<AnyForm>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "themes");
    let t = crate::domain::theme_export::parse(&s(f.fields.get("json")), |n| {
        crate::templates::default_template(n).is_some()
    })
    .map_err(AppError::User)?;
    check_imported_properties(&t.properties)?;
    // The theme and its templates arrive together or not at all.
    let mut uow = crate::usecase::Uow::begin(&ctx.app).await?;
    let tid: i32 = sqlx::query_scalar(
        "INSERT INTO themes (name, pid, properties, stylesheet) VALUES ($1, 1, $2, $3) RETURNING tid",
    )
    .bind(format!("{} (imported)", t.name))
    .bind(serde_json::Value::Object(t.properties))
    .bind(&t.stylesheet)
    .fetch_one(uow.conn())
    .await?;
    for (n, src) in &t.templates {
        sqlx::query(
            "INSERT INTO templates (title, theme, template, dateline) VALUES ($1, $2, $3, $4)",
        )
        .bind(n)
        .bind(tid)
        .bind(src)
        .bind(now())
        .execute(uow.conn())
        .await?;
    }
    uow.invalidate(&["themes", "templates"]);
    uow.commit(&ctx.app).await?;
    Ok(ctx.redirect("/admin/themes", "The theme has been imported."))
}

pub async fn templates(ctx: Ctx, Path(tid): Path<i32>) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "themes");
    let t = ctx
        .cache
        .theme(tid)
        .cloned()
        .ok_or_else(|| AppError::not_found("theme"))?;
    let list: Vec<_> = crate::templates::default_template_names()
        .into_iter()
        .map(|n| {
            let own = ctx.cache.templates.contains_key(&(tid, n.clone()));
            let mut inherited = None;
            let mut cur = t.pid;
            while cur > 0 {
                if ctx.cache.templates.contains_key(&(cur, n.clone())) {
                    inherited = ctx.cache.theme(cur).map(|x| x.name.clone());
                    break;
                }
                cur = ctx.cache.theme(cur).map(|x| x.pid).unwrap_or(0);
            }
            let group = n
                .split_once('/')
                .map(|(g, _)| g.to_string())
                .unwrap_or_else(|| "general".into());
            minijinja::context! { name => n, own => own, inherited => inherited, group => group }
        })
        .collect();
    crate::admin::page(
        &ctx,
        "admin/templates.html",
        "themes",
        &format!("Templates: {}", t.name),
        minijinja::context! { th => &t, list => list },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct NameQ {
    #[serde(default)]
    pub name: String,
}

pub async fn template_form(
    ctx: Ctx,
    Path(tid): Path<i32>,
    Query(q): Query<NameQ>,
) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "themes");
    let t = ctx
        .cache
        .theme(tid)
        .cloned()
        .ok_or_else(|| AppError::not_found("theme"))?;
    let default = crate::templates::default_template(&q.name)
        .ok_or_else(|| AppError::not_found("template"))?;
    let source = crate::templates::resolve_source(&ctx.cache, tid, &q.name, None)
        .unwrap_or_else(|| default.clone());
    let own = ctx.cache.templates.contains_key(&(tid, q.name.clone()));
    crate::admin::page(
        &ctx,
        "admin/template_edit.html",
        "themes",
        &format!("{} — {}", q.name, t.name),
        minijinja::context! { th => &t, name => q.name, source => source, own => own, error => "" },
    )
    .await
}

pub async fn template_save(
    ctx: Ctx,
    Path(tid): Path<i32>,
    CsrfForm(f): CsrfForm<AnyForm>,
) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "themes");
    let t = ctx
        .cache
        .theme(tid)
        .cloned()
        .ok_or_else(|| AppError::not_found("theme"))?;
    let name = s(f.fields.get("name"));
    crate::templates::default_template(&name).ok_or_else(|| AppError::not_found("template"))?;
    let src = s(f.fields.get("template")).replace("\r\n", "\n");
    // Validate the syntax before saving so a typo can't take the board down.
    let env = minijinja::Environment::new();
    if let Err(e) = env.template_from_str(&src) {
        return crate::admin::page(&ctx, "admin/template_edit.html", "themes", &format!("{name} — {}", t.name), minijinja::context! { th => &t, name => name, source => src, own => true, error => format!("{e:#}") }).await;
    }
    sqlx::query("INSERT INTO templates (title, theme, template, dateline) VALUES ($1, $2, $3, $4) ON CONFLICT (theme, title) DO UPDATE SET template = $3, dateline = $4")
        .bind(&name)
        .bind(tid)
        .bind(&src)
        .bind(now())
        .execute(&ctx.app.db)
        .await?;
    ctx.app.invalidate(&["templates"]).await?;
    crate::admin::log(
        &ctx,
        "themes",
        "Edited template",
        serde_json::json!({"tid": tid, "template": name}),
    )
    .await;
    Ok(ctx.redirect(
        &format!(
            "/admin/themes/{tid}/template?name={}",
            percent_encoding::utf8_percent_encode(&name, percent_encoding::NON_ALPHANUMERIC)
        ),
        "The template has been saved.",
    ))
}

pub async fn template_revert(
    ctx: Ctx,
    Path(tid): Path<i32>,
    CsrfForm(f): CsrfForm<AnyForm>,
) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "themes");
    let name = s(f.fields.get("name"));
    sqlx::query("DELETE FROM templates WHERE theme = $1 AND title = $2")
        .bind(tid)
        .bind(&name)
        .execute(&ctx.app.db)
        .await?;
    ctx.app.invalidate(&["templates"]).await?;
    crate::admin::log(
        &ctx,
        "themes",
        "Reverted template",
        serde_json::json!({"tid": tid, "template": name}),
    )
    .await;
    Ok(ctx.redirect(
        &format!("/admin/themes/{tid}/templates"),
        "The template has been reverted.",
    ))
}

/// A brand colour must be `#rrggbb`.
pub fn valid_brand(c: &str) -> bool {
    c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|x| x.is_ascii_hexdigit())
}

/// A banner URL ends up inside CSS `url('…')`: allow only site-relative or https URLs without
/// characters that could break out of it.
pub fn valid_banner(u: &str) -> bool {
    (u.starts_with('/') && !u.starts_with("//") || u.starts_with("https://"))
        && u.len() < 500
        && !u.chars().any(|c| {
            matches!(c, '\'' | '"' | '(' | ')' | '\\' | '<' | '>' | ';')
                || c.is_whitespace()
                || c.is_control()
        })
}

/// The values the Branding form validates (brand, banner, logo, colour mode) end up in CSS and
/// templates; an imported file gets the same checks as the form.
fn check_imported_properties(props: &serde_json::Map<String, serde_json::Value>) -> AppResult<()> {
    let text = |k: &str| props.get(k).and_then(|v| v.as_str()).unwrap_or("");
    if !text("brand").is_empty() && !valid_brand(text("brand")) {
        return Err(AppError::user("The brand colour must look like #1f45e0."));
    }
    for (k, what) in [("banner", "banner"), ("logo", "logo")] {
        if !text(k).is_empty() && !valid_banner(text(k)) {
            return Err(AppError::user(format!(
                "The {what} must be a site path or an https:// URL."
            )));
        }
    }
    if !matches!(text("colormode"), "" | "light" | "dark") {
        return Err(AppError::user("The colour mode must be light or dark."));
    }
    // Branding values must be text: a number or object here would skip the checks above.
    for k in ["brand", "banner", "logo", "colormode"] {
        if props.get(k).is_some_and(|v| !v.is_string() && !v.is_null()) {
            return Err(AppError::user(format!(
                "The theme property {k} must be text."
            )));
        }
    }
    Ok(())
}

/// Merge the Branding fields of the theme form into the properties JSON.
fn apply_branding(
    props: &mut serde_json::Value,
    fl: &std::collections::HashMap<String, serde_json::Value>,
) -> AppResult<()> {
    if !fl.contains_key("branding") {
        return Ok(());
    }
    if !props.is_object() {
        *props = serde_json::json!({});
    }
    let o = props.as_object_mut().unwrap();
    let mut set = |k: &str, v: String| {
        if v.is_empty() {
            o.remove(k);
        } else {
            o.insert(k.to_string(), serde_json::Value::String(v));
        }
    };
    let brand = s(fl.get("brand")).trim().to_lowercase();
    let use_brand = s(fl.get("use_brand")) == "1";
    if use_brand && !valid_brand(&brand) {
        return Err(AppError::user("The brand colour must look like #1f45e0."));
    }
    set("brand", if use_brand { brand } else { String::new() });
    let mode = s(fl.get("colormode"));
    set(
        "colormode",
        if matches!(mode.as_str(), "light" | "dark") {
            mode
        } else {
            String::new()
        },
    );
    let banner = s(fl.get("banner")).trim().to_string();
    if !banner.is_empty() && !valid_banner(&banner) {
        return Err(AppError::user(
            "The banner must be a site path (/uploads/…) or an https:// URL.",
        ));
    }
    set("banner", banner);
    set(
        "hero_title",
        s(fl.get("hero_title")).trim().chars().take(80).collect(),
    );
    set(
        "hero_text",
        s(fl.get("hero_text")).trim().chars().take(240).collect(),
    );
    let logo = s(fl.get("logo")).trim().to_string();
    if !logo.is_empty() && !valid_banner(&logo) {
        return Err(AppError::user(
            "The logo must be a site path or an https:// URL.",
        ));
    }
    set("logo", logo);
    Ok(())
}

/// Upload a banner image for a theme: resized to at most 2400 px wide and stored as JPEG.
pub async fn banner_upload(
    ctx: Ctx,
    Path(tid): Path<i32>,
    mut mp: axum::extract::Multipart,
) -> AppResult<Response> {
    crate::admin::acp_guard!(ctx, "themes");
    let theme = ctx
        .cache
        .theme(tid)
        .cloned()
        .ok_or_else(|| AppError::not_found("theme"))?;
    let mut token = String::new();
    let mut file: Option<crate::infra::uploads::Spooled> = None;
    while let Some(field) = mp
        .next_field()
        .await
        .map_err(|e| AppError::user(format!("Upload failed: {e}")))?
    {
        match field.name().unwrap_or("") {
            "file" => {
                // The form puts `my_post_key` first (or the X-CSRF-Token header carries it), so
                // an unauthenticated request is refused before a 12 MB file is spooled.
                ctx.check_csrf(&token)?;
                match crate::infra::uploads::spool(&ctx.app.cfg.upload_dir, field, 12 * 1024 * 1024)
                    .await
                {
                    Ok(f) => file = Some(f),
                    Err(crate::infra::uploads::SpoolError::Empty) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            "my_post_key" => token = field.text().await.unwrap_or_default(),
            _ => {}
        }
    }
    ctx.check_csrf(&token)?;
    let upload = file.ok_or_else(|| AppError::user("Please choose an image."))?;
    let jpeg = crate::infra::uploads::with_image(upload.path().to_path_buf(), move |img| {
        let img = if img.width() > 2400 {
            img.resize(2400, 2400, image::imageops::FilterType::Lanczos3)
        } else {
            img
        };
        let mut out = std::io::Cursor::new(Vec::new());
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 84);
        img.to_rgb8()
            .write_with_encoder(enc)
            .map_err(|e| e.to_string())?;
        Ok(out.into_inner())
    })
    .await
    .map_err(AppError::User)?;
    drop(upload);
    let name = format!("banner_{tid}_{}.jpg", crate::util::random_token(8));
    ctx.app
        .storage
        .put_bytes(&format!("banners/{name}"), jpeg.into())
        .await
        .map_err(AppError::Other)?;
    let mut props = theme.properties.0.clone();
    if !props.is_object() {
        props = serde_json::json!({});
    }
    let old = props
        .get("banner")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    props["banner"] = serde_json::Value::String(format!("/uploads/banners/{name}"));
    sqlx::query("UPDATE themes SET properties = $2 WHERE tid = $1")
        .bind(tid)
        .bind(&props)
        .execute(&ctx.app.db)
        .await?;
    if let Some(old) = old.and_then(|o| o.strip_prefix("/uploads/banners/").map(|x| x.to_string()))
        && !old.contains('/')
        && !old.contains("..")
        && let Err(e) = ctx.app.storage.delete(&format!("banners/{old}")).await
    {
        tracing::warn!("removing the old banner failed: {e:#}");
    }
    ctx.app.invalidate(&["themes"]).await?;
    crate::admin::log(
        &ctx,
        "themes",
        "Uploaded theme banner",
        serde_json::json!({"tid": tid}),
    )
    .await;
    Ok(ctx.redirect(
        &format!("/admin/themes/edit?tid={tid}"),
        "The banner has been uploaded.",
    ))
}
