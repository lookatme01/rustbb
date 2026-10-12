#!/usr/bin/env node
// Contrast check for the Ink tokens (static/css/tokens.css), in light and dark mode.
//
//   node tests/a11y/contrast.mjs
//
// Reads the token file itself, so a palette change that breaks WCAG AA fails here before any page
// is rendered. Text pairs need 4.5:1, icons and other non-text marks 3:1. Every accent a member can
// pick is checked as button fill (with its button text), as text on every surface and on its own
// wash. Exits 1 on any failure.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const css = readFileSync(fileURLToPath(new URL("../../static/css/tokens.css", import.meta.url)), "utf8");

// Declarations of the first block that opens with `selector {`.
function block(selector) {
  const at = css.indexOf(selector + " {");
  if (at < 0) throw new Error(`no ${selector} block in tokens.css`);
  let i = css.indexOf("{", at) + 1, depth = 1, start = i;
  while (depth) { if (css[i] === "{") depth++; else if (css[i] === "}") depth--; i++; }
  const vars = {};
  for (const m of css.slice(start, i - 1).matchAll(/(--[\w-]+)\s*:\s*([^;]+);/g)) vars[m[1]] = m[2].trim();
  return vars;
}
const light = block(":root");
const dark = { ...light, ...block(':root[data-mode="dark"]') };
// The "auto" copy under prefers-color-scheme must match the explicit dark block exactly.
const autoDark = block(':root:not([data-mode="light"]):not([data-mode="dark"])');
const drift = Object.keys(autoDark).filter((k) => autoDark[k] !== dark[k]);

function parse(v) {
  v = v.trim();
  let m = v.match(/^#([0-9a-f]{6})$/i);
  if (m) return [0, 2, 4].map((i) => parseInt(m[1].slice(i, i + 2), 16)).concat(1);
  m = v.match(/^rgba?\(([^)]+)\)$/);
  if (m) { const p = m[1].split(",").map((x) => parseFloat(x)); return [p[0], p[1], p[2], p.length > 3 ? p[3] : 1]; }
  throw new Error("cannot parse colour " + v);
}
function resolve(vars, name, depth = 0) {
  const v = vars[name];
  if (v === undefined || depth > 10) throw new Error(`token ${name} is not defined`);
  const m = v.match(/^var\((--[\w-]+)\)$/);
  return m ? resolve(vars, m[1], depth + 1) : parse(v);
}
const over = (fg, bg) => { const a = fg[3]; return [0, 1, 2].map((i) => fg[i] * a + bg[i] * (1 - a)).concat(1); };
const mix = (c, bg, pct) => [0, 1, 2].map((i) => c[i] * pct + bg[i] * (1 - pct)).concat(1);
const lum = (c) => { const f = (x) => { x /= 255; return x <= 0.03928 ? x / 12.92 : ((x + 0.055) / 1.055) ** 2.4; }; return 0.2126 * f(c[0]) + 0.7152 * f(c[1]) + 0.0722 * f(c[2]); };
const ratio = (a, b) => { const [x, y] = [lum(a), lum(b)].sort((p, q) => q - p); return (x + 0.05) / (y + 0.05); };

const ACCENTS = ["ink", "cobalt", "moss", "plum", "raspberry", "amber"];
const SURFACES = ["--canvas", "--surface", "--surface-2", "--surface-3"];
const failures = [];
let checks = 0;
function need(mode, label, fg, bg, min) {
  checks++;
  const r = ratio(fg, bg);
  if (r < min) failures.push(`${mode}: ${label} is ${r.toFixed(2)}:1, needs ${min}:1`);
}

for (const [mode, v] of [["light", light], ["dark", dark]]) {
  const c = (n) => resolve(v, n);
  const washPct = mode === "light" ? 0.10 : 0.14;
  for (const s of SURFACES) {
    const bg = c(s);
    for (const t of ["--ink", "--soft", "--faint"]) need(mode, `${t} on ${s}`, c(t), bg, 4.5);
    for (const t of ["--ok", "--warn", "--bad"]) need(mode, `${t} on ${s}`, c(t), bg, 4.5);
    for (let i = 0; i < 6; i++) need(mode, `icon --l${i} on --t${i} over ${s}`, c(`--l${i}`), over(c(`--t${i}`), bg), 3);
    need(mode, `--live dot on ${s}`, c("--live"), bg, 3);
  }
  for (const t of ["ok", "warn", "bad"]) need(mode, `--${t} on --${t}-bg`, c(`--${t}`), c(`--${t}-bg`), 4.5);
  for (let i = 0; i < 6; i++) {
    need(mode, `unread icon --canvas on --l${i}`, c("--canvas"), c(`--l${i}`), 3);
    need(mode, `avatar initial on --av-${i}`, c("--av-ink"), c(`--av-${i}`), 4.5);
    need(mode, `chip text --ink on --t${i}`, c("--ink"), over(c(`--t${i}`), c("--surface")), 4.5);
  }
  for (const a of ["bronze", "silver", "gold", "green", "blue", "purple", "red"]) {
    const tone = c(`--award-${a}`);
    need(mode, `badge icon ${a}`, tone, mix(tone, c("--surface"), 0.12), 3);
    need(mode, `badge label on ${a}`, c("--ink"), mix(tone, c("--surface"), 0.12), 4.5);
  }
  need(mode, "danger button text", c("--surface"), c("--bad"), 4.5);
  need(mode, "focus ring --focus on --canvas", c("--focus"), c("--canvas"), 3);
  for (const a of ACCENTS) {
    const brand = c(`--accent-${a}`);
    need(mode, `${a}: button text`, c("--brand-ink"), brand, 4.5);
    for (const s of SURFACES) {
      need(mode, `${a}: link text on ${s}`, brand, c(s), 4.5);
      need(mode, `${a}: text on its wash over ${s}`, brand, mix(brand, c(s), washPct), 4.5);
    }
    need(mode, `${a}: dock badge`, c("--ink"), c("--canvas"), 4.5);
  }
}

if (drift.length) failures.push(`the auto-dark block differs from data-mode="dark" for ${drift.join(", ")}`);
for (const f of failures) console.log("✗ " + f);
console.log(`${checks} contrast pairs checked, ${failures.length} failing`);
process.exit(failures.length ? 1 : 0);
