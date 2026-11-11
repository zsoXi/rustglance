//! JavaScript run in the page through `Runtime.evaluate`.
//!
//! Each constant is a function expression called as `(f)(arg)`. Nothing is
//! injected ahead of time: the ref registry lives in a symbol-keyed global
//! that the first call creates and later calls reuse until the page reloads.

/// Wraps a function body with the ref registry and helpers in scope as `RG`.
macro_rules! with_rg {
    ($body:literal) => {
        concat!(
            r#"(a) => {
const RG = (() => {
  const k = Symbol.for('rustglance');
  const s = globalThis[k] || (globalThis[k] = { n: 0, ids: new WeakMap(), els: new Map() });
  const ref = (el) => {
    let id = s.ids.get(el);
    if (!id) { id = 'e' + (++s.n); s.ids.set(el, id); s.els.set(id, new WeakRef(el)); }
    return id;
  };
  const get = (id) => { const w = s.els.get(id); const el = w && w.deref(); return el && el.isConnected ? el : null; };
  const INTERACTIVE = 'a[href],button,input:not([type=hidden]),select,textarea,summary,[contenteditable=""],[contenteditable=true],[role=button],[role=link],[role=tab],[role=menuitem],[role=menuitemcheckbox],[role=menuitemradio],[role=checkbox],[role=radio],[role=switch],[role=option],[role=combobox],[role=textbox],[role=searchbox],[role=treeitem],[role=slider],[tabindex]:not([tabindex="-1"])';
  const clean = (t) => (t || '').replace(/\s+/g, ' ').trim();
  const labelled = (el) => {
    const ids = el.getAttribute('aria-labelledby');
    if (!ids) return '';
    return clean(ids.split(/\s+/).map((i) => { const l = document.getElementById(i); return l ? l.innerText : ''; }).join(' '));
  };
  const name = (el) => clean(
    el.getAttribute('aria-label') || labelled(el) ||
    (el.labels && el.labels[0] && el.labels[0].innerText) ||
    el.getAttribute('placeholder') || el.getAttribute('title') || el.getAttribute('alt') ||
    el.innerText || (el.type === 'password' ? '' : el.value));
  const own = (el) => clean([...el.childNodes].filter((n) => n.nodeType === 3).map((n) => n.textContent).join(' '));
  // Opacity is not checked: without a window CSS fade-ins can stay at opacity 0
  // while the element already takes clicks, and hover-only controls start at 0.
  const visible = (el) => el.checkVisibility
    ? el.checkVisibility({ checkVisibilityCSS: true })
    : !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
  // Its children may still show: visibility: hidden can be undone by a child, and
  // display: contents has no box of its own. display: none hides everything below.
  const boxed = (el) => getComputedStyle(el).display === 'contents'
    || (el.checkVisibility ? el.checkVisibility() : el.getClientRects().length > 0);
  const role = (el) => {
    const r = el.getAttribute('role');
    if (r) return r;
    const t = el.tagName.toLowerCase();
    if (t === 'a') return 'link';
    if (t === 'button' || t === 'summary') return 'button';
    if (t === 'select') return 'combobox';
    if (t === 'textarea') return 'textbox';
    if (t === 'input') {
      const ty = el.type || 'text';
      if (ty === 'checkbox' || ty === 'radio' || ty === 'file') return ty;
      if (['button', 'submit', 'reset', 'image'].includes(ty)) return 'button';
      if (ty === 'range') return 'slider';
      return 'textbox';
    }
    if (el.isContentEditable) return 'textbox';
    return t;
  };
  const desc = (el) => {
    let d = el.tagName.toLowerCase();
    if (el.id) d += '#' + el.id;
    const cls = typeof el.className === 'string' ? el.className.trim().split(/\s+/).filter(Boolean).slice(0, 2) : [];
    if (cls.length) d += '.' + cls.join('.');
    const n = name(el);
    return n ? d + ' ' + JSON.stringify(n.length > 80 ? n.slice(0, 80) + '...' : n) : d;
  };
  const find = (text) => {
    const t = clean(text).toLowerCase();
    const all = [...document.querySelectorAll(INTERACTIVE)].filter(visible);
    let hits = all.filter((e) => name(e).toLowerCase() === t);
    if (!hits.length) hits = all.filter((e) => name(e).toLowerCase().includes(t));
    if (hits.length === 1) return { el: hits[0] };
    return { many: hits.map((e) => ref(e) + ' ' + role(e) + ' ' + JSON.stringify(name(e))) };
  };
  return { ref, get, INTERACTIVE, clean, name, own, visible, boxed, role, desc, find };
})();
"#,
            $body,
            "\n}"
        )
    };
}

/// Resolves when the document is loaded and the DOM has not changed for
/// `a.quiet` ms (true), or after `a.max` ms (false).
pub const DOM_QUIET: &str = r#"(a) => new Promise((done) => {
  const start = performance.now();
  let last = start;
  const mo = new MutationObserver(() => { last = performance.now(); });
  mo.observe(document, { subtree: true, childList: true, characterData: true });
  const tick = () => {
    const now = performance.now();
    if (document.readyState === 'complete' && now - last >= a.quiet) { mo.disconnect(); done(true); }
    else if (now - start >= a.max) { mo.disconnect(); done(false); }
    else setTimeout(tick, 50);
  };
  setTimeout(tick, 50);
})"#;

pub const INFO: &str = "() => ({ url: location.href, title: document.title })";

/// `a.rel` parsed by the browser's URL parser against `a.base`; null when invalid.
pub const RESOLVE: &str =
    "(a) => { try { return new URL(a.rel, a.base || undefined).href; } catch (e) { return null; } }";

/// Text outline of the page: interactive elements with refs, headings,
/// text, and landmark groups. `a.within` limits it to one element.
pub const TREE: &str = with_rg!(
    r#"
const root = a.within ? RG.get(a.within) : document.body;
if (!root) return { stale: true };
const GROUPS = { DIALOG: 'dialog', NAV: 'nav', ASIDE: 'aside', MAIN: 'main', FORM: 'form', HEADER: 'header', FOOTER: 'footer', TABLE: 'table', UL: 'list', OL: 'list' };
const GROUP_ROLES = ['dialog', 'alertdialog', 'navigation', 'complementary', 'main', 'tablist', 'menu', 'listbox', 'list', 'table', 'grid', 'region', 'alert', 'status'];
const BLOCKERS = RG.INTERACTIVE + ',h1,h2,h3,h4,h5,h6,dialog,nav,aside,main,form,table,ul,ol,[role]';
const out = [];
const q = (t) => JSON.stringify(t);
const group = (el) => {
  const r = el.getAttribute('role');
  if (r && GROUP_ROLES.includes(r)) return r;
  return GROUPS[el.tagName] || null;
};
const state = (el) => {
  const f = [];
  if (el.matches(':disabled') || el.getAttribute('aria-disabled') === 'true') f.push('disabled');
  if (el.readOnly || el.getAttribute('aria-readonly') === 'true') f.push('readonly');
  if (el.checked || el.getAttribute('aria-checked') === 'true') f.push('checked');
  if (el.getAttribute('aria-selected') === 'true' || (el.tagName === 'OPTION' && el.selected)) f.push('selected');
  const ex = el.getAttribute('aria-expanded');
  if (ex) f.push(ex === 'true' ? 'expanded' : 'collapsed');
  if (el.tagName === 'SELECT') f.push('value=' + q(el.selectedOptions[0] ? el.selectedOptions[0].text : ''));
  else if (el.type === 'password') f.push(el.value ? 'value=<hidden>' : 'value=""');
  else if ((el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') && el.type !== 'checkbox' && el.type !== 'radio') f.push('value=' + q(el.value));
  if (el.tagName === 'A' && el.getAttribute('href')) f.push('-> ' + el.getAttribute('href'));
  return f.length ? ' ' + f.join(' ') : '';
};
const kids = (el) => el.shadowRoot ? [...el.shadowRoot.children, ...el.children] : [...el.children];
const walk = (el, depth) => {
  if (el.getAttribute('aria-hidden') === 'true' || el.inert) return;
  if (['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'SVG'].includes(el.tagName.toUpperCase())) return;
  if (!RG.visible(el)) {
    if (RG.boxed(el)) for (const c of kids(el)) walk(c, depth);
    return;
  }
  const pad = '  '.repeat(depth);
  const nested = el.querySelector(RG.INTERACTIVE);
  if (el.matches(RG.INTERACTIVE)) {
    if (!nested) { out.push(pad + RG.ref(el) + ' ' + RG.role(el) + ' ' + q(RG.name(el)) + state(el)); return; }
    const n = RG.clean(el.getAttribute('aria-label') || el.getAttribute('title') || RG.own(el));
    out.push(pad + RG.ref(el) + ' ' + RG.role(el) + ' ' + q(n) + state(el));
    for (const c of kids(el)) walk(c, depth + 1);
    return;
  }
  const h = /^H([1-6])$/.exec(el.tagName);
  if (h && !nested) { out.push(pad + 'h' + h[1] + ' ' + q(RG.clean(el.innerText))); return; }
  const g = group(el);
  if (g) {
    const label = RG.clean(el.getAttribute('aria-label') || '');
    out.push(pad + g + (label ? ' ' + q(label) : ''));
    for (const c of kids(el)) walk(c, depth + 1);
    return;
  }
  // Keep prose in reading order when inline links interrupt its text nodes.
  // Link refs remain separately actionable instead of moving their words to
  // the end of the sentence (own-text followed by all children).
  if (['P','TD','TH','DT','DD','FIGCAPTION'].includes(el.tagName)
      && nested && !el.querySelector('p,ul,ol,table,div,section,article')) {
    const t = RG.clean(el.innerText);
    if (t) out.push(pad + 'text ' + q(t));
    for (const c of el.querySelectorAll(RG.INTERACTIVE)) {
      const parent = c.parentElement?.closest(RG.INTERACTIVE);
      if (!parent || !el.contains(parent)) walk(c, depth + 1);
    }
    return;
  }
  if (!el.shadowRoot && !el.querySelector(BLOCKERS)) {
    const t = RG.clean(el.innerText);
    if (t) out.push(pad + q(t));
    return;
  }
  const t = RG.own(el);
  if (t) out.push(pad + q(t));
  for (const c of kids(el)) walk(c, depth);
};
walk(root, 0);
let lines = out;
if (a.find) {
  const f = a.find.toLowerCase();
  lines = out.filter((l) => l.toLowerCase().includes(f)).map((l) => l.trim());
}
return { url: location.href, title: document.title, count: lines.length, tree: lines.join('\n') };
"#
);

/// Scrolls the target into view and returns the point to click, or why not.
pub const CLICK_POINT: &str = with_rg!(
    r#"
let el;
if (a.ref) { el = RG.get(a.ref); if (!el) return { stale: true }; }
else { const f = RG.find(a.text); if (!f.el) return { many: f.many }; el = f.el; }
if (el.disabled || el.getAttribute('aria-disabled') === 'true') return { disabled: true, ref: RG.ref(el) };
el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
const r = el.getBoundingClientRect();
if (r.width < 1 || r.height < 1) return { zero: true, ref: RG.ref(el) };
const x = r.left + r.width / 2, y = r.top + r.height / 2;
let hit = document.elementFromPoint(x, y);
while (hit && hit.shadowRoot) {
  const inner = hit.shadowRoot.elementFromPoint(x, y);
  if (!inner || inner === hit) break;
  hit = inner;
}
if (hit && hit !== el && !el.contains(hit)) {
  return { ref: RG.ref(el), covered: { ref: RG.ref(hit), desc: RG.desc(hit) } };
}
return { ref: RG.ref(el), x, y, desc: RG.desc(el) };
"#
);

/// Focuses the field (or picks a `<select>` option) before typing.
pub const FOCUS: &str = with_rg!(
    r#"
const el = a.ref ? RG.get(a.ref) : document.activeElement;
if (!el) return { stale: true };
if (el.type === 'password') return { ref: RG.ref(el), password: true };
if (el.matches(':disabled') || el.getAttribute('aria-disabled') === 'true' || el.closest('[inert]')) return { ref: RG.ref(el), disabled: true };
if (el.readOnly || el.getAttribute('aria-readonly') === 'true') return { ref: RG.ref(el), readonly: true };
el.scrollIntoView({ block: 'center', behavior: 'instant' });
if (el.tagName === 'SELECT' && a.text != null) {
  const t = RG.clean(a.text).toLowerCase();
  const opt = [...el.options].find((o) => RG.clean(o.text).toLowerCase() === t || o.value === a.text);
  if (!opt) return { ref: RG.ref(el), options: [...el.options].map((o) => o.text) };
  if (opt.disabled || (opt.parentElement?.tagName === 'OPTGROUP' && opt.parentElement.disabled)) return { ref: RG.ref(el), disabled: true };
  el.value = opt.value;
  el.dispatchEvent(new Event('input', { bubbles: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
  return { ref: RG.ref(el), selected: opt.text };
}
el.focus();
const act = document.activeElement;
if (act !== el && !el.contains(act)) return { ref: RG.ref(el), unfocusable: RG.desc(el) };
if (a.clear) {
  if (typeof el.select === 'function') el.select();
  else { const r = document.createRange(); r.selectNodeContents(el); const s = getSelection(); s.removeAllRanges(); s.addRange(r); }
}
return { ref: RG.ref(el) };
"#
);

/// The current value of a field, with passwords hidden.
pub const VALUE: &str = with_rg!(
    r#"
const el = RG.get(a.ref);
if (!el) return null;
if (el.type === 'password') return el.value ? '<hidden>' : '';
if (el.tagName === 'SELECT') return el.selectedOptions[0] ? el.selectedOptions[0].text : '';
return 'value' in el ? el.value : el.innerText;
"#
);

/// The file input for `a.ref` (the input itself, one inside it, or the one
/// its label points to), or the only file input on the page.
pub const FILE_INPUT: &str = with_rg!(
    r#"
const all = document.querySelectorAll('input[type=file]');
const only = all.length === 1 ? all[0] : null;
if (!a.ref) return only || (all.length ? 'many' : 'none');
const el = RG.get(a.ref);
if (!el) return 'stale';
if (el.matches('input[type=file]')) return el;
const lab = el.closest('label');
return el.querySelector('input[type=file]') ||
  (el.htmlFor && document.getElementById(el.htmlFor)) ||
  (lab && (lab.control || lab.querySelector('input[type=file]'))) ||
  only || 'none';
"#
);

/// Starts a download of `a.url` through a temporary link.
pub const DOWNLOAD_URL: &str = "(a) => { const l = document.createElement('a'); l.href = a.url; l.download = ''; \
document.body.appendChild(l); l.click(); l.remove(); return true; }";

/// Element box for a screenshot clip, in page coordinates.
pub const RECT: &str = with_rg!(
    r#"
const el = RG.get(a.ref);
if (!el) return null;
el.scrollIntoView({ block: 'center', behavior: 'instant' });
const r = el.getBoundingClientRect();
return { x: r.left + scrollX, y: r.top + scrollY, width: r.width, height: r.height };
"#
);

/// Conditions for `check`; every given one must hold.
pub const CHECK: &str = with_rg!(
    r#"
const body = document.body ? document.body.innerText : '';
const res = {};
if (a.text != null) res.text = body.includes(a.text);
if (a.gone != null) res.gone = !body.includes(a.gone);
if (a.url != null) res.url = location.href.includes(a.url);
if (a.ref != null) res.ref = !!RG.get(a.ref);
return { pass: Object.values(res).every(Boolean), conditions: res, url: location.href };
"#
);

/// GET with the page's cookies. The abort keeps it inside the CDP timeout.
pub const GET: &str = r#"async (a) => {
  const base = /^https?:/.test(location.href) ? location.href : a.base;
  if (!base) throw new Error('no page open and no start_url; call open first');
  const u = new URL(a.path, base);
  if (u.protocol !== 'http:' && u.protocol !== 'https:') throw new Error('only http and https URLs');
  for (const [k, v] of Object.entries(a.query || {})) u.searchParams.set(k, String(v));
  let r;
  try {
    r = await fetch(u, { credentials: 'include', headers: { accept: 'application/json' }, signal: AbortSignal.timeout(a.ms) });
  } catch (e) {
    return { failed: String(e && e.name === 'TimeoutError' ? 'timeout' : e) };
  }
  const text = await r.text();
  let body = text;
  try { body = JSON.parse(text); } catch (e) {}
  return { status: r.status, url: r.url, retry_after: r.headers.get('retry-after'), body };
}"#;
