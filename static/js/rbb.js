/* rbb front-end enhancements. Every feature degrades gracefully without JS. */
(function () {
  "use strict";
  const $ = (s, r = document) => r.querySelector(s);
  const $$ = (s, r = document) => Array.from(r.querySelectorAll(s));
  const csrf = () => ($('meta[name="csrf-token"]') || {}).content || "";
  const store = {
    get(k) { try { return localStorage.getItem(k); } catch (e) { return null; } },
    set(k, v) { try { localStorage.setItem(k, v); } catch (e) {} },
    del(k) { try { localStorage.removeItem(k); } catch (e) {} },
  };
  async function post(url, data) {
    const body = new URLSearchParams(data || {});
    body.set("my_post_key", csrf());
    const r = await fetch(url, { method: "POST", body, headers: { "X-Requested-With": "fetch", Accept: "application/json" }, credentials: "same-origin" });
    return r;
  }
  function cookie(name, value, maxAge) {
    document.cookie = name + "=" + encodeURIComponent(value) + "; path=/; SameSite=Lax" + (maxAge !== undefined ? "; max-age=" + maxAge : "");
  }
  function readCookie(name) {
    const m = document.cookie.match(new RegExp("(?:^|; )" + name + "=([^;]*)"));
    return m ? decodeURIComponent(m[1]) : "";
  }

  document.addEventListener("DOMContentLoaded", () => {
    // Auto-submitting selects (theme / language / color mode).
    $$(".js-autosubmit").forEach((sel) => sel.addEventListener("change", () => {
      const f = sel.form;
      if (f.classList.contains("js-theme-form")) f.action = "/theme/" + sel.value;
      if (f.classList.contains("js-lang-form")) f.action = "/lang/" + sel.value;
      f.submit();
    }));
    // Forum jump.
    $$(".js-forumjump").forEach((f) => f.addEventListener("submit", (e) => { e.preventDefault(); location.href = "/forum/" + f.fid.value; }));
    $$(".js-calselect").forEach((s) => s.addEventListener("change", () => { location.href = "/calendar/" + s.value; }));
    $$(".js-forumjump select").forEach((s) => s.addEventListener("change", () => { location.href = "/forum/" + s.value; }));
    $$(".js-jump-go").forEach((b) => { b.hidden = true; });

    // Keep the current tab in view when the main nav scrolls sideways (phones).
    const tabs = $(".tabs"), cur = $(".tabs a[aria-current]");
    if (tabs) {
      const fit = () => tabs.classList.toggle("overflowing", tabs.scrollWidth > tabs.clientWidth + 1);
      fit();
      if (window.ResizeObserver) new ResizeObserver(fit).observe(tabs);
    }
    if (tabs && cur && tabs.scrollWidth > tabs.clientWidth) {
      const right = cur.offsetLeft + cur.offsetWidth - tabs.offsetLeft + 40;
      if (right > tabs.clientWidth) tabs.scrollLeft = right - tabs.clientWidth;
    }

    // Dropdown menus built on <details> close on an outside click or Escape.
    const menus = () => $$("details.popmenu[open], details.usermenu[open]");
    document.addEventListener("click", (e) => menus().forEach((d) => { if (!d.contains(e.target)) d.open = false; }));
    document.addEventListener("keydown", (e) => {
      if (e.key !== "Escape") return;
      menus().forEach((d) => { d.open = false; if (d.contains(document.activeElement)) $("summary", d).focus(); });
    });

    // Side navigation: mark the link for the current page when the server didn't.
    $$(".sidenav").forEach((nav) => {
      if ($("a.active, a[aria-current]", nav)) return;
      const here = location.pathname;
      let best = null;
      $$("a[href^='/']", nav).forEach((a) => {
        const h = a.getAttribute("href").split("?")[0];
        if ((here === h || here.startsWith(h + "/")) && (!best || h.length > best.getAttribute("href").length)) best = a;
      });
      if (best) { best.classList.add("active"); best.setAttribute("aria-current", "page"); }
    });

    // Long side navigations (the ACP) fold into sections; the current one stays open.
    if (window.matchMedia("(min-width: 901px)").matches) $$(".js-nav-groups").forEach((nav) => {
      const kids = Array.from(nav.children);
      let group = null;
      kids.forEach((el, i) => {
        if (el.tagName === "H3" && i > 0) {
          group = document.createElement("details");
          group.className = "sidenav-group";
          const sum = document.createElement("summary");
          sum.textContent = el.textContent;
          group.appendChild(sum);
          el.replaceWith(group);
        } else if (group) {
          group.appendChild(el);
        }
      });
      $$("details.sidenav-group", nav).forEach((d) => {
        const key = "rbb.nav." + $("summary", d).textContent;
        d.open = !!$("a.active", d) || store.get(key) === "1";
        d.addEventListener("toggle", () => (d.open ? store.set(key, "1") : store.del(key)));
      });
    });

    // Tables that scroll sideways on phones must be reachable by keyboard.
    const focusScrollers = () => $$("table.grid").forEach((t) => {
      if (t.scrollWidth > t.clientWidth + 1) t.tabIndex = 0; else t.removeAttribute("tabindex");
    });
    focusScrollers();
    addEventListener("resize", focusScrollers);

    // Collapsible categories.
    $$("[data-collapse]").forEach((box) => {
      const key = "rbb.collapse." + box.dataset.collapse;
      const btn = $(".collapse-toggle", box);
      if (!btn) return;
      const apply = (c) => { box.classList.toggle("collapsed", c); btn.setAttribute("aria-expanded", String(!c)); };
      apply(store.get(key) === "1");
      btn.addEventListener("click", () => { const c = !box.classList.contains("collapsed"); apply(c); c ? store.set(key, "1") : store.del(key); });
    });

    // Confirm dialogs.
    $$(".js-confirm").forEach((f) => f.addEventListener("submit", (e) => { if (!confirm(f.dataset.confirm || "Are you sure?")) e.preventDefault(); }));
    // Show/hide toggles.
    $$(".js-toggle").forEach((c) => { const t = $(c.dataset.target); if (t) c.addEventListener("change", () => { t.hidden = !c.checked; }); });
    // Ignored posts.
    $$(".js-show-ignored").forEach((b) => b.addEventListener("click", () => { b.closest(".post").classList.remove("ignored-post"); b.parentElement.remove(); }));

    // Localize quote dates.
    $$("time.quote_date[data-ts]").forEach((t) => {
      const d = new Date(parseInt(t.dataset.ts, 10) * 1000);
      if (!isNaN(d)) { t.textContent = d.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" }); t.dateTime = d.toISOString(); }
    });
    // Copy code buttons.
    document.addEventListener("click", (e) => {
      const b = e.target.closest(".copy_code");
      if (!b) return;
      const code = b.closest(".codeblock").querySelector("code");
      navigator.clipboard && navigator.clipboard.writeText(code.textContent).then(() => { b.textContent = "Copied"; setTimeout(() => (b.textContent = "Copy"), 1500); });
    });

    // Inline moderation selection.
    const updateSel = () => $$(".js-selcount").forEach((c) => {
      const form = c.closest("form");
      c.textContent = $$('input.inline-check[form="' + form.id + '"]:checked').length;
    });
    $$("input.inline-check").forEach((c) => c.addEventListener("change", updateSel));
    $$(".js-check-all").forEach((b) => b.addEventListener("click", () => {
      const form = b.closest("form");
      const boxes = $$('input.inline-check[form="' + form.id + '"]');
      const all = boxes.every((x) => x.checked);
      boxes.forEach((x) => (x.checked = !all));
      updateSel();
    }));

    // Captcha refresh.
    $$(".js-captcha-refresh").forEach((b) => b.addEventListener("click", async () => {
      const r = await fetch("/captcha/refresh", { credentials: "same-origin" });
      const j = await r.json();
      const wrap = b.closest(".captcha");
      $(".js-captcha-img", wrap).src = "/captcha/" + j.hash;
      $(".js-captcha-hash", wrap).value = j.hash;
    }));

    // Registration helpers.
    const tz = $(".js-tz");
    if (tz && !tz.value) {
      try { const z = Intl.DateTimeFormat().resolvedOptions().timeZone; if ([...tz.options].some((o) => o.value === z)) tz.value = z; } catch (e) {}
    }
    const nameInput = $(".js-checkname");
    if (nameInput) {
      let timer;
      nameInput.addEventListener("input", () => {
        clearTimeout(timer);
        timer = setTimeout(async () => {
          const v = nameInput.value.trim();
          if (v.length < 2) return;
          const r = await fetch("/member/checkname?username=" + encodeURIComponent(v));
          if (!r.ok) return;
          const j = await r.json();
          const s = $(".js-name-status");
          if (nameInput.value.trim() !== v) return;
          s.textContent = j.available ? "That username is available." : (j.valid ? "That username is already taken." : "That username contains invalid characters.");
          s.classList.toggle("ok", !!j.available);
          s.classList.toggle("bad", !j.available);
        }, 400);
      });
    }
    // Show/hide password buttons.
    $$(".js-reveal").forEach((b) => {
      const input = document.getElementById(b.getAttribute("aria-controls"));
      if (!input) return;
      b.hidden = false;
      b.addEventListener("click", () => {
        const show = input.type === "password";
        input.type = show ? "text" : "password";
        b.textContent = show ? "Hide" : "Show";
        b.setAttribute("aria-pressed", String(show));
        input.focus();
      });
    });

    initEditors();
    initMultiquote();
    initReactions();
    initAttachments();
    initLive();
  });

  // ------------------------------------------------------------------ editor
  function wrap(ta, open, close) {
    const s = ta.selectionStart, e = ta.selectionEnd, v = ta.value;
    const sel = v.slice(s, e);
    ta.value = v.slice(0, s) + open + sel + close + v.slice(e);
    ta.focus();
    ta.selectionStart = s + open.length;
    ta.selectionEnd = s + open.length + sel.length;
    ta.dispatchEvent(new Event("input"));
  }
  function insertAtCursor(ta, text) {
    if (ta.rbbRich && ta.rbbRich.active) return ta.rbbRich.insertText(text);
    const s = ta.selectionStart, v = ta.value;
    ta.value = v.slice(0, s) + text + v.slice(ta.selectionEnd);
    ta.selectionStart = ta.selectionEnd = s + text.length;
    ta.focus();
    ta.dispatchEvent(new Event("input"));
  }
  window.rbbInsert = insertAtCursor;

  function initEditors() {
    $$(".js-editor").forEach((ed) => {
      const ta = $("textarea", ed);
      const form = ed.closest("form");
      // In rich-text mode (static/js/editor.mjs) the same buttons format the rich view.
      const rich = () => ed.rbbRich && ed.rbbRich.active ? ed.rbbRich : null;
      const ask = (b) => {
        if (b.dataset.prompt) return prompt(b.dataset.prompt, "https://");
        if (b.dataset.tag === "img") return prompt("Image URL", "https://");
        if (b.dataset.tag === "video") return prompt("Video URL", "https://");
        return null;
      };
      $$(".editor-toolbar button[data-tag]", ed).forEach((b) => b.addEventListener("click", () => {
        const tag = b.dataset.tag;
        const r = rich();
        if (r) return r.command(tag, { arg: b.dataset.arg, value: ask(b) });
        if (b.dataset.list) return wrap(ta, "[list]\n[*]", "\n[/list]");
        if (b.dataset.prompt) {
          const url = prompt(b.dataset.prompt, "https://");
          if (!url) return;
          const sel = ta.value.slice(ta.selectionStart, ta.selectionEnd);
          return sel ? wrap(ta, "[url=" + url + "]", "[/url]") : insertAtCursor(ta, "[url]" + url + "[/url]");
        }
        if (b.dataset.arg) return wrap(ta, "[" + tag + "=" + b.dataset.arg + "]", "[/" + tag + "]");
        wrap(ta, "[" + tag + "]", "[/" + tag + "]");
      }));
      $$(".editor-toolbar select[data-tag]", ed).forEach((s) => s.addEventListener("change", () => {
        if (!s.value) return;
        const r = rich();
        if (r) { r.command(s.dataset.tag, { arg: s.value }); s.value = ""; return; }
        wrap(ta, "[" + s.dataset.tag + "=" + s.value + "]", "[/" + s.dataset.tag + "]");
        s.value = "";
      }));
      $$(".js-smilie", ed).forEach((b) => b.addEventListener("click", () => insertAtCursor(ta, " " + b.dataset.code + " ")));
      // Keyboard shortcuts.
      ta.addEventListener("keydown", (e) => {
        if (!(e.ctrlKey || e.metaKey)) return;
        const k = e.key.toLowerCase();
        if (k === "b" || k === "i" || k === "u") { e.preventDefault(); wrap(ta, "[" + k + "]", "[/" + k + "]"); }
        if (k === "enter" && form) { e.preventDefault(); form.requestSubmit ? form.requestSubmit() : form.submit(); }
      });
      // Preview tab.
      const pv = $(".live-preview", ed);
      const tabW = $(".js-tab-write", ed), tabP = $(".js-tab-preview", ed);
      if (tabP) tabP.addEventListener("click", async () => {
        ed.classList.add("previewing"); tabP.classList.add("active"); tabW.classList.remove("active");
        pv.textContent = "Loading preview…";
        const r = await post("/preview", { message: ta.value, fid: (form && form.dataset.fid) || 0 });
        if (r.ok) { const j = await r.json(); pv.innerHTML = j.html; } else pv.textContent = "Preview failed.";
      });
      if (tabW) tabW.addEventListener("click", () => { ed.classList.remove("previewing"); tabW.classList.add("active"); tabP.classList.remove("active"); const r = rich(); r ? $(".wy-surface", ed).focus() : ta.focus(); });
      // Character count.
      const cc = $(".js-charcount", ed);
      const count = () => { if (cc) cc.textContent = ta.value.length + " characters"; };
      ta.addEventListener("input", count); count();
      // Local autosave (restored if the page is reloaded before posting).
      if (form) {
        const key = "rbb.draft." + location.pathname;
        const status = $(".js-draft-status", ed);
        const saved = store.get(key);
        if (saved && !ta.value.trim()) { ta.value = saved; count(); ta.dispatchEvent(new CustomEvent("input", { detail: { restored: true } })); if (status) status.textContent = "Restored unsent text."; }
        let t;
        ta.addEventListener("input", () => { clearTimeout(t); t = setTimeout(() => { if (form.dataset.noAutosave) { store.del(key); return; } ta.value.trim() ? store.set(key, ta.value) : store.del(key); if (status) status.textContent = "Saved locally"; }, 800); });
        form.addEventListener("submit", (e) => { if (!e.submitter || !e.submitter.name || e.submitter.name === "savedraft") store.del(key); });
      }
    });
  }

  // ------------------------------------------------------------------ quoting
  function initMultiquote() {
    const selected = () => readCookie("multiquote").split(",").filter(Boolean);
    const sync = () => { const s = selected(); $$(".js-multiquote").forEach((b) => { const on = s.includes(b.dataset.pid); b.setAttribute("aria-pressed", String(on)); b.textContent = on ? "✓ Multi-quote" : "Multi-quote"; }); };
    $$(".js-multiquote").forEach((b) => b.addEventListener("click", () => {
      let s = selected();
      s = s.includes(b.dataset.pid) ? s.filter((x) => x !== b.dataset.pid) : s.concat([b.dataset.pid]);
      cookie("multiquote", s.join(","), s.length ? 3600 : 0);
      sync();
    }));
    sync();
    // Quote into the quick reply box without leaving the page.
    document.addEventListener("click", async (e) => {
      const q = e.target.closest(".js-quote");
      if (!q) return;
      const ta = $("#quickreply textarea") || (q.closest("form") ? null : $("textarea[name=message]"));
      if (!ta) return;
      e.preventDefault();
      const r = await fetch("/post/" + q.dataset.pid + "/quote", { credentials: "same-origin" });
      if (!r.ok) return;
      const j = await r.json();
      ta.value += (ta.value && !ta.value.endsWith("\n") ? "\n" : "") + j.quote;
      ta.dispatchEvent(new Event("input"));
      ta.focus();
      ta.scrollIntoView({ block: "center" });
    });
  }

  // ------------------------------------------------------------------ reactions
  function initReactions() {
    document.addEventListener("click", async (e) => {
      const tog = e.target.closest(".js-react-toggle");
      if (tog) { tog.parentElement.classList.toggle("open"); return; }
      const b = e.target.closest(".js-react");
      if (!b) { $$(".reaction-picker.open").forEach((p) => p.classList.remove("open")); return; }
      const box = b.closest(".reactions");
      const r = await post("/post/" + box.dataset.pid + "/react", { kind: b.dataset.kind });
      if (!r.ok) return;
      const j = await r.json();
      $$(".reaction", box).forEach((x) => x.remove());
      const picker = $(".reaction-picker", box);
      j.reactions.forEach((x) => {
        const btn = document.createElement("button");
        btn.type = "button";
        btn.className = "reaction js-react" + (x.mine ? " mine" : "");
        btn.dataset.kind = x.kind;
        btn.title = x.kind;
        btn.textContent = x.emoji + " " + x.count;
        box.insertBefore(btn, picker);
      });
      if (picker) picker.classList.remove("open");
    });
  }

  // ------------------------------------------------------------------ attachments
  function initAttachments() {
    $$(".js-attachments").forEach((wrapEl) => {
      const form = wrapEl.closest("form");
      const list = $(".js-attach-list", wrapEl);
      const hash = $(".js-posthash", form);
      const ta = $("textarea[name=message]", form);
      const upload = async (files) => {
        for (const f of files) {
          const li = document.createElement("li");
          li.textContent = "Uploading " + f.name + "…";
          list.appendChild(li);
          const fd = new FormData();
          fd.append("file", f);
          fd.append("posthash", hash ? hash.value : "");
          fd.append("pid", wrapEl.dataset.pid || "0");
          fd.append("fid", form.dataset.fid || "0");
          fd.append("my_post_key", csrf());
          const r = await fetch("/attachment/upload", { method: "POST", body: fd, credentials: "same-origin", headers: { "X-CSRF-Token": csrf() } });
          const j = await r.json().catch(() => ({ error: "Upload failed" }));
          if (!r.ok || j.error) { li.textContent = f.name + ": " + (j.error || "upload failed"); li.style.color = "var(--bad)"; continue; }
          li.dataset.aid = j.aid;
          li.innerHTML = "";
          const name = document.createElement("span"); name.textContent = j.filename;
          const size = document.createElement("span"); size.className = "faint small"; size.textContent = j.size;
          const ins = document.createElement("button"); ins.type = "button"; ins.className = "btn secondary small js-attach-insert"; ins.textContent = "Insert";
          const rm = document.createElement("button"); rm.type = "button"; rm.className = "btn secondary small js-attach-remove"; rm.textContent = "Remove";
          li.append(name, size, ins, rm);
        }
      };
      if (ta) ta.rbbUpload = upload; // files pasted or dropped into the rich-text view
      const input = $(".js-file", wrapEl);
      input.addEventListener("change", () => { upload(input.files); input.value = ""; });
      const dz = $(".js-dropzone", wrapEl);
      ["dragenter", "dragover"].forEach((ev) => dz.addEventListener(ev, (e) => { e.preventDefault(); dz.classList.add("drag"); }));
      ["dragleave", "drop"].forEach((ev) => dz.addEventListener(ev, (e) => { e.preventDefault(); dz.classList.remove("drag"); }));
      dz.addEventListener("drop", (e) => upload(e.dataTransfer.files));
      if (ta) ta.addEventListener("paste", (e) => {
        const files = [...(e.clipboardData || {}).files || []];
        if (files.length) { e.preventDefault(); upload(files); }
      });
      list.addEventListener("click", async (e) => {
        const li = e.target.closest("li");
        if (!li) return;
        if (e.target.classList.contains("js-attach-insert") && ta) insertAtCursor(ta, "[attachment=" + li.dataset.aid + "]");
        if (e.target.classList.contains("js-attach-remove")) {
          const r = await post("/attachment/" + li.dataset.aid + "/remove", {});
          if (r.ok) li.remove();
        }
      });
    });
  }

  // ------------------------------------------------------------------ live updates (SSE)
  function initLive() {
    if (!window.EventSource) return;
    const posts = $("#posts");
    const uid = document.body.dataset.uid;
    const tid = posts && posts.dataset.live === "1" ? posts.dataset.tid : "";
    if (!tid && uid === "0") return;
    const es = new EventSource("/live" + (tid ? "?tid=" + tid : ""));
    const banner = $("#live-banner");
    let pending = 0;
    es.addEventListener("newpost", (ev) => {
      const d = JSON.parse(ev.data);
      if (String(d.uid) === uid) return;
      pending++;
      if (banner) { banner.hidden = false; $("button", banner).textContent = pending === 1 ? "1 new reply — show it" : pending + " new replies — show them"; }
    });
    es.addEventListener("alert", () => {
      const c = $("#alert-count");
      if (c) { c.hidden = false; c.textContent = String((parseInt(c.textContent, 10) || 0) + 1); }
    });
    if (banner) $("button", banner).addEventListener("click", async () => {
      const r = await fetch("/thread/" + tid + "/since/" + posts.dataset.lastPid, { credentials: "same-origin" });
      if (!r.ok) return;
      const html = await r.text();
      const doc = new DOMParser().parseFromString(html, "text/html");
      const arts = $$("article.post", doc);
      arts.forEach((a) => { if (!document.getElementById(a.id)) posts.appendChild(document.importNode(a, true)); });
      if (arts.length) posts.dataset.lastPid = arts[arts.length - 1].dataset.pid;
      pending = 0;
      banner.hidden = true;
    });
  }

  // Prefetch same-origin pages on hover/touch so navigation feels instant. The server treats
  // `Sec-Purpose: prefetch` requests as non-visits (no read marking, no view counts).
  (function () {
    const done = new Set();
    let timer = 0;
    const skip = /^\/(member\/logout|pm(?:\/|$)|admin|modcp|attachment|live|syndication|captcha)/;
    function eligible(a) {
      if (!a || !a.href || a.target || a.hasAttribute("download") || a.dataset.noPrefetch !== undefined) return false;
      let u;
      try { u = new URL(a.href, location.href); } catch (e) { return false; }
      if (u.origin !== location.origin || u.hash && u.pathname === location.pathname) return false;
      if (skip.test(u.pathname) || done.has(u.href) || done.size > 40) return false;
      const c = navigator.connection;
      if (c && (c.saveData || /2g/.test(c.effectiveType || ""))) return false;
      return u.href;
    }
    function prefetch(href) {
      done.add(href);
      const l = document.createElement("link");
      l.rel = "prefetch";
      l.href = href;
      document.head.appendChild(l);
    }
    document.addEventListener("pointerover", (e) => {
      const href = eligible(e.target.closest && e.target.closest("a"));
      if (!href) return;
      clearTimeout(timer);
      timer = setTimeout(() => prefetch(href), 70);
    }, { passive: true });
    document.addEventListener("pointerout", () => clearTimeout(timer), { passive: true });
    document.addEventListener("touchstart", (e) => {
      const href = eligible(e.target.closest && e.target.closest("a"));
      if (href) prefetch(href);
    }, { passive: true });
  })();
})();

// Admin theme editor: preview the brand colour live.
(() => {
  const pick = document.querySelector('.brand-pick input[type=color]');
  const prev = document.querySelector('.brand-preview');
  if (!pick || !prev) return;
  const use = document.querySelector('.brand-pick input[name=use_brand]');
  const apply = () => prev.style.setProperty("--brand", use && !use.checked ? "" : pick.value);
  pick.addEventListener("input", () => { if (use) use.checked = true; apply(); });
  if (use) use.addEventListener("change", apply);
  apply();
})();

// Avatars that fail to load (dead remote URL, missing upload) fall back to the initial-letter markup.
(function () {
  function fallback(img) {
    if (!img.matches("img[data-initial]") || !img.parentNode) return;
    const letter = img.dataset.initial || "?";
    if (img.parentElement.classList.contains("face")) {
      img.replaceWith(document.createTextNode(letter));
    } else if (img.dataset.fallback === "member") {
      const s = document.createElement("span");
      s.className = "avatar-fallback";
      s.style.cssText = "width:32px;height:32px;font-size:.9rem;margin:0";
      s.textContent = letter;
      img.replaceWith(s);
    } else {
      const d = document.createElement("div");
      d.className = "avatar-fallback";
      d.setAttribute("aria-hidden", "true");
      d.textContent = letter;
      img.replaceWith(d);
    }
  }
  document.addEventListener("error", (e) => { if (e.target instanceof HTMLImageElement) fallback(e.target); }, true);
  const sweep = () => document.querySelectorAll("img[data-initial]").forEach((i) => { if (i.complete && i.naturalWidth === 0) fallback(i); });
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", sweep); else sweep();
})();

// Show the chosen file's name next to a styled file picker.
document.addEventListener("change", (e) => {
  const i = e.target;
  if (!(i instanceof HTMLInputElement) || i.type !== "file") return;
  const n = i.closest(".file-pick")?.querySelector(".file-name");
  if (n) n.textContent = i.files && i.files.length ? i.files[0].name : "No file chosen";
});
