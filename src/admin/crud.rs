//! Generic table editor for the many small configuration tables (smilies, post icons, word
//! filters, custom MyCode, attachment types, profile fields, prefixes, report reasons, security
//! questions, help docs, calendars, warning types/levels, user titles, moderator tools, ban filters).

use crate::ctx::{CsrfForm, Ctx};
use crate::error::{AppError, AppResult};
use crate::util::now;
use axum::Router;
use axum::extract::{Path, Query};
use axum::response::Response;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Copy, Serialize, PartialEq)]
pub enum Kind {
    Text,
    Textarea,
    Int,
    Bool,
    Select(&'static [(&'static str, &'static str)]),
    Groups,
    Forums,
    Json,
    Days,
    Now,
}

#[derive(Clone, Copy, Serialize)]
pub struct Field {
    pub name: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: Kind,
}

pub struct Table {
    pub slug: &'static str,
    pub table: &'static str,
    pub key: &'static str,
    pub title: &'static str,
    pub singular: &'static str,
    pub section: &'static str,
    pub module: &'static str,
    pub description: &'static str,
    pub fields: &'static [Field],
    pub list: &'static [&'static str],
    pub order: &'static str,
    pub cache: &'static [&'static str],
    pub bump_parser: bool,
}

const fn f(name: &'static str, label: &'static str, help: &'static str, kind: Kind) -> Field {
    Field {
        name,
        label,
        help,
        kind,
    }
}

const YES: Kind = Kind::Bool;
const DOW: &[(&str, &str)] = &[
    ("0", "Sunday"),
    ("1", "Monday"),
    ("2", "Tuesday"),
    ("3", "Wednesday"),
    ("4", "Thursday"),
    ("5", "Friday"),
    ("6", "Saturday"),
];
const FIELD_TYPES: &[(&str, &str)] = &[
    ("text", "Text box"),
    ("textarea", "Text area"),
    ("select", "Select box"),
    ("multiselect", "Multiple select"),
    ("radio", "Radio buttons"),
    ("checkbox", "Check boxes"),
];
const TOOL_TYPES: &[(&str, &str)] = &[("t", "Thread tool"), ("p", "Post tool")];
const FILTER_TYPES: &[(&str, &str)] = &[
    ("1", "IP address"),
    ("2", "Username"),
    ("3", "Email address"),
];

pub static TABLES: &[Table] = &[
    Table {
        slug: "smilies",
        table: "smilies",
        key: "sid",
        title: "Smilies",
        singular: "smilie",
        section: "content",
        module: "content",
        description: "Emoticons replaced in posts. Each text code on its own line.",
        fields: &[
            f("name", "Name", "", Kind::Text),
            f(
                "find",
                "Text to replace",
                "One code per line, e.g. :)",
                Kind::Textarea,
            ),
            f(
                "image",
                "Image path",
                "e.g. /static/smilies/smile.svg or a full URL",
                Kind::Text,
            ),
            f("disporder", "Display order", "", Kind::Int),
            f("showclickable", "Show in the editor's smilie box", "", YES),
        ],
        list: &["image", "name", "find", "disporder"],
        order: "disporder, sid",
        cache: &["parser"],
        bump_parser: true,
    },
    Table {
        slug: "icons",
        table: "icons",
        key: "iid",
        title: "Post Icons",
        singular: "post icon",
        section: "content",
        module: "content",
        description: "Icons users can choose for threads and posts.",
        fields: &[
            f("name", "Name", "", Kind::Text),
            f("path", "Image path", "", Kind::Text),
        ],
        list: &["path", "name"],
        order: "name",
        cache: &["icons"],
        bump_parser: false,
    },
    Table {
        slug: "badwords",
        table: "badwords",
        key: "bid",
        title: "Word Filters",
        singular: "word filter",
        section: "content",
        module: "content",
        description: "Words replaced in posts, signatures and messages. Use * as a wildcard, or enable regex.",
        fields: &[
            f("badword", "Word", "", Kind::Text),
            f("replacement", "Replacement", "", Kind::Text),
            f("regex", "Treat as regular expression", "", YES),
        ],
        list: &["badword", "replacement", "regex"],
        order: "badword",
        cache: &["parser"],
        bump_parser: true,
    },
    Table {
        slug: "mycode",
        table: "mycode",
        key: "cid",
        title: "Custom MyCode",
        singular: "MyCode",
        section: "content",
        module: "content",
        description: "Additional MyCode tags implemented with regular expressions (applied to escaped text; captures are $1, $2…).",
        fields: &[
            f("title", "Title", "", Kind::Text),
            f(
                "description",
                "Description",
                "Shown on the MyCode help page.",
                Kind::Textarea,
            ),
            f(
                "regex",
                "Regular expression",
                r"e.g. \[highlight\](.*?)\[/highlight\]",
                Kind::Textarea,
            ),
            f(
                "replacement",
                "Replacement",
                "e.g. <mark>$1</mark>",
                Kind::Textarea,
            ),
            f("active", "Active", "", YES),
            f("parseorder", "Parse order", "", Kind::Int),
        ],
        list: &["title", "regex", "active", "parseorder"],
        order: "parseorder, cid",
        cache: &["parser"],
        bump_parser: true,
    },
    Table {
        slug: "attachtypes",
        table: "attachtypes",
        key: "atid",
        title: "Attachment Types",
        singular: "attachment type",
        section: "content",
        module: "content",
        description: "File extensions that can be uploaded, with size limits.",
        fields: &[
            f("name", "Name", "", Kind::Text),
            f(
                "extension",
                "Extension",
                "Without the dot, e.g. png",
                Kind::Text,
            ),
            f("mimetype", "MIME type", "", Kind::Text),
            f("maxsize", "Maximum size (KB)", "0 = unlimited", Kind::Int),
            f("icon", "Icon path", "", Kind::Text),
            f("enabled", "Enabled", "", YES),
            f(
                "groups",
                "Allowed groups",
                "None selected = all groups",
                Kind::Groups,
            ),
            f(
                "forums",
                "Allowed forums",
                "None selected = all forums",
                Kind::Forums,
            ),
            f("avatarfile", "Can be used as an avatar", "", YES),
        ],
        list: &["extension", "name", "mimetype", "maxsize", "enabled"],
        order: "extension",
        cache: &["attachtypes"],
        bump_parser: false,
    },
    Table {
        slug: "profilefields",
        table: "profilefields",
        key: "fid",
        title: "Custom Profile Fields",
        singular: "profile field",
        section: "users",
        module: "users",
        description: "Extra fields on member profiles.",
        fields: &[
            f("name", "Title", "", Kind::Text),
            f("description", "Description", "", Kind::Textarea),
            f("type", "Field type", "", Kind::Select(FIELD_TYPES)),
            f(
                "options",
                "Options",
                "For select/radio/checkbox fields: one option per line",
                Kind::Textarea,
            ),
            f("regex", "Validation regex", "Optional", Kind::Text),
            f("maxlength", "Maximum length", "0 = unlimited", Kind::Int),
            f("disporder", "Display order", "", Kind::Int),
            f("required", "Required", "", YES),
            f("registration", "Show on registration", "", YES),
            f("profile", "Show on profile", "", YES),
            f("postbit", "Show next to posts", "", YES),
            f("viewableby", "Viewable by", "None = everyone", Kind::Groups),
            f("editableby", "Editable by", "None = everyone", Kind::Groups),
            f("allowmycode", "Parse MyCode", "", YES),
            f("allowsmilies", "Parse smilies", "", YES),
        ],
        list: &["name", "type", "required", "disporder"],
        order: "disporder, fid",
        cache: &["profilefields"],
        bump_parser: false,
    },
    Table {
        slug: "prefixes",
        table: "threadprefixes",
        key: "pid",
        title: "Thread Prefixes",
        singular: "thread prefix",
        section: "forums",
        module: "forums",
        description: "Labels users can put in front of thread subjects.",
        fields: &[
            f("prefix", "Prefix", "", Kind::Text),
            f(
                "displaystyle",
                "Display HTML",
                "Optional, e.g. <span class=\"thread_prefix\" style=\"background:#fde2e2\">Bug</span>",
                Kind::Textarea,
            ),
            f(
                "forums",
                "Available in forums",
                "None = all forums",
                Kind::Forums,
            ),
            f(
                "groups",
                "Available to groups",
                "None = all groups",
                Kind::Groups,
            ),
        ],
        list: &["prefix", "displaystyle"],
        order: "prefix",
        cache: &["prefixes"],
        bump_parser: false,
    },
    Table {
        slug: "reportreasons",
        table: "reportreasons",
        key: "rid",
        title: "Report Reasons",
        singular: "report reason",
        section: "content",
        module: "content",
        description: "Reasons users choose from when reporting content.",
        fields: &[
            f("title", "Title", "", Kind::Text),
            f(
                "appliesto",
                "Applies to",
                "\"all\" or a comma separated list of: post, profile, reputation, pm",
                Kind::Text,
            ),
            f("extra", "Require an explanation", "", YES),
            f("disporder", "Display order", "", Kind::Int),
        ],
        list: &["title", "appliesto", "disporder"],
        order: "disporder, rid",
        cache: &["reportreasons"],
        bump_parser: false,
    },
    Table {
        slug: "questions",
        table: "questions",
        key: "qid",
        title: "Security Questions",
        singular: "security question",
        section: "content",
        module: "content",
        description: "Questions asked during registration to stop bots (enable in Settings → Security).",
        fields: &[
            f("question", "Question", "", Kind::Text),
            f(
                "answer",
                "Answers",
                "One acceptable answer per line (case-insensitive)",
                Kind::Textarea,
            ),
            f("active", "Active", "", YES),
        ],
        list: &["question", "active", "shown", "correct", "incorrect"],
        order: "qid",
        cache: &[],
        bump_parser: false,
    },
    Table {
        slug: "helpsections",
        table: "helpsections",
        key: "sid",
        title: "Help Sections",
        singular: "help section",
        section: "content",
        module: "content",
        description: "Sections of the help documents page.",
        fields: &[
            f("name", "Name", "", Kind::Text),
            f("description", "Description", "", Kind::Textarea),
            f("disporder", "Display order", "", Kind::Int),
            f("enabled", "Enabled", "", YES),
        ],
        list: &["name", "disporder", "enabled"],
        order: "disporder, sid",
        cache: &[],
        bump_parser: false,
    },
    Table {
        slug: "helpdocs",
        table: "helpdocs",
        key: "hid",
        title: "Help Documents",
        singular: "help document",
        section: "content",
        module: "content",
        description: "Documents shown on the help page (MyCode allowed).",
        fields: &[
            f("sid", "Section ID", "The ID of the help section", Kind::Int),
            f("name", "Title", "", Kind::Text),
            f("description", "Short description", "", Kind::Text),
            f("document", "Document", "", Kind::Textarea),
            f("disporder", "Display order", "", Kind::Int),
            f("enabled", "Enabled", "", YES),
        ],
        list: &["name", "sid", "disporder", "enabled"],
        order: "sid, disporder, hid",
        cache: &[],
        bump_parser: false,
    },
    Table {
        slug: "calendars",
        table: "calendars",
        key: "cid",
        title: "Calendars",
        singular: "calendar",
        section: "content",
        module: "content",
        description: "Calendars and their options.",
        fields: &[
            f("name", "Name", "", Kind::Text),
            f("disporder", "Display order", "", Kind::Int),
            f("startofweek", "Week starts on", "", Kind::Select(DOW)),
            f("showbirthdays", "Show birthdays", "", YES),
            f(
                "eventlimit",
                "Events shown per day in month view",
                "",
                Kind::Int,
            ),
            f("moderation", "Moderate new events", "", YES),
            f("allowmycode", "Allow MyCode", "", YES),
            f("allowsmilies", "Allow smilies", "", YES),
            f("allowimgcode", "Allow [img]", "", YES),
            f("allowvideocode", "Allow [video]", "", YES),
            f("allowhtml", "Allow HTML (sanitized)", "", YES),
        ],
        list: &["name", "disporder", "moderation"],
        order: "disporder, cid",
        cache: &["calendars"],
        bump_parser: false,
    },
    Table {
        slug: "warningtypes",
        table: "warningtypes",
        key: "tid",
        title: "Warning Types",
        singular: "warning type",
        section: "users",
        module: "users",
        description: "Predefined warnings moderators can issue.",
        fields: &[
            f("title", "Title", "", Kind::Text),
            f("points", "Points", "", Kind::Int),
            f(
                "expirationtime",
                "Expires after (days)",
                "0 = never",
                Kind::Days,
            ),
        ],
        list: &["title", "points", "expirationtime"],
        order: "title",
        cache: &[],
        bump_parser: false,
    },
    Table {
        slug: "warninglevels",
        table: "warninglevels",
        key: "lid",
        title: "Warning Levels",
        singular: "warning level",
        section: "users",
        module: "users",
        description: "Automatic actions when a user's warning level reaches a percentage of the maximum points.",
        fields: &[
            f("percentage", "Percentage", "e.g. 50", Kind::Int),
            f(
                "action",
                "Action (JSON)",
                r#"{"type":"moderate"|"suspend"|"ban","length":seconds,"usergroup":7}"#,
                Kind::Json,
            ),
        ],
        list: &["percentage", "action"],
        order: "percentage",
        cache: &[],
        bump_parser: false,
    },
    Table {
        slug: "usertitles",
        table: "usertitles",
        key: "utid",
        title: "User Titles",
        singular: "user title",
        section: "users",
        module: "groups",
        description: "Titles and stars awarded by post count (used when a group has no fixed title).",
        fields: &[
            f("posts", "Minimum posts", "", Kind::Int),
            f("title", "Title", "", Kind::Text),
            f("stars", "Stars", "", Kind::Int),
            f("starimage", "Star image", "", Kind::Text),
        ],
        list: &["posts", "title", "stars"],
        order: "posts",
        cache: &["usertitles"],
        bump_parser: false,
    },
    Table {
        slug: "modtools",
        table: "modtools",
        key: "tid",
        title: "Moderator Tools",
        singular: "moderator tool",
        section: "forums",
        module: "forums",
        description: "Custom one-click moderation tools shown in thread/post moderation menus.",
        fields: &[
            f("name", "Name", "", Kind::Text),
            f("description", "Description", "", Kind::Textarea),
            f("type", "Type", "", Kind::Select(TOOL_TYPES)),
            f("forums", "Forums", "None = all forums", Kind::Forums),
            f(
                "groups",
                "Groups that can use it",
                "None = all moderators",
                Kind::Groups,
            ),
            f(
                "threadoptions",
                "Thread options (JSON)",
                r#"e.g. {"openthread":"close","stickthread":"toggle","movethread":5,"movethreadredirect":true,"newsubject":"[Solved] {subject}","addreply":"Closed by staff.","pm_subject":"Your thread","pm_message":"…"}"#,
                Kind::Json,
            ),
            f(
                "postoptions",
                "Post options (JSON)",
                r#"e.g. {"approveposts":"approve","softdeleteposts":"softdelete","mergeposts":true,"splitposts":-2,"splitpostsnewsubject":"{subject} (split)"}"#,
                Kind::Json,
            ),
        ],
        list: &["name", "type"],
        order: "name",
        cache: &[],
        bump_parser: false,
    },
    Table {
        slug: "banfilters",
        table: "banfilters",
        key: "fid",
        title: "Ban Filters",
        singular: "ban filter",
        section: "users",
        module: "bans",
        description: "Banned IP addresses (wildcards * and CIDR like 10.0.0.0/8), usernames and email addresses (wildcards, or a bare domain).",
        fields: &[
            f("filter", "Filter", "", Kind::Text),
            f("type", "Type", "", Kind::Select(FILTER_TYPES)),
            f("dateline", "", "", Kind::Now),
        ],
        list: &["filter", "type", "lastuse", "dateline"],
        order: "type, filter",
        cache: &[],
        bump_parser: false,
    },
];

pub fn nav() -> Vec<(String, String, String)> {
    TABLES
        .iter()
        .map(|t| {
            (
                t.slug.to_string(),
                t.title.to_string(),
                t.section.to_string(),
            )
        })
        .collect()
}

fn table(slug: &str) -> AppResult<&'static Table> {
    TABLES
        .iter()
        .find(|t| t.slug == slug)
        .ok_or_else(|| AppError::not_found("table"))
}

pub fn router() -> Router<crate::app::App> {
    Router::new()
        .route("/crud/{slug}", get(list))
        .route("/crud/{slug}/edit", get(edit_form))
        .route("/crud/{slug}/save", post(save))
        .route("/crud/{slug}/delete", post(delete))
}

fn display(ctx: &Ctx, t: &Table, col: &str, v: &serde_json::Value) -> String {
    let field = t.fields.iter().find(|f| f.name == col);
    let s = match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(b) => {
            if *b {
                "Yes".into()
            } else {
                "No".into()
            }
        }
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    match field.map(|f| f.kind) {
        Some(Kind::Select(opts)) => opts
            .iter()
            .find(|o| o.0 == s)
            .map(|o| o.1.to_string())
            .unwrap_or(s),
        Some(Kind::Days) => format!("{} days", v.as_i64().unwrap_or(0) / 86400),
        _ if col == "dateline" || col == "lastuse" => {
            ctx.fmt_date(v.as_i64().unwrap_or(0), "datetime")
        }
        _ => crate::util::truncate_chars(&s, 80),
    }
}

pub async fn list(ctx: Ctx, Path(slug): Path<String>) -> AppResult<Response> {
    let t = table(&slug)?;
    crate::admin::acp_guard!(ctx, t.module);
    let rows: Vec<serde_json::Value> = sqlx::query_scalar(&format!(
        "SELECT row_to_json(x) FROM (SELECT * FROM {} ORDER BY {} LIMIT 2000) x",
        t.table, t.order
    ))
    .fetch_all(&ctx.app.db)
    .await?;
    let items: Vec<_> = rows
        .iter()
        .map(|r| {
            let cells: Vec<(String, String)> = t.list.iter().map(|c| (c.to_string(), display(&ctx, t, c, &r[*c]))).collect();
            minijinja::context! { id => r[t.key].as_i64().unwrap_or(0), cells => cells, image => t.list.first().filter(|c| **c == "image" || **c == "path").and_then(|c| r[*c].as_str().map(|s| s.to_string())) }
        })
        .collect();
    let headers: Vec<String> = t
        .list
        .iter()
        .map(|c| {
            t.fields
                .iter()
                .find(|f| f.name == *c)
                .map(|f| f.label.to_string())
                .unwrap_or_else(|| title_case(c))
        })
        .collect();
    crate::admin::page(
        &ctx,
        "admin/crud_list.html",
        t.section,
        t.title,
        minijinja::context! { slug => t.slug, table_title => t.title, singular => t.singular, description => t.description, headers => headers, items => items, has_image => t.list.first().map(|c| *c == "image" || *c == "path").unwrap_or(false) },
    )
    .await
}

fn title_case(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

#[derive(Deserialize, Default)]
pub struct IdQ {
    pub id: Option<i32>,
}

pub async fn edit_form(
    ctx: Ctx,
    Path(slug): Path<String>,
    Query(q): Query<IdQ>,
) -> AppResult<Response> {
    let t = table(&slug)?;
    crate::admin::acp_guard!(ctx, t.module);
    let row: serde_json::Value = match q.id {
        Some(id) => sqlx::query_scalar(&format!(
            "SELECT row_to_json(x) FROM (SELECT * FROM {} WHERE {} = $1) x",
            t.table, t.key
        ))
        .bind(id)
        .fetch_optional(&ctx.app.db)
        .await?
        .ok_or_else(|| AppError::not_found(t.singular))?,
        None => serde_json::json!({}),
    };
    let groups: Vec<(i32, String)> = {
        let mut v: Vec<_> = ctx
            .cache
            .groups
            .values()
            .map(|g| (g.gid, g.title.clone()))
            .collect();
        v.sort();
        v
    };
    let forums = crate::routes::forumdisplay::forum_jump(&ctx);
    let all_forums: Vec<(i32, String, usize)> = ctx
        .cache
        .forums
        .iter()
        .map(|f| {
            (
                f.fid,
                f.name.clone(),
                ctx.cache.forum_depth.get(&f.fid).copied().unwrap_or(0),
            )
        })
        .collect();
    let _ = forums;
    let fields: Vec<_> = t
        .fields
        .iter()
        .filter(|f| f.kind != Kind::Now)
        .map(|f| {
            let v = &row[f.name];
            let (value, selected): (String, Vec<i64>) = match f.kind {
                Kind::Groups | Kind::Forums => (String::new(), v.as_array().map(|a| a.iter().filter_map(|x| x.as_i64()).collect()).unwrap_or_default()),
                Kind::Json => (if v.is_null() { "{}".into() } else { serde_json::to_string_pretty(v).unwrap_or_default() }, vec![]),
                Kind::Days => ((v.as_i64().unwrap_or(0) / 86400).to_string(), vec![]),
                Kind::Bool => (if v.as_bool().unwrap_or(q.id.is_none() && matches!(f.name, "active" | "enabled" | "showclickable" | "profile" | "allowmycode" | "allowsmilies" | "showbirthdays")) { "1".into() } else { String::new() }, vec![]),
                _ => (match v {
                    serde_json::Value::Null => String::new(),
                    serde_json::Value::String(s) => s.clone(),
                    o => o.to_string(),
                }, vec![]),
            };
            let kind = match f.kind {
                Kind::Text => "text",
                Kind::Textarea => "textarea",
                Kind::Int | Kind::Days => "int",
                Kind::Bool => "bool",
                Kind::Select(_) => "select",
                Kind::Groups => "groups",
                Kind::Forums => "forums",
                Kind::Json => "json",
                Kind::Now => "now",
            };
            let options: Vec<(String, String)> = match f.kind {
                Kind::Select(o) => o.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
                _ => vec![],
            };
            minijinja::context! { name => f.name, label => f.label, help => f.help, kind => kind, value => value, selected => selected, options => options }
        })
        .collect();
    crate::admin::page(
        &ctx,
        "admin/crud_edit.html",
        t.section,
        &format!("{} {}", if q.id.is_some() { "Edit" } else { "Add" }, t.singular),
        minijinja::context! { slug => t.slug, table_title => t.title, id => q.id.unwrap_or(0), fields => fields, groups => groups, forums => all_forums },
    )
    .await
}

#[derive(Deserialize, Default)]
pub struct AnyForm {
    #[serde(default, flatten)]
    pub fields: HashMap<String, serde_json::Value>,
}

fn field_str(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(a)) => {
            a.last().and_then(|x| x.as_str()).unwrap_or("").to_string()
        }
        _ => String::new(),
    }
}

fn field_ints(v: Option<&serde_json::Value>) -> Vec<i32> {
    match v {
        Some(serde_json::Value::String(s)) => {
            s.split(',').filter_map(|x| x.trim().parse().ok()).collect()
        }
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().and_then(|s| s.parse().ok()))
            .collect(),
        _ => vec![],
    }
}

pub async fn save(
    ctx: Ctx,
    Path(slug): Path<String>,
    CsrfForm(form): CsrfForm<AnyForm>,
) -> AppResult<Response> {
    let t = table(&slug)?;
    crate::admin::acp_guard!(ctx, t.module);
    let id: i32 = field_str(form.fields.get("id")).parse().unwrap_or(0);
    // Validate specific tables.
    if t.slug == "mycode" {
        let re = field_str(form.fields.get("regex"));
        if regex::Regex::new(&re).is_err() {
            return Err(AppError::user("The regular expression is invalid."));
        }
    }
    if t.slug == "badwords"
        && field_str(form.fields.get("regex")) == "1"
        && regex::Regex::new(&field_str(form.fields.get("badword"))).is_err()
    {
        return Err(AppError::user(
            "The word filter regular expression is invalid.",
        ));
    }
    let fields: Vec<&Field> = t
        .fields
        .iter()
        .filter(|f| !(f.kind == Kind::Now && id > 0))
        .collect();
    let cols: Vec<&str> = fields.iter().map(|f| f.name).collect();
    let sql = if id > 0 {
        let sets: Vec<String> = cols
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c} = ${}", i + 1))
            .collect();
        format!(
            "UPDATE {} SET {} WHERE {} = ${}",
            t.table,
            sets.join(", "),
            t.key,
            cols.len() + 1
        )
    } else {
        let ph: Vec<String> = (1..=cols.len()).map(|i| format!("${i}")).collect();
        format!(
            "INSERT INTO {} ({}) VALUES ({})",
            t.table,
            cols.join(", "),
            ph.join(", ")
        )
    };
    let mut q = sqlx::query(&sql);
    for fdef in &fields {
        let v = form.fields.get(fdef.name);
        q = match fdef.kind {
            Kind::Text | Kind::Textarea | Kind::Select(_) => {
                q.bind(field_str(v).trim().to_string())
            }
            Kind::Int => {
                let n: i64 = field_str(v).trim().parse().unwrap_or(0);
                // Column types vary (smallint/int); send as text-cast-free integer.
                q.bind(n as i32)
            }
            Kind::Days => q.bind(field_str(v).trim().parse::<i64>().unwrap_or(0) * 86400),
            Kind::Bool => q.bind(matches!(field_str(v).as_str(), "1" | "on" | "yes" | "true")),
            Kind::Groups | Kind::Forums => q.bind(field_ints(v)),
            Kind::Json => {
                let raw = field_str(v);
                let j: serde_json::Value = if raw.trim().is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&raw).map_err(|e| {
                        AppError::user(format!("{} is not valid JSON: {e}", fdef.label))
                    })?
                };
                q.bind(j)
            }
            Kind::Now => q.bind(now()),
        };
    }
    if id > 0 {
        q = q.bind(id);
    }
    q.execute(&ctx.app.db).await.map_err(|e| match &e {
        sqlx::Error::Database(d) => AppError::user(format!("Could not save: {}", d.message())),
        _ => AppError::Db(e),
    })?;
    after_change(&ctx, t).await?;
    crate::admin::log(
        &ctx,
        t.slug,
        if id > 0 { "edit" } else { "add" },
        serde_json::json!({"id": id}),
    )
    .await;
    Ok(ctx.redirect(
        &format!("/admin/crud/{}", t.slug),
        &format!("The {} has been saved.", t.singular),
    ))
}

async fn after_change(ctx: &Ctx, t: &Table) -> AppResult<()> {
    if !t.cache.is_empty() {
        ctx.app.invalidate(t.cache).await?;
    }
    if t.bump_parser {
        ctx.app.bump_parser_rev().await?;
    }
    Ok(())
}

pub async fn delete(
    ctx: Ctx,
    Path(slug): Path<String>,
    CsrfForm(form): CsrfForm<AnyForm>,
) -> AppResult<Response> {
    let t = table(&slug)?;
    crate::admin::acp_guard!(ctx, t.module);
    let id: i32 = field_str(form.fields.get("id")).parse().unwrap_or(0);
    sqlx::query(&format!("DELETE FROM {} WHERE {} = $1", t.table, t.key))
        .bind(id)
        .execute(&ctx.app.db)
        .await?;
    after_change(&ctx, t).await?;
    crate::admin::log(&ctx, t.slug, "delete", serde_json::json!({"id": id})).await;
    Ok(ctx.redirect(
        &format!("/admin/crud/{}", t.slug),
        &format!("The {} has been deleted.", t.singular),
    ))
}
