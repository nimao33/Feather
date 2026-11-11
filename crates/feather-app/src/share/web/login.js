'use strict';

// The sign-in page: the password's eye button, and, when the share is
// encrypted end to end, signing in without ever sending the password.

const encoder = new TextEncoder();

function hex(bytes) {
  return Array.from(new Uint8Array(bytes), (byte) => byte.toString(16).padStart(2, '0')).join('');
}

function unhex(text) {
  const bytes = new Uint8Array(text.length / 2);
  for (let at = 0; at < bytes.length; at += 1) bytes[at] = parseInt(text.substr(at * 2, 2), 16);
  return bytes;
}

function concat(...parts) {
  const out = new Uint8Array(parts.reduce((sum, part) => sum + part.length, 0));
  let at = 0;
  for (const part of parts) { out.set(part, at); at += part.length; }
  return out;
}

async function hmac(key, label, ...parts) {
  const hmacKey = await crypto.subtle.importKey('raw', key, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
  return new Uint8Array(await crypto.subtle.sign('HMAC', hmacKey, concat(encoder.encode(label), ...parts)));
}

function note(text) {
  let line = document.querySelector('.note');
  if (!line) {
    line = document.createElement('p');
    line.className = 'note';
    line.setAttribute('role', 'alert');
    document.querySelector('form').before(line);
  }
  line.textContent = text;
}

async function signIn(user, password, button) {
  button.disabled = true;
  button.textContent = 'Signing in…';
  try {
    const hello = await (await fetch('/e2e/hello', { credentials: 'same-origin' })).json();
    const base = await crypto.subtle.importKey('raw', encoder.encode(password), 'PBKDF2', false, ['deriveBits']);
    const key = new Uint8Array(await crypto.subtle.deriveBits(
      { name: 'PBKDF2', hash: 'SHA-256', salt: unhex(hello.salt), iterations: hello.rounds }, base, 256));
    const server = unhex(hello.nonce);
    const client = crypto.getRandomValues(new Uint8Array(16));
    const proof = await hmac(key, 'feather login', server, client, encoder.encode(user));
    const body = new URLSearchParams({ id: hello.id, user, client: hex(client), proof: hex(proof) });
    const response = await fetch('/e2e/login', { method: 'POST', body, credentials: 'same-origin' });
    const answer = await response.json();
    if (!response.ok) throw new Error(answer.error || 'Sign-in failed.');
    // The server proves it knows the key too.
    const expected = await hmac(key, 'feather server', server, client);
    if (hex(expected) !== answer.server) throw new Error('This server could not prove it is Feather.');
    const hkdf = await crypto.subtle.importKey('raw', key, 'HKDF', false, ['deriveBits']);
    const session = await crypto.subtle.deriveBits(
      { name: 'HKDF', hash: 'SHA-256', salt: concat(server, client), info: encoder.encode('feather session') }, hkdf, 256);
    sessionStorage.setItem('feather-key', hex(session));
    location.href = '/';
  } catch (error) {
    note(error.message);
    button.disabled = false;
    button.textContent = 'Sign in';
  }
}

function start() {
  const password = document.getElementById('password');
  const eye = document.getElementById('reveal');
  eye.hidden = false;
  eye.addEventListener('click', () => {
    const showing = password.type === 'text';
    password.type = showing ? 'password' : 'text';
    eye.setAttribute('aria-pressed', String(!showing));
    eye.setAttribute('aria-label', showing ? 'Show password' : 'Hide password');
    password.focus();
  });
  if (document.body.dataset.e2e !== '1') return;
  const form = document.querySelector('form');
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    signIn(document.getElementById('user').value, password.value, form.querySelector('button[type="submit"]'));
  });
}

start();
