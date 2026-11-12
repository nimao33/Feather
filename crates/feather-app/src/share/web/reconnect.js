'use strict';

// The installed app's reconnect page. Shown by the service worker when the
// address the app was opened at doesn't answer: a tunnel's address may have
// changed, or the phone may be on the computer's Wi-Fi now, or away from
// it. It asks every address this app has seen whether Feather answers there
// (only `/api/ping`, which says nothing else), prefers the local network,
// and otherwise reads the QR code from Feather's Settings or takes a typed
// address. Nothing from the share is stored or sent here.

const $ = (id) => document.getElementById(id);
const KEY = 'feather-addresses';

/** 10.x, 192.168.x, 172.16-31.x, 100.64-127.x (Tailscale), link-local, .local, localhost. */
function isLocal(host) {
  if (host === 'localhost' || host.endsWith('.local') || host.startsWith('[')) return true;
  const parts = host.split('.').map(Number);
  if (parts.length !== 4 || parts.some((part) => !Number.isInteger(part))) return false;
  const [a, b] = parts;
  return a === 10 || a === 127 || (a === 192 && b === 168) || (a === 172 && b >= 16 && b <= 31)
    || (a === 100 && b >= 64 && b <= 127) || (a === 169 && b === 254);
}

function known() {
  let list = [];
  try { list = JSON.parse(localStorage.getItem(KEY) || '[]'); } catch (_) { list = []; }
  if (!Array.isArray(list)) list = [];
  const origins = list.map((item) => item && item.url).filter((url) => typeof url === 'string');
  if (!origins.includes(location.origin)) origins.unshift(location.origin);
  return origins.map((url) => {
    let host = url;
    try { host = new URL(url).hostname; } catch (_) { /* kept as is */ }
    return { url, host, local: isLocal(host) };
  });
}

function remember(url) {
  try {
    const list = JSON.parse(localStorage.getItem(KEY) || '[]').filter((item) => item && item.url !== url);
    list.unshift({ url, seen: Date.now() });
    localStorage.setItem(KEY, JSON.stringify(list.slice(0, 12)));
  } catch (_) { /* private mode */ }
}

/** Whether Feather answers at `origin`, within a few seconds. */
async function probe(origin, wait = 3500) {
  const stop = new AbortController();
  const timer = setTimeout(() => stop.abort(), wait);
  try {
    const response = await fetch(origin + '/api/ping', {
      mode: 'cors', cache: 'no-store', credentials: 'omit', signal: stop.signal,
    });
    if (!response.ok) return false;
    const data = await response.json();
    return data && data.app === 'feather';
  } catch (_) {
    return false;
  } finally {
    clearTimeout(timer);
  }
}

function go(url) {
  remember(url);
  location.href = url + '/';
}

function say(title, message) {
  $('title').textContent = title;
  $('message').textContent = message;
}

/** An address the person typed or scanned, as an origin. */
function origin(text) {
  let value = (text || '').trim();
  if (!value) return null;
  if (!/^https?:\/\//i.test(value)) value = 'https://' + value;
  try {
    const url = new URL(value);
    return url.protocol === 'https:' || url.hostname === 'localhost' || url.hostname === '127.0.0.1' ? url.origin : null;
  } catch (_) {
    return null;
  }
}

let looking = false;

async function look() {
  if (looking) return;
  looking = true;
  $('choices').replaceChildren();
  say('Looking for Feather…', 'Checking the addresses this app has used before.');
  const list = known();
  const answers = await Promise.all(list.map((item) => probe(item.url).then((ok) => (ok ? item : null))));
  looking = false;
  const working = answers.filter(Boolean);
  if (working.some((item) => item.url === location.origin)) {
    // Only a hiccup: this address answers again.
    location.replace('/');
    return;
  }
  if (!working.length) {
    say('Feather can’t be reached',
      'The computer may be asleep, Feather may be closed, or its internet address may have changed. '
      + 'Scan the QR code in Feather’s Settings, under Sharing, or type the address shown there.');
    return;
  }
  if (working.length === 1) {
    say('Found Feather', 'Opening ' + working[0].host + '…');
    go(working[0].url);
    return;
  }
  // More than one answers: the local network is faster, so it comes first.
  working.sort((a, b) => Number(b.local) - Number(a.local));
  say('Feather answers at more than one address', 'Choose how to connect. On the same Wi-Fi, the local network is much faster.');
  $('choices').replaceChildren(...working.map((item, index) => {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = index === 0 ? 'primary' : '';
    const label = document.createElement('strong');
    label.textContent = item.local ? 'Local network' : 'Internet';
    const host = document.createElement('span');
    host.textContent = item.host;
    button.append(label, host);
    button.addEventListener('click', () => go(item.url));
    return button;
  }));
}

// ----- The QR code ---------------------------------------------------------------

let scanning = null;

function stopScan() {
  if (scanning) {
    scanning.stream.getTracks().forEach((track) => track.stop());
    clearInterval(scanning.timer);
    scanning = null;
  }
  $('scanner').hidden = true;
}

async function scan() {
  stopScan();
  try {
    const detector = new BarcodeDetector({ formats: ['qr_code'] });
    const stream = await navigator.mediaDevices.getUserMedia({ video: { facingMode: 'environment' }, audio: false });
    const video = $('camera');
    video.srcObject = stream;
    $('scanner').hidden = false;
    await video.play();
    const timer = setInterval(async () => {
      if (!scanning || video.readyState < 2) return;
      let codes = [];
      try { codes = await detector.detect(video); } catch (_) { return; }
      const found = codes.map((code) => origin(code.rawValue)).find(Boolean);
      if (!found) return;
      stopScan();
      say('Found an address', 'Opening ' + new URL(found).host + '…');
      // Opened even if it doesn't answer yet: the browser may first need
      // the certificate of a local address accepted.
      if (!(await probe(found))) say('Opening ' + new URL(found).host + '…', 'It didn’t answer yet; the browser may ask about its certificate first.');
      go(found);
    }, 300);
    scanning = { stream, timer };
  } catch (error) {
    stopScan();
    say('The camera couldn’t be used', 'Type the address shown in Feather’s Settings instead.');
  }
}

$('retry').addEventListener('click', look);
if ('BarcodeDetector' in window && navigator.mediaDevices && navigator.mediaDevices.getUserMedia) {
  $('scan').hidden = false;
  $('scan').addEventListener('click', scan);
}
$('stop-scan').addEventListener('click', stopScan);
$('manual').addEventListener('submit', (event) => {
  event.preventDefault();
  const found = origin($('address').value);
  if (!found) {
    say('That isn’t an address', 'It looks like https://something, as shown in Feather’s Settings.');
    return;
  }
  go(found);
});
window.addEventListener('online', look);
look();
