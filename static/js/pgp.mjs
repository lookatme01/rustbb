// rbb end-to-end identity for private messages — page controllers.
// Loaded as a module only on pages that need it (User CP → Encryption & identity, compose,
// read, verify). See pgp/core.mjs for the cryptography and src/pgp.rs for the server's checks.

import * as core from "./pgp/core.mjs?v=1";
import { h, icon, modal, confirmDialog, toast, copyText, download, fprBlock, when, seal } from "./pgp/ui.mjs?v=1";
import qrcode from "./vendor/qrcode.mjs?v=1";

const { api, page } = core;
const $ = (s, r = document) => r.querySelector(s);

const errText = (e) => (e && e.message) || String(e);
const nameOf = (u) => (u && u.username) || "this member";

// ------------------------------------------------------------------ trust

/** Parse "ts: N" out of a verification statement. */
const statementTs = (st) => Number((st.match(/^ts: (\d+)$/m) || [])[1] || 0);

/**
 * Check one of my verification records really was signed by one of my keys, over exactly the
 * statement it claims. `myKeys` is my key history (from /pgp/me) and my key on this device.
 */
async function verificationHolds(v, subjectUid, myKeys) {
  if (!v) return false;
  const expected = core.statements.verification(page.board, page.uid, v.verifier_fpr, subjectUid, v.subject_fpr, statementTs(v.statement));
  if (expected !== v.statement) return false;
  const armored = myKeys.local && myKeys.local.fpr === v.verifier_fpr ? myKeys.local.armoredPublic
    : (myKeys.history.find((k) => k.fingerprint === v.verifier_fpr) || {}).armored;
  if (!armored) return false;
  return core.verifySig(await core.readPublic(armored), v.statement, v.signature);
}

let myKeysP;
function myKeys() {
  if (!myKeysP) myKeysP = Promise.all([api.me(), core.localKey()]).then(([me, local]) => ({ me, local, history: me.history || [] }));
  return myKeysP;
}

/**
 * How far to trust `fpr` as `user`'s key:
 *   verified  — I verified exactly this key (and my signed record of that checks out)
 *   changed   — I verified, or previously saw, a different key for them
 *   signed    — a valid key, but I haven't verified it
 */
async function trust(user, fpr) {
  const mine = await myKeys();
  const v = user.verification;
  if (v && await verificationHolds(v, user.uid, mine)) {
    return v.subject_fpr === fpr ? { state: "verified", since: v.created } : { state: "changed", why: "verified" };
  }
  const pin = await core.store.pin(user.uid).catch(() => null);
  if (pin && pin.fpr !== fpr) return { state: "changed", why: "seen" };
  return { state: "signed" };
}

const trustCopy = {
  verified: (u) => ["Verified", `You verified ${nameOf(u)}'s identity`],
  signed: (u) => ["Not verified yet", `Compare safety numbers with ${nameOf(u)} to be sure`],
  changed: (u, t) => ["Key changed", t && t.why === "verified" ? `${nameOf(u)} has a different key from the one you verified` : `${nameOf(u)}'s key is different from the last time you messaged`],
};

// ------------------------------------------------------------------ QR / scanning

function qrSvg(text) {
  const q = qrcode(0, "M");
  q.addData(text);
  q.make();
  // The SVG is generated from our own data by the library; no message content is involved.
  return h("div.pgp-qr", { html: q.createSvgTag({ cellSize: 4, margin: 2, scalable: true }), role: "img", "aria-label": "Verification QR code" });
}

async function scanCode() {
  if (!("BarcodeDetector" in window)) throw new Error("This browser can't scan QR codes. Compare the numbers instead, or paste their code.");
  const detector = new window.BarcodeDetector({ formats: ["qr_code"] });
  const stream = await navigator.mediaDevices.getUserMedia({ video: { facingMode: "environment" } });
  try {
    return await modal({
      title: "Scan their code",
      build: (close) => {
        const video = h("video.pgp-video", { playsinline: true, muted: true });
        video.srcObject = stream;
        video.play();
        let alive = true;
        const tick = async () => {
          if (!alive) return;
          try {
            const codes = await detector.detect(video);
            const hit = codes.find((c) => c.rawValue && c.rawValue.startsWith("rbb-sn:"));
            if (hit) { alive = false; close(hit.rawValue); return; }
          } catch (e) { /* frame not ready */ }
          requestAnimationFrame(tick);
        };
        requestAnimationFrame(tick);
        return h("div.pgp-stack", h("p.pgp-muted", "Point your camera at the QR code on their verification screen."), video,
          h("div.pgp-actions", h("button.btn.secondary", { type: "button", onclick: () => { alive = false; close(null); } }, "Cancel")));
      },
    });
  } finally {
    stream.getTracks().forEach((t) => t.stop());
  }
}

// ------------------------------------------------------------------ settings (User CP)

async function settingsPage(root) {
  const render = async () => {
    root.replaceChildren(h("div.pgp-loading", h("span.pgp-spinner"), "Loading your keys…"));
    myKeysP = null;
    let mine;
    try { mine = await myKeys(); } catch (e) { root.replaceChildren(h("div.notice.bad", errText(e))); return; }
    const { me, local } = mine;
    const server = me.key;
    if (!server && !local) return root.replaceChildren(onboarding(me));
    if (server && local && local.fpr === server.fingerprint) return root.replaceChildren(await dashboard(me, local));
    if (server && (!local || local.fpr !== server.fingerprint)) return root.replaceChildren(restore(me, local));
    return root.replaceChildren(orphan(me, local));
  };

  // No key anywhere: explain and offer to create or import one.
  function onboarding(me) {
    return h("section.pgp-hero",
      h("div.pgp-hero-art", icon("shieldCheck")),
      h("h2", "Know who you're talking to"),
      h("p.pgp-lede", "Give your account an identity key. Your messages get a signature only you can make, and you can encrypt conversations so that only you and the other person can read them. Not even the board's staff can."),
      h("ul.pgp-points",
        h("li", icon("pen"), h("div", h("strong", "Signed messages"), h("span", "Recipients can see that a message really came from you and wasn't changed."))),
        h("li", icon("lock"), h("div", h("strong", "End-to-end encryption"), h("span", "Encrypted messages are sealed in your browser and opened only in theirs."))),
        h("li", icon("shieldCheck"), h("div", h("strong", "Verified contacts"), h("span", "Compare a safety number once and you'll know if anyone ever steps in between you.")))),
      h("div.pgp-actions.start",
        h("button.btn", { type: "button", onclick: () => createFlow(me) }, icon("sparkle"), "Create my key"),
        h("button.btn.secondary", { type: "button", onclick: () => importFlow(me) }, "Import an existing key")),
      h("p.pgp-fineprint", "Uses OpenPGP (Curve25519), so your key also works with GnuPG and other OpenPGP apps."));
  }

  // Passphrase + options, then generate, publish, and hand over the recovery kit.
  async function createFlow(me, { replacing = null } = {}) {
    const opts = await modal({
      title: replacing ? "Create a new key" : "Create your key",
      build: (close) => passphraseForm({ submitLabel: "Create key", onSubmit: (v) => close(v), replacing }),
    });
    if (!opts) return;
    const progress = busy(replacing ? "Creating and publishing your new key…" : "Creating your key…");
    try {
      const previousKey = replacing ? await core.unlockedKey("Unlock your current key so it can vouch for the new one.").catch(() => null) : null;
      const k = await core.generate({ username: me.username, passphrase: opts.passphrase });
      await core.publish({ ...k, backup: opts.backup, source: "generated", previousKey });
      await core.saveLocal({ ...k, remember: opts.remember });
      progress.close();
      await recoveryKit(me, k);
      toast(replacing ? "Your new key is ready. Contacts who verified you will be asked to verify again." : "Your key is ready.");
    } catch (e) {
      progress.close();
      toast(errText(e), "bad");
    }
    render();
  }

  async function importFlow(me, { replacing = false } = {}) {
    const res = await modal({
      title: "Import a private key",
      wide: true,
      build: (close) => {
        const ta = h("textarea.pgp-armor", { rows: 8, placeholder: "-----BEGIN PGP PRIVATE KEY BLOCK-----", spellcheck: "false", required: true });
        const file = h("input", { type: "file", accept: ".asc,.gpg,.key,.txt,text/plain" });
        file.addEventListener("change", async () => { if (file.files[0]) ta.value = await file.files[0].text(); });
        const pass = h("input", { type: "password", autocomplete: "off" });
        const err = h("p.pgp-error", { role: "alert" });
        const inner = passphraseForm({
          submitLabel: "Import key", replacing,
          intro: "Choose the passphrase that will protect the key in this browser. It can be the same as the key's current passphrase.",
          onSubmit: async (v) => {
            err.textContent = "";
            try {
              const k = await core.importPrivate(ta.value, pass.value, v.passphrase);
              close({ k, ...v });
            } catch (e) { err.textContent = errText(e); throw e; }
          },
        });
        return h("div.pgp-stack",
          h("p.pgp-muted", "Paste your armored private key, or choose the file. It is unlocked here in your browser; only the public part (and, if you choose, a passphrase-locked backup) is sent to the board."),
          ta, h("label.pgp-file", icon("download"), h("span", "Or choose a key file"), file),
          h("label.pgp-field", h("span", "Current passphrase of this key (if it has one)"), pass),
          err, inner);
      },
    });
    if (!res) return;
    const progress = busy("Publishing your key…");
    try {
      const previousKey = replacing ? await core.unlockedKey("Unlock your current key so it can vouch for the new one.").catch(() => null) : null;
      await core.publish({ ...res.k, backup: res.backup, source: "imported", previousKey });
      await core.saveLocal({ ...res.k, remember: res.remember });
      progress.close();
      toast("Your key has been imported.");
    } catch (e) { progress.close(); toast(errText(e), "bad"); }
    render();
  }

  function passphraseForm({ submitLabel, onSubmit, intro, replacing }) {
    const p1 = h("input", { type: "password", autocomplete: "new-password", required: true, minlength: 10 });
    const p2 = h("input", { type: "password", autocomplete: "new-password", required: true });
    const meter = h("div.pgp-meter", h("span"));
    const hint = h("small.pgp-hint", core.strength("").hint);
    const backup = h("input", { type: "checkbox", checked: true });
    const remember = h("input", { type: "checkbox", checked: true });
    const err = h("p.pgp-error", { role: "alert" });
    const btn = h("button.btn", { type: "submit" }, submitLabel);
    const update = () => {
      const s = core.strength(p1.value);
      meter.dataset.score = s.score;
      meter.firstChild.style.width = `${s.score * 25}%`;
      hint.textContent = s.hint;
    };
    p1.addEventListener("input", update);
    const reveal = h("button.pgp-linkbtn", { type: "button", onclick: () => { const t = p1.type === "password" ? "text" : "password"; p1.type = t; p2.type = t; reveal.textContent = t === "password" ? "Show" : "Hide"; } }, "Show");
    const suggest = h("button.pgp-linkbtn", { type: "button", onclick: () => { const s = core.suggestPassphrase(); p1.value = s; p2.value = s; p1.type = "text"; p2.type = "text"; reveal.textContent = "Hide"; update(); } }, "Suggest one");
    return h("form.pgp-stack", {
      onsubmit: async (e) => {
        e.preventDefault();
        err.textContent = "";
        if (p1.value !== p2.value) { err.textContent = "The passphrases don't match."; return; }
        if (p1.value.length < 10 || core.strength(p1.value).score < 2) { err.textContent = "Please choose a stronger passphrase."; return; }
        btn.disabled = true;
        try { await onSubmit({ passphrase: p1.value, backup: backup.checked, remember: remember.checked }); } catch (x) { /* shown by caller */ } finally { btn.disabled = false; }
      },
    },
      intro ? h("p.pgp-muted", intro) : h("p.pgp-muted", "Your passphrase locks your private key. The board never sees it and can't reset it, so pick something you'll remember and write it down somewhere safe."),
      replacing ? h("div.notice.warn", "Everyone who verified you will be told your key changed and asked to verify you again. Messages sent to your old key can still be read on devices that have it.") : null,
      h("label.pgp-field", h("span", "Passphrase ", reveal, " · ", suggest), p1, meter, hint),
      h("label.pgp-field", h("span", "Repeat passphrase"), p2),
      h("label.pgp-check", backup, h("span", "Keep an encrypted backup on the board"), h("small", "Restore your key on another device with your passphrase. The backup is locked before it leaves this browser.")),
      h("label.pgp-check", remember, h("span", "Remember on this device"), h("small", "Stay unlocked in this browser. Turn this off on shared computers.")),
      err,
      h("div.pgp-actions", btn));
  }

  function busy(text) {
    let closeFn;
    modal({ title: "Just a moment", dismissable: false, build: (close) => { closeFn = close; return h("div.pgp-loading", h("span.pgp-spinner"), text); } });
    return { close: () => closeFn && closeFn() };
  }

  async function recoveryKit(me, k) {
    const kit = [
      `rbb recovery kit for ${me.username} on ${page.board}`,
      `Created: ${new Date().toISOString()}`,
      `Fingerprint: ${core.formatFpr(k.fpr)}`,
      "",
      "Keep this file somewhere safe and offline. Your private key below is locked with your",
      "passphrase; you need both to restore it. The revocation certificate lets you declare the",
      "key invalid in other OpenPGP apps if you ever lose it.",
      "",
      k.armoredPrivate.trim(),
      "",
      k.revocationCertificate ? k.revocationCertificate.trim() : "",
      "",
    ].join("\n");
    await modal({
      title: "Save your recovery kit",
      dismissable: false,
      build: (close) => {
        const done = h("button.btn", { type: "button", disabled: true, onclick: () => close(true) }, "Done");
        return h("div.pgp-stack",
          h("div.pgp-success", icon("shieldCheck"), h("div", h("strong", "Your identity key is live"), fprBlock(k.fpr))),
          h("p", "Download your recovery kit now. If you lose this device and don't keep a backup on the board, it's the only way to get your key back."),
          h("div.pgp-actions.start",
            h("button.btn.secondary", { type: "button", onclick: () => { download(`rbb-recovery-${me.username}.txt`, kit); done.disabled = false; } }, icon("download"), "Download recovery kit"),
            h("button.pgp-linkbtn", { type: "button", onclick: () => { done.disabled = false; } }, "Skip — I have a backup")),
          h("div.pgp-actions", done));
      },
    });
  }

  // Key on the board and on this device: the everyday screen.
  async function dashboard(me, local) {
    const key = me.key;
    const remembered = await core.isRemembered();
    const verifs = me.verifications || [];
    const card = h("section.pgp-card.pgp-identity",
      h("div.pgp-identity-head", seal("verified", "Your identity key", `${key.algorithm} · created ${when(key.created)}`),
        h("a.btn.secondary.small", { href: `/user/${page.uid}/pgp.asc`, download: true }, icon("download"), "Public key")),
      h("div.pgp-fpr-wrap", h("span.pgp-label", "Fingerprint"), fprBlock(key.fingerprint),
        h("button.pgp-linkbtn", { type: "button", onclick: () => copyText(key.fingerprint, "Fingerprint copied") }, icon("copy"), "Copy")),
      key.expires ? h("p.pgp-muted", `Expires ${when(key.expires)}.`) : null);

    const deviceRow = h("div.pgp-row",
      icon("device"),
      h("div", h("strong", remembered ? "Unlocked on this device" : "Locked on this device"),
        h("small", remembered ? "You can sign and read encrypted messages here without your passphrase." : "You'll be asked for your passphrase when you sign or open encrypted messages.")),
      remembered
        ? h("button.btn.secondary.small", { type: "button", onclick: async () => { await core.lock(); toast("Locked. Your passphrase will be needed next time."); render(); } }, icon("lock"), "Lock")
        : h("button.btn.secondary.small", { type: "button", onclick: async () => { try { const k = await core.unlockedKey("Unlock to remember your key on this device."); await core.rememberOnDevice(k, true); render(); } catch (e) { if (!(e instanceof core.Locked)) toast(errText(e), "bad"); } } }, icon("unlock"), "Remember here"));

    const backupRow = h("div.pgp-row",
      icon("cloud"),
      h("div", h("strong", me.backup ? "Encrypted backup on the board" : "No backup on the board"),
        h("small", me.backup ? "Restore your key on a new device with your passphrase." : "If you lose this browser's data, you'll need your recovery kit.")),
      me.backup
        ? h("button.btn.secondary.small", { type: "button", onclick: async () => { if (await confirmDialog({ title: "Delete the backup?", text: "You'll need your recovery kit to use your key on another device.", confirm: "Delete backup", danger: true })) { await api.backup(""); toast("Backup deleted."); render(); } } }, "Delete backup")
        : h("button.btn.secondary.small", { type: "button", onclick: async () => { try { await api.backup(local.armoredPrivate); toast("Backup saved."); render(); } catch (e) { toast(errText(e), "bad"); } } }, "Back up now"));

    const tools = h("section.pgp-card",
      h("h3", "This device"), deviceRow, backupRow,
      h("div.pgp-actions.start.pgp-tools",
        h("button.btn.secondary.small", { type: "button", onclick: () => changePass(me) }, icon("key"), "Change passphrase"),
        h("button.btn.secondary.small", { type: "button", onclick: () => exportFlow(me) }, icon("download"), "Export private key"),
        h("button.btn.secondary.small", { type: "button", onclick: () => recoveryKit(me, { fpr: key.fingerprint, armoredPrivate: local.armoredPrivate, revocationCertificate: "" }) }, "Recovery kit")));

    const contacts = h("section.pgp-card",
      h("h3", "Verified contacts"),
      verifs.length
        ? h("ul.pgp-contacts", verifs.map((v) => {
            const stale = v.current_fpr !== v.subject_fpr;
            return h("li",
              seal(stale ? "changed" : "verified", v.username || `#${v.subject}`, stale ? (v.current_fpr ? "Key changed since you verified" : "No longer has a key") : `Verified ${when(v.created)}`),
              h("a.btn.secondary.small", { href: `/pm/verify/${v.subject}` }, stale ? "Verify again" : "View"));
          }))
        : h("p.pgp-muted", "You haven't verified anyone yet. Open a conversation and choose “Verify identity”, or visit a member's profile."));

    const danger = h("section.pgp-card.pgp-danger",
      h("h3", "Replace or revoke"),
      h("p.pgp-muted", "Replace your key if you think it was exposed or you want a fresh one. Revoke it if you want to stop using encryption entirely."),
      h("div.pgp-actions.start",
        h("button.btn.secondary.small", { type: "button", onclick: () => createFlow(me, { replacing: key }) }, "Create a new key"),
        h("button.btn.secondary.small", { type: "button", onclick: () => importFlow(me, { replacing: true }) }, "Import a different key"),
        h("button.btn.danger.small", { type: "button", onclick: () => revokeFlow() }, "Revoke key")));
    return h("div.pgp-grid", card, tools, contacts, danger);
  }

  async function changePass(me) {
    let key;
    try { key = await core.unlockedKey("Unlock with your current passphrase."); } catch (e) { return; }
    const v = await modal({ title: "Change passphrase", build: (close) => passphraseForm({ submitLabel: "Change passphrase", onSubmit: (x) => close(x) }) });
    if (!v) return;
    try {
      const armored = await core.changePassphrase(key, v.passphrase);
      if (v.backup) await api.backup(armored);
      await core.rememberOnDevice(key, v.remember);
      toast("Passphrase changed.");
    } catch (e) { toast(errText(e), "bad"); }
    render();
  }

  async function exportFlow(me) {
    let key;
    try { key = await core.unlockedKey("Unlock your key to export it."); } catch (e) { return; }
    const v = await modal({
      title: "Export for other apps",
      build: (close) => passphraseForm({
        submitLabel: "Export",
        intro: "Choose a passphrase for the exported file. The file uses standard protection that GnuPG, Thunderbird and other OpenPGP apps can import.",
        onSubmit: (x) => close(x),
      }),
    });
    if (!v) return;
    download(`${me.username}-private-key.asc`, await core.exportForOtherApps(key, v.passphrase));
  }

  async function revokeFlow() {
    const password = await modal({
      title: "Revoke your key",
      build: (close) => {
        const pw = h("input", { type: "password", autocomplete: "current-password", required: true });
        return h("form.pgp-stack", { onsubmit: (e) => { e.preventDefault(); close(pw.value); } },
          h("div.notice.bad", "Your key will stop working for new messages everywhere. People who verified you will be alerted. Old messages stay readable on devices that still have the key."),
          h("label.pgp-field", h("span", "Your account password"), pw),
          h("div.pgp-actions", h("button.btn.secondary", { type: "button", onclick: () => close(null) }, "Cancel"), h("button.btn.danger", { type: "submit" }, "Revoke key")));
      },
    });
    if (!password) return;
    try { await api.revoke(password); await core.store.remove(); toast("Your key has been revoked."); } catch (e) { toast(errText(e), "bad"); }
    render();
  }

  // The board has a key for this account, but this device doesn't (or has a different one).
  function restore(me, local) {
    const key = me.key;
    const pass = h("input", { type: "password", autocomplete: "current-password" });
    const remember = h("input", { type: "checkbox", checked: true });
    const err = h("p.pgp-error", { role: "alert" });
    const btn = h("button.btn", { type: "submit" }, "Restore key");
    const form = me.backup ? h("form.pgp-stack", {
      onsubmit: async (e) => {
        e.preventDefault();
        err.textContent = ""; btn.disabled = true; btn.textContent = "Restoring…";
        try {
          const locked = await core.openpgp.readPrivateKey({ armoredKey: me.backup });
          let k;
          try { k = await core.openpgp.decryptKey({ privateKey: locked, passphrase: pass.value }); } catch (x) { throw new Error("That passphrase is incorrect."); }
          if (core.fprOf(k) !== key.fingerprint) throw new Error("The backup doesn't match your current key.");
          await core.saveLocal({ key: k, armoredPrivate: me.backup, armoredPublic: key.armored, remember: remember.checked });
          toast("Your key is ready on this device.");
          render();
        } catch (x) { err.textContent = errText(x); form.classList.remove("shake"); void form.offsetWidth; form.classList.add("shake"); }
        finally { btn.disabled = false; btn.textContent = "Restore key"; }
      },
    },
      h("label.pgp-field", h("span", "Passphrase"), pass),
      h("label.pgp-check", remember, h("span", "Remember on this device")),
      err, h("div.pgp-actions.start", btn)) : null;
    return h("section.pgp-card.pgp-restore",
      seal("locked", local ? "This device has an old key" : "Your key isn't on this device", `Current key ${core.formatFpr(key.fingerprint).slice(-14)}`),
      me.backup
        ? h("p", "Enter your passphrase to restore your key from the encrypted backup.")
        : h("p", "There's no backup on the board. Import your key from your recovery kit or another device, or create a new key."),
      form,
      h("div.pgp-actions.start",
        h("button.btn.secondary.small", { type: "button", onclick: () => importFlow(me) }, "Import from a file"),
        h("button.btn.secondary.small", { type: "button", onclick: () => createFlow(me, { replacing: key }) }, "Start over with a new key")));
  }

  // A key on this device that the board doesn't know (revoked, or published from here and lost).
  function orphan(me, local) {
    return h("section.pgp-card",
      seal("changed", "Unpublished key on this device", core.formatFpr(local.fpr).slice(-14)),
      h("p", "This browser has a key that isn't your current identity key on the board (it may have been revoked or replaced from another device)."),
      h("div.pgp-actions.start",
        h("button.btn", { type: "button", onclick: async () => {
          try {
            const k = await core.unlockedKey("Unlock the key to publish it.");
            await core.publish({ key: k, armoredPublic: local.armoredPublic, armoredPrivate: local.armoredPrivate, backup: false, source: "imported" });
            toast("Key published."); render();
          } catch (e) { if (!(e instanceof core.Locked)) toast(errText(e), "bad"); }
        } }, "Publish this key"),
        h("button.btn.secondary", { type: "button", onclick: async () => { if (await confirmDialog({ title: "Remove key from this device?", text: "You won't be able to read messages encrypted to it on this device.", confirm: "Remove", danger: true })) { await core.store.remove(); render(); } } }, "Remove from this device"),
        h("button.btn.secondary", { type: "button", onclick: () => createFlow(me) }, "Create a new key")));
  }

  render();
}

// ------------------------------------------------------------------ verify a contact

async function verifyPage(root) {
  const them = Number(root.dataset.uid);
  const render = async () => {
    myKeysP = null;
    let mine, user;
    try { [mine, user] = await Promise.all([myKeys(), api.user(them)]); } catch (e) { root.replaceChildren(h("div.notice.bad", errText(e))); return; }
    const { me, local } = mine;
    const out = [];
    if (them === page.uid) {
      root.replaceChildren(me.key
        ? h("section.pgp-card", h("h3", "Your fingerprint"), fprBlock(me.key.fingerprint), h("p.pgp-muted", "Others see this when they verify you. Open their profile or a conversation with them to compare safety numbers."))
        : noKeyYet());
      return;
    }
    if (!me.key || !local || local.fpr !== me.key.fingerprint) { root.replaceChildren(noKeyYet(me.key)); return; }
    if (!user.key) {
      root.replaceChildren(h("section.pgp-card.pgp-empty", icon("shield"), h("h3", `${nameOf(user)} hasn't set up encryption yet`),
        h("p.pgp-muted", "Once they create an identity key you'll be able to verify them here and send them encrypted messages.")), history(user));
      return;
    }
    const t = await trust(user, user.key.fingerprint);
    const sn = await core.safetyNumber({ uid: page.uid, fpr: me.key.fingerprint }, { uid: them, fpr: user.key.fingerprint });
    const status = h("div.pgp-verify-status",
      t.state === "verified" ? seal("verified", `${nameOf(user)} is verified`, `Since ${when(t.since)}`)
        : t.state === "changed" ? seal("changed", ...trustCopy.changed(user, t))
        : seal("signed", `${nameOf(user)} isn't verified yet`, "Compare the safety number below"));
    out.push(status);

    const digits = h("div.pgp-sn", { "aria-label": "Safety number" }, sn.groups.map((g) => h("span", g)));
    const result = h("div.pgp-compare-result", { role: "status" });
    const compare = (code) => {
      result.replaceChildren(code.trim() === sn.code
        ? seal("verified", "It's a match", "You're both seeing the same keys")
        : seal("invalid", "Codes don't match", "Someone may be intercepting your conversation, or one of you changed keys. Don't mark as verified."));
      if (code.trim() === sn.code) primary.classList.add("pulse");
    };
    const pasteBox = h("form.pgp-paste", { hidden: true, onsubmit: (e) => { e.preventDefault(); compare(pasteInput.value); } });
    const pasteInput = h("input", { type: "text", placeholder: "rbb-sn:1:…", spellcheck: "false", "aria-label": "Their verification code" });
    pasteBox.append(pasteInput, h("button.btn.secondary.small", { type: "submit" }, "Compare"));

    const primary = t.state === "verified"
      ? h("button.btn.secondary", { type: "button", onclick: unverify }, "Remove verification")
      : h("button.btn", { type: "button", onclick: markVerified }, icon("shieldCheck"), "Mark as verified");

    out.push(h("section.pgp-card.pgp-verify",
      h("div.pgp-verify-people", person(mine.me.username, "You", me.key), h("div.pgp-link-line", icon("lock")), person(user.username, "Them", user.key)),
      h("div.pgp-sn-wrap",
        h("div", h("h3", "Safety number"),
          h("p.pgp-muted", `Compare these numbers with ${nameOf(user)} in person, on a video call, or through another app you trust. If they match on both screens, nobody is in the middle.`),
          digits,
          h("div.pgp-actions.start",
            h("button.btn.secondary.small", { type: "button", onclick: () => copyText(sn.code, "Verification code copied") }, icon("copy"), "Copy my code"),
            h("button.btn.secondary.small", { type: "button", onclick: () => { pasteBox.hidden = !pasteBox.hidden; if (!pasteBox.hidden) pasteInput.focus(); } }, "Paste their code"),
            "BarcodeDetector" in window ? h("button.btn.secondary.small", { type: "button", onclick: async () => { try { const c = await scanCode(); if (c) compare(c); } catch (e) { toast(errText(e), "warn"); } } }, icon("scan"), "Scan their code") : null),
          pasteBox, result),
        qrSvg(sn.code)),
      h("div.pgp-actions", primary)));
    out.push(history(user));
    root.replaceChildren(...out);

    async function markVerified() {
      try {
        const key = await core.unlockedKey(`Unlock your key to sign your verification of ${nameOf(user)}.`);
        const ts = Math.floor(Date.now() / 1000);
        const st = core.statements.verification(page.board, page.uid, me.key.fingerprint, them, user.key.fingerprint, ts);
        await api.verify(them, st, await core.sign(key, st));
        await core.store.setPin(them, user.key.fingerprint);
        toast(`${nameOf(user)} is now verified.`);
        root.classList.add("pgp-celebrate");
        setTimeout(() => root.classList.remove("pgp-celebrate"), 1200);
        render();
      } catch (e) { if (!(e instanceof core.Locked)) toast(errText(e), "bad"); }
    }
    async function unverify() {
      if (!(await confirmDialog({ title: "Remove verification?", text: `${nameOf(user)} will show as not verified until you compare safety numbers again.`, confirm: "Remove" }))) return;
      try { await api.unverify(them); toast("Verification removed."); render(); } catch (e) { toast(errText(e), "bad"); }
    }
  };

  function person(name, who, key) {
    return h("div.pgp-person", h("span.pgp-label", who), h("strong", name), fprBlock(key.fingerprint), h("small.pgp-muted", `${key.algorithm} · since ${when(key.added)}`));
  }
  function noKeyYet(hasServerKey) {
    return h("section.pgp-card.pgp-empty", icon("key"),
      h("h3", hasServerKey ? "Your key isn't on this device" : "Set up your own key first"),
      h("p.pgp-muted", hasServerKey ? "Restore your key on this device to verify others." : "Verification works by comparing your key with theirs, so you need one too. It takes a few seconds."),
      h("a.btn", { href: "/usercp/pgp" }, hasServerKey ? "Restore my key" : "Create my key"));
  }
  function history(user) {
    const hist = user.history || [];
    if (!hist.length) return h("span");
    return h("section.pgp-card",
      h("h3", icon("history"), `${nameOf(user)}'s key history`),
      h("ol.pgp-timeline", hist.map((k) => h(`li.is-${k.status}`,
        h("div.pgp-tl-head", h("strong", { active: "Current key", replaced: "Replaced", revoked: "Revoked", expired: "Expired" }[k.status] || k.status),
          h("span.pgp-muted", k.status === "active" || k.status === "expired" ? `since ${when(k.added)}` : `${when(k.added)} – ${when(k.retired)}`)),
        h("code.pgp-fpr-mini", core.formatFpr(k.fingerprint)),
        k.transition_from ? h("small.pgp-vouch", icon("check"), "Vouched for by their previous key") : null,
        k.source === "imported" ? h("small.pgp-muted", "Imported key") : null))));
  }
  render();
}

// ------------------------------------------------------------------ compose

async function composePage(form) {
  const ta = $("#message", form);
  const toInput = $("#to", form);
  const bccInput = $("#bcc", form);
  const subject = $("#subject", form);
  const hidden = (name) => $(`input[name="${name}"]`, form) || form.appendChild(h("input", { type: "hidden", name }));
  const modeInput = hidden("pgp_mode");
  const payloadInput = hidden("pgp_payload");
  const sigInput = hidden("pgp_sig");
  const initialMode = form.dataset.pgpMode || "";
  const sourceEl = $("#pgp-source");
  const source = sourceEl ? JSON.parse(sourceEl.textContent) : null;

  let mine = null, local = null, recips = [], missing = [], mode = initialMode, userChose = !!initialMode, bypass = false;

  const panel = h("div.pgp-compose", { "aria-live": "polite" });
  const anchor = ta.closest(".form-row");
  anchor.parentNode.insertBefore(h("div.form-row.pgp-compose-row", h("span.label", h("strong", "Protection")), panel), anchor.nextSibling);

  const options = [
    ["", "Off", "shield"],
    ["sign", "Signed", "pen"],
    ["encrypt", "Encrypted", "lock"],
  ];
  const seg = h("div.pgp-seg", { role: "radiogroup", "aria-label": "Message protection" });
  const buttons = options.map(([val, label, ic]) => {
    const b = h("button", { type: "button", role: "radio", "aria-checked": "false", dataset: { mode: val }, onclick: () => { userChose = true; setMode(val); } }, icon(ic), label);
    seg.appendChild(b);
    return b;
  });
  const explain = h("p.pgp-explain");
  const chips = h("ul.pgp-chips");
  const warn = h("div.pgp-compose-warn");
  panel.append(seg, explain, chips, warn);

  function available() {
    const ready = !!(local && mine && mine.key && local.fpr === mine.key.fingerprint);
    const allKeys = recips.length > 0 && missing.length === 0 && recips.every((r) => r.user.key);
    return { sign: ready, encrypt: ready && allKeys };
  }

  function setMode(m) {
    const av = available();
    if (m === "encrypt" && !av.encrypt) m = av.sign ? "sign" : "";
    if (m === "sign" && !av.sign) m = "";
    mode = m;
    modeInput.value = m;
    buttons.forEach((b) => {
      const on = b.dataset.mode === m;
      b.setAttribute("aria-checked", String(on));
      b.classList.toggle("on", on);
      b.disabled = (b.dataset.mode === "sign" && !av.sign) || (b.dataset.mode === "encrypt" && !av.encrypt);
    });
    form.dataset.noAutosave = m === "encrypt" ? "1" : "";
    if (m === "encrypt") { try { localStorage.removeItem("rbb.draft." + location.pathname); } catch (e) { /* ignore */ } }
    if (bccInput) {
      bccInput.disabled = !!m;
      bccInput.closest(".form-row").classList.toggle("pgp-dim", !!m);
      bccInput.title = m ? "BCC isn't available for signed or encrypted messages." : "";
    }
    explain.replaceChildren(...explainFor(m, av));
  }

  function explainFor(m, av) {
    if (!mine) return [h("span.pgp-spinner.small"), " Checking keys…"];
    if (!mine.key) return ["Sign and encrypt your messages with an identity key. ", h("a", { href: "/usercp/pgp" }, "Set one up")];
    if (!av.sign) return ["Your key isn't on this device. ", h("a", { href: "/usercp/pgp" }, "Restore it"), " to sign or encrypt."];
    if (m === "encrypt") return [icon("lock"), " End-to-end encrypted and signed. Only you and your recipients can read the message. The subject is not encrypted."];
    if (m === "sign") return [icon("pen"), av.encrypt ? " Signed, so recipients know it's from you. It is not encrypted." : " Signed, so recipients know it's from you. Everyone needs a key before you can encrypt."];
    return ["This message will be sent without a signature or encryption."];
  }

  async function renderChips() {
    const items = [];
    for (const r of recips) {
      const u = r.user;
      if (u.system) { items.push(h("li", seal("off", u.username, "System account"))); continue; }
      if (!u.key) { items.push(h("li", seal("off", u.username, "No key"))); continue; }
      r.trust = await trust(u, u.key.fingerprint);
      const [label] = trustCopy[r.trust.state](u, r.trust);
      items.push(h("li", h("a.pgp-chip-link", { href: `/pm/verify/${u.uid}`, target: "_blank", title: "Verify identity" }, seal(r.trust.state, u.username, label))));
    }
    for (const n of missing) items.push(h("li", seal("invalid", n, "No such member")));
    chips.replaceChildren(...items);
    const changed = recips.filter((r) => r.trust && r.trust.state === "changed");
    warn.replaceChildren(...(changed.length ? [h("div.notice.warn", icon("shieldAlert"), ` ${changed.map((r) => r.user.username).join(", ")} ${changed.length > 1 ? "have" : "has"} a different key from before. `, h("a", { href: `/pm/verify/${changed[0].user.uid}`, target: "_blank" }, "Verify now"), " before sending anything sensitive.")] : []));
  }

  let lookupSeq = 0;
  async function refreshRecipients() {
    const names = toInput.value.split(/[,;]/).map((s) => s.trim()).filter(Boolean);
    const seq = ++lookupSeq;
    if (!names.length) { recips = []; missing = []; chips.replaceChildren(); setMode(mode); return; }
    try {
      const res = await api.lookup({ names });
      if (seq !== lookupSeq) return;
      recips = res.users.filter((u) => names.some((n) => n.toLowerCase() === u.username.toLowerCase())).map((u) => ({ user: u }));
      missing = res.missing || [];
    } catch (e) { return; }
    const av = available();
    if (!userChose) setMode(av.encrypt ? "encrypt" : av.sign ? "sign" : "");
    else setMode(mode);
    await renderChips();
  }

  let t;
  toInput.addEventListener("input", () => { clearTimeout(t); t = setTimeout(refreshRecipients, 350); });
  toInput.addEventListener("change", refreshRecipients);

  // Encrypted drafts never go to the server's preview.
  document.addEventListener("click", (e) => {
    const tab = e.target.closest(".js-tab-preview");
    if (!tab || mode !== "encrypt" || !form.contains(tab)) return;
    e.stopPropagation();
    const ed = tab.closest(".js-editor");
    ed.classList.add("previewing"); tab.classList.add("active"); $(".js-tab-write", ed).classList.remove("active");
    core.renderMyCode(ta.value, $(".live-preview", ed));
  }, true);

  form.addEventListener("submit", async (e) => {
    if (bypass || !mode) return;
    const btn = e.submitter;
    const action = btn && btn.name === "preview" ? "preview" : btn && btn.name === "savedraft" ? "draft" : "send";
    if (action === "preview" && mode === "sign") return; // server preview of plain text is fine
    e.preventDefault();
    if (action === "preview") {
      const ed = $(".js-editor", form);
      $(".js-tab-preview", ed).click();
      return;
    }
    if (action === "draft" && mode === "sign") { resubmit(btn); return; }
    try {
      const key = await core.unlockedKey(action === "draft" ? "Unlock your key to save an encrypted draft." : "Unlock your key to sign this message.");
      const toUids = recips.map((r) => r.user.uid);
      if (action === "send") {
        if (missing.length) throw new Error(`${missing.join(", ")} ${missing.length > 1 ? "aren't members" : "isn't a member"}.`);
        const changed = recips.filter((r) => r.trust && r.trust.state === "changed");
        if (changed.length && mode === "encrypt") {
          const ok = await confirmDialog({
            title: "Their key changed",
            text: `${changed.map((r) => r.user.username).join(", ")} now ${changed.length > 1 ? "use different keys" : "uses a different key"} from before. Anyone who swapped in a key could read this message. Send anyway?`,
            confirm: "Send anyway", danger: true,
          });
          if (!ok) return;
        }
      }
      const payload = core.messagePayload({ from: page.uid, fpr: mine.key.fingerprint, to: toUids, subject: subject.value, body: ta.value });
      if (mode === "sign") {
        payloadInput.value = payload;
        sigInput.value = await core.sign(key, payload);
      } else {
        const keys = [await core.readPublic(local.armoredPublic)];
        if (action === "send") {
          for (const r of recips) {
            const pk = await core.readPublic(r.user.key.armored);
            if (core.fprOf(pk) !== r.user.key.fingerprint) throw new Error(`${r.user.username}'s key didn't match its fingerprint. Not sending.`);
            keys.push(pk);
          }
        }
        const armored = await core.encryptTo(keys, key, payload);
        ta.dataset.name = ta.name; ta.removeAttribute("name");
        hidden("message").value = armored;
        payloadInput.value = ""; sigInput.value = "";
      }
      resubmit(btn);
    } catch (x) {
      if (!(x instanceof core.Locked)) toast(errText(x), "bad");
    }
  });

  function resubmit(btn) {
    bypass = true;
    if (form.requestSubmit && btn) form.requestSubmit(btn); else form.submit();
    bypass = false;
  }
  // Coming back with the Back button: put the plain text field back.
  window.addEventListener("pageshow", () => { if (ta.dataset.name) { ta.name = ta.dataset.name; const m = $('input[type="hidden"][name="message"]', form); if (m) m.remove(); } });

  // Load keys, then decrypt anything the server couldn't put in the form.
  try {
    [mine, local] = await Promise.all([api.me(), core.localKey()]);
  } catch (e) { mine = { key: null }; }
  setMode(mode);
  await refreshRecipients();
  if (source) await loadSource(source);

  async function loadSource(src) {
    const box = h("div.notice.pgp-source", icon("lock"), " This conversation is end-to-end encrypted. ");
    const btn = h("button.btn.small", { type: "button" }, "Unlock to continue");
    box.appendChild(btn);
    const go = async () => {
      try {
        const key = await core.unlockedKey("Unlock your key to load the encrypted message.");
        const { text } = await core.decrypt(src.armored, key, []);
        let body = text;
        try { body = JSON.parse(text).body; } catch (e) { /* not a payload */ }
        if (src.mode === "draft" || src.mode === "resume") ta.value = body;
        else if (src.mode === "forward") ta.value = `\n\n[quote='${src.from}']\n${body}\n[/quote]` + ta.value;
        else ta.value = `[quote='${src.from}']\n${body}\n[/quote]\n` + ta.value;
        ta.dispatchEvent(new Event("input"));
        box.remove();
        userChose = true; setMode("encrypt");
      } catch (e) {
        if (!(e instanceof core.Locked)) box.replaceChildren(icon("shieldAlert"), ` Couldn't decrypt the message: ${errText(e)}`);
      }
    };
    btn.addEventListener("click", go);
    panel.prepend(box);
    if (await core.isRemembered()) go();
  }
}

// ------------------------------------------------------------------ read

async function readPage(root) {
  const data = JSON.parse($("#pgp-data").textContent);
  const sealHost = $(".pgp-msg-seal", root);
  const body = $(".js-pm-body", root);
  const setSeal = (...args) => sealHost.replaceChildren(...args);
  setSeal(seal("pending", "Checking signature…"));

  let sender;
  try { sender = await api.user(data.from); } catch (e) { setSeal(seal("invalid", "Couldn't check the signature", errText(e))); return; }
  const who = data.from === page.uid ? "you" : nameOf(sender);
  const signerInfo = (sender.history || []).find((k) => k.fingerprint === data.fpr);
  if (!signerInfo) return fail(`Signed with a key that doesn't belong to ${who}.`);
  const signerKey = await core.readPublic(signerInfo.armored).catch(() => null);
  if (!signerKey || core.fprOf(signerKey) !== data.fpr) return fail("The signing key couldn't be read.");

  let payloadText, sigValid;
  if (data.level === 1) {
    payloadText = data.payload;
    sigValid = await core.verifySig(signerKey, payloadText, data.sig);
    if (!sigValid) return fail("The signature doesn't match the message. It may have been altered.");
    finish();
  } else {
    body.replaceChildren(h("div.pgp-locked-body", icon("lock", "big"), h("p", "This message is end-to-end encrypted."), h("button.btn", { type: "button", onclick: () => open(true) }, "Unlock to read")));
    setSeal(seal("encrypted", "Encrypted message", "Unlock your key to read and check it"));
    if (await core.isRemembered()) open(false);
  }

  async function open() {
    let key;
    try { key = await core.unlockedKey("Unlock your key to read this message."); }
    catch (e) {
      if (e instanceof core.NoLocalKey) body.replaceChildren(h("div.pgp-locked-body", icon("key", "big"), h("p", "Your key isn't on this device."), h("a.btn", { href: "/usercp/pgp" }, "Restore my key")));
      return;
    }
    body.replaceChildren(h("div.pgp-loading", h("span.pgp-spinner"), "Decrypting…"));
    try {
      const r = await core.decrypt(data.armored, key, [signerKey]);
      payloadText = r.text;
      sigValid = r.valid;
    } catch (e) {
      body.replaceChildren(h("div.pgp-locked-body", icon("shieldX", "big"), h("p", "This message couldn't be decrypted. It may have been encrypted to a key you no longer have.")));
      return fail("Couldn't decrypt the message.");
    }
    if (!sigValid) { renderBody(payloadText); return fail("The encrypted message isn't signed by the sender's key."); }
    finish();
  }

  function renderBody(text) {
    let src = text;
    try { src = JSON.parse(text).body; } catch (e) { /* show as-is */ }
    const target = h("div");
    core.renderMyCode(src, target, data.allow_mycode);
    body.replaceChildren(...target.childNodes);
  }

  async function finish() {
    let p;
    try { p = JSON.parse(payloadText); } catch (e) { return fail("The signed content is malformed."); }
    const problems = [];
    if (p.v !== 1 || p.t !== "rbb-pm") problems.push("unknown format");
    if (p.board !== page.board) problems.push("it was signed for a different board");
    if (p.from !== data.from || p.fpr !== data.fpr) problems.push("it was signed by someone else");
    if (core.normalizeSubject(data.subject) !== p.subject) problems.push("the subject was changed");
    if (data.level === 1 && core.normalizeBody(data.body) !== p.body) problems.push("the text was changed");
    if (!data.sent && data.from !== page.uid && !p.to.includes(page.uid)) problems.push("it was addressed to someone else");
    if (Math.abs(p.ts - data.dateline) > 900) problems.push("it was signed at a different time than it was sent");
    if (data.level === 2) renderBody(payloadText);
    if (problems.length) return fail(`This message doesn't match what was signed: ${problems.join(", ")}.`);

    const signedAt = new Date(p.ts * 1000).toLocaleString();
    if (data.from === page.uid) {
      return setSeal(detailsSeal(seal("verified", "Signed by you", data.level === 2 ? "End-to-end encrypted" : null), p, signedAt));
    }
    const t = await trust(sender, data.fpr);
    let extra = null;
    if (signerInfo.status === "revoked") extra = `${who} later revoked this key`;
    else if (signerInfo.status === "replaced") extra = "Signed with their previous key";
    const [label, sub] = trustCopy[t.state](sender, t);
    const main = seal(t.state, t.state === "verified" ? `From ${who} · Verified` : t.state === "changed" ? `From ${who} · Key changed` : `Signed by ${who}`,
      [data.level === 2 ? "End-to-end encrypted" : "Signed, not encrypted", t.state === "verified" ? `Identity verified ${when(t.since)}` : sub, extra].filter(Boolean).join(" · ") || label);
    const actions = [];
    if (t.state !== "verified") actions.push(h("a.btn.small", { href: `/pm/verify/${data.from}` }, icon("shieldCheck"), t.state === "changed" ? "Verify again" : "Verify identity"));
    if (t.state === "changed" && t.why === "seen") {
      actions.push(h("button.btn.secondary.small", { type: "button", onclick: async () => { await core.store.setPin(data.from, data.fpr); toast("Noted. You'll be warned if their key changes again."); finish(); } }, "I know they changed keys"));
    }
    setSeal(h("div.pgp-seal-row", detailsSeal(main, p, signedAt), ...actions));
    // Remember the first key seen for this member (trust on first use).
    if (signerInfo.status === "active") {
      const pin = await core.store.pin(data.from).catch(() => null);
      if (!pin) await core.store.setPin(data.from, data.fpr).catch(() => {});
    }
  }

  function detailsSeal(sealEl, p, signedAt) {
    const d = h("details.pgp-details", h("summary", sealEl),
      h("dl",
        h("dt", "Signed by"), h("dd", `${nameOf(sender)} (member #${data.from})`),
        h("dt", "Key"), h("dd", fprBlock(data.fpr)),
        h("dt", "Signed at"), h("dd", signedAt),
        h("dt", "Protection"), h("dd", data.level === 2 ? "Signed and end-to-end encrypted" : "Signed (not encrypted)")),
      data.level === 1 ? h("details.pgp-source-view", h("summary", "Show the signed text"), h("pre", p.body)) : null);
    return d;
  }

  function fail(reason) {
    setSeal(h("div.pgp-seal-row", seal("invalid", "Signature problem", reason)));
    root.classList.add("pgp-tampered");
  }
}

// ------------------------------------------------------------------ boot

function boot() {
  const s = $("[data-pgp-settings]"); if (s) settingsPage(s);
  const v = $("[data-pgp-verify]"); if (v) verifyPage(v);
  const c = $("form[data-pgp-compose]"); if (c) composePage(c);
  const r = $("[data-pgp-read]"); if (r && $("#pgp-data")) readPage(r);
}
if (!window.isSecureContext || !window.crypto || !crypto.subtle) {
  document.querySelectorAll("[data-pgp-settings], [data-pgp-verify]").forEach((el) => el.replaceChildren(h("div.notice.bad", "Encryption needs a secure connection (HTTPS). Ask the board's administrator to enable it.")));
} else {
  boot();
}
