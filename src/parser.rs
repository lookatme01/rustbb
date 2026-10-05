//! MyCode (BBCode) parser.
//!
//! Input is tokenized into a tree (tolerant of mis-nesting and unclosed tags), text nodes are
//! HTML-escaped, and known tags are rendered to safe HTML. All attribute values are validated.
//! The output of `parse` for posts is cached in `posts.message_html` and invalidated through the
//! global parser revision, so parsing cost is paid once per post edit, not per view.

use crate::util::escape_html;
use regex::Regex;
use std::sync::LazyLock;

#[derive(Clone, Debug)]
pub struct ParseOptions {
    pub allow_html: bool,
    pub allow_mycode: bool,
    pub allow_smilies: bool,
    pub allow_imgcode: bool,
    pub allow_videocode: bool,
    pub filter_badwords: bool,
    pub nofollow: bool,
    /// Author username for `/me` support.
    pub me_username: Option<String>,
    pub mentions: bool,
    /// Render newlines as <br>.
    pub nl2br: bool,
}

impl Default for ParseOptions {
    fn default() -> Self {
        ParseOptions {
            allow_html: false,
            allow_mycode: true,
            allow_smilies: true,
            allow_imgcode: true,
            allow_videocode: true,
            filter_badwords: true,
            nofollow: true,
            me_username: None,
            mentions: true,
            nl2br: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Smilie {
    pub find: String,
    pub image: String,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct Badword {
    pub re: Regex,
    pub replacement: String,
}

#[derive(Clone, Debug)]
pub struct CustomMyCode {
    pub re: Regex,
    pub replacement: String,
}

/// Board-wide parser data (smilies, word filters, custom MyCodes), cached in memory.
#[derive(Clone, Debug, Default)]
pub struct ParserData {
    pub smilies: Vec<Smilie>,
    pub badwords: Vec<Badword>,
    pub custom: Vec<CustomMyCode>,
}

impl ParserData {
    pub fn new(
        mut smilies: Vec<Smilie>,
        badwords: Vec<(String, bool, String)>,
        custom: Vec<(String, String)>,
    ) -> Self {
        // Longest codes first so ":-)" wins over ":)".
        smilies.sort_by_key(|a| std::cmp::Reverse(a.find.len()));
        let badwords = badwords
            .into_iter()
            .filter_map(|(word, is_regex, replacement)| {
                let pat = if is_regex {
                    word
                } else {
                    let w = regex::escape(&word).replace(r"\*", r"\S*");
                    format!(r"(?i)(^|\b|\W){w}($|\b|\W)")
                };
                Regex::new(&pat).ok().map(|re| Badword { re, replacement })
            })
            .collect();
        let custom = custom
            .into_iter()
            .filter_map(|(re, rep)| {
                Regex::new(&re).ok().map(|re| CustomMyCode {
                    re,
                    replacement: rep,
                })
            })
            .collect();
        ParserData {
            smilies,
            badwords,
            custom,
        }
    }

    pub fn filter_badwords(&self, text: &str) -> String {
        let mut out = text.to_string();
        for b in &self.badwords {
            let rep = b.replacement.clone();
            out =
                b.re.replace_all(&out, |c: &regex::Captures| {
                    if c.len() >= 3 {
                        format!(
                            "{}{}{}",
                            c.get(1).map_or("", |m| m.as_str()),
                            rep,
                            c.get(c.len() - 1).map_or("", |m| m.as_str())
                        )
                    } else {
                        rep.clone()
                    }
                })
                .into_owned();
        }
        out
    }
}

#[derive(Debug, Clone)]
enum Node {
    Text(String),
    Elem {
        tag: String,
        arg: Option<String>,
        raw_open: String,
        raw_close: String,
        children: Vec<Node>,
        closed: bool,
    },
    /// Verbatim content (code blocks).
    Raw {
        tag: String,
        arg: Option<String>,
        content: String,
    },
}

/// Bump whenever the parser's output changes: `install::upgrade` then invalidates every post's
/// cached HTML, so existing posts pick up the change. (2: titled video frames.)
pub const CODE_REV: i32 = 2;

const KNOWN: &[&str] = &[
    "b",
    "i",
    "u",
    "s",
    "sup",
    "sub",
    "color",
    "size",
    "font",
    "align",
    "left",
    "center",
    "right",
    "justify",
    "url",
    "email",
    "img",
    "quote",
    "list",
    "*",
    "hr",
    "video",
    "spoiler",
    "code",
    "php",
    "attachment",
    "indent",
];
const VERBATIM: &[&str] = &["code", "php", "noparse"];

struct Tag {
    name: String,
    arg: Option<String>,
    closing: bool,
    raw: String,
}

/// Try to read a tag starting at `s[0] == '['`. Returns the tag and its byte length.
fn read_tag(s: &str) -> Option<(Tag, usize)> {
    let end = s.find(']')?;
    if end > 512 {
        return None;
    }
    let inner = &s[1..end];
    let raw = s[..=end].to_string();
    if let Some(name) = inner.strip_prefix('/') {
        let name = name.trim().to_ascii_lowercase();
        if KNOWN.contains(&name.as_str()) || VERBATIM.contains(&name.as_str()) {
            return Some((
                Tag {
                    name,
                    arg: None,
                    closing: true,
                    raw,
                },
                end + 1,
            ));
        }
        return None;
    }
    let name_end = inner.find(['=', ' ']).unwrap_or(inner.len());
    let name = inner[..name_end].to_ascii_lowercase();
    if !(KNOWN.contains(&name.as_str()) || VERBATIM.contains(&name.as_str())) {
        return None;
    }
    let rest = &inner[name_end..];
    let arg = if let Some(v) = rest.strip_prefix('=') {
        Some(v.to_string())
    } else if !rest.trim().is_empty() {
        Some(rest.trim().to_string()) // attributes form, e.g. [img align=left]
    } else {
        None
    };
    Some((
        Tag {
            name,
            arg,
            closing: false,
            raw,
        },
        end + 1,
    ))
}

fn tokenize_and_build(input: &str) -> Vec<Node> {
    // Stack of open elements; index 0 is the root.
    struct Open {
        tag: String,
        arg: Option<String>,
        raw_open: String,
        children: Vec<Node>,
    }
    let mut stack: Vec<Open> = vec![Open {
        tag: String::new(),
        arg: None,
        raw_open: String::new(),
        children: vec![],
    }];
    let mut text = String::new();
    let mut i = 0;
    let bytes = input.as_bytes();

    fn push_text(stack: &mut [Open], text: &mut String) {
        if text.is_empty() {
            return;
        }
        let top = stack.last_mut().unwrap();
        if let Some(Node::Text(t)) = top.children.last_mut() {
            t.push_str(text);
        } else {
            top.children.push(Node::Text(std::mem::take(text)));
        }
        text.clear();
    }

    fn close_top(stack: &mut Vec<Open>, raw_close: String, closed: bool) {
        let o = stack.pop().unwrap();
        let parent = stack.last_mut().unwrap();
        parent.children.push(Node::Elem {
            tag: o.tag,
            arg: o.arg,
            raw_open: o.raw_open,
            raw_close,
            children: o.children,
            closed,
        });
    }

    while i < input.len() {
        if bytes[i] == b'['
            && let Some((tag, len)) = read_tag(&input[i..])
        {
            if !tag.closing && VERBATIM.contains(&tag.name.as_str()) {
                let close = format!("[/{}]", tag.name);
                let after = &input[i + len..];
                if let Some(pos) = find_ci(after, &close) {
                    push_text(&mut stack, &mut text);
                    let content = after[..pos].to_string();
                    stack.last_mut().unwrap().children.push(Node::Raw {
                        tag: tag.name.clone(),
                        arg: tag.arg,
                        content,
                    });
                    i += len + pos + close.len();
                    continue;
                }
            } else if tag.closing {
                if let Some(pos) = stack.iter().rposition(|o| o.tag == tag.name)
                    && pos > 0
                {
                    push_text(&mut stack, &mut text);
                    while stack.len() > pos + 1 {
                        // auto-close mis-nested inner tags
                        close_top(&mut stack, String::new(), true);
                    }
                    close_top(&mut stack, tag.raw, true);
                    i += len;
                    continue;
                }
            } else if tag.name == "*" {
                push_text(&mut stack, &mut text);
                // A new list item closes the previous one.
                if stack.last().map(|o| o.tag == "*").unwrap_or(false) {
                    close_top(&mut stack, String::new(), true);
                }
                if stack.last().map(|o| o.tag == "list").unwrap_or(false) {
                    stack.push(Open {
                        tag: tag.name,
                        arg: None,
                        raw_open: tag.raw,
                        children: vec![],
                    });
                    i += len;
                    continue;
                }
                text.push_str(&tag.raw);
                i += len;
                continue;
            } else if tag.name == "hr" || tag.name == "attachment" {
                push_text(&mut stack, &mut text);
                stack.last_mut().unwrap().children.push(Node::Elem {
                    tag: tag.name,
                    arg: tag.arg,
                    raw_open: tag.raw,
                    raw_close: String::new(),
                    children: vec![],
                    closed: true,
                });
                i += len;
                continue;
            } else if stack.len() < 64 {
                push_text(&mut stack, &mut text);
                stack.push(Open {
                    tag: tag.name,
                    arg: tag.arg,
                    raw_open: tag.raw,
                    children: vec![],
                });
                i += len;
                continue;
            }
            // fallthrough: treat as literal
            text.push_str(&tag.raw);
            i += len;
            continue;
        }
        let ch = input[i..].chars().next().unwrap();
        text.push(ch);
        i += ch.len_utf8();
    }
    push_text(&mut stack, &mut text);
    while stack.len() > 1 {
        let implicit = stack.last().map(|o| o.tag == "*").unwrap_or(false);
        close_top(&mut stack, String::new(), implicit);
    }
    stack.pop().unwrap().children
}

fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let lh = hay.to_ascii_lowercase();
    lh.find(&needle.to_ascii_lowercase())
}

static URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(?:https?://|www\.)[^\s<>"'\[\]]+[^\s<>"'\[\].,;:!?)]"#).unwrap()
});
static MENTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:^|[\s(])@(?:&quot;([^&\n]{1,40}?)&quot;|([\p{L}\p{N}_.\-]{2,40}))"#).unwrap()
});
static COLOR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(#[0-9a-fA-F]{3,8}|[a-zA-Z]{3,20})$").unwrap());
static FONT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^[a-zA-Z0-9 ,\-]{1,50}$"#).unwrap());
static DIM_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d{1,4})x(\d{1,4})$").unwrap());

pub struct Parser<'a> {
    pub data: &'a ParserData,
    pub opts: &'a ParseOptions,
}

struct Ctx {
    quote_depth: usize,
    in_link: bool,
    images: usize,
}

impl<'a> Parser<'a> {
    pub fn new(data: &'a ParserData, opts: &'a ParseOptions) -> Self {
        Parser { data, opts }
    }

    pub fn parse(&self, message: &str) -> String {
        // \u{1}/\u{2} delimit the parser's own /me markers; users must not be able to forge them.
        let mut msg = message
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace(['\u{1}', '\u{2}'], "");
        if self.opts.filter_badwords && !self.data.badwords.is_empty() {
            msg = self.data.badwords_filter(&msg);
        }
        // /me support: "/me waves" at line start
        if let Some(name) = &self.opts.me_username
            && msg.contains("/me ")
        {
            let esc = name.replace('[', "&#91;");
            msg = msg
                .lines()
                .map(|l| {
                    if let Some(rest) = l.strip_prefix("/me ") {
                        format!("\u{1}me\u{2}* {esc} {rest}\u{1}/me\u{2}")
                    } else {
                        l.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        let mut out = if !self.opts.allow_mycode {
            self.render_text(
                &msg,
                &mut Ctx {
                    quote_depth: 0,
                    in_link: false,
                    images: 0,
                },
            )
        } else {
            let nodes = tokenize_and_build(&msg);
            let mut ctx = Ctx {
                quote_depth: 0,
                in_link: false,
                images: 0,
            };
            let mut s = String::with_capacity(msg.len() * 2);
            self.render_nodes(&nodes, &mut ctx, &mut s);
            s
        };
        if out.contains('\u{1}') {
            out = out
                .replace("\u{1}me\u{2}", "<span class=\"mycode_me\">")
                .replace("\u{1}/me\u{2}", "</span>");
        }
        if self.opts.allow_html {
            out = sanitize_html(&out);
        }
        out
    }

    fn render_nodes(&self, nodes: &[Node], ctx: &mut Ctx, out: &mut String) {
        let mut skip_nl = false;
        for n in nodes {
            match n {
                Node::Text(t) => {
                    let t = if skip_nl {
                        t.strip_prefix('\n').unwrap_or(t)
                    } else {
                        t.as_str()
                    };
                    out.push_str(&self.render_text(t, ctx));
                    skip_nl = false;
                }
                Node::Raw { tag, arg, content } => {
                    self.render_code(tag, arg.as_deref(), content, out);
                    skip_nl = true;
                }
                Node::Elem {
                    tag,
                    arg,
                    raw_open,
                    raw_close,
                    children,
                    closed,
                } => {
                    let block = self.render_elem(
                        tag,
                        arg.as_deref(),
                        raw_open,
                        raw_close,
                        children,
                        *closed,
                        ctx,
                        out,
                    );
                    skip_nl = block;
                }
            }
        }
    }

    fn children_html(&self, children: &[Node], ctx: &mut Ctx) -> String {
        let mut s = String::new();
        // strip one leading newline inside block elements
        let mut v = children.to_vec();
        if let Some(Node::Text(t)) = v.first_mut()
            && let Some(rest) = t.strip_prefix('\n')
        {
            *t = rest.to_string();
        }
        if let Some(Node::Text(t)) = v.last_mut()
            && let Some(rest) = t.strip_suffix('\n')
        {
            *t = rest.to_string();
        }
        self.render_nodes(&v, ctx, &mut s);
        s
    }

    fn plain_text(children: &[Node]) -> String {
        let mut s = String::new();
        for c in children {
            match c {
                Node::Text(t) => s.push_str(t),
                Node::Raw { content, .. } => s.push_str(content),
                Node::Elem {
                    raw_open,
                    raw_close,
                    children,
                    ..
                } => {
                    s.push_str(raw_open);
                    s.push_str(&Self::plain_text(children));
                    s.push_str(raw_close);
                }
            }
        }
        s
    }

    /// Returns true when the element is block-level (a following newline is swallowed).
    #[allow(clippy::too_many_arguments)]
    fn render_elem(
        &self,
        tag: &str,
        arg: Option<&str>,
        raw_open: &str,
        raw_close: &str,
        children: &[Node],
        closed: bool,
        ctx: &mut Ctx,
        out: &mut String,
    ) -> bool {
        let literal = |this: &Self, ctx: &mut Ctx, out: &mut String| {
            out.push_str(&this.render_text(raw_open, ctx));
            this.render_nodes(children, ctx, out);
            out.push_str(&this.render_text(raw_close, ctx));
        };
        if !closed {
            literal(self, ctx, out);
            return false;
        }
        let arg = arg.map(|a| strip_quotes(a.trim()));
        match tag {
            "b" | "i" | "u" | "s" | "sup" | "sub" => {
                let h = match tag {
                    "b" => "strong",
                    "i" => "em",
                    "s" => "del",
                    t => t,
                };
                out.push_str(&format!("<{h} class=\"mycode_{tag}\">"));
                self.render_nodes(children, ctx, out);
                out.push_str(&format!("</{h}>"));
                false
            }
            "color" => match arg.filter(|a| COLOR_RE.is_match(a)) {
                Some(c) => {
                    out.push_str(&format!(
                        "<span style=\"color: {c};\" class=\"mycode_color\">"
                    ));
                    self.render_nodes(children, ctx, out);
                    out.push_str("</span>");
                    false
                }
                None => {
                    literal(self, ctx, out);
                    false
                }
            },
            "size" => {
                let size = arg.and_then(|a| match a {
                    "xx-small" | "x-small" | "small" | "medium" | "large" | "x-large"
                    | "xx-large" => Some(a.to_string()),
                    n => n
                        .parse::<u32>()
                        .ok()
                        .map(|n| format!("{}pt", n.clamp(6, 50))),
                });
                match size {
                    Some(sz) => {
                        out.push_str(&format!(
                            "<span style=\"font-size: {sz};\" class=\"mycode_size\">"
                        ));
                        self.render_nodes(children, ctx, out);
                        out.push_str("</span>");
                    }
                    None => literal(self, ctx, out),
                }
                false
            }
            "font" => match arg.filter(|a| FONT_RE.is_match(a)) {
                Some(f) => {
                    out.push_str(&format!(
                        "<span style=\"font-family: {f};\" class=\"mycode_font\">"
                    ));
                    self.render_nodes(children, ctx, out);
                    out.push_str("</span>");
                    false
                }
                None => {
                    literal(self, ctx, out);
                    false
                }
            },
            "align" | "left" | "center" | "right" | "justify" => {
                let a = if tag == "align" {
                    arg.unwrap_or("")
                } else {
                    tag
                };
                if matches!(a, "left" | "center" | "right" | "justify") {
                    out.push_str(&format!(
                        "<div style=\"text-align: {a};\" class=\"mycode_align\">"
                    ));
                    out.push_str(&self.children_html(children, ctx));
                    out.push_str("</div>");
                    true
                } else {
                    literal(self, ctx, out);
                    false
                }
            }
            "indent" => {
                out.push_str("<div class=\"mycode_indent\">");
                out.push_str(&self.children_html(children, ctx));
                out.push_str("</div>");
                true
            }
            "url" => {
                let (href, label_children): (String, Option<&[Node]>) = match arg {
                    Some(a) if !a.is_empty() => (a.to_string(), Some(children)),
                    _ => (Self::plain_text(children), None),
                };
                match safe_url(&href) {
                    Some(u) if !ctx.in_link => {
                        let rel = if self.opts.nofollow {
                            "nofollow ugc noopener"
                        } else {
                            "noopener"
                        };
                        out.push_str(&format!(
                            "<a href=\"{}\" target=\"_blank\" rel=\"{rel}\" class=\"mycode_url\">",
                            escape_html(&u)
                        ));
                        ctx.in_link = true;
                        match label_children {
                            Some(c) => self.render_nodes(c, ctx, out),
                            None => out.push_str(&escape_html(&shorten_url(&href))),
                        }
                        ctx.in_link = false;
                        out.push_str("</a>");
                    }
                    _ => literal(self, ctx, out),
                }
                false
            }
            "email" => {
                let addr = match arg {
                    Some(a) if !a.is_empty() => a.to_string(),
                    _ => Self::plain_text(children),
                };
                if crate::util::valid_email(addr.trim()) {
                    out.push_str(&format!(
                        "<a href=\"mailto:{}\" class=\"mycode_email\">",
                        escape_html(addr.trim())
                    ));
                    if arg.is_some() {
                        self.render_nodes(children, ctx, out);
                    } else {
                        out.push_str(&escape_html(addr.trim()));
                    }
                    out.push_str("</a>");
                } else {
                    literal(self, ctx, out);
                }
                false
            }
            "img" => {
                let src = Self::plain_text(children);
                let src = src.trim();
                let Some(u) = safe_url(src).filter(|u| u.starts_with("http") || u.starts_with('/'))
                else {
                    literal(self, ctx, out);
                    return false;
                };
                if !self.opts.allow_imgcode {
                    out.push_str(&format!(
                        "<a href=\"{0}\" target=\"_blank\" rel=\"nofollow noopener\" class=\"mycode_url\">[img]{0}[/img]</a>",
                        escape_html(&u)
                    ));
                    return false;
                }
                ctx.images += 1;
                let mut attrs = String::new();
                if let Some(a) = arg {
                    if let Some(c) = DIM_RE.captures(a) {
                        attrs.push_str(&format!(" width=\"{}\" height=\"{}\"", &c[1], &c[2]));
                    } else if let Some(al) = a.strip_prefix("align=") {
                        let al = strip_quotes(al);
                        if al == "left" || al == "right" {
                            attrs.push_str(&format!(" class=\"mycode_img mycode_img_{al}\""));
                        }
                    }
                }
                if !attrs.contains("class=") {
                    attrs.push_str(" class=\"mycode_img\"");
                }
                let alt = u.rsplit('/').next().unwrap_or("image");
                out.push_str(&format!(
                    "<img src=\"{}\" loading=\"lazy\" decoding=\"async\" alt=\"[Image: {}]\"{attrs} />",
                    escape_html(&u),
                    escape_html(alt)
                ));
                false
            }
            "quote" => {
                let (name, pid, dateline) = parse_quote_arg(arg);
                ctx.quote_depth += 1;
                out.push_str("<blockquote class=\"mycode_quote\">");
                if let Some(n) = name {
                    out.push_str("<cite>");
                    out.push_str(&escape_html(&n));
                    out.push_str(" wrote:");
                    if let Some(d) = dateline {
                        out.push_str(&format!(
                            " <time class=\"quote_date\" data-ts=\"{d}\"></time>"
                        ));
                    }
                    if let Some(p) = pid {
                        out.push_str(&format!(" <a href=\"/post/{p}\" class=\"quick_jump\" title=\"Go to post\">&#x2191;</a>"));
                    }
                    out.push_str("</cite>");
                } else {
                    out.push_str("<cite>Quote:</cite>");
                }
                out.push_str(&self.children_html(children, ctx));
                out.push_str("</blockquote>");
                ctx.quote_depth -= 1;
                true
            }
            "spoiler" => {
                let title = arg.filter(|a| !a.is_empty()).unwrap_or("Spoiler");
                out.push_str(&format!(
                    "<details class=\"mycode_spoiler\"><summary>{}</summary><div class=\"spoiler_body\">",
                    escape_html(title)
                ));
                out.push_str(&self.children_html(children, ctx));
                out.push_str("</div></details>");
                true
            }
            "list" => {
                let (open, close) = match arg {
                    Some("1") => ("<ol type=\"1\" class=\"mycode_list\">", "</ol>"),
                    Some("a") => ("<ol type=\"a\" class=\"mycode_list\">", "</ol>"),
                    Some("A") => ("<ol type=\"A\" class=\"mycode_list\">", "</ol>"),
                    Some("i") => ("<ol type=\"i\" class=\"mycode_list\">", "</ol>"),
                    Some("I") => ("<ol type=\"I\" class=\"mycode_list\">", "</ol>"),
                    _ => ("<ul class=\"mycode_list\">", "</ul>"),
                };
                out.push_str(open);
                for c in children {
                    match c {
                        Node::Elem { tag, children, .. } if tag == "*" => {
                            out.push_str("<li>");
                            out.push_str(&self.children_html(children, ctx));
                            out.push_str("</li>");
                        }
                        Node::Text(t) if t.trim().is_empty() => {}
                        other => {
                            // content before first [*]
                            let mut s = String::new();
                            self.render_nodes(std::slice::from_ref(other), ctx, &mut s);
                            if !s.trim().is_empty() {
                                out.push_str("<li>");
                                out.push_str(&s);
                                out.push_str("</li>");
                            }
                        }
                    }
                }
                out.push_str(close);
                true
            }
            "*" => {
                // [*] outside of a list: treat as literal
                literal(self, ctx, out);
                false
            }
            "hr" => {
                out.push_str("<hr class=\"mycode_hr\" />");
                true
            }
            "attachment" => {
                match arg.and_then(|a| a.parse::<i64>().ok()) {
                    Some(aid) => out.push_str(&format!("<!--attachment:{aid}-->")),
                    None => out.push_str(&escape_html(raw_open)),
                }
                false
            }
            "video" => {
                let url = Self::plain_text(children);
                match (
                    self.opts.allow_videocode,
                    arg.and_then(|a| video_embed(a, url.trim())),
                ) {
                    (true, Some(html)) => {
                        out.push_str(&html);
                        true
                    }
                    _ => {
                        literal(self, ctx, out);
                        false
                    }
                }
            }
            _ => {
                literal(self, ctx, out);
                false
            }
        }
    }

    fn render_code(&self, tag: &str, arg: Option<&str>, content: &str, out: &mut String) {
        if tag == "noparse" {
            out.push_str(&escape_html(content).replace('\n', "<br />\n"));
            return;
        }
        let content = content.strip_prefix('\n').unwrap_or(content);
        let content = content.strip_suffix('\n').unwrap_or(content);
        let lang = match (tag, arg) {
            ("php", _) => "PHP".to_string(),
            (_, Some(a))
                if a.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '#')
                    && a.len() < 20 =>
            {
                a.to_string()
            }
            _ => String::new(),
        };
        let title = if lang.is_empty() {
            "Code:".to_string()
        } else {
            format!("{} Code:", escape_html(&lang))
        };
        out.push_str(&format!(
            "<div class=\"codeblock\"><div class=\"title\">{title}<button type=\"button\" class=\"copy_code\" title=\"Copy\">Copy</button></div><pre><code{}>{}</code></pre></div>",
            if lang.is_empty() { String::new() } else { format!(" class=\"language-{}\"", escape_html(&lang.to_lowercase())) },
            escape_html(content)
        ));
    }

    /// Render a plain text run: escape, custom MyCodes, auto-link, smilies, mentions, newlines.
    fn render_text(&self, text: &str, ctx: &mut Ctx) -> String {
        if text.is_empty() {
            return String::new();
        }
        let mut esc = if self.opts.allow_html {
            text.to_string()
        } else {
            escape_html(text)
        };
        for c in &self.data.custom {
            esc = c.re.replace_all(&esc, c.replacement.as_str()).into_owned();
        }
        let mut out = String::with_capacity(esc.len() + 32);
        if !ctx.in_link && self.opts.allow_mycode {
            let mut last = 0;
            for m in URL_RE.find_iter(&esc) {
                // don't auto-link inside HTML produced by custom MyCode
                let before = &esc[..m.start()];
                if before.ends_with("=\"")
                    || before.ends_with("='")
                    || before.ends_with('>') && before.ends_with("\">")
                {
                    continue;
                }
                out.push_str(&self.decorate(&esc[last..m.start()]));
                let raw = m.as_str();
                let href = if raw.to_ascii_lowercase().starts_with("www.") {
                    format!("http://{raw}")
                } else {
                    raw.to_string()
                };
                let rel = if self.opts.nofollow {
                    "nofollow ugc noopener"
                } else {
                    "noopener"
                };
                out.push_str(&format!(
                    "<a href=\"{href}\" target=\"_blank\" rel=\"{rel}\" class=\"mycode_url\">{}</a>",
                    shorten_url(raw)
                ));
                last = m.end();
            }
            out.push_str(&self.decorate(&esc[last..]));
        } else {
            out.push_str(&self.decorate(&esc));
        }
        if self.opts.nl2br {
            out = out.replace('\n', "<br />\n");
        }
        out
    }

    /// Smilies + mentions on escaped, non-link text.
    fn decorate(&self, s: &str) -> String {
        let mut s = s.to_string();
        if self.opts.allow_smilies && !self.data.smilies.is_empty() {
            s = replace_smilies(&s, &self.data.smilies);
        }
        if self.opts.mentions && s.contains('@') {
            s = MENTION_RE
                .replace_all(&s, |c: &regex::Captures| {
                    let whole = c.get(0).unwrap().as_str();
                    let lead = if whole.starts_with('@') { "" } else { &whole[..1] };
                    let name = c.get(1).or(c.get(2)).unwrap().as_str();
                    let raw_name = html_unescape(name);
                    let name_url = percent_encoding::utf8_percent_encode(&raw_name, percent_encoding::NON_ALPHANUMERIC);
                    format!("{lead}<a href=\"/user/name/{name_url}\" class=\"mycode_mention\">@{name}</a>")
                })
                .into_owned();
        }
        s
    }
}

impl ParserData {
    fn badwords_filter(&self, s: &str) -> String {
        self.filter_badwords(s)
    }
}

fn replace_smilies(s: &str, smilies: &[Smilie]) -> String {
    // Escape the smilie codes the same way the text was escaped.
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let bytes = s.as_bytes();
    'outer: while i < s.len() {
        let prev_ok = i == 0 || {
            let pc = s[..i].chars().next_back().unwrap();
            pc.is_whitespace() || !pc.is_alphanumeric() && pc != ';' && pc != '&' && pc != '/'
        };
        if prev_ok {
            for sm in smilies {
                let f = escape_html(&sm.find);
                if !f.is_empty() && s[i..].starts_with(&f) {
                    let after = &s[i + f.len()..];
                    let next_ok = after
                        .chars()
                        .next()
                        .map(|c| c.is_whitespace() || !c.is_alphanumeric())
                        .unwrap_or(true);
                    if next_ok {
                        out.push_str(&format!(
                            "<img src=\"{}\" alt=\"{}\" title=\"{}\" class=\"smilie\" />",
                            escape_html(&sm.image),
                            f,
                            escape_html(&sm.name)
                        ));
                        i += f.len();
                        continue 'outer;
                    }
                }
            }
        }
        let ch_len = s[i..].chars().next().unwrap().len_utf8();
        out.push_str(&s[i..i + ch_len]);
        let _ = bytes;
        i += ch_len;
    }
    out
}

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

static QUOTE_ARG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*(?:'([^']*)'|"([^"]*)"|([^\s'"]+))?(?:.*?\bpid=['"]?(\d+)['"]?)?(?:.*?\bdateline=['"]?(\d+)['"]?)?"#)
        .unwrap()
});

fn parse_quote_arg(arg: Option<&str>) -> (Option<String>, Option<i64>, Option<i64>) {
    let Some(a) = arg else {
        return (None, None, None);
    };
    // The arg arrives with quotes already stripped if it was a single quoted token.
    let raw = a;
    if let Some(c) = QUOTE_ARG_RE.captures(raw) {
        let name = c
            .get(1)
            .or(c.get(2))
            .or(c.get(3))
            .map(|m| m.as_str().to_string())
            .filter(|n| !n.is_empty());
        let name = name
            .map(|n| {
                // name might have captured `pid=..` if arg had no quoted name
                if n.starts_with("pid=") || n.starts_with("dateline=") {
                    String::new()
                } else {
                    n
                }
            })
            .filter(|n| !n.is_empty());
        let pid = c.get(4).and_then(|m| m.as_str().parse().ok());
        let dl = c.get(5).and_then(|m| m.as_str().parse().ok());
        // A plain unquoted name with spaces (e.g. [quote=John Smith]) — take everything when no attrs.
        if pid.is_none() && dl.is_none() && !raw.contains('\'') && !raw.contains('"') {
            return (
                Some(raw.trim().to_string()).filter(|s| !s.is_empty()),
                None,
                None,
            );
        }
        return (name, pid, dl);
    }
    (Some(a.to_string()), None, None)
}

/// Validate a URL for use in href/src. Returns a normalized URL or None.
pub fn safe_url(u: &str) -> Option<String> {
    let u = u.trim();
    if u.is_empty()
        || u.len() > 2048
        || u.contains(['"', '<', '>', '\n', '\r'])
        || u.chars().any(|c| c.is_control())
    {
        return None;
    }
    let lower = u.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("ftp://")
        || lower.starts_with("mailto:")
    {
        return Some(u.to_string());
    }
    if lower.starts_with("www.") {
        return Some(format!("http://{u}"));
    }
    // `/\host` is treated like `//host` by browsers.
    if u.starts_with('/') && !u.starts_with("//") && !u.contains('\\') {
        return Some(u.to_string());
    }
    if u.starts_with('#') {
        return Some(u.to_string());
    }
    // Reject any other scheme (javascript:, data:, vbscript:, ...)
    if lower.contains(':') {
        return None;
    }
    None
}

fn shorten_url(u: &str) -> String {
    if u.chars().count() > 60 {
        let head: String = u.chars().take(40).collect();
        let tail: String = u
            .chars()
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("{head}…{tail}")
    } else {
        u.to_string()
    }
}

static YT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:youtube\.com/(?:watch\?(?:.*&)?v=|embed/|shorts/)|youtu\.be/)([A-Za-z0-9_\-]{6,20})",
    )
    .unwrap()
});
static VIMEO_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"vimeo\.com/(?:video/)?(\d{3,12})").unwrap());
static DM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"dailymotion\.com/video/([A-Za-z0-9]{3,20})").unwrap());
static TWITCH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"twitch\.tv/videos/(\d{3,15})").unwrap());

fn video_embed(service: &str, url: &str) -> Option<String> {
    let src = match service.to_ascii_lowercase().as_str() {
        "youtube" => format!(
            "https://www.youtube-nocookie.com/embed/{}",
            YT_RE.captures(url)?.get(1)?.as_str()
        ),
        "vimeo" => format!(
            "https://player.vimeo.com/video/{}",
            VIMEO_RE.captures(url)?.get(1)?.as_str()
        ),
        "dailymotion" => format!(
            "https://www.dailymotion.com/embed/video/{}",
            DM_RE.captures(url)?.get(1)?.as_str()
        ),
        "twitch" => format!(
            "https://player.twitch.tv/?video={}&parent=localhost&autoplay=false",
            TWITCH_RE.captures(url)?.get(1)?.as_str()
        ),
        _ => return None,
    };
    let title = match service {
        "youtube" => "YouTube video",
        "vimeo" => "Vimeo video",
        "dailymotion" => "Dailymotion video",
        "twitch" => "Twitch video",
        _ => "Embedded video",
    };
    Some(format!(
        "<div class=\"mycode_video\"><iframe src=\"{src}\" title=\"{title}\" loading=\"lazy\" allowfullscreen referrerpolicy=\"strict-origin-when-cross-origin\" sandbox=\"allow-scripts allow-same-origin allow-presentation allow-popups\"></iframe></div>"
    ))
}

/// Allow-list HTML sanitizer used when a forum permits HTML. Keeps a small set of formatting
/// tags, drops every attribute except safe href/src/title/alt/class on the appropriate tags.
pub fn sanitize_html(input: &str) -> String {
    static TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)<(/?)([a-zA-Z][a-zA-Z0-9]*)([^>]*)>|<!--.*?-->").unwrap()
    });
    static ATTR_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"([a-zA-Z\-]+)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#).unwrap()
    });
    const ALLOWED: &[&str] = &[
        "a",
        "b",
        "strong",
        "i",
        "em",
        "u",
        "s",
        "del",
        "sup",
        "sub",
        "p",
        "br",
        "hr",
        "ul",
        "ol",
        "li",
        "blockquote",
        "cite",
        "pre",
        "code",
        "span",
        "div",
        "img",
        "table",
        "thead",
        "tbody",
        "tr",
        "td",
        "th",
        "h1",
        "h2",
        "h3",
        "h4",
        "details",
        "summary",
        "time",
        "iframe",
        "button",
    ];
    // Text between recognised tags is copied through, but any `<` or `>` in it is escaped:
    // an unterminated tag (`<meta http-equiv=refresh …` at the end of a post, or `<!--`) would
    // otherwise reach the browser raw and swallow the page markup that follows it.
    fn push_text(out: &mut String, t: &str) {
        out.push_str(&t.replace('<', "&lt;").replace('>', "&gt;"));
    }
    let mut out = String::with_capacity(input.len());
    let mut last = 0;
    for m in TAG_RE.captures_iter(input) {
        let whole = m.get(0).unwrap();
        push_text(&mut out, &input[last..whole.start()]);
        last = whole.end();
        let Some(name) = m.get(2) else {
            // keep our own attachment placeholders only
            if whole.as_str().starts_with("<!--attachment:") {
                out.push_str(whole.as_str());
            }
            continue;
        };
        let name = name.as_str().to_ascii_lowercase();
        if !ALLOWED.contains(&name.as_str()) {
            continue;
        }
        let closing = !m.get(1).unwrap().as_str().is_empty();
        if closing {
            out.push_str(&format!("</{name}>"));
            continue;
        }
        let mut attrs = String::new();
        for a in ATTR_RE.captures_iter(m.get(3).map_or("", |x| x.as_str())) {
            let key = a[1].to_ascii_lowercase();
            let val = a
                .get(2)
                .or(a.get(3))
                .or(a.get(4))
                .map_or("", |x| x.as_str());
            let keep = match key.as_str() {
                "href" if name == "a" => safe_url(&html_unescape(val)).is_some(),
                "src" if name == "img" => safe_url(&html_unescape(val))
                    .map(|u| u.starts_with("http") || u.starts_with('/'))
                    .unwrap_or(false),
                "src" if name == "iframe" => {
                    let v = html_unescape(val);
                    v.starts_with("https://www.youtube-nocookie.com/")
                        || v.starts_with("https://player.vimeo.com/")
                        || v.starts_with("https://www.dailymotion.com/embed/")
                        || v.starts_with("https://player.twitch.tv/")
                }
                "title" | "alt" | "width" | "height" | "loading" | "decoding" | "datetime"
                | "type" | "colspan" | "rowspan" => true,
                "class" => val
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == ' '),
                "style" => {
                    let v = html_unescape(val).to_ascii_lowercase();
                    // No external loads, legacy script hooks, or layout that can escape the
                    // post body to overlay the page (fake login boxes, hidden click targets).
                    ![
                        "url(",
                        "expression",
                        "@import",
                        "behavior",
                        "position",
                        "\\",
                        "/*",
                    ]
                    .iter()
                    .any(|bad| v.contains(bad))
                }
                "target" | "rel" | "allowfullscreen" | "referrerpolicy" | "sandbox" | "data-ts" => {
                    true
                }
                _ => false,
            };
            if keep {
                attrs.push_str(&format!(" {key}=\"{}\"", escape_html(&html_unescape(val))));
            }
        }
        if name == "iframe" && !attrs.contains(" src=") {
            continue;
        }
        let self_close = matches!(name.as_str(), "br" | "hr" | "img");
        out.push_str(&format!(
            "<{name}{attrs}{}>",
            if self_close { " /" } else { "" }
        ));
    }
    push_text(&mut out, &input[last..]);
    out
}

fn html_unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Convert MyCode to plain text (for emails, previews, feed summaries, meta descriptions).
/// Text that renders literally when placed inside generated MyCode: usernames and subjects
/// can't open tags (links, images, quotes…) or become auto-links. Each `[`-free segment, and
/// each `[` on its own, is wrapped in `[noparse]`, so no segment can contain a closing tag.
pub fn literal(text: &str) -> String {
    text.split('[')
        .map(|seg| {
            if seg.is_empty() {
                String::new()
            } else {
                format!("[noparse]{seg}[/noparse]")
            }
        })
        .collect::<Vec<_>>()
        .join("[noparse][[/noparse]")
}

pub fn to_plaintext(message: &str) -> String {
    static TAGS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?is)\[(?:/?(?:b|i|u|s|sup|sub|color|size|font|align|left|center|right|justify|url|email|list|\*|hr|spoiler|indent|code|php|noparse)(?:=[^\]]*)?)\]").unwrap()
    });
    static QUOTES: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)\[quote[^\]]*\].*?\[/quote\]").unwrap());
    static MEDIA: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?is)\[(img|video)[^\]]*\].*?\[/(img|video)\]|\[attachment=\d+\]").unwrap()
    });
    let mut s = message.to_string();
    for _ in 0..3 {
        let n = QUOTES.replace_all(&s, "").into_owned();
        if n == s {
            break;
        }
        s = n;
    }
    s = MEDIA.replace_all(&s, "").into_owned();
    s = TAGS.replace_all(&s, "").into_owned();
    s.trim().to_string()
}

/// Remove quotes nested deeper than `max_depth` (used when building a reply quote).
pub fn limit_quote_depth(message: &str, max_depth: usize) -> String {
    if max_depth == 0 {
        return message.to_string();
    }
    let mut out = String::with_capacity(message.len());
    let mut depth = 0usize;
    let mut i = 0;
    let lower = message.to_ascii_lowercase();
    while i < message.len() {
        if lower[i..].starts_with("[quote")
            && let Some(end) = message[i..].find(']')
        {
            depth += 1;
            if depth <= max_depth {
                out.push_str(&message[i..i + end + 1]);
            }
            i += end + 1;
            continue;
        }
        if lower[i..].starts_with("[/quote]") {
            if depth <= max_depth {
                out.push_str("[/quote]");
            }
            depth = depth.saturating_sub(1);
            i += 8;
            continue;
        }
        let ch = message[i..].chars().next().unwrap();
        if depth <= max_depth {
            out.push(ch);
        }
        i += ch.len_utf8();
    }
    out
}

pub fn count_images(message: &str) -> usize {
    message.to_ascii_lowercase().matches("[img").count()
}
pub fn count_videos(message: &str) -> usize {
    message.to_ascii_lowercase().matches("[video=").count()
}

/// Extract @mentioned usernames from a raw message (outside code/quotes is not enforced).
pub fn extract_mentions(message: &str) -> Vec<String> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?:^|[\s(])@(?:"([^"\n]{1,40}?)"|([\p{L}\p{N}_.\-]{2,40}))"#).unwrap()
    });
    let stripped = QUOTE_STRIP.replace_all(message, "");
    let mut v: Vec<String> = RE
        .captures_iter(&stripped)
        .filter_map(|c| c.get(1).or(c.get(2)).map(|m| m.as_str().to_string()))
        .collect();
    v.sort();
    v.dedup();
    v.truncate(20);
    v
}
static QUOTE_STRIP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)\[quote[^\]]*\].*?\[/quote\]|\[code\].*?\[/code\]").unwrap()
});

/// Pids quoted in the message ([quote=... pid=N]) so quoted users can be alerted.
pub fn extract_quoted_pids(message: &str) -> Vec<i32> {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)\[quote=[^\]]*?pid=['"]?(\d+)"#).unwrap());
    let mut v: Vec<i32> = RE
        .captures_iter(message)
        .filter_map(|c| c[1].parse().ok())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Highlight search terms in rendered HTML (text outside tags only).
pub fn highlight(html: &str, terms: &[String]) -> String {
    let terms: Vec<String> = terms
        .iter()
        .filter(|t| t.len() >= 2)
        .map(|t| regex::escape(&escape_html(t)))
        .collect();
    if terms.is_empty() {
        return html.to_string();
    }
    let Ok(re) = Regex::new(&format!("(?i)({})", terms.join("|"))) else {
        return html.to_string();
    };
    let mut out = String::with_capacity(html.len() + 64);
    let mut in_tag = false;
    let mut buf = String::new();
    for ch in html.chars() {
        if ch == '<' {
            out.push_str(&re.replace_all(&buf, "<mark class=\"highlight\">$1</mark>"));
            buf.clear();
            in_tag = true;
            out.push(ch);
        } else if ch == '>' && in_tag {
            in_tag = false;
            out.push(ch);
        } else if in_tag {
            out.push(ch);
        } else {
            buf.push(ch);
        }
    }
    out.push_str(&re.replace_all(&buf, "<mark class=\"highlight\">$1</mark>"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_text_cannot_open_tags() {
        for evil in [
            "[url=https://evil.example]Admin[/url]",
            "[img]https://evil.example/x.png[/img]",
            "a[/noparse][b]x[/b]",
            "[[b]]",
            "https://evil.example",
            "x[/NOPARSE][url]https://e.example[/url]",
        ] {
            let html = p(&literal(evil));
            assert!(
                !html.contains("<a")
                    && !html.contains("<img")
                    && !html.contains("<strong")
                    && !html.contains("<b>"),
                "{evil} -> {html}"
            );
            assert_eq!(html, escape_html(evil), "{evil} renders verbatim");
        }
        assert_eq!(p(&literal("Plain Name")), "Plain Name");
    }

    fn p(s: &str) -> String {
        let data = ParserData::new(
            vec![Smilie {
                find: ":)".into(),
                image: "/static/smilies/smile.png".into(),
                name: "Smile".into(),
            }],
            vec![("darn".into(), false, "****".into())],
            vec![],
        );
        let o = ParseOptions::default();
        Parser::new(&data, &o).parse(s)
    }

    #[test]
    fn basic_tags() {
        assert_eq!(p("[b]bold[/b]"), "<strong class=\"mycode_b\">bold</strong>");
        assert_eq!(
            p("[i][b]x[/i][/b]"),
            "<em class=\"mycode_i\"><strong class=\"mycode_b\">x</strong></em>[/b]"
        );
    }

    #[test]
    fn escapes_html() {
        assert_eq!(
            p("<script>alert(1)</script>"),
            "&lt;script&gt;alert(1)&lt;/script&gt;"
        );
        assert!(!p("[url=javascript:alert(1)]x[/url]").contains("href"));
        assert!(!p("[img]javascript:alert(1)[/img]").contains("<img"));
        assert!(!p("[color=red;background:url(x)]x[/color]").contains("style"));
        assert!(!p("[url=http://a.com\" onmouseover=\"x]y[/url]").contains("onmouseover=\""));
    }

    #[test]
    fn unclosed_is_literal() {
        assert_eq!(p("[b]open"), "[b]open");
    }

    #[test]
    fn urls() {
        let h = p("see https://example.com/a?b=1&c=2 now");
        assert!(
            h.contains("href=\"https://example.com/a?b=1&amp;c=2\""),
            "{h}"
        );
        let h = p("[url=https://x.org]site[/url]");
        assert!(h.contains(">site</a>"));
    }

    #[test]
    fn quotes_code_lists() {
        let h = p("[quote='Bob' pid='5' dateline='100']hi[/quote]");
        assert!(h.contains("Bob wrote:") && h.contains("/post/5"), "{h}");
        let h = p("[code]<b>[b]x[/b]</b>[/code]");
        assert!(h.contains("&lt;b&gt;[b]x[/b]&lt;/b&gt;"), "{h}");
        let h = p("[list]\n[*]a\n[*]b\n[/list]");
        assert_eq!(h, "<ul class=\"mycode_list\"><li>a</li><li>b</li></ul>");
        let h = p("[quote=John Smith]x[/quote]");
        assert!(h.contains("John Smith wrote:"), "{h}");
    }

    #[test]
    fn smilies_badwords() {
        assert!(p("hi :)").contains("class=\"smilie\""));
        assert!(!p("a:)b").contains("smilie"));
        assert_eq!(p("oh darn it"), "oh **** it");
    }

    #[test]
    fn video() {
        let h = p("[video=youtube]https://www.youtube.com/watch?v=dQw4w9WgXcQ[/video]");
        assert!(h.contains("youtube-nocookie.com/embed/dQw4w9WgXcQ"), "{h}");
    }

    #[test]
    fn mentions() {
        assert!(p("hey @alice").contains("/user/name/alice"));
        assert_eq!(
            extract_mentions("hi @bob and @\"Mary Ann\""),
            vec!["Mary Ann".to_string(), "bob".to_string()]
        );
    }

    #[test]
    fn quote_depth() {
        let m = "[quote]a[quote]b[quote]c[/quote][/quote][/quote]";
        assert_eq!(limit_quote_depth(m, 2), "[quote]a[quote]b[/quote][/quote]");
    }

    #[test]
    fn sanitizer() {
        let s = sanitize_html(
            "<p onclick=\"x\">a</p><script>bad()</script><a href=\"javascript:1\">l</a>",
        );
        assert_eq!(s, "<p>a</p>bad()<a>l</a>");
    }

    #[test]
    fn sanitizer_escapes_unterminated_tags() {
        // Audit #1: these reached the page raw and became live markup.
        let s = sanitize_html("hi <meta http-equiv=refresh content=\"0;url=//evil.example\"");
        assert!(!s.contains("<meta"), "{s}");
        let s = sanitize_html("x <img src=x onerror=alert(1) ");
        assert!(!s.contains("<img"), "{s}");
        let s = sanitize_html("a <!-- swallow the rest");
        assert!(!s.contains("<!--"), "{s}");
        // Real tags and our attachment placeholders still work.
        assert_eq!(sanitize_html("<b>x</b> 1 &lt; 2"), "<b>x</b> 1 &lt; 2");
        assert_eq!(sanitize_html("<!--attachment:5-->"), "<!--attachment:5-->");
        let s = sanitize_html("<div style=\"position:fixed;top:0\">x</div>");
        assert_eq!(s, "<div>x</div>");
    }

    #[test]
    fn html_forum_posts_are_safe() {
        let data = ParserData::default();
        let o = ParseOptions {
            allow_html: true,
            ..Default::default()
        };
        let h = Parser::new(&data, &o)
            .parse("hi <meta http-equiv=refresh content=\"0;url=//evil.example/x\"");
        assert!(!h.contains("<meta"), "{h}");
    }

    #[test]
    fn marker_chars_cannot_forge_markup() {
        let h = p("a \u{1}/me\u{2} b \u{1}me\u{2}");
        assert!(!h.contains("span"), "{h}");
    }

    #[test]
    fn backslash_urls_rejected() {
        assert!(safe_url("/\\evil.example").is_none());
        assert!(!p("[url=/\\evil.example]x[/url]").contains("href"));
        assert_eq!(safe_url("/thread/1").as_deref(), Some("/thread/1"));
    }
}
