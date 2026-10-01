#!/usr/bin/env node
// Realistic traffic simulator for rbb: logged-in members (each with their own session) and guests
// browse a board the way people do, with think time between clicks, an open live-updates stream
// per member, and the occasional reply. With --ramp it keeps adding members until the board can't
// keep up, while watching the server process and Postgres, and reports where it broke.
//
//   node tests/simulate.mjs [options]
//
//   --base URL          board to hit                         (default http://127.0.0.1:8090)
//   --users N           logged-in members to start with      (default 200)
//   --guests N          anonymous visitors                   (default 50)
//   --duration S        seconds of browsing (without --ramp) (default 60)
//   --think MS          average pause between clicks, 0 = as fast as possible (default 1500)
//   --writes R          chance a member action is a reply    (default 0.02)
//   --first-user N      first seeded account (userN)         (default 1)
//   --password P        seeded password                      (default password123)
//   --login-concurrency N  parallel log-ins across workers   (default 16)
//   --workers N         client processes (default: half the CPU cores). One Node process tops
//                       out around 1,000 req/s of HTML, so big runs need several.
//   --no-live           don't hold /live streams open
//   --ramp              keep adding members until the board can't keep up:
//   --step N              members added per ramp step        (default 250)
//   --step-secs S         seconds per ramp step              (default 20)
//   --max-users N         stop the ramp here                 (default 20000)
//   --slo-p95 MS          stop after two 5 s windows above this p95 (default 1000)
//   --slo-errors R        …or above this error rate          (default 0.02)
//   --pg URL            Postgres to watch                    (default postgres://rbb@127.0.0.1:5433/rbb_load)
//   --pid PID           rbb process to watch (default: whoever listens on the --base port)
//   --same-ip           send every request from this machine's IP. By default each virtual user
//                       gets its own X-Forwarded-For address, like real visitors; the server must
//                       run with RBB_TRUST_PROXY=true for that, or its per-IP login limit (20 per
//                       5 minutes) stops most log-ins.
//
// Works against a board seeded with `rbb seed` (accounts user1…userN). Needs Node 18+.

import { fork, execFile } from "node:child_process";
import { availableParallelism } from "node:os";
import { fileURLToPath } from "node:url";

const args = process.argv.slice(2);
const opt = (name, def) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 && args[i + 1] !== undefined && !args[i + 1].startsWith("--") ? args[i + 1] : def;
};
const O = {
  base: opt("base", "http://127.0.0.1:8090").replace(/\/$/, ""),
  users: Number(opt("users", 200)),
  guests: Number(opt("guests", 50)),
  duration: Number(opt("duration", 60)) * 1000,
  think: Number(opt("think", 1500)),
  writes: Number(opt("writes", 0.02)),
  first: Number(opt("first-user", 1)),
  password: opt("password", "password123"),
  loginConc: Number(opt("login-concurrency", 16)),
  workers: Number(opt("workers", Math.max(1, Math.floor(availableParallelism() / 2)))),
  live: !args.includes("--no-live"),
  spoof: !args.includes("--same-ip"),
  ramp: args.includes("--ramp"),
  step: Number(opt("step", 250)),
  stepSecs: Number(opt("step-secs", 20)),
  maxUsers: Number(opt("max-users", 20000)),
  sloP95: Number(opt("slo-p95", 1000)),
  sloErr: Number(opt("slo-errors", 0.02)),
  pg: opt("pg", "postgres://rbb@127.0.0.1:5433/rbb_load"),
  pid: opt("pid", ""),
};

// ------------------------------------------------------------------ latency histograms
// Log buckets (4% wide) from 0.05 ms to ~2 min: mergeable across processes, cheap to ship.

const B0 = 0.05, BR = Math.log(1.04), NB = 380;
const newHist = () => new Array(NB).fill(0);
const bucket = (ms) => Math.max(0, Math.min(NB - 1, Math.floor(Math.log(Math.max(ms, B0) / B0) / BR)));
const bucketMs = (i) => B0 * Math.exp((i + 0.5) * BR);
const histAdd = (h, o) => { for (let i = 0; i < NB; i++) h[i] += o[i]; };
const histCount = (h) => h.reduce((a, b) => a + b, 0);
function histPct(h, p) {
  const n = histCount(h);
  if (!n) return 0;
  const target = Math.ceil((p / 100) * n);
  let acc = 0;
  for (let i = 0; i < NB; i++) if ((acc += h[i]) >= target) return bucketMs(i);
  return bucketMs(NB - 1);
}
const fmt = (ms) => (ms >= 100 ? ms.toFixed(0) : ms.toFixed(1)).padStart(6);

if (process.env.RBB_SIM_WORKER) await worker();
else await coordinator();

// ================================================================== worker process

async function worker() {
  let pools = null, running = true;
  const abort = new AbortController();
  const loops = [];
  let stats = new Map(); // label -> { h, n, errors, hits, codes }
  let lag = 0;
  {
    let last = performance.now();
    setInterval(() => { const now = performance.now(); lag = Math.max(lag, now - last - 100); last = now; }, 100).unref();
  }
  const record = (label, ms, status, hit) => {
    let s = stats.get(label);
    if (!s) stats.set(label, (s = { h: newHist(), n: 0, errors: 0, hits: 0, codes: {} }));
    s.h[bucket(ms)]++;
    s.n++;
    if (status === 0 || status >= 400) { s.errors++; s.codes[status] = (s.codes[status] || 0) + 1; }
    if (hit) s.hits++;
  };
  const flush = () => {
    const out = {};
    for (const [k, v] of stats) out[k] = v;
    process.send({ type: "stats", stats: out, lag });
    stats = new Map();
    lag = 0;
  };
  setInterval(flush, 1000).unref(); // ship stats to the coordinator every second

  let ipSeq = 0;
  class Client {
    constructor(name) {
      this.name = name;
      this.cookies = new Map();
      this.csrf = "";
      const n = Number(process.env.RBB_SIM_WORKER) * 1_000_000 + ++ipSeq;
      this.ip = `10.${(n >> 16) & 255}.${(n >> 8) & 255}.${n & 255}`;
    }
    header() { return [...this.cookies].map(([k, v]) => `${k}=${v}`).join("; "); }
    async req(label, path, { form, redirect = "manual", parse = false } = {}) {
      const headers = { Cookie: this.header(), "User-Agent": "Mozilla/5.0 (rbb-simulate)", Accept: "text/html" };
      if (O.spoof) headers["X-Forwarded-For"] = this.ip;
      let body;
      if (form) {
        body = new URLSearchParams({ my_post_key: this.csrf, ...form });
        headers["Content-Type"] = "application/x-www-form-urlencoded";
      }
      const t0 = performance.now();
      let status = 0, text = "", hit = false;
      try {
        const r = await fetch(O.base + path, { method: form ? "POST" : "GET", headers, body, redirect });
        // Decoding 50 KB of HTML per click is most of a client's CPU; only do it when the page's
        // CSRF token is needed (log-ins, replies).
        const buf = await r.arrayBuffer();
        if (parse) text = new TextDecoder().decode(buf);
        status = r.status;
        hit = r.headers.get("x-rbb-cache") === "hit";
        for (const c of r.headers.getSetCookie()) {
          const kv = c.split(";")[0];
          const i = kv.indexOf("=");
          if (/max-age=0/i.test(c)) this.cookies.delete(kv.slice(0, i));
          else this.cookies.set(kv.slice(0, i), kv.slice(i + 1));
        }
      } catch (e) { status = 0; }
      if (label) record(label, performance.now() - t0, status, hit);
      const i = text.indexOf('name="csrf-token" content="');
      if (i >= 0) this.csrf = text.slice(i + 27, text.indexOf('"', i + 27));
      return { status, text };
    }
  }

  const pick = (a) => a[Math.floor(Math.random() * a.length)];
  const weighted = (table) => {
    const total = table.reduce((n, [, w]) => n + w, 0);
    let r = Math.random() * total;
    for (const [v, w] of table) if ((r -= w) <= 0) return v;
    return table[0][0];
  };
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const think = () => (O.think > 0 ? sleep(O.think * (0.3 + Math.random() * 1.4)) : Promise.resolve());

  async function browseOnce(c, member) {
    const who = member ? "member" : "guest";
    const action = weighted(member
      ? [["index", 12], ["forum", 25], ["thread", 36], ["profile", 7], ["whats new", 6], ["inbox", 3], ["member list", 2], ["alerts", 3], ...(O.writes > 0 ? [["reply", O.writes * 100]] : [])]
      : [["index", 15], ["forum", 30], ["thread", 45], ["profile", 10]]);
    switch (action) {
      case "index": return c.req(`${who} · index`, "/");
      case "forum": {
        const [fid, max] = pick(pools.forums);
        const page = Math.random() < 0.7 ? 1 : 1 + Math.floor(Math.random() * Math.min(max, 50));
        return c.req(`${who} · forum${page > 1 ? " (deep page)" : ""}`, `/forum/${fid}${page > 1 ? `?page=${page}` : ""}`);
      }
      case "thread": {
        const tid = pick(pools.threads);
        return Math.random() < 0.8 ? c.req(`${who} · thread`, `/thread/${tid}`) : c.req(`${who} · thread (last post)`, `/thread/${tid}/lastpost`, { redirect: "follow" });
      }
      case "profile": return c.req(`${who} · profile`, `/user/${pick(pools.people)}`);
      case "whats new": return c.req("member · what's new", "/search/new", { redirect: "follow" });
      case "inbox": return c.req("member · inbox", "/pm");
      case "member list": return c.req("member · member list", "/members");
      case "alerts": return c.req("member · alerts", "/usercp/alerts");
      case "reply": {
        const tid = pick(pools.threads);
        await c.req(null, `/thread/${tid}`, { parse: true }); // a fresh CSRF token, like a real page view first
        return c.req("member · post reply", `/newreply/${tid}`, { form: { message: `Simulated reply ${Math.random().toString(36).slice(2)} :)`, quickreply: "1" } });
      }
    }
  }
  async function liveStream(c) {
    // Like the browser's EventSource on board pages: one long-lived connection per member.
    try {
      const headers = { Cookie: c.header(), Accept: "text/event-stream" };
      if (O.spoof) headers["X-Forwarded-For"] = c.ip;
      const r = await fetch(O.base + "/live", { headers, signal: abort.signal });
      const reader = r.body.getReader();
      while (running) { const { done } = await reader.read(); if (done) break; }
    } catch (e) { /* aborted at the end */ }
  }
  const loop = async (c, member) => {
    await sleep(Math.random() * Math.max(O.think, 200)); // don't start in lockstep
    while (running) { await browseOnce(c, member); await think(); }
  };
  async function login(i) {
    const c = new Client(`user${i}`);
    await c.req(null, "/member/login", { parse: true });
    const r = await c.req("member · log in", "/member/login", { form: { username: c.name, password: O.password, remember: "1" } });
    if (r.status !== 303) return null;
    await c.req(null, "/", { parse: true });
    return c;
  }

  process.on("message", async (m) => {
    if (m.type === "init") {
      pools = m.pools;
      for (let i = 0; i < m.guests; i++) loops.push(loop(new Client(`guest${i}`), false));
    } else if (m.type === "add") {
      let next = m.start, ok = 0, failed = 0;
      const end = m.start + m.count;
      await Promise.all(Array.from({ length: Math.max(1, m.concurrency) }, async () => {
        while (next < end && running) {
          const c = await login(next++);
          if (!c) { failed++; continue; }
          ok++;
          if (O.live) liveStream(c);
          loops.push(loop(c, true));
        }
      }));
      process.send({ type: "added", id: m.id, ok, failed });
    } else if (m.type === "stop") {
      running = false;
      abort.abort();
      await Promise.race([Promise.allSettled(loops), sleep(5000)]);
      flush();
      process.exit(0);
    }
  });
}

// ================================================================== coordinator

async function coordinator() {
  const sh = (cmd, a) => new Promise((res) => execFile(cmd, a, { env: { ...process.env, PATH: `/opt/homebrew/opt/postgresql@17/bin:/usr/sbin:/usr/bin:/bin:${process.env.PATH}` }, timeout: 4000 }, (e, out) => res(e ? "" : String(out).trim())));
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const serverPid = O.pid || (await sh("lsof", [`-tiTCP:${new URL(O.base).port || 80}`, "-sTCP:LISTEN"])).split("\n")[0];

  async function sampleServer() {
    const out = { cpu: NaN, rss: NaN, pgActive: NaN, pgLock: NaN, pgIdleTx: NaN, wait: "" };
    if (serverPid) {
      const [cpu, rss] = (await sh("ps", ["-o", "%cpu=,rss=", "-p", serverPid])).split(/\s+/).map(Number);
      Object.assign(out, { cpu, rss: rss / 1024 });
    }
    const q = await sh("psql", [O.pg, "-Atc", `SELECT count(*) FILTER (WHERE state = 'active'), count(*) FILTER (WHERE wait_event_type = 'Lock'), count(*) FILTER (WHERE state LIKE 'idle in trans%'),
      (SELECT string_agg(w, ' ') FROM (SELECT wait_event_type || ':' || wait_event || '×' || count(*) AS w FROM pg_stat_activity WHERE datname = current_database() AND state = 'active' AND wait_event IS NOT NULL AND pid <> pg_backend_pid() GROUP BY wait_event_type, wait_event ORDER BY count(*) DESC LIMIT 2) x)
      FROM pg_stat_activity WHERE datname = current_database() AND pid <> pg_backend_pid()`]);
    if (q) {
      const [a, l, t, w] = q.split("|");
      Object.assign(out, { pgActive: Number(a), pgLock: Number(l), pgIdleTx: Number(t), wait: w || "" });
    }
    return out;
  }

  // Discover forums (with page counts), threads and members from the board itself.
  const pools = { forums: [], threads: [], people: [] };
  {
    const get = async (p) => { try { return (await (await fetch(O.base + p, { headers: { "X-Forwarded-For": "10.254.0.1" } })).text()).replace(/&#x2f;/gi, "/"); } catch (e) { return ""; } };
    const idx = await get("/");
    const fids = [...new Set([...idx.matchAll(/href="\/forum\/(\d+)/g)].map((m) => Number(m[1])))];
    const threads = new Set(), people = new Set();
    for (const fid of fids.slice(0, 100)) {
      const html = await get(`/forum/${fid}`);
      const pages = Math.max(1, ...[...html.matchAll(/\?page=(\d+)/g)].map((m) => Number(m[1])));
      pools.forums.push([fid, pages]);
      for (const m of html.matchAll(/href="\/thread\/(\d+)/g)) threads.add(Number(m[1]));
      for (const m of html.matchAll(/href="\/user\/(\d+)/g)) people.add(Number(m[1]));
    }
    pools.threads = [...threads];
    pools.people = [...people];
    console.log(`rbb simulate → ${O.base}: ${O.users} members${O.ramp ? ` +${O.step} every ${O.stepSecs}s up to ${O.maxUsers} (stop when p95 > ${O.sloP95} ms or errors > ${O.sloErr * 100}%)` : ""}, ${O.guests} guests, think ~${O.think} ms, writes ${(O.writes * 100).toFixed(1)}%${O.live ? ", live streams" : ""}, ${O.workers} client processes${serverPid ? `, watching pid ${serverPid}` : ""}`);
    console.log(`discovered ${pools.forums.length} forums, ${pools.threads.length} threads, ${pools.people.length} members`);
    if (!pools.forums.length || !pools.threads.length) { console.log("Nothing to browse: is the board up, with forums and threads?"); process.exit(1); }
  }

  // Start the client processes.
  const self = fileURLToPath(import.meta.url);
  const workers = [];
  const totals = new Map(); // label -> { h, n, errors, hits, codes }
  let win = { h: newHist(), n: 0, errors: 0 }, winLag = 0;
  const merge = (label, s) => {
    let t = totals.get(label);
    if (!t) totals.set(label, (t = { h: newHist(), n: 0, errors: 0, hits: 0, codes: {} }));
    histAdd(t.h, s.h); t.n += s.n; t.errors += s.errors; t.hits += s.hits;
    for (const [c, n] of Object.entries(s.codes)) t.codes[c] = (t.codes[c] || 0) + n;
    // Judge page views, not log-ins (argon2 is slow on purpose).
    if (!label.endsWith("log in")) { histAdd(win.h, s.h); win.n += s.n; win.errors += s.errors; }
  };
  const waiting = new Map();
  let addSeq = 0;
  for (let i = 0; i < O.workers; i++) {
    const w = fork(self, process.argv.slice(2), { env: { ...process.env, RBB_SIM_WORKER: String(i + 1) } });
    w.on("message", (m) => {
      if (m.type === "stats") { for (const [k, v] of Object.entries(m.stats)) merge(k, v); winLag = Math.max(winLag, m.lag); }
      else if (m.type === "added") { waiting.get(m.id)?.(m); waiting.delete(m.id); }
    });
    w.send({ type: "init", pools, guests: Math.floor(O.guests / O.workers) + (i < O.guests % O.workers ? 1 : 0) });
    workers.push(w);
  }
  let nextUser = 0, members = 0, loginFailed = 0;
  async function addMembers(n) {
    n = Math.min(n, O.maxUsers - nextUser);
    if (n <= 0) return;
    const per = Math.ceil(n / workers.length);
    const conc = Math.max(1, Math.round(O.loginConc / workers.length));
    const results = await Promise.all(workers.map((w, i) => {
      const count = Math.max(0, Math.min(per, n - i * per));
      if (!count) return { ok: 0, failed: 0 };
      const id = ++addSeq;
      return new Promise((res) => { waiting.set(id, res); w.send({ type: "add", id, start: O.first + nextUser + i * per, count, concurrency: conc }); });
    }));
    nextUser += n;
    for (const r of results) { members += r.ok; loginFailed += r.failed; }
  }

  const t0 = performance.now();
  await addMembers(O.users);
  console.log(`logged in ${members} members in ${((performance.now() - t0) / 1000).toFixed(1)}s${loginFailed ? ` (${loginFailed} failed)` : ""}`);
  if (loginFailed > members && O.spoof) console.log("Most log-ins failed. If the server doesn't run with RBB_TRUST_PROXY=true it sees one IP and its login limit applies; restart it with that, or use fewer --users.");

  const started = performance.now();
  const steps = [];
  const newStep = () => ({ users: members, h: newHist(), n: 0, errors: 0, cpu: [], pg: [], lock: 0, from: performance.now() });
  let step = newStep(), breaches = 0, stopReason = "";
  console.log(`\n   t  members   req/s    p50     p95     p99   err  │ rbb cpu    rss │ pg active lock idle-tx │ client lag │ top db waits`);
  const ticker = setInterval(async () => {
    const w = win, lag = winLag;
    win = { h: newHist(), n: 0, errors: 0 }; winLag = 0;
    const srv = await sampleServer();
    const p95 = histPct(w.h, 95);
    histAdd(step.h, w.h); step.n += w.n; step.errors += w.errors; step.cpu.push(srv.cpu); step.pg.push(srv.pgActive); step.lock = Math.max(step.lock, srv.pgLock || 0);
    const t = ((performance.now() - started) / 1000).toFixed(0).padStart(4);
    console.log(`${t}  ${String(members).padStart(7)}  ${String(Math.round(w.n / 5)).padStart(6)} ${fmt(histPct(w.h, 50))} ${fmt(p95)} ${fmt(histPct(w.h, 99))} ${String(w.errors).padStart(5)}  │ ${(srv.cpu || 0).toFixed(0).padStart(5)}% ${(srv.rss || 0).toFixed(0).padStart(5)}M │ ${String(srv.pgActive ?? "-").padStart(9)} ${String(srv.pgLock ?? "-").padStart(4)} ${String(srv.pgIdleTx ?? "-").padStart(7)} │ ${lag.toFixed(0).padStart(7)} ms │ ${srv.wait}`);
    if (lag > 300) console.log(`         ⚠ a client process lagged ${lag.toFixed(0)} ms: add --workers; the numbers above are partly limited by this machine's client`);
    const errRate = w.n ? w.errors / w.n : 0;
    const warm = performance.now() - started > O.stepSecs * 1000; // the first step is warm-up
    if (O.ramp && warm && (p95 > O.sloP95 || errRate > O.sloErr)) {
      if (++breaches >= 2) stopReason = p95 > O.sloP95 ? `p95 ${p95.toFixed(0)} ms > ${O.sloP95} ms` : `error rate ${(errRate * 100).toFixed(1)}% > ${O.sloErr * 100}%`;
    } else breaches = 0;
  }, 5000);

  const closeStep = () => {
    const secs = (performance.now() - step.from) / 1000;
    const avg = (a) => { const v = a.filter((x) => !Number.isNaN(x)); return v.length ? v.reduce((x, y) => x + y, 0) / v.length : NaN; };
    steps.push({ users: step.users, rps: step.n / secs, p50: histPct(step.h, 50), p95: histPct(step.h, 95), p99: histPct(step.h, 99), err: step.n ? step.errors / step.n : 0, cpu: avg(step.cpu), pg: avg(step.pg), lock: step.lock });
  };
  if (O.ramp) {
    while (!stopReason && nextUser < O.maxUsers) {
      for (let s = 0; s < O.stepSecs && !stopReason; s++) await sleep(1000);
      closeStep();
      if (stopReason) break;
      step = newStep();
      await addMembers(O.step);
      step.users = members;
    }
    if (!stopReason && step.n) closeStep();
  } else {
    await sleep(O.duration);
    closeStep();
  }
  clearInterval(ticker);
  await Promise.all(workers.map((w) => new Promise((res) => { w.once("exit", res); w.send({ type: "stop" }); setTimeout(() => { w.kill(); res(); }, 8000); })));
  const secs = (performance.now() - started) / 1000;

  if (O.ramp) {
    console.log(`\nramp stopped: ${stopReason || (nextUser >= O.maxUsers ? `reached --max-users ${O.maxUsers}` : "done")}`);
    console.log(`\n members    req/s     p50     p95     p99   errors  rbb cpu  pg active  lock waits`);
    for (const r of steps) console.log(`${String(r.users).padStart(8)} ${r.rps.toFixed(0).padStart(8)} ${fmt(r.p50)} ${fmt(r.p95)} ${fmt(r.p99)} ${(r.err * 100).toFixed(1).padStart(7)}% ${(r.cpu || 0).toFixed(0).padStart(7)}% ${(r.pg || 0).toFixed(1).padStart(10)} ${String(r.lock).padStart(11)}`);
    const ok = steps.slice(1).filter((r) => r.p95 <= O.sloP95 && r.err <= O.sloErr);
    const best = ok[ok.length - 1];
    if (best) console.log(`\ncapacity within SLO (p95 ≤ ${O.sloP95} ms, errors ≤ ${O.sloErr * 100}%): ${best.users} active members, ${best.rps.toFixed(0)} req/s`);
  }

  console.log(`\n${"page".padEnd(30)} ${"requests".padStart(9)} ${"req/s".padStart(8)} ${"p50".padStart(8)} ${"p95".padStart(8)} ${"p99".padStart(8)}  errors  cache`);
  const all = newHist();
  let total = 0, errors = 0;
  for (const [label, s] of [...totals].sort((a, b) => b[1].n - a[1].n)) {
    histAdd(all, s.h); total += s.n; errors += s.errors;
    const codes = Object.entries(s.codes).map(([c, n]) => `${c === "0" ? "net" : c}×${n}`).join(" ");
    const rate = label.endsWith("log in") ? "" : (s.n / secs).toFixed(1);
    console.log(`${label.padEnd(30)} ${String(s.n).padStart(9)} ${rate.padStart(8)} ${fmt(histPct(s.h, 50))} ms ${fmt(histPct(s.h, 95))} ms ${fmt(histPct(s.h, 99))} ms  ${String(s.errors).padStart(6)}  ${s.hits ? `${Math.round((s.hits * 100) / s.n)}% hit` : ""}${codes ? `  (${codes})` : ""}`);
  }
  console.log(`${"all".padEnd(30)} ${String(total).padStart(9)} ${(total / secs).toFixed(1).padStart(8)} ${fmt(histPct(all, 50))} ms ${fmt(histPct(all, 95))} ms ${fmt(histPct(all, 99))} ms  ${String(errors).padStart(6)}`);
  process.exit(errors > total * 0.01 ? 1 : 0);
}
