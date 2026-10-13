#!/usr/bin/env node
// Run against a disposable installed board: node tests/a11y/mobile-cp.mjs URL PASSWORD
import assert from "node:assert/strict";
import { chromium } from "playwright";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const base = (process.argv[2] || "http://127.0.0.1:8099").replace(/\/$/, "");
const password = process.argv[3] || "admin12345";
const browser = await chromium.launch(process.env.CHROMIUM_PATH ? { executablePath: process.env.CHROMIUM_PATH } : {});
const context = await browser.newContext({ viewport: { width: 390, height: 844 }, reducedMotion: "reduce", bypassCSP: true });
// Verify an isolated deployment's assets while using a local server for page data.
if (process.env.CP_ASSET_ROOT) {
  for (const [asset, contentType] of [["css/rbb.css", "text/css"], ["js/rbb.js", "application/javascript"]]) {
    await context.route(`**/static/${asset}*`, (route) => route.fulfill({
      contentType, body: readFileSync(`${process.env.CP_ASSET_ROOT}/${asset}`),
    }));
  }
}
const page = await context.newPage();
const errors = [];
page.on("pageerror", (error) => errors.push(error.message));
const paths = [
  "/admin", "/admin/settings", "/admin/settings/images", "/admin/settings/system",
  "/admin/forums", "/admin/forums/3/permissions", "/admin/users", "/admin/users/1?tab=details",
  "/admin/users/1?tab=ips", "/admin/groups", "/admin/groups/edit?gid=2", "/admin/themes",
  "/admin/automod", "/admin/badges/edit", "/admin/promotions/edit", "/admin/tools/tasks",
  "/admin/tools/adminlog", "/admin/tools/systemlog", "/admin/tools/cache",
  "/usercp", "/usercp/profile", "/usercp/options", "/usercp/security", "/usercp/activity",
  "/usercp/avatar", "/usercp/signature", "/usercp/subscriptions", "/usercp/forumsubscriptions",
  "/usercp/attachments", "/usercp/usergroups", "/usercp/lists", "/usercp/notepad", "/usercp/pgp",
];

async function checkOverflow(label) {
  const overflow = await page.evaluate(() => {
    const offenders = [];
    if (document.documentElement.scrollWidth > innerWidth + 1) offenders.push("page");
    document.querySelectorAll(".cp-navigation, .cp-content, .cp-content .tabs, .cp-content table, .cp-content td, .cp-content .btn").forEach((el) => {
      if (!el.getClientRects().length) return;
      const rect = el.getBoundingClientRect();
      if (rect.width > 1 && (rect.right > innerWidth + 1 || rect.left < -1 || el.scrollWidth > el.clientWidth + 1)) {
        offenders.push(`${el.tagName}.${el.className}: ${el.textContent.trim().slice(0, 60)}`);
      }
    });
    return offenders;
  });
  assert.deepEqual(overflow, [], `${label}: horizontal overflow`);
}

try {
  await page.goto(base + "/member/login");
  await page.locator("input[name=username]").fill("admin");
  await page.locator("input[name=password]").fill(password);
  await Promise.all([page.waitForNavigation(), page.locator("form.auth-form button[type=submit]").click()]);
  await page.goto(base + "/admin");
  if (await page.locator("form[action='/admin/verify']").count()) {
    await page.locator("form[action='/admin/verify'] input[name=password]").fill(password);
    await Promise.all([page.waitForNavigation(), page.locator("form[action='/admin/verify'] button[type=submit]").click()]);
  }

  for (const width of [320, 390, 768, 860]) {
    await page.setViewportSize({ width, height: 844 });
    for (const path of paths) {
      const response = await page.goto(base + path);
      assert.equal(response.status(), 200, path);
      await page.locator(".cp-content").waitFor();
      assert.equal(await page.locator(".cp-navigation").evaluate((el) => el.open), false, path);
      await checkOverflow(`${width}px ${path}`);
      await page.locator(".cp-navigation > summary").click();
      assert.equal(await page.locator(".cp-navigation .sidenav").isVisible(), true);
      await checkOverflow(`${width}px ${path} expanded menu`);
      await page.keyboard.press("Escape");
      assert.equal(await page.locator(".cp-navigation").evaluate((el) => el.open), false);
    }
  }

  // An unbroken subject/filename must wrap without clipping the record or its actions.
  await page.setViewportSize({ width: 320, height: 844 });
  await page.goto(base + "/admin/users");
  await page.locator("tbody td a").first().evaluate((el) => { el.textContent = "long-name-".repeat(30); });
  await checkOverflow("long member name");
  await page.screenshot({ path: "/tmp/rbb-mobile-admin.png", fullPage: true });
  await page.goto(base + "/usercp/security");
  await page.locator(".twofa-setup > summary").click();
  await checkOverflow("expanded two-factor setup");
  await page.screenshot({ path: "/tmp/rbb-mobile-usercp.png", fullPage: true });

  await page.addScriptTag({ content: readFileSync(require.resolve("axe-core/axe.min.js"), "utf8") });
  const violations = await page.evaluate(async () => (await axe.run(document.querySelector(".cp-layout"), {
    runOnly: { type: "tag", values: ["wcag2a", "wcag2aa", "wcag21aa", "wcag22aa"] },
  })).violations.map((v) => ({ id: v.id, nodes: v.nodes.map((n) => n.target) })));
  assert.deepEqual(violations, [], "CP accessibility");

  // Resizing back to desktop restores the sidebar and normal table layout.
  await page.setViewportSize({ width: 1280, height: 900 });
  assert.equal(await page.locator(".cp-navigation").evaluate((el) => el.open), true);
  assert.equal(await page.locator(".cp-navigation > summary").isVisible(), false);
  assert.equal(await page.locator("table.grid").first().evaluate((el) => getComputedStyle(el).display), "table");
  assert.deepEqual(errors, [], "browser JavaScript errors");
  console.log(`Passed ${paths.length * 4} CP page/viewport checks, menu interaction, long text, accessibility, and desktop resize.`);
} finally {
  await browser.close();
}
