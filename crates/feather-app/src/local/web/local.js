'use strict';

// Feather's window where it runs as a local page (Linux and others). The
// page talks only to Feather on 127.0.0.1; names go in as text, never HTML.

const $ = (id) => document.getElementById(id);
const PAGE = 200;
const TABS = [
  ['Everything', 'M4 6h16M4 12h16M4 18h16'],
  ['Files', 'M7 3h7l5 5v13H7zM14 3v5h5'],
  ['Folders', 'M3 7h6l2 2h10v10H3z'],
  ['Documents', 'M7 3h7l5 5v13H7zM10 13h6M10 17h6'],
  ['Images', 'M4 5h16v14H4zM4 16l5-5 4 4 3-3 4 4'],
  ['Video', 'M4 6h12v12H4zM16 10l4-2v8l-4-2'],
  ['Audio', 'M9 18V6l10-2v12M9 18a3 3 0 1 1-6 0 3 3 0 0 1 6 0zM19 16a3 3 0 1 1-6 0 3 3 0 0 1 6 0z'],
  ['Archives', 'M4 7h16v13H4zM3 4h18v3H3zM10 11h4'],
  ['Executables', 'M4 5h16v14H4zM8 10l3 2-3 2M13 15h4'],
  ['Code', 'M9 7l-5 5 5 5M15 7l5 5-5 5'],
];
const state = { category: 'Everything', query: '', items: [], total: 0, capped: false, selected: -1, loading: false, sizes: 'auto', generation: 0 };
let controller = null;
let timer = 0;

async function call(path, options = {}) {
  const response = await fetch(path, Object.assign({ credentials: 'same-origin' }, options));
  let data = {};
  try { data = await response.json(); } catch (_) { /* empty */ }
  if (!response.ok) throw new Error(data.error || response.statusText);
  return data;
}

function post(path, fields = {}) {
  return call(path, { method: 'POST', body: new URLSearchParams(fields) });
}

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function icon(path) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('aria-hidden', 'true');
  const shape = document.createElementNS('http://www.w3.org/2000/svg', 'path');
  shape.setAttribute('d', path);
  svg.append(shape);
  return svg;
}

function grouped(number) {
  return number.toLocaleString();
}

function size(bytes, dir) {
  if (dir) return '';
  const unit = (power, name) => {
    const value = bytes / 1024 ** power;
    const places = value >= 100 ? 0 : value >= 10 ? 1 : 2;
    return value.toLocaleString(undefined, { maximumFractionDigits: places, minimumFractionDigits: places }) + ' ' + name;
  };
  if (state.sizes === 'kb') return grouped(Math.ceil(bytes / 1024)) + ' KB';
  if (state.sizes === 'mb') return unit(2, 'MB');
  if (state.sizes === 'gb') return unit(3, 'GB');
  if (bytes < 1024) return bytes + ' B';
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = bytes / 1024;
  let at = 0;
  while (value >= 1024 && at < units.length - 1) { value /= 1024; at += 1; }
  return value.toFixed(1) + ' ' + units[at];
}

function when(seconds) {
  if (!seconds) return '';
  return new Date(seconds * 1000).toLocaleString(undefined, { year: 'numeric', month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
}

function tabs() {
  const nav = $('tabs');
  nav.replaceChildren(...TABS.map(([name, path]) => {
    const button = element('button', 'nav');
    button.type = 'button';
    button.append(icon(path), element('span', '', name));
    if (name === state.category) button.setAttribute('aria-current', 'true');
    button.addEventListener('click', () => { state.category = name; tabs(); search(); });
    return button;
  }));
}

function rowFor(item, index) {
  const row = element('div', 'item');
  row.setAttribute('role', 'option');
  row.dataset.index = String(index);
  const name = element('span', 'name');
  const text = element('span', 'text');
  text.append(element('span', 'title', item.name), element('span', 'where', item.folder));
  name.append(element('span', item.dir ? 'kind dir' : 'kind'), text);
  row.append(name, element('span', 'size', size(item.size, item.dir)), element('span', 'when', when(item.modified)));
  if (index === state.selected) row.setAttribute('aria-selected', 'true');
  return row;
}

function render(append) {
  const list = $('list');
  if (!append) list.replaceChildren();
  const start = list.querySelectorAll('.item').length;
  const rows = state.items.slice(start).map((item, at) => rowFor(item, start + at));
  list.append(...rows);
  const empty = $('empty');
  empty.hidden = state.items.length > 0;
  empty.textContent = state.query || state.category !== 'Everything' ? 'Nothing matches.' : 'Nothing indexed yet. Open Settings to choose places to search.';
  $('count').textContent = state.total ? grouped(state.total) + (state.capped ? '+' : '') + ' found' : '';
}

async function load(append) {
  const generation = ++state.generation;
  if (controller) controller.abort();
  controller = new AbortController();
  state.loading = true;
  const params = new URLSearchParams({ q: state.query, category: state.category, offset: String(append ? state.items.length : 0), limit: String(PAGE) });
  try {
    const data = await call('/api/search?' + params, { signal: controller.signal });
    if (generation !== state.generation) return;
    if (!append) { state.items = []; state.selected = data.items.length ? 0 : -1; }
    state.items.push(...data.items);
    state.total = data.total;
    state.capped = data.capped;
    render(append);
  } catch (error) {
    if (error.name !== 'AbortError') $('count').textContent = error.message;
  } finally {
    state.loading = false;
  }
}

function search() {
  clearTimeout(timer);
  timer = setTimeout(() => load(false), 90);
}

function select(index) {
  if (!state.items.length) return;
  state.selected = Math.max(0, Math.min(index, state.items.length - 1));
  const list = $('list');
  list.querySelectorAll('[aria-selected="true"]').forEach((row) => row.removeAttribute('aria-selected'));
  const row = list.querySelector(`[data-index="${state.selected}"]`);
  if (row) { row.setAttribute('aria-selected', 'true'); row.scrollIntoView({ block: 'nearest' }); }
}

async function act(what, item) {
  if (!item) return;
  if (what === 'copy') {
    try { await navigator.clipboard.writeText(item.path); } catch (_) { /* not allowed */ }
    return;
  }
  try { await post(what === 'open' ? '/api/open' : '/api/reveal', { path: item.path }); } catch (error) { $('count').textContent = error.message; }
}

function hideMenu() { $('menu').hidden = true; }

function showMenu(x, y, item) {
  const menu = $('menu');
  menu.hidden = false;
  menu.style.left = Math.min(x, innerWidth - menu.offsetWidth - 8) + 'px';
  menu.style.top = Math.min(y, innerHeight - menu.offsetHeight - 8) + 'px';
  menu.onclick = (event) => {
    const what = event.target.closest('button')?.dataset.do;
    hideMenu();
    if (what) act(what, item);
  };
}

function itemAt(event) {
  const row = event.target.closest('.item');
  return row ? Number(row.dataset.index) : -1;
}

function applyLook(settings) {
  if (settings.theme === 'system') delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = settings.theme;
  state.sizes = settings.sizes;
}

async function refreshStatus() {
  try {
    const status = await call('/api/status');
    const text = status.indexing ? 'Updating the index…' : grouped(status.items) + ' items';
    $('index').textContent = status.notice ? text + ' · ' + status.notice : text;
    return status;
  } catch (error) {
    $('index').textContent = error.message;
    return null;
  }
}

async function openSettings(firstRun) {
  const settings = await call('/api/settings');
  const status = await call('/api/status');
  $('roots').value = settings.roots.length ? settings.roots.join('\n') : (firstRun ? status.home : '');
  $('excluded').value = settings.excluded.join('\n');
  $('auto_update').value = settings.auto_update === 'off' ? 'off' : settings.auto_update === 'hourly' ? 'hourly' : 'live';
  $('show_hidden').checked = settings.show_hidden;
  $('show_system').checked = settings.show_system;
  $('theme').value = settings.theme;
  $('sizes').value = settings.sizes;
  $('menu').checked = settings.menu || firstRun;
  $('autostart').checked = settings.autostart;
  $('settings-error').textContent = '';
  $('settings').showModal();
}

async function saveSettings() {
  const fields = {
    roots: $('roots').value,
    excluded: $('excluded').value,
    auto_update: $('auto_update').value,
    show_hidden: $('show_hidden').checked ? '1' : '0',
    show_system: $('show_system').checked ? '1' : '0',
    theme: $('theme').value,
    sizes: $('sizes').value,
    menu: $('menu').checked ? '1' : '0',
    autostart: $('autostart').checked ? '1' : '0',
  };
  try {
    await post('/api/settings', fields);
    $('settings').close();
    applyLook({ theme: fields.theme, sizes: fields.sizes });
    refreshStatus();
    search();
  } catch (error) {
    $('settings-error').textContent = error.message;
  }
}

function start() {
  tabs();
  const query = $('query');
  query.addEventListener('input', () => { state.query = query.value; search(); });
  const list = $('list');
  list.addEventListener('click', (event) => { const index = itemAt(event); if (index >= 0) select(index); hideMenu(); });
  list.addEventListener('dblclick', (event) => { const index = itemAt(event); if (index >= 0) act('open', state.items[index]); });
  list.addEventListener('contextmenu', (event) => {
    const index = itemAt(event);
    if (index < 0) return;
    event.preventDefault();
    select(index);
    showMenu(event.clientX, event.clientY, state.items[index]);
  });
  list.addEventListener('scroll', () => {
    if (!state.loading && state.items.length < state.total && list.scrollTop + list.clientHeight > list.scrollHeight - 600) load(true);
  });
  document.addEventListener('click', (event) => { if (!event.target.closest('.menu')) hideMenu(); });
  document.addEventListener('keydown', (event) => {
    if ($('settings').open) return;
    const item = state.items[state.selected];
    if (event.key === 'ArrowDown') { event.preventDefault(); select(state.selected + 1); }
    else if (event.key === 'ArrowUp') { event.preventDefault(); select(state.selected - 1); }
    else if (event.key === 'PageDown') { event.preventDefault(); select(state.selected + 10); }
    else if (event.key === 'PageUp') { event.preventDefault(); select(state.selected - 10); }
    else if (event.key === 'Enter' && item) { event.preventDefault(); act(event.shiftKey || event.ctrlKey ? 'reveal' : 'open', item); }
    else if (event.key === 'Escape') { hideMenu(); query.value = ''; state.query = ''; search(); query.focus(); }
    else if (event.key.length === 1 && !event.ctrlKey && !event.metaKey && !event.altKey && document.activeElement !== query) query.focus();
  });
  $('open-settings').addEventListener('click', () => openSettings(false));
  $('save').addEventListener('click', saveSettings);
  $('cancel').addEventListener('click', () => $('settings').close());
  $('reindex').addEventListener('click', async () => { await post('/api/reindex'); refreshStatus(); });
  $('quit').addEventListener('click', async () => {
    try { await post('/api/quit'); } catch (_) { /* gone */ }
    document.body.replaceChildren(element('p', 'empty', 'Feather has quit. Start it again from your applications menu.'));
  });
  call('/api/settings').then(applyLook).catch(() => {});
  refreshStatus().then((status) => { if (status && !status.roots.length) openSettings(true); });
  setInterval(async () => {
    const before = $('index').textContent;
    await refreshStatus();
    // A finished index shows new results.
    if (before.startsWith('Updating') && !$('index').textContent.startsWith('Updating')) search();
  }, 2000);
  load(false);
}

start();
