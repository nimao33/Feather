//! Sharing the indexed folders with other devices: a web page over HTTPS
//! and an FTP server over TLS, both behind one password.
//!
//! Security, in short:
//! - Everything is off until the person turns it on, picks folders, and
//!   sets a password of at least ten characters (stored as an Argon2 hash).
//! - Only the chosen folders are served, minus the "leave out" list and
//!   Feather's own data; every visitor path is checked name by name and
//!   resolved on disk (see `files`).
//! - "This PC" listens on 127.0.0.1 only; "Local network" refuses any
//!   address that isn't private; the internet goes through a Cloudflare or
//!   ngrok tunnel to 127.0.0.1, or directly when chosen.
//! - The web server speaks only HTTPS, with strict headers, HttpOnly
//!   SameSite cookies, a CSRF token on every change, and request limits.
//! - Wrong passwords lock an address out for longer each time.
//! - "Full access" deletes to the Recycle Bin, never permanently.

pub mod auth;
pub mod files;
pub mod firewall;
mod ftp;
pub(crate) mod http;
pub mod secret;
pub mod thumbs;
pub mod tls;
pub mod tunnel;
mod zip;

use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;

use feather_core::IndexSet;

use crate::settings::{FtpMode, HttpMode, Settings, ShareAccess, Tunnel};

/// Everything the servers need from Settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub http: HttpMode,
    pub ftp: FtpMode,
    pub access: ShareAccess,
    pub tunnel: Tunnel,
    pub http_port: u16,
    pub ftp_port: u16,
    pub user: String,
    pub password_hash: String,
    pub folders: Vec<String>,
    pub excluded: Vec<String>,
    pub show_hidden: bool,
    pub show_system: bool,
    pub ftp_plain: bool,
    /// The decrypted tunnel token.
    pub tunnel_token: Hidden,
    /// The password itself, for the end-to-end key; empty when unknown.
    pub password: Hidden,
    /// Requests and replies are encrypted in the page itself.
    pub e2e: bool,
    /// ngrok's free static domain, for an address that never changes.
    pub tunnel_domain: String,
}

/// A secret that never shows up in logs or debug output.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Hidden(pub String);

impl std::fmt::Debug for Hidden {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(if self.0.is_empty() { "\"\"" } else { "\"***\"" })
    }
}

impl Config {
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            http: settings.http_mode,
            ftp: settings.ftp_mode,
            access: settings.share_access,
            tunnel: settings.tunnel,
            http_port: settings.http_port,
            ftp_port: settings.ftp_port,
            user: settings.share_user.clone(),
            password_hash: settings.share_password.clone(),
            folders: settings.share_folders.clone(),
            excluded: settings.excluded_folders.clone(),
            show_hidden: settings.show_hidden,
            show_system: settings.show_system,
            ftp_plain: settings.ftp_plain,
            tunnel_token: Hidden(secret::open(&settings.tunnel_token)),
            password: Hidden(secret::open(&settings.share_password_saved)),
            e2e: settings.share_e2e,
            tunnel_domain: settings.tunnel_domain.clone(),
        }
    }

    pub fn wanted(&self) -> bool {
        self.http != HttpMode::Off || self.ftp != FtpMode::Off
    }

    /// Why sharing can't start, if it can't.
    /// No password: anyone who can connect gets in.
    pub fn open(&self) -> bool {
        self.password_hash.is_empty()
    }

    pub fn problem(&self) -> Option<String> {
        if self.folders.is_empty() {
            return Some("Choose folders to share in Settings first.".into());
        }
        if self.e2e && self.http != HttpMode::Off && !self.open() && self.password.0.is_empty() {
            return Some(
                "Type the sharing password again in Settings; end-to-end encryption needs it."
                    .into(),
            );
        }
        None
    }

    /// The web server is behind a tunnel, so it only listens locally.
    fn tunnelled(&self) -> bool {
        self.access == ShareAccess::Internet && self.tunnel != Tunnel::Direct
    }
}

/// State both servers share.
pub(crate) struct Shared {
    pub config: Config,
    pub roots: files::Roots,
    pub guard: auth::Guard,
    pub sessions: auth::Sessions,
    pub tls: Arc<rustls::ServerConfig>,
    pub index: RwLock<Arc<IndexSet>>,
    pub stop: AtomicBool,
    pub connections: AtomicUsize,
    log: Mutex<Option<std::fs::File>>,
    /// The end-to-end key and its salt, when that is on.
    pub e2e: Option<([u8; 32], [u8; 16])>,
    pub handshakes: auth::Handshakes,
    /// With no password, the one form token every visitor gets.
    pub open_csrf: String,
    /// Serves plain HTTP instead of HTTPS. Only tests set it, to drive the
    /// page from a browser on 127.0.0.1.
    pub plain: bool,
    /// The addresses people can open, for the page to remember.
    pub status: Arc<Mutex<Status>>,
}

impl Shared {
    /// Notes one request or sign-in in `sharing.log`, without secrets.
    pub fn log(&self, address: IpAddr, what: &str) {
        let mut file = self.log.lock().unwrap();
        if let Some(file) = file.as_mut() {
            let _ = writeln!(file, "{} {address} {what}", timestamp());
        }
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

/// What the person sees about sharing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// Addresses people can open, best first.
    pub addresses: Vec<String>,
    /// Problems worth showing.
    pub problems: Vec<String>,
    /// The certificate fingerprint to compare in the browser.
    pub fingerprint: String,
}

impl Status {
    /// One line for the window's status area.
    pub fn summary(&self) -> String {
        match (self.addresses.first(), self.problems.first()) {
            (Some(address), _) => format!("Sharing at {address}"),
            (None, Some(problem)) => format!("Sharing: {problem}"),
            (None, None) => "Sharing is starting...".into(),
        }
    }
}

/// The running servers. Dropping it stops them.
pub struct Sharing {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    tunnel: Option<tunnel::Tunnel>,
    status: Arc<Mutex<Status>>,
}

impl Sharing {
    /// Starts the servers `config` asks for. Problems that stop a server are
    /// in `status().problems`.
    pub fn start(config: Config, index: Arc<IndexSet>) -> Self {
        let folder = tls::folder().unwrap_or_else(|| std::env::temp_dir().join("feather-sharing"));
        Self::start_in(config, index, &folder)
    }

    /// Like `start`, keeping the certificate and log in `folder`.
    pub fn start_in(config: Config, index: Arc<IndexSet>, folder: &std::path::Path) -> Self {
        Self::start_with(config, index, folder, false)
    }

    fn start_with(
        config: Config,
        index: Arc<IndexSet>,
        folder: &std::path::Path,
        plain_for_tests: bool,
    ) -> Self {
        let status = Arc::new(Mutex::new(Status::default()));
        let identity = tls::Identity::load_or_create(folder);
        let tls = identity.as_ref().ok().and_then(|identity| {
            identity
                .server_config()
                .map_err(|error| status.lock().unwrap().problems.push(error))
                .ok()
        });
        if let Err(error) = &identity {
            status.lock().unwrap().problems.push(error.clone());
        }
        let (roots, errors) = files::Roots::new(
            &config.folders,
            &config.excluded,
            config.show_hidden,
            config.show_system,
        );
        status.lock().unwrap().problems.extend(errors);
        let log = open_log(folder);
        let fallback = Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("default TLS versions")
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(NoCertificate)),
        );
        let have_tls = tls.is_some();
        // PBKDF2 once here, so sign-ins don't wait for it.
        let e2e = (config.e2e && !config.open() && !config.password.0.is_empty()).then(|| {
            let salt = auth::e2e_salt();
            (auth::e2e_key(&config.password.0, &salt), salt)
        });
        let shared = Arc::new(Shared {
            config: config.clone(),
            roots,
            guard: auth::Guard::default(),
            sessions: auth::Sessions::default(),
            tls: tls.unwrap_or(fallback),
            index: RwLock::new(index),
            stop: AtomicBool::new(false),
            connections: AtomicUsize::new(0),
            log: Mutex::new(log),
            e2e,
            handshakes: auth::Handshakes::default(),
            open_csrf: auth::token(24),
            plain: plain_for_tests,
            status: Arc::clone(&status),
        });
        let mut sharing = Self {
            shared: Arc::clone(&shared),
            threads: Vec::new(),
            tunnel: None,
            status: Arc::clone(&status),
        };
        if let Ok(identity) = &identity {
            status.lock().unwrap().fingerprint = identity.fingerprint();
        }
        if let Some(problem) = config.problem() {
            status.lock().unwrap().problems.insert(0, problem);
            return sharing;
        }
        if config.open() {
            status
                .lock()
                .unwrap()
                .problems
                .push("There's no password: anyone who can reach these addresses gets in.".into());
        }
        if !have_tls {
            return sharing;
        }
        if shared.roots.roots.is_empty() {
            return sharing;
        }
        let host = lan_address()
            .map(|address| address.to_string())
            .unwrap_or_else(|| "localhost".into());

        if config.http != HttpMode::Off {
            // Behind a tunnel the page still answers on the local network,
            // so phones at home reach it directly; only the internet goes
            // through the tunnel.
            let bind = if config.access == ShareAccess::Local {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            } else {
                IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            };
            match TcpListener::bind(SocketAddr::new(bind, config.http_port)) {
                Ok(listener) => {
                    let local = if bind.is_loopback() || config.access == ShareAccess::Local {
                        "localhost".to_string()
                    } else {
                        host.clone()
                    };
                    status
                        .lock()
                        .unwrap()
                        .addresses
                        .push(format!("https://{local}:{}", config.http_port));
                    sharing
                        .threads
                        .push(http::spawn(Arc::clone(&shared), listener));
                    if config.tunnelled() {
                        match tunnel::Tunnel::start(
                            config.tunnel,
                            config.http_port,
                            &config.tunnel_token.0,
                            &config.tunnel_domain,
                            Arc::clone(&status),
                        ) {
                            Ok(tunnel) => sharing.tunnel = Some(tunnel),
                            Err(error) => status.lock().unwrap().problems.push(error),
                        }
                    }
                }
                Err(error) => status.lock().unwrap().problems.push(format!(
                    "The web server could not use port {}: {error}",
                    config.http_port
                )),
            }
        }

        if config.ftp != FtpMode::Off {
            // FTP can't travel through the tunnels, so on "Internet" with a
            // tunnel it stays on the local network.
            let bind = match config.access {
                ShareAccess::Local => IpAddr::V4(Ipv4Addr::LOCALHOST),
                _ => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            };
            match TcpListener::bind(SocketAddr::new(bind, config.ftp_port)) {
                Ok(listener) => {
                    let local = if config.access == ShareAccess::Local {
                        "localhost".to_string()
                    } else {
                        host.clone()
                    };
                    let scheme = if config.ftp_plain { "ftp" } else { "ftps" };
                    status
                        .lock()
                        .unwrap()
                        .addresses
                        .push(format!("{scheme}://{local}:{}", config.ftp_port));
                    sharing
                        .threads
                        .push(ftp::spawn(Arc::clone(&shared), listener));
                    if config.tunnelled() {
                        status.lock().unwrap().problems.push(
                            "FTP stays on the local network; tunnels carry the web page only."
                                .into(),
                        );
                    }
                }
                Err(error) => status.lock().unwrap().problems.push(format!(
                    "The FTP server could not use port {}: {error}",
                    config.ftp_port
                )),
            }
        }
        sharing
    }

    /// Searches go to the newest index.
    pub fn set_index(&self, index: Arc<IndexSet>) {
        *self.shared.index.write().unwrap() = index;
    }

    pub fn status(&self) -> Status {
        let mut status = self.status.lock().unwrap().clone();
        // A public address from a tunnel goes first.
        status.addresses.sort_by_key(|address| {
            !address.contains("trycloudflare") && !address.contains("ngrok")
        });
        status
    }

    pub fn config(&self) -> &Config {
        &self.shared.config
    }
}

impl Drop for Sharing {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(tunnel) = self.tunnel.take() {
            drop(tunnel);
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// Without a certificate nothing can be served; the servers aren't started.
#[derive(Debug)]
struct NoCertificate;

impl rustls::server::ResolvesServerCert for NoCertificate {
    fn resolve(
        &self,
        _hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        None
    }
}

/// Whether `access` lets `address` connect at all.
pub(crate) fn allowed(access: ShareAccess, tunnel: Tunnel, address: IpAddr) -> bool {
    match access {
        ShareAccess::Local => address.is_loopback(),
        ShareAccess::Network => is_private(address),
        ShareAccess::Internet if tunnel == Tunnel::Direct => true,
        // Behind a tunnel the web server listens on 127.0.0.1 only; FTP
        // still serves the local network.
        ShareAccess::Internet => is_private(address),
    }
}

/// Loopback, private, link-local, and carrier-grade (Tailscale) addresses.
fn is_private(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

/// This computer's address on the local network. Nothing is sent: each
/// probe only asks which address a packet would leave from. A VPN such as
/// Cloudflare WARP carries traffic to the internet while the home network
/// stays on Wi-Fi, so home-network ranges win over the internet route.
pub fn lan_address() -> Option<IpAddr> {
    let probe = |target: &str| -> Option<IpAddr> {
        let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect(target).ok()?;
        let address = socket.local_addr().ok()?.ip();
        (!address.is_unspecified() && !address.is_loopback()).then_some(address)
    };
    let rank = |address: &IpAddr| match address {
        IpAddr::V4(v4) => match v4.octets() {
            [192, 168, ..] => 0,
            [10, ..] => 1,
            [172, second, ..] if (16..32).contains(&second) => 2,
            _ => 3,
        },
        IpAddr::V6(_) => 4,
    };
    [
        "192.168.1.1:9",
        "192.168.0.1:9",
        "10.0.0.1:9",
        "192.0.2.1:9",
    ]
    .iter()
    .filter_map(|target| probe(target))
    .min_by_key(rank)
}

fn open_log(folder: &std::path::Path) -> Option<std::fs::File> {
    std::fs::create_dir_all(folder).ok()?;
    let path: PathBuf = folder.join("sharing.log");
    // Keep the log small: start over past 5 MB.
    if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > 5 * 1024 * 1024) {
        let _ = std::fs::rename(&path, folder.join("sharing.old.log"));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

/// The time now in UTC, like `2026-11-10 08:15:02`.
pub(crate) fn timestamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| time.as_secs())
        .unwrap_or(0);
    let (year, month, day) = civil(seconds / 86_400);
    let rest = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// Days since 1970 as a calendar date (Howard Hinnant's algorithm).
pub(crate) fn civil(days: u64) -> (i64, u32, u32) {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    /// Trusts exactly one certificate: the one the test server made.
    #[derive(Debug)]
    struct Pinned(Vec<u8>, Arc<rustls::crypto::CryptoProvider>);

    impl rustls::client::danger::ServerCertVerifier for Pinned {
        fn verify_server_cert(
            &self,
            end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _name: &rustls::pki_types::ServerName<'_>,
            _ocsp: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            if end_entity.as_ref() == self.0.as_slice() {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            } else {
                Err(rustls::Error::General("not the pinned certificate".into()))
            }
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.1.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.1.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.1.signature_verification_algorithms.supported_schemes()
        }
    }

    fn client(cert: &[u8]) -> Arc<rustls::ClientConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pinned(cert.to_vec(), provider)))
            .with_no_client_auth();
        Arc::new(config)
    }

    fn tls(
        config: &Arc<rustls::ClientConfig>,
        stream: TcpStream,
    ) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let connection = rustls::ClientConnection::new(Arc::clone(config), name).unwrap();
        rustls::StreamOwned::new(connection, stream)
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// One HTTPS request; returns the status, the head, and the body.
    fn https(
        config: &Arc<rustls::ClientConfig>,
        port: u16,
        request: &str,
        body: &[u8],
    ) -> (u16, String, Vec<u8>) {
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut stream = tls(config, stream);
        stream.write_all(request.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        let split = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("a response head");
        let head = String::from_utf8_lossy(&response[..split]).to_string();
        let status = head[9..12].parse().unwrap();
        (status, head, response[split + 4..].to_vec())
    }

    fn sample_share(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("feather-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let shared = base.join("shared");
        std::fs::create_dir_all(shared.join("docs")).unwrap();
        std::fs::write(shared.join("docs").join("a.txt"), b"hello").unwrap();
        (base.clone(), shared, base.join("state"))
    }

    fn sign_in_form(host: &str, form: &str, extra: &str) -> String {
        format!(
            "POST /login HTTP/1.1\r\n{host}{extra}Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{form}",
            form.len()
        )
    }

    #[test]
    fn without_a_password_everyone_gets_in_but_changes_still_need_the_page() {
        let (base, shared, state) = sample_share("open");
        let port = free_port();
        let mut config = Config::from_settings(&Settings::default());
        config.http = HttpMode::Full;
        config.access = ShareAccess::Local;
        config.http_port = port;
        config.folders = vec![shared.to_string_lossy().to_string()];
        let sharing = Sharing::start_in(config, Arc::new(IndexSet::empty()), &state);
        assert!(sharing
            .status()
            .problems
            .iter()
            .any(|line| line.contains("no password")));
        let tls_config = client(&std::fs::read(state.join("server.cer")).unwrap());
        let host = format!("Host: localhost:{port}\r\nConnection: close\r\n");
        let get = |path: &str| format!("GET {path} HTTP/1.1\r\n{host}\r\n");
        let (code, _, _) = https(&tls_config, port, &get("/"), b"");
        assert_eq!(code, 200);
        let (code, _, body) = https(&tls_config, port, &get("/api/info"), b"");
        assert_eq!(code, 200);
        let info = String::from_utf8(body).unwrap();
        assert!(info.contains("\"user\":\"\""));
        let (code, _, _) = https(&tls_config, port, &get("/api/list?root=0&path=docs"), b"");
        assert_eq!(code, 200);
        // A change without the page's token is still refused.
        let (code, _, _) = https(
            &tls_config,
            port,
            &format!("POST /api/mkdir?root=0&path=&name=x HTTP/1.1\r\n{host}\r\n"),
            b"",
        );
        assert_eq!(code, 403);
        drop(sharing);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn browsers_that_hide_the_origin_can_still_sign_in() {
        let (base, shared, state) = sample_share("origin");
        let port = free_port();
        let mut config = Config::from_settings(&Settings::default());
        config.http = HttpMode::Download;
        config.access = ShareAccess::Local;
        config.http_port = port;
        config.user = "tester".into();
        config.password_hash = auth::hash_password("abcd").unwrap();
        config.folders = vec![shared.to_string_lossy().to_string()];
        let sharing = Sharing::start_in(config, Arc::new(IndexSet::empty()), &state);
        let tls_config = client(&std::fs::read(state.join("server.cer")).unwrap());
        let host = format!("Host: localhost:{port}\r\nConnection: close\r\n");
        let form = "user=tester&password=abcd";
        // What Chrome sends for a form on a page with a strict referrer
        // policy: Origin "null", but Sec-Fetch-Site says it's this site.
        let (code, _, _) = https(
            &tls_config,
            port,
            &sign_in_form(
                &host,
                form,
                "Origin: null\r\nSec-Fetch-Site: same-origin\r\n",
            ),
            b"",
        );
        assert_eq!(code, 303);
        let (code, _, _) = https(
            &tls_config,
            port,
            &sign_in_form(
                &host,
                form,
                &format!("Origin: https://localhost:{port}\r\n"),
            ),
            b"",
        );
        assert_eq!(code, 303);
        // Another site can't sign people in.
        let (code, _, _) = https(
            &tls_config,
            port,
            &sign_in_form(
                &host,
                form,
                "Origin: https://evil.example\r\nSec-Fetch-Site: cross-site\r\n",
            ),
            b"",
        );
        assert_eq!(code, 403);
        // Static files are cached and checked by tag.
        let (code, head, _) = https(
            &tls_config,
            port,
            &format!("GET /app.js HTTP/1.1\r\n{host}Accept-Encoding: gzip\r\n\r\n"),
            b"",
        );
        assert_eq!(code, 200);
        assert!(head.contains("Content-Encoding: gzip"));
        let tag = head
            .lines()
            .find_map(|line| line.strip_prefix("ETag: "))
            .unwrap()
            .to_string();
        let (code, _, body) = https(
            &tls_config,
            port,
            &format!("GET /app.js HTTP/1.1\r\n{host}If-None-Match: {tag}\r\n\r\n"),
            b"",
        );
        assert_eq!((code, body.len()), (304, 0));
        drop(sharing);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The installed app's files and its ping are there for anyone, and the
    /// ping says only that this is Feather.
    #[test]
    fn the_installed_app_can_find_feather_again() {
        let (base, shared, state) = sample_share("pwa");
        let port = free_port();
        let mut config = Config::from_settings(&Settings::default());
        config.http = HttpMode::Download;
        config.access = ShareAccess::Local;
        config.http_port = port;
        config.user = "tester".into();
        config.password_hash = auth::hash_password("abcd").unwrap();
        config.folders = vec![shared.to_string_lossy().to_string()];
        let sharing = Sharing::start_in(config, Arc::new(IndexSet::empty()), &state);
        let tls_config = client(&std::fs::read(state.join("server.cer")).unwrap());
        let host = format!("Host: localhost:{port}\r\nConnection: close\r\n");
        let (code, head, body) = https(
            &tls_config,
            port,
            &format!("GET /api/ping HTTP/1.1\r\n{host}Origin: https://other.example\r\n\r\n"),
            b"",
        );
        assert_eq!(code, 200);
        assert!(head.contains("Access-Control-Allow-Origin: *"));
        assert!(!head.contains("Set-Cookie"));
        assert_eq!(body, b"{\"app\":\"feather\"}");
        let (code, head, _) = https(
            &tls_config,
            port,
            &format!(
                "OPTIONS /api/ping HTTP/1.1\r\n{host}Origin: https://other.example\r\n\
                 Access-Control-Request-Method: GET\r\nAccess-Control-Request-Private-Network: true\r\n\r\n"
            ),
            b"",
        );
        assert_eq!(code, 204);
        assert!(head.contains("Access-Control-Allow-Private-Network: true"));
        // Nothing else answers other sites.
        let (code, head, _) = https(
            &tls_config,
            port,
            &format!("GET /api/info HTTP/1.1\r\n{host}Origin: https://other.example\r\n\r\n"),
            b"",
        );
        assert_eq!(code, 401);
        assert!(!head.contains("Access-Control-Allow-Origin"));
        for (path, kind) in [
            ("/manifest.webmanifest", "application/manifest+json"),
            ("/sw.js", "text/javascript"),
            ("/reconnect", "text/html"),
            ("/reconnect.js", "text/javascript"),
            ("/icon-192.png", "image/png"),
            ("/icon-512.png", "image/png"),
            ("/icon-maskable.png", "image/png"),
            ("/apple-touch-icon.png", "image/png"),
        ] {
            let (code, head, body) = https(
                &tls_config,
                port,
                &format!("GET {path} HTTP/1.1\r\n{host}\r\n"),
                b"",
            );
            assert_eq!(code, 200, "{path}");
            assert!(
                head.contains(&format!("Content-Type: {kind}")),
                "{path}: {head}"
            );
            assert!(!body.is_empty(), "{path}");
        }
        drop(sharing);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn end_to_end_encryption_never_sends_the_password() {
        use super::http::{cipher, frame_aad, open, seal};
        let (base, shared, state) = sample_share("sealed");
        let port = free_port();
        let mut config = Config::from_settings(&Settings::default());
        config.http = HttpMode::Full;
        config.access = ShareAccess::Local;
        config.http_port = port;
        config.user = "tester".into();
        config.password_hash = auth::hash_password("abcd").unwrap();
        config.password = Hidden("abcd".into());
        config.e2e = true;
        config.folders = vec![shared.to_string_lossy().to_string()];
        let sharing = Sharing::start_in(config, Arc::new(IndexSet::empty()), &state);
        assert!(
            sharing.status().problems.is_empty(),
            "{:?}",
            sharing.status().problems
        );
        let tls_config = client(&std::fs::read(state.join("server.cer")).unwrap());
        let host = format!("Host: localhost:{port}\r\nConnection: close\r\n");

        // The plain form is refused: the password must not travel.
        let (code, _, _) = https(
            &tls_config,
            port,
            &sign_in_form(&host, "user=tester&password=abcd", ""),
            b"",
        );
        assert_eq!(code, 400);

        let (_, _, hello) = https(
            &tls_config,
            port,
            &format!("GET /e2e/hello HTTP/1.1\r\n{host}\r\n"),
            b"",
        );
        let hello = String::from_utf8(hello).unwrap();
        let field = |name: &str| {
            hello
                .split(&format!("\"{name}\":"))
                .nth(1)
                .unwrap()
                .trim_start_matches('"')
                .split(['"', ',', '}'])
                .next()
                .unwrap()
                .to_string()
        };
        let salt = auth::unhex(&field("salt")).unwrap();
        let server_nonce = auth::unhex(&field("nonce")).unwrap();
        // The browser's side: derive, prove.
        let key = auth::e2e_key("abcd", &salt);
        let client_nonce = [5u8; 16];
        let proof = auth::proof(
            &key,
            "feather login",
            &[&server_nonce, &client_nonce, b"tester"],
        );
        let body = format!(
            "id={}&user=tester&client={}&proof={}",
            field("id"),
            auth::hex(&client_nonce),
            auth::hex(&proof)
        );
        let (code, head, answer) = https(
            &tls_config,
            port,
            &format!(
                "POST /e2e/login HTTP/1.1\r\n{host}Content-Length: {}\r\n\r\n",
                body.len()
            ),
            body.as_bytes(),
        );
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&answer));
        let expected = auth::proof(&key, "feather server", &[&server_nonce, &client_nonce]);
        assert!(String::from_utf8_lossy(&answer).contains(&auth::hex(&expected)));
        let cookie = head
            .lines()
            .find_map(|line| line.strip_prefix("Set-Cookie: "))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        assert!(cookie.starts_with("__Host-feather="));
        let signed = format!("{host}Cookie: {cookie}\r\n");
        let session = cipher(&auth::session_key(&key, &server_nonce, &client_nonce));

        // Plain calls are refused once signed in end to end.
        let (code, _, _) = https(
            &tls_config,
            port,
            &format!("GET /api/list?root=0 HTTP/1.1\r\n{signed}\r\n"),
            b"",
        );
        assert_eq!(code, 403);
        // Folders aren't zipped end to end, and the page is told why.
        let (code, _, body) = https(
            &tls_config,
            port,
            &format!("GET /api/zip?root=0&path=docs HTTP/1.1\r\n{signed}\r\n"),
            b"",
        );
        assert_eq!(code, 403);
        assert!(String::from_utf8_lossy(&body).contains("encrypted end to end"));

        // A sealed listing.
        let request = seal(&session, b"feather request", b"op=/api/list&root=0&path=");
        let (code, _, reply) = https(
            &tls_config,
            port,
            &format!(
                "POST /api/sealed HTTP/1.1\r\n{signed}Content-Length: {}\r\n\r\n",
                request.len()
            ),
            &request,
        );
        assert_eq!(code, 200);
        let mut aad = b"feather reply".to_vec();
        aad.extend_from_slice(&request[..12]);
        let (_, plain) = open(&session, &aad, &reply).expect("a sealed reply");
        let plain = String::from_utf8(plain).unwrap();
        assert!(
            plain.starts_with("200\n") && plain.contains("\"name\":\"docs\""),
            "{plain}"
        );
        assert!(
            !String::from_utf8_lossy(&reply).contains("docs"),
            "names never show"
        );

        // Sending it again is a replay and is refused.
        let (_, _, again) = https(
            &tls_config,
            port,
            &format!(
                "POST /api/sealed HTTP/1.1\r\n{signed}Content-Length: {}\r\n\r\n",
                request.len()
            ),
            &request,
        );
        assert!(String::from_utf8_lossy(&again).contains("couldn't be read"));

        // A sealed download, frame by frame.
        let request = seal(&session, b"feather download", b"root=0&path=docs/a.txt");
        let (code, _, frames) = https(
            &tls_config,
            port,
            &format!(
                "POST /api/sealed-download HTTP/1.1\r\n{signed}Content-Length: {}\r\n\r\n",
                request.len()
            ),
            &request,
        );
        assert_eq!(code, 200);
        let nonce: [u8; 12] = request[..12].try_into().unwrap();
        let length = u32::from_be_bytes(frames[..4].try_into().unwrap()) as usize;
        let (_, frame) = open(
            &session,
            &frame_aad("feather download", &nonce, 0),
            &frames[4..4 + length],
        )
        .unwrap();
        assert_eq!(frame, b"\x01hello");

        // A sealed upload.
        let meta = seal(
            &session,
            b"feather upload",
            b"root=0&path=docs&name=b.txt&replace=0",
        );
        let meta_nonce: [u8; 12] = meta[..12].try_into().unwrap();
        let frame = seal(
            &session,
            &frame_aad("feather upload", &meta_nonce, 0),
            b"\x01uploaded",
        );
        let mut body = (frame.len() as u32).to_be_bytes().to_vec();
        body.extend_from_slice(&frame);
        let (code, _, _) = https(
            &tls_config,
            port,
            &format!(
                "PUT /api/sealed-upload HTTP/1.1\r\n{signed}X-Feather-Sealed: {}\r\nContent-Length: {}\r\n\r\n",
                auth::hex(&meta),
                body.len()
            ),
            &body,
        );
        assert_eq!(code, 201);
        assert_eq!(
            std::fs::read(shared.join("docs").join("b.txt")).unwrap(),
            b"uploaded"
        );

        // A wrong password's proof fails.
        let (_, _, hello) = https(
            &tls_config,
            port,
            &format!("GET /e2e/hello HTTP/1.1\r\n{host}\r\n"),
            b"",
        );
        let hello = String::from_utf8(hello).unwrap();
        let id = hello
            .split("\"id\":\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let nonce = auth::unhex(
            hello
                .split("\"nonce\":\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap(),
        )
        .unwrap();
        let wrong = auth::proof(
            &auth::e2e_key("abce", &salt),
            "feather login",
            &[&nonce, &client_nonce, b"tester"],
        );
        let body = format!(
            "id={id}&user=tester&client={}&proof={}",
            auth::hex(&client_nonce),
            auth::hex(&wrong)
        );
        let (code, _, _) = https(
            &tls_config,
            port,
            &format!(
                "POST /e2e/login HTTP/1.1\r\n{host}Content-Length: {}\r\n\r\n",
                body.len()
            ),
            body.as_bytes(),
        );
        assert_eq!(code, 401);
        drop(sharing);
        let log = std::fs::read_to_string(state.join("sharing.log")).unwrap();
        assert!(!log.contains("abcd"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Serves sample files over plain HTTP on 127.0.0.1 for three minutes,
    /// to try the page in a browser: `cargo test serve_page_for_a_browser
    /// -- --ignored`, with FEATHER_PAGE_PASSWORD and FEATHER_PAGE_E2E set.
    #[test]
    #[ignore]
    fn serve_page_for_a_browser() {
        let (_, shared, state) = sample_share("browser");
        std::fs::write(shared.join("notes.txt"), vec![b'x'; 1500]).unwrap();
        if let Ok(pictures) = std::env::var("FEATHER_PAGE_PICTURES") {
            for entry in std::fs::read_dir(pictures).unwrap().flatten() {
                let _ = std::fs::copy(entry.path(), shared.join(entry.file_name()));
            }
        }
        let mut config = Config::from_settings(&Settings::default());
        config.http = HttpMode::Full;
        config.access = ShareAccess::Local;
        config.http_port = 18444;
        config.user = "tester".into();
        let password = std::env::var("FEATHER_PAGE_PASSWORD").unwrap_or_default();
        if !password.is_empty() {
            config.password_hash = auth::hash_password(&password).unwrap();
            config.password = Hidden(password);
        }
        config.e2e = std::env::var("FEATHER_PAGE_E2E").is_ok();
        config.folders = vec![shared.to_string_lossy().to_string()];
        let sharing = Sharing::start_with(config, Arc::new(IndexSet::empty()), &state, true);
        println!("ready {:?}", sharing.status().problems);
        let seconds = std::env::var("FEATHER_PAGE_SECONDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(180);
        std::thread::sleep(Duration::from_secs(seconds));
    }

    #[test]
    fn the_servers_serve_only_what_is_shared_to_who_signs_in() {
        let base = std::env::temp_dir().join(format!("feather-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let shared = base.join("shared");
        std::fs::create_dir_all(shared.join("docs")).unwrap();
        std::fs::write(shared.join("docs").join("a.txt"), b"hello").unwrap();
        std::fs::write(base.join("secret.txt"), b"secret").unwrap();
        let state = base.join("state");
        let (http_port, ftp_port) = (free_port(), free_port());
        let mut config = Config::from_settings(&Settings::default());
        config.http = HttpMode::Full;
        config.ftp = FtpMode::Full;
        config.access = ShareAccess::Local;
        config.http_port = http_port;
        config.ftp_port = ftp_port;
        config.user = "tester".into();
        config.password_hash = auth::hash_password("correct horse battery").unwrap();
        config.folders = vec![shared.to_string_lossy().to_string()];
        let sharing = Sharing::start_in(config, Arc::new(IndexSet::empty()), &state);
        let status = sharing.status();
        assert!(status.problems.is_empty(), "{:?}", status.problems);
        assert_eq!(
            status.addresses[0],
            format!("https://localhost:{http_port}")
        );
        let cert = std::fs::read(state.join("server.cer")).unwrap();
        let tls_config = client(&cert);
        let host = format!("Host: localhost:{http_port}\r\nConnection: close\r\n");

        // Signed out, the page sends people to sign in.
        let (code, head, _) = https(
            &tls_config,
            http_port,
            &format!("GET / HTTP/1.1\r\n{host}\r\n"),
            b"",
        );
        assert_eq!(code, 303);
        assert!(head.contains("Location: /login"));
        assert!(head.contains("Content-Security-Policy: default-src 'none'"));
        let (code, _, _) = https(
            &tls_config,
            http_port,
            &format!("GET /api/list?root=0 HTTP/1.1\r\n{host}\r\n"),
            b"",
        );
        assert_eq!(code, 401);

        let form = |password: &str| format!("user=tester&password={}", password.replace(' ', "+"));
        let wrong = form("wrong horse battery");
        let (code, _, _) = https(
            &tls_config,
            http_port,
            &format!(
                "POST /login HTTP/1.1\r\n{host}Content-Length: {}\r\n\r\n",
                wrong.len()
            ),
            wrong.as_bytes(),
        );
        assert_eq!(code, 401);
        let right = form("correct horse battery");
        let (code, head, _) = https(
            &tls_config,
            http_port,
            &format!(
                "POST /login HTTP/1.1\r\n{host}Content-Length: {}\r\n\r\n",
                right.len()
            ),
            right.as_bytes(),
        );
        assert_eq!(code, 303);
        let cookie_line = head
            .lines()
            .find(|line| line.starts_with("Set-Cookie: "))
            .unwrap();
        assert!(cookie_line.contains("HttpOnly") && cookie_line.contains("Secure"));
        assert!(cookie_line.contains("SameSite=Strict"));
        let cookie = cookie_line["Set-Cookie: ".len()..]
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let signed = format!("{host}Cookie: {cookie}\r\n");

        let (code, _, body) = https(
            &tls_config,
            http_port,
            &format!("GET /api/info HTTP/1.1\r\n{signed}\r\n"),
            b"",
        );
        assert_eq!(code, 200);
        let info = String::from_utf8(body).unwrap();
        let csrf = info
            .split("\"csrf\":\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .to_string();
        assert!(info.contains("\"mode\":\"full\""));

        let (code, _, body) = https(
            &tls_config,
            http_port,
            &format!("GET /api/list?root=0&path= HTTP/1.1\r\n{signed}\r\n"),
            b"",
        );
        assert_eq!(code, 200);
        assert!(String::from_utf8_lossy(&body).contains("\"name\":\"docs\""));
        let (code, head, body) = https(
            &tls_config,
            http_port,
            &format!("GET /api/download?root=0&path=docs/a.txt HTTP/1.1\r\n{signed}\r\n"),
            b"",
        );
        assert_eq!((code, body.as_slice()), (200, b"hello".as_slice()));
        assert!(head.contains("Content-Disposition: attachment"));
        let (code, _, body) = https(
            &tls_config,
            http_port,
            &format!("GET /api/download?root=0&path=docs/a.txt HTTP/1.1\r\n{signed}Range: bytes=1-3\r\n\r\n"),
            b"",
        );
        assert_eq!((code, body.as_slice()), (206, b"ell".as_slice()));

        // Pictures and videos show in the page; nothing else ever does.
        std::fs::write(shared.join("docs").join("page.html"), b"<script>1</script>").unwrap();
        let (code, head, _) = https(
            &tls_config,
            http_port,
            &format!(
                "GET /api/download?root=0&path=docs/page.html&inline=1 HTTP/1.1\r\n{signed}\r\n"
            ),
            b"",
        );
        assert_eq!(code, 200);
        assert!(head.contains("Content-Type: application/octet-stream"));
        assert!(head.contains("Content-Disposition: attachment"));
        let wallpapers = std::path::Path::new(r"C:\Windows\Web\Wallpaper\Windows");
        let picture = std::fs::read_dir(wallpapers)
            .ok()
            .and_then(|mut entries| entries.find_map(|entry| Some(entry.ok()?.path())));
        if let Some(picture) = picture.filter(|path| thumbs::media_type(path).is_some()) {
            std::fs::copy(&picture, shared.join("docs").join("photo.jpg")).unwrap();
            let (code, head, _) = https(
                &tls_config,
                http_port,
                &format!("GET /api/download?root=0&path=docs/photo.jpg&inline=1 HTTP/1.1\r\n{signed}Range: bytes=0-99\r\n\r\n"),
                b"",
            );
            assert_eq!(code, 206);
            assert!(head.contains("Content-Type: image/jpeg"), "{head}");
            assert!(head.contains("Content-Disposition: inline"));
            let (code, head, body) = https(
                &tls_config,
                http_port,
                &format!(
                    "GET /api/thumb?root=0&path=docs/photo.jpg&size=200 HTTP/1.1\r\n{signed}\r\n"
                ),
                b"",
            );
            assert_eq!(code, 200, "{head}");
            assert!(head.contains("Content-Type: image/jpeg"));
            assert_eq!(&body[..2], &[0xff, 0xd8]);
            let tag = head
                .lines()
                .find_map(|line| line.strip_prefix("ETag: "))
                .unwrap()
                .to_string();
            let (code, _, _) = https(
                &tls_config,
                http_port,
                &format!("GET /api/thumb?root=0&path=docs/photo.jpg&size=200 HTTP/1.1\r\n{signed}If-None-Match: {tag}\r\n\r\n"),
                b"",
            );
            assert_eq!(code, 304);
        }
        let (code, _, _) = https(
            &tls_config,
            http_port,
            &format!("GET /api/thumb?root=0&path=../secret.txt HTTP/1.1\r\n{signed}\r\n"),
            b"",
        );
        assert!(code == 400 || code == 404, "{code}");
        // Climbing out of the shared folder doesn't work, however it's written.
        for path in [
            "../secret.txt",
            "docs/../../secret.txt",
            "..%2Fsecret.txt",
            "docs%5C..%5C..%5Csecret.txt",
        ] {
            let (code, _, body) = https(
                &tls_config,
                http_port,
                &format!("GET /api/download?root=0&path={path} HTTP/1.1\r\n{signed}\r\n"),
                b"",
            );
            assert!(code == 400 || code == 404, "{path}: {code}");
            assert_ne!(body, b"secret");
        }
        // Changes need the form token.
        let (code, _, _) = https(
            &tls_config,
            http_port,
            &format!("POST /api/mkdir?root=0&path=&name=new HTTP/1.1\r\n{signed}\r\n"),
            b"",
        );
        assert_eq!(code, 403);
        let (code, _, _) = https(
            &tls_config,
            http_port,
            &format!("POST /api/mkdir?root=0&path=&name=new HTTP/1.1\r\n{signed}X-Feather-Csrf: {csrf}\r\n\r\n"),
            b"",
        );
        assert_eq!(code, 201);
        assert!(shared.join("new").is_dir());
        let (code, _, _) = https(
            &tls_config,
            http_port,
            &format!("PUT /api/upload?root=0&path=docs&name=b.txt HTTP/1.1\r\n{signed}X-Feather-Csrf: {csrf}\r\nContent-Length: 2\r\n\r\n"),
            b"up",
        );
        assert_eq!(code, 201);
        assert_eq!(
            std::fs::read(shared.join("docs").join("b.txt")).unwrap(),
            b"up"
        );
        // The same name again: the page is asked, then told what to do.
        let put = |query: &str, body: &[u8]| {
            https(
                &tls_config,
                http_port,
                &format!(
                    "PUT /api/upload?{query} HTTP/1.1\r\n{signed}X-Feather-Csrf: {csrf}\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                ),
                body,
            )
        };
        let (code, _, body) = put("root=0&path=docs&name=b.txt", b"again");
        assert_eq!(code, 409);
        assert!(String::from_utf8_lossy(&body).contains("\"conflict\":true"));
        let (code, _, body) = put("root=0&path=docs&name=b.txt&conflict=keep", b"both");
        assert_eq!(code, 201);
        assert!(String::from_utf8_lossy(&body).contains("b (2).txt"));
        assert_eq!(
            std::fs::read(shared.join("docs").join("b (2).txt")).unwrap(),
            b"both"
        );
        let (code, _, _) = put("root=0&path=docs&name=b.txt&conflict=replace", b"new");
        assert_eq!(code, 201);
        assert_eq!(
            std::fs::read(shared.join("docs").join("b.txt")).unwrap(),
            b"new"
        );
        // A whole folder: its folders are made, safely.
        let (code, _, _) = put("root=0&path=docs&name=c.txt&sub=Trip%2FDay+1", b"c");
        assert_eq!(code, 201);
        assert_eq!(
            std::fs::read(shared.join("docs").join("Trip").join("Day 1").join("c.txt")).unwrap(),
            b"c"
        );
        for sub in ["..%2F..", "Trip%2F..%2F..%2Fescape", "a%3Ab", "CON"] {
            let (code, _, _) = put(&format!("root=0&path=docs&name=d.txt&sub={sub}"), b"d");
            assert_eq!(code, 400, "{sub}");
        }
        assert!(!base.join("escape").exists() && !base.join("d.txt").exists());
        // A file where a folder should be.
        let (code, _, _) = put("root=0&path=docs&name=e.txt&sub=a.txt", b"e");
        assert_eq!(code, 409);

        // FTP refuses to sign in without TLS, then works over it.
        let control = TcpStream::connect(("127.0.0.1", ftp_port)).unwrap();
        control
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut plain = BufReader::new(control.try_clone().unwrap());
        let mut writer = control.try_clone().unwrap();
        let mut line = String::new();
        plain.read_line(&mut line).unwrap();
        assert!(line.starts_with("220"));
        writer.write_all(b"USER tester\r\n").unwrap();
        line.clear();
        plain.read_line(&mut line).unwrap();
        assert!(line.starts_with("530"), "{line}");
        writer.write_all(b"AUTH TLS\r\n").unwrap();
        line.clear();
        plain.read_line(&mut line).unwrap();
        assert!(line.starts_with("234"), "{line}");
        drop(plain);
        let mut secure = BufReader::new(tls(&tls_config, control));
        let say =
            |stream: &mut BufReader<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>,
             command: &str|
             -> String {
                stream
                    .get_mut()
                    .write_all(format!("{command}\r\n").as_bytes())
                    .unwrap();
                stream.get_mut().flush().unwrap();
                let mut reply = String::new();
                stream.read_line(&mut reply).unwrap();
                reply
            };
        assert!(say(&mut secure, "USER tester").starts_with("331"));
        assert!(say(&mut secure, "PASS correct horse battery").starts_with("230"));
        assert!(say(&mut secure, "PBSZ 0").starts_with("200"));
        assert!(say(&mut secure, "PROT P").starts_with("200"));
        assert!(say(&mut secure, "CWD /shared/docs").starts_with("250"));
        assert!(say(&mut secure, "CWD ../../..").starts_with("250"));
        assert!(say(&mut secure, "PWD").contains("\"/\""));
        assert!(say(&mut secure, "CWD /shared/docs").starts_with("250"));
        let reply = say(&mut secure, "EPSV");
        assert!(reply.starts_with("229"), "{reply}");
        let port: u16 = reply
            .split("|||")
            .nth(1)
            .unwrap()
            .split('|')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        secure.get_mut().write_all(b"RETR a.txt\r\n").unwrap();
        secure.get_mut().flush().unwrap();
        let mut reply = String::new();
        secure.read_line(&mut reply).unwrap();
        assert!(reply.starts_with("150"), "{reply}");
        let data = TcpStream::connect(("127.0.0.1", port)).unwrap();
        data.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut data = tls(&tls_config, data);
        let mut content = Vec::new();
        let _ = data.read_to_end(&mut content);
        assert_eq!(content, b"hello");
        reply.clear();
        secure.read_line(&mut reply).unwrap();
        assert!(reply.starts_with("226"), "{reply}");
        assert!(say(&mut secure, "RETR ../../secret.txt").starts_with("550"));
        assert!(say(&mut secure, "QUIT").starts_with("221"));

        drop(sharing);
        let log = std::fs::read_to_string(state.join("sharing.log")).unwrap();
        assert!(log.contains("web sign-in failed") && log.contains("FTP sign-in: tester"));
        assert!(
            !log.contains("correct horse"),
            "passwords never reach the log"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The body of a chunked reply.
    fn unchunk(mut body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let line = body.windows(2).position(|w| w == b"\r\n").unwrap();
            let size = usize::from_str_radix(std::str::from_utf8(&body[..line]).unwrap(), 16)
                .expect("a chunk size");
            body = &body[line + 2..];
            if size == 0 {
                assert_eq!(body, b"\r\n", "the reply ends cleanly");
                return out;
            }
            out.extend_from_slice(&body[..size]);
            assert_eq!(&body[size..size + 2], b"\r\n");
            body = &body[size + 2..];
        }
    }

    #[cfg(windows)]
    fn make_hidden(path: &std::path::Path) {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // FILE_ATTRIBUTE_HIDDEN
        let done = unsafe {
            windows_sys::Win32::Storage::FileSystem::SetFileAttributesW(wide.as_ptr(), 0x2)
        };
        assert_ne!(done, 0);
    }

    /// A link (a junction on Windows) at `link` to the folder `target`.
    fn link_folder(target: &std::path::Path, link: &std::path::Path) -> bool {
        #[cfg(windows)]
        {
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .is_ok_and(|output| output.status.success())
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
    }

    #[test]
    fn folders_download_as_a_zip_of_only_what_may_be_seen() {
        let base = std::env::temp_dir().join(format!("feather-zip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let shared = base.join("shared");
        let outside = base.join("outside");
        std::fs::create_dir_all(shared.join("docs").join("sub")).unwrap();
        std::fs::create_dir_all(shared.join("docs").join("private")).unwrap();
        std::fs::create_dir_all(shared.join("docs").join("empty")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(shared.join("docs").join("a.txt"), b"hello").unwrap();
        let big: Vec<u8> = (0..300_000u32).map(|n| (n % 251) as u8).collect();
        std::fs::write(shared.join("docs").join("sub").join("Fotó 1.bin"), &big).unwrap();
        std::fs::write(shared.join("docs").join("private").join("p.txt"), b"p").unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        let hidden = if cfg!(windows) { "hid.txt" } else { ".hid.txt" };
        std::fs::write(shared.join("docs").join(hidden), b"hidden").unwrap();
        #[cfg(windows)]
        make_hidden(&shared.join("docs").join(hidden));
        let linked = link_folder(&outside, &shared.join("docs").join("out"));
        // A link back up must not loop.
        let looped = link_folder(
            &shared.join("docs"),
            &shared.join("docs").join("sub").join("up"),
        );
        // A file another program holds open can't be read: it is left out.
        #[cfg(windows)]
        let _held = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::write(shared.join("docs").join("locked.txt"), b"busy").unwrap();
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(shared.join("docs").join("locked.txt"))
                .unwrap()
        };
        let real = files::display_path(&std::fs::canonicalize(&shared).unwrap());
        let sep = std::path::MAIN_SEPARATOR;

        let state = base.join("state");
        let port = free_port();
        let mut config = Config::from_settings(&Settings::default());
        // Downloading is the least a share allows; that's all a zip needs.
        config.http = HttpMode::Download;
        config.access = ShareAccess::Local;
        config.http_port = port;
        config.user = "tester".into();
        config.password_hash = auth::hash_password("abcd").unwrap();
        config.folders = vec![shared.to_string_lossy().to_string()];
        config.excluded = vec![format!("{real}{sep}docs{sep}private")];
        config.show_hidden = false;
        config.show_system = false;
        let sharing = Sharing::start_in(config, Arc::new(IndexSet::empty()), &state);
        let tls_config = client(&std::fs::read(state.join("server.cer")).unwrap());
        let host = format!("Host: localhost:{port}\r\nConnection: close\r\n");

        // Signed out: nothing.
        let (code, _, _) = https(
            &tls_config,
            port,
            &format!("GET /api/zip?root=0&path=docs HTTP/1.1\r\n{host}\r\n"),
            b"",
        );
        assert_eq!(code, 401);
        let form = "user=tester&password=abcd";
        let (_, head, _) = https(&tls_config, port, &sign_in_form(&host, form, ""), b"");
        let cookie = head
            .lines()
            .find_map(|line| line.strip_prefix("Set-Cookie: "))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let signed = format!("{host}Cookie: {cookie}\r\n");

        let (code, head, body) = https(
            &tls_config,
            port,
            &format!("GET /api/zip?root=0&path=docs HTTP/1.1\r\n{signed}\r\n"),
            b"",
        );
        assert_eq!(code, 200, "{head}");
        assert!(head.contains("Content-Type: application/zip"));
        assert!(head.contains("Transfer-Encoding: chunked"));
        assert!(!head.contains("Content-Length"));
        assert!(head.contains("filename=\"docs.zip\""), "{head}");
        let entries = zip::read::entries(&unchunk(&body));
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        let data = |name: &str| {
            entries
                .iter()
                .find(|entry| entry.name == name)
                .map(|entry| entry.data.clone())
        };
        assert_eq!(
            data("docs/a.txt").as_deref(),
            Some(&b"hello"[..]),
            "{names:?}"
        );
        assert_eq!(data("docs/sub/Fotó 1.bin"), Some(big.clone()));
        assert!(names.contains(&"docs/") && names.contains(&"docs/empty/"));
        assert!(names.contains(&"docs/sub/"));
        for name in &names {
            assert!(
                !name.contains("private"),
                "left out folders aren't zipped: {name}"
            );
            assert!(
                !name.contains("hid.txt"),
                "hidden files aren't zipped: {name}"
            );
            assert!(
                !name.contains("out/"),
                "links outside aren't followed: {name}"
            );
            assert!(!name.contains("secret"), "{name}");
            assert!(
                !name.contains("sub/up/"),
                "a link back up isn't followed: {name}"
            );
        }
        assert!(linked || cfg!(windows), "made the link outside");
        let _ = looped;
        #[cfg(windows)]
        {
            assert!(!names.contains(&"docs/locked.txt"));
            let note = data("docs/Feather - not included.txt").expect("a note");
            assert!(String::from_utf8_lossy(&note).contains("docs/locked.txt"));
        }

        // The whole shared folder is named after it.
        let (code, head, body) = https(
            &tls_config,
            port,
            &format!("GET /api/zip?root=0&path= HTTP/1.1\r\n{signed}\r\n"),
            b"",
        );
        assert_eq!(code, 200);
        assert!(head.contains("filename=\"shared.zip\""), "{head}");
        let entries = zip::read::entries(&unchunk(&body));
        assert!(entries
            .iter()
            .any(|entry| entry.name == "shared/docs/a.txt"));

        // Some of a folder: only what was chosen, and only visible names.
        let (code, _, body) = https(
            &tls_config,
            port,
            &format!(
                "GET /api/zip?root=0&path=docs&name=a.txt&name=sub&name=private HTTP/1.1\r\n{signed}\r\n"
            ),
            b"",
        );
        assert_eq!(code, 200);
        let entries = zip::read::entries(&unchunk(&body));
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert!(names.contains(&"docs/a.txt") && names.contains(&"docs/sub/Fotó 1.bin"));
        assert!(!names
            .iter()
            .any(|name| name.contains("empty") || name.contains("private")));
        let (code, _, _) = https(
            &tls_config,
            port,
            &format!("GET /api/zip?root=0&path=docs&name=..%2Fouter HTTP/1.1\r\n{signed}\r\n"),
            b"",
        );
        assert_eq!(code, 400);

        // HTTP/1.0 has no chunks: the zip ends with the connection.
        let (code, head, body) = https(
            &tls_config,
            port,
            &format!(
                "GET /api/zip?root=0&path=docs/sub HTTP/1.0\r\nHost: localhost\r\nCookie: {cookie}\r\n\r\n"
            ),
            b"",
        );
        assert_eq!(code, 200);
        assert!(!head.contains("chunked"));
        assert!(zip::read::entries(&body)
            .iter()
            .any(|entry| entry.name == "sub/Fotó 1.bin"));

        // Only folders a visitor could open themselves.
        for path in [
            "../outside",
            "docs/../..",
            "docs/private",
            "docs/out",
            "docs%5C..%5C..%5Coutside",
            "missing",
        ] {
            let (code, _, body) = https(
                &tls_config,
                port,
                &format!("GET /api/zip?root=0&path={path} HTTP/1.1\r\n{signed}\r\n"),
                b"",
            );
            assert!(code == 400 || code == 404, "{path}: {code}");
            assert!(!String::from_utf8_lossy(&body).contains("secret"));
        }
        let (code, _, _) = https(
            &tls_config,
            port,
            &format!("GET /api/zip?root=0&path=docs/a.txt HTTP/1.1\r\n{signed}\r\n"),
            b"",
        );
        assert_eq!(code, 400);
        let (code, _, _) = https(
            &tls_config,
            port,
            &format!("GET /api/zip?root=7&path= HTTP/1.1\r\n{signed}\r\n"),
            b"",
        );
        assert_eq!(code, 404);

        drop(sharing);
        let log = std::fs::read_to_string(state.join("sharing.log")).unwrap();
        assert!(log.contains("download folder"), "{log}");
        assert!(log.contains("zip of docs done"), "{log}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn who_may_connect() {
        let lan: IpAddr = "192.168.1.20".parse().unwrap();
        let public: IpAddr = "8.8.8.8".parse().unwrap();
        let local: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(allowed(ShareAccess::Local, Tunnel::Cloudflare, local));
        assert!(!allowed(ShareAccess::Local, Tunnel::Cloudflare, lan));
        assert!(allowed(ShareAccess::Network, Tunnel::Cloudflare, lan));
        assert!(!allowed(ShareAccess::Network, Tunnel::Cloudflare, public));
        assert!(allowed(ShareAccess::Internet, Tunnel::Direct, public));
        assert!(!allowed(ShareAccess::Internet, Tunnel::Ngrok, public));
        assert!(allowed(
            ShareAccess::Network,
            Tunnel::Direct,
            "100.101.1.2".parse().unwrap()
        ));
        assert!(!allowed(
            ShareAccess::Network,
            Tunnel::Direct,
            "100.200.1.2".parse().unwrap()
        ));
    }

    #[test]
    fn dates_come_out_right() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20_402), (2025, 11, 10));
        assert_eq!(civil(11_016), (2000, 2, 29));
    }

    #[test]
    fn sharing_needs_a_password_and_folders() {
        let mut config = Config::from_settings(&Settings::default());
        config.http = HttpMode::Search;
        assert!(config.problem().is_some(), "no folders");
        config.folders = vec!["C:\\".into()];
        assert!(config.problem().is_none(), "the password is optional");
        assert!(config.open());
        config.password_hash = "x".into();
        assert!(!config.open());
    }
}
