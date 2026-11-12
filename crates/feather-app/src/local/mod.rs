//! Feather's window on Linux (and anywhere without the Win32 window): the
//! app runs as a small server on 127.0.0.1 and shows itself in a browser
//! window. That needs no GUI toolkit, so it works on every distribution,
//! desktop, and window manager, on X11 and Wayland alike.
//!
//! Only this computer can reach it, and only with the random key each run
//! makes: the browser gets it once in the address it opens and keeps it in
//! a cookie. The Host header must be the loopback address, so a web page
//! can't reach it through DNS rebinding, and changes must come from the page
//! itself.

pub mod desktop;

use std::io::{self, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use feather_core::scan::ScanOptions;
use feather_core::{EntryRef, IndexSet, Query};

use crate::filters::{Category, FilterSet};
use crate::library;
use crate::settings::{self, AutoUpdate, DarkMode, Settings};
use crate::share::auth;
use crate::share::http::{
    error_json, json, parse, parse_form, quote, redirect, send, static_file, Conn, Params, Request,
    Wire,
};

const LOCAL_HTML: &str = include_str!("web/local.html");
const LOCAL_JS: &str = include_str!("web/local.js");
const LOCAL_CSS: &str = include_str!("web/local.css");
const ICON_SVG: &str = include_str!("../share/web/icon.svg");
const COOKIE: &str = "feather_local";
const PAGE: usize = 200;
/// Searches stop counting here, so a one-letter search stays quick.
const COUNT_LIMIT: usize = 5_000_000;

struct App {
    settings: Mutex<Settings>,
    index: RwLock<Arc<IndexSet>>,
    library: PathBuf,
    token: String,
    port: u16,
    indexing: AtomicBool,
    cancel: Arc<AtomicBool>,
    last_build: Mutex<Option<Instant>>,
    notice: Mutex<String>,
    stop: AtomicBool,
}

/// Where a running Feather leaves its port and key for the next launch.
fn state_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("feather").join("window.state"))
}

fn write_private(path: &Path, text: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(text.as_bytes())
}

/// The running Feather's port and key, when one answers.
fn running() -> Option<(u16, String)> {
    let text = std::fs::read_to_string(state_file()?).ok()?;
    let (port, token) = text.trim().split_once(' ')?;
    let port: u16 = port.parse().ok()?;
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        Duration::from_secs(1),
    )
    .ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(
        stream,
        "GET /api/ping HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Feather-Key: {token}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut reply = String::new();
    io::Read::read_to_string(&mut stream, &mut reply).ok()?;
    reply
        .starts_with("HTTP/1.1 200")
        .then(|| (port, token.to_string()))
}

fn address(port: u16, token: &str) -> String {
    format!("http://127.0.0.1:{port}/?key={token}")
}

/// Asks a running Feather to quit. True when one was running.
pub fn quit() -> bool {
    let Some((port, token)) = running() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)) else {
        return false;
    };
    let _ = write!(
        stream,
        "POST /api/quit HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Feather-Key: {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    true
}

/// Shows Feather: opens the window of the copy already running, or starts
/// one. With `background`, starts without opening a window.
pub fn run(index_dir: PathBuf, background: bool) -> Result<(), String> {
    if let Some((port, token)) = running() {
        if !background {
            desktop::open_browser(&address(port, &token))?;
        }
        return Ok(());
    }
    library::apply_pending(&index_dir);
    let active = library::active_library(&index_dir);
    let index = IndexSet::open(&active).unwrap_or_else(|_| IndexSet::empty());
    let loaded = settings::load();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|error| format!("Couldn't open Feather's window: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    let token = auth::token(32);
    let app = Arc::new(App {
        settings: Mutex::new(loaded.settings),
        index: RwLock::new(Arc::new(index)),
        library: index_dir,
        token: token.clone(),
        port,
        indexing: AtomicBool::new(false),
        cancel: Arc::new(AtomicBool::new(false)),
        last_build: Mutex::new(None),
        notice: Mutex::new(String::new()),
        stop: AtomicBool::new(false),
    });
    if let Some(path) = state_file() {
        let _ = write_private(&path, &format!("{port} {token}"));
    }
    // Catch up on what changed while Feather was closed.
    if !app.settings.lock().unwrap().index_roots.is_empty() {
        start_reindex(&app, &[]);
    }
    let keeper = Arc::clone(&app);
    let _ = std::thread::Builder::new()
        .name("feather-refresh".into())
        .spawn(move || keep_fresh(&keeper));
    if !background {
        let _ = desktop::open_browser(&address(port, &token));
    }
    serve(&app, listener);
    if let Some(path) = state_file() {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

fn serve(app: &Arc<App>, listener: TcpListener) {
    let _ = listener.set_nonblocking(true);
    while !app.stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) if peer.ip().is_loopback() => {
                let app = Arc::clone(app);
                let _ = std::thread::Builder::new()
                    .name("feather-window-client".into())
                    .spawn(move || {
                        let _ = connection(&app, stream);
                    });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
    app.cancel.store(true, Ordering::SeqCst);
}

fn connection(app: &Arc<App>, stream: TcpStream) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let _ = stream.set_nodelay(true);
    let mut conn = Conn {
        wire: Wire::Plain(stream),
        buffer: Vec::new(),
        gzip: false,
    };
    for _ in 0..1000 {
        let Ok(Some(raw)) = conn.read_head() else {
            break;
        };
        let Ok(request) = parse(&raw) else {
            break;
        };
        conn.gzip = request
            .header("accept-encoding")
            .is_some_and(|value| value.contains("gzip"));
        match handle(app, &mut conn, &request) {
            Ok(true) if !request.close && !app.stop.load(Ordering::SeqCst) => {}
            _ => break,
        }
    }
    Ok(())
}

/// Only the loopback name, so a page elsewhere can't reach this through a
/// hostname that points at 127.0.0.1.
fn right_host(app: &App, request: &Request) -> bool {
    let port = app.port;
    request.header("host").is_some_and(|host| {
        host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
    })
}

fn signed_in(app: &App, request: &Request) -> bool {
    let key = request
        .cookie(COOKIE)
        .or_else(|| request.header("x-feather-key").map(str::to_string));
    key.is_some_and(|key| auth::same(key.as_bytes(), app.token.as_bytes()))
}

fn from_page(app: &App, request: &Request) -> bool {
    if request.header("x-feather-key").is_some() {
        return true;
    }
    if let Some(site) = request.header("sec-fetch-site") {
        return site == "same-origin";
    }
    let port = app.port;
    request.header("origin").is_none_or(|origin| {
        origin == format!("http://127.0.0.1:{port}") || origin == format!("http://localhost:{port}")
    })
}

fn handle(app: &Arc<App>, conn: &mut Conn, request: &Request) -> io::Result<bool> {
    if !right_host(app, request) {
        send(conn, 403, "text/plain", b"Forbidden", &[], true)?;
        return Ok(false);
    }
    if request.length > 64 * 1024 {
        send(conn, 413, "text/plain", b"Too large", &[], true)?;
        return Ok(false);
    }
    let body = conn.read_small_body(request.length)?;
    let query = Params(request.query.clone());
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/local.js") => {
            static_file(conn, request, "text/javascript; charset=utf-8", LOCAL_JS)
        }
        ("GET", "/local.css") => static_file(conn, request, "text/css; charset=utf-8", LOCAL_CSS),
        ("GET", "/icon.svg" | "/favicon.ico") => {
            static_file(conn, request, "image/svg+xml", ICON_SVG)
        }
        ("GET", "/") if query.get("key").is_some() => {
            // The key arrives once in the address; from then on, a cookie.
            if query
                .get("key")
                .is_some_and(|key| auth::same(key.as_bytes(), app.token.as_bytes()))
            {
                redirect(
                    conn,
                    "/",
                    &[(
                        "Set-Cookie",
                        format!("{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict", app.token),
                    )],
                )?;
            } else {
                send(
                    conn,
                    403,
                    "text/plain",
                    b"That key is out of date. Start Feather again.",
                    &[],
                    false,
                )?;
            }
            Ok(true)
        }
        ("GET", "/") if signed_in(app, request) => {
            send(
                conn,
                200,
                "text/html; charset=utf-8",
                LOCAL_HTML.as_bytes(),
                &[],
                false,
            )?;
            Ok(true)
        }
        ("GET", "/") => {
            send(
                conn,
                403,
                "text/plain; charset=utf-8",
                "Open Feather from your applications menu, or run `feather`.".as_bytes(),
                &[],
                false,
            )?;
            Ok(true)
        }
        _ if !signed_in(app, request) => {
            json(
                conn,
                401,
                &error_json("Start Feather again to open its window."),
            )?;
            Ok(true)
        }
        (method, _) if method != "GET" && !from_page(app, request) => {
            json(
                conn,
                403,
                &error_json("That request didn't come from Feather's window."),
            )?;
            Ok(false)
        }
        ("GET", "/api/ping") => {
            send(conn, 200, "text/plain", b"ok", &[], false)?;
            Ok(true)
        }
        ("GET", "/api/status") => {
            json(conn, 200, &status(app))?;
            Ok(true)
        }
        ("GET", "/api/search") => {
            json(conn, 200, &search(app, &query))?;
            Ok(true)
        }
        ("GET", "/api/settings") => {
            json(conn, 200, &settings_json(&app.settings.lock().unwrap()))?;
            Ok(true)
        }
        ("POST", path) => {
            let form = Params(
                std::str::from_utf8(&body)
                    .ok()
                    .and_then(parse_form)
                    .unwrap_or_default(),
            );
            let (code, reply) = action(app, path, &form);
            json(conn, code, &reply)?;
            Ok(true)
        }
        _ => {
            json(conn, 404, &error_json("Not found."))?;
            Ok(true)
        }
    }
}

fn action(app: &Arc<App>, path: &str, form: &Params) -> (u16, String) {
    match path {
        "/api/open" | "/api/reveal" => {
            let target = form.get("path").unwrap_or("");
            if !indexed(app, target) {
                return (404, error_json("That isn't something Feather found."));
            }
            let result = if path == "/api/open" {
                desktop::open_path(Path::new(target))
            } else {
                desktop::reveal(Path::new(target))
            };
            match result {
                Ok(()) => (200, "{}".into()),
                Err(error) => (400, error_json(&error)),
            }
        }
        "/api/settings" => save_settings(app, form),
        "/api/reindex" => {
            start_reindex(app, &[]);
            (200, "{}".into())
        }
        "/api/quit" => {
            app.stop.store(true, Ordering::SeqCst);
            (200, "{}".into())
        }
        _ => (404, error_json("Not found.")),
    }
}

/// True for a path inside a place Feather indexes.
fn indexed(app: &App, path: &str) -> bool {
    !path.is_empty()
        && Path::new(path).exists()
        && app
            .settings
            .lock()
            .unwrap()
            .index_roots
            .iter()
            .any(|root| feather_core::path_within(path, root))
}

fn status(app: &App) -> String {
    let settings = app.settings.lock().unwrap();
    let roots: Vec<String> = settings
        .index_roots
        .iter()
        .map(|root| quote(root))
        .collect();
    format!(
        "{{\"items\":{},\"indexing\":{},\"notice\":{},\"roots\":[{}],\"home\":{},\"theme\":\"{}\",\"sizes\":\"{}\"}}",
        app.index.read().unwrap().item_count(),
        app.indexing.load(Ordering::SeqCst),
        quote(&app.notice.lock().unwrap()),
        roots.join(","),
        quote(&desktop::home().unwrap_or_default()),
        theme_name(settings.dark_mode),
        settings.size_format.as_str(),
    )
}

fn theme_name(mode: DarkMode) -> &'static str {
    match mode {
        DarkMode::Light => "light",
        DarkMode::Full => "dark",
        DarkMode::FollowSystem => "system",
    }
}

fn settings_json(settings: &Settings) -> String {
    let roots: Vec<String> = settings
        .index_roots
        .iter()
        .map(|root| quote(root))
        .collect();
    let excluded: Vec<String> = settings
        .excluded_folders
        .iter()
        .map(|folder| quote(folder))
        .collect();
    format!(
        "{{\"roots\":[{}],\"excluded\":[{}],\"auto_update\":\"{}\",\"show_hidden\":{},\"show_system\":{},\"theme\":\"{}\",\"sizes\":\"{}\",\"autostart\":{},\"menu\":{}}}",
        roots.join(","),
        excluded.join(","),
        settings.auto_update.as_str(),
        settings.show_hidden,
        settings.show_system,
        theme_name(settings.dark_mode),
        settings.size_format.as_str(),
        desktop::autostart_enabled(),
        desktop::menu_entry_exists(),
    )
}

fn lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn save_settings(app: &Arc<App>, form: &Params) -> (u16, String) {
    let mut settings = app.settings.lock().unwrap().clone();
    let old_roots = settings.index_roots.clone();
    if let Some(roots) = form.get("roots") {
        let roots = lines(roots);
        if let Some(missing) = roots.iter().find(|root| !Path::new(root.as_str()).is_dir()) {
            return (400, error_json(&format!("{missing} isn't a folder.")));
        }
        settings.index_roots = roots;
    }
    if let Some(excluded) = form.get("excluded") {
        settings.excluded_folders = lines(excluded);
    }
    if let Some(mode) = form.get("auto_update").and_then(AutoUpdate::parse) {
        settings.auto_update = mode;
    }
    if let Some(value) = form.get("show_hidden") {
        settings.show_hidden = value == "1";
    }
    if let Some(value) = form.get("show_system") {
        settings.show_system = value == "1";
    }
    if let Some(theme) = form.get("theme") {
        settings.dark_mode = match theme {
            "light" => DarkMode::Light,
            "dark" => DarkMode::Full,
            _ => DarkMode::FollowSystem,
        };
    }
    if let Some(format) = form.get("sizes").and_then(settings::SizeFormat::parse) {
        settings.size_format = format;
    }
    if let Some(value) = form.get("autostart") {
        if let Err(error) = desktop::set_autostart(value == "1") {
            return (400, error_json(&error));
        }
    }
    if let Some(value) = form.get("menu") {
        if let Err(error) = desktop::set_menu_entry(value == "1") {
            return (400, error_json(&error));
        }
    }
    settings.setup_done = true;
    settings.normalize();
    if let Err(error) = settings.save() {
        return (
            400,
            error_json(&format!("Couldn't save the settings: {error}")),
        );
    }
    let roots_changed = settings.index_roots != old_roots;
    *app.settings.lock().unwrap() = settings;
    if roots_changed {
        start_reindex(app, &[]);
    }
    (200, "{}".into())
}

/// Rebuilds the index in the background, unless that is already going on.
fn start_reindex(app: &Arc<App>, reuse: &[String]) {
    if app.indexing.swap(true, Ordering::SeqCst) {
        return;
    }
    let settings = app.settings.lock().unwrap().clone();
    let library = app.library.clone();
    let cancel = Arc::clone(&app.cancel);
    let reuse = reuse.to_vec();
    let app = Arc::clone(app);
    let _ = std::thread::Builder::new()
        .name("feather-indexer".into())
        .spawn(move || {
            *app.notice.lock().unwrap() = "Reading your files...".into();
            let mut excluded_paths = settings.scan_excluded_paths.clone();
            if let Some(parent) = library.parent() {
                excluded_paths.push(parent.to_string_lossy().to_string());
            }
            let options = ScanOptions {
                excluded_paths,
                excluded_names: settings.scan_excluded_names.clone(),
                ..ScanOptions::default()
            };
            let result =
                library::build_walk(&library, &settings.index_roots, &reuse, &options, &cancel);
            let notice = match result {
                Ok((staged, failures)) => match IndexSet::open(&staged) {
                    Ok(set) => {
                        let _ = library::activate(&library, &staged);
                        *app.index.write().unwrap() = Arc::new(set);
                        library::cleanup(&library);
                        if failures.is_empty() {
                            String::new()
                        } else {
                            format!("Couldn't read {}", failures.join(", "))
                        }
                    }
                    Err(error) => format!("Couldn't open the new index: {error}"),
                },
                Err(error) => format!("Indexing failed: {error}"),
            };
            *app.notice.lock().unwrap() = notice;
            *app.last_build.lock().unwrap() = Some(Instant::now());
            app.indexing.store(false, Ordering::SeqCst);
        });
}

/// Rebuilds now and then, as "Update the index" says. Without a file
/// watcher here, "right away" means every 15 minutes.
fn keep_fresh(app: &Arc<App>) {
    while !app.stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_secs(30));
        let wait = match app.settings.lock().unwrap().auto_update {
            AutoUpdate::Off => continue,
            AutoUpdate::Hourly => Duration::from_secs(60 * 60),
            _ => Duration::from_secs(15 * 60),
        };
        let due = app
            .last_build
            .lock()
            .unwrap()
            .is_none_or(|last| last.elapsed() >= wait);
        if due && !app.settings.lock().unwrap().index_roots.is_empty() {
            start_reindex(app, &[]);
        }
    }
}

/// Seconds since 1970 from a Windows file time, as the index stores them.
fn unix_seconds(filetime: u64) -> u64 {
    (filetime / 10_000_000).saturating_sub(11_644_473_600)
}

fn parent_of(path: &str) -> String {
    match path.rfind(['/', '\\']) {
        Some(0) => "/".into(),
        Some(at) => path[..at].to_string(),
        None => String::new(),
    }
}

/// One page of results: `offset` and `limit` rows of everything that
/// matches, plus how many match in all.
fn search(app: &App, params: &Params) -> String {
    let settings = app.settings.lock().unwrap().clone();
    let index = app.index.read().unwrap().clone();
    let mut filters = FilterSet::default();
    if let Some(category) = params.get("category").and_then(Category::parse) {
        category.apply(&mut filters);
    }
    let text = params.get("q").unwrap_or("").trim().to_string();
    let query = Query::parse(&filters.compose(&text));
    let offset: usize = params
        .get("offset")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let limit: usize = params
        .get("limit")
        .and_then(|value| value.parse().ok())
        .unwrap_or(PAGE)
        .min(1000);
    let cap = settings.result_cap(index.item_count()).min(COUNT_LIMIT);
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut total = 0usize;
    let mut rows = Vec::new();
    let mut capped = false;
    'volumes: for (volume_index, volume) in index.volumes().iter().enumerate() {
        let Some(scoped) = query.volume_paths(&volume.prefix) else {
            continue;
        };
        let filter = query.volume_filter(volume);
        if !filter.may_match() {
            continue;
        }
        let mut paths = feather_core::PathCheck::new(&query);
        for id in volume.iter_ids() {
            if id & 0xfff == 0 && Instant::now() > deadline {
                capped = true;
                break 'volumes;
            }
            let entry = volume.entry_summary(id);
            if (!settings.show_hidden && entry.flags & 2 != 0)
                || (!settings.show_system && entry.flags & 4 != 0)
                || !query.matches_name(entry.name_search, entry.size, entry.flags)
                || !query.matches_date_of(volume, id)
                || !query.matches_folder_parts(&filter, entry.parent_frn, entry.name_search)
            {
                continue;
            }
            let reference = EntryRef::new(volume_index, id);
            let in_page = total >= offset && rows.len() < limit;
            let excluded = !settings.excluded_folders.is_empty();
            if scoped
                && !excluded
                && !paths.matches(volume, entry.parent_frn, entry.name_search, || {
                    index.resolve_path(reference)
                })
            {
                continue;
            }
            if excluded || in_page {
                let Some(path) = index.resolve_path(reference) else {
                    continue;
                };
                if (scoped && !query.matches_path(&path))
                    || settings
                        .excluded_folders
                        .iter()
                        .any(|folder| feather_core::path_within(&path, folder))
                {
                    continue;
                }
                if in_page {
                    let view = index.entry(reference);
                    rows.push(format!(
                        "{{\"path\":{},\"name\":{},\"folder\":{},\"dir\":{},\"size\":{},\"modified\":{}}}",
                        quote(&path),
                        quote(&String::from_utf16_lossy(view.name_display)),
                        quote(&parent_of(&path)),
                        view.is_dir(),
                        view.size,
                        unix_seconds(view.mtime)
                    ));
                }
            }
            total += 1;
            if total >= cap {
                capped = true;
                break 'volumes;
            }
        }
    }
    format!(
        "{{\"total\":{total},\"capped\":{capped},\"items\":[{}]}}",
        rows.join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_and_folders() {
        assert_eq!(unix_seconds(133_000_000_000_000_000), 1_655_526_400);
        assert_eq!(parent_of("/home/me/a.txt"), "/home/me");
        assert_eq!(parent_of("/a.txt"), "/");
        assert_eq!(parent_of(r"C:\Users\a.txt"), r"C:\Users");
        assert_eq!(
            lines(" a \n\n b\r\n"),
            vec!["a".to_string(), "b".to_string()]
        );
    }
}
