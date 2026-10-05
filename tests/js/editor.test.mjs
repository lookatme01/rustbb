// The rich-text editor's MyCode conversions: `node --test tests/js`.
//
// A tiny fake DOM stands in for the browser: enough of Node, Element and DocumentFragment for
// static/js/editor.mjs to build and read back its editable view.

import { test } from "node:test";
import assert from "node:assert/strict";
import { parse, serialize, toFragment, fromDom } from "../../static/js/editor.mjs";

class N {
  constructor(type, name) { this.nodeType = type; this.nodeName = name; this.childNodes = []; this.parentNode = null; }
  appendChild(c) {
    if (c.nodeType === 11) { for (const k of [...c.childNodes]) this.appendChild(k); c.childNodes = []; return c; }
    if (c.parentNode) c.parentNode.childNodes.splice(c.parentNode.childNodes.indexOf(c), 1);
    c.parentNode = this;
    this.childNodes.push(c);
    return c;
  }
  get lastChild() { return this.childNodes[this.childNodes.length - 1] || null; }
  get textContent() { return this.nodeType === 3 ? this.data : this.childNodes.map((c) => c.textContent).join(""); }
}
class El extends N {
  constructor(name) { super(1, name.toUpperCase()); this.attrs = new Map(); this.style = {}; }
  setAttribute(k, v) { this.attrs.set(k, String(v)); }
  getAttribute(k) { return this.attrs.has(k) ? this.attrs.get(k) : null; }
}
class Text extends N { constructor(d) { super(3, "#text"); this.data = d; } }
const doc = {
  createElement: (n) => new El(n),
  createTextNode: (d) => new Text(d),
  createDocumentFragment: () => new N(11, "#document-fragment"),
};
/// Build browser-made DOM by hand: h("div", {style: {textAlign: "center"}}, "text", h("br")).
function h(name, props, ...kids) {
  const e = new El(name);
  if (props && !(props instanceof N) && typeof props === "object") {
    for (const [k, v] of Object.entries(props)) k === "style" ? Object.assign(e.style, v) : e.setAttribute(k, v);
  } else if (props !== undefined && props !== null) kids.unshift(props);
  for (const k of kids) e.appendChild(typeof k === "string" ? new Text(k) : k);
  return e;
}
const view = (src) => { const root = new El("div"); root.appendChild(toFragment(parse(src), doc)); return root; };
const roundTrip = (src) => fromDom(view(src));

const SAMPLES = [
  "",
  "plain text",
  "two\nlines\n\nand a gap",
  "trailing newline\n",
  "\n\nleading newlines",
  "[b]bold[/b] [i]italic[/i] [u]u[/u] [s]s[/s] x[sup]2[/sup] H[sub]2[/sub]O",
  "[B]upper case tags[/b]",
  "[b]nested [i]tags[/i][/b]",
  "[color=red]red[/color] [color=#ff8800]hex[/color] [size=large]big[/size] [size=5]five[/size] [font=Georgia]serif[/font]",
  "[url]https://example.com[/url] and [url=https://example.com/a?b=1&c=2]a link[/url]",
  "[url=\"https://example.com\"]quoted arg[/url]",
  "[email]me@example.com[/email] [email=me@example.com]mail me[/email]",
  "[img]https://example.com/a.png[/img] [img=100x50]https://example.com/b.png[/img] [img align=left]https://example.com/c.png[/img]",
  "[video=youtube]https://www.youtube.com/watch?v=abc[/video]",
  "see [attachment=12] here",
  "[quote]plain quote[/quote]",
  "[quote=\"alice\" pid=\"5\" dateline=\"1700000000\"]said\nthings[/quote]\nreply",
  "[quote=bob][quote=alice]inner[/quote]\nouter[/quote]",
  "[code]fn main() {\n    println!(\"[b]not bold[/b]\");\n}[/code]",
  "[php]<?php echo 1; ?>[/php] [noparse][b]raw[/b][/noparse]",
  "[spoiler]hidden[/spoiler] [spoiler=Ending]twist[/spoiler]",
  "[list]\n[*]one\n[*]two\n[/list]",
  "[list=1]\n[*]first\n[*]second [b]bold[/b]\n[/list]\nafter",
  "[list][*]tight[*]items[/list]",
  "[align=center]centred[/align] [center]also[/center] [right]r[/right] [justify]j[/justify] [indent]in[/indent]",
  "a line\n[hr]\nanother",
  "unknown [tag]stays[/tag] text, [b]unclosed bold, [/i] stray closer, [ not a tag",
  "[b][i]misnested[/b][/i]",
  "[*] item outside a list",
  "<script>alert(1)</script> & \"quotes\" 'single' <b>not html</b>",
  "emoji 🦀 and smilies :) :D",
  "[b]bold ending in newline\n[/b]",
  "[quote]ends with newline\n[/quote]",
];

test("parse and serialize are exact inverses", () => {
  for (const s of SAMPLES) assert.equal(serialize(parse(s)), s, JSON.stringify(s));
});

test("MyCode survives the editable view unchanged", () => {
  for (const s of SAMPLES) assert.equal(roundTrip(s), s, JSON.stringify(s));
});

test("random MyCode survives the editable view unchanged", () => {
  const parts = ["a", "b c", "\n", "\n\n", "[b]", "[/b]", "[i]", "[/i]", "[quote]", "[/quote]", "[quote=x]", "[list]", "[*]", "[/list]",
    "[code]", "[/code]", "[url=https://x.y]", "[/url]", "[img]", "[/img]", "[color=red]", "[/color]", "[hr]", "[", "]", "[/", "<", "&", " "];
  let seed = 42;
  const rnd = (n) => { seed = (seed * 1103515245 + 12345) & 0x7fffffff; return seed % n; };
  for (let i = 0; i < 20000; i++) {
    let s = "";
    for (let j = rnd(14); j > 0; j--) s += parts[rnd(parts.length)];
    assert.equal(serialize(parse(s)), s, JSON.stringify(s));
    assert.equal(roundTrip(s), s, JSON.stringify(s));
  }
});

test("locked chips keep images, videos and attachments", () => {
  const v = view("[img=10x10]https://example.com/a.png[/img] [attachment=7]");
  const chips = v.childNodes.filter((n) => n.getAttribute && n.getAttribute("class") === "wy-atom");
  assert.equal(chips.length, 2);
  assert.equal(chips[0].getAttribute("contenteditable"), "false");
  assert.equal(chips[0].getAttribute("data-raw"), "[img=10x10]https://example.com/a.png[/img]");
  // No remote request from the editor: the chip is text, not an <img>.
  assert.ok(!JSON.stringify(v.childNodes.map((n) => n.nodeName)).includes("IMG"));
});

test("unsafe arguments never reach styles or links", () => {
  const v = view("[color=red;background:url(x)]a[/color][url=javascript:alert(1)]b[/url][font=x\"><img]c[/font]");
  const [color, link, font] = v.childNodes;
  assert.equal(color.style.color, undefined);
  assert.equal(link.getAttribute("href"), null);
  assert.equal(font.style.fontFamily, undefined);
  // …and still round-trip exactly.
  assert.equal(fromDom(v), "[color=red;background:url(x)]a[/color][url=javascript:alert(1)]b[/url][font=x\"><img]c[/font]");
});

test("formatting the browser adds while editing becomes MyCode", () => {
  const cases = [
    [h("div", "plain ", h("b", "bold"), " ", h("strong", "strong"), " ", h("i", "it"), " ", h("em", "em")), "plain [b]bold[/b] [b]strong[/b] [i]it[/i] [i]em[/i]"],
    [h("div", h("u", "u"), h("strike", "s"), h("del", "d")), "[u]u[/u][s]s[/s][s]d[/s]"],
    [h("div", h("font", { color: "#ff0000" }, "red")), "[color=#ff0000]red[/color]"],
    [h("div", h("span", { style: { color: "rgb(0, 128, 255)", fontWeight: "bold" } }, "x")), "[b][color=#0080ff]x[/color][/b]"],
    [h("div", h("a", { href: "https://example.com" }, "https://example.com"), " ", h("a", { href: "https://example.com/x" }, "label")), "[url]https://example.com[/url] [url=https://example.com/x]label[/url]"],
    [h("div", h("a", { href: "javascript:alert(1)" }, "click")), "click"],
    [h("div", h("a", { href: "mailto:a@b.c" }, "a@b.c")), "[email]a@b.c[/email]"],
    // Chrome's Enter: each line its own <div>, an empty line a <div><br></div>.
    [h("div", "line1", h("div", "line2"), h("div", h("br")), h("div", "line4")), "line1\nline2\n\nline4"],
    [h("div", h("p", "para one"), h("p", "para two")), "para one\npara two"],
    [h("div", h("div", { style: { textAlign: "center" } }, "mid")), "[align=center]mid[/align]"],
    [h("div", h("ul", h("li", "a"), h("li", "b"))), "[list]\n[*]a\n[*]b\n[/list]"],
    [h("div", h("ol", h("li", "a"))), "[list=1]\n[*]a\n[/list]"],
    [h("div", h("blockquote", "quoted")), "[quote]quoted[/quote]"],
    [h("div", h("pre", "x < y && [b]")), "[code]x < y && [b][/code]"],
    [h("div", h("img", { src: "https://example.com/p.png" }), h("img", { src: "data:image/png;base64,AAAA" })), "[img]https://example.com/p.png[/img]"],
    [h("div", h("h2", "Title"), "body"), "[b]Title[/b]\nbody"],
    [h("div", h("script", "alert(1)"), h("style", "p{}"), "safe"), "safe"],
    [h("div", "a  b​"), "a  b"],
    [h("div", h("b")), ""],
  ];
  for (const [dom, want] of cases) assert.equal(fromDom(dom), want);
});

test("editing inside rendered MyCode keeps the original tags", () => {
  const v = view("[quote=\"alice\" pid=\"5\"]old[/quote]\n[list]\n[*]one\n[/list]");
  const quote = v.childNodes[0];
  quote.childNodes[0].data = "new";
  const list = v.childNodes.find((n) => n.nodeName === "UL");
  list.appendChild(h("li", "two")); // Enter at the end of a list adds a bare <li>
  assert.equal(fromDom(v), "[quote=\"alice\" pid=\"5\"]new[/quote]\n[list]\n[*]one\n[*]two\n[/list]");
});
