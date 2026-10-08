#!/usr/bin/env node
// Accessibility audit: runs axe-core (WCAG 2.0/2.1/2.2 A and AA rules) on the board's main pages
// as a guest, a member and an administrator, in light and dark mode, on desktop and phone widths.
//
//   scripts/a11y.sh                                   # fresh board, server and audit
//   node tests/a11y/axe.mjs [base_url] [admin_password]  # against a running board
//
// Expects a freshly installed board (administrator "admin", the welcome thread 1 in forum 3).
// Packages are pinned in tests/a11y/package.json (`npm ci` there). Exits 1 on any violation.
// CHROMIUM_PATH picks the browser binary instead of Playwright's own download.

import { createRequire } from "node:module";
import { readFileSync } from "node:fs";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || "playwright-core");
const AXE = readFileSync(require.resolve("axe-core/axe.min.js"), "utf8");

const BASE = (process.argv[2] || "http://127.0.0.1:8097").replace(/\/$/, "");
const PASSWORD = process.argv[3] || process.env.RBB_ADMIN_PASSWORD || "admin12345";
const TAGS = ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa"];

const GUEST = ["/", "/forum/3", "/thread/1", "/member/login", "/member/register", "/member/lostpw", "/search", "/members", "/team", "/online",
  "/calendar", "/stats", "/help", "/mycode", "/badges", "/rules", "/privacy", "/portal", "/archive", "/user/1", "/thread/1/print", "/thread/999999"];
const EXPECTED_ERRORS = ["/thread/999999"];
const MEMBER = ["/", "/member/welcome", "/thread/1", "/newthread/3", "/newreply/1", "/usercp", "/usercp/profile", "/usercp/options", "/usercp/password", "/usercp/avatar",
  "/usercp/signature", "/usercp/subscriptions", "/usercp/security", "/pm", "/pm/send", "/search/new", "/usercp/alerts"];
const ADMIN = ["/admin", "/admin/settings", "/admin/settings/images", "/admin/forums", "/admin/users", "/admin/groups", "/admin/themes", "/modcp",
  "/modcp/reports", "/modcp/modqueue"];

const launch = () => chromium.launch(process.env.CHROMIUM_PATH ? { executablePath: process.env.CHROMIUM_PATH } : {});

async function login(context) {
  const page = await context.newPage();
  await page.goto(BASE + "/member/login");
  await page.fill("input[name=username]", "admin");
  await page.fill("input[name=password]", PASSWORD);
  await Promise.all([page.waitForNavigation(), page.click("form.auth-form button[type=submit]")]);
  // The Admin CP asks for the password again.
  await page.goto(BASE + "/admin");
  if (await page.$("form[action='/admin/verify']")) {
    await page.fill("form[action='/admin/verify'] input[name=password]", PASSWORD);
    await Promise.all([page.waitForNavigation(), page.click("form[action='/admin/verify'] button[type=submit]")]);
  }
  await page.close();
}

async function audit(context, path, label) {
  const page = await context.newPage();
  let resp = null;
  try { resp = await page.goto(BASE + path, { waitUntil: "load" }); } catch (e) { /* an error status without a page */ }
  // Pages that don't exist on this board (optional features) are skipped, not audited; error
  // pages are audited where a page is expected to be missing.
  if (!resp || (resp.status() >= 400 && !EXPECTED_ERRORS.includes(path))) { await page.close(); return { path, label, skipped: resp ? resp.status() : "no page" }; }
  await page.addScriptTag({ content: AXE });
  const res = await page.evaluate(async ([tags, all]) => {
    // Colours members pick in posts ([color=…]) are their content: no single colour is readable
    // on both the light and the dark theme, so they're excluded (member content).
    const r = await window.axe.run({ include: [["html"]], exclude: [[".mycode_color"]] }, { runOnly: { type: "tag", values: tags }, resultTypes: ["violations"] });
    return r.violations.map((v) => ({ id: v.id, impact: v.impact, help: v.help, nodes: v.nodes.slice(0, all ? 1000 : 5).map((n) => ({ target: n.target.join(" "), summary: n.failureSummary })) , count: v.nodes.length }));
  }, [TAGS, !!process.env.AXE_ALL]);
  await page.close();
  return { path, label, violations: res };
}

/// Keyboard checks axe can't make: the first Tab reaches the skip link, and each of the next
/// TABS stops shows a focus indicator (2.4.7) and isn't hidden under the sticky header (2.4.11).
const TABS = 30;
async function keyboard(context, path, label) {
  const page = await context.newPage();
  await page.goto(BASE + path);
  const problems = [];
  // A page that focuses a field itself (autofocus) rightly starts there instead.
  const autofocused = await page.evaluate(() => document.activeElement && document.activeElement !== document.body);
  await page.keyboard.press("Tab");
  const first = await page.evaluate(() => document.activeElement && document.activeElement.getAttribute("href"));
  if (!autofocused && first !== "#content") problems.push(`first Tab focuses ${first}, not the skip link`);
  for (let i = 0; i < TABS; i++) {
    await page.keyboard.press("Tab");
    const f = await page.evaluate(() => {
      const el = document.activeElement;
      if (!el || el === document.body) return null;
      const cs = getComputedStyle(el);
      const r = el.getBoundingClientRect();
      const header = document.querySelector("body > header, .site-header, header");
      const hb = header && !header.contains(el) && getComputedStyle(header).position === "sticky" ? header.getBoundingClientRect().bottom : 0;
      const name = el.tagName.toLowerCase() + (el.id ? "#" + el.id : "") + (el.className && typeof el.className === "string" ? "." + el.className.trim().split(/\s+/).join(".") : "") + " " + (el.textContent || el.getAttribute("aria-label") || "").trim().slice(0, 30);
      const indicator = (cs.outlineStyle !== "none" && parseFloat(cs.outlineWidth) > 0) || cs.boxShadow !== "none";
      const hidden = r.width === 0 && r.height === 0;
      return { name, indicator, obscured: !hidden && r.bottom <= hb + 1, hidden };
    });
    if (!f) break;
    if (f.hidden) continue;
    if (!f.indicator) problems.push(`no visible focus indicator on ${f.name}`);
    if (f.obscured) problems.push(`focused ${f.name} is hidden under the sticky header`);
  }
  await page.close();
  return { path, label, keyboard: problems };
}

const browser = await launch();
const results = [];
for (const [scheme, viewport] of [["light", { width: 1280, height: 900 }], ["dark", { width: 1280, height: 900 }], ["light", { width: 390, height: 844 }]]) {
  const label = `${scheme}${viewport.width < 600 ? ", phone" : ""}`;
  const guest = await browser.newContext({ colorScheme: scheme, viewport, reducedMotion: "reduce", bypassCSP: true });
  for (const p of GUEST) results.push(await audit(guest, p, "guest, " + label));
  for (const p of ["/", "/forum/3", "/thread/1", "/member/login"]) results.push(await keyboard(guest, p, "guest, " + label));
  await guest.close();
  const admin = await browser.newContext({ colorScheme: scheme, viewport, reducedMotion: "reduce", bypassCSP: true });
  await login(admin);
  for (const p of [...MEMBER, ...ADMIN]) results.push(await audit(admin, p, "admin, " + label));
  for (const p of ["/thread/1", "/newthread/3", "/usercp"]) results.push(await keyboard(admin, p, "admin, " + label));
  await admin.close();
}
await browser.close();

let total = 0;
const byRule = new Map();
for (const r of results) {
  if (r.skipped) { console.log(`skip ${r.path} (${r.label}): ${r.skipped}`); continue; }
  if (r.keyboard) {
    for (const k of r.keyboard) {
      total++;
      const e = byRule.get("keyboard") || { impact: "serious", help: "Keyboard focus is visible and not obscured", pages: [], examples: [] };
      e.pages.push(`${r.path} (${r.label})`);
      if (e.examples.length < 10) e.examples.push({ target: r.path, summary: k });
      byRule.set("keyboard", e);
    }
    continue;
  }
  for (const v of r.violations) {
    total += v.count;
    const e = byRule.get(v.id) || { impact: v.impact, help: v.help, pages: [], examples: v.nodes };
    e.pages.push(`${r.path} (${r.label}, ${v.count})`);
    if (process.env.AXE_ALL && e.examples !== v.nodes) e.examples.push(...v.nodes);
    byRule.set(v.id, e);
  }
}
for (const [id, e] of byRule) {
  console.log(`\n✗ ${id} [${e.impact}] ${e.help}`);
  console.log("  pages: " + e.pages.join("; "));
  for (const n of e.examples) console.log(`  - ${n.target}\n    ${n.summary.replace(/\n/g, "\n    ")}`);
}
const audited = results.filter((r) => !r.skipped && !r.keyboard).length;
console.log(`\n${audited} page views audited, ${byRule.size} rules violated, ${total} elements`);
process.exit(byRule.size ? 1 : 0);
