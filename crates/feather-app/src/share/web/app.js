'use strict';

// Feather's shared-files page. Everything shown comes from the JSON API and
// goes into the page as text, never as HTML.

const state = { info: null, root: null, path: '', query: '', entries: [] };
const $ = (id) => document.getElementById(id);

// With end-to-end encryption on, every call is sealed with the session
// key made at sign-in; the tunnel and anything else in the middle sees only
// opaque bytes. Otherwise calls are plain HTTPS requests.
const e2e = document.body.dataset.e2e === '1';
const encoder = new TextEncoder();
const decoder = new TextDecoder();
const FRAME = 256 * 1024;
let sessionKey = null;

function signedOut() {
  sessionStorage.removeItem('feather-key');
  location.href = '/login';
  return new Error('Signed out.');
}

function unhex(text) {
  const bytes = new Uint8Array(text.length / 2);
  for (let at = 0; at < bytes.length; at += 1) bytes[at] = parseInt(text.substr(at * 2, 2), 16);
  return bytes;
}

function hex(bytes) {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
}

function concat(...parts) {
  const out = new Uint8Array(parts.reduce((sum, part) => sum + part.length, 0));
  let at = 0;
  for (const part of parts) { out.set(part, at); at += part.length; }
  return out;
}

function index64(index) {
  const bytes = new Uint8Array(8);
  new DataView(bytes.buffer).setBigUint64(0, BigInt(index));
  return bytes;
}

async function key() {
  if (sessionKey) return sessionKey;
  const raw = sessionStorage.getItem('feather-key');
  if (!raw) throw signedOut();
  sessionKey = await crypto.subtle.importKey('raw', unhex(raw), 'AES-GCM', false, ['encrypt', 'decrypt']);
  return sessionKey;
}

/** nonce || ciphertext || tag */
async function sealBytes(aad, plain) {
  const nonce = crypto.getRandomValues(new Uint8Array(12));
  const sealed = await crypto.subtle.encrypt({ name: 'AES-GCM', iv: nonce, additionalData: aad }, await key(), plain);
  return concat(nonce, new Uint8Array(sealed));
}

async function openBytes(aad, sealed) {
  const plain = await crypto.subtle.decrypt(
    { name: 'AES-GCM', iv: sealed.slice(0, 12), additionalData: aad }, await key(), sealed.slice(12));
  return new Uint8Array(plain);
}

async function failure(response) {
  if (response.status === 401) throw signedOut();
  let message = response.statusText || 'Something went wrong.';
  let data = {};
  try { data = await response.json(); message = data.error || message; } catch (_) { /* not JSON */ }
  const error = new Error(message);
  error.status = response.status;
  error.data = data;
  return error;
}

/** One API call: `op` like '/api/list', `params` an object. Returns the reply's JSON. */
async function call(op, params = {}, method = 'GET') {
  if (!e2e) {
    const headers = state.info ? { 'X-Feather-Csrf': state.info.csrf } : {};
    const url = op + (Object.keys(params).length ? '?' + query(params) : '');
    const response = await fetch(url, { method, headers, credentials: 'same-origin' });
    if (!response.ok) throw await failure(response);
    return response.status === 204 ? {} : response.json();
  }
  const sealed = await sealBytes(encoder.encode('feather request'), encoder.encode(query(Object.assign({ op }, params))));
  const response = await fetch('/api/sealed', { method: 'POST', body: sealed, credentials: 'same-origin' });
  if (!response.ok) throw await failure(response);
  const reply = decoder.decode(await openBytes(concat(encoder.encode('feather reply'), sealed.slice(0, 12)),
    new Uint8Array(await response.arrayBuffer())));
  const split = reply.indexOf('\n');
  const code = Number(reply.slice(0, split));
  const data = JSON.parse(reply.slice(split + 1));
  if (code === 401) throw signedOut();
  if (code >= 400) {
    const error = new Error(data.error || 'Something went wrong.');
    error.status = code;
    error.data = data;
    throw error;
  }
  return data;
}

/**
 * A file, or with `thumb: '1'` its preview, fetched sealed and decrypted
 * frame by frame. `progress` hears the share received, 0 to 1.
 */
async function sealedBlob(params, type = '', progress = null) {
  const sealed = await sealBytes(encoder.encode('feather download'), encoder.encode(query(params)));
  const response = await fetch('/api/sealed-download', { method: 'POST', body: sealed, credentials: 'same-origin' });
  if (!response.ok) throw await failure(response);
  const total = Number(response.headers.get('Content-Length')) || 0;
  const label = concat(encoder.encode('feather download'), sealed.slice(0, 12));
  const reader = response.body.getReader();
  const parts = [];
  let pending = new Uint8Array(0);
  let index = 0;
  let received = 0;
  let finished = false;
  for (;;) {
    const { done, value } = await reader.read();
    if (value) {
      pending = concat(pending, value);
      received += value.length;
      if (progress && total) progress(received / total);
    }
    while (pending.length >= 4) {
      const length = new DataView(pending.buffer, pending.byteOffset, 4).getUint32(0);
      if (pending.length < 4 + length) break;
      const frame = await openBytes(concat(label, index64(index)), pending.slice(4, 4 + length));
      pending = pending.slice(4 + length);
      parts.push(frame.slice(1));
      index += 1;
      if (frame[0] === 1) finished = true;
    }
    if (done) break;
  }
  if (!finished || pending.length) throw new Error('The download was cut short.');
  return new Blob(parts, { type });
}

/** Saves a file: a plain link, or, end to end, decrypted frame by frame. */
async function download(root, path, name) {
  const url = URL.createObjectURL(await sealedBlob({ root, path }));
  const link = document.createElement('a');
  link.href = url;
  link.download = name;
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 60000);
}

/** Sends a file: plainly, or sealed in frames with its name sealed too. */
async function send(file, params) {
  if (!e2e) {
    const response = await fetch('/api/upload?' + query(params), {
      method: 'PUT', body: file, credentials: 'same-origin', headers: { 'X-Feather-Csrf': state.info.csrf },
    });
    if (!response.ok) throw await failure(response);
    return response.json();
  }
  const meta = await sealBytes(encoder.encode('feather upload'), encoder.encode(query(params)));
  const label = concat(encoder.encode('feather upload'), meta.slice(0, 12));
  const frames = [];
  const count = Math.max(1, Math.ceil(file.size / FRAME));
  for (let index = 0; index < count; index += 1) {
    const data = new Uint8Array(await file.slice(index * FRAME, (index + 1) * FRAME).arrayBuffer());
    const frame = await sealBytes(concat(label, index64(index)), concat(new Uint8Array([index + 1 === count ? 1 : 0]), data));
    const length = new Uint8Array(4);
    new DataView(length.buffer).setUint32(0, frame.length);
    frames.push(length, frame);
  }
  const response = await fetch('/api/sealed-upload', {
    method: 'PUT', body: new Blob(frames), credentials: 'same-origin', headers: { 'X-Feather-Sealed': hex(meta) },
  });
  if (!response.ok) throw await failure(response);
  return response.json();
}

function query(params) {
  return Object.entries(params)
    .map(([key, value]) => encodeURIComponent(key) + '=' + encodeURIComponent(value))
    .join('&');
}

function status(text, isError = false) {
  const line = $('status');
  line.textContent = text || '';
  line.classList.toggle('error', isError);
}

function size(bytes, dir) {
  if (dir) return '';
  if (bytes < 1024) return bytes + ' B';
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit += 1; }
  return value.toFixed(value < 10 ? 1 : 0) + ' ' + units[unit];
}

function when(seconds) {
  if (!seconds) return '';
  return new Date(seconds * 1000).toLocaleString(undefined, {
    year: 'numeric', month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit',
  });
}

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function join(base, name) {
  return base ? base + '/' + name : name;
}

function downloadLink(root, path) {
  return '/api/download?' + query({ root, path });
}

const DOWNLOAD_ICON = '<svg viewBox="0 0 20 20" aria-hidden="true"><path d="M10 3v10m-4-4 4 4 4-4M4 16h12"/></svg>';

function zipLink(root, path, names) {
  const params = { root, path };
  let link = '/api/zip?' + query(params);
  for (const name of names || []) link += '&' + query({ name });
  return link;
}

/**
 * Saves a folder, or some of what's in it, as one zip. The server streams
 * it, so the browser's own download list shows the progress. End to end,
 * folders can't be zipped: sealed downloads are decrypted in the page's
 * memory, and a folder can be far bigger than a phone can hold.
 */
function saveZip(root, path, label, names) {
  if (e2e) {
    status('Folders can’t be downloaded as a zip while the share is encrypted end to end. '
      + 'Open the folder and download its files one at a time.', true);
    return;
  }
  const link = document.createElement('a');
  link.href = zipLink(root, path, names);
  link.setAttribute('download', label + '.zip');
  document.body.append(link);
  link.click();
  link.remove();
  status('Zipping ' + label + '… Your browser shows the download as it arrives.');
  setTimeout(() => { if ($('status').textContent.startsWith('Zipping ' + label)) status(''); }, 8000);
}

function folderLabel(root, path) {
  const parts = path.split('/').filter(Boolean);
  return parts.length ? parts[parts.length - 1] : rootName(root);
}

async function save(root, path, name) {
  status('Downloading ' + name + '…');
  try {
    await download(root, path, name);
    status('');
  } catch (error) {
    status(error.message, true);
  }
}

function rootName(root) {
  const found = state.info.roots.find((item) => item.id === root);
  return found ? found.name : '';
}

function trail() {
  const nav = $('trail');
  nav.replaceChildren();
  const crumb = (label, action) => {
    const button = element('button', '', label);
    button.type = 'button';
    button.addEventListener('click', action);
    nav.append(button);
  };
  const separator = () => nav.append(element('span', 'sep', '/'));
  if (state.query) {
    crumb('Search results for “' + state.query + '”', () => {});
    return;
  }
  if (state.info.roots.length > 1) {
    crumb('Shared folders', () => open(null, ''));
  }
  if (state.root === null) return;
  if (state.info.roots.length > 1) separator();
  crumb(rootName(state.root), () => open(state.root, ''));
  let walked = '';
  for (const name of state.path.split('/').filter(Boolean)) {
    walked = join(walked, name);
    const target = walked;
    separator();
    crumb(name, () => open(state.root, target));
  }
}

// ----- Pictures and videos ---------------------------------------------------

// Pictures a browser shows itself, ones only Feather's preview can show
// (HEIC from phones, TIFF), and videos.
const PICTURE = /\.(jpe?g|jfif|png|gif|webp|avif|bmp)$/i;
const STILL = /\.(heic|heif|tiff?)$/i;
const VIDEO = /\.(mp4|m4v|webm|mov|ogv|mkv)$/i;
const TYPES = {
  jpg: 'image/jpeg', jpeg: 'image/jpeg', jfif: 'image/jpeg', png: 'image/png', gif: 'image/gif',
  webp: 'image/webp', avif: 'image/avif', bmp: 'image/bmp', mp4: 'video/mp4', m4v: 'video/mp4',
  webm: 'video/webm', mov: 'video/quicktime', ogv: 'video/ogg', mkv: 'video/x-matroska',
};
const THUMB = 320;
const BIG = 2048;

function mediaKind(name) {
  if (PICTURE.test(name)) return 'picture';
  if (STILL.test(name)) return 'still';
  if (VIDEO.test(name)) return 'video';
  return null;
}

function typeOf(name) {
  return TYPES[name.slice(name.lastIndexOf('.') + 1).toLowerCase()] || '';
}

function inlineLink(root, path) {
  return '/api/download?' + query({ root, path, inline: '1' });
}

function thumbLink(root, path, side = THUMB) {
  return '/api/thumb?' + query({ root, path, size: side });
}

// End to end, previews are fetched sealed, a few at a time, as they come
// into view; their decrypted copies stay for the visit.
const sealedThumbs = new Map();
const thumbQueue = [];
let thumbsBusy = 0;

function pumpThumbs() {
  while (thumbsBusy < 3 && thumbQueue.length) {
    const job = thumbQueue.shift();
    thumbsBusy += 1;
    job().finally(() => { thumbsBusy -= 1; pumpThumbs(); });
  }
}

function sealedThumb(entry) {
  const key = entry.root + '/' + entry.path;
  if (!sealedThumbs.has(key)) {
    sealedThumbs.set(key, new Promise((resolve, reject) => {
      thumbQueue.push(() => sealedBlob({ root: entry.root, path: entry.path, thumb: '1', size: THUMB }, 'image/jpeg')
        .catch(() => (entry.kind === 'picture'
          ? sealedBlob({ root: entry.root, path: entry.path }, typeOf(entry.name)) : Promise.reject()))
        .then((blob) => resolve(URL.createObjectURL(blob)), reject));
      pumpThumbs();
    }));
  }
  return sealedThumbs.get(key);
}

const watcher = 'IntersectionObserver' in window ? new IntersectionObserver((seen) => {
  for (const sighting of seen) {
    if (!sighting.isIntersecting) continue;
    watcher.unobserve(sighting.target);
    sighting.target.load();
  }
}, { rootMargin: '300px' }) : null;

/** A preview image for `entry`, loaded when it scrolls near. */
function thumbImage(entry, className) {
  const frame = element('span', className + ' ' + entry.kind);
  const img = element('img');
  img.alt = '';
  img.decoding = 'async';
  img.draggable = false;
  const failed = () => { img.remove(); frame.classList.add('none'); };
  img.addEventListener('load', () => frame.classList.add('ready'));
  if (!e2e) {
    img.loading = 'lazy';
    img.addEventListener('error', () => {
      // Without Windows' previews, pictures show themselves.
      if (entry.kind === 'picture' && !img.dataset.full) {
        img.dataset.full = '1';
        img.src = inlineLink(entry.root, entry.path);
      } else {
        failed();
      }
    });
    img.src = thumbLink(entry.root, entry.path);
  } else {
    frame.load = () => sealedThumb(entry).then((url) => { img.src = url; }, failed);
    if (watcher) watcher.observe(frame); else frame.load();
  }
  frame.append(img);
  if (entry.kind === 'video') frame.append(element('span', 'play'));
  return frame;
}

// ----- The video player ------------------------------------------------------

const ICONS = {
  play: '<svg viewBox="0 0 20 20" aria-hidden="true"><path class="fill" d="M6 4.2v11.6a.6.6 0 0 0 .9.5l9.2-5.8a.6.6 0 0 0 0-1L6.9 3.7a.6.6 0 0 0-.9.5Z"/></svg>',
  pause: '<svg viewBox="0 0 20 20" aria-hidden="true"><rect class="fill" x="5" y="4" width="3.4" height="12" rx="1"/><rect class="fill" x="11.6" y="4" width="3.4" height="12" rx="1"/></svg>',
  loop: '<svg viewBox="0 0 20 20" aria-hidden="true"><path d="M4 9V8a3 3 0 0 1 3-3h9m-2.5-2.5L16 5l-2.5 2.5M16 11v1a3 3 0 0 1-3 3H4m2.5 2.5L4 15l2.5-2.5"/></svg>',
  rotate: '<svg viewBox="0 0 20 20" aria-hidden="true"><path d="M15.5 9.5A5.5 5.5 0 1 1 13 4.9M13.5 2v3.2h-3.2"/></svg>',
  sound: '<svg viewBox="0 0 20 20" aria-hidden="true"><path d="M3.5 8v4h3l4 3.5v-11l-4 3.5h-3Z"/><path d="M13.5 7.5a3.5 3.5 0 0 1 0 5M15.5 5.5a6.4 6.4 0 0 1 0 9"/></svg>',
  muted: '<svg viewBox="0 0 20 20" aria-hidden="true"><path d="M3.5 8v4h3l4 3.5v-11l-4 3.5h-3Z"/><path d="m13.5 8 4 4m0-4-4 4"/></svg>',
  full: '<svg viewBox="0 0 20 20" aria-hidden="true"><path d="M3.5 7.5v-4h4M12.5 3.5h4v4M16.5 12.5v4h-4M7.5 16.5h-4v-4"/></svg>',
  shrink: '<svg viewBox="0 0 20 20" aria-hidden="true"><path d="M7.5 3.5v4h-4M16.5 7.5h-4v-4M12.5 16.5v-4h4M3.5 12.5h4v4"/></svg>',
};

function clock(seconds) {
  if (!Number.isFinite(seconds)) return '0:00';
  const whole = Math.max(0, Math.floor(seconds));
  const h = Math.floor(whole / 3600);
  const m = Math.floor((whole % 3600) / 60);
  const s = String(whole % 60).padStart(2, '0');
  return h ? h + ':' + String(m).padStart(2, '0') + ':' + s : m + ':' + s;
}

function iconButton(className, label, icon) {
  const button = element('button', 'pbtn ' + className);
  button.type = 'button';
  button.setAttribute('aria-label', label);
  button.title = label;
  button.innerHTML = ICONS[icon];
  return button;
}

function remembered(name) {
  try { return localStorage.getItem(name) === '1'; } catch (_) { return false; }
}

function remember(name, on) {
  try { localStorage.setItem(name, on ? '1' : '0'); } catch (_) { /* private mode */ }
}

/**
 * A video with Feather's own controls, made for thumbs: a big play button,
 * a seek bar that shows what has loaded, repeat, rotate, and full screen
 * that turns a phone sideways. Double-tap a side to skip 10 seconds.
 */
function videoPlayer(source, entry) {
  const shell = element('div', 'player');
  const video = document.createElement('video');
  video.playsInline = true;
  video.preload = 'metadata';
  video.autoplay = true;
  video.loop = remembered('feather-loop');
  video.muted = remembered('feather-muted');
  video.src = source;
  video.setAttribute('aria-label', entry.name);

  const big = iconButton('big', 'Play', 'play');
  const skip = element('div', 'skip');
  const controls = element('div', 'controls');
  const seek = element('input', 'seek');
  seek.type = 'range';
  seek.min = '0';
  seek.max = '1000';
  seek.step = '1';
  seek.value = '0';
  seek.setAttribute('aria-label', 'Seek');
  const play = iconButton('toggle', 'Play', 'play');
  const time = element('span', 'ptime', '0:00 / 0:00');
  const gap = element('span', 'pgap');
  const loop = iconButton('loop', 'Repeat', 'loop');
  const rotate = iconButton('rotate', 'Rotate', 'rotate');
  const sound = iconButton('sound', 'Mute', video.muted ? 'muted' : 'sound');
  const full = iconButton('full', 'Full screen', 'full');
  const row = element('div', 'prow');
  row.append(play, time, gap, loop, rotate, sound, full);
  controls.append(seek, row);
  shell.append(video, big, skip, controls);

  let turn = 0;
  let idleTimer = 0;
  let seeking = false;

  const fit = () => {
    const sideways = turn % 180 !== 0;
    if (sideways) {
      video.style.width = shell.clientHeight + 'px';
      video.style.height = shell.clientWidth + 'px';
    } else {
      video.style.width = '';
      video.style.height = '';
    }
    video.style.transform = 'translate(-50%, -50%) rotate(' + turn + 'deg)';
  };
  const resized = 'ResizeObserver' in window ? new ResizeObserver(fit) : null;
  if (resized) resized.observe(shell);
  // A short word in the middle, so a tap on a small button is confirmed.
  const note = element('div', 'pnote');
  let noteTimer = 0;
  const say = (text) => {
    note.textContent = text;
    note.classList.add('show');
    clearTimeout(noteTimer);
    noteTimer = setTimeout(() => note.classList.remove('show'), 900);
  };
  const wake = () => {
    shell.classList.remove('idle');
    clearTimeout(idleTimer);
    if (!video.paused) idleTimer = setTimeout(() => shell.classList.add('idle'), 2600);
  };
  const toggle = () => {
    if (video.paused || video.ended) video.play().catch(() => {}); else video.pause();
  };
  const paint = () => {
    const playing = !video.paused && !video.ended;
    play.innerHTML = ICONS[playing ? 'pause' : 'play'];
    play.setAttribute('aria-label', playing ? 'Pause' : 'Play');
    play.title = playing ? 'Pause' : 'Play';
    big.hidden = playing;
    shell.classList.toggle('paused', !playing);
    loop.setAttribute('aria-pressed', String(video.loop));
    loop.title = video.loop ? 'Repeat is on' : 'Repeat';
    sound.innerHTML = ICONS[video.muted ? 'muted' : 'sound'];
    sound.setAttribute('aria-label', video.muted ? 'Unmute' : 'Mute');
    const inFull = document.fullscreenElement === shell;
    full.innerHTML = ICONS[inFull ? 'shrink' : 'full'];
    full.setAttribute('aria-label', inFull ? 'Leave full screen' : 'Full screen');
  };
  const progress = () => {
    const length = video.duration;
    const at = Number.isFinite(length) && length > 0 ? video.currentTime / length : 0;
    let loaded = 0;
    for (let index = 0; index < video.buffered.length; index += 1) {
      if (video.buffered.start(index) <= video.currentTime + 0.5) loaded = Math.max(loaded, video.buffered.end(index));
    }
    const buffered = Number.isFinite(length) && length > 0 ? loaded / length : 0;
    if (!seeking) seek.value = String(Math.round(at * 1000));
    seek.style.setProperty('--played', (at * 100).toFixed(2) + '%');
    seek.style.setProperty('--loaded', (Math.max(at, buffered) * 100).toFixed(2) + '%');
    time.textContent = clock(video.currentTime) + ' / ' + clock(length);
  };

  big.addEventListener('click', (event) => { event.stopPropagation(); toggle(); wake(); });
  play.addEventListener('click', () => { toggle(); wake(); });
  loop.addEventListener('click', () => {
    video.loop = !video.loop;
    remember('feather-loop', video.loop);
    say(video.loop ? 'Repeat on' : 'Repeat off');
    paint();
    wake();
  });
  sound.addEventListener('click', () => { video.muted = !video.muted; remember('feather-muted', video.muted); paint(); wake(); });
  rotate.addEventListener('click', () => { turn = (turn + 90) % 360; fit(); say('Rotated ' + turn + '\u00b0'); wake(); });
  full.addEventListener('click', async () => {
    wake();
    if (document.fullscreenElement) { document.exitFullscreen().catch(() => {}); return; }
    if (shell.requestFullscreen) {
      try {
        await shell.requestFullscreen({ navigationUI: 'hide' });
        // A wide video turns the phone sideways, where phones allow it.
        const wide = (video.videoWidth >= video.videoHeight) !== (turn % 180 !== 0);
        if (screen.orientation && screen.orientation.lock) {
          screen.orientation.lock(wide ? 'landscape' : 'portrait').catch(() => {});
        }
      } catch (_) { /* refused */ }
    } else if (video.webkitEnterFullscreen) {
      // iPhone: only the system player goes full screen.
      video.webkitEnterFullscreen();
    }
  });
  const fullChanged = () => {
    if (document.fullscreenElement !== shell && screen.orientation && screen.orientation.unlock) {
      try { screen.orientation.unlock(); } catch (_) { /* not locked */ }
    }
    paint();
    requestAnimationFrame(fit);
  };
  document.addEventListener('fullscreenchange', fullChanged);

  seek.addEventListener('input', () => {
    seeking = true;
    if (Number.isFinite(video.duration)) {
      video.currentTime = (Number(seek.value) / 1000) * video.duration;
      seek.style.setProperty('--played', (Number(seek.value) / 10).toFixed(2) + '%');
      time.textContent = clock(video.currentTime) + ' / ' + clock(video.duration);
    }
    wake();
  });
  seek.addEventListener('change', () => { seeking = false; });

  // Taps: the mouse plays and pauses; a finger shows or hides the controls;
  // two quick taps on a side skip 10 seconds.
  let lastTap = 0;
  video.addEventListener('pointerup', (event) => {
    if (viewer.swiped) return;
    const now = Date.now();
    const box = shell.getBoundingClientRect();
    const side = event.clientX < box.left + box.width / 3 ? -1 : event.clientX > box.right - box.width / 3 ? 1 : 0;
    if (now - lastTap < 320 && side && Number.isFinite(video.duration)) {
      video.currentTime = Math.min(video.duration, Math.max(0, video.currentTime + side * 10));
      skip.textContent = side < 0 ? '− 10 s' : '+ 10 s';
      skip.className = 'skip show ' + (side < 0 ? 'left' : 'right');
      setTimeout(() => { skip.className = 'skip'; }, 500);
      lastTap = 0;
      wake();
      return;
    }
    lastTap = now;
    if (event.pointerType === 'mouse') { toggle(); wake(); return; }
    if (shell.classList.contains('idle') || video.paused) wake(); else shell.classList.add('idle');
  });
  shell.addEventListener('pointermove', (event) => { if (event.pointerType === 'mouse') wake(); });
  controls.addEventListener('pointerdown', () => wake());

  for (const name of ['play', 'pause', 'ended', 'volumechange']) video.addEventListener(name, () => { paint(); wake(); });
  for (const name of ['timeupdate', 'progress', 'durationchange', 'loadedmetadata', 'seeked']) video.addEventListener(name, progress);
  video.addEventListener('loadedmetadata', fit);
  video.addEventListener('play', () => { if (document.activeElement === big) play.focus({ preventScroll: true }); });
  window.addEventListener('resize', fit);
  shell.append(note);
  shell.cleanup = () => {
    clearTimeout(idleTimer);
    clearTimeout(noteTimer);
    if (resized) resized.disconnect();
    window.removeEventListener('resize', fit);
    document.removeEventListener('fullscreenchange', fullChanged);
    if (document.fullscreenElement === shell) document.exitFullscreen().catch(() => {});
  };
  shell.toggle = toggle;
  shell.wake = wake;
  requestAnimationFrame(fit);
  paint();
  wake();
  return { shell, video };
}

// ----- The viewer -------------------------------------------------------------

const viewer = { list: [], at: -1, media: null, shell: null, sealed: new Map(), wheelUntil: 0, swiped: false };

/** Where the full picture or video comes from: a link, or a decrypted copy. */
async function mediaSource(entry, progress) {
  if (!e2e) {
    return entry.kind === 'still' ? thumbLink(entry.root, entry.path, BIG) : inlineLink(entry.root, entry.path);
  }
  const key = entry.root + '/' + entry.path;
  if (!viewer.sealed.has(key)) {
    const params = entry.kind === 'still'
      ? { root: entry.root, path: entry.path, thumb: '1', size: BIG }
      : { root: entry.root, path: entry.path };
    const type = entry.kind === 'still' ? 'image/jpeg' : typeOf(entry.name);
    viewer.sealed.set(key, sealedBlob(params, type, progress).then((blob) => URL.createObjectURL(blob)));
    // Keep a few decrypted copies, not the whole folder.
    while (viewer.sealed.size > 4) {
      const [oldest, url] = viewer.sealed.entries().next().value;
      viewer.sealed.delete(oldest);
      url.then((link) => URL.revokeObjectURL(link), () => {});
    }
  }
  try {
    return await viewer.sealed.get(key);
  } catch (error) {
    viewer.sealed.delete(key);
    throw error;
  }
}

function stopMedia() {
  if (viewer.shell && viewer.shell.cleanup) viewer.shell.cleanup();
  if (viewer.media && viewer.media.tagName === 'VIDEO') {
    viewer.media.pause();
    viewer.media.removeAttribute('src');
    viewer.media.load();
  }
  viewer.media = null;
  viewer.shell = null;
}

function viewerNote(text) {
  const note = element('div', 'viewer-note', text);
  $('viewer-stage').replaceChildren(note);
  return note;
}

async function showAt(at) {
  const list = viewer.list;
  if (!list.length) return;
  at = (at + list.length) % list.length;
  viewer.at = at;
  const entry = list[at];
  stopMedia();
  $('viewer-name').textContent = entry.name;
  $('viewer-count').textContent = list.length > 1 ? (at + 1) + ' of ' + list.length : '';
  $('viewer-prev').hidden = $('viewer-next').hidden = list.length < 2;
  const saveButton = $('viewer-save');
  saveButton.href = downloadLink(entry.root, entry.path);
  saveButton.setAttribute('download', entry.name);
  for (const [index, thumb] of Array.from($('viewer-strip').children).entries()) {
    const current = index === at;
    thumb.classList.toggle('current', current);
    thumb.setAttribute('aria-current', current ? 'true' : 'false');
    if (current) thumb.scrollIntoView({ block: 'nearest', inline: 'center', behavior: 'smooth' });
  }
  const loading = viewerNote(e2e ? 'Decrypting…' : '');
  let source;
  try {
    source = await mediaSource(entry, (share) => {
      if (viewer.at === at) loading.textContent = 'Decrypting… ' + Math.round(share * 100) + '%';
    });
  } catch (error) {
    if (viewer.at === at) viewerNote(error.message || 'This can’t be shown.');
    return;
  }
  if (viewer.at !== at) return;
  let media;
  let shell;
  if (entry.kind === 'video') {
    const player = videoPlayer(source, entry);
    media = player.video;
    shell = player.shell;
    media.addEventListener('error', () => {
      if (viewer.media !== media) return;
      const network = media.error && media.error.code === 2;
      viewerNote(network
        ? 'The video stopped loading. The computer sharing it uploads slowly; try again, or download it.'
        : 'This video can’t play in the browser. Download it to watch it.');
    });
    watchLoading(media);
    shell.append(media.loadingNote);
  } else {
    media = document.createElement('img');
    media.alt = entry.name;
    media.decoding = 'async';
    media.draggable = false;
    media.className = 'viewer-media';
    media.addEventListener('error', () => {
      if (viewer.media !== media) return;
      // No preview (away from Windows): wait for the picture itself.
      if (!media.dataset.full && media.src.indexOf('/api/thumb') >= 0) {
        media.dataset.full = '1';
        media.src = source;
        return;
      }
      viewerNote('This picture can’t be shown. Download it to open it.');
    });
    if (!e2e && entry.kind === 'picture') {
      // The preview, already loaded for the grid, shows at once; the full
      // picture takes its place when it has arrived and decoded.
      media.src = thumbLink(entry.root, entry.path);
      const full = new Image();
      full.src = source;
      full.decode().then(() => { if (viewer.media === media) media.src = source; },
        () => { if (viewer.media === media) media.src = source; });
    } else {
      media.src = source;
    }
    shell = media;
  }
  viewer.media = media;
  viewer.shell = shell;
  $('viewer-stage').replaceChildren(shell);
  if (!e2e) {
    // The pictures either side load now, so flipping is instant.
    for (const step of [1, -1]) {
      const near = list[(at + step + list.length) % list.length];
      if (near.kind !== 'video') new Image().src = near.kind === 'still'
        ? thumbLink(near.root, near.path, BIG) : inlineLink(near.root, near.path);
    }
  }
}

/**
 * Says so when a video is slow to arrive, with how much has loaded, instead
 * of a black box: over the internet the computer's upload sets the speed.
 */
function watchLoading(media) {
  const note = element('div', 'viewer-loading');
  note.hidden = true;
  let since = Date.now();
  const loaded = () => {
    if (!media.duration || !media.buffered.length) return '';
    const end = media.buffered.end(media.buffered.length - 1);
    return ' ' + Math.min(99, Math.round((end / media.duration) * 100)) + '%';
  };
  const tick = setInterval(() => {
    if (viewer.media !== media) { clearInterval(tick); return; }
    const stuck = media.readyState < 3 && !media.paused && Date.now() - since > 3000;
    const starting = media.readyState < 2 && Date.now() - since > 3000;
    note.hidden = !(stuck || starting);
    if (!note.hidden) {
      note.textContent = 'Loading' + loaded() + '… The computer sharing this uploads slowly, so videos take '
        + 'a while over the internet. On the same Wi-Fi they start right away.';
    }
  }, 1000);
  media.addEventListener('waiting', () => { since = Date.now(); });
  media.addEventListener('playing', () => { note.hidden = true; });
  media.addEventListener('canplay', () => { note.hidden = true; since = Date.now(); });
  media.loadingNote = note;
}

function step(by) {
  if (viewer.list.length > 1) showAt(viewer.at + by);
}

function openViewer(entry) {
  viewer.list = state.entries.filter((candidate) => candidate.kind);
  const at = viewer.list.findIndex((candidate) => candidate.root === entry.root && candidate.path === entry.path);
  const strip = $('viewer-strip');
  strip.replaceChildren(...viewer.list.map((candidate, index) => {
    const button = element('button', 'strip-item');
    button.type = 'button';
    button.setAttribute('aria-label', candidate.name);
    button.append(thumbImage(candidate, 'thumb'));
    button.addEventListener('click', () => showAt(index));
    return button;
  }));
  strip.hidden = viewer.list.length < 2;
  const box = $('viewer');
  box.hidden = false;
  document.body.classList.add('viewing');
  // The phone's back button closes the viewer instead of leaving.
  if (!history.state || !history.state.viewer) history.pushState({ viewer: true }, '');
  showAt(Math.max(0, at));
  $('viewer-close').focus({ preventScroll: true });
}

function closeViewer(fromHistory = false) {
  if ($('viewer').hidden) return;
  stopMedia();
  $('viewer-stage').replaceChildren();
  $('viewer').hidden = true;
  document.body.classList.remove('viewing');
  viewer.at = -1;
  if (!fromHistory && history.state && history.state.viewer) history.back();
}

function wireViewer() {
  $('viewer-close').addEventListener('click', () => closeViewer());
  $('viewer-prev').addEventListener('click', () => step(-1));
  $('viewer-next').addEventListener('click', () => step(1));
  $('viewer-save').addEventListener('click', (event) => {
    if (!e2e) return;
    event.preventDefault();
    const entry = viewer.list[viewer.at];
    if (entry) save(entry.root, entry.path, entry.name);
  });
  window.addEventListener('popstate', () => closeViewer(true));
  document.addEventListener('keydown', (event) => {
    if ($('viewer').hidden) return;
    if (event.key === 'Escape') { event.preventDefault(); closeViewer(); }
    // Space plays and pauses; arrows move the seek bar while it has focus.
    const focus = document.activeElement;
    if (event.key === ' ' && viewer.shell && viewer.shell.toggle && !(focus && /BUTTON|INPUT|A/.test(focus.tagName))) {
      event.preventDefault();
      viewer.shell.toggle();
      viewer.shell.wake();
    }
    const inVideo = focus && (focus.tagName === 'VIDEO' || focus.tagName === 'INPUT');
    if (event.key === 'ArrowRight' && !inVideo) { event.preventDefault(); step(1); }
    if (event.key === 'ArrowLeft' && !inVideo) { event.preventDefault(); step(-1); }
  });
  const stage = $('viewer-stage');
  // Clicking the dark around a picture closes, like most viewers.
  stage.addEventListener('click', (event) => {
    // The click that ends a swipe isn't a click on the dark.
    if (viewer.swiped) { viewer.swiped = false; return; }
    if (event.target === stage) closeViewer();
  });
  // A sideways scroll on a touchpad flips once per gesture.
  stage.addEventListener('wheel', (event) => {
    if (Math.abs(event.deltaX) < 30 || Math.abs(event.deltaX) < Math.abs(event.deltaY)) return;
    event.preventDefault();
    const now = Date.now();
    if (now < viewer.wheelUntil) { viewer.wheelUntil = now + 250; return; }
    viewer.wheelUntil = now + 450;
    step(event.deltaX > 0 ? 1 : -1);
  }, { passive: false });
  // Swiping: the picture follows the finger, then flips past a threshold.
  let swipe = null;
  stage.addEventListener('pointerdown', (event) => {
    if (event.pointerType === 'mouse' && event.button !== 0) return;
    // The player's own buttons and seek bar aren't swiped.
    if (event.target.closest && event.target.closest('.controls, .pbtn')) return;
    swipe = { x: event.clientX, y: event.clientY, id: event.pointerId, moved: false };
  });
  stage.addEventListener('pointermove', (event) => {
    if (!swipe || event.pointerId !== swipe.id) return;
    const dx = event.clientX - swipe.x;
    const dy = event.clientY - swipe.y;
    if (!swipe.moved && Math.abs(dx) > 10 && Math.abs(dx) > Math.abs(dy)) {
      swipe.moved = true;
      stage.setPointerCapture(event.pointerId);
    }
    if (swipe.moved && viewer.shell) {
      viewer.shell.style.translate = dx + 'px 0';
      viewer.shell.style.transition = 'none';
    }
  });
  const end = (event) => {
    if (!swipe || event.pointerId !== swipe.id) return;
    const dx = event.clientX - swipe.x;
    const moved = swipe.moved;
    swipe = null;
    const shell = viewer.shell;
    if (!moved || !shell) return;
    viewer.swiped = true;
    setTimeout(() => { viewer.swiped = false; }, 400);
    shell.style.transition = '';
    if (Math.abs(dx) > Math.min(90, window.innerWidth / 5) && viewer.list.length > 1) {
      step(dx < 0 ? 1 : -1);
    } else {
      shell.style.translate = '';
    }
  };
  stage.addEventListener('pointerup', end);
  stage.addEventListener('pointercancel', end);
}

// ----- Folder contents ----------------------------------------------------------

function entryFor(item, root, path, where, top = false) {
  return { ...item, root, path, where, top, kind: item.dir ? null : mediaKind(item.name) };
}

function activate(entry) {
  if (entry.dir) {
    state.query = '';
    $('query').value = '';
    open(entry.root, entry.path);
  } else if (entry.kind) {
    openViewer(entry);
  }
}

/** A plain file's link saves it; end to end it is decrypted first. */
function saveLink(entry, className, text) {
  const link = element('a', className, text);
  link.href = downloadLink(entry.root, entry.path);
  link.setAttribute('download', entry.name);
  if (e2e) link.addEventListener('click', (event) => { event.preventDefault(); save(entry.root, entry.path, entry.name); });
  return link;
}

function row(entry) {
  const tr = document.createElement('tr');
  const name = element('td', 'name');
  let link;
  if (entry.dir || entry.kind) {
    link = element('button', 'entry');
    link.type = 'button';
    link.addEventListener('click', () => activate(entry));
  } else {
    link = saveLink(entry, 'entry');
  }
  link.append(entry.kind ? thumbImage(entry, 'mini') : element('span', entry.dir ? 'kind dir' : 'kind'));
  const text = element('span', 'label', entry.name);
  if (entry.where !== undefined) text.append(element('span', 'where', entry.where));
  link.append(text);
  name.append(link);
  tr.append(name, element('td', 'size', size(entry.size, entry.dir)), element('td', 'when', when(entry.modified)));
  const actions = element('td', 'actions');
  if (entry.kind) {
    const get = saveLink(entry, 'icon-button', '');
    get.setAttribute('aria-label', 'Download ' + entry.name);
    get.title = 'Download';
    get.innerHTML = DOWNLOAD_ICON;
    actions.append(get);
  } else if (entry.dir && !e2e) {
    actions.append(zipButton(entry, 'icon-button'));
  }
  if (state.info.mode === 'full' && !entry.top) {
    const rename = element('button', '', 'Rename');
    rename.type = 'button';
    rename.addEventListener('click', () => renameItem(entry.root, entry.path, entry.name));
    const remove = element('button', 'danger', 'Delete');
    remove.type = 'button';
    remove.addEventListener('click', () => deleteItem(entry.root, entry.path, entry.name));
    actions.append(rename, remove);
  }
  tr.append(actions);
  return tr;
}

/** A button that saves a folder as a zip. */
function zipButton(entry, className) {
  const button = element('button', className);
  button.type = 'button';
  button.setAttribute('aria-label', 'Download ' + entry.name + ' as a zip');
  button.title = 'Download as .zip';
  button.innerHTML = DOWNLOAD_ICON;
  button.addEventListener('click', (event) => {
    event.stopPropagation();
    saveZip(entry.root, entry.path, entry.name);
  });
  return button;
}

function tile(entry) {
  const li = element('li', 'tile');
  let link;
  if (entry.dir || entry.kind) {
    link = element('button', 'tile-link');
    link.type = 'button';
    link.addEventListener('click', () => activate(entry));
  } else {
    link = saveLink(entry, 'tile-link');
  }
  if (entry.kind) {
    link.append(thumbImage(entry, 'face'));
  } else {
    const face = element('span', 'face ' + (entry.dir ? 'folder' : 'file'));
    if (!entry.dir) {
      const dot = entry.name.lastIndexOf('.');
      face.append(element('span', 'ext', dot > 0 ? entry.name.slice(dot + 1, dot + 6).toUpperCase() : ''));
    }
    link.append(face);
  }
  const caption = element('span', 'caption', entry.name);
  caption.title = entry.name;
  link.append(caption);
  if (entry.where !== undefined) link.append(element('span', 'where', entry.where));
  li.append(link);
  if (entry.dir && !e2e) li.append(zipButton(entry, 'tile-action'));
  return li;
}

/** Thumbnails suit folders that are mostly pictures and videos. */
function preferredView() {
  let chosen = null;
  try { chosen = localStorage.getItem('feather-view'); } catch (_) { /* private mode */ }
  if (chosen === 'list' || chosen === 'grid') return chosen;
  const files = state.entries.filter((entry) => !entry.dir);
  const media = files.filter((entry) => entry.kind).length;
  return media > 0 && media * 2 >= files.length ? 'grid' : 'list';
}

function render() {
  const view = preferredView();
  const entries = state.entries;
  $('as-list').setAttribute('aria-pressed', String(view === 'list'));
  $('as-grid').setAttribute('aria-pressed', String(view === 'grid'));
  $('list').hidden = view !== 'list' || !entries.length;
  $('grid').hidden = view !== 'grid' || !entries.length;
  if (view === 'list') {
    $('items').replaceChildren(...entries.map(row));
    $('grid').replaceChildren();
  } else {
    $('grid').replaceChildren(...entries.map(tile));
    $('items').replaceChildren();
  }
}

function chooseView(view) {
  try { localStorage.setItem('feather-view', view); } catch (_) { /* private mode */ }
  render();
}

function show(entries, truncated, emptyText) {
  state.entries = entries;
  render();
  $('empty').hidden = entries.length > 0;
  $('empty').textContent = emptyText;
  if (truncated) status('Only the first part is shown. Search to find something specific.');
}

async function open(root, path) {
  state.root = root;
  state.path = path;
  $('tools').hidden = !(state.info.mode === 'full' && root !== null);
  $('zip').hidden = e2e || root === null;
  trail();
  status('');
  if (root === null) {
    show(state.info.roots.map((item) => entryFor({ name: item.name, dir: true, size: 0, modified: 0 }, item.id, '', undefined, true)),
      false, 'Nothing is shared yet.');
    return;
  }
  try {
    const data = await call('/api/list', { root, path });
    show(data.items.map((item) => entryFor(item, root, join(path, item.name))), data.truncated, 'This folder is empty.');
  } catch (error) {
    show([], false, '');
    status(error.message, true);
  }
}

async function search(text) {
  state.query = text;
  // Uploads go to a folder, not into search results.
  $('tools').hidden = true;
  $('zip').hidden = true;
  trail();
  status('Searching…');
  try {
    const data = await call('/api/search', { q: text });
    status('');
    show(
      data.items.map((item) => {
        const folder = item.path.includes('/') ? item.path.slice(0, item.path.lastIndexOf('/')) : '';
        const where = rootName(item.root) + (folder ? '/' + folder : '');
        return entryFor(item, item.root, item.path, where);
      }),
      data.truncated,
      'Nothing shared matches “' + text + '”.',
    );
  } catch (error) {
    show([], false, '');
    status(error.message, true);
  }
}

// ----- Uploads ------------------------------------------------------------------

/**
 * Asks what to do with a name that is already there: Replace, Keep both,
 * or Skip, and whether to do the same for the rest of this upload.
 */
function askConflict(name, isFolder, more) {
  return new Promise((resolve) => {
    const box = $('conflict');
    $('conflict-text').textContent = '“' + name + '” is already in this folder.';
    $('conflict-all').checked = false;
    $('conflict-all-row').hidden = !more;
    $('conflict-replace').hidden = isFolder;
    const done = (choice) => {
      box.close();
      resolve({ choice, all: more && $('conflict-all').checked });
    };
    $('conflict-replace').onclick = () => done('replace');
    $('conflict-keep').onclick = () => done('keep');
    $('conflict-skip').onclick = () => done('skip');
    box.oncancel = (event) => { event.preventDefault(); done('skip'); };
    box.showModal();
    $('conflict-keep').focus();
  });
}

/**
 * Uploads files into the open folder. Each item is `{ file, sub }`, where
 * `sub` is the folder inside it the file came from (for whole folders),
 * which the server makes as needed.
 */
async function upload(items) {
  if (!items.length || state.root === null || state.info.mode !== 'full') return;
  const root = state.root;
  const path = state.path;
  let always = null;
  let sent = 0;
  let skipped = 0;
  for (const [index, item] of items.entries()) {
    const label = items.length > 1 ? (index + 1) + ' of ' + items.length + ': ' : '';
    status('Uploading ' + label + item.file.name + '…');
    const params = { root, path, name: item.file.name, sub: item.sub || '', conflict: always && always !== 'skip' ? always : 'ask' };
    try {
      try {
        await send(item.file, params);
      } catch (error) {
        if (!(error.status === 409 && error.data && error.data.conflict)) throw error;
        let choice = always;
        if (!choice) {
          const answer = await askConflict(item.file.name, error.data.dir, index + 1 < items.length);
          choice = answer.choice;
          if (answer.all) always = choice;
        }
        if (choice === 'skip') { skipped += 1; continue; }
        params.conflict = choice;
        await send(item.file, params);
      }
      sent += 1;
    } catch (error) {
      status(item.file.name + ': ' + error.message, true);
      if (state.root === root && state.path === path) refresh();
      return;
    }
  }
  if (state.root === root && state.path === path && !state.query) await refresh();
  const what = sent === 1 ? 'Uploaded 1 file.' : 'Uploaded ' + sent + ' files.';
  status(skipped ? what + ' Skipped ' + skipped + '.' : what);
}

/** Files chosen in a picker; a folder picker gives each its folder too. */
function picked(input) {
  const items = Array.from(input.files || []).map((file) => {
    const relative = file.webkitRelativePath || '';
    const sub = relative.includes('/') ? relative.slice(0, relative.lastIndexOf('/')) : '';
    return { file, sub };
  });
  input.value = '';
  return items;
}

/** Everything in a dropped folder, with the folders it was in. */
function readEntry(entry, sub, out) {
  return new Promise((resolve) => {
    if (entry.isFile) {
      entry.file((file) => { out.push({ file, sub }); resolve(); }, () => resolve());
    } else if (entry.isDirectory) {
      const reader = entry.createReader();
      const inner = sub ? sub + '/' + entry.name : entry.name;
      const batch = () => reader.readEntries(async (entries) => {
        if (!entries.length) { resolve(); return; }
        for (const child of entries) await readEntry(child, inner, out);
        batch();
      }, () => resolve());
      batch();
    } else {
      resolve();
    }
  });
}

async function dropped(transfer) {
  const out = [];
  const entries = Array.from(transfer.items || [])
    .map((item) => (item.webkitGetAsEntry ? item.webkitGetAsEntry() : null))
    .filter(Boolean);
  if (entries.length) {
    for (const entry of entries) await readEntry(entry, '', out);
  } else {
    for (const file of Array.from(transfer.files || [])) out.push({ file, sub: '' });
  }
  return out;
}

/** Desktop: drop files or folders anywhere on the page to upload them. */
function wireDrop() {
  let depth = 0;
  const can = (event) => state.info.mode === 'full' && state.root !== null && !state.query
    && event.dataTransfer && Array.from(event.dataTransfer.types || []).includes('Files');
  const show = (on) => $('drop').classList.toggle('show', on);
  document.addEventListener('dragenter', (event) => {
    if (!can(event)) return;
    event.preventDefault();
    depth += 1;
    show(true);
  });
  document.addEventListener('dragover', (event) => {
    if (!can(event)) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = 'copy';
  });
  document.addEventListener('dragleave', () => {
    depth = Math.max(0, depth - 1);
    if (!depth) show(false);
  });
  document.addEventListener('drop', async (event) => {
    depth = 0;
    show(false);
    if (!can(event)) return;
    event.preventDefault();
    upload(await dropped(event.dataTransfer));
  });
}

async function makeFolder() {
  const name = prompt('Name of the new folder');
  if (!name) return;
  try {
    await call('/api/mkdir', { root: state.root, path: state.path, name }, 'POST');
    open(state.root, state.path);
  } catch (error) {
    status(error.message, true);
  }
}

async function renameItem(root, path, current) {
  const to = prompt('New name', current);
  if (!to || to === current) return;
  try {
    await call('/api/rename', { root, path, to }, 'POST');
    refresh();
  } catch (error) {
    status(error.message, true);
  }
}

async function deleteItem(root, path, name) {
  if (!confirm('Move “' + name + '” to the Recycle Bin on the computer running Feather?')) return;
  try {
    await call('/api/delete', { root, path }, 'POST');
    status('Moved ' + name + ' to the Recycle Bin.');
    refresh();
  } catch (error) {
    status(error.message, true);
  }
}

function refresh() {
  return state.query ? search(state.query) : open(state.root, state.path);
}

/** Through a tunnel, points people on the same Wi-Fi to the fast address. */
function offerLocal() {
  const local = state.info.local;
  if (!local || remembered('feather-local-hidden')) return;
  let host = '';
  try { host = new URL(local).host; } catch (_) { return; }
  if (host === location.host) return;
  const box = $('local');
  const text = element('span', 'local-text', 'On the same Wi-Fi as the computer? Videos play much faster at ');
  const link = element('a', '', host);
  link.href = local + location.pathname;
  text.append(link, element('span', '', '. The first time, your browser warns about Feather’s certificate: choose Advanced, then continue.'));
  const close = element('button', 'quiet local-close', '×');
  close.type = 'button';
  close.setAttribute('aria-label', 'Hide');
  close.addEventListener('click', () => { box.hidden = true; remember('feather-local-hidden', true); });
  box.replaceChildren(text, close);
  box.hidden = false;
}

/**
 * The installed app keeps its page files, and remembers every address of
 * this Feather, so when this one stops answering (a tunnel's new address,
 * or leaving the Wi-Fi) its reconnect page can look for the others.
 */
function rememberAddresses(info) {
  const KEY = 'feather-addresses';
  try {
    const seen = JSON.parse(localStorage.getItem(KEY) || '[]').filter((item) => item && typeof item.url === 'string');
    const fresh = [location.origin, info.local, ...(info.addresses || [])]
      .filter(Boolean)
      .map((url) => { try { return new URL(url).origin; } catch (_) { return null; } })
      .filter((url) => url && url !== 'null');
    const now = Date.now();
    const merged = [...new Set(fresh)].map((url) => ({ url, seen: now }));
    for (const item of seen) if (!merged.some((known) => known.url === item.url)) merged.push(item);
    localStorage.setItem(KEY, JSON.stringify(merged.slice(0, 12)));
  } catch (_) { /* private mode */ }
  if ('serviceWorker' in navigator && window.isSecureContext) {
    navigator.serviceWorker.register('/sw.js').catch(() => { /* e.g. a certificate the browser doesn't trust */ });
  }
}

async function start() {
  try {
    state.info = await call('/api/info');
  } catch (error) {
    status(error.message, true);
    return;
  }
  rememberAddresses(state.info);
  $('user').textContent = state.info.user;
  // Without a password there is nothing to sign out of.
  $('logout').hidden = !state.info.user;
  const searchable = state.info.mode === 'search' || state.info.mode === 'full';
  $('search').hidden = !searchable;
  $('search').addEventListener('submit', (event) => {
    event.preventDefault();
    const text = $('query').value.trim();
    if (text) search(text); else { state.query = ''; open(state.root, state.path); }
  });
  $('logout').addEventListener('click', async () => {
    try { await call('/logout', {}, 'POST'); } catch (_) { /* signed out anyway */ }
    sessionStorage.removeItem('feather-key');
    location.href = '/login';
  });
  for (const id of ['files', 'folder', 'camera']) {
    $(id).addEventListener('change', (event) => upload(picked(event.target)));
  }
  // Folder pickers aren't on every phone.
  if (!('webkitdirectory' in document.createElement('input'))) $('folder-label').hidden = true;
  wireDrop();
  $('mkdir').addEventListener('click', makeFolder);
  $('zip').addEventListener('click', () => saveZip(state.root, state.path, folderLabel(state.root, state.path)));
  offerLocal();
  $('as-list').addEventListener('click', () => chooseView('list'));
  $('as-grid').addEventListener('click', () => chooseView('grid'));
  wireViewer();
  if (state.info.roots.length === 1) open(state.info.roots[0].id, ''); else open(null, '');
}

start();
