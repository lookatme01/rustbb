// rbb end-to-end identity: key storage, OpenPGP operations, signed statements, safety numbers.
//
// Private keys never leave the browser unencrypted. They live in IndexedDB, locked with the
// member's passphrase (Argon2 + AEAD). "Remember on this device" additionally keeps an unlocked
// copy sealed with a non-extractable WebCrypto key, so it can be used without the passphrase
// but can't be copied off the device by reading storage.

import * as openpgp from "../vendor/openpgp.min.mjs?v=1";

export { openpgp };

// Strong protection for stored private keys (RFC 9580 Argon2 S2K with AEAD).
const LOCK_CONFIG = { s2kType: openpgp.enums.s2k.argon2, aeadProtect: true };
// Protection readable by GnuPG and older tools, used only for "export for other apps".
const COMPAT_CONFIG = { s2kType: openpgp.enums.s2k.iterated, aeadProtect: false };

const enc = new TextEncoder();
const dec = new TextDecoder();

// ------------------------------------------------------------------ page context

export const page = {
  get uid() { return Number(document.body.dataset.uid || 0); },
  get board() { return (document.querySelector("[data-pgp-board]") || {}).dataset?.pgpBoard || location.origin; },
  get csrf() { return (document.querySelector('meta[name="csrf-token"]') || {}).content || ""; },
};

// ------------------------------------------------------------------ server API

async function call(path, form) {
  const opts = { credentials: "same-origin", headers: { Accept: "application/json", "X-Requested-With": "fetch" } };
  if (form) {
    const body = new URLSearchParams();
    for (const [k, v] of Object.entries(form)) body.set(k, v == null ? "" : String(v));
    body.set("my_post_key", page.csrf);
    Object.assign(opts, { method: "POST", body });
    opts.headers["X-CSRF-Token"] = page.csrf;
  }
  let r;
  try { r = await fetch(path, opts); } catch (e) { throw new Error("Couldn't reach the board. Check your connection and try again."); }
  let j = null;
  try { j = await r.json(); } catch (e) { /* not JSON */ }
  if (!r.ok) throw new Error((j && j.error) || `The board returned an error (${r.status}).`);
  return j;
}

export const api = {
  me: () => call("/pgp/me"),
  user: (uid) => call(`/pgp/user/${uid}`),
  lookup: ({ names = [], uids = [] }) => call(`/pgp/lookup?names=${encodeURIComponent(names.join(","))}&uids=${uids.join(",")}`),
  challenge: () => call("/pgp/challenge"),
  publish: (f) => call("/pgp/key", f),
  backup: (backup) => call("/pgp/backup", { backup }),
  revoke: (password) => call("/pgp/revoke", { password }),
  verify: (subject, statement, signature) => call("/pgp/verify", { subject, statement, signature }),
  unverify: (subject) => call("/pgp/unverify", { subject }),
};

// ------------------------------------------------------------------ IndexedDB

let dbp;
function db() {
  if (!dbp) {
    dbp = new Promise((resolve, reject) => {
      const r = indexedDB.open("rbb-pgp", 1);
      r.onupgradeneeded = () => {
        r.result.createObjectStore("keys", { keyPath: "id" });
        r.result.createObjectStore("pins", { keyPath: "id" });
      };
      r.onsuccess = () => resolve(r.result);
      r.onerror = () => reject(r.error || new Error("This browser won't let the board store your key (IndexedDB is unavailable, perhaps in private browsing)."));
    });
  }
  return dbp;
}
async function idb(store, mode, fn) {
  const d = await db();
  return new Promise((resolve, reject) => {
    const tx = d.transaction(store, mode);
    const req = fn(tx.objectStore(store));
    tx.oncomplete = () => resolve(req && req.result);
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error);
  });
}
const keyId = () => `${page.board}|${page.uid}`;
export const store = {
  get: () => idb("keys", "readonly", (s) => s.get(keyId())),
  put: (rec) => idb("keys", "readwrite", (s) => s.put({ ...rec, id: keyId(), uid: page.uid, board: page.board })),
  remove: () => idb("keys", "readwrite", (s) => s.delete(keyId())),
  pin: (uid) => idb("pins", "readonly", (s) => s.get(`${keyId()}|${uid}`)),
  setPin: (uid, fpr) => idb("pins", "readwrite", (s) => s.put({ id: `${keyId()}|${uid}`, fpr, at: Date.now() })),
};

// ------------------------------------------------------------------ unlocking

let unlocked = null; // { fpr, key } — decrypted private key for this page view only
let prompt = null;
/** Register the UI used to ask for the passphrase: async ({ reason, fpr, attempt }) => ({ passphrase, remember }) | null */
export function setPassphrasePrompt(fn) { prompt = fn; }

export class Locked extends Error {}
export class NoLocalKey extends Error {}

/** The member's key record on this device, or null. */
export async function localKey() {
  try { return (await store.get()) || null; } catch (e) { return null; }
}

async function sealForDevice(key) {
  const deviceKey = await crypto.subtle.generateKey({ name: "AES-GCM", length: 256 }, false, ["encrypt", "decrypt"]);
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const sealed = await crypto.subtle.encrypt({ name: "AES-GCM", iv }, deviceKey, enc.encode(key.armor()));
  return { deviceKey, iv, sealed };
}
async function openSealed(rec) {
  const plain = await crypto.subtle.decrypt({ name: "AES-GCM", iv: rec.device.iv }, rec.device.deviceKey, rec.device.sealed);
  return openpgp.readPrivateKey({ armoredKey: dec.decode(plain) });
}

/** Keep (or stop keeping) an unlocked copy of the key on this device. */
export async function rememberOnDevice(key, on = true) {
  const rec = await localKey();
  if (!rec) return;
  rec.device = on ? await sealForDevice(key) : null;
  await store.put(rec);
}

/** Forget the unlocked key on this page and this device; the passphrase will be needed again. */
export async function lock() {
  unlocked = null;
  const rec = await localKey();
  if (rec && rec.device) { rec.device = null; await store.put(rec); }
}

export async function isRemembered() {
  const rec = await localKey();
  return !!(rec && rec.device);
}

/** Try a passphrase against the stored key. Returns the decrypted key or throws. */
export async function unlockWith(passphrase, remember) {
  const rec = await localKey();
  if (!rec) throw new NoLocalKey("Your key isn't on this device.");
  const locked = await openpgp.readPrivateKey({ armoredKey: rec.armoredPrivate });
  let key;
  try {
    key = await openpgp.decryptKey({ privateKey: locked, passphrase });
  } catch (e) {
    throw new Error("That passphrase is incorrect.");
  }
  unlocked = { fpr: rec.fpr, key };
  if (remember) await rememberOnDevice(key, true);
  return key;
}

/**
 * The member's decrypted private key: from this page's cache, the device seal, or by asking for
 * the passphrase. Throws Locked if the member cancels, NoLocalKey if there is no key here.
 */
export async function unlockedKey(reason = "Unlock your key to continue.") {
  const rec = await localKey();
  if (!rec) throw new NoLocalKey("Your key isn't on this device.");
  if (unlocked && unlocked.fpr === rec.fpr) return unlocked.key;
  if (rec.device) {
    try {
      const key = await openSealed(rec);
      unlocked = { fpr: rec.fpr, key };
      return key;
    } catch (e) { /* seal unusable (browser data partly cleared); fall back to passphrase */ }
  }
  if (!prompt) throw new Locked("Your key is locked.");
  const got = await prompt({ reason, fpr: rec.fpr });
  if (!got) throw new Locked("Your key is locked.");
  return got.key;
}

// ------------------------------------------------------------------ keys

export const fprOf = (key) => key.getFingerprint().toUpperCase();

export async function readPublic(armored) {
  return openpgp.readKey({ armoredKey: armored });
}

/** Create a new identity key. Returns the decrypted key plus what to store and publish. */
export async function generate({ username, passphrase }) {
  const host = new URL(page.board).host;
  const { privateKey, publicKey, revocationCertificate } = await openpgp.generateKey({
    type: "ecc", curve: "curve25519Legacy",
    userIDs: [{ name: username, comment: `${host} #${page.uid}` }],
    passphrase, format: "armored", config: LOCK_CONFIG,
  });
  const key = await openpgp.decryptKey({ privateKey: await openpgp.readPrivateKey({ armoredKey: privateKey }), passphrase });
  return { key, armoredPrivate: privateKey, armoredPublic: publicKey, revocationCertificate, fpr: fprOf(key) };
}

/**
 * Import an existing private key (armored). If it is protected, `passphrase` unlocks it; either
 * way it is re-locked with `newPassphrase` (or the same one) using strong protection.
 */
export async function importPrivate(armored, passphrase, newPassphrase) {
  let key;
  try {
    key = await openpgp.readPrivateKey({ armoredKey: armored.trim() });
  } catch (e) {
    throw new Error("That doesn't look like an OpenPGP private key. Paste the whole block, including the BEGIN and END lines.");
  }
  if (!key.isDecrypted()) {
    try { key = await openpgp.decryptKey({ privateKey: key, passphrase }); } catch (e) { throw new Error("That passphrase doesn't unlock this key."); }
  }
  const pass = newPassphrase || passphrase;
  if (!pass) throw new Error("Choose a passphrase to protect the key on this device.");
  const armoredPrivate = (await openpgp.encryptKey({ privateKey: key, passphrase: pass, config: LOCK_CONFIG })).armor();
  return { key, armoredPrivate, armoredPublic: key.toPublic().armor(), revocationCertificate: "", fpr: fprOf(key) };
}

/** Re-lock the stored key with a new passphrase. */
export async function changePassphrase(key, passphrase) {
  const armoredPrivate = (await openpgp.encryptKey({ privateKey: key, passphrase, config: LOCK_CONFIG })).armor();
  const rec = await localKey();
  await store.put({ ...rec, armoredPrivate });
  return armoredPrivate;
}

/** The private key locked in a format GnuPG and other OpenPGP apps can import. */
export async function exportForOtherApps(key, passphrase) {
  return (await openpgp.encryptKey({ privateKey: key, passphrase, config: COMPAT_CONFIG })).armor();
}

// ------------------------------------------------------------------ signatures and messages

export async function sign(key, text) {
  const message = await openpgp.createMessage({ binary: enc.encode(text) });
  return openpgp.sign({ message, signingKeys: key, detached: true, format: "armored" });
}

/** True if `armoredSig` is a valid signature over `text` by `publicKey` (a key object). */
export async function verifySig(publicKey, text, armoredSig) {
  try {
    const signature = await openpgp.readSignature({ armoredSignature: armoredSig });
    const message = await openpgp.createMessage({ binary: enc.encode(text) });
    const { signatures } = await openpgp.verify({ message, signature, verificationKeys: publicKey, format: "binary" });
    if (!signatures.length) return false;
    await signatures[0].verified;
    return true;
  } catch (e) {
    return false;
  }
}

/** Encrypt (and sign) a payload to the given public keys. */
export async function encryptTo(publicKeys, signingKey, text) {
  const message = await openpgp.createMessage({ binary: enc.encode(text) });
  return openpgp.encrypt({ message, encryptionKeys: publicKeys, signingKeys: signingKey, format: "armored" });
}

/**
 * Decrypt a message. Returns { text, signed, valid }: `signed` if it carries a signature by one of
 * `verificationKeys`, `valid` if that signature checks out.
 */
export async function decrypt(armored, key, verificationKeys) {
  const message = await openpgp.readMessage({ armoredMessage: armored });
  const { data, signatures } = await openpgp.decrypt({ message, decryptionKeys: key, verificationKeys, format: "binary" });
  let signed = false, valid = false;
  for (const s of signatures) {
    try { await s.verified; signed = true; valid = true; break; } catch (e) { if (!/Could not find signing key/i.test(e.message)) signed = true; }
  }
  return { text: dec.decode(data), signed, valid };
}

// ------------------------------------------------------------------ signed statements
// These must match `src/pgp.rs` byte for byte.

export const statements = {
  keyProof: (board, uid, fpr, challenge) => `rbb-key-proof/v1\nboard: ${board}\nuid: ${uid}\nfingerprint: ${fpr}\nchallenge: ${challenge}\n`,
  transition: (board, uid, from, to) => `rbb-key-transition/v1\nboard: ${board}\nuid: ${uid}\nfrom: ${from}\nto: ${to}\n`,
  verification: (board, verifier, verifierFpr, subject, subjectFpr, ts) =>
    `rbb-verify/v1\nboard: ${board}\nverifier: ${verifier} ${verifierFpr}\nsubject: ${subject} ${subjectFpr}\nts: ${ts}\n`,
};

export const normalizeBody = (s) => s.replace(/\r\n?/g, "\n");
export const normalizeSubject = (s) => Array.from(s.replace(/^\s+|\s+$/gu, "")).slice(0, 120).join("");

/** The canonical signed payload of a private message (field order matters). */
export function messagePayload({ from, fpr, to, subject, body, ts }) {
  return JSON.stringify({
    v: 1, t: "rbb-pm", board: page.board, from, fpr,
    to: [...to].sort((a, b) => a - b),
    subject: normalizeSubject(subject), body: normalizeBody(body),
    ts: ts ?? Math.floor(Date.now() / 1000),
  });
}

// ------------------------------------------------------------------ publishing

/**
 * Publish a key: prove possession by signing a fresh challenge, vouch for it with the previous
 * key when that is still unlocked, and optionally store the locked private key as a backup.
 */
export async function publish({ key, armoredPublic, armoredPrivate, backup, source, previousKey }) {
  const { challenge, board, uid } = await api.challenge();
  const fpr = fprOf(key);
  const proof = await sign(key, statements.keyProof(board, uid, fpr, challenge));
  let transition_sig = "";
  if (previousKey && fprOf(previousKey) !== fpr) {
    transition_sig = await sign(previousKey, statements.transition(board, uid, fprOf(previousKey), fpr));
  }
  return api.publish({ public_key: armoredPublic, challenge, proof, transition_sig, backup: backup ? armoredPrivate : "", source });
}

/** Save a key on this device (replacing any other key for this account on this board). */
export async function saveLocal({ key, armoredPrivate, armoredPublic, remember }) {
  await store.put({ fpr: fprOf(key), armoredPrivate, armoredPublic, created: Date.now(), device: null });
  unlocked = { fpr: fprOf(key), key };
  if (remember) await rememberOnDevice(key, true);
}

// ------------------------------------------------------------------ safety numbers

const hexBytes = (hex) => Uint8Array.from(hex.match(/../g).map((b) => parseInt(b, 16)));
const concat = (...parts) => { const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0)); let i = 0; for (const p of parts) { out.set(p, i); i += p.length; } return out; };

async function half(board, uid, fpr) {
  const f = hexBytes(fpr);
  let h = concat(new Uint8Array([0, 2]), f, enc.encode(`${board}#${uid}`));
  for (let i = 0; i < 1024; i++) h = new Uint8Array(await crypto.subtle.digest("SHA-512", concat(h, f)));
  const groups = [];
  for (let g = 0; g < 6; g++) {
    let n = 0;
    for (let j = 0; j < 5; j++) n = n * 256 + h[g * 5 + j];
    groups.push(String(n % 100000).padStart(5, "0"));
  }
  return groups;
}

/**
 * The safety number two members compare: 12 groups of 5 digits, the same on both sides only if
 * each sees the other's real key. `code` is the machine-readable form used by the QR code.
 */
export async function safetyNumber(a, b) {
  const [lo, hi] = a.uid < b.uid ? [a, b] : [b, a];
  const groups = [...(await half(page.board, lo.uid, lo.fpr)), ...(await half(page.board, hi.uid, hi.fpr))];
  const code = `rbb-sn:1:${page.board}:${lo.uid}:${lo.fpr}:${hi.uid}:${hi.fpr}`;
  return { groups, code };
}

/** Fingerprint in ten groups of four for reading aloud. */
export const formatFpr = (fpr) => (fpr || "").replace(/(.{4})/g, "$1 ").trim();

// ------------------------------------------------------------------ passphrases

/** Rough passphrase strength, 0–4, with a hint. Deliberately simple and conservative. */
export function strength(p) {
  if (!p) return { score: 0, hint: "Use at least 12 characters, or four or more random words." };
  let pool = 0;
  if (/[a-z]/.test(p)) pool += 26;
  if (/[A-Z]/.test(p)) pool += 26;
  if (/[0-9]/.test(p)) pool += 10;
  if (/[^a-zA-Z0-9]/.test(p)) pool += 20;
  let bits = p.length * Math.log2(Math.max(pool, 2));
  if (/(.)\1{2,}/.test(p)) bits -= 15;
  if (/^(?:password|qwerty|letmein|123456|abc123|iloveyou|admin|welcome)/i.test(p)) bits = Math.min(bits, 10);
  if (/^\d+$/.test(p)) bits = Math.min(bits, 30);
  const words = p.trim().split(/[\s\-_.]+/).filter((w) => w.length >= 3).length;
  if (words >= 4) bits = Math.max(bits, 60);
  const score = bits < 40 ? 1 : bits < 60 ? 2 : bits < 80 ? 3 : 4;
  const hint = p.length < 12 ? "Make it at least 12 characters." : score < 3 ? "Add more words or mix in numbers and symbols." : score < 4 ? "Good. A little longer would be even better." : "Strong passphrase.";
  return { score, hint };
}

/** A random, readable passphrase: six groups of four letters/digits (≈ 124 bits). */
export function suggestPassphrase() {
  const alphabet = "abcdefghjkmnpqrstuvwxyz23456789";
  const bytes = crypto.getRandomValues(new Uint8Array(24));
  const chars = Array.from(bytes, (b) => alphabet[b % alphabet.length]);
  return [0, 1, 2, 3, 4, 5].map((i) => chars.slice(i * 4, i * 4 + 4).join("")).join("-");
}

// ------------------------------------------------------------------ MyCode for decrypted messages
// Encrypted messages are rendered here, never by the server. Only a safe subset is supported and
// everything is built with DOM APIs, so message text can never become markup or script.

const SIMPLE = { b: "strong", i: "em", u: "u", s: "s", sub: "sub", sup: "sup" };

function safeUrl(u) {
  try {
    const url = new URL(u.trim(), location.href);
    return ["http:", "https:", "mailto:"].includes(url.protocol) ? url.href : null;
  } catch (e) { return null; }
}

function textWithBreaks(parent, text) {
  const urlRe = /\bhttps?:\/\/[^\s<>\[\]"']+/g;
  text.split("\n").forEach((line, i) => {
    if (i) parent.appendChild(document.createElement("br"));
    let last = 0, m;
    while ((m = urlRe.exec(line))) {
      if (m.index > last) parent.appendChild(document.createTextNode(line.slice(last, m.index)));
      const href = safeUrl(m[0]);
      if (href) { const a = document.createElement("a"); a.href = href; a.rel = "nofollow noopener"; a.target = "_blank"; a.textContent = m[0]; parent.appendChild(a); }
      else parent.appendChild(document.createTextNode(m[0]));
      last = m.index + m[0].length;
    }
    if (last < line.length) parent.appendChild(document.createTextNode(line.slice(last)));
  });
}

export function renderMyCode(src, target, allowMyCode = true) {
  target.textContent = "";
  if (!allowMyCode) { textWithBreaks(target, src); return; }
  const re = /\[(\/?)(b|i|u|s|sub|sup|quote|code|url|list|\*|hr|spoiler)(?:=("[^"\]]*"|'[^'\]]*'|[^\]]*))?\]/gi;
  const stack = [{ tag: null, el: target }];
  const top = () => stack[stack.length - 1];
  let last = 0, m;
  const text = (t) => t && textWithBreaks(top().el, t);
  while ((m = re.exec(src))) {
    const [raw, closing, name0, arg0] = m;
    const name = name0.toLowerCase();
    const arg = arg0 ? arg0.replace(/^["']|["']$/g, "") : "";
    // Inside [code], everything but its own closing tag is literal.
    if (top().tag === "code" && !(closing && name === "code")) continue;
    text(src.slice(last, m.index));
    last = m.index + raw.length;
    if (closing) {
      const idx = stack.map((s) => s.tag).lastIndexOf(name);
      if (idx > 0) { stack.length = idx; } else text(raw);
      continue;
    }
    let el = null, container = null;
    if (SIMPLE[name]) el = document.createElement(SIMPLE[name]);
    else if (name === "hr") { top().el.appendChild(document.createElement("hr")); continue; }
    else if (name === "quote") {
      el = document.createElement("blockquote"); el.className = "mycode_quote";
      if (arg) { const c = document.createElement("cite"); c.textContent = arg.split(/\s+pid=|\s+dateline=/)[0] + " wrote:"; el.appendChild(c); }
    } else if (name === "code") {
      el = document.createElement("pre"); el.className = "mycode_code"; container = document.createElement("code"); el.appendChild(container);
    } else if (name === "spoiler") {
      el = document.createElement("details"); el.className = "mycode_spoiler";
      const s = document.createElement("summary"); s.textContent = arg || "Spoiler"; el.appendChild(s);
    } else if (name === "list") {
      el = document.createElement(arg ? "ol" : "ul");
    } else if (name === "*") {
      if (top().tag === "*") stack.pop();
      if (top().tag !== "list") { text(raw); continue; }
      el = document.createElement("li");
    } else if (name === "url") {
      // [url=x]label[/url] or [url]x[/url]
      const close = src.toLowerCase().indexOf("[/url]", last);
      const inner = close >= 0 ? src.slice(last, close) : "";
      const href = safeUrl(arg || inner);
      if (!href || close < 0) { text(raw); continue; }
      const a = document.createElement("a"); a.href = href; a.rel = "nofollow noopener"; a.target = "_blank";
      a.textContent = inner || href;
      top().el.appendChild(a);
      last = close + 6; re.lastIndex = last;
      continue;
    }
    top().el.appendChild(el);
    stack.push({ tag: name, el: container || el });
  }
  if (top().tag === "code") { top().el.appendChild(document.createTextNode(src.slice(last))); return; }
  text(src.slice(last));
}
