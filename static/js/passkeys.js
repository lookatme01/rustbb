/* Passkeys: add one in the User CP, sign in with one on the login page.
   The server sends WebAuthn options as JSON (base64url buffers) and expects
   PublicKeyCredential.toJSON() back. Without WebAuthn the forms stay hidden or explain why. */
(function () {
  "use strict";
  if (!window.PublicKeyCredential || !navigator.credentials) return;

  // base64url <-> ArrayBuffer, for browsers without the JSON helpers (before 2024–25).
  const fromB64 = (s) => {
    s = s.replace(/-/g, "+").replace(/_/g, "/");
    const bin = atob(s + "=".repeat((4 - (s.length % 4)) % 4));
    return Uint8Array.from(bin, (c) => c.charCodeAt(0)).buffer;
  };
  const toB64 = (buf) => {
    let s = "";
    new Uint8Array(buf).forEach((b) => { s += String.fromCharCode(b); });
    return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  };

  function creationOptions(json) {
    if (PublicKeyCredential.parseCreationOptionsFromJSON) return PublicKeyCredential.parseCreationOptionsFromJSON(json);
    const o = Object.assign({}, json);
    o.challenge = fromB64(json.challenge);
    o.user = Object.assign({}, json.user, { id: fromB64(json.user.id) });
    o.excludeCredentials = (json.excludeCredentials || []).map((c) => Object.assign({}, c, { id: fromB64(c.id) }));
    return o;
  }
  function requestOptions(json) {
    if (PublicKeyCredential.parseRequestOptionsFromJSON) return PublicKeyCredential.parseRequestOptionsFromJSON(json);
    const o = Object.assign({}, json);
    o.challenge = fromB64(json.challenge);
    o.allowCredentials = (json.allowCredentials || []).map((c) => Object.assign({}, c, { id: fromB64(c.id) }));
    return o;
  }
  function credentialJSON(cred) {
    if (typeof cred.toJSON === "function") return cred.toJSON();
    const r = cred.response;
    const response = { clientDataJSON: toB64(r.clientDataJSON) };
    if (r.attestationObject) {
      response.attestationObject = toB64(r.attestationObject);
      response.transports = r.getTransports ? r.getTransports() : [];
      if (r.getAuthenticatorData) response.authenticatorData = toB64(r.getAuthenticatorData());
      if (r.getPublicKey && r.getPublicKey()) response.publicKey = toB64(r.getPublicKey());
      if (r.getPublicKeyAlgorithm) response.publicKeyAlgorithm = r.getPublicKeyAlgorithm();
    } else {
      response.authenticatorData = toB64(r.authenticatorData);
      response.signature = toB64(r.signature);
      response.userHandle = r.userHandle ? toB64(r.userHandle) : null;
    }
    return {
      id: cred.id, rawId: toB64(cred.rawId), type: cred.type, response,
      authenticatorAttachment: cred.authenticatorAttachment || null,
      clientExtensionResults: cred.getClientExtensionResults ? cred.getClientExtensionResults() : {},
    };
  }

  async function post(url, form, extra) {
    const body = new URLSearchParams(new FormData(form));
    Object.entries(extra || {}).forEach(([k, v]) => body.set(k, v));
    const r = await fetch(url, { method: "POST", body, credentials: "same-origin", headers: { Accept: "application/json", "X-Requested-With": "fetch" } });
    let data = {};
    try { data = await r.json(); } catch (e) { data = { error: "Something went wrong (" + r.status + "). Please reload the page and try again." }; }
    if (!r.ok || data.error) throw new Error(data.error || "Something went wrong. Please try again.");
    return data;
  }

  // The browser's reason a ceremony stopped, in words: cancelling isn't an error worth shouting about.
  function why(e) {
    if (e && e.name === "NotAllowedError") return "The passkey request was cancelled or timed out.";
    if (e && e.name === "InvalidStateError") return "This device already has a passkey for your account.";
    if (e && e.name === "SecurityError") return "Your browser refused: passkeys need this site's secure address.";
    return (e && e.message) || "Something went wrong. Please try again.";
  }

  function wire(form, run) {
    const err = form.querySelector(".js-passkey-error");
    const btn = form.querySelector("button[type=submit]");
    form.hidden = false;
    form.addEventListener("submit", async (ev) => {
      ev.preventDefault();
      if (err) err.hidden = true;
      btn.disabled = true;
      try {
        const done = await run(form);
        if (done && done.redirect) window.location.assign(done.redirect);
      } catch (e) {
        if (err) { err.textContent = why(e); err.hidden = false; }
      } finally {
        btn.disabled = false;
      }
    });
  }

  document.addEventListener("DOMContentLoaded", () => {
    document.querySelectorAll(".js-passkey-add").forEach((form) => wire(form, async (f) => {
      const options = await post("/usercp/passkeys/begin", f);
      const cred = await navigator.credentials.create({ publicKey: creationOptions(options) });
      const pw = f.querySelector("input[name=password]");
      if (pw) pw.value = "";
      return post("/usercp/passkeys/finish", f, { credential: JSON.stringify(credentialJSON(cred)), password: "" });
    }));
    document.querySelectorAll(".js-passkey-login").forEach((form) => wire(form, async (f) => {
      const options = await post("/member/login/passkey/begin", f);
      const cred = await navigator.credentials.get({ publicKey: requestOptions(options) });
      const remember = document.querySelector("input[name=remember]");
      return post("/member/login/passkey/finish", f, {
        credential: JSON.stringify(credentialJSON(cred)),
        remember: remember && remember.checked ? "1" : "0",
      });
    }));
  });
})();
