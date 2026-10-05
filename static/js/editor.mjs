// Rich-text (WYSIWYG) mode for the MyCode editor.
//
// The textarea stays the form field and the source of truth: rich mode is a contenteditable view
// of it. MyCode is parsed here (mirroring the server's tag set) into editable HTML, and the
// edited HTML is turned back into MyCode whenever it changes. Every element this module creates
// remembers the exact tags it came from, and MyCode the view can't edit faithfully (images,
// videos, attachments) becomes a locked chip that keeps its source, so switching modes or
// editing one word never rewrites the rest of a post.
//
// The conversion functions take a `document`, so they run under Node with a small fake DOM
// (tests/js/editor.test.mjs). Without JavaScript the plain MyCode editor works as before.

const KNOWN = new Set(["b", "i", "u", "s", "sup", "sub", "color", "size", "font", "align", "left", "center", "right", "justify", "url", "email", "img", "quote", "list", "*", "hr", "video", "spoiler", "code", "php", "attachment", "indent"]);
const VERBATIM = new Set(["code", "php", "noparse"]);
/// Tags shown as locked chips: their content isn't editable text.
const ATOMS = new Set(["img", "video", "attachment"]);
const INLINE = { b: "STRONG", i: "EM", u: "U", s: "S", sup: "SUP", sub: "SUB", color: "SPAN", size: "SPAN", font: "SPAN", url: "A", email: "A" };
const BLOCK = { quote: "BLOCKQUOTE", spoiler: "DIV", align: "DIV", left: "DIV", center: "DIV", right: "DIV", justify: "DIV", indent: "DIV" };
const BLOCK_TAGS = new Set(["DIV", "P", "BLOCKQUOTE", "PRE", "UL", "OL", "LI", "H1", "H2", "H3", "H4", "H5", "H6", "TABLE", "TR", "SECTION", "ARTICLE", "HEADER", "FOOTER", "FIGURE", "DETAILS", "SUMMARY"]);

// ------------------------------------------------------------------ MyCode → tree

/// One tag at `s[i] == "["`, like the server's `read_tag`: `{name, arg, closing, raw}` or null.
function readTag(s, i) {
  const end = s.indexOf("]", i);
  if (end < 0 || end - i > 512) return null;
  const inner = s.slice(i + 1, end);
  const raw = s.slice(i, end + 1);
  if (inner.startsWith("/")) {
    const name = inner.slice(1).trim().toLowerCase();
    return KNOWN.has(name) || VERBATIM.has(name) ? { name, arg: null, closing: true, raw } : null;
  }
  let ne = inner.search(/[= ]/);
  if (ne < 0) ne = inner.length;
  const name = inner.slice(0, ne).toLowerCase();
  if (!KNOWN.has(name) && !VERBATIM.has(name)) return null;
  const rest = inner.slice(ne);
  const arg = rest.startsWith("=") ? rest.slice(1) : rest.trim() ? rest.trim() : null;
  return { name, arg, closing: false, raw };
}

/// Parse MyCode into a tree that serializes back to exactly the same text:
/// `{t:"text", v}`, `{t:"tag", name, arg, open, close, kids}`, `{t:"raw", name, v}` for
/// verbatim blocks, atoms and void tags.
export function parse(src) {
  const root = { t: "root", kids: [] };
  const stack = [root];
  const top = () => stack[stack.length - 1];
  const text = (v) => {
    const k = top().kids;
    const last = k[k.length - 1];
    if (last && last.t === "text") last.v += v; else k.push({ t: "text", v });
  };
  // An element that never closes was just text: its opening tag, then its children.
  const unwind = (n) => {
    const parent = top();
    parent.kids.pop();
    text(n.open);
    for (const k of n.kids) k.t === "text" ? text(k.v) : parent.kids.push(k);
  };
  let i = 0, plain = 0;
  const flush = (to) => { if (to > plain) text(src.slice(plain, to)); };
  while (i < src.length) {
    const j = src.indexOf("[", i);
    if (j < 0) break;
    const tag = readTag(src, j);
    if (!tag) { i = j + 1; continue; }
    const after = j + tag.raw.length;
    if (tag.closing) {
      let d = stack.length - 1;
      while (d > 0 && stack[d].name !== tag.name) d--;
      if (d === 0) { i = after; continue; }
      flush(j);
      while (stack.length - 1 > d) { const n = stack.pop(); unwind(n); }
      const n = stack.pop();
      n.close = tag.raw;
      i = plain = after;
      continue;
    }
    if (VERBATIM.has(tag.name)) {
      const m = src.slice(after).search(new RegExp("\\[/" + tag.name + "\\s*\\]", "i"));
      if (m < 0) { i = after; continue; }
      const cl = src.slice(after + m).match(/^\[[^\]]*\]/)[0];
      flush(j);
      top().kids.push({ t: "raw", name: tag.name, open: tag.raw, body: src.slice(after, after + m), close: cl });
      i = plain = after + m + cl.length;
      continue;
    }
    if (tag.name === "hr" || tag.name === "attachment") {
      flush(j);
      top().kids.push({ t: "raw", name: tag.name, open: tag.raw, body: "", close: "" });
      i = plain = after;
      continue;
    }
    if (tag.name === "*") {
      if (top().name !== "list") { i = after; continue; }
      flush(j);
      top().kids.push({ t: "item", open: tag.raw });
      i = plain = after;
      continue;
    }
    flush(j);
    const n = { t: "tag", name: tag.name, arg: tag.arg, open: tag.raw, close: "", kids: [] };
    top().kids.push(n);
    stack.push(n);
    i = plain = after;
  }
  flush(src.length);
  while (stack.length > 1) unwind(stack.pop());
  // Atoms keep their whole source.
  const atomize = (kids) => kids.map((k) => {
    if (k.t === "tag" && ATOMS.has(k.name)) return { t: "raw", name: k.name, open: serialize([k]), body: "", close: "" };
    if (k.t === "tag") k.kids = atomize(k.kids);
    return k;
  });
  root.kids = atomize(root.kids);
  return root.kids;
}

export function serialize(nodes) {
  let out = "";
  for (const n of nodes) {
    if (n.t === "text") out += n.v;
    else if (n.t === "item") out += n.open;
    else if (n.t === "raw") out += n.open + n.body + n.close;
    else out += n.open + serialize(n.kids) + n.close;
  }
  return out;
}

// ------------------------------------------------------------------ tree → editable DOM

const SAFE_COLOR = /^(#[0-9a-f]{3,8}|[a-z]{3,20})$/i;
const SAFE_FONT = /^[a-z0-9 ,'"-]{1,60}$/i;
const SIZES = { "xx-small": ".6em", "x-small": ".75em", small: ".85em", medium: "1em", large: "1.25em", "x-large": "1.5em", "xx-large": "2em" };
const unquote = (a) => (a || "").trim().replace(/^["']|["']$/g, "");

function atomLabel(n) {
  const inner = n.open.replace(/^\[[^\]]*\]/, "").replace(/\[\/[^\]]*\]$/, "").trim();
  if (n.name === "img") return "🖼 " + (inner || "image");
  if (n.name === "video") return "▶ " + (inner || "video");
  if (n.name === "attachment") return "📎 Attachment " + (n.open.match(/=(\d+)/) || ["", ""])[1];
  return n.open;
}

/// Build editable DOM for parsed MyCode. A trailing line break in any container is doubled,
/// as browsers need (the last `<br>` of a block only ends the line, it doesn't add one).
export function toFragment(nodes, doc) {
  const frag = doc.createDocumentFragment();
  fill(frag, nodes, doc, true);
  return frag;
}

function fill(parent, nodes, doc, container) {
  for (const n of nodes) parent.appendChild(build(n, doc));
  const last = parent.lastChild;
  if (container && last && last.nodeName === "BR") parent.appendChild(doc.createElement("br"));
}

function build(n, doc) {
  if (n.t === "text") {
    const f = doc.createDocumentFragment();
    n.v.split("\n").forEach((line, idx) => {
      if (idx) f.appendChild(doc.createElement("br"));
      if (line) f.appendChild(doc.createTextNode(line));
    });
    return f;
  }
  if (n.t === "raw") {
    if (n.name === "hr") {
      const hr = doc.createElement("hr");
      hr.setAttribute("data-open", n.open);
      return hr;
    }
    if (VERBATIM.has(n.name)) {
      const pre = doc.createElement("pre");
      pre.setAttribute("data-my", n.name);
      pre.setAttribute("data-open", n.open);
      pre.setAttribute("data-close", n.close);
      pre.appendChild(doc.createTextNode(n.body));
      return pre;
    }
    const chip = doc.createElement("span");
    chip.setAttribute("class", "wy-atom");
    chip.setAttribute("contenteditable", "false");
    chip.setAttribute("data-raw", n.open);
    chip.setAttribute("title", n.open);
    chip.appendChild(doc.createTextNode(atomLabel(n)));
    return chip;
  }
  if (n.t === "item") return doc.createTextNode(n.open); // only inside lists, handled there
  let el;
  if (n.name === "list") {
    const arg = unquote(n.arg);
    el = doc.createElement(arg ? "ol" : "ul");
    if (arg && /^[1aAiI]$/.test(arg)) el.setAttribute("type", arg);
    // Content before the first [*] and each item's trailing newline are kept as attributes.
    let lead = "", li = null;
    for (const k of n.kids) {
      if (k.t === "item") {
        li = doc.createElement("li");
        li.setAttribute("data-open", k.open);
        li._kids = [];
        el.appendChild(li);
      } else if (!li) {
        lead += k.t === "text" ? k.v : serialize([k]);
      } else li._kids.push(k);
    }
    el.setAttribute("data-lead", lead);
    for (const item of Array.from(el.childNodes)) {
      const kids = item._kids;
      delete item._kids;
      const last = kids[kids.length - 1];
      if (last && last.t === "text" && last.v.endsWith("\n")) {
        item.setAttribute("data-nl", "1");
        kids[kids.length - 1] = { t: "text", v: last.v.slice(0, -1) };
      }
      fill(item, kids, doc, true);
    }
  } else if (INLINE[n.name]) {
    el = doc.createElement(INLINE[n.name].toLowerCase());
    const arg = unquote(n.arg);
    if (n.name === "color" && SAFE_COLOR.test(arg)) el.style.color = arg;
    if (n.name === "size") el.style.fontSize = SIZES[arg.toLowerCase()] || (/^\d{1,2}$/.test(arg) ? Math.min(+arg, 50) + "pt" : "");
    if (n.name === "font" && SAFE_FONT.test(arg)) el.style.fontFamily = arg;
    if (n.name === "url" || n.name === "email") {
      const href = n.name === "email" ? "mailto:" + (arg || serialize(n.kids)) : arg || serialize(n.kids);
      if (/^(https?:\/\/|mailto:|\/)/i.test(href)) el.setAttribute("href", href);
    }
    fill(el, n.kids, doc, false);
  } else if (BLOCK[n.name]) {
    el = doc.createElement(BLOCK[n.name].toLowerCase());
    const arg = unquote(n.arg);
    const align = n.name === "align" ? arg.toLowerCase() : n.name;
    if (["left", "center", "right", "justify"].includes(align)) el.style.textAlign = align;
    if (n.name === "indent") el.style.marginLeft = "2em";
    if (n.name === "spoiler") el.setAttribute("class", "wy-spoiler");
    if (n.name === "quote" || n.name === "spoiler") {
      const who = n.name === "quote" ? (arg.match(/^("[^"]*"|'[^']*'|[^ ]+)/) || [""])[0] : arg;
      if (who) el.setAttribute("data-label", unquote(who));
    }
    fill(el, n.kids, doc, true);
  } else {
    el = doc.createElement("span");
    fill(el, n.kids, doc, false);
  }
  el.setAttribute("data-my", n.name);
  el.setAttribute("data-open", n.open);
  el.setAttribute("data-close", n.close);
  return el;
}

// ------------------------------------------------------------------ editable DOM → MyCode

const isBlock = (el) => BLOCK_TAGS.has(el.nodeName);

function rgbToHex(c) {
  const m = /^rgba?\((\d+),\s*(\d+),\s*(\d+)/i.exec(c || "");
  if (!m) return c;
  return "#" + [m[1], m[2], m[3]].map((x) => (+x).toString(16).padStart(2, "0")).join("");
}

/// The MyCode for the children of `root` (an element or fragment).
export function fromDom(root) {
  const st = { out: "", needNl: false };
  walk(root, st, true);
  return st.out;
}

function emit(st, s) {
  if (!s) return;
  if (st.needNl) {
    st.out += "\n";
    st.needNl = false;
  }
  st.out += s;
}

function walk(parent, st, container) {
  const kids = Array.from(parent.childNodes);
  kids.forEach((n, idx) => {
    // The last <br> of a container only ends its line.
    if (n.nodeName === "BR" && idx === kids.length - 1 && container) return;
    node(n, st);
  });
}

/// Inline wrapper tags implied by an element the browser created (bold, links, colours…).
function implied(el) {
  const name = el.nodeName;
  const st = el.style || {};
  const w = [];
  if (name === "B" || name === "STRONG" || /^(bold|[6-9]00)$/.test(st.fontWeight || "")) w.push(["[b]", "[/b]"]);
  if (name === "I" || name === "EM" || st.fontStyle === "italic") w.push(["[i]", "[/i]"]);
  if (name === "U" || /underline/.test(st.textDecoration || st.textDecorationLine || "")) w.push(["[u]", "[/u]"]);
  if (name === "S" || name === "STRIKE" || name === "DEL" || /line-through/.test(st.textDecoration || st.textDecorationLine || "")) w.push(["[s]", "[/s]"]);
  if (name === "SUP") w.push(["[sup]", "[/sup]"]);
  if (name === "SUB") w.push(["[sub]", "[/sub]"]);
  const color = name === "FONT" ? el.getAttribute("color") : st.color;
  if (color) w.push(["[color=" + rgbToHex(color) + "]", "[/color]"]);
  const face = name === "FONT" ? el.getAttribute("face") : "";
  if (face) w.push(["[font=" + face + "]", "[/font]"]);
  if (name === "CODE") w.push(["[code]", "[/code]"]);
  return w;
}

function node(n, st) {
  if (n.nodeType === 3) {
    emit(st, n.data.replace(/\u00a0/g, " ").replace(/\u200b/g, ""));
    return;
  }
  if (n.nodeType !== 1 && n.nodeType !== 11) return;
  if (n.nodeType === 11) { walk(n, st, false); return; }
  const el = n;
  const name = el.nodeName;
  if (name === "SCRIPT" || name === "STYLE" || name === "TEMPLATE" || el.getAttribute("data-skip") !== null) return;
  const raw = el.getAttribute("data-raw");
  if (raw !== null) { emit(st, raw); return; }
  if (name === "BR") { emit(st, "\n"); return; }
  const my = el.getAttribute("data-my");
  const open = el.getAttribute("data-open");
  if (name === "HR") { if (open === null) blockStart(st); emit(st, open || "[hr]"); return; }
  if (name === "PRE") {
    if (!my) blockStart(st);
    emit(st, (open || "[code]") + el.textContent + (el.getAttribute("data-close") || "[/code]"));
    if (!my) st.needNl = true;
    return;
  }
  if (name === "IMG") {
    const src = el.getAttribute("src") || "";
    if (/^https?:\/\//i.test(src)) emit(st, "[img]" + src + "[/img]");
    return;
  }
  if (name === "UL" || name === "OL") { list(el, st, my); return; }
  if (my && open !== null) {
    // An element this module rendered: its own tags, exactly.
    const inner = { out: "", needNl: false };
    walk(el, inner, isBlock(el));
    emit(st, open + inner.out + (el.getAttribute("data-close") || ""));
    return;
  }
  if (name === "A") {
    const href = el.getAttribute("href") || "";
    const text = fromDom(el);
    if (/^mailto:/i.test(href)) { const a = href.slice(7); emit(st, text === a ? "[email]" + a + "[/email]" : "[email=" + a + "]" + text + "[/email]"); return; }
    if (!/^(https?:\/\/|\/)/i.test(href)) { emit(st, text); return; }
    emit(st, text === href || !text ? "[url]" + href + "[/url]" : "[url=" + href + "]" + text + "[/url]");
    return;
  }
  const block = isBlock(el);
  if (block) blockStart(st);
  const wraps = implied(el);
  const align = block && el.style && el.style.textAlign;
  if (align && ["left", "center", "right", "justify"].includes(align)) wraps.unshift(["[align=" + align + "]", "[/align]"]);
  if (name === "BLOCKQUOTE") wraps.unshift(["[quote]", "[/quote]"]);
  if (/^H[1-6]$/.test(name)) wraps.unshift(["[b]", "[/b]"]);
  if (wraps.length) {
    const inner = { out: "", needNl: false };
    walk(el, inner, block);
    if (inner.out) emit(st, wraps.map((w) => w[0]).join("") + inner.out + wraps.map((w) => w[1]).reverse().join(""));
  } else {
    walk(el, st, block);
  }
  if (block) st.needNl = true;
}

function blockStart(st) {
  if (st.needNl) { st.out += "\n"; st.needNl = false; return; }
  if (st.out && !st.out.endsWith("\n")) st.out += "\n";
}

function list(el, st, my) {
  const items = Array.from(el.childNodes).filter((c) => c.nodeName === "LI");
  if (my) {
    let s = el.getAttribute("data-open") + (el.getAttribute("data-lead") || "");
    for (const li of items) s += (li.getAttribute("data-open") || "[*]") + fromDomContainer(li) + (li.getAttribute("data-open") === null || li.getAttribute("data-nl") ? "\n" : "");
    emit(st, s + el.getAttribute("data-close"));
    return;
  }
  blockStart(st);
  const type = el.nodeName === "OL" ? "=" + (el.getAttribute("type") || "1") : "";
  let s = "[list" + type + "]\n";
  for (const li of items) s += "[*]" + fromDomContainer(li) + "\n";
  emit(st, s + "[/list]");
  st.needNl = true;
}

function fromDomContainer(el) {
  const st = { out: "", needNl: false };
  walk(el, st, true);
  return st.out;
}

// ------------------------------------------------------------------ the editor UI

function enhance(ed) {
  const ta = ed.querySelector("textarea");
  if (!ta || ed.rbbRich) return;
  const form = ed.closest("form");
  const surface = document.createElement("div");
  surface.className = "wy-surface post-body";
  surface.contentEditable = "true";
  surface.setAttribute("role", "textbox");
  surface.setAttribute("aria-multiline", "true");
  surface.setAttribute("aria-label", ta.getAttribute("aria-label") || "Message");
  surface.setAttribute("spellcheck", "true");
  surface.hidden = true;
  ta.after(surface);

  const switcher = document.createElement("div");
  switcher.className = "wy-switch";
  switcher.setAttribute("role", "group");
  switcher.setAttribute("aria-label", "Editor mode");
  const mkBtn = (label, mode) => {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "wy-mode";
    b.textContent = label;
    b.dataset.mode = mode;
    switcher.appendChild(b);
    return b;
  };
  const richBtn = mkBtn("Rich text", "rich");
  const srcBtn = mkBtn("MyCode", "source");
  const toolbar = ed.querySelector(".editor-toolbar");
  (toolbar || ed).prepend(switcher);

  let lastSrc = null; // MyCode the surface was last rendered from or synced to
  // Only an edited view writes back: until then the textarea's text stands exactly as it is.
  let dirty = false;
  const api = {
    active: false,
    // Rich view → textarea (and its listeners: autosave, character count).
    sync() {
      if (!api.active || !dirty) return;
      const src = fromDom(surface);
      if (src === lastSrc) return;
      lastSrc = src;
      ta.value = src;
      ta.dispatchEvent(new CustomEvent("input", { detail: { fromRich: true } }));
    },
    render() {
      surface.textContent = "";
      surface.appendChild(toFragment(parse(ta.value), document));
      lastSrc = ta.value;
      dirty = false;
    },
    insertText(text) {
      surface.focus();
      insertMyCode(text);
    },
    command,
  };
  ed.rbbRich = api;
  ta.rbbRich = api;

  function setMode(mode, remember) {
    const rich = mode === "rich";
    if (rich === api.active && !remember) return;
    if (rich) { api.render(); } else { api.sync(); }
    api.active = rich;
    surface.hidden = !rich;
    ta.hidden = rich;
    // A hidden required field blocks submitting without saying why; the server checks length.
    if (rich) { ta.dataset.required = ta.required ? "1" : ""; ta.required = false; } else if (ta.dataset.required) ta.required = true;
    ed.classList.toggle("rich", rich);
    richBtn.setAttribute("aria-pressed", String(rich));
    srcBtn.setAttribute("aria-pressed", String(!rich));
    if (remember) { try { localStorage.setItem("rbb.editor.mode", mode); } catch (e) {} }
  }
  richBtn.addEventListener("click", () => { setMode("rich", true); surface.focus(); });
  srcBtn.addEventListener("click", () => { setMode("source", true); ta.focus(); });

  surface.addEventListener("input", () => { dirty = true; api.sync(); });
  surface.addEventListener("blur", () => api.sync());
  // Text put into the textarea by other features (quoting a post, restoring a draft).
  ta.addEventListener("input", (e) => { if (api.active && !(e.detail && e.detail.fromRich) && ta.value !== lastSrc) api.render(); });
  if (form) form.addEventListener("submit", () => api.sync(), true);
  ed.addEventListener("click", (e) => { if (e.target.closest(".js-tab-preview")) api.sync(); }, true);

  surface.addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter" && form) {
      e.preventDefault();
      api.sync();
      form.requestSubmit ? form.requestSubmit() : form.submit();
      return;
    }
    // Plain line breaks, the way MyCode stores them, instead of browser-specific blocks.
    if (e.key === "Enter" && !e.shiftKey && !inside("LI", "PRE") && document.queryCommandSupported && document.queryCommandSupported("insertLineBreak")) {
      e.preventDefault();
      document.execCommand("insertLineBreak");
    }
  });
  // Pasted HTML keeps only what MyCode can say.
  surface.addEventListener("paste", (e) => {
    const cd = e.clipboardData;
    if (!cd) return;
    if (cd.files && cd.files.length && ta.rbbUpload) { e.preventDefault(); ta.rbbUpload([...cd.files]); return; }
    const html = cd.getData("text/html");
    e.preventDefault();
    if (html) {
      const doc = new DOMParser().parseFromString(html, "text/html");
      insertMyCode(fromDom(doc.body).replace(/^\n+|\n+$/g, ""));
    } else {
      document.execCommand("insertText", false, cd.getData("text/plain"));
    }
  });
  surface.addEventListener("drop", (e) => {
    if (e.dataTransfer && e.dataTransfer.files.length && ta.rbbUpload) { e.preventDefault(); ta.rbbUpload([...e.dataTransfer.files]); }
  });

  function inside(...names) {
    const sel = getSelection();
    let n = sel && sel.anchorNode;
    while (n && n !== surface) { if (names.includes(n.nodeName)) return true; n = n.parentNode; }
    return false;
  }
  function insertHtml(html) { dirty = true; document.execCommand("insertHTML", false, html); }
  function insertMyCode(src) {
    const tmp = document.createElement("div");
    tmp.appendChild(toFragment(parse(src), document));
    insertHtml(tmp.innerHTML);
    api.sync();
  }
  /// The current selection as MyCode (or plain text for code blocks).
  function selectedMyCode(plain) {
    const sel = getSelection();
    if (!sel || !sel.rangeCount || !surface.contains(sel.anchorNode)) return "";
    const r = sel.getRangeAt(0);
    if (plain) return r.toString();
    const holder = document.createElement("div");
    holder.appendChild(r.cloneContents());
    return fromDom(holder);
  }
  function command(tag, opts) {
    surface.focus();
    const exec = { b: "bold", i: "italic", u: "underline", s: "strikeThrough" }[tag];
    dirty = true;
    if (exec) { document.execCommand(exec); api.sync(); return; }
    if (tag === "list") { document.execCommand("insertUnorderedList"); api.sync(); return; }
    if (tag === "url") {
      const url = opts.value;
      if (!url) return;
      const sel = selectedMyCode();
      insertMyCode(sel ? "[url=" + url + "]" + sel + "[/url]" : "[url]" + url + "[/url]");
      return;
    }
    if (tag === "img" || tag === "video") {
      const url = opts.value;
      if (!url) return;
      insertMyCode(tag === "img" ? "[img]" + url + "[/img]" : "[video=" + (opts.arg || "youtube") + "]" + url + "[/video]");
      return;
    }
    const inner = selectedMyCode(tag === "code");
    const arg = opts.arg ? "=" + opts.arg : "";
    insertMyCode("[" + tag + arg + "]" + inner + "[/" + tag + "]");
  }

  let mode = "rich";
  try { mode = localStorage.getItem("rbb.editor.mode") || "rich"; } catch (e) {}
  setMode(mode, false);
  if (mode !== "rich") { richBtn.setAttribute("aria-pressed", "false"); srcBtn.setAttribute("aria-pressed", "true"); }
}

if (typeof document !== "undefined" && document.querySelectorAll) {
  // After rbb.js's own start-up (which may restore unsent text into the textarea).
  const run = () => document.querySelectorAll(".js-editor").forEach(enhance);
  if (document.readyState === "complete") run();
  else { document.addEventListener("DOMContentLoaded", run); window.addEventListener("load", run); }
}
