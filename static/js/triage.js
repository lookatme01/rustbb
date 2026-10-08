// Keyboard triage for the Mod CP inbox. j/k move, a approve, d delete or dismiss, e dismiss,
// o/Enter open, ? shows the cheat-sheet. Actions click the item's real form buttons.
(function () {
  'use strict';
  var list = document.querySelector('[data-triage]');
  var help = document.getElementById('triage-help');
  var helpBtn = document.querySelector('[data-triage-help]');
  var items = list ? Array.prototype.slice.call(list.querySelectorAll('.inbox-item')) : [];
  var cur = -1;

  function focusItem(i) {
    if (!items.length) return;
    i = Math.max(0, Math.min(items.length - 1, i));
    if (cur >= 0) {
      items[cur].classList.remove('is-current');
      items[cur].removeAttribute('aria-current');
    }
    cur = i;
    var el = items[cur];
    el.classList.add('is-current');
    el.setAttribute('aria-current', 'true');
    el.focus({ preventScroll: true });
    el.scrollIntoView({ block: 'nearest' });
  }

  function toggleHelp(force) {
    if (!help) return;
    var show = typeof force === 'boolean' ? force : help.hidden;
    help.hidden = !show;
  }

  function press(sel, confirmMsg) {
    if (cur < 0) return;
    var btn = items[cur].querySelector(sel);
    if (!btn) return;
    if (confirmMsg && !window.confirm(confirmMsg)) return;
    btn.click();
  }

  if (helpBtn) helpBtn.addEventListener('click', function () { toggleHelp(); });

  document.addEventListener('keydown', function (e) {
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    var t = e.target;
    var tag = t && t.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || (t && t.isContentEditable)) return;
    var k = e.key;
    if (k === '?') { e.preventDefault(); toggleHelp(); return; }
    if (k === 'Escape') { toggleHelp(false); return; }
    if (!items.length) return;
    switch (k) {
      case 'j': e.preventDefault(); focusItem(cur < 0 ? 0 : cur + 1); break;
      case 'k': e.preventDefault(); focusItem(cur < 0 ? 0 : cur - 1); break;
      case 'a': press('[data-act="approve"]'); break;
      case 'e': press('[data-act="dismiss"]'); break;
      case 'd':
        if (cur >= 0 && items[cur].querySelector('[data-act="delete"]')) press('[data-act="delete"]', 'Delete this item?');
        else press('[data-act="dismiss"]');
        break;
      case 'o':
      case 'Enter':
        // Enter on a real link or button keeps its own meaning.
        if (k === 'Enter' && (tag === 'A' || tag === 'BUTTON')) return;
        if (cur >= 0 && items[cur].dataset.open) { e.preventDefault(); window.location.href = items[cur].dataset.open; }
        break;
    }
  });

  items.forEach(function (el, i) {
    el.addEventListener('focusin', function () {
      if (cur !== i) {
        if (cur >= 0) { items[cur].classList.remove('is-current'); items[cur].removeAttribute('aria-current'); }
        cur = i;
        el.classList.add('is-current');
        el.setAttribute('aria-current', 'true');
      }
    });
  });
})();
