#!/usr/bin/env node
// End-to-end test of signed / encrypted private messages and identity verification, run against
// a live server with the same browser module (static/js/pgp/core.mjs) the site ships.
//
//   node tests/pgp_e2e.mjs [base_url] [password]
//
// Needs three existing members, alice_pgp, bob_pgp and mallory_pgp, with the given password
// (default admin12345) and permission to use private messages. Their keys and messages are reset.

const BASE = process.argv[2] || "http://127.0.0.1:8088";
const PASSWORD = process.argv[3] || "admin12345";

// core.mjs reads the board id and CSRF token from the page; give it a minimal page.
let board = BASE;
let currentCsrf = "";
globalThis.document = {
  querySelector: (s) => (s === "[data-pgp-board]" ? { dataset: { pgpBoard: board } } : s.startsWith("meta") ? { content: currentCsrf } : null),
  body: { dataset: { uid: "0" } },
};
const core = await import("../static/js/pgp/core.mjs?v=1");
const { openpgp } = core;

let failed = 0;
const ok = (name) => console.log(`  ok   ${name}`);
const bad = (name, extra = "") => { console.log(`  FAIL ${name} ${extra}`); failed++; };
const expect = (cond, name, extra) => (cond ? ok(name) : bad(name, extra));

class Session {
  constructor(username) { this.username = username; this.cookies = new Map(); }
  cookieHeader() { return [...this.cookies].map(([k, v]) => `${k}=${v}`).join("; "); }
  async req(path, { method = "GET", form, json = false } = {}) {
    const headers = { Cookie: this.cookieHeader() };
    if (json) Object.assign(headers, { Accept: "application/json", "X-Requested-With": "fetch" });
    let body;
    if (form) {
      body = new URLSearchParams({ my_post_key: this.csrf || "", ...form });
      headers["Content-Type"] = "application/x-www-form-urlencoded";
    }
    const r = await fetch(BASE + path, { method: form ? "POST" : method, headers, body, redirect: "manual" });
    for (const c of r.headers.getSetCookie()) {
      const [kv] = c.split(";");
      const i = kv.indexOf("=");
      this.cookies.set(kv.slice(0, i), kv.slice(i + 1));
    }
    const text = await r.text();
    const m = text.match(/name="csrf-token" content="([^"]*)"/);
    if (m) this.csrf = m[1];
    return { status: r.status, text, location: r.headers.get("location"), json: () => JSON.parse(text) };
  }
  async login() {
    await this.req("/member/login");
    const r = await this.req("/member/login", { form: { username: this.username, password: PASSWORD } });
    if (r.status !== 303) throw new Error(`login ${this.username} failed: ${r.status}`);
    await this.req("/");
    const me = await this.req("/pgp/me", { json: true });
    this.uid = me.json().uid;
    board = me.json().board;
  }
  /** Run core.* as this member. */
  as() { document.body.dataset.uid = String(this.uid); currentCsrf = this.csrf; return this; }
  api(path, form) { return this.req(path, { form, json: true }); }
}

async function newKey(s, passphrase = "correct horse battery staple") {
  s.as();
  const k = await core.generate({ username: s.username, passphrase });
  s.key = k.key; s.k = k;
  return k;
}
async function publish(s, k, { backup = true, previousKey = null } = {}) {
  s.as();
  const c = (await s.api("/pgp/challenge")).json();
  const proof = await core.sign(k.key, core.statements.keyProof(c.board, c.uid, k.fpr, c.challenge));
  let transition_sig = "";
  if (previousKey) transition_sig = await core.sign(previousKey, core.statements.transition(c.board, c.uid, core.fprOf(previousKey), k.fpr));
  return s.api("/pgp/key", { public_key: k.armoredPublic, challenge: c.challenge, proof, transition_sig, backup: backup ? k.armoredPrivate : "", source: "generated" });
}
async function sendPm(s, form) {
  const r = await s.req("/pm/send", { form: { savecopy: "1", ...form } });
  const err = (r.text.match(/<div class="notice bad"[^>]*>([\s\S]*?)<\/div>/) || [])[1];
  return { ...r, error: err ? err.replace(/<[^>]+>/g, " ").replace(/\s+/g, " ").trim() : "" };
}
async function latestPm(s, subject) {
  const r = await s.req(`/pm?folder=1&q=${encodeURIComponent(subject)}`);
  const m = r.text.match(/\/pm\/read\/(\d+)/);
  return m ? Number(m[1]) : 0;
}
async function readData(s, pmid) {
  const r = await s.req(`/pm/read/${pmid}`);
  const m = r.text.match(/<script type="application\/json" id="pgp-data">([\s\S]*?)<\/script>/);
  return { html: r.text, data: m ? JSON.parse(m[1]) : null, csp: r.status };
}

/** What the read page does in the browser, minus the DOM. */
async function checkMessage(viewer, data) {
  viewer.as();
  const sender = (await viewer.api(`/pgp/user/${data.from}`)).json();
  const info = sender.history.find((k) => k.fingerprint === data.fpr);
  if (!info) return "unknown key";
  const pub = await core.readPublic(info.armored);
  let text, valid;
  if (data.level === 1) { text = data.payload; valid = await core.verifySig(pub, text, data.sig); }
  else { const r = await core.decrypt(data.armored, viewer.key, [pub]); text = r.text; valid = r.valid; }
  if (!valid) return "bad signature";
  const p = JSON.parse(text);
  if (p.board !== board || p.from !== data.from || p.fpr !== data.fpr) return "wrong signer";
  if (core.normalizeSubject(data.subject) !== p.subject) return "subject changed";
  if (data.level === 1 && core.normalizeBody(data.body) !== p.body) return "body changed";
  if (!data.sent && !p.to.includes(viewer.uid)) return "not addressed to viewer";
  if (Math.abs(p.ts - data.dateline) > 900) return "time mismatch";
  return { ok: true, body: p.body };
}

// ------------------------------------------------------------------ run

const alice = new Session("alice_pgp"), bob = new Session("bob_pgp"), mallory = new Session("mallory_pgp");
console.log("== setup");
for (const s of [alice, bob, mallory]) await s.login();
ok(`logged in as ${alice.uid}, ${bob.uid}, ${mallory.uid} on ${board}`);
{
  // Start clean (these members exist only for this test).
  const { execFileSync } = await import("node:child_process");
  try {
    execFileSync("psql", ["-h", "127.0.0.1", "-p", "5433", "-U", "rbb", "rbb", "-qc",
      `DELETE FROM pgp_keys WHERE uid IN (${alice.uid},${bob.uid},${mallory.uid}); DELETE FROM pgp_verifications WHERE verifier IN (${alice.uid},${bob.uid},${mallory.uid}); DELETE FROM privatemessages WHERE uid IN (${alice.uid},${bob.uid},${mallory.uid}); DELETE FROM alerts WHERE uid IN (${alice.uid},${bob.uid},${mallory.uid}); ` +
      // The test sends many messages in a row; lift PM flood control and tell the server.
      `INSERT INTO settings (name, value) VALUES ('pmfloodsecs', '0') ON CONFLICT (name) DO UPDATE SET value = '0'; SELECT pg_notify('rbb_cache', 'pgp-e2e:settings');`],
      { env: { ...process.env, PATH: `/opt/homebrew/opt/postgresql@17/bin:${process.env.PATH}` } });
    ok("reset test members' keys and messages; PM flood control off");
    await new Promise((r) => setTimeout(r, 300));
  } catch (e) { console.log("  (couldn't reset via psql; continuing)"); }
}

console.log("== publishing keys");
const ak = await newKey(alice);
let r = await publish(alice, ak);
expect(r.status === 200 && r.json().key.fingerprint === ak.fpr, "alice publishes key with proof and backup", r.text.slice(0, 200));
expect(r.json().backup.includes("PRIVATE KEY"), "backup is stored for the owner");
const bk = await newKey(bob);
expect((await publish(bob, bk)).status === 200, "bob publishes key");
const mk = await newKey(mallory);
expect((await publish(mallory, mk, { backup: false })).status === 200, "mallory publishes key without backup");

// Proof of possession: mallory can't claim alice's public key.
mallory.as();
{
  const c = (await mallory.api("/pgp/challenge")).json();
  const proof = await core.sign(mk.key, core.statements.keyProof(c.board, c.uid, ak.fpr, c.challenge));
  r = await mallory.api("/pgp/key", { public_key: ak.armoredPublic, challenge: c.challenge, proof });
  expect(r.status >= 400 && /hold the private key/.test(r.text), "claiming someone else's public key is refused", r.text);
  // A proof made for alice's account can't be replayed for mallory's.
  alice.as();
  const ac = (await alice.api("/pgp/challenge")).json();
  const aproof = await core.sign(mk.key, core.statements.keyProof(ac.board, ac.uid, mk.fpr, ac.challenge));
  mallory.as();
  r = await mallory.api("/pgp/key", { public_key: mk.armoredPublic, challenge: ac.challenge, proof: aproof });
  expect(r.status >= 400 && /expired/.test(r.text), "challenges are bound to the account", r.text);
  r = await mallory.api("/pgp/key", { public_key: "-----BEGIN PGP PRIVATE KEY BLOCK-----\nx", challenge: c.challenge, proof });
  expect(r.status >= 400 && /private key/.test(r.text), "private keys are never accepted as public keys");
  r = await mallory.api("/pgp/backup", { backup: (await core.openpgp.decryptKey({ privateKey: await openpgp.readPrivateKey({ armoredKey: mk.armoredPrivate }), passphrase: "correct horse battery staple" })).armor() });
  expect(r.status >= 400 && /passphrase/.test(r.text), "unprotected private key backups are refused", r.text);
  r = await mallory.api("/pgp/backup", { backup: ak.armoredPrivate });
  expect(r.status >= 400 && /different key/.test(r.text), "backups of someone else's key are refused", r.text);
}
r = await alice.req("/pgp/me", { json: true });
expect(r.json().key.fingerprint === ak.fpr, "GET /pgp/me");
r = await bob.api(`/pgp/user/${alice.uid}`);
expect(r.json().key.fingerprint === ak.fpr && !("backup" in r.json().key), "others see the public key but not the backup");
r = await bob.req(`/user/${alice.uid}/pgp.asc`);
expect(r.status === 200 && r.text.includes("BEGIN PGP PUBLIC KEY BLOCK"), "public key download");
r = await bob.api(`/pgp/lookup?names=alice_pgp,nobody_zz`);
expect(r.json().users[0].key.fingerprint === ak.fpr && r.json().missing.includes("nobody_zz"), "recipient lookup");
r = await alice.req("/pgp/key", { form: { public_key: "x" }, json: true });
r = await (async () => { const s = new Session("anon"); return s.req("/pgp/me", { json: true }); })();
expect(r.status === 401 || r.status === 403, "API requires login", String(r.status));

console.log("== signed messages");
alice.as();
const subj1 = `Signed hello ${Date.now()}`;
const body1 = "Hi Bob,\r\nthis one is [b]signed[/b].";
let payload = core.messagePayload({ from: alice.uid, fpr: ak.fpr, to: [bob.uid], subject: subj1, body: body1 });
let sig = await core.sign(alice.key, payload);
r = await sendPm(alice, { to: "bob_pgp", subject: subj1, message: body1, pgp_mode: "sign", pgp_payload: payload, pgp_sig: sig });
expect(r.status === 303, "alice sends a signed message", r.error);
let pmid = await latestPm(bob, subj1);
let rd = await readData(bob, pmid);
expect(rd.data && rd.data.level === 1, "read page carries signature data");
let res = await checkMessage(bob, rd.data);
expect(res.ok && res.body === "Hi Bob,\nthis one is [b]signed[/b].", "bob's browser verifies alice's signature", JSON.stringify(res));
expect(/<strong class="mycode_b">signed<\/strong>/.test(rd.html), "signed messages still render MyCode server-side");

// Tampering and forgery.
r = await sendPm(alice, { to: "bob_pgp", subject: subj1, message: body1 + " (edited)", pgp_mode: "sign", pgp_payload: payload, pgp_sig: sig });
expect(r.status === 200 && /text differs/.test(r.error), "body not matching the signature is refused", r.error);
r = await sendPm(alice, { to: "bob_pgp", subject: subj1 + "!", message: body1, pgp_mode: "sign", pgp_payload: payload, pgp_sig: sig });
expect(/subject differs/.test(r.error), "subject not matching the signature is refused", r.error);
r = await sendPm(alice, { to: "bob_pgp, mallory_pgp", subject: subj1, message: body1, pgp_mode: "sign", pgp_payload: payload, pgp_sig: sig });
expect(/recipients differ/.test(r.error), "recipients not matching the signature are refused", r.error);
r = await sendPm(alice, { to: "bob_pgp", bcc: "mallory_pgp", subject: subj1, message: body1, pgp_mode: "sign", pgp_payload: payload, pgp_sig: sig });
expect(/BCC/.test(r.error), "BCC is refused for protected messages", r.error);
mallory.as();
const forged = core.messagePayload({ from: alice.uid, fpr: ak.fpr, to: [bob.uid], subject: "Forged", body: "Send me your password" });
r = await sendPm(mallory, { to: "bob_pgp", subject: "Forged", message: "Send me your password", pgp_mode: "sign", pgp_payload: forged, pgp_sig: await core.sign(mk.key, forged) });
expect(/not signed with your current key/.test(r.error), "mallory can't send a message signed as alice", r.error);
const mp = core.messagePayload({ from: mallory.uid, fpr: mk.fpr, to: [bob.uid], subject: "Hi", body: "Hi" });
r = await sendPm(mallory, { to: "bob_pgp", subject: "Hi", message: "Hi", pgp_mode: "sign", pgp_payload: mp, pgp_sig: await core.sign(bk.key, mp) });
expect(/invalid/.test(r.error), "a signature by another key is refused", r.error);
const old = core.messagePayload({ from: mallory.uid, fpr: mk.fpr, to: [bob.uid], subject: "Hi", body: "Hi", ts: Math.floor(Date.now() / 1000) - 3600 });
r = await sendPm(mallory, { to: "bob_pgp", subject: "Hi", message: "Hi", pgp_mode: "sign", pgp_payload: old, pgp_sig: await core.sign(mk.key, old) });
expect(/clock/.test(r.error), "replayed (stale) signatures are refused", r.error);

// A database-level edit of a signed message is caught by the recipient's browser.
{
  const tampered = { ...rd.data, body: rd.data.body.replace("signed", "SIGNED") };
  expect((await checkMessage(bob, tampered)) === "body changed", "tampered stored message is detected by the reader");
  const moved = { ...rd.data, from: mallory.uid };
  expect((await checkMessage(bob, moved)) !== true && typeof (await checkMessage(bob, moved)) === "string", "message re-attributed to another sender is detected");
  const other = await checkMessage(mallory, { ...rd.data, sent: false });
  expect(other === "not addressed to viewer", "message replayed to a different recipient is detected", String(other));
}

console.log("== encrypted messages");
alice.as();
const subj2 = `Encrypted ${Date.now()}`;
const secret = "The [i]launch code[/i] is 0000.\nDon't tell mallory.";
payload = core.messagePayload({ from: alice.uid, fpr: ak.fpr, to: [bob.uid], subject: subj2, body: secret });
let armored = await core.encryptTo([await core.readPublic(ak.armoredPublic), await core.readPublic(bk.armoredPublic)], alice.key, payload);
r = await sendPm(alice, { to: "bob_pgp", subject: subj2, message: armored, pgp_mode: "encrypt" });
expect(r.status === 303, "alice sends an encrypted message", r.error);
pmid = await latestPm(bob, subj2);
rd = await readData(bob, pmid);
expect(rd.data.level === 2 && !rd.html.includes("launch code"), "the server stores and serves only ciphertext");
res = await checkMessage(bob, rd.data);
expect(res.ok && res.body === secret, "bob decrypts and verifies", JSON.stringify(res));
{
  const sentId = Number(((await alice.req(`/pm?folder=2&q=${encodeURIComponent(subj2)}`)).text.match(/\/pm\/read\/(\d+)/) || [])[1]);
  const sent = await readData(alice, sentId);
  const mine = await checkMessage(alice, sent.data);
  expect(mine.ok && mine.body === secret, "alice can read her sent copy");
  let mallorySaw = "decrypted";
  try { await core.decrypt(rd.data.armored, mallory.key, []); } catch (e) { mallorySaw = "refused"; }
  expect(mallorySaw === "refused", "mallory's key can't decrypt it");
}
armored = await core.encryptTo([await core.readPublic(ak.armoredPublic)], alice.key, payload);
r = await sendPm(alice, { to: "bob_pgp", subject: subj2, message: armored, pgp_mode: "encrypt" });
expect(/key changed/.test(r.error), "a message not encrypted to every recipient is refused", r.error);
armored = await core.encryptTo([await core.readPublic(bk.armoredPublic)], alice.key, payload);
r = await sendPm(alice, { to: "bob_pgp", subject: subj2, message: armored, pgp_mode: "encrypt" });
expect(/your own key/.test(r.error), "a message the sender couldn't read back is refused", r.error);
r = await sendPm(alice, { to: "bob_pgp", subject: subj2, message: "plain text pretending", pgp_mode: "encrypt" });
expect(/not an armored OpenPGP message/.test(r.error), "plain text can't pass as encrypted", r.error);
r = await alice.req("/pm/send?uid=" + bob.uid);
expect(!!(await fetch(BASE + "/pm/send", { headers: { Cookie: alice.cookieHeader() } })).headers.get("content-security-policy")?.includes("wasm-unsafe-eval"), "compose page allows WebAssembly for Argon2");
// Reply to an encrypted message: the server hands the ciphertext to the browser to quote.
r = await bob.req(`/pm/send?pmid=${pmid}`);
expect(/id="pgp-source"/.test(r.text) && !r.text.includes("launch code") && /data-pgp-mode="encrypt"/.test(r.text), "replying to an encrypted message quotes client-side");
// Encrypted draft (to self only).
bob.as();
const dp = core.messagePayload({ from: bob.uid, fpr: bk.fpr, to: [alice.uid], subject: "draft", body: "secret draft" });
r = await sendPm(bob, { to: "alice_pgp", subject: "draft", message: await core.encryptTo([await core.readPublic(bk.armoredPublic)], bob.key, dp), pgp_mode: "encrypt", savedraft: "1" });
expect(r.status === 303 && /folder=3/.test(r.location || ""), "encrypted drafts are saved", r.error);

console.log("== verification");
bob.as();
{
  const ts = Math.floor(Date.now() / 1000);
  const st = core.statements.verification(board, bob.uid, bk.fpr, alice.uid, ak.fpr, ts);
  r = await bob.api("/pgp/verify", { subject: alice.uid, statement: st, signature: await core.sign(bob.key, st) });
  expect(r.status === 200 && r.json().verification.subject_fpr === ak.fpr, "bob verifies alice", r.text.slice(0, 200));
  const v = r.json().verification;
  expect(await core.verifySig(await core.readPublic(bk.armoredPublic), v.statement, v.signature), "the stored verification is signed by bob's key");
  const st2 = core.statements.verification(board, bob.uid, bk.fpr, mallory.uid, ak.fpr, ts);
  r = await bob.api("/pgp/verify", { subject: mallory.uid, statement: st2, signature: await core.sign(bob.key, st2) });
  expect(r.status >= 400, "verifying the wrong fingerprint is refused");
  const st3 = core.statements.verification(board, bob.uid, bk.fpr, mallory.uid, mk.fpr, ts);
  r = await bob.api("/pgp/verify", { subject: mallory.uid, statement: st3, signature: await core.sign(mk.key, st3) });
  expect(r.status >= 400, "verification signed by someone else's key is refused");
  const a = await core.safetyNumber({ uid: alice.uid, fpr: ak.fpr }, { uid: bob.uid, fpr: bk.fpr });
  const b = await core.safetyNumber({ uid: bob.uid, fpr: bk.fpr }, { uid: alice.uid, fpr: ak.fpr });
  const m = await core.safetyNumber({ uid: bob.uid, fpr: bk.fpr }, { uid: alice.uid, fpr: mk.fpr });
  expect(a.groups.join("") === b.groups.join("") && a.code === b.code && a.groups.length === 12, "both sides compute the same safety number");
  expect(m.groups.join("") !== a.groups.join(""), "a swapped key gives a different safety number");
}

console.log("== key rotation and revocation");
alice.as();
const ak2 = await newKey(alice, "a whole new passphrase here");
r = await publish(alice, ak2, { previousKey: ak.key });
expect(r.status === 200 && r.json().key.fingerprint === ak2.fpr, "alice rotates her key with a transition signature", r.text.slice(0, 200));
r = await bob.api(`/pgp/user/${alice.uid}`);
const hist = r.json().history;
expect(hist.length === 2 && hist.find((k) => k.fingerprint === ak.fpr).status === "replaced" && hist[0].transition_from === ak.fpr, "history shows the old key as replaced and the vouch");
expect(r.json().verification.subject_fpr === ak.fpr, "bob's verification now points at the old key (so the UI shows 'key changed')");
r = await bob.req("/usercp/alerts");
expect(/new encryption key/.test(r.text), "bob is alerted that alice's key changed");
res = await checkMessage(bob, (await readData(bob, await latestPm(bob, subj1))).data);
expect(res.ok, "old messages still verify after rotation");
r = await publish(alice, ak);
expect(r.status === 200, "a replaced key can be restored");
r = await publish(alice, ak2, { previousKey: ak.key });
r = await alice.api("/pgp/revoke", { password: "wrong" });
expect(r.status >= 400 && /incorrect/.test(r.text), "revoking needs the account password");
r = await alice.api("/pgp/revoke", { password: PASSWORD });
expect(r.status === 200 && !r.json().key, "alice revokes her key");
r = await publish(alice, ak2);
expect(r.status >= 400 && /revoked/.test(r.text), "a revoked key can't be published again");
alice.as();
payload = core.messagePayload({ from: alice.uid, fpr: ak2.fpr, to: [bob.uid], subject: "after", body: "after" });
r = await sendPm(alice, { to: "bob_pgp", subject: "after", message: "after", pgp_mode: "sign", pgp_payload: payload, pgp_sig: await core.sign(ak2.key, payload) });
expect(/need an encryption key/.test(r.error), "no signing once the key is revoked", r.error);

console.log("== pages");
for (const [s, p] of [[bob, "/usercp/pgp"], [bob, `/pm/verify/${alice.uid}`], [bob, `/user/${alice.uid}`]]) {
  r = await s.req(p);
  expect(r.status === 200, `GET ${p}`, String(r.status));
}
r = await bob.req(`/user/${mallory.uid}`);
expect(/Identity key/.test(r.text), "profiles show the identity key");
r = await bob.req("/pm?folder=1");
expect(/pgp-flag is-encrypted/.test(r.text) && /pgp-flag is-signed/.test(r.text), "the inbox marks signed and encrypted messages");

{
  // Put PM flood control back to its default.
  const { execFileSync } = await import("node:child_process");
  try {
    execFileSync("psql", ["-h", "127.0.0.1", "-p", "5433", "-U", "rbb", "rbb", "-qc",
      "DELETE FROM settings WHERE name = 'pmfloodsecs'; SELECT pg_notify('rbb_cache', 'pgp-e2e:settings');"],
      { env: { ...process.env, PATH: `/opt/homebrew/opt/postgresql@17/bin:${process.env.PATH}` } });
  } catch (e) { /* ignore */ }
}
console.log(failed ? `\n${failed} check(s) FAILED` : "\nall PGP checks passed");
process.exit(failed ? 1 : 0);
