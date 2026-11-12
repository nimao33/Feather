'use strict';

// Feather's service worker. It keeps the page's own files, so the app
// installed from this address opens even when the computer can't be
// reached, and then shows the reconnect page instead of the browser's
// error. Shared files and API answers are never stored here.

const CACHE = 'feather-shell-3';
const SHELL = [
  '/app.css', '/app.js', '/icon.svg', '/icon-192.png', '/manifest.webmanifest',
  '/reconnect', '/reconnect.js',
];

self.addEventListener('install', (event) => {
  event.waitUntil(caches.open(CACHE).then((cache) => cache.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener('activate', (event) => {
  event.waitUntil(caches.keys()
    .then((names) => Promise.all(names.filter((name) => name !== CACHE).map((name) => caches.delete(name))))
    .then(() => self.clients.claim()));
});

/** The network first, so a new Feather's page is used at once. */
async function fresh(request) {
  try {
    const response = await fetch(request);
    if (response.ok && response.type === 'basic') {
      const copy = response.clone();
      caches.open(CACHE).then((cache) => cache.put(request, copy)).catch(() => {});
    }
    return response;
  } catch (error) {
    const kept = await caches.match(request, { ignoreSearch: true });
    if (kept) return kept;
    throw error;
  }
}

self.addEventListener('fetch', (event) => {
  const request = event.request;
  if (request.method !== 'GET') return;
  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  if (request.mode === 'navigate') {
    // The page itself is never stored: when it can't be fetched, the
    // reconnect page looks for the computer at its other addresses.
    event.respondWith(fetch(request).catch(async () => {
      const page = await caches.match('/reconnect');
      return page || new Response('Feather can’t be reached.', { status: 503, headers: { 'Content-Type': 'text/plain; charset=utf-8' } });
    }));
    return;
  }
  if (SHELL.includes(url.pathname)) event.respondWith(fresh(request));
});
