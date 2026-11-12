'use strict';

// Feather's shared-files page. Everything shown comes from the JSON API and
// goes into the page as text, never as HTML.

const state = {
  info: null, root: null, path: '', query: '', entries: [],
  sort: { key: 'name', dir: 1 }, sizes: new Map(), selecting: false, selected: new Map(), clip: null,
};
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
  back: '<svg viewBox="0 0 20 20" aria-hidden="true"><path d="M4.2 9.5a6 6 0 1 1 1.9 4.9M4 4.5v4.8h4.8"/><text class="fill" x="11.4" y="13" font-size="6.2" font-weight="700" text-anchor="middle">10</text></svg>',
  forward: '<svg viewBox="0 0 20 20" aria-hidden="true"><path d="M15.8 9.5a6 6 0 1 0-1.9 4.9M16 4.5v4.8h-4.8"/><text class="fill" x="8.6" y="13" font-size="6.2" font-weight="700" text-anchor="middle">10</text></svg>',
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
 * a seek bar that shows what has loaded, 10 seconds back and forward,
 * repeat, rotate, and full screen that turns a phone sideways.
 *
 * The video and its controls sit in a frame that turns as a whole, so when
 * the video is turned the controls stay along its bottom edge as seen. The
 * frame, not the player, turns: browsers don't turn a full-screen element.
 */
function videoPlayer(source, entry) {
  const shell = element('div', 'player');
  const frame = element('div', 'pframe');
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
  const back = iconButton('back', '10 seconds back', 'back');
  const forward = iconButton('forward', '10 seconds forward', 'forward');
  const time = element('span', 'ptime', '0:00 / 0:00');
  const gap = element('span', 'pgap');
  const loop = iconButton('loop', 'Repeat', 'loop');
  const rotate = iconButton('rotate', 'Rotate', 'rotate');
  const sound = iconButton('sound', 'Mute', video.muted ? 'muted' : 'sound');
  const full = iconButton('full', 'Full screen', 'full');
  const row = element('div', 'prow');
  row.append(play, back, forward, time, gap, loop, rotate, sound, full);
  controls.append(seek, row);
  const note = element('div', 'pnote');
  frame.append(video, big, skip, controls, note);
  shell.append(frame);

  // `turn` is a quarter turn or more; `live` is what two fingers add
  // until they let go.
  let turn = 0;
  let live = 0;
  let idleTimer = 0;
  let seeking = false;

  const fit = () => {
    const sideways = turn % 180 !== 0;
    frame.style.width = (sideways ? shell.clientHeight : shell.clientWidth) + 'px';
    frame.style.height = (sideways ? shell.clientWidth : shell.clientHeight) + 'px';
    frame.style.transform = 'translate(-50%, -50%) rotate(' + (turn + live) + 'deg)';
  };
  const resized = 'ResizeObserver' in window ? new ResizeObserver(fit) : null;
  if (resized) resized.observe(shell);
  // A short word in the middle, so a tap on a small button is confirmed.
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
  const skipBy = (seconds) => {
    if (!Number.isFinite(video.duration)) return;
    video.currentTime = Math.min(video.duration, Math.max(0, video.currentTime + seconds));
    skip.textContent = seconds < 0 ? '− 10 s' : '+ 10 s';
    skip.className = 'skip show ' + (seconds < 0 ? 'left' : 'right');
    clearTimeout(skip.timer);
    skip.timer = setTimeout(() => { skip.className = 'skip'; }, 500);
    wake();
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
  back.addEventListener('click', () => skipBy(-10));
  forward.addEventListener('click', () => skipBy(10));
  loop.addEventListener('click', () => {
    video.loop = !video.loop;
    remember('feather-loop', video.loop);
    say(video.loop ? 'Repeat on' : 'Repeat off');
    paint();
    wake();
  });
  sound.addEventListener('click', () => { video.muted = !video.muted; remember('feather-muted', video.muted); paint(); wake(); });
  rotate.addEventListener('click', () => {
    turn = (turn + 90) % 360;
    animateOnce(frame);
    fit();
    say('Rotated ' + turn + '°');
    wake();
  });
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

  // A click plays and pauses; a finger tap shows or hides the controls.
  // Taps that were part of a pinch, a pan, or a swipe don't count.
  video.addEventListener('pointerup', (event) => {
    if (viewer.swiped || gesture.moved) return;
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
  shell.skipBy = skipBy;
  shell.frame = frame;
  shell.zoomed = video;
  /** The whole turn as seen, for the viewer's gestures. */
  shell.turnBase = () => turn;
  shell.turnLive = (degrees) => { live = degrees; fit(); };
  /** Two fingers let go: the turn settles on the nearest quarter. */
  shell.settle = (degrees) => {
    turn = (((Math.round((turn + degrees) / 90) * 90) % 360) + 360) % 360;
    live = 0;
    animateOnce(frame);
    fit();
  };
  shell.upright = () => { turn = 0; live = 0; fit(); };
  requestAnimationFrame(fit);
  paint();
  wake();
  return { shell, video };
}

/** Lets the next change of `node`'s transform glide instead of jump. */
function animateOnce(node) {
  if (!node) return;
  node.classList.add('glide');
  clearTimeout(node.glideTimer);
  node.glideTimer = setTimeout(() => node.classList.remove('glide'), 280);
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
  resetView(false);
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
    shell.frame.append(media.loadingNote);
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
  showAt(Math.max(0, at));
  $('viewer-close').focus({ preventScroll: true });
  syncHistory();
}

function closeViewer() {
  if ($('viewer').hidden) return;
  stopMedia();
  $('viewer-stage').replaceChildren();
  $('viewer').hidden = true;
  document.body.classList.remove('viewing');
  viewer.at = -1;
  syncHistory();
}

// ----- Zooming, turning, and panning in the viewer -----------------------------
//
// Everything is one transform, in screen pixels from the middle of the
// stage: `x`, `y` move, `s` scales, `a` turns (degrees). A picture takes it
// whole; a video's frame takes the turn (with its controls) and the video
// inside it the move and scale. Pointer events drive it, so fingers, pens,
// and mice behave the same, and frames are drawn at most once per paint.

const gesture = { s: 1, a: 0, x: 0, y: 0, pointers: new Map(), pinch: null, pan: null, swipe: null, moved: false, frame: 0, lastTap: null };
const MIN_SCALE = 0.25;
const MAX_SCALE = 10;

function atFit() {
  return Math.abs(gesture.s - 1) < 0.02 && Math.abs(gesture.a) < 0.5 && Math.abs(gesture.x) < 2 && Math.abs(gesture.y) < 2;
}

/** The turn the picture already has before the gesture's own. */
function baseTurn() {
  return viewer.shell && viewer.shell.turnBase ? viewer.shell.turnBase() : 0;
}

function paintView() {
  gesture.frame = 0;
  const shell = viewer.shell;
  if (!shell) return;
  if (shell.zoomed) {
    // A video: turn the frame, then move and scale inside the turned frame.
    shell.turnLive(gesture.a);
    const turn = ((baseTurn() + gesture.a) * Math.PI) / 180;
    const lx = gesture.x * Math.cos(turn) + gesture.y * Math.sin(turn);
    const ly = -gesture.x * Math.sin(turn) + gesture.y * Math.cos(turn);
    shell.zoomed.style.transform = 'translate(-50%, -50%) translate(' + lx + 'px, ' + ly + 'px) scale(' + gesture.s + ')';
  } else {
    shell.style.transform = 'translate(' + gesture.x + 'px, ' + gesture.y + 'px) rotate(' + gesture.a + 'deg) scale(' + gesture.s + ')';
  }
  $('viewer').classList.toggle('zoomed', !atFit());
}

function drawView() {
  if (!gesture.frame) gesture.frame = requestAnimationFrame(paintView);
}

/** Back to fit, upright, and centered; `animated` glides there. */
function resetView(animated = true) {
  const shell = viewer.shell;
  if (animated && shell) {
    animateOnce(shell.zoomed || shell);
    if (shell.frame) animateOnce(shell.frame);
  }
  gesture.s = 1;
  gesture.a = 0;
  gesture.x = 0;
  gesture.y = 0;
  if (shell && shell.upright) shell.upright();
  if (gesture.frame) cancelAnimationFrame(gesture.frame);
  gesture.frame = 0;
  if (shell) paintView();
  $('viewer').classList.remove('zoomed');
}

/** Scales by `factor` around the screen point (px, py) from the stage's middle. */
function zoomAround(px, py, factor) {
  const s = Math.min(MAX_SCALE, Math.max(MIN_SCALE, gesture.s * factor));
  const ratio = s / gesture.s;
  gesture.x = px - (px - gesture.x) * ratio;
  gesture.y = py - (py - gesture.y) * ratio;
  gesture.s = s;
  drawView();
}

function stageCenter() {
  const box = $('viewer-stage').getBoundingClientRect();
  return { x: box.left + box.width / 2, y: box.top + box.height / 2 };
}

function wireGestures() {
  const stage = $('viewer-stage');
  const fromCenter = (event) => {
    const center = stageCenter();
    return { x: event.clientX - center.x, y: event.clientY - center.y };
  };
  const twoFingers = () => {
    const [first, second] = Array.from(gesture.pointers.values());
    const center = stageCenter();
    return {
      x: (first.x + second.x) / 2 - center.x,
      y: (first.y + second.y) / 2 - center.y,
      distance: Math.max(1, Math.hypot(second.x - first.x, second.y - first.y)),
      angle: (Math.atan2(second.y - first.y, second.x - first.x) * 180) / Math.PI,
    };
  };
  const startPinch = () => {
    const now = twoFingers();
    // The point of the picture under the fingers, in its own unturned,
    // unscaled frame; it stays under them.
    const turn = ((baseTurn() + gesture.a) * Math.PI) / 180;
    const dx = (now.x - gesture.x) / gesture.s;
    const dy = (now.y - gesture.y) / gesture.s;
    gesture.pinch = {
      distance: now.distance,
      angle: now.angle,
      s: gesture.s,
      a: gesture.a,
      cx: dx * Math.cos(turn) + dy * Math.sin(turn),
      cy: -dx * Math.sin(turn) + dy * Math.cos(turn),
    };
    gesture.pan = null;
    if (gesture.swipe) endSwipe(0);
  };
  const endSwipe = (dx) => {
    const swipe = gesture.swipe;
    gesture.swipe = null;
    const shell = viewer.shell;
    if (!swipe || !swipe.moved || !shell) return;
    viewer.swiped = true;
    setTimeout(() => { viewer.swiped = false; }, 400);
    shell.style.transition = '';
    if (Math.abs(dx) > Math.min(90, window.innerWidth / 5) && viewer.list.length > 1) {
      step(dx < 0 ? 1 : -1);
    } else {
      shell.style.translate = '';
    }
  };

  stage.addEventListener('pointerdown', (event) => {
    if (event.pointerType === 'mouse' && event.button !== 0) return;
    // The player's own buttons and seek bar keep their pointers.
    if (event.target.closest && event.target.closest('.controls, .pbtn')) return;
    if (!viewer.shell) return;
    gesture.pointers.set(event.pointerId, { x: event.clientX, y: event.clientY });
    if (gesture.pointers.size === 1) gesture.moved = false;
    if (gesture.pointers.size === 2) {
      // Both fingers stay with the stage until they let go.
      for (const id of gesture.pointers.keys()) {
        try { stage.setPointerCapture(id); } catch (_) { /* gone already */ }
      }
      startPinch();
    } else if (gesture.pointers.size === 1) {
      gesture.start = { x: event.clientX, y: event.clientY, at: Date.now() };
      if (atFit()) {
        gesture.swipe = { x: event.clientX, y: event.clientY, moved: false };
      } else {
        gesture.pan = { x: event.clientX, y: event.clientY };
      }
    }
  });
  stage.addEventListener('pointermove', (event) => {
    const pointer = gesture.pointers.get(event.pointerId);
    if (!pointer) return;
    pointer.x = event.clientX;
    pointer.y = event.clientY;
    if (!gesture.moved && gesture.start && Math.hypot(event.clientX - gesture.start.x, event.clientY - gesture.start.y) > 8) {
      // Only now is it a drag: a tap still reaches the video's own handler.
      gesture.moved = true;
      try { stage.setPointerCapture(event.pointerId); } catch (_) { /* gone already */ }
    }
    if (gesture.pinch && gesture.pointers.size >= 2) {
      const now = twoFingers();
      const pinch = gesture.pinch;
      gesture.s = Math.min(MAX_SCALE, Math.max(MIN_SCALE, pinch.s * (now.distance / pinch.distance)));
      let turned = now.angle - pinch.angle;
      if (turned > 180) turned -= 360;
      if (turned < -180) turned += 360;
      gesture.a = pinch.a + turned;
      const turn = ((baseTurn() + gesture.a) * Math.PI) / 180;
      gesture.x = now.x - gesture.s * (pinch.cx * Math.cos(turn) - pinch.cy * Math.sin(turn));
      gesture.y = now.y - gesture.s * (pinch.cx * Math.sin(turn) + pinch.cy * Math.cos(turn));
      drawView();
      return;
    }
    if (gesture.pan) {
      gesture.x += event.clientX - gesture.pan.x;
      gesture.y += event.clientY - gesture.pan.y;
      gesture.pan = { x: event.clientX, y: event.clientY };
      drawView();
      return;
    }
    const swipe = gesture.swipe;
    if (swipe && viewer.shell) {
      const dx = event.clientX - swipe.x;
      const dy = event.clientY - swipe.y;
      if (!swipe.moved && Math.abs(dx) > 10 && Math.abs(dx) > Math.abs(dy)) swipe.moved = true;
      if (swipe.moved) {
        viewer.shell.style.translate = dx + 'px 0';
        viewer.shell.style.transition = 'none';
      }
    }
  });
  const up = (event) => {
    if (!gesture.pointers.has(event.pointerId)) return;
    gesture.pointers.delete(event.pointerId);
    if (gesture.pinch && gesture.pointers.size < 2) {
      gesture.pinch = null;
      viewer.swiped = true;
      setTimeout(() => { viewer.swiped = false; }, 400);
      // A video's turn settles on the nearest quarter, controls and all.
      if (viewer.shell && viewer.shell.settle) {
        viewer.shell.settle(gesture.a);
        gesture.a = 0;
        drawView();
      }
      if (gesture.pointers.size === 1) {
        const [left] = Array.from(gesture.pointers.values());
        gesture.pan = { x: left.x, y: left.y };
      }
      gesture.lastTap = null;
      return;
    }
    if (gesture.swipe) endSwipe(event.clientX - gesture.swipe.x);
    if (gesture.pan && gesture.moved) {
      viewer.swiped = true;
      setTimeout(() => { viewer.swiped = false; }, 400);
    }
    gesture.pan = null;
    // Two quick taps in one place: back to fit. (Mice double-click.)
    if (event.type === 'pointerup' && event.pointerType !== 'mouse' && !gesture.moved && gesture.pointers.size === 0) {
      const now = Date.now();
      const last = gesture.lastTap;
      if (last && now - last.at < 320 && Math.hypot(event.clientX - last.x, event.clientY - last.y) < 40) {
        gesture.lastTap = null;
        resetView(true);
        if (viewer.shell && viewer.shell.wake) viewer.shell.wake();
      } else {
        gesture.lastTap = { at: now, x: event.clientX, y: event.clientY };
      }
    }
  };
  stage.addEventListener('pointerup', up);
  stage.addEventListener('pointercancel', up);
  stage.addEventListener('dblclick', (event) => {
    if (event.target.closest && event.target.closest('.controls, .pbtn')) return;
    resetView(true);
  });
  stage.addEventListener('wheel', (event) => {
    if (!viewer.shell) return;
    if (event.target.closest && event.target.closest('.controls')) return;
    event.preventDefault();
    // A sideways scroll on a touchpad flips once per gesture, at fit.
    if (!event.ctrlKey && atFit() && Math.abs(event.deltaX) >= 30 && Math.abs(event.deltaX) > Math.abs(event.deltaY)) {
      const now = Date.now();
      if (now < viewer.wheelUntil) { viewer.wheelUntil = now + 250; return; }
      viewer.wheelUntil = now + 450;
      step(event.deltaX > 0 ? 1 : -1);
      return;
    }
    const lines = event.deltaMode === 1 ? 32 : event.deltaMode === 2 ? 400 : 1;
    const point = fromCenter(event);
    zoomAround(point.x, point.y, Math.exp((-event.deltaY * lines) * (event.ctrlKey ? 0.01 : 0.0018)));
  }, { passive: false });
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
  document.addEventListener('keydown', (event) => {
    if ($('viewer').hidden || menu.layer || openDialog()) return;
    if (event.key === 'Escape') { event.preventDefault(); closeViewer(); return; }
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
    if (event.key === '+' || event.key === '=') { event.preventDefault(); zoomAround(0, 0, 1.25); }
    if (event.key === '-') { event.preventDefault(); zoomAround(0, 0, 0.8); }
    if (event.key === '0') { event.preventDefault(); resetView(true); }
  });
  const stage = $('viewer-stage');
  // Clicking the dark around a picture closes, like most viewers.
  stage.addEventListener('click', (event) => {
    // The click that ends a swipe, pinch, or pan isn't a click on the dark.
    if (viewer.swiped || gesture.moved) { viewer.swiped = false; return; }
    if (event.target === stage && atFit()) closeViewer();
  });
  wireGestures();
}

// ----- Folder contents ----------------------------------------------------------

const MENU_ICONS = {
  open: '<path d="M7 4H4.5A1.5 1.5 0 0 0 3 5.5v10A1.5 1.5 0 0 0 4.5 17h10a1.5 1.5 0 0 0 1.5-1.5V13M11 3h6v6M17 3l-8 8"/>',
  folder: '<path d="M2.5 6V15a1.5 1.5 0 0 0 1.5 1.5h12a1.5 1.5 0 0 0 1.5-1.5V7.5A1.5 1.5 0 0 0 16 6h-6.5L8 4H4A1.5 1.5 0 0 0 2.5 5.5Z"/>',
  download: '<path d="M10 3v10m-4-4 4 4 4-4M4 16h12"/>',
  move: '<path d="M2.5 6V15a1.5 1.5 0 0 0 1.5 1.5h12a1.5 1.5 0 0 0 1.5-1.5V7.5A1.5 1.5 0 0 0 16 6h-6.5L8 4H4A1.5 1.5 0 0 0 2.5 5.5ZM7 11.2h6m-2.2-2.4 2.4 2.4-2.4 2.4"/>',
  cut: '<circle cx="5.5" cy="14.5" r="2.5"/><circle cx="14.5" cy="14.5" r="2.5"/><path d="M7.4 12.8 15 3M12.6 12.8 5 3"/>',
  copy: '<rect x="6.5" y="6.5" width="10" height="10" rx="1.6"/><path d="M13.5 6.5V5A1.5 1.5 0 0 0 12 3.5H5A1.5 1.5 0 0 0 3.5 5v7A1.5 1.5 0 0 0 5 13.5h1.5"/>',
  paste: '<path d="M7 4.5H5.5A1.5 1.5 0 0 0 4 6v10a1.5 1.5 0 0 0 1.5 1.5h9A1.5 1.5 0 0 0 16 16V6a1.5 1.5 0 0 0-1.5-1.5H13"/><rect x="7" y="2.5" width="6" height="3.5" rx="1"/>',
  rename: '<path d="M12.5 4.5 15.5 7.5 7 16H4v-3Zm-2 2 3 3"/>',
  newfolder: '<path d="M2.5 6V15a1.5 1.5 0 0 0 1.5 1.5h12a1.5 1.5 0 0 0 1.5-1.5V7.5A1.5 1.5 0 0 0 16 6h-6.5L8 4H4A1.5 1.5 0 0 0 2.5 5.5ZM10 8.5v5m-2.5-2.5h5"/>',
  select: '<rect x="3" y="3" width="14" height="14" rx="3"/><path d="m6.5 10 2.4 2.4 4.6-4.8"/>',
  delete: '<path d="M3.5 5.5h13M8 5.5V4a1 1 0 0 1 1-1h2a1 1 0 0 1 1 1v1.5M5.5 5.5l.8 10.6A1.5 1.5 0 0 0 7.8 17.5h4.4a1.5 1.5 0 0 0 1.5-1.4l.8-10.6M8.5 8.5v6M11.5 8.5v6"/>',
  more: '<circle class="fill" cx="4.5" cy="10" r="1.5"/><circle class="fill" cx="10" cy="10" r="1.5"/><circle class="fill" cx="15.5" cy="10" r="1.5"/>',
  upload: '<path d="M10 14V3.5m-4 4 4-4 4 4M4 16.5h12"/>',
  camera: '<path d="M3 7.5A1.5 1.5 0 0 1 4.5 6h2l1.5-2h4l1.5 2h2A1.5 1.5 0 0 1 17 7.5v7a1.5 1.5 0 0 1-1.5 1.5h-11A1.5 1.5 0 0 1 3 14.5Z"/><circle cx="10" cy="10.8" r="3"/>',
  sort: '<path d="M6 3.5v13m-3-3 3 3 3-3M14 16.5v-13m-3 3 3-3 3 3"/>',
  check: '<path d="m4.5 10.5 3.5 3.5 7.5-8"/>',
  close: '<path d="m5 5 10 10M15 5 5 15"/>',
  up: '<path d="m5 12 5-5 5 5"/>',
  down: '<path d="m5 8 5 5 5-5"/>',
  all: '<rect x="2.5" y="2.5" width="6" height="6" rx="1.5"/><rect x="11.5" y="2.5" width="6" height="6" rx="1.5"/><rect x="2.5" y="11.5" width="6" height="6" rx="1.5"/><rect x="11.5" y="11.5" width="6" height="6" rx="1.5"/>',
};

function svgIcon(name) {
  return '<svg viewBox="0 0 20 20" aria-hidden="true">' + (MENU_ICONS[name] || '') + '</svg>';
}

function entryKey(entry) {
  return entry.root + '/' + entry.path;
}

function entryFor(item, root, path, where, top = false) {
  return { ...item, root, path, where, top, kind: item.dir ? null : mediaKind(item.name) };
}

const full = () => state.info.mode === 'full';
const inFolder = () => state.root !== null && !state.query;

/** Tap: a folder opens, a picture or video shows, anything else downloads. */
function activate(entry) {
  if (entry.dir) {
    state.query = '';
    $('query').value = '';
    open(entry.root, entry.path);
  } else if (entry.kind) {
    openViewer(entry);
  } else {
    saveEntry(entry);
  }
}

function saveEntry(entry) {
  if (e2e) { save(entry.root, entry.path, entry.name); return; }
  const link = document.createElement('a');
  link.href = downloadLink(entry.root, entry.path);
  link.setAttribute('download', entry.name);
  document.body.append(link);
  link.click();
  link.remove();
}

/** A plain file's link saves it; end to end it is decrypted first. */
function saveLink(entry, className, text) {
  const link = element('a', className, text);
  link.href = downloadLink(entry.root, entry.path);
  link.setAttribute('download', entry.name);
  if (e2e) link.addEventListener('click', (event) => { event.preventDefault(); save(entry.root, entry.path, entry.name); });
  return link;
}

/** The folder's total and how many items are in it, or … while counting. */
function folderSize(entry) {
  const total = state.sizes.get(entryKey(entry));
  if (!total) return { size: state.query ? '' : '…', items: '' };
  const plus = total.partial ? '+' : '';
  const items = total.items === 1 ? '1 item' : total.items + plus + ' items';
  return { size: (total.size ? size(total.size, false) : '0 B') + plus, items };
}

/** The little ⋯ button that opens an item's menu. */
function moreButton(entry, className) {
  const button = element('button', className);
  button.type = 'button';
  button.setAttribute('aria-label', 'More for ' + entry.name);
  button.setAttribute('aria-haspopup', 'menu');
  button.title = 'More';
  button.innerHTML = svgIcon('more');
  button.addEventListener('click', (event) => {
    event.stopPropagation();
    event.preventDefault();
    if (state.selecting) { toggleSelect(entry); return; }
    openEntryMenu(entry, button, isTouch() && event.pointerType !== 'mouse' ? null : below(button));
  });
  return button;
}

function row(entry) {
  const tr = document.createElement('tr');
  tr.className = 'item';
  const name = element('td', 'name');
  let link;
  if (entry.dir || entry.kind) {
    link = element('button', 'entry');
    link.type = 'button';
    link.addEventListener('click', () => activate(entry));
  } else {
    link = saveLink(entry, 'entry');
  }
  link.append(element('span', 'tick'));
  link.append(entry.kind ? thumbImage(entry, 'mini') : element('span', entry.dir ? 'kind dir' : 'kind'));
  const text = element('span', 'label', entry.name);
  const meta = element('span', 'meta');
  if (entry.where !== undefined) meta.append(element('span', 'where', entry.where));
  if (entry.dir) meta.append(element('span', 'count', folderSize(entry).items));
  const block = element('span', 'text');
  block.append(text, meta);
  link.append(block);
  name.append(link);
  const sizeCell = element('td', 'size', entry.dir ? folderSize(entry).size : size(entry.size, false));
  tr.append(name, sizeCell, element('td', 'when', when(entry.modified)));
  const actions = element('td', 'actions');
  if (entry.kind) {
    const get = saveLink(entry, 'icon-button wide', '');
    get.setAttribute('aria-label', 'Download ' + entry.name);
    get.title = 'Download';
    get.innerHTML = DOWNLOAD_ICON;
    actions.append(get);
  } else if (entry.dir && !e2e) {
    actions.append(zipButton(entry, 'icon-button wide'));
  }
  if (full() && !entry.top) {
    // Phones have the menu (hold, or ⋯) instead.
    const rename = element('button', 'wide', 'Rename');
    rename.type = 'button';
    rename.addEventListener('click', () => renameItem(entry));
    const remove = element('button', 'danger wide', 'Delete');
    remove.type = 'button';
    remove.addEventListener('click', () => deleteItems([entry]));
    actions.append(rename, remove);
  }
  actions.append(moreButton(entry, 'icon-button more'));
  tr.append(actions);
  entry.nodes = { item: tr, size: sizeCell, count: meta.querySelector('.count') };
  wirePress(tr, entry);
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
  const li = element('li', 'tile item');
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
  link.append(element('span', 'tick'));
  const caption = element('span', 'caption', entry.name);
  caption.title = entry.name;
  link.append(caption);
  if (entry.where !== undefined) link.append(element('span', 'where', entry.where));
  let count = null;
  if (entry.dir) {
    count = element('span', 'where count', folderTileText(entry));
    link.append(count);
  }
  li.append(link);
  li.append(moreButton(entry, 'tile-action'));
  entry.nodes = { item: li, size: null, count };
  wirePress(li, entry);
  return li;
}

function folderTileText(entry) {
  const total = folderSize(entry);
  return total.items ? total.items + ' · ' + total.size : total.size;
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

// ----- Sorting --------------------------------------------------------------------

const SORTS = { name: 'Name', size: 'Size', modified: 'Date modified', type: 'Type' };
const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

function loadSort() {
  let saved = '';
  try { saved = localStorage.getItem('feather-sort') || ''; } catch (_) { /* private mode */ }
  const [key, way] = saved.split(':');
  return { key: SORTS[key] ? key : 'name', dir: way === 'desc' ? -1 : 1 };
}

function chooseSort(key, dir) {
  state.sort = { key, dir };
  try { localStorage.setItem('feather-sort', key + ':' + (dir < 0 ? 'desc' : 'asc')); } catch (_) { /* private mode */ }
  render();
}

function extension(entry) {
  if (entry.dir) return '';
  const dot = entry.name.lastIndexOf('.');
  return dot > 0 ? entry.name.slice(dot + 1).toLowerCase() : '';
}

/** Folders first, then by the chosen order; names settle ties. */
function sorted(entries) {
  const { key, dir } = state.sort;
  const bytes = (entry) => {
    if (!entry.dir) return entry.size;
    const total = state.sizes.get(entryKey(entry));
    return total ? total.size : -1;
  };
  const by = {
    name: () => 0,
    size: (a, b) => bytes(a) - bytes(b),
    modified: (a, b) => a.modified - b.modified,
    type: (a, b) => collator.compare(extension(a), extension(b)),
  }[key];
  return entries.slice().sort((a, b) => {
    if (a.dir !== b.dir) return a.dir ? -1 : 1;
    const first = by(a, b) || collator.compare(a.name, b.name);
    return first * dir;
  });
}

function openSortMenu(opener, at) {
  const items = Object.entries(SORTS).map(([key, label]) => ({
    label,
    icon: state.sort.key === key ? (state.sort.dir < 0 ? 'down' : 'up') : '',
    checked: state.sort.key === key,
    run: () => chooseSort(key, state.sort.key === key ? -state.sort.dir : (key === 'name' || key === 'type' ? 1 : -1)),
  }));
  items.push('-');
  items.push({ label: 'Ascending', icon: state.sort.dir > 0 ? 'check' : '', checked: state.sort.dir > 0, run: () => chooseSort(state.sort.key, 1) });
  items.push({ label: 'Descending', icon: state.sort.dir < 0 ? 'check' : '', checked: state.sort.dir < 0, run: () => chooseSort(state.sort.key, -1) });
  showMenu(items, { title: 'Sort by', opener, at });
}

// ----- Showing a folder ---------------------------------------------------------

function render() {
  const view = preferredView();
  const entries = sorted(state.entries);
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
  paintMarks();
  $('sort').title = 'Sort by ' + SORTS[state.sort.key].toLowerCase() + (state.sort.dir < 0 ? ', descending' : '');
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

/** The toolbar for the place shown: what can be done here. */
function paintTools() {
  const here = inFolder();
  $('tools').hidden = !here;
  for (const node of document.querySelectorAll('[data-full]')) node.hidden = !full();
  $('t-paste').hidden = !(full() && state.clip);
  if (state.clip) {
    const count = state.clip.items.length;
    $('t-paste-label').textContent = 'Paste' + (count > 1 ? ' ' + count : '');
    $('t-paste').title = 'Paste ' + (count === 1 ? state.clip.items[0].name : count + ' items') + ' here';
  }
  $('zip').hidden = e2e || state.root === null || !!state.query;
}

async function open(root, path) {
  state.root = root;
  state.path = path;
  state.entries = [];
  endSelect(true);
  sizesTicket += 1;
  paintTools();
  trail();
  status('');
  syncHistory();
  if (root === null) {
    show(state.info.roots.map((item) => entryFor({ name: item.name, dir: true, size: 0, modified: 0 }, item.id, '', undefined, true)),
      false, 'Nothing is shared yet.');
    loadSizes();
    return;
  }
  try {
    const data = await call('/api/list', { root, path });
    if (state.root !== root || state.path !== path) return;
    show(data.items.map((item) => entryFor(item, root, join(path, item.name))), data.truncated, 'This folder is empty.');
    loadSizes();
  } catch (error) {
    show([], false, '');
    status(error.message, true);
  }
}

/** The folder above, or the list of shared folders. */
function goUp() {
  if (state.root === null) return;
  if (state.path) {
    const parts = state.path.split('/').filter(Boolean);
    parts.pop();
    open(state.root, parts.join('/'));
  } else if (state.info.roots.length > 1) {
    open(null, '');
  }
}

async function search(text) {
  state.query = text;
  endSelect(true);
  sizesTicket += 1;
  paintTools();
  trail();
  status('Searching…');
  syncHistory();
  try {
    const data = await call('/api/search', { q: text });
    if (state.query !== text) return;
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

function clearSearch() {
  state.query = '';
  $('query').value = '';
  open(state.root, state.path);
}

function refresh() {
  return state.query ? search(state.query) : open(state.root, state.path);
}

// ----- Folder totals --------------------------------------------------------------

let sizesTicket = 0;
const pause = (ms) => new Promise((resolve) => { setTimeout(resolve, ms); });

/**
 * Asks for the folders' totals after the folder is shown, and again while
 * the computer is still adding them up; each arrives in place.
 */
async function loadSizes() {
  const ticket = ++sizesTicket;
  const root = state.root;
  const path = state.path;
  let wait = 350;
  for (let round = 0; round < 80; round += 1) {
    let waiting = 0;
    try {
      if (root === null) {
        for (const item of state.info.roots) {
          const data = await call('/api/sizes', { root: item.id, path: '', self: '1' });
          for (const found of data.sizes) state.sizes.set(item.id + '/', found);
          waiting += data.waiting;
        }
      } else {
        const data = await call('/api/sizes', { root, path });
        for (const found of data.sizes) state.sizes.set(root + '/' + join(path, found.name), found);
        waiting = data.waiting;
      }
    } catch (_) {
      return;
    }
    if (ticket !== sizesTicket) return;
    paintSizes();
    if (!waiting) return;
    await pause(wait);
    if (ticket !== sizesTicket) return;
    wait = Math.min(2500, wait * 1.4);
  }
}

function paintSizes() {
  if (state.sort.key === 'size') { render(); return; }
  for (const entry of state.entries) {
    if (!entry.dir || !entry.nodes) continue;
    const total = folderSize(entry);
    if (entry.nodes.size) entry.nodes.size.textContent = total.size;
    if (entry.nodes.count) {
      entry.nodes.count.textContent = entry.nodes.size ? total.items : folderTileText(entry);
    }
  }
}

// ----- Holding, right-clicking, and the menu --------------------------------------

const menu = { layer: null, opener: null };

function isTouch() {
  return matchMedia('(pointer: coarse)').matches;
}

function below(node) {
  const box = node.getBoundingClientRect();
  return { x: box.right, y: box.bottom + 4, alignRight: true };
}

/**
 * Hold (half a second, with a buzz), right-click, the menu key, or ⋯
 * opens an item's menu. A hold neither selects text nor opens the item.
 * While selecting, a tap or hold ticks the item instead.
 */
function wirePress(node, entry) {
  let timer = 0;
  let start = null;
  const cancel = () => { clearTimeout(timer); timer = 0; };
  node.addEventListener('pointerdown', (event) => {
    if (event.pointerType === 'mouse') return;
    start = { x: event.clientX, y: event.clientY };
    cancel();
    timer = setTimeout(() => {
      timer = 0;
      node.dataset.pressed = String(Date.now());
      try { if (navigator.vibrate) navigator.vibrate(12); } catch (_) { /* not allowed */ }
      if (state.selecting) toggleSelect(entry); else openEntryMenu(entry, node, null);
    }, 500);
  });
  node.addEventListener('pointermove', (event) => {
    if (timer && start && Math.hypot(event.clientX - start.x, event.clientY - start.y) > 10) cancel();
  });
  node.addEventListener('pointerup', cancel);
  node.addEventListener('pointercancel', cancel);
  node.addEventListener('contextmenu', (event) => {
    event.preventDefault();
    // Android also says "context menu" for a hold: one menu only.
    if (Date.now() - Number(node.dataset.pressed || 0) < 1200) return;
    const touch = event.pointerType === 'touch' || (timer !== 0);
    cancel();
    if (touch) {
      node.dataset.pressed = String(Date.now());
      try { if (navigator.vibrate) navigator.vibrate(12); } catch (_) { /* not allowed */ }
    }
    if (state.selecting && touch) { toggleSelect(entry); return; }
    openEntryMenu(entry, node, touch ? null : { x: event.clientX, y: event.clientY });
  });
  node.addEventListener('click', (event) => {
    if (Date.now() - Number(node.dataset.pressed || 0) < 900) {
      event.preventDefault();
      event.stopPropagation();
      return;
    }
    if (state.selecting && !(event.target.closest && event.target.closest('.more, .tile-action'))) {
      event.preventDefault();
      event.stopPropagation();
      toggleSelect(entry);
    }
  }, true);
  node.addEventListener('keydown', (event) => {
    if (event.key === 'ContextMenu' || (event.shiftKey && event.key === 'F10')) {
      event.preventDefault();
      const box = (event.target.getBoundingClientRect ? event.target : node).getBoundingClientRect();
      openEntryMenu(entry, event.target, isTouch() ? null : { x: box.left + 24, y: box.bottom });
    }
  });
}

function openEntryMenu(entry, opener, at) {
  showMenu(entryActions(entry), { title: entry.name, opener, at });
}

function entryActions(entry) {
  const items = [];
  const changeable = full() && !entry.top;
  items.push({ label: entry.dir ? 'Open' : entry.kind ? 'Open' : 'Download', icon: entry.dir ? 'folder' : entry.kind ? 'open' : 'download', run: () => activate(entry) });
  if (entry.kind) items.push({ label: 'Download', icon: 'download', run: () => saveEntry(entry) });
  if (entry.dir && !e2e) items.push({ label: 'Download as .zip', icon: 'download', run: () => saveZip(entry.root, entry.path, entry.name) });
  if (changeable) items.push({ label: 'Move to…', icon: 'move', strong: true, run: () => moveOrCopy([entry]) });
  if (full()) {
    items.push('-');
    if (changeable) items.push({ label: 'Cut', icon: 'cut', run: () => toClipboard([entry], 'cut') });
    items.push({ label: 'Copy', icon: 'copy', run: () => toClipboard([entry], 'copy') });
    if (entry.dir && state.clip) {
      items.push({ label: 'Paste into ' + entry.name, icon: 'paste', run: () => paste({ root: entry.root, path: entry.path, name: entry.name }) });
    }
    if (changeable) items.push({ label: 'Rename', icon: 'rename', run: () => renameItem(entry) });
    if (inFolder()) items.push({ label: 'New folder', icon: 'newfolder', run: makeFolder });
  }
  if (inFolder()) items.push({ label: 'Select', icon: 'select', run: () => startSelect(entry) });
  if (changeable) {
    items.push('-');
    items.push({ label: 'Delete', icon: 'delete', danger: true, run: () => deleteItems([entry]) });
  }
  return items;
}

/**
 * A menu of `items` ({ label, icon, run, danger, strong, checked } or '-').
 * With `at` (a point) it pops up there, kept on screen; without, it is a
 * sheet from the bottom, as phones have them, closed by a tap outside, a
 * swipe down, Back, or Escape. Arrow keys move through it.
 */
function showMenu(items, { title = '', opener = null, at = null } = {}) {
  closeMenu(true);
  const sheet = !at;
  const layer = element('div', 'menu-layer' + (sheet ? ' sheet' : ''));
  const box = element('div', 'menu');
  box.setAttribute('role', 'menu');
  if (title) box.setAttribute('aria-label', title);
  if (sheet) {
    const head = element('div', 'menu-head');
    head.append(element('span', 'menu-grip'));
    if (title) head.append(element('span', 'menu-title', title));
    box.append(head);
    wireSheetDrag(head, box);
  }
  const list = element('div', 'menu-items');
  for (const item of items) {
    if (item === '-') { list.append(element('div', 'menu-sep')); continue; }
    const button = element('button', 'menu-item' + (item.danger ? ' danger' : '') + (item.strong ? ' strong' : ''));
    button.type = 'button';
    button.setAttribute('role', item.checked === undefined ? 'menuitem' : 'menuitemradio');
    if (item.checked !== undefined) button.setAttribute('aria-checked', String(item.checked));
    button.innerHTML = svgIcon(item.icon);
    button.append(element('span', '', item.label));
    button.addEventListener('click', () => {
      closeMenu(true);
      item.run();
    });
    list.append(button);
  }
  box.append(list);
  layer.append(box);
  document.body.append(layer);
  menu.layer = layer;
  menu.opener = opener;
  layer.addEventListener('pointerdown', (event) => {
    if (event.target === layer) { event.preventDefault(); closeMenu(); }
  });
  layer.addEventListener('contextmenu', (event) => {
    event.preventDefault();
    if (event.target === layer) closeMenu();
  });
  layer.addEventListener('keydown', (event) => {
    const buttons = Array.from(list.querySelectorAll('.menu-item'));
    const at = buttons.indexOf(document.activeElement);
    if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); closeMenu(); }
    else if (event.key === 'ArrowDown') { event.preventDefault(); buttons[(at + 1) % buttons.length].focus(); }
    else if (event.key === 'ArrowUp') { event.preventDefault(); buttons[(at - 1 + buttons.length) % buttons.length].focus(); }
    else if (event.key === 'Home') { event.preventDefault(); buttons[0].focus(); }
    else if (event.key === 'End') { event.preventDefault(); buttons[buttons.length - 1].focus(); }
    else if (event.key === 'Tab') { event.preventDefault(); buttons[(at + (event.shiftKey ? -1 : 1) + buttons.length) % buttons.length].focus(); }
  });
  if (!sheet) {
    const width = box.offsetWidth;
    const height = box.offsetHeight;
    const room = 8;
    let left = at.alignRight ? at.x - width : at.x;
    left = Math.max(room, Math.min(left, window.innerWidth - width - room));
    let top = at.y;
    if (top + height > window.innerHeight - room) top = Math.max(room, at.y - height);
    box.style.left = left + 'px';
    box.style.top = top + 'px';
  }
  requestAnimationFrame(() => layer.classList.add('shown'));
  const first = list.querySelector('.menu-item');
  if (first) first.focus({ preventScroll: true });
  syncHistory();
}

function closeMenu(keepFocus = false) {
  if (!menu.layer) return;
  const layer = menu.layer;
  menu.layer = null;
  layer.remove();
  if (!keepFocus && menu.opener && menu.opener.isConnected) menu.opener.focus({ preventScroll: true });
  syncHistory();
}

/** Pulling a sheet down by its top closes it. */
function wireSheetDrag(head, box) {
  let drag = null;
  head.addEventListener('pointerdown', (event) => {
    drag = { y: event.clientY, at: Date.now(), id: event.pointerId };
    try { head.setPointerCapture(event.pointerId); } catch (_) { /* gone */ }
    box.style.transition = 'none';
  });
  head.addEventListener('pointermove', (event) => {
    if (!drag || event.pointerId !== drag.id) return;
    box.style.transform = 'translateY(' + Math.max(0, event.clientY - drag.y) + 'px)';
  });
  const end = (event) => {
    if (!drag || event.pointerId !== drag.id) return;
    const pulled = event.clientY - drag.y;
    const quick = pulled > 30 && Date.now() - drag.at < 250;
    drag = null;
    box.style.transition = '';
    if (pulled > 80 || quick) closeMenu(); else box.style.transform = '';
  };
  head.addEventListener('pointerup', end);
  head.addEventListener('pointercancel', end);
}

// ----- Selecting several ---------------------------------------------------------

function startSelect(first) {
  state.selecting = true;
  state.selected.clear();
  if (first) state.selected.set(entryKey(first), first);
  paintMarks();
  syncHistory();
}

function endSelect(quiet = false) {
  if (!state.selecting) return;
  state.selecting = false;
  state.selected.clear();
  paintMarks();
  if (!quiet) syncHistory();
}

function toggleSelect(entry) {
  const key = entryKey(entry);
  if (state.selected.has(key)) state.selected.delete(key); else state.selected.set(key, entry);
  paintMarks();
}

function selectAll() {
  const everything = state.entries.every((entry) => state.selected.has(entryKey(entry)));
  state.selected.clear();
  if (!everything) for (const entry of state.entries) state.selected.set(entryKey(entry), entry);
  paintMarks();
}

/** Ticks, cut items, and the bar of what to do with the selection. */
function paintMarks() {
  document.body.classList.toggle('selecting', state.selecting);
  const cut = state.clip && state.clip.mode === 'cut' ? new Set(state.clip.items.map(entryKey)) : new Set();
  for (const entry of state.entries) {
    if (!entry.nodes) continue;
    const key = entryKey(entry);
    entry.nodes.item.classList.toggle('selected', state.selecting && state.selected.has(key));
    entry.nodes.item.classList.toggle('cut', cut.has(key));
    entry.nodes.item.setAttribute('aria-selected', String(state.selecting && state.selected.has(key)));
  }
  const bar = $('selbar');
  bar.hidden = !state.selecting;
  const count = state.selected.size;
  $('sel-count').replaceChildren(element('b', '', String(count)), element('span', 'sel-word', ' selected'));
  const picked = Array.from(state.selected.values());
  const changeable = full() && picked.every((entry) => !entry.top);
  for (const id of ['sel-move', 'sel-delete']) $(id).hidden = !changeable;
  $('sel-download').disabled = !count;
  $('sel-move').disabled = !count;
  $('sel-delete').disabled = !count;
}

function selectionMenu(opener) {
  const picked = Array.from(state.selected.values());
  const items = [];
  const changeable = full() && picked.every((entry) => !entry.top);
  if (picked.length && changeable) items.push({ label: 'Cut', icon: 'cut', run: () => { toClipboard(picked, 'cut'); endSelect(); } });
  if (picked.length && full()) items.push({ label: 'Copy', icon: 'copy', run: () => { toClipboard(picked, 'copy'); endSelect(); } });
  if (picked.length && changeable) items.push({ label: 'Copy to…', icon: 'copy', run: () => moveOrCopy(picked, 'copy') });
  items.push({ label: 'Select all', icon: 'all', run: selectAll });
  showMenu(items, { title: picked.length + ' selected', opener, at: isTouch() ? null : below(opener) });
}

/** Several items download as one zip; a single file as itself. */
function downloadSelected() {
  const picked = Array.from(state.selected.values());
  if (!picked.length) return;
  if (picked.length === 1 && !picked[0].dir) { saveEntry(picked[0]); endSelect(); return; }
  if (e2e) {
    const filesOnly = picked.filter((entry) => !entry.dir);
    status('Folders can’t be zipped while the share is encrypted end to end; downloading the '
      + filesOnly.length + ' file(s) one at a time.', filesOnly.length < picked.length);
    (async () => { for (const entry of filesOnly) await save(entry.root, entry.path, entry.name); })();
    endSelect();
    return;
  }
  saveZip(state.root, state.path, folderLabel(state.root, state.path), picked.map((entry) => entry.name));
  endSelect();
}

// ----- Cut, copy, paste, and move ------------------------------------------------

function toClipboard(items, mode) {
  state.clip = { mode, items: items.map((entry) => ({ root: entry.root, path: entry.path, name: entry.name, dir: entry.dir })) };
  const what = items.length === 1 ? '“' + items[0].name + '”' : items.length + ' items';
  status((mode === 'cut' ? 'Cut ' : 'Copied ') + what + '. Open a folder and choose Paste.');
  paintTools();
  paintMarks();
}

async function paste(destination) {
  const clip = state.clip;
  if (!clip) return;
  const done = await transfer(clip.items, clip.mode === 'cut' ? 'move' : 'copy', destination);
  if (clip.mode === 'cut' && done) state.clip = null;
  paintTools();
  paintMarks();
}

/** Move to… (or Copy to…): a folder picker, then the move. */
async function moveOrCopy(items, preferred = 'move') {
  const choice = await pickFolder(items, preferred);
  endSelect();
  if (choice) await transfer(items, choice.mode, choice.folder);
}

/**
 * Moves or copies `items` into `destination` ({ root, path, name }), one
 * by one; a name already there is asked about like an upload's.
 * Returns whether everything went (or was skipped on purpose).
 */
async function transfer(items, mode, destination) {
  let always = null;
  let moved = 0;
  let skipped = 0;
  const verb = mode === 'move' ? 'Moving ' : 'Copying ';
  const where = destination.name || folderLabel(destination.root, destination.path);
  for (const [index, item] of items.entries()) {
    const label = items.length > 1 ? (index + 1) + ' of ' + items.length + ': ' : '';
    status(verb + label + item.name + '…');
    const params = { root: item.root, path: item.path, to_root: destination.root, to: destination.path, conflict: always && always !== 'skip' ? always : 'ask' };
    try {
      try {
        await call('/api/' + mode, params, 'POST');
      } catch (error) {
        if (!(error.status === 409 && error.data && error.data.conflict)) throw error;
        let choice = always;
        if (!choice) {
          const answer = await askConflict(item.name, error.data.dir, index + 1 < items.length, where, true);
          choice = answer.choice;
          if (answer.all) always = choice;
        }
        if (choice === 'skip') { skipped += 1; continue; }
        params.conflict = choice;
        await call('/api/' + mode, params, 'POST');
      }
      moved += 1;
    } catch (error) {
      status(item.name + ': ' + error.message, true);
      refresh();
      return false;
    }
  }
  refresh();
  const what = moved === 1 ? '1 item' : moved + ' items';
  status((mode === 'move' ? 'Moved ' : 'Copied ') + what + ' to ' + where + '.' + (skipped ? ' Skipped ' + skipped + '.' : ''));
  return true;
}

/**
 * The folder picker: walks the shared folders and offers Move here and
 * Copy here. The items' own folders (and what's in them) can't be chosen.
 */
function pickFolder(items, preferred) {
  return new Promise((resolve) => {
    const box = $('picker');
    let place = { root: state.root, path: state.root === null ? '' : state.path };
    if (place.root === null && state.info.roots.length === 1) place = { root: state.info.roots[0].id, path: '' };
    const inside = (root, path) => items.some((item) => item.dir && item.root === root
      && (path === item.path || path.startsWith(item.path + '/')));
    const changeable = items.every((item) => !item.top);
    $('picker-title').textContent = (changeable ? 'Move or copy ' : 'Copy ')
      + (items.length === 1 ? '“' + items[0].name + '”' : items.length + ' items');
    $('picker-move').hidden = !changeable;
    const finish = (mode) => {
      box.onclose = null;
      box.close();
      resolve(mode ? { mode, folder: { root: place.root, path: place.path } } : null);
      syncHistory();
    };
    const load = async () => {
      const list = $('picker-list');
      const trailNode = $('picker-trail');
      trailNode.replaceChildren();
      const crumb = (label, target) => {
        const button = element('button', '', label);
        button.type = 'button';
        button.addEventListener('click', () => { place = target; load(); });
        trailNode.append(button);
      };
      if (state.info.roots.length > 1) crumb('Shared folders', { root: null, path: '' });
      if (place.root !== null) {
        if (state.info.roots.length > 1) trailNode.append(element('span', 'sep', '/'));
        crumb(rootName(place.root), { root: place.root, path: '' });
        let walked = '';
        for (const name of place.path.split('/').filter(Boolean)) {
          walked = join(walked, name);
          trailNode.append(element('span', 'sep', '/'));
          crumb(name, { root: place.root, path: walked });
        }
      }
      const blocked = place.root === null || inside(place.root, place.path);
      $('picker-move').disabled = blocked;
      $('picker-copy').disabled = blocked;
      $('picker-new').hidden = place.root === null;
      list.replaceChildren(element('li', 'picker-note', 'Loading…'));
      let folders = [];
      try {
        if (place.root === null) {
          folders = state.info.roots.map((item) => ({ name: item.name, root: item.id, path: '' }));
        } else {
          const data = await call('/api/list', { root: place.root, path: place.path });
          folders = data.items.filter((item) => item.dir)
            .sort((a, b) => collator.compare(a.name, b.name))
            .map((item) => ({ name: item.name, root: place.root, path: join(place.path, item.name) }));
        }
      } catch (error) {
        list.replaceChildren(element('li', 'picker-note', error.message));
        return;
      }
      if (!box.open) return;
      list.replaceChildren(...folders.map((folder) => {
        const li = element('li');
        const button = element('button', 'picker-folder');
        button.type = 'button';
        button.append(element('span', 'kind dir'), element('span', 'label', folder.name));
        if (inside(folder.root, folder.path)) {
          button.disabled = true;
          button.title = 'It can’t go into itself.';
        }
        button.addEventListener('click', () => { place = { root: folder.root, path: folder.path }; load(); });
        li.append(button);
        return li;
      }));
      if (!folders.length) list.append(element('li', 'picker-note', 'No folders in here.'));
    };
    $('picker-move').onclick = () => finish('move');
    $('picker-copy').onclick = () => finish('copy');
    $('picker-cancel').onclick = () => finish(null);
    $('picker-new').onclick = async () => {
      const name = await askText('New folder', 'Name of the new folder', '', 'Create');
      if (!name) return;
      try {
        await call('/api/mkdir', { root: place.root, path: place.path, name }, 'POST');
        place = { root: place.root, path: join(place.path, name) };
      } catch (error) {
        status(error.message, true);
      }
      load();
    };
    box.oncancel = (event) => { event.preventDefault(); finish(null); };
    box.onclose = () => finish(null);
    $('picker-move').classList.toggle('primary', preferred === 'move');
    $('picker-copy').classList.toggle('primary', preferred !== 'move' || !changeable);
    box.showModal();
    load();
    syncHistory();
  });
}

// ----- Small dialogs ---------------------------------------------------------------

function openDialog() {
  return document.querySelector('dialog[open]');
}

/** Asks for a name; resolves with it, or null. */
function askText(title, label, value, okLabel) {
  return new Promise((resolve) => {
    const box = $('ask');
    $('ask-title').textContent = title;
    $('ask-label').textContent = label;
    $('ask-text').hidden = true;
    const input = $('ask-input');
    input.hidden = false;
    input.value = value;
    $('ask-ok').textContent = okLabel;
    $('ask-ok').className = 'primary';
    const done = (answer) => {
      $('ask-form').onsubmit = null;
      box.oncancel = null;
      if (box.open) box.close();
      resolve(answer);
      syncHistory();
    };
    $('ask-form').onsubmit = (event) => {
      event.preventDefault();
      const text = input.value.trim();
      done(text || null);
    };
    $('ask-cancel').onclick = () => done(null);
    box.oncancel = (event) => { event.preventDefault(); done(null); };
    box.showModal();
    input.focus();
    // The name without its extension is what people usually change.
    const dot = value.lastIndexOf('.');
    input.setSelectionRange(0, dot > 0 ? dot : value.length);
    syncHistory();
  });
}

/** Asks yes or no; resolves true for `okLabel`. */
function confirmBox(title, text, okLabel, danger = false) {
  return new Promise((resolve) => {
    const box = $('ask');
    $('ask-title').textContent = title;
    $('ask-label').textContent = '';
    $('ask-text').hidden = false;
    $('ask-text').textContent = text;
    $('ask-input').hidden = true;
    $('ask-ok').textContent = okLabel;
    $('ask-ok').className = danger ? 'primary danger-fill' : 'primary';
    const done = (answer) => {
      $('ask-form').onsubmit = null;
      box.oncancel = null;
      if (box.open) box.close();
      resolve(answer);
      syncHistory();
    };
    $('ask-form').onsubmit = (event) => { event.preventDefault(); done(true); };
    $('ask-cancel').onclick = () => done(false);
    box.oncancel = (event) => { event.preventDefault(); done(false); };
    box.showModal();
    $('ask-cancel').focus();
    syncHistory();
  });
}

/**
 * Asks what to do with a name that is already there: Replace, Keep both,
 * or Skip, and whether to do the same for the rest. Uploads, moves, and
 * copies all ask with this.
 */
function askConflict(name, isFolder, more, where = '', folderReplace = false) {
  return new Promise((resolve) => {
    const box = $('conflict');
    $('conflict-text').textContent = '“' + name + '” is already in ' + (where ? '“' + where + '”' : 'this folder') + '.'
      + (isFolder && folderReplace ? ' Replace sends the folder that’s there to the ' + binName() + '.' : '');
    $('conflict-all').checked = false;
    $('conflict-all-row').hidden = !more;
    $('conflict-replace').hidden = isFolder && !folderReplace;
    const done = (choice) => {
      box.oncancel = null;
      box.close();
      resolve({ choice, all: more && $('conflict-all').checked });
      syncHistory();
    };
    $('conflict-replace').onclick = () => done('replace');
    $('conflict-keep').onclick = () => done('keep');
    $('conflict-skip').onclick = () => done('skip');
    box.oncancel = (event) => { event.preventDefault(); done('skip'); };
    box.showModal();
    $('conflict-keep').focus();
    syncHistory();
  });
}

// ----- Uploads ------------------------------------------------------------------

/**
 * Uploads files into the open folder. Each item is `{ file, sub }`, where
 * `sub` is the folder inside it the file came from (for whole folders),
 * which the server makes as needed.
 */
async function upload(items) {
  if (!items.length || state.root === null || !full()) return;
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
  const can = (event) => full() && inFolder()
    && event.dataTransfer && Array.from(event.dataTransfer.types || []).includes('Files');
  const showDrop = (on) => $('drop').classList.toggle('show', on);
  document.addEventListener('dragenter', (event) => {
    if (!can(event)) return;
    event.preventDefault();
    depth += 1;
    showDrop(true);
  });
  document.addEventListener('dragover', (event) => {
    if (!can(event)) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = 'copy';
  });
  document.addEventListener('dragleave', () => {
    depth = Math.max(0, depth - 1);
    if (!depth) showDrop(false);
  });
  document.addEventListener('drop', async (event) => {
    depth = 0;
    showDrop(false);
    if (!can(event)) return;
    event.preventDefault();
    upload(await dropped(event.dataTransfer));
  });
}

// ----- New folder, rename, delete -------------------------------------------------

async function makeFolder() {
  if (!inFolder()) return;
  const name = await askText('New folder', 'Name of the new folder', '', 'Create');
  if (!name) return;
  try {
    await call('/api/mkdir', { root: state.root, path: state.path, name }, 'POST');
    refresh();
  } catch (error) {
    status(error.message, true);
  }
}

async function renameItem(entry) {
  const to = await askText('Rename', 'New name', entry.name, 'Rename');
  if (!to || to === entry.name) return;
  try {
    await call('/api/rename', { root: entry.root, path: entry.path, to }, 'POST');
    refresh();
  } catch (error) {
    status(error.message, true);
  }
}

function binName() {
  return state.info.bin || 'Recycle Bin';
}

/**
 * Deletes after asking: to the Recycle Bin (or the trash). When the trash
 * can't take something, asks again before deleting it for good.
 */
async function deleteItems(items) {
  if (!items.length) return;
  const what = items.length === 1 ? '“' + items[0].name + '”' : items.length + ' items';
  const sure = await confirmBox('Delete ' + what + '?',
    (items.length === 1 ? 'It goes' : 'They go') + ' to the ' + binName()
      + ' on the computer running Feather, where ' + (items.length === 1 ? 'it' : 'they') + ' can be restored.', 'Delete', true);
  if (!sure) return;
  endSelect();
  let gone = 0;
  for (const item of items) {
    status('Deleting ' + item.name + '…');
    try {
      try {
        await call('/api/delete', { root: item.root, path: item.path }, 'POST');
      } catch (error) {
        if (!(error.status === 409 && error.data && error.data.permanent)) throw error;
        const forGood = await confirmBox('Delete “' + item.name + '” for good?',
          error.message + ' Deleting it for good can’t be undone.', 'Delete for good', true);
        if (!forGood) continue;
        await call('/api/delete', { root: item.root, path: item.path, permanent: '1' }, 'POST');
      }
      gone += 1;
    } catch (error) {
      status(item.name + ': ' + error.message, true);
      refresh();
      return;
    }
  }
  refresh();
  status(gone === 1 ? 'Deleted 1 item.' : 'Deleted ' + gone + ' items.');
}

// ----- Back goes up ----------------------------------------------------------------
//
// The page keeps at most one history entry of its own above where it was
// opened. While a menu, dialog, the viewer, or a selection is open, or a
// folder below the top is shown, that entry is there, so Android's Back
// (button or swipe) and the browser's Back land on this page: it closes
// what is open, or goes up one folder, and puts the entry back if still
// needed. At the top with nothing open, Back leaves.

const nav = { guard: false, ignore: 0, dropping: false };

function historyNeeded() {
  return !!menu.layer || !!openDialog() || !$('viewer').hidden || state.selecting || !atHome();
}

function atHome() {
  if (state.query) return false;
  if (state.info.roots.length === 1) return state.root === state.info.roots[0].id && !state.path;
  return state.root === null;
}

function placeHash() {
  if (atHome() || state.root === null) return '';
  return '#/' + state.root + (state.path ? '/' + state.path.split('/').map(encodeURIComponent).join('/') : '');
}

function syncHistory() {
  if (!state.info) return;
  const needed = historyNeeded();
  const url = location.pathname + location.search + placeHash();
  if (needed && !nav.guard) {
    history.pushState({ feather: 'top' }, '', url);
    nav.guard = true;
  } else if (needed) {
    history.replaceState({ feather: 'top' }, '', url);
  } else if (nav.guard && !nav.dropping) {
    // A moment later, so a menu item that opens a dialog keeps the entry.
    nav.dropping = true;
    setTimeout(() => {
      nav.dropping = false;
      if (nav.guard && !historyNeeded()) {
        nav.guard = false;
        nav.ignore += 1;
        history.back();
      }
    }, 60);
  }
}

function wireHistory() {
  window.addEventListener('popstate', (event) => {
    const top = !!(event.state && event.state.feather === 'top');
    if (nav.ignore) {
      nav.ignore -= 1;
      nav.guard = top;
      return;
    }
    nav.guard = top;
    if (top) return;
    const dialog = openDialog();
    if (menu.layer) closeMenu(true);
    else if (dialog) dialog.dispatchEvent(new Event('cancel', { cancelable: true }));
    else if (!$('viewer').hidden) closeViewer();
    else if (state.selecting) endSelect();
    else if (state.query) clearSearch();
    else if (!atHome()) goUp();
    syncHistory();
  });
}

/** A link to a folder (`#/root/path`) opens it, with Back going to the top. */
function linkedPlace() {
  const hash = location.hash;
  if (!hash.startsWith('#/')) return null;
  const parts = hash.slice(2).split('/');
  const root = Number(parts.shift());
  if (!Number.isInteger(root) || !state.info.roots.some((item) => item.id === root)) return null;
  try {
    return { root, path: parts.filter(Boolean).map(decodeURIComponent).join('/') };
  } catch (_) {
    return null;
  }
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

function wireTools() {
  for (const id of ['files', 'folder', 'camera']) {
    $(id).addEventListener('change', (event) => upload(picked(event.target)));
  }
  // Folder pickers aren't on every phone.
  const folders = 'webkitdirectory' in document.createElement('input');
  if (!folders) $('t-folder').remove();
  $('t-upload').addEventListener('click', () => $('files').click());
  if ($('t-folder')) $('t-folder').addEventListener('click', () => $('folder').click());
  $('t-camera').addEventListener('click', () => $('camera').click());
  $('t-mkdir').addEventListener('click', makeFolder);
  $('t-paste').addEventListener('click', () => paste({ root: state.root, path: state.path }));
  $('t-select').addEventListener('click', () => (state.selecting ? endSelect() : startSelect(null)));
  $('t-more').addEventListener('click', (event) => {
    const items = [];
    if (full()) {
      if (folders) items.push({ label: 'Upload folder', icon: 'upload', run: () => $('folder').click() });
      items.push({ label: 'New folder', icon: 'newfolder', run: makeFolder });
      if (state.clip) items.push({ label: 'Paste here', icon: 'paste', run: () => paste({ root: state.root, path: state.path }) });
    }
    items.push({ label: state.selecting ? 'Stop selecting' : 'Select', icon: 'select', run: () => (state.selecting ? endSelect() : startSelect(null)) });
    if (!e2e) items.push({ label: 'Download this folder', icon: 'download', run: () => saveZip(state.root, state.path, folderLabel(state.root, state.path)) });
    showMenu(items, { title: folderLabel(state.root, state.path), opener: event.currentTarget, at: isTouch() ? null : below(event.currentTarget) });
  });
  $('sort').addEventListener('click', (event) => openSortMenu(event.currentTarget, isTouch() ? null : below(event.currentTarget)));
  $('sel-close').addEventListener('click', () => endSelect());
  $('sel-download').addEventListener('click', downloadSelected);
  $('sel-move').addEventListener('click', () => moveOrCopy(Array.from(state.selected.values())));
  $('sel-delete').addEventListener('click', () => deleteItems(Array.from(state.selected.values())));
  $('sel-more').addEventListener('click', (event) => selectionMenu(event.currentTarget));
  document.addEventListener('keydown', (event) => {
    if (menu.layer && event.key === 'Escape') { event.preventDefault(); closeMenu(); return; }
    if (!$('viewer').hidden || openDialog() || menu.layer) return;
    if (event.key === 'Escape' && state.selecting) { event.preventDefault(); endSelect(); }
  });
  for (const id of ['ask', 'conflict', 'picker']) $(id).addEventListener('close', () => setTimeout(syncHistory, 0));
}

async function start() {
  try {
    state.info = await call('/api/info');
  } catch (error) {
    status(error.message, true);
    return;
  }
  rememberAddresses(state.info);
  state.sort = loadSort();
  $('user').textContent = state.info.user;
  // Without a password there is nothing to sign out of.
  $('logout').hidden = !state.info.user;
  const searchable = state.info.mode === 'search' || state.info.mode === 'full';
  $('search').hidden = !searchable;
  $('search').addEventListener('submit', (event) => {
    event.preventDefault();
    const text = $('query').value.trim();
    if (text) search(text); else clearSearch();
  });
  $('logout').addEventListener('click', async () => {
    try { await call('/logout', {}, 'POST'); } catch (_) { /* signed out anyway */ }
    sessionStorage.removeItem('feather-key');
    location.href = '/login';
  });
  wireTools();
  wireDrop();
  $('zip').addEventListener('click', () => saveZip(state.root, state.path, folderLabel(state.root, state.path)));
  offerLocal();
  $('as-list').addEventListener('click', () => chooseView('list'));
  $('as-grid').addEventListener('click', () => chooseView('grid'));
  wireViewer();
  wireHistory();
  // The page's own entry is the top; a link to a folder goes above it.
  const linked = linkedPlace();
  history.replaceState({ feather: 'base' }, '', location.pathname + location.search);
  if (linked) open(linked.root, linked.path);
  else if (state.info.roots.length === 1) open(state.info.roots[0].id, '');
  else open(null, '');
}

start();
