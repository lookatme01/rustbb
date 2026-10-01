// Small UI toolkit for the end-to-end identity screens: element builder, dialogs, toasts, icons.

import { setPassphrasePrompt, unlockWith, formatFpr } from "./core.mjs?v=1";

/** h("div.cls#id", { attrs }, ...children). Strings become text nodes, never markup. */
export function h(sel, attrs, ...kids) {
  const [, tag = "div", rest = ""] = sel.match(/^([a-z0-9-]*)(.*)$/i);
  const el = document.createElement(tag || "div");
  for (const part of rest.match(/[.#][^.#]+/g) || []) {
    if (part[0] === ".") el.classList.add(part.slice(1)); else el.id = part.slice(1);
  }
  if (attrs && (typeof attrs !== "object" || attrs instanceof Node || Array.isArray(attrs))) { kids.unshift(attrs); attrs = null; }
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v == null || v === false) continue;
    if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "html") el.innerHTML = v; // only ever used with the static SVG icons below
    else if (k === "dataset") Object.assign(el.dataset, v);
    else el.setAttribute(k, v === true ? "" : v);
  }
  const add = (k) => {
    if (k == null || k === false) return;
    if (Array.isArray(k)) k.forEach(add);
    else el.appendChild(k instanceof Node ? k : document.createTextNode(String(k)));
  };
  kids.forEach(add);
  return el;
}

const svg = (d, extra = "") => `<svg viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"${extra}>${d}</svg>`;
export const icons = {
  shield: svg('<path d="M12 3l7 3v5c0 4.6-3 8.4-7 10-4-1.6-7-5.4-7-10V6z"/>'),
  shieldCheck: svg('<path d="M12 3l7 3v5c0 4.6-3 8.4-7 10-4-1.6-7-5.4-7-10V6z"/><path d="M8.5 12.2l2.4 2.4 4.6-4.9"/>'),
  shieldAlert: svg('<path d="M12 3l7 3v5c0 4.6-3 8.4-7 10-4-1.6-7-5.4-7-10V6z"/><path d="M12 8v4.5M12 16h.01"/>'),
  shieldX: svg('<path d="M12 3l7 3v5c0 4.6-3 8.4-7 10-4-1.6-7-5.4-7-10V6z"/><path d="M9.5 9.5l5 5M14.5 9.5l-5 5"/>'),
  lock: svg('<rect x="5" y="11" width="14" height="10" rx="2"/><path d="M8 11V8a4 4 0 118 0v3"/>'),
  unlock: svg('<rect x="5" y="11" width="14" height="10" rx="2"/><path d="M8 11V8a4 4 0 017.7-1.5"/>'),
  key: svg('<circle cx="8" cy="15" r="4"/><path d="M11 12l8-8M16 7l2 2M14 9l2 2"/>'),
  pen: svg('<path d="M4 20l4-1 11-11-3-3L5 16z"/><path d="M14 6l3 3"/>'),
  check: svg('<path d="M5 12.5l4.5 4.5L19 7.5"/>'),
  copy: svg('<rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V6a2 2 0 00-2-2H6a2 2 0 00-2 2v8a2 2 0 002 2h2"/>'),
  scan: svg('<path d="M4 8V6a2 2 0 012-2h2M16 4h2a2 2 0 012 2v2M20 16v2a2 2 0 01-2 2h-2M8 20H6a2 2 0 01-2-2v-2M4 12h16"/>'),
  download: svg('<path d="M12 4v11M7 10l5 5 5-5M5 20h14"/>'),
  device: svg('<rect x="3" y="5" width="18" height="12" rx="2"/><path d="M8 21h8M12 17v4"/>'),
  cloud: svg('<path d="M7 18a4 4 0 01-.6-8 6 6 0 0111.4 1.5A3.5 3.5 0 0117.5 18z"/>'),
  eye: svg('<path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12z"/><circle cx="12" cy="12" r="3"/>'),
  sparkle: svg('<path d="M12 3v4M12 17v4M3 12h4M17 12h4M6 6l2.5 2.5M15.5 15.5L18 18M6 18l2.5-2.5M15.5 8.5L18 6"/>'),
  history: svg('<path d="M3 12a9 9 0 103-6.7L3 8"/><path d="M3 3v5h5M12 7v5l3 2"/>'),
  x: svg('<path d="M6 6l12 12M18 6L6 18"/>'),
};
export const icon = (name, cls = "") => h(`span.pgp-icon${cls ? "." + cls : ""}`, { html: icons[name], "aria-hidden": "true" });

// ------------------------------------------------------------------ toasts

let toastHost;
export function toast(message, kind = "ok") {
  if (!toastHost) { toastHost = h("div.pgp-toasts", { role: "status", "aria-live": "polite" }); document.body.appendChild(toastHost); }
  const t = h(`div.pgp-toast.${kind}`, icon(kind === "ok" ? "check" : "shieldAlert"), h("span", message));
  toastHost.appendChild(t);
  setTimeout(() => { t.classList.add("leaving"); setTimeout(() => t.remove(), 300); }, 3800);
}

// ------------------------------------------------------------------ dialogs

/** Open a modal. `build(close)` returns the body; resolves with whatever `close(value)` gets. */
export function modal({ title, build, wide = false, dismissable = true }) {
  return new Promise((resolve) => {
    const d = h(`dialog.pgp-dialog${wide ? ".wide" : ""}`, { "aria-labelledby": "pgp-dialog-title" });
    let done = false;
    const close = (v) => { if (done) return; done = true; d.classList.add("closing"); setTimeout(() => { d.close(); d.remove(); }, 160); resolve(v); };
    d.appendChild(h("header.pgp-dialog-head",
      h("h2#pgp-dialog-title", title),
      dismissable ? h("button.pgp-x", { type: "button", "aria-label": "Close", onclick: () => close(null) }, icon("x")) : null));
    d.appendChild(h("div.pgp-dialog-body", build(close)));
    d.addEventListener("cancel", (e) => { e.preventDefault(); if (dismissable) close(null); });
    document.body.appendChild(d);
    d.showModal();
    const first = d.querySelector("input, textarea, button.btn:not(.secondary)");
    if (first) first.focus();
  });
}

export function confirmDialog({ title, text, confirm = "Continue", cancel = "Cancel", danger = false, detail }) {
  return modal({
    title,
    build: (close) => h("div",
      h("p", text),
      detail || null,
      h("div.pgp-actions",
        h("button.btn.secondary", { type: "button", onclick: () => close(false) }, cancel),
        h(`button.btn${danger ? ".danger" : ""}`, { type: "button", onclick: () => close(true) }, confirm))),
  }).then(Boolean);
}

/** Ask for the key passphrase, retrying on a wrong one. Resolves { key } or null. */
export function askPassphrase({ reason, fpr }) {
  return modal({
    title: "Unlock your key",
    build: (close) => {
      const input = h("input", { type: "password", autocomplete: "current-password", required: true, "aria-describedby": "pgp-unlock-err" });
      const remember = h("input", { type: "checkbox" });
      const err = h("p.pgp-error#pgp-unlock-err", { role: "alert" });
      const btn = h("button.btn", { type: "submit" }, "Unlock");
      const form = h("form.pgp-stack", {
        onsubmit: async (e) => {
          e.preventDefault();
          btn.disabled = true; btn.textContent = "Unlocking…"; err.textContent = "";
          try {
            const key = await unlockWith(input.value, remember.checked);
            close({ key });
          } catch (x) {
            err.textContent = x.message; form.classList.remove("shake"); void form.offsetWidth; form.classList.add("shake");
            input.select();
          } finally { btn.disabled = false; btn.textContent = "Unlock"; }
        },
      },
        h("p.pgp-muted", reason),
        fpr ? h("p.pgp-fpr-mini", icon("key"), formatFpr(fpr).slice(-19)) : null,
        h("label.pgp-field", h("span", "Passphrase"), input),
        h("label.pgp-check", remember, h("span", "Remember on this device"), h("small", "Stay unlocked in this browser until you lock it.")),
        err,
        h("div.pgp-actions", h("button.btn.secondary", { type: "button", onclick: () => close(null) }, "Cancel"), btn));
      return form;
    },
  });
}
setPassphrasePrompt(askPassphrase);

// ------------------------------------------------------------------ bits

export async function copyText(text, what = "Copied") {
  try { await navigator.clipboard.writeText(text); toast(`${what} to the clipboard.`); }
  catch (e) { toast("Couldn't copy. Select the text and copy it yourself.", "warn"); }
}

export function download(name, text, type = "text/plain") {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const a = h("a", { href: url, download: name });
  document.body.appendChild(a); a.click(); a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 2000);
}

/** Fingerprint laid out as ten groups of four, for reading aloud. */
export function fprBlock(fpr) {
  return h("code.pgp-fpr", { title: fpr }, (formatFpr(fpr).split(" ")).map((g) => h("span", g)));
}

export const when = (ts) => ts ? new Date(ts * 1000).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" }) : "";

/** Status pill used across the identity screens. kind: verified | signed | changed | invalid | pending | locked | off */
export function seal(kind, label, sub) {
  const ic = { verified: "shieldCheck", signed: "shield", changed: "shieldAlert", invalid: "shieldX", pending: "shield", locked: "lock", off: "shield", encrypted: "lock" }[kind] || "shield";
  return h(`span.pgp-seal.is-${kind}`, icon(ic), h("span.pgp-seal-text", h("strong", label), sub ? h("small", sub) : null));
}
