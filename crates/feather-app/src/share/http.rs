//! The web server: HTTPS only, a sign-in page, and a small file browser
//! that talks to a JSON API. Requests are read with strict limits on size
//! and time, and every response carries headers that keep the page from
//! being framed, sniffed, or fed scripts from elsewhere.
//!
//! With end-to-end encryption on, the password never crosses the wire:
//! the page proves it knows the password-derived key, both sides derive a
//! fresh session key, and every request and reply after that (names, file
//! contents, uploads) is sealed with AES-256-GCM inside the HTTPS stream.
//! A tunnel in the middle sees only opaque bytes.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use feather_core::{EntryRef, Query};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use rustls::{ServerConnection, StreamOwned};

use super::files::{self, Denied, Item};
use super::zip::ZipWriter;
use super::{allowed, auth, thumbs, Shared};
use crate::settings::{HttpMode, ShareAccess, Tunnel};

const MAX_CONNECTIONS: usize = 64;
const MAX_HEAD: usize = 16 * 1024;
const MAX_HEADERS: usize = 100;
const MAX_FORM: u64 = 8 * 1024;
const MAX_UPLOAD: u64 = 16 * 1024 * 1024 * 1024;
/// A request's head must arrive within this, however slowly it trickles.
const HEAD_DEADLINE: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
const REQUESTS_PER_CONNECTION: usize = 500;
const LIST_LIMIT: usize = 5000;
const SEARCH_LIMIT: usize = 200;
const SEARCH_TIME: Duration = Duration::from_secs(3);
/// The `__Host-` prefix makes browsers refuse the cookie unless it is
/// Secure, for the whole site, and set by this exact host.
const COOKIE: &str = "__Host-feather";
/// Sealed file data travels in frames of this much plaintext.
const FRAME: usize = 256 * 1024;
/// Bytes a frame adds: length, nonce, last-frame flag, and tag.
const FRAME_OVERHEAD: u64 = 4 + 12 + 1 + 16;
/// Replies bigger than this are compressed when the browser accepts gzip.
const COMPRESS_FROM: usize = 1024;

/// A folder's zip holds at most this many files and folders...
const ZIP_ENTRIES: usize = 100_000;
/// ...and goes at most this deep.
const ZIP_DEPTH: usize = 64;
/// Why folders aren't zipped end to end: sealed downloads are decrypted in
/// the page's memory before they are saved, and a folder can be far bigger
/// than a phone can hold.
const SEALED_ZIP: &str = "Folders can't be downloaded as a zip while the share is encrypted end to end. Open the folder and download its files one at a time.";

const APP_HTML: &str = include_str!("web/app.html");
const LOGIN_HTML: &str = include_str!("web/login.html");
const APP_CSS: &str = include_str!("web/app.css");
const APP_JS: &str = include_str!("web/app.js");
const LOGIN_JS: &str = include_str!("web/login.js");
const ICON_SVG: &str = include_str!("web/icon.svg");
const RECONNECT_HTML: &str = include_str!("web/reconnect.html");
const RECONNECT_JS: &str = include_str!("web/reconnect.js");
const SERVICE_WORKER: &str = include_str!("web/sw.js");
const MANIFEST: &str = include_str!("web/manifest.webmanifest");
const ICON_192: &[u8] = include_bytes!("web/icon-192.png");
const ICON_512: &[u8] = include_bytes!("web/icon-512.png");
const ICON_MASKABLE: &[u8] = include_bytes!("web/icon-maskable.png");
const APPLE_TOUCH_ICON: &[u8] = include_bytes!("web/apple-touch-icon.png");

/// A page file. The browser test server can read them from
/// `FEATHER_WEB_DIR` instead, so changes show without a rebuild.
fn asset(name: &str, built_in: &'static str) -> &'static str {
    #[cfg(test)]
    if let Some(folder) = std::env::var_os("FEATHER_WEB_DIR") {
        if let Ok(text) = std::fs::read_to_string(Path::new(&folder).join(name)) {
            return Box::leak(text.into_boxed_str());
        }
    }
    let _ = name;
    built_in
}

/// Sent with every response.
const SECURITY_HEADERS: &str = "Strict-Transport-Security: max-age=31536000\r\n\
X-Content-Type-Options: nosniff\r\n\
X-Frame-Options: DENY\r\n\
Referrer-Policy: same-origin\r\n\
Cross-Origin-Opener-Policy: same-origin\r\n\
Cross-Origin-Resource-Policy: same-origin\r\n\
Permissions-Policy: camera=(), microphone=(), geolocation=(), interest-cohort=()\r\n\
Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; \
img-src 'self' data: blob:; media-src 'self' blob:; connect-src 'self'; manifest-src 'self'; \
worker-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'\r\n\
Server: Feather\r\n";

/// The reconnect page, shown by the installed app when its address stops
/// answering: it may ask other addresses of this Feather if they answer
/// (`/api/ping` only), and use the camera to read the QR code in Settings.
/// It never holds or sends anything from the share.
const RECONNECT_HEADERS: &str = "Strict-Transport-Security: max-age=31536000\r\n\
X-Content-Type-Options: nosniff\r\n\
X-Frame-Options: DENY\r\n\
Referrer-Policy: no-referrer\r\n\
Cross-Origin-Opener-Policy: same-origin\r\n\
Cross-Origin-Resource-Policy: same-origin\r\n\
Permissions-Policy: camera=(self), microphone=(), geolocation=(), interest-cohort=()\r\n\
Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; \
img-src 'self' data: blob:; media-src 'self' blob:; connect-src 'self' https:; manifest-src 'self'; \
form-action 'none'; frame-ancestors 'none'; base-uri 'none'\r\n\
Server: Feather\r\n";

pub fn spawn(shared: Arc<Shared>, listener: TcpListener) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("feather-web".into())
        .spawn(move || accept(shared, listener))
        .expect("start the web server")
}

fn accept(shared: Arc<Shared>, listener: TcpListener) {
    if listener.set_nonblocking(true).is_err() {
        return;
    }
    while !shared.stopping() {
        match listener.accept() {
            Ok((stream, peer)) => {
                let address = peer.ip();
                // Anyone the access setting doesn't allow is dropped unread.
                if !allowed(shared.config.access, shared.config.tunnel, address)
                    || shared.connections.load(Ordering::SeqCst) >= MAX_CONNECTIONS
                {
                    continue;
                }
                shared.connections.fetch_add(1, Ordering::SeqCst);
                let worker = Arc::clone(&shared);
                let started = std::thread::Builder::new()
                    .name("feather-web-client".into())
                    .spawn(move || {
                        let _ = serve(&worker, stream, address);
                        worker.connections.fetch_sub(1, Ordering::SeqCst);
                    });
                if started.is_err() {
                    shared.connections.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

fn tunnelled(shared: &Shared) -> bool {
    shared.config.access == ShareAccess::Internet && shared.config.tunnel != Tunnel::Direct
}

/// The connection: TLS, or plain only in tests.
pub(crate) enum Wire {
    Tls(Box<StreamOwned<ServerConnection, TcpStream>>),
    Plain(TcpStream),
}

impl Read for Wire {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Wire::Tls(stream) => stream.read(buffer),
            Wire::Plain(stream) => stream.read(buffer),
        }
    }
}

impl Write for Wire {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Wire::Tls(stream) => stream.write(buffer),
            Wire::Plain(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Wire::Tls(stream) => stream.flush(),
            Wire::Plain(stream) => stream.flush(),
        }
    }
}

pub(crate) struct Conn {
    pub(crate) wire: Wire,
    /// Bytes read past the request head.
    pub(crate) buffer: Vec<u8>,
    /// The browser takes gzip replies.
    pub(crate) gzip: bool,
}

impl Conn {
    /// The next request head, or `None` when the visitor closed cleanly.
    pub(crate) fn read_head(&mut self) -> io::Result<Option<Vec<u8>>> {
        let started = Instant::now();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(end) = find(&self.buffer, b"\r\n\r\n") {
                return Ok(Some(self.buffer.drain(..end + 4).collect()));
            }
            if self.buffer.len() > MAX_HEAD {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let read = match self.wire.read(&mut chunk) {
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => 0,
                Err(error) => return Err(error),
            };
            if read == 0 {
                return if self.buffer.is_empty() {
                    Ok(None)
                } else {
                    Err(io::ErrorKind::UnexpectedEof.into())
                };
            }
            self.buffer.extend_from_slice(&chunk[..read]);
            if started.elapsed() > HEAD_DEADLINE {
                return Err(io::ErrorKind::TimedOut.into());
            }
        }
    }

    /// Exactly `wanted` body bytes into `out`, from what was buffered first.
    fn read_exact_body(&mut self, out: &mut [u8]) -> io::Result<()> {
        let buffered = self.buffer.len().min(out.len());
        out[..buffered].copy_from_slice(&self.buffer[..buffered]);
        self.buffer.drain(..buffered);
        self.wire.read_exact(&mut out[buffered..]).map_err(|error| {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                io::ErrorKind::UnexpectedEof.into()
            } else {
                error
            }
        })
    }

    /// Feeds exactly `length` body bytes to `sink`.
    fn read_body(
        &mut self,
        length: u64,
        sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut left = length;
        let mut chunk = vec![0u8; 64 * 1024];
        while left > 0 {
            let want = (chunk.len() as u64).min(left) as usize;
            self.read_exact_body(&mut chunk[..want])?;
            sink(&chunk[..want])?;
            left -= want as u64;
        }
        Ok(())
    }

    pub(crate) fn read_small_body(&mut self, length: u64) -> io::Result<Vec<u8>> {
        let mut body = vec![0u8; length as usize];
        self.read_exact_body(&mut body)?;
        Ok(body)
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) query: Vec<(String, String)>,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) length: u64,
    pub(crate) close: bool,
    /// HTTP/1.0, which has no chunked replies.
    pub(crate) http10: bool,
}

impl Request {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn cookie(&self, name: &str) -> Option<String> {
        self.header("cookie")?.split(';').find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key == name).then(|| value.to_string())
        })
    }
}

/// Parameters of one API call, plain or sealed.
pub(crate) struct Params(pub(crate) Vec<(String, String)>);

impl Params {
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// Every value given for `name`.
    fn all(&self, name: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    fn root(&self) -> usize {
        self.get("root")
            .and_then(|value| value.parse().ok())
            .unwrap_or(usize::MAX)
    }

    fn path(&self) -> &str {
        self.get("path").unwrap_or("")
    }
}

/// Parses a request head. Anything unusual is refused rather than guessed.
pub(crate) fn parse(head: &[u8]) -> Result<Request, u16> {
    let text = std::str::from_utf8(head).map_err(|_| 400u16)?;
    let mut lines = text.split("\r\n");
    let first = lines.next().ok_or(400u16)?;
    let mut parts = first.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(400);
    };
    if method.is_empty() || method.len() > 10 || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(400);
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(505);
    }
    if !target.starts_with('/') || target.len() > 8 * 1024 {
        return Err(400);
    }
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        // Folded header lines are a smuggling trick; refuse them.
        if line.starts_with([' ', '\t']) {
            return Err(400);
        }
        let (name, value) = line.split_once(':').ok_or(400u16)?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_!#$%&'*+.^`|~".contains(&b))
        {
            return Err(400);
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
        if headers.len() > MAX_HEADERS {
            return Err(431);
        }
    }
    if headers.iter().any(|(name, _)| name == "transfer-encoding") {
        return Err(501);
    }
    let mut length: Option<u64> = None;
    for (_, value) in headers.iter().filter(|(name, _)| name == "content-length") {
        let parsed: u64 = value.parse().map_err(|_| 400u16)?;
        if length.is_some_and(|known| known != parsed) {
            return Err(400);
        }
        length = Some(parsed);
    }
    let connection = headers
        .iter()
        .find(|(name, _)| name == "connection")
        .map(|(_, value)| value.to_ascii_lowercase())
        .unwrap_or_default();
    let close = connection.contains("close")
        || (version == "HTTP/1.0" && !connection.contains("keep-alive"));
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Ok(Request {
        method: method.to_string(),
        path: path.to_string(),
        query: parse_form(query).ok_or(400u16)?,
        headers,
        length: length.unwrap_or(0),
        close,
        http10: version == "HTTP/1.0",
    })
}

/// `a=1&b=two` with `+` and `%XX` decoded. `None` for broken encoding.
pub(crate) fn parse_form(text: &str) -> Option<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    for pair in text.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        pairs.push((decode(key)?, decode(value)?));
    }
    Some(pairs)
}

fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = text.get(at + 1..at + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                at += 2;
            }
            byte => out.push(byte),
        }
        at += 1;
    }
    let decoded = String::from_utf8(out).ok()?;
    (!decoded.contains('\0')).then_some(decoded)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        303 => "See Other",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        505 => "HTTP Version Not Supported",
        _ => "Error",
    }
}

/// Writes a response head; the body follows separately.
pub(crate) fn head(
    conn: &mut Conn,
    status: u16,
    content_type: &str,
    length: u64,
    extra: &[(&str, String)],
    close: bool,
) -> io::Result<()> {
    let mut text = format!("HTTP/1.1 {status} {}\r\n{SECURITY_HEADERS}", reason(status));
    text.push_str(&format!(
        "Content-Type: {content_type}\r\nContent-Length: {length}\r\n"
    ));
    if !extra.iter().any(|(name, _)| *name == "Cache-Control") {
        text.push_str("Cache-Control: no-store\r\n");
    }
    text.push_str(if close {
        "Connection: close\r\n"
    } else {
        "Connection: keep-alive\r\nKeep-Alive: timeout=15\r\n"
    });
    for (name, value) in extra {
        // Header values never carry line breaks from file names.
        let value: String = value.chars().filter(|c| !c.is_control()).collect();
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str("\r\n");
    conn.wire.write_all(text.as_bytes())
}

/// Sends a whole reply, gzipped when it's text, big enough, and wanted.
pub(crate) fn send(
    conn: &mut Conn,
    status: u16,
    content_type: &str,
    body: &[u8],
    extra: &[(&str, String)],
    close: bool,
) -> io::Result<()> {
    let compressible = content_type.starts_with("text/")
        || content_type.starts_with("application/json")
        || content_type.starts_with("image/svg");
    if conn.gzip && compressible && body.len() >= COMPRESS_FROM {
        let packed = gzip(body);
        let mut headers = extra.to_vec();
        headers.push(("Content-Encoding", "gzip".into()));
        headers.push(("Vary", "Accept-Encoding".into()));
        head(
            conn,
            status,
            content_type,
            packed.len() as u64,
            &headers,
            close,
        )?;
        conn.wire.write_all(&packed)?;
    } else {
        head(conn, status, content_type, body.len() as u64, extra, close)?;
        conn.wire.write_all(body)?;
    }
    conn.wire.flush()
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let deflated = miniz_oxide::deflate::compress_to_vec(data, 6);
    let mut out = Vec::with_capacity(deflated.len() + 18);
    out.extend_from_slice(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255]);
    out.extend_from_slice(&deflated);
    let mut crc = crc32fast::Hasher::new();
    crc.update(data);
    out.extend_from_slice(&crc.finalize().to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

pub(crate) fn json(conn: &mut Conn, status: u16, body: &str) -> io::Result<()> {
    send(
        conn,
        status,
        "application/json; charset=utf-8",
        body.as_bytes(),
        &[],
        false,
    )
}

pub(crate) fn error_json(message: &str) -> String {
    format!("{{\"error\":{}}}", quote(message))
}

pub(crate) fn redirect(conn: &mut Conn, to: &str, extra: &[(&str, String)]) -> io::Result<()> {
    let mut headers = vec![("Location", to.to_string())];
    headers.extend(extra.iter().cloned());
    send(conn, 303, "text/plain; charset=utf-8", b"", &headers, false)
}

/// A JSON string, with `<`, `>` and `&` escaped too so it is safe anywhere.
pub(crate) fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub(crate) fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn item_json(item: &Item) -> String {
    format!(
        "{{\"name\":{},\"dir\":{},\"size\":{},\"modified\":{}}}",
        quote(&item.name),
        item.dir,
        item.size,
        item.modified
    )
}

fn serve(shared: &Shared, stream: TcpStream, address: IpAddr) -> io::Result<()> {
    // Accepted from a non-blocking listener, Windows makes it non-blocking too.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let _ = stream.set_nodelay(true);
    let wire = if shared.plain {
        Wire::Plain(stream)
    } else {
        let connection =
            ServerConnection::new(Arc::clone(&shared.tls)).map_err(io::Error::other)?;
        Wire::Tls(Box::new(StreamOwned::new(connection, stream)))
    };
    let mut conn = Conn {
        wire,
        buffer: Vec::new(),
        gzip: false,
    };
    for _ in 0..REQUESTS_PER_CONNECTION {
        if shared.stopping() {
            break;
        }
        let Ok(Some(raw)) = conn.read_head() else {
            break;
        };
        let request = match parse(&raw) {
            Ok(request) => request,
            Err(status) => {
                let _ = send(
                    &mut conn,
                    status,
                    "text/plain; charset=utf-8",
                    reason(status).as_bytes(),
                    &[],
                    true,
                );
                break;
            }
        };
        conn.gzip = request
            .header("accept-encoding")
            .is_some_and(|value| value.split(',').any(|part| part.trim().starts_with("gzip")));
        let client = client_address(shared, address, &request);
        match handle(shared, &mut conn, &request, client) {
            Ok(true) if !request.close => {}
            _ => break,
        }
    }
    if let Wire::Tls(stream) = &mut conn.wire {
        stream.conn.send_close_notify();
        let _ = stream.flush();
    }
    Ok(())
}

/// The visitor's real address. Behind a tunnel the connection comes from
/// 127.0.0.1 and the tunnel names the visitor in a header.
fn client_address(shared: &Shared, peer: IpAddr, request: &Request) -> IpAddr {
    if !tunnelled(shared) || !peer.is_loopback() {
        return peer;
    }
    request
        .header("cf-connecting-ip")
        .or_else(|| {
            request
                .header("x-forwarded-for")
                .and_then(|value| value.split(',').next())
        })
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(peer)
}

/// Whether a request that changes something came from Feather's own page.
/// Browsers say so in `Sec-Fetch-Site`, which no page can fake; older ones
/// send `Origin`, which must name this site (as the tunnel sees it, too).
fn from_this_site(shared: &Shared, request: &Request) -> bool {
    if let Some(site) = request.header("sec-fetch-site") {
        return site == "same-origin" || site == "none";
    }
    match request.header("origin") {
        None => true,
        Some("null") => false,
        Some(origin) => {
            let forwarded = tunnelled(shared)
                .then(|| request.header("x-forwarded-host"))
                .flatten();
            [request.header("host"), forwarded]
                .into_iter()
                .flatten()
                .any(|host| {
                    origin.eq_ignore_ascii_case(&format!("https://{host}"))
                        || (shared.plain && origin.eq_ignore_ascii_case(&format!("http://{host}")))
                })
        }
    }
}

/// Handles one request. `Ok(true)` keeps the connection for the next.
fn handle(shared: &Shared, conn: &mut Conn, request: &Request, client: IpAddr) -> io::Result<bool> {
    let takes_body = matches!(
        (request.method.as_str(), request.path.as_str()),
        ("POST", "/login")
            | ("POST", "/e2e/login")
            | ("PUT", "/api/upload")
            | ("POST", "/api/sealed")
            | ("POST", "/api/sealed-download")
            | ("PUT", "/api/sealed-upload")
    );
    if request.length > 0 && !takes_body {
        // A body nobody reads would be taken as the next request.
        send(conn, 413, "text/plain", b"Unexpected body", &[], true)?;
        return Ok(false);
    }
    let token_value = request.cookie(COOKIE);
    let session = if shared.config.open() {
        Some((
            String::new(),
            auth::Session {
                csrf: shared.open_csrf.clone(),
                key: None,
            },
        ))
    } else {
        token_value.as_ref().and_then(|value| {
            shared
                .sessions
                .get(value)
                .map(|session| (value.clone(), session))
        })
    };
    let e2e = shared.e2e.is_some();
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/app.css") => static_bytes(
            conn,
            request,
            "text/css; charset=utf-8",
            asset("app.css", APP_CSS).as_bytes(),
        ),
        ("GET", "/app.js") => static_bytes(
            conn,
            request,
            "text/javascript; charset=utf-8",
            asset("app.js", APP_JS).as_bytes(),
        ),
        ("GET", "/login.js") => static_bytes(
            conn,
            request,
            "text/javascript; charset=utf-8",
            asset("login.js", LOGIN_JS).as_bytes(),
        ),
        ("GET", "/icon.svg" | "/favicon.ico") => static_bytes(
            conn,
            request,
            "image/svg+xml",
            asset("icon.svg", ICON_SVG).as_bytes(),
        ),
        ("GET", "/icon-192.png") => static_bytes(conn, request, "image/png", ICON_192),
        ("GET", "/icon-512.png") => static_bytes(conn, request, "image/png", ICON_512),
        ("GET", "/icon-maskable.png") => static_bytes(conn, request, "image/png", ICON_MASKABLE),
        ("GET", "/apple-touch-icon.png" | "/apple-touch-icon-precomposed.png") => {
            static_bytes(conn, request, "image/png", APPLE_TOUCH_ICON)
        }
        ("GET", "/manifest.webmanifest") => static_bytes(
            conn,
            request,
            "application/manifest+json",
            asset("manifest.webmanifest", MANIFEST).as_bytes(),
        ),
        ("GET", "/sw.js") => static_bytes(
            conn,
            request,
            "text/javascript; charset=utf-8",
            asset("sw.js", SERVICE_WORKER).as_bytes(),
        ),
        ("GET", "/reconnect.js") => static_bytes(
            conn,
            request,
            "text/javascript; charset=utf-8",
            asset("reconnect.js", RECONNECT_JS).as_bytes(),
        ),
        ("GET", "/reconnect") => reconnect_page(conn),
        // Whether this is a Feather that answers, for the installed app
        // looking for its other addresses. Says nothing else, to anyone.
        ("GET" | "OPTIONS", "/api/ping") => ping(conn, request),
        ("GET", "/login") => {
            if session.is_some() {
                redirect(conn, "/", &[])?;
            } else {
                login_page(conn, e2e, 200, "")?;
            }
            Ok(true)
        }
        ("POST", "/login") if !e2e => login(shared, conn, request, client),
        ("POST", "/login") => {
            // With end-to-end on, the password is never sent: the page signs
            // in with a proof instead.
            login_page(
                conn,
                e2e,
                400,
                "This share signs in with JavaScript. Turn it on and try again.",
            )?;
            Ok(true)
        }
        ("GET", "/e2e/hello") if e2e => {
            let (id, nonce) = shared.handshakes.start();
            let salt = shared.e2e.map(|(_, salt)| salt).unwrap_or_default();
            json(
                conn,
                200,
                &format!(
                    "{{\"id\":\"{id}\",\"salt\":\"{}\",\"rounds\":{},\"nonce\":\"{}\"}}",
                    auth::hex(&salt),
                    auth::E2E_ROUNDS,
                    auth::hex(&nonce)
                ),
            )?;
            Ok(true)
        }
        ("POST", "/e2e/login") if e2e => e2e_login(shared, conn, request, client),
        ("GET", "/") => {
            if session.is_some() {
                let page =
                    asset("app.html", APP_HTML).replace("{{e2e}}", if e2e { "1" } else { "0" });
                send(
                    conn,
                    200,
                    "text/html; charset=utf-8",
                    page.as_bytes(),
                    &[],
                    false,
                )?;
            } else {
                redirect(conn, "/login", &[])?;
            }
            Ok(true)
        }
        (_, path) if path.starts_with("/api/") || path == "/logout" => {
            let Some((token_value, session)) = session else {
                json(conn, 401, &error_json("Please sign in."))?;
                return Ok(true);
            };
            if request.method != "GET" && !from_this_site(shared, request) {
                json(
                    conn,
                    403,
                    &error_json("This request didn't come from Feather's page."),
                )?;
                return Ok(false);
            }
            match (session.key, path) {
                (Some(key), "/api/sealed") => {
                    sealed_call(shared, conn, request, client, &token_value, &key)
                }
                (Some(key), "/api/sealed-download") => {
                    sealed_download(shared, conn, request, client, &token_value, &key)
                }
                (Some(key), "/api/sealed-upload") => {
                    sealed_upload(shared, conn, request, client, &token_value, &key)
                }
                (Some(_), "/api/zip") => {
                    json(conn, 403, &error_json(SEALED_ZIP))?;
                    Ok(true)
                }
                // Signed in end to end: nothing travels in the clear.
                (Some(_), _) => {
                    json(
                        conn,
                        403,
                        &error_json("This share is encrypted end to end."),
                    )?;
                    Ok(true)
                }
                (None, _) if e2e => {
                    json(conn, 401, &error_json("Please sign in again."))?;
                    Ok(true)
                }
                (None, _) => plain_api(shared, conn, request, client, &token_value, &session),
            }
        }
        _ => {
            json(conn, 404, &error_json("Not found."))?;
            Ok(true)
        }
    }
}

/// A file that never changes while Feather runs: cached by the browser and
/// checked with a tag, so reloading the page costs a few bytes.
pub(crate) fn static_file(
    conn: &mut Conn,
    request: &Request,
    content_type: &str,
    body: &'static str,
) -> io::Result<bool> {
    static_bytes(conn, request, content_type, body.as_bytes())
}

/// Like `static_file`, for pictures.
fn static_bytes(
    conn: &mut Conn,
    request: &Request,
    content_type: &str,
    body: &'static [u8],
) -> io::Result<bool> {
    static TAGS: OnceLock<std::sync::Mutex<std::collections::HashMap<usize, String>>> =
        OnceLock::new();
    let tags = TAGS.get_or_init(Default::default);
    let tag = tags
        .lock()
        .unwrap()
        .entry(body.as_ptr() as usize)
        .or_insert_with(|| {
            let digest = ring::digest::digest(&ring::digest::SHA256, body);
            format!("\"{}\"", auth::hex(&digest.as_ref()[..12]))
        })
        .clone();
    let cache = [
        ("Cache-Control", "no-cache".to_string()),
        ("ETag", tag.clone()),
    ];
    if request.header("if-none-match") == Some(tag.as_str()) {
        head(conn, 304, content_type, 0, &cache, false)?;
        conn.wire.flush()?;
        return Ok(true);
    }
    send(conn, 200, content_type, body, &cache, false)?;
    Ok(true)
}

/// The installed app's page for when its address stops answering.
fn reconnect_page(conn: &mut Conn) -> io::Result<bool> {
    let body = asset("reconnect.html", RECONNECT_HTML);
    let text = format!(
        "HTTP/1.1 200 OK\r\n{RECONNECT_HEADERS}Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\
         Keep-Alive: timeout=15\r\n\r\n",
        body.len()
    );
    conn.wire.write_all(text.as_bytes())?;
    conn.wire.write_all(body.as_bytes())?;
    conn.wire.flush()?;
    Ok(true)
}

/// `{"app":"feather"}` for anyone, from any page, so the installed app can
/// tell which of the addresses it remembers answer. Private network access
/// preflights are allowed, since the local address is usually private.
fn ping(conn: &mut Conn, request: &Request) -> io::Result<bool> {
    let mut headers = vec![("Access-Control-Allow-Origin", "*".to_string())];
    if request.method == "OPTIONS" {
        headers.push(("Access-Control-Allow-Methods", "GET".to_string()));
        headers.push(("Access-Control-Max-Age", "600".to_string()));
        if request.header("access-control-request-private-network") == Some("true") {
            headers.push(("Access-Control-Allow-Private-Network", "true".to_string()));
        }
        send(conn, 204, "text/plain", b"", &headers, false)?;
    } else {
        send(
            conn,
            200,
            "application/json; charset=utf-8",
            b"{\"app\":\"feather\"}",
            &headers,
            false,
        )?;
    }
    Ok(true)
}

fn login_page(conn: &mut Conn, e2e: bool, status: u16, message: &str) -> io::Result<()> {
    let note = if message.is_empty() {
        String::new()
    } else {
        format!(
            "<p class=\"note\" role=\"alert\">{}</p>",
            escape_html(message)
        )
    };
    let page = asset("login.html", LOGIN_HTML)
        .replace("{{note}}", &note)
        .replace("{{e2e}}", if e2e { "1" } else { "0" });
    send(
        conn,
        status,
        "text/html; charset=utf-8",
        page.as_bytes(),
        &[],
        false,
    )
}

fn cookie_header(token_value: &str) -> (&'static str, String) {
    (
        "Set-Cookie",
        format!("{COOKIE}={token_value}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=43200"),
    )
}

fn login(shared: &Shared, conn: &mut Conn, request: &Request, client: IpAddr) -> io::Result<bool> {
    if request.length > MAX_FORM {
        send(conn, 413, "text/plain", b"Too large", &[], true)?;
        return Ok(false);
    }
    if !from_this_site(shared, request) {
        login_page(conn, false, 403, "Sign in from Feather's own page.")?;
        return Ok(false);
    }
    let body = conn.read_small_body(request.length)?;
    let form = Params(
        std::str::from_utf8(&body)
            .ok()
            .and_then(parse_form)
            .unwrap_or_default(),
    );
    if let Err(wait) = shared.guard.check(client) {
        shared.log(client, "web sign-in refused: locked out");
        login_page(conn, false, 429, &locked_message(wait))?;
        return Ok(true);
    }
    let config = &shared.config;
    let user = form.get("user").unwrap_or("");
    let password = form.get("password").unwrap_or("");
    if auth::check(user, password, &config.user, &config.password_hash) {
        shared.guard.succeeded(client);
        let (token_value, _) = shared.sessions.create(None);
        shared.log(client, &format!("web sign-in: {}", config.user));
        redirect(conn, "/", &[cookie_header(&token_value)])?;
    } else {
        let delay = shared.guard.delay(client);
        shared.guard.failed(client);
        shared.log(client, "web sign-in failed");
        std::thread::sleep(delay);
        login_page(conn, false, 401, "That user name and password don't match.")?;
    }
    Ok(true)
}

fn locked_message(wait: Duration) -> String {
    format!(
        "Too many wrong tries. Try again in {} minutes.",
        wait.as_secs().div_ceil(60).max(1)
    )
}

/// Signs in with a proof of the password-derived key; the password itself
/// never leaves the browser.
fn e2e_login(
    shared: &Shared,
    conn: &mut Conn,
    request: &Request,
    client: IpAddr,
) -> io::Result<bool> {
    if request.length > MAX_FORM {
        send(conn, 413, "text/plain", b"Too large", &[], true)?;
        return Ok(false);
    }
    if !from_this_site(shared, request) {
        json(conn, 403, &error_json("Sign in from Feather's own page."))?;
        return Ok(false);
    }
    let body = conn.read_small_body(request.length)?;
    let form = Params(
        std::str::from_utf8(&body)
            .ok()
            .and_then(parse_form)
            .unwrap_or_default(),
    );
    if let Err(wait) = shared.guard.check(client) {
        shared.log(client, "web sign-in refused: locked out");
        json(conn, 429, &error_json(&locked_message(wait)))?;
        return Ok(true);
    }
    let Some((key, _)) = shared.e2e else {
        json(conn, 400, &error_json("End-to-end sign-in is off."))?;
        return Ok(true);
    };
    let user = form.get("user").unwrap_or("");
    let server_nonce = form.get("id").and_then(|id| shared.handshakes.take(id));
    let client_nonce = form
        .get("client")
        .and_then(auth::unhex)
        .filter(|nonce| nonce.len() == 16);
    let given = form.get("proof").and_then(auth::unhex);
    let ok = match (&server_nonce, &client_nonce, &given) {
        (Some(server), Some(client_bytes), Some(given)) => {
            let expected = auth::proof(
                &key,
                "feather login",
                &[server, client_bytes, user.as_bytes()],
            );
            auth::same(&expected, given)
                && auth::same(user.as_bytes(), shared.config.user.as_bytes())
        }
        _ => false,
    };
    if !ok {
        let delay = shared.guard.delay(client);
        shared.guard.failed(client);
        shared.log(client, "web sign-in failed");
        std::thread::sleep(delay);
        json(
            conn,
            401,
            &error_json("That user name and password don't match."),
        )?;
        return Ok(true);
    }
    let (server, client_bytes) = (
        server_nonce.unwrap_or_default(),
        client_nonce.unwrap_or_default(),
    );
    shared.guard.succeeded(client);
    let session = auth::session_key(&key, &server, &client_bytes);
    let (token_value, _) = shared.sessions.create(Some(session));
    shared.log(
        client,
        &format!("web sign-in (end to end): {}", shared.config.user),
    );
    let answer = auth::proof(&key, "feather server", &[&server, &client_bytes]);
    send(
        conn,
        200,
        "application/json; charset=utf-8",
        format!("{{\"server\":\"{}\"}}", auth::hex(&answer)).as_bytes(),
        &[cookie_header(&token_value)],
        false,
    )?;
    Ok(true)
}

fn mode_name(mode: HttpMode) -> &'static str {
    match mode {
        HttpMode::Off => "off",
        HttpMode::Download => "download",
        HttpMode::Search => "search",
        HttpMode::Full => "full",
    }
}

/// What an API call answers.
enum Reply {
    Json(u16, String),
    /// A file to send, with an optional byte range; `inline` shows a
    /// picture or video in the page instead of saving it.
    File(PathBuf, bool),
    /// A JPEG preview of a file, at most this many pixels across.
    Thumb(PathBuf, u32),
    /// A folder as a zip: the shared folder's number, the path in it, and
    /// when only some of what's in it is wanted, their names.
    Zip(usize, String, Vec<String>),
    /// Signed out.
    Logout,
}

/// The API calls that don't stream: the same for plain and sealed requests.
fn call(
    shared: &Shared,
    method: &str,
    path: &str,
    params: &Params,
    client: IpAddr,
    csrf: &str,
) -> Reply {
    let mode = shared.config.http;
    let full = mode == HttpMode::Full;
    let ok = |body: String| Reply::Json(200, body);
    let refuse = |denied: Denied| match denied {
        Denied::BadName => Reply::Json(400, error_json("That name isn't allowed.")),
        Denied::NotFound => Reply::Json(404, error_json("Not found.")),
    };
    match (method, path) {
        ("GET", "/api/info") => {
            let roots: Vec<String> = shared
                .roots
                .roots
                .iter()
                .enumerate()
                .map(|(id, root)| format!("{{\"id\":{id},\"name\":{}}}", quote(&root.name)))
                .collect();
            // The faster address for someone on the same network, when
            // they came in some other way.
            let local = match (shared.config.access, super::lan_address()) {
                (ShareAccess::Local, _) | (_, None) => String::new(),
                (_, Some(address)) => {
                    format!(
                        "https://{}",
                        SocketAddr::new(address, shared.config.http_port)
                    )
                }
            };
            // Every web address of this Feather, for the installed app to
            // try when the one it was opened at stops answering.
            let addresses: Vec<String> = shared
                .status
                .lock()
                .unwrap()
                .addresses
                .iter()
                .filter(|address| address.starts_with("https://"))
                .map(|address| quote(address))
                .collect();
            ok(format!(
                "{{\"user\":{},\"mode\":\"{}\",\"csrf\":{},\"local\":{},\"addresses\":[{}],\"bin\":\"{}\",\"roots\":[{}]}}",
                quote(if shared.config.open() {
                    ""
                } else {
                    &shared.config.user
                }),
                mode_name(mode),
                quote(csrf),
                quote(&local),
                addresses.join(","),
                if cfg!(windows) { "Recycle Bin" } else { "trash" },
                roots.join(",")
            ))
        }
        ("POST", "/logout") => Reply::Logout,
        ("GET", "/api/list") => match shared.roots.resolve(params.root(), params.path()) {
            Ok(folder) if folder.is_dir() => match shared.roots.list(&folder, LIST_LIMIT) {
                Ok((items, truncated)) => {
                    let items: Vec<String> = items.iter().map(item_json).collect();
                    ok(format!(
                        "{{\"items\":[{}],\"truncated\":{truncated}}}",
                        items.join(",")
                    ))
                }
                Err(_) => Reply::Json(404, error_json("That folder can't be opened.")),
            },
            Ok(_) => Reply::Json(400, error_json("That is a file, not a folder.")),
            Err(denied) => refuse(denied),
        },
        ("GET", "/api/search") if mode == HttpMode::Search || full => {
            let text = params.get("q").unwrap_or("").trim();
            if text.is_empty() || text.len() > 300 {
                Reply::Json(400, error_json("Type something to search for."))
            } else {
                ok(search(shared, text))
            }
        }
        ("GET", "/api/thumb") => match shared.roots.resolve(params.root(), params.path()) {
            Ok(file) if file.is_file() => {
                let side = params
                    .get("size")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(256u32);
                Reply::Thumb(file, side.clamp(32, thumbs::MAX_SIDE))
            }
            Ok(_) => Reply::Json(400, error_json("That is a folder, not a file.")),
            Err(denied) => refuse(denied),
        },
        ("GET", "/api/download") => match shared.roots.resolve(params.root(), params.path()) {
            Ok(file) if file.is_file() => {
                let inline = params.get("inline") == Some("1");
                // Views are logged where the request's range is known.
                if !inline {
                    shared.log(client, &format!("download {}", files::display_path(&file)));
                }
                Reply::File(file, inline)
            }
            Ok(_) => Reply::Json(
                400,
                error_json("Folders can't be downloaded; open them instead."),
            ),
            Err(denied) => refuse(denied),
        },
        ("GET", "/api/zip") => match shared.roots.resolve(params.root(), params.path()) {
            Ok(folder) if folder.is_dir() => {
                let names: Vec<String> = params.all("name").into_iter().map(String::from).collect();
                if names.iter().any(|name| files::check_name(name).is_err()) {
                    return refuse(Denied::BadName);
                }
                let what = if names.is_empty() {
                    String::new()
                } else {
                    format!(" ({} chosen)", names.len())
                };
                shared.log(
                    client,
                    &format!("download folder {}{what}", files::display_path(&folder)),
                );
                Reply::Zip(params.root(), params.path().to_string(), names)
            }
            Ok(_) => Reply::Json(400, error_json("That is a file, not a folder.")),
            Err(denied) => refuse(denied),
        },
        ("POST", "/api/mkdir") if full => {
            let name = params.get("name").unwrap_or("");
            match shared.roots.resolve_new(params.root(), params.path(), name) {
                Ok(target) if target.exists() => Reply::Json(
                    409,
                    error_json("Something with that name is already there."),
                ),
                Ok(target) => match std::fs::create_dir(&target) {
                    Ok(()) => {
                        shared.sizes.forget();
                        shared.log(
                            client,
                            &format!("new folder {}", files::display_path(&target)),
                        );
                        Reply::Json(201, "{}".into())
                    }
                    Err(error) => {
                        Reply::Json(400, error_json(&format!("Couldn't create it: {error}")))
                    }
                },
                Err(denied) => refuse(denied),
            }
        }
        ("POST", "/api/rename") if full => {
            let to = params.get("to").unwrap_or("");
            let path = params.path();
            // Either separator: resolve accepts both.
            let parent = path
                .rsplit_once(['/', '\\'])
                .map(|(parent, _)| parent)
                .unwrap_or("");
            match (
                shared.roots.resolve(params.root(), path),
                shared.roots.resolve_new(params.root(), parent, to),
            ) {
                (Ok(from), _) if shared.roots.is_root(params.root(), &from) => {
                    Reply::Json(403, error_json("A shared folder itself can't be renamed."))
                }
                // Left-out folders are matched by their full path, so a new
                // name for a folder above one would expose it.
                (Ok(from), _) if from.is_dir() && shared.roots.holds_blocked(&from) => {
                    Reply::Json(403, error_json("This folder holds a folder that isn't shared, so it can't be renamed, moved or deleted from here."))
                }
                (Ok(_), Ok(target)) if target.exists() => Reply::Json(
                    409,
                    error_json("Something with that name is already there."),
                ),
                (Ok(from), Ok(target)) => match std::fs::rename(&from, &target) {
                    Ok(()) => {
                        shared.sizes.forget();
                        shared.log(
                            client,
                            &format!(
                                "rename {} to {}",
                                files::display_path(&from),
                                files::display_path(&target)
                            ),
                        );
                        ok("{}".into())
                    }
                    Err(error) => {
                        Reply::Json(400, error_json(&format!("Couldn't rename it: {error}")))
                    }
                },
                (Err(denied), _) | (_, Err(denied)) => refuse(denied),
            }
        }
        ("POST", "/api/delete") if full => match shared.roots.resolve(params.root(), params.path())
        {
            Ok(target) if shared.roots.is_root(params.root(), &target) => {
                Reply::Json(403, error_json("A shared folder itself can't be deleted."))
            }
            Ok(target) if target.is_dir() && shared.roots.holds_blocked(&target) => {
                Reply::Json(403, error_json("This folder holds a folder that isn't shared, so it can't be renamed, moved or deleted from here."))
            }
            // Windows always has the Recycle Bin: visitors never delete for
            // good there.
            Ok(_) if cfg!(windows) && params.get("permanent") == Some("1") => Reply::Json(
                403,
                error_json("Items can only be moved to the Recycle Bin."),
            ),
            Ok(target) => {
                // For good only when the person chose that, after the trash
                // couldn't take it.
                let permanent = params.get("permanent") == Some("1");
                let result = if permanent {
                    files::remove(&target)
                } else {
                    files::recycle(&target)
                };
                shared.sizes.forget();
                match result {
                    Ok(()) => {
                        let how = if permanent {
                            "delete for good"
                        } else {
                            "delete"
                        };
                        shared.log(client, &format!("{how} {}", files::display_path(&target)));
                        ok(format!("{{\"permanent\":{permanent}}}"))
                    }
                    Err(error) if !permanent && cfg!(not(windows)) => Reply::Json(
                        409,
                        format!("{{\"error\":{},\"permanent\":true}}", quote(&error)),
                    ),
                    Err(error) => Reply::Json(400, error_json(&error)),
                }
            }
            Err(denied) => refuse(denied),
        },
        ("POST", "/api/move") if full => transfer(shared, params, client, false),
        ("POST", "/api/copy") if full => transfer(shared, params, client, true),
        ("GET", "/api/sizes") => folder_sizes(shared, params),
        _ => Reply::Json(404, error_json("Not found.")),
    }
}

fn denied_json(denied: Denied) -> Reply {
    match denied {
        Denied::BadName => Reply::Json(400, error_json("That name isn't allowed.")),
        Denied::NotFound => Reply::Json(404, error_json("Not found.")),
    }
}

/// Moves or copies one file or folder (`root`, `path`) into the folder
/// `to` of shared folder `to_root`. Both ends go through the same checks as
/// everything else, a folder can't go into itself, and a folder holding a
/// left-out folder can't be moved, since that would carry it out of the
/// list. A name already taken is answered with the same 409 as an upload,
/// so the page can ask Replace, Keep both, or Skip; Replace sends what was
/// there to the Recycle Bin first.
fn transfer(shared: &Shared, params: &Params, client: IpAddr, copy: bool) -> Reply {
    let roots = &shared.roots;
    let root = params.root();
    let to_root = params
        .get("to_root")
        .and_then(|value| value.parse().ok())
        .unwrap_or(root);
    let to = params.get("to").unwrap_or("").trim_matches('/');
    let from = match roots.resolve(root, params.path()) {
        Ok(from) => from,
        Err(denied) => return denied_json(denied),
    };
    let folder = match roots.resolve(to_root, to) {
        Ok(folder) if folder.is_dir() => folder,
        Ok(_) => return denied_json(Denied::NotFound),
        Err(denied) => return denied_json(denied),
    };
    if !copy && roots.is_root(root, &from) {
        return Reply::Json(403, error_json("A shared folder itself can't be moved."));
    }
    let Some(name) = from
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return Reply::Json(403, error_json("That can't be moved."));
    };
    if from.is_dir() && files::inside(&from, &folder) {
        return Reply::Json(400, error_json("A folder can't go into itself."));
    }
    if !copy && from.is_dir() && roots.holds_blocked(&from) {
        return Reply::Json(
            403,
            error_json("That folder holds a folder that isn't shared, so it can't be moved."),
        );
    }
    let mut target = match roots.resolve_new(to_root, to, &name) {
        Ok(target) => target,
        Err(denied) => return denied_json(denied),
    };
    let here = files::same_path(&target, &from);
    if std::fs::symlink_metadata(&target).is_ok() {
        match Conflict::of(params) {
            Conflict::Ask => return Reply::Json(409, conflict_json(&name, target.is_dir())),
            Conflict::KeepBoth if here && !copy => {
                return Reply::Json(400, error_json("It's already in that folder."))
            }
            Conflict::KeepBoth => match files::free_name(&target) {
                Some(free) => target = free,
                None => return Reply::Json(409, error_json("No free name was found.")),
            },
            Conflict::Replace if here || files::inside(&target, &from) => {
                return Reply::Json(400, error_json("Something can't replace itself."))
            }
            Conflict::Replace => {
                // Only something the visitor can see may be replaced.
                let Ok(existing) = roots.resolve(to_root, &files::join_relative(to, &name)) else {
                    return Reply::Json(409, error_json("That name can't be used there."));
                };
                if roots.is_root(to_root, &existing) || roots.holds_blocked(&existing) {
                    return Reply::Json(409, error_json("That can't be replaced."));
                }
                if let Err(error) = files::recycle(&existing) {
                    return Reply::Json(400, error_json(&error));
                }
                shared.log(
                    client,
                    &format!("delete {} (replaced)", files::display_path(&existing)),
                );
            }
        }
    }
    shared.sizes.forget();
    let result = if copy {
        copy_tree(roots, &from, &target).map(|_| ())
    } else {
        move_item(roots, &from, &target)
    };
    match result {
        Ok(()) => {
            shared.log(
                client,
                &format!(
                    "{} {} to {}",
                    if copy { "copy" } else { "move" },
                    files::display_path(&from),
                    files::display_path(&target)
                ),
            );
            Reply::Json(201, saved_json(&target))
        }
        Err(error) => Reply::Json(400, error_json(&error)),
    }
}

/// Moves in one step on one drive. Across drives it copies what a visitor
/// may see and removes the original only when that was everything.
fn move_item(roots: &files::Roots, from: &Path, target: &Path) -> Result<(), String> {
    if std::fs::rename(from, target).is_ok() {
        return Ok(());
    }
    let left_out = copy_tree(roots, from, target)?;
    if left_out > 0 {
        return Err(format!(
            "It was copied, but {left_out} hidden or unreadable item(s) couldn't come along, \
             so the original was kept."
        ));
    }
    files::remove(from)
}

/// Copies a file, or a folder and what a visitor may see in it, to
/// `target`, which doesn't exist yet. Links aren't followed. Returns how
/// many items were left out; on failure, what was made is removed.
fn copy_tree(roots: &files::Roots, from: &Path, target: &Path) -> Result<usize, String> {
    let failed = |error: io::Error| {
        let _ = files::remove(target);
        format!("Couldn't copy it: {error}")
    };
    if !from.is_dir() {
        return copy_file(from, target).map(|()| 0).map_err(failed);
    }
    let mut left_out = 0;
    let mut count = 0;
    let mut pending = vec![(from.to_path_buf(), target.to_path_buf(), 0usize)];
    while let Some((source, made, depth)) = pending.pop() {
        std::fs::create_dir(&made).map_err(failed)?;
        let listing = std::fs::read_dir(&source).map_err(failed)?;
        for entry in listing.flatten() {
            count += 1;
            if count > ZIP_ENTRIES {
                let _ = files::remove(target);
                return Err("That folder holds too many items to copy here.".into());
            }
            let path = entry.path();
            let name = entry.file_name();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                left_out += 1;
                continue;
            };
            let usable = name
                .to_str()
                .is_some_and(|name| files::check_name(name).is_ok())
                && roots.visible(&path)
                && !roots.is_blocked(&path)
                && !metadata.file_type().is_symlink()
                && !files::is_reparse(&metadata);
            if !usable {
                left_out += 1;
            } else if metadata.is_dir() {
                if depth + 1 >= ZIP_DEPTH {
                    left_out += 1;
                } else {
                    pending.push((path, made.join(&name), depth + 1));
                }
            } else if copy_file(&path, &made.join(&name)).is_err() {
                left_out += 1;
            }
        }
    }
    Ok(left_out)
}

/// Copies one file without overwriting anything.
fn copy_file(from: &Path, to: &Path) -> io::Result<()> {
    let mut source = std::fs::File::open(from)?;
    let mut made = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    let copied = io::copy(&mut source, &mut made).and_then(|_| made.flush());
    if copied.is_err() {
        drop(made);
        let _ = std::fs::remove_file(to);
    }
    copied
}

/// The totals of the folders in folder `path` of shared folder `root`, as
/// far as they are known; the page asks again for the rest. With `self=1`,
/// the total of the folder itself.
fn folder_sizes(shared: &Shared, params: &Params) -> Reply {
    let folder = match shared.roots.resolve(params.root(), params.path()) {
        Ok(folder) if folder.is_dir() => folder,
        Ok(_) => return Reply::Json(400, error_json("That is a file, not a folder.")),
        Err(denied) => return denied_json(denied),
    };
    let (names, paths): (Vec<String>, Vec<PathBuf>) = if params.get("self") == Some("1") {
        (vec![String::new()], vec![folder])
    } else {
        let Ok((items, _)) = shared.roots.list(&folder, LIST_LIMIT) else {
            return Reply::Json(404, error_json("That folder can't be opened."));
        };
        items
            .into_iter()
            .filter(|item| item.dir)
            .filter_map(|item| {
                let path = files::join_relative(params.path(), &item.name);
                let real = shared.roots.resolve(params.root(), &path).ok()?;
                Some((item.name, real))
            })
            .unzip()
    };
    let totals = shared.sizes.lookup(&shared.roots, &paths);
    let waiting = totals.iter().filter(|total| total.is_none()).count();
    let found: Vec<String> = names
        .iter()
        .zip(totals)
        .filter_map(|(name, total)| {
            let total = total?;
            Some(format!(
                "{{\"name\":{},\"size\":{},\"items\":{},\"partial\":{}}}",
                quote(name),
                total.bytes,
                total.items,
                total.partial
            ))
        })
        .collect();
    Reply::Json(
        200,
        format!("{{\"sizes\":[{}],\"waiting\":{waiting}}}", found.join(",")),
    )
}

/// An API call without end-to-end encryption.
fn plain_api(
    shared: &Shared,
    conn: &mut Conn,
    request: &Request,
    client: IpAddr,
    token_value: &str,
    session: &auth::Session,
) -> io::Result<bool> {
    if request.method != "GET" {
        let token_ok = request
            .header("x-feather-csrf")
            .is_some_and(|token| auth::same(token.as_bytes(), session.csrf.as_bytes()));
        if !token_ok {
            json(
                conn,
                403,
                &error_json("This request didn't come from Feather's page."),
            )?;
            return Ok(false);
        }
    }
    if (request.method.as_str(), request.path.as_str()) == ("PUT", "/api/upload") {
        if shared.config.http != HttpMode::Full {
            json(conn, 404, &error_json("Not found."))?;
            return Ok(false);
        }
        let params = Params(request.query.clone());
        return upload_plain(shared, conn, request, &params, client);
    }
    let params = Params(request.query.clone());
    match call(
        shared,
        &request.method,
        &request.path,
        &params,
        client,
        &session.csrf,
    ) {
        Reply::Json(status, body) => json(conn, status, &body)?,
        Reply::Logout => {
            shared.sessions.remove(token_value);
            send(
                conn,
                204,
                "text/plain",
                b"",
                &[(
                    "Set-Cookie",
                    format!("{COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0"),
                )],
                false,
            )?;
        }
        Reply::File(path, inline) => {
            // A video is fetched in many pieces; one line for its start.
            let start = request
                .header("range")
                .is_none_or(|range| range.trim().starts_with("bytes=0-"));
            if inline && start {
                shared.log(client, &format!("view {}", files::display_path(&path)));
            }
            return download_plain(conn, request, &path, inline);
        }
        Reply::Thumb(path, side) => return thumb_plain(conn, request, &path, side),
        Reply::Zip(root, relative, names) => {
            return zip_plain(shared, conn, request, client, root, &relative, &names)
        }
    }
    Ok(true)
}

/// A tag for a file's preview: it changes when the file does.
fn thumb_tag(path: &Path, side: u32) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(format!("\"t{modified:x}-{:x}-{side}\"", metadata.len()))
}

/// A preview, cached by the browser for a day and checked with a tag.
fn thumb_plain(conn: &mut Conn, request: &Request, path: &Path, side: u32) -> io::Result<bool> {
    let tag = thumb_tag(path, side).unwrap_or_default();
    let cache = [
        ("Cache-Control", "private, max-age=86400".to_string()),
        ("ETag", tag.clone()),
    ];
    if !tag.is_empty() && request.header("if-none-match") == Some(tag.as_str()) {
        head(conn, 304, "image/jpeg", 0, &cache, false)?;
        conn.wire.flush()?;
        return Ok(true);
    }
    match thumbs::thumbnail(path, side) {
        Some(jpeg) => send(conn, 200, "image/jpeg", &jpeg, &cache, false)?,
        None => json(conn, 404, &error_json("There's no preview of that file."))?,
    }
    Ok(true)
}

/// Searches the index for shared files, with a time limit.
fn search(shared: &Shared, text: &str) -> String {
    let index = shared.index.read().unwrap().clone();
    let query = Query::parse(text);
    let deadline = Instant::now() + SEARCH_TIME;
    let config = &shared.config;
    let mut found = Vec::new();
    let mut truncated = false;
    'volumes: for (volume_index, volume) in index.volumes().iter().enumerate() {
        if query.volume_paths(&volume.prefix).is_none()
            || !shared.roots.roots.iter().any(|root| {
                feather_core::path_within(&root.display, &volume.prefix)
                    || feather_core::path_within(&volume.prefix, &root.display)
            })
        {
            continue;
        }
        let filter = query.volume_filter(volume);
        if !filter.may_match() {
            continue;
        }
        for id in volume.iter_ids() {
            if id & 0x3ff == 0 && Instant::now() > deadline {
                truncated = true;
                break 'volumes;
            }
            let entry = volume.entry_summary(id);
            if (!config.show_hidden && entry.flags & 2 != 0)
                || (!config.show_system && entry.flags & 4 != 0)
                || !query.matches_name(entry.name_search, entry.size, entry.flags)
                || !query.matches_date_of(volume, id)
                || !query.matches_folder_parts(&filter, entry.parent_frn, entry.name_search)
            {
                continue;
            }
            let Some(path) = index.resolve_path(EntryRef::new(volume_index, id)) else {
                continue;
            };
            if !query.matches_path(&path) {
                continue;
            }
            let Some((root, relative)) = shared.roots.locate(&path) else {
                continue;
            };
            let Ok(real) = shared.roots.resolve(root, &relative) else {
                continue;
            };
            let Some(item) = shared.roots.describe(&real) else {
                continue;
            };
            found.push(format!(
                "{{\"root\":{root},\"path\":{},\"name\":{},\"dir\":{},\"size\":{},\"modified\":{}}}",
                quote(&relative),
                quote(&item.name),
                item.dir,
                item.size,
                item.modified
            ));
            if found.len() >= SEARCH_LIMIT {
                truncated = true;
                break 'volumes;
            }
        }
    }
    format!(
        "{{\"items\":[{}],\"truncated\":{truncated}}}",
        found.join(",")
    )
}

/// `bytes=a-b`, `bytes=a-`, or `bytes=-n` for a file of `size` bytes.
fn parse_range(value: &str, size: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = value.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let range = match (start.trim(), end.trim()) {
        ("", suffix) => {
            let suffix: u64 = suffix.parse().ok()?;
            if suffix == 0 {
                return Some(Err(()));
            }
            (size.saturating_sub(suffix), size.checked_sub(1)?)
        }
        (start, "") => (start.parse().ok()?, size.checked_sub(1)?),
        (start, end) => (
            start.parse().ok()?,
            end.parse::<u64>().ok()?.min(size.saturating_sub(1)),
        ),
    };
    if range.0 > range.1 || range.0 >= size {
        return Some(Err(()));
    }
    Some(Ok(range))
}

/// `filename=` for old browsers and `filename*=` with the real name.
fn disposition(name: &str, inline: bool) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = name
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect();
    let how = if inline { "inline" } else { "attachment" };
    format!("{how}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

fn download_plain(
    conn: &mut Conn,
    request: &Request,
    path: &Path,
    inline: bool,
) -> io::Result<bool> {
    // Only pictures and videos are shown in the page; anything else is
    // always saved, so a shared HTML file can't run as this site.
    let media = thumbs::media_type(path).filter(|_| inline);
    let Ok(mut file) = std::fs::File::open(path) else {
        json(conn, 404, &error_json("That file can't be opened."))?;
        return Ok(true);
    };
    let size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".into());
    let mut headers = vec![
        ("Content-Disposition", disposition(&name, media.is_some())),
        ("Accept-Ranges", "bytes".to_string()),
    ];
    if media.is_some() {
        // Flipping back to a picture shouldn't fetch it again.
        headers.push(("Cache-Control", "private, max-age=3600".to_string()));
    }
    let (status, start, length) = match request
        .header("range")
        .and_then(|value| parse_range(value, size))
    {
        Some(Ok((start, end))) => {
            headers.push(("Content-Range", format!("bytes {start}-{end}/{size}")));
            (206, start, end - start + 1)
        }
        Some(Err(())) => {
            headers.push(("Content-Range", format!("bytes */{size}")));
            send(conn, 416, "text/plain", b"", &headers, false)?;
            return Ok(true);
        }
        None => (200, 0, size),
    };
    file.seek(SeekFrom::Start(start))?;
    head(
        conn,
        status,
        media.unwrap_or("application/octet-stream"),
        length,
        &headers,
        false,
    )?;
    let mut left = length;
    let mut chunk = vec![0u8; 64 * 1024];
    while left > 0 {
        let want = (chunk.len() as u64).min(left) as usize;
        let read = file.read(&mut chunk[..want])?;
        if read == 0 {
            // The file shrank; the length promised can't be kept.
            return Ok(false);
        }
        conn.wire.write_all(&chunk[..read])?;
        left -= read as u64;
    }
    conn.wire.flush()?;
    Ok(true)
}

/// A reply body in HTTP/1.1 chunks, so a download cut short shows as
/// failed instead of as a broken zip.
struct Chunked<'a> {
    wire: &'a mut Wire,
    buffer: Vec<u8>,
    /// HTTP/1.0: plain bytes, ended by closing the connection.
    raw: bool,
}

impl Chunked<'_> {
    const SIZE: usize = 64 * 1024;

    fn emit(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        if self.raw {
            self.wire.write_all(&self.buffer)?;
        } else {
            self.wire
                .write_all(format!("{:x}\r\n", self.buffer.len()).as_bytes())?;
            self.wire.write_all(&self.buffer)?;
            self.wire.write_all(b"\r\n")?;
        }
        self.buffer.clear();
        Ok(())
    }

    fn end(mut self) -> io::Result<()> {
        self.emit()?;
        if !self.raw {
            self.wire.write_all(b"0\r\n\r\n")?;
        }
        self.wire.flush()
    }
}

impl Write for Chunked<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.buffer.len() + bytes.len() > Self::SIZE {
            self.emit()?;
        }
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() >= Self::SIZE {
            self.emit()?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// What went into a folder's zip.
#[derive(Default)]
pub(crate) struct Packed {
    pub(crate) files: u64,
    pub(crate) bytes: u64,
    /// Paths in the zip that couldn't be read, or were nested too deep.
    pub(crate) skipped: Vec<String>,
    /// The entry limit was reached.
    pub(crate) truncated: bool,
}

/// A name that is safe as one part of a path inside a zip.
fn zip_part(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':') {
                '_'
            } else {
                c
            }
        })
        .collect();
    match clean.trim() {
        "" | "." | ".." => "folder".into(),
        _ => clean,
    }
}

fn modified_secs(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|time| time.as_secs())
        .unwrap_or(0)
}

/// The zip's name for folder `relative` of shared folder `root`.
fn zip_top(shared: &Shared, root: usize, relative: &str) -> String {
    let name = relative
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .or_else(|| shared.roots.roots.get(root).map(|root| root.name.as_str()))
        .unwrap_or("Shared");
    zip_part(name)
}

fn shared_dir(roots: &files::Roots, root: usize, relative: &str) -> Result<PathBuf, Denied> {
    roots.resolve(root, relative).and_then(|real| {
        if real.is_dir() {
            Ok(real)
        } else {
            Err(Denied::NotFound)
        }
    })
}

/// Writes folder `relative` of shared folder `root`, and everything in it a
/// visitor may see, into `zip` under `top/`. Every file and folder goes
/// through `Roots::resolve`, the same check as a single download: names,
/// hidden and system items, left-out folders, and links or junctions that
/// lead outside the shared folder are all left out, silently, as a listing
/// leaves them out. Each folder is read once, so a link back up can't loop.
/// Files that can't be read are left out and named in a note at the end.
pub(crate) fn pack_folder<W: Write>(
    roots: &files::Roots,
    root: usize,
    relative: &str,
    only: &[String],
    top: &str,
    zip: &mut ZipWriter<W>,
    stop: &dyn Fn() -> bool,
) -> io::Result<Packed> {
    let mut packed = Packed::default();
    let mut seen = std::collections::HashSet::new();
    let relative = relative.trim_matches(['/', '\\']).to_string();
    // Folders still to read: path in the share, path in the zip, depth.
    let mut pending = vec![(relative, top.to_string(), 0usize)];
    while let Some((folder, inside, depth)) = pending.pop() {
        if stop() {
            return Ok(packed);
        }
        let Ok(real) = shared_dir(roots, root, &folder) else {
            continue;
        };
        if !seen.insert(real.clone()) {
            continue;
        }
        if zip.len() >= ZIP_ENTRIES {
            packed.truncated = true;
            break;
        }
        let modified = std::fs::metadata(&real)
            .map(|metadata| modified_secs(&metadata))
            .unwrap_or(0);
        zip.add_dir(&inside, modified)?;
        let Ok(listing) = std::fs::read_dir(&real) else {
            packed.skipped.push(format!("{inside}/"));
            continue;
        };
        let mut names: Vec<String> = listing
            .flatten()
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .filter(|name| files::check_name(name).is_ok())
            .collect();
        names.sort_by_key(|name| name.to_lowercase());
        let mut folders = Vec::new();
        for name in names {
            if depth == 0 && !only.is_empty() && !only.contains(&name) {
                continue;
            }
            let path = if folder.is_empty() {
                name.clone()
            } else {
                format!("{folder}/{name}")
            };
            let in_zip = format!("{inside}/{}", zip_part(&name));
            // Anything a visitor couldn't open alone isn't packed, or named.
            let Ok(real) = roots.resolve(root, &path) else {
                continue;
            };
            let Ok(metadata) = std::fs::metadata(&real) else {
                continue;
            };
            if metadata.is_dir() {
                if depth + 1 >= ZIP_DEPTH {
                    packed.skipped.push(format!("{in_zip}/"));
                } else {
                    folders.push((path, in_zip, depth + 1));
                }
                continue;
            }
            if zip.len() >= ZIP_ENTRIES {
                packed.truncated = true;
                break;
            }
            if stop() {
                return Ok(packed);
            }
            let Ok(mut file) = std::fs::File::open(&real) else {
                packed.skipped.push(in_zip);
                continue;
            };
            let before = zip.written();
            let whole =
                zip.add_file(&in_zip, modified_secs(&metadata), metadata.len(), &mut file)?;
            packed.files += 1;
            packed.bytes += zip.written() - before;
            if !whole {
                packed.skipped.push(format!("{in_zip} (incomplete)"));
            }
        }
        if packed.truncated {
            break;
        }
        // The stack takes the last first, so push in reverse name order.
        pending.extend(folders.into_iter().rev());
    }
    if !packed.skipped.is_empty() || packed.truncated {
        let note = skipped_note(&packed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|time| time.as_secs())
            .unwrap_or(0);
        zip.add_file(
            &format!("{top}/Feather - not included.txt"),
            now,
            note.len() as u64,
            &mut note.as_bytes(),
        )?;
    }
    Ok(packed)
}

fn skipped_note(packed: &Packed) -> String {
    let mut note =
        String::from("Feather couldn't put everything from this folder in the zip.\r\n\r\n");
    if packed.truncated {
        note.push_str(&format!(
            "The zip stopped at {ZIP_ENTRIES} files and folders. Download the folders \
             inside one at a time for the rest.\r\n\r\n"
        ));
    }
    if !packed.skipped.is_empty() {
        note.push_str("These couldn't be read, or were nested too deep:\r\n");
        for path in packed.skipped.iter().take(1000) {
            note.push_str(path);
            note.push_str("\r\n");
        }
        if packed.skipped.len() > 1000 {
            note.push_str(&format!("...and {} more\r\n", packed.skipped.len() - 1000));
        }
    }
    note
}

/// A folder as a zip, streamed: nothing is gathered first, in memory or on
/// disk, so the download starts at once and can be any size.
fn zip_plain(
    shared: &Shared,
    conn: &mut Conn,
    request: &Request,
    client: IpAddr,
    root: usize,
    relative: &str,
    only: &[String],
) -> io::Result<bool> {
    let top = zip_top(shared, root, relative);
    let disposition: String = disposition(&format!("{top}.zip"), false)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let mut text = format!("HTTP/1.1 200 OK\r\n{SECURITY_HEADERS}");
    text.push_str("Content-Type: application/zip\r\nCache-Control: no-store\r\n");
    text.push_str(&format!("Content-Disposition: {disposition}\r\n"));
    text.push_str(if request.http10 {
        "Connection: close\r\n\r\n"
    } else {
        "Transfer-Encoding: chunked\r\nConnection: keep-alive\r\nKeep-Alive: timeout=15\r\n\r\n"
    });
    conn.wire.write_all(text.as_bytes())?;
    let mut body = Chunked {
        wire: &mut conn.wire,
        buffer: Vec::with_capacity(Chunked::SIZE),
        raw: request.http10,
    };
    let mut zip = ZipWriter::new(&mut body);
    let stop = || shared.stopping();
    let Ok(packed) = pack_folder(&shared.roots, root, relative, only, &top, &mut zip, &stop) else {
        // The visitor left; the download shows as failed.
        return Ok(false);
    };
    if shared.stopping() || zip.finish().is_err() || body.end().is_err() {
        return Ok(false);
    }
    let mut summary = format!(
        "zip of {top} done: {} files, {} bytes",
        packed.files, packed.bytes
    );
    if !packed.skipped.is_empty() {
        summary.push_str(&format!(", {} left out", packed.skipped.len()));
    }
    if packed.truncated {
        summary.push_str(", stopped at the entry limit");
    }
    shared.log(client, &summary);
    Ok(!request.http10)
}

fn denied_reply(denied: Denied) -> (u16, String) {
    match denied {
        Denied::BadName => (400, error_json("That name isn't allowed.")),
        Denied::NotFound => (404, error_json("Not found.")),
    }
}

/// What to do when something with the same name is already there.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Conflict {
    /// Say so (409) and let the visitor choose.
    Ask,
    Replace,
    /// Use a free name like `name (2).txt`.
    KeepBoth,
}

impl Conflict {
    fn of(params: &Params) -> Self {
        match (params.get("conflict"), params.get("replace")) {
            (Some("replace"), _) | (None, Some("1")) => Conflict::Replace,
            (Some("keep"), _) => Conflict::KeepBoth,
            _ => Conflict::Ask,
        }
    }
}

/// The 409 reply that lets the page ask Replace, Keep both, or Skip.
fn conflict_json(name: &str, folder: bool) -> String {
    format!(
        "{{\"error\":{},\"conflict\":true,\"name\":{},\"dir\":{folder}}}",
        quote(&format!("{name} is already there.")),
        quote(name)
    )
}

/// Makes the folders of `sub` (like `Trip/Day 1`) inside folder `path` of
/// shared folder `root`, as an uploaded folder had them. Each name is
/// checked like any other, and a name taken by something a visitor can't
/// see, or by a file, is refused. Returns the path the files go in.
fn make_folders(
    shared: &Shared,
    root: usize,
    path: &str,
    sub: &str,
) -> Result<String, (u16, String)> {
    let mut at = path.trim_matches('/').to_string();
    for (depth, part) in sub
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .enumerate()
    {
        files::check_name(part).map_err(denied_reply)?;
        if depth >= 32 {
            return Err((400, error_json("Those folders are nested too deep.")));
        }
        let next = if at.is_empty() {
            part.to_string()
        } else {
            format!("{at}/{part}")
        };
        match shared.roots.resolve(root, &next) {
            Ok(folder) if folder.is_dir() => {}
            Ok(_) => {
                return Err((
                    409,
                    error_json(&format!(
                        "A file called {part} is in the way of that folder."
                    )),
                ))
            }
            Err(_) => {
                let target = shared
                    .roots
                    .resolve_new(root, &at, part)
                    .map_err(denied_reply)?;
                // Taken by something hidden or left out: not ours to touch.
                if std::fs::symlink_metadata(&target).is_ok() {
                    return Err(denied_reply(Denied::NotFound));
                }
                std::fs::create_dir(&target).map_err(|error| {
                    (
                        400,
                        error_json(&format!("Couldn't create the folder {part}: {error}")),
                    )
                })?;
            }
        }
        at = next;
    }
    Ok(at)
}

/// Where an upload goes, checked before any of it is read.
fn upload_target(shared: &Shared, params: &Params) -> Result<PathBuf, (u16, String)> {
    let name = params.get("name").unwrap_or("");
    files::check_name(name).map_err(denied_reply)?;
    let root = params.root();
    // The place must be a folder a visitor may use before anything is made in it.
    match shared.roots.resolve(root, params.path()) {
        Ok(folder) if folder.is_dir() => {}
        Ok(_) => return Err(denied_reply(Denied::NotFound)),
        Err(denied) => return Err(denied_reply(denied)),
    }
    let folder = match params.get("sub").filter(|sub| !sub.is_empty()) {
        Some(sub) => make_folders(shared, root, params.path(), sub)?,
        None => params.path().to_string(),
    };
    let mut target = shared
        .roots
        .resolve_new(root, &folder, name)
        .map_err(denied_reply)?;
    let taken = std::fs::symlink_metadata(&target).is_ok();
    match Conflict::of(params) {
        _ if !taken => {}
        Conflict::KeepBoth => {
            target =
                files::free_name(&target).ok_or((409, error_json("No free name was found.")))?;
        }
        Conflict::Replace if target.is_file() => {}
        Conflict::Replace => {
            return Err((409, error_json("A folder with that name is already there.")));
        }
        Conflict::Ask => return Err((409, conflict_json(name, target.is_dir()))),
    }
    if target.parent().is_none() {
        return Err((400, error_json("That folder can't be used.")));
    }
    Ok(target)
}

/// The reply to an upload: the name it was saved under.
fn saved_json(target: &Path) -> String {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{{\"name\":{}}}", quote(&name))
}

/// Writes beside the target, then moves it into place in one step.
fn receive(
    target: &Path,
    fill: &mut dyn FnMut(&mut std::fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let folder = target.parent().unwrap_or(Path::new("."));
    let temporary = folder.join(format!(".feather-upload-{}.part", auth::token(8)));
    let result = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        fill(&mut file)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn upload_plain(
    shared: &Shared,
    conn: &mut Conn,
    request: &Request,
    params: &Params,
    client: IpAddr,
) -> io::Result<bool> {
    if request.header("content-length").is_none() {
        send(conn, 411, "text/plain", b"Length required", &[], true)?;
        return Ok(false);
    }
    if request.length > MAX_UPLOAD {
        json(conn, 413, &error_json("That file is too large."))?;
        return Ok(false);
    }
    let target = match upload_target(shared, params) {
        Ok(target) => target,
        Err((status, body)) => {
            json(conn, status, &body)?;
            return Ok(false);
        }
    };
    let length = request.length;
    match receive(&target, &mut |file| {
        conn.read_body(length, &mut |bytes| file.write_all(bytes))
    }) {
        Ok(()) => {
            shared.sizes.forget();
            shared.log(client, &format!("upload {}", files::display_path(&target)));
            json(conn, 201, &saved_json(&target))?;
            Ok(true)
        }
        Err(error) => {
            let _ = json(
                conn,
                400,
                &error_json(&format!("The upload failed: {error}")),
            );
            Ok(false)
        }
    }
}

// ----- Sealed requests ----------------------------------------------------

pub(super) fn cipher(key: &[u8; 32]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).expect("a 32-byte key"))
}

/// `nonce || ciphertext || tag`.
pub(super) fn seal(key: &LessSafeKey, aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let nonce: [u8; 12] = auth::random_bytes();
    let mut data = plain.to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(aad),
        &mut data,
    )
    .expect("sealing");
    let mut out = nonce.to_vec();
    out.extend_from_slice(&data);
    out
}

/// Opens `nonce || ciphertext || tag`; the nonce and the plaintext.
pub(super) fn open(key: &LessSafeKey, aad: &[u8], sealed: &[u8]) -> Option<([u8; 12], Vec<u8>)> {
    if sealed.len() < 12 + 16 {
        return None;
    }
    let nonce: [u8; 12] = sealed[..12].try_into().ok()?;
    let mut data = sealed[12..].to_vec();
    let plain = key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut data,
        )
        .ok()?
        .to_vec();
    Some((nonce, plain))
}

/// Reads a sealed request: its nonce (refusing replays) and parameters.
fn open_request(
    shared: &Shared,
    key: &LessSafeKey,
    token_value: &str,
    label: &str,
    sealed: &[u8],
) -> Option<([u8; 12], Params)> {
    let (nonce, plain) = open(key, label.as_bytes(), sealed)?;
    if !shared.sessions.fresh_nonce(token_value, nonce) {
        return None;
    }
    let form = parse_form(std::str::from_utf8(&plain).ok()?)?;
    Some((nonce, Params(form)))
}

/// One sealed API call: `op=/api/list&root=0&path=...`, encrypted.
fn sealed_call(
    shared: &Shared,
    conn: &mut Conn,
    request: &Request,
    client: IpAddr,
    token_value: &str,
    key: &[u8; 32],
) -> io::Result<bool> {
    if request.length > MAX_FORM {
        send(conn, 413, "text/plain", b"Too large", &[], true)?;
        return Ok(false);
    }
    let body = conn.read_small_body(request.length)?;
    let key = cipher(key);
    let Some((nonce, params)) = open_request(shared, &key, token_value, "feather request", &body)
    else {
        json(conn, 400, &error_json("That request couldn't be read."))?;
        return Ok(true);
    };
    let op = params.get("op").unwrap_or("").to_string();
    let method = if matches!(
        op.as_str(),
        "/api/info" | "/api/list" | "/api/search" | "/api/sizes"
    ) {
        "GET"
    } else {
        "POST"
    };
    let (status, reply) = match call(shared, method, &op, &params, client, "") {
        Reply::Json(status, body) => (status, body),
        Reply::Logout => {
            shared.sessions.remove(token_value);
            (200, "{}".into())
        }
        Reply::File(..) | Reply::Thumb(..) | Reply::Zip(..) => {
            (400, error_json("Use the download call."))
        }
    };
    let mut aad = b"feather reply".to_vec();
    aad.extend_from_slice(&nonce);
    let sealed = seal(&key, &aad, format!("{status}\n{reply}").as_bytes());
    send(conn, 200, "application/octet-stream", &sealed, &[], false)?;
    Ok(true)
}

/// The additional data of frame `index` of a sealed transfer.
pub(super) fn frame_aad(label: &str, request_nonce: &[u8; 12], index: u64) -> Vec<u8> {
    let mut aad = label.as_bytes().to_vec();
    aad.extend_from_slice(request_nonce);
    aad.extend_from_slice(&index.to_be_bytes());
    aad
}

/// A file, sealed in frames: `[length][nonce][last flag + data + tag]`.
/// Each frame is bound to its place, so frames can't be dropped, reordered,
/// or cut short without the page noticing.
fn sealed_download(
    shared: &Shared,
    conn: &mut Conn,
    request: &Request,
    client: IpAddr,
    token_value: &str,
    key: &[u8; 32],
) -> io::Result<bool> {
    if request.length > MAX_FORM {
        send(conn, 413, "text/plain", b"Too large", &[], true)?;
        return Ok(false);
    }
    let body = conn.read_small_body(request.length)?;
    let key = cipher(key);
    let Some((nonce, params)) = open_request(shared, &key, token_value, "feather download", &body)
    else {
        json(conn, 400, &error_json("That request couldn't be read."))?;
        return Ok(true);
    };
    let op = if params.get("thumb") == Some("1") {
        "/api/thumb"
    } else {
        "/api/download"
    };
    let (mut file, size): (Box<dyn Read>, u64) = match call(shared, "GET", op, &params, client, "")
    {
        Reply::File(path, _) => {
            let Ok(file) = std::fs::File::open(&path) else {
                json(conn, 404, &error_json("That file can't be opened."))?;
                return Ok(true);
            };
            let size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            (Box::new(file), size)
        }
        Reply::Thumb(path, side) => match thumbs::thumbnail(&path, side) {
            Some(jpeg) => {
                let size = jpeg.len() as u64;
                (Box::new(std::io::Cursor::new(jpeg)), size)
            }
            None => {
                json(conn, 404, &error_json("There's no preview of that file."))?;
                return Ok(true);
            }
        },
        Reply::Json(status, body) => {
            json(conn, status, &body)?;
            return Ok(true);
        }
        Reply::Zip(..) => {
            json(conn, 403, &error_json(SEALED_ZIP))?;
            return Ok(true);
        }
        Reply::Logout => return Ok(false),
    };
    let frames = size.div_ceil(FRAME as u64).max(1);
    head(
        conn,
        200,
        "application/octet-stream",
        size + frames * FRAME_OVERHEAD,
        &[],
        false,
    )?;
    let mut chunk = vec![0u8; FRAME];
    for index in 0..frames {
        let want = ((size - index * FRAME as u64) as usize).min(FRAME);
        if file.read_exact(&mut chunk[..want]).is_err() {
            return Ok(false);
        }
        let last = index + 1 == frames;
        let mut plain = Vec::with_capacity(want + 1);
        plain.push(u8::from(last));
        plain.extend_from_slice(&chunk[..want]);
        let sealed = seal(&key, &frame_aad("feather download", &nonce, index), &plain);
        conn.wire.write_all(&(sealed.len() as u32).to_be_bytes())?;
        conn.wire.write_all(&sealed)?;
    }
    conn.wire.flush()?;
    Ok(true)
}

/// An upload sealed the same way, its name and place sealed in a header.
fn sealed_upload(
    shared: &Shared,
    conn: &mut Conn,
    request: &Request,
    client: IpAddr,
    token_value: &str,
    key: &[u8; 32],
) -> io::Result<bool> {
    if shared.config.http != HttpMode::Full {
        json(conn, 404, &error_json("Not found."))?;
        return Ok(false);
    }
    if request.header("content-length").is_none() || request.length > MAX_UPLOAD + MAX_UPLOAD / 64 {
        json(conn, 413, &error_json("That file is too large."))?;
        return Ok(false);
    }
    let key = cipher(key);
    let header = request
        .header("x-feather-sealed")
        .and_then(auth::unhex)
        .unwrap_or_default();
    let Some((nonce, params)) = open_request(shared, &key, token_value, "feather upload", &header)
    else {
        json(conn, 400, &error_json("That request couldn't be read."))?;
        return Ok(false);
    };
    let target = match upload_target(shared, &params) {
        Ok(target) => target,
        Err((status, body)) => {
            json(conn, status, &body)?;
            return Ok(false);
        }
    };
    let mut left = request.length;
    let result = receive(&target, &mut |file| {
        let mut index = 0u64;
        loop {
            if left < 4 {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let mut length = [0u8; 4];
            conn.read_exact_body(&mut length)?;
            let length = u32::from_be_bytes(length) as usize;
            if length > FRAME + 64 || (length as u64) + 4 > left {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let mut sealed = vec![0u8; length];
            conn.read_exact_body(&mut sealed)?;
            left -= 4 + length as u64;
            let (_, plain) = open(&key, &frame_aad("feather upload", &nonce, index), &sealed)
                .ok_or(io::ErrorKind::InvalidData)?;
            let (&last, data) = plain.split_first().ok_or(io::ErrorKind::InvalidData)?;
            file.write_all(data)?;
            index += 1;
            if last == 1 {
                return if left == 0 {
                    Ok(())
                } else {
                    Err(io::ErrorKind::InvalidData.into())
                };
            }
        }
    });
    match result {
        Ok(()) => {
            shared.sizes.forget();
            shared.log(client, &format!("upload {}", files::display_path(&target)));
            json(conn, 201, &saved_json(&target))?;
            Ok(true)
        }
        Err(error) => {
            let _ = json(
                conn,
                400,
                &error_json(&format!("The upload failed: {error}")),
            );
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_heads_are_parsed_strictly() {
        let ok = parse(b"GET /api/list?root=0&path=a%20b HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(ok.path, "/api/list");
        assert_eq!(Params(ok.query.clone()).get("path"), Some("a b"));
        assert_eq!(ok.header("host"), Some("x"));
        assert!(parse(b"GET /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n").is_err());
        assert!(
            parse(b"GET /x HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n").is_err()
        );
        assert!(parse(b"GET /x HTTP/1.1\r\n folded: yes\r\n\r\n").is_err());
        assert!(parse(b"get /x HTTP/1.1\r\n\r\n").is_err());
        assert!(parse(b"GET x HTTP/1.1\r\n\r\n").is_err());
        assert!(parse(b"GET /x?a=%zz HTTP/1.1\r\n\r\n").is_err());
        assert!(parse(b"GET /x?a=%00 HTTP/1.1\r\n\r\n").is_err());
        assert!(parse(b"GET /x HTTP/2.0\r\n\r\n").is_err());
    }

    #[test]
    fn ranges_and_names() {
        assert_eq!(parse_range("bytes=0-9", 100), Some(Ok((0, 9))));
        assert_eq!(parse_range("bytes=90-", 100), Some(Ok((90, 99))));
        assert_eq!(parse_range("bytes=-10", 100), Some(Ok((90, 99))));
        assert_eq!(parse_range("bytes=100-", 100), Some(Err(())));
        assert_eq!(parse_range("bytes=0-1,4-5", 100), None);
        assert_eq!(parse_range("items=0-1", 100), None);
        let header = disposition("a\"b é.txt", false);
        assert!(header.contains("filename=\"a_b _.txt\""));
        assert!(header.contains("filename*=UTF-8''a%22b%20%C3%A9.txt"));
        assert_eq!(quote("<a>\"\n"), "\"\\u003ca\\u003e\\\"\\u000a\"");
    }

    #[test]
    fn gzip_is_valid() {
        let text = "hello hello hello hello ".repeat(100);
        let packed = gzip(text.as_bytes());
        assert!(packed.len() < text.len() / 4);
        let mut decoder = flate2_free_inflate(&packed[10..packed.len() - 8]);
        decoder.truncate(text.len());
        assert_eq!(decoder, text.as_bytes());
    }

    fn flate2_free_inflate(data: &[u8]) -> Vec<u8> {
        miniz_oxide::inflate::decompress_to_vec(data).unwrap()
    }

    #[test]
    fn sealed_messages_round_trip_and_bind_their_context() {
        let key = cipher(&[9; 32]);
        let sealed = seal(&key, b"feather request", b"op=/api/info");
        let (_, plain) = open(&key, b"feather request", &sealed).unwrap();
        assert_eq!(plain, b"op=/api/info");
        assert!(open(&key, b"feather reply", &sealed).is_none());
        let mut tampered = sealed.clone();
        tampered[20] ^= 1;
        assert!(open(&key, b"feather request", &tampered).is_none());
    }
}
