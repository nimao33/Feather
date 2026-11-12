//! The FTP server, for apps like FileZilla and WinSCP. It asks for TLS
//! (`AUTH TLS`, also called explicit FTPS) before anyone may sign in,
//! unless plain FTP is allowed in Settings. Only passive mode is offered,
//! and a data connection must come from the same address as its control
//! connection, so nobody else can grab a transfer.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustls::{ServerConnection, StreamOwned};

use super::files::{self, Item};
use super::{allowed, auth, civil, Shared};
use crate::settings::FtpMode;

const MAX_CONNECTIONS: usize = 16;
const IDLE: Duration = Duration::from_secs(300);
const DATA_WAIT: Duration = Duration::from_secs(20);
const DATA_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_LINE: usize = 4096;
const MAX_TRIES: u32 = 3;
const LIST_LIMIT: usize = 20_000;

pub fn spawn(shared: Arc<Shared>, listener: TcpListener) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("feather-ftp".into())
        .spawn(move || accept(shared, listener))
        .expect("start the FTP server")
}

fn accept(shared: Arc<Shared>, listener: TcpListener) {
    if listener.set_nonblocking(true).is_err() {
        return;
    }
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while !shared.stopping() {
        match listener.accept() {
            Ok((stream, peer)) => {
                if !allowed(shared.config.access, shared.config.tunnel, peer.ip())
                    || active.load(Ordering::SeqCst) >= MAX_CONNECTIONS
                {
                    continue;
                }
                active.fetch_add(1, Ordering::SeqCst);
                let (worker, count) = (Arc::clone(&shared), Arc::clone(&active));
                let started = std::thread::Builder::new()
                    .name("feather-ftp-client".into())
                    .spawn(move || {
                        let _ = serve(&worker, stream, peer.ip());
                        count.fetch_sub(1, Ordering::SeqCst);
                    });
                if started.is_err() {
                    active.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// A connection that may have switched to TLS.
enum Link {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ServerConnection, TcpStream>>),
    Gone,
}

impl Read for Link {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Link::Plain(stream) => stream.read(buffer),
            Link::Tls(stream) => match stream.read(buffer) {
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
                other => other,
            },
            Link::Gone => Ok(0),
        }
    }
}

impl Write for Link {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Link::Plain(stream) => stream.write(buffer),
            Link::Tls(stream) => stream.write(buffer),
            Link::Gone => Err(io::ErrorKind::NotConnected.into()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Link::Plain(stream) => stream.flush(),
            Link::Tls(stream) => stream.flush(),
            Link::Gone => Ok(()),
        }
    }
}

impl Link {
    fn is_tls(&self) -> bool {
        matches!(self, Link::Tls(_))
    }

    /// Ends the connection, saying goodbye properly over TLS.
    fn finish(self) {
        match self {
            Link::Tls(mut stream) => {
                stream.conn.send_close_notify();
                let _ = stream.flush();
                let _ = stream.sock.shutdown(std::net::Shutdown::Both);
            }
            Link::Plain(stream) => {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            Link::Gone => {}
        }
    }
}

/// Where a path points in the virtual tree: the list of shared folders, or
/// somewhere inside one.
enum Place {
    Top,
    Inside(usize, String),
}

struct Session<'a> {
    shared: &'a Shared,
    control: Link,
    buffer: Vec<u8>,
    peer: IpAddr,
    local: IpAddr,
    user: Option<String>,
    signed_in: bool,
    misses: u32,
    /// The current folder, as names from the top.
    cwd: Vec<String>,
    protect_data: bool,
    passive: Option<TcpListener>,
    restart: u64,
    rename_from: Option<(usize, PathBuf)>,
}

fn serve(shared: &Shared, stream: TcpStream, peer: IpAddr) -> io::Result<()> {
    // Accepted from a non-blocking listener, Windows makes it non-blocking too.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IDLE))?;
    stream.set_write_timeout(Some(DATA_TIMEOUT))?;
    let local = stream.local_addr()?.ip();
    let mut session = Session {
        shared,
        control: Link::Plain(stream),
        buffer: Vec::new(),
        peer,
        local,
        user: None,
        signed_in: false,
        misses: 0,
        cwd: Vec::new(),
        protect_data: false,
        passive: None,
        restart: 0,
        rename_from: None,
    };
    session.reply(220, "Feather FTP is ready.")?;
    while !shared.stopping() {
        let Some(line) = session.read_line()? else {
            break;
        };
        let (command, argument) = match line.split_once(' ') {
            Some((command, argument)) => (command.to_ascii_uppercase(), argument.to_string()),
            None => (line.to_ascii_uppercase(), String::new()),
        };
        if !session.command(&command, &argument)? {
            break;
        }
    }
    if shared.stopping() {
        let _ = session.reply(421, "Feather stopped sharing.");
    }
    std::mem::replace(&mut session.control, Link::Gone).finish();
    Ok(())
}

impl Session<'_> {
    fn reply(&mut self, code: u16, text: &str) -> io::Result<()> {
        // Replies never carry line breaks from file names.
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        self.control
            .write_all(format!("{code} {text}\r\n").as_bytes())?;
        self.control.flush()
    }

    fn read_line(&mut self) -> io::Result<Option<String>> {
        let mut chunk = [0u8; 1024];
        loop {
            if let Some(end) = self.buffer.iter().position(|&byte| byte == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=end).collect();
                let text = String::from_utf8_lossy(&line);
                return Ok(Some(text.trim_end_matches(['\r', '\n']).to_string()));
            }
            if self.buffer.len() > MAX_LINE {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let read = self.control.read(&mut chunk)?;
            if read == 0 {
                return Ok(None);
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }

    fn full(&self) -> bool {
        self.shared.config.ftp == FtpMode::Full
    }

    /// Runs one command. `Ok(false)` ends the session.
    fn command(&mut self, command: &str, argument: &str) -> io::Result<bool> {
        match command {
            "AUTH" => {
                let kind = argument.to_ascii_uppercase();
                if self.control.is_tls() {
                    self.reply(503, "TLS is already on.")?;
                } else if kind == "TLS" || kind == "TLS-C" || kind == "SSL" {
                    self.reply(234, "Starting TLS.")?;
                    self.buffer.clear();
                    let Link::Plain(stream) = std::mem::replace(&mut self.control, Link::Gone)
                    else {
                        return Ok(false);
                    };
                    let connection = ServerConnection::new(Arc::clone(&self.shared.tls))
                        .map_err(io::Error::other)?;
                    self.control = Link::Tls(Box::new(StreamOwned::new(connection, stream)));
                } else {
                    self.reply(504, "Only AUTH TLS is supported.")?;
                }
            }
            "PBSZ" => {
                if self.control.is_tls() {
                    self.reply(200, "PBSZ=0")?;
                } else {
                    self.reply(503, "Use AUTH TLS first.")?;
                }
            }
            "PROT" => match argument.to_ascii_uppercase().as_str() {
                "P" if self.control.is_tls() => {
                    self.protect_data = true;
                    self.reply(200, "Data connections are protected.")?;
                }
                "C" if self.shared.config.ftp_plain => {
                    self.protect_data = false;
                    self.reply(200, "Data connections are not protected.")?;
                }
                "C" => self.reply(536, "Data connections must be protected (PROT P).")?,
                _ => self.reply(504, "Use PROT P.")?,
            },
            "USER" => {
                if !self.control.is_tls() && !self.shared.config.ftp_plain {
                    self.reply(530, "Use AUTH TLS (FTPS) first; plain FTP is off.")?;
                } else {
                    self.user = Some(argument.to_string());
                    self.signed_in = false;
                    self.reply(331, "Now send the password.")?;
                }
            }
            "PASS" => return self.sign_in(argument),
            "QUIT" => {
                self.reply(221, "Goodbye.")?;
                return Ok(false);
            }
            "NOOP" => self.reply(200, "OK.")?,
            "SYST" => self.reply(215, "UNIX Type: L8")?,
            "FEAT" => {
                self.control.write_all(
                    b"211-Features:\r\n AUTH TLS\r\n PBSZ\r\n PROT\r\n UTF8\r\n EPSV\r\n PASV\r\n SIZE\r\n MDTM\r\n REST STREAM\r\n MLST type*;size*;modify*;\r\n211 End\r\n",
                )?;
                self.control.flush()?;
            }
            "OPTS" if argument.to_ascii_uppercase().starts_with("UTF8") => {
                self.reply(200, "UTF-8 is always on.")?;
            }
            "CLNT" => self.reply(200, "OK.")?,
            _ if !self.signed_in => self.reply(530, "Please sign in first.")?,
            _ => self.signed_in_command(command, argument)?,
        }
        Ok(true)
    }

    fn sign_in(&mut self, password: &str) -> io::Result<bool> {
        let Some(user) = self.user.clone() else {
            self.reply(503, "Send USER first.")?;
            return Ok(true);
        };
        if self.shared.guard.check(self.peer).is_err() {
            self.shared
                .log(self.peer, "FTP sign-in refused: locked out");
            self.reply(421, "Too many wrong tries. Try again later.")?;
            return Ok(false);
        }
        let config = &self.shared.config;
        if config.open() || auth::check(&user, password, &config.user, &config.password_hash) {
            self.shared.guard.succeeded(self.peer);
            self.signed_in = true;
            self.shared.log(self.peer, &format!("FTP sign-in: {user}"));
            self.reply(230, "Signed in.")?;
            Ok(true)
        } else {
            self.shared.guard.failed(self.peer);
            self.shared.log(self.peer, "FTP sign-in failed");
            self.misses += 1;
            std::thread::sleep(Duration::from_secs(1));
            self.reply(530, "That user name and password don't match.")?;
            Ok(self.misses < MAX_TRIES)
        }
    }

    fn signed_in_command(&mut self, command: &str, argument: &str) -> io::Result<()> {
        match command {
            "PWD" | "XPWD" => {
                let path = format!("/{}", self.cwd.join("/")).replace('"', "\"\"");
                self.reply(257, &format!("\"{path}\" is the current folder."))?;
            }
            "CWD" | "XCWD" => self.change_folder(argument)?,
            "CDUP" | "XCUP" => self.change_folder("..")?,
            "TYPE" => match argument.to_ascii_uppercase().chars().next() {
                Some('I') | Some('A') | Some('L') => self.reply(200, "Type set.")?,
                _ => self.reply(504, "Use TYPE I.")?,
            },
            "MODE" if argument.eq_ignore_ascii_case("S") => self.reply(200, "Stream mode.")?,
            "STRU" if argument.eq_ignore_ascii_case("F") => self.reply(200, "File structure.")?,
            "PASV" => self.passive(false)?,
            "EPSV" => self.passive(true)?,
            "PORT" | "EPRT" => self.reply(502, "Active mode is off; use passive mode.")?,
            "LIST" | "NLST" | "MLSD" => self.list(command, argument)?,
            "MLST" => self.mlst(argument)?,
            "SIZE" => match self.file(argument) {
                Some((_, path)) => {
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    self.reply(213, &size.to_string())?;
                }
                None => self.reply(550, "No such file.")?,
            },
            "MDTM" => match self.file(argument) {
                Some((_, path)) => {
                    let item = self.shared.roots.describe(&path);
                    let stamp = item
                        .map(|item| mlsd_time(item.modified))
                        .unwrap_or_default();
                    self.reply(213, &stamp)?;
                }
                None => self.reply(550, "No such file.")?,
            },
            "REST" => match argument.trim().parse::<u64>() {
                Ok(offset) => {
                    self.restart = offset;
                    self.reply(350, &format!("Restarting at {offset}."))?;
                }
                Err(_) => self.reply(501, "REST needs a number.")?,
            },
            "RETR" => self.retrieve(argument)?,
            "ABOR" => self.reply(226, "Nothing to abort.")?,
            "STAT" => self.reply(211, "Feather FTP.")?,
            "ALLO" => self.reply(202, "No need.")?,
            "STOR" | "DELE" | "MKD" | "XMKD" | "RMD" | "XRMD" | "RNFR" | "RNTO" | "APPE"
                if !self.full() =>
            {
                self.reply(550, "This share is read-only.")?;
            }
            "STOR" => self.store(argument)?,
            "DELE" | "RMD" | "XRMD" => self.delete(argument)?,
            "MKD" | "XMKD" => self.make_folder(argument)?,
            "RNFR" => match self.place(argument).and_then(|place| self.existing(place)) {
                Some((root, path)) if !self.shared.roots.is_root(root, &path) => {
                    self.rename_from = Some((root, path));
                    self.reply(350, "Now send the new name.")?;
                }
                _ => self.reply(550, "Can't rename that.")?,
            },
            "RNTO" => self.rename_to(argument)?,
            _ => self.reply(502, "That command isn't supported.")?,
        }
        Ok(())
    }

    /// Resolves a visitor's path against the current folder. `..` can't
    /// climb above the top.
    fn place(&self, argument: &str) -> Option<Place> {
        let argument = argument.trim();
        let mut names: Vec<String> = if argument.starts_with('/') {
            Vec::new()
        } else {
            self.cwd.clone()
        };
        for name in argument.split(['/', '\\']) {
            match name {
                "" | "." => {}
                ".." => {
                    names.pop();
                }
                name => names.push(name.to_string()),
            }
        }
        if names.is_empty() {
            return Some(Place::Top);
        }
        let root = self
            .shared
            .roots
            .roots
            .iter()
            .position(|root| root.name == names[0])?;
        Some(Place::Inside(root, names[1..].join("/")))
    }

    fn existing(&self, place: Place) -> Option<(usize, PathBuf)> {
        match place {
            Place::Top => None,
            Place::Inside(root, relative) => self
                .shared
                .roots
                .resolve(root, &relative)
                .ok()
                .map(|path| (root, path)),
        }
    }

    fn file(&self, argument: &str) -> Option<(usize, PathBuf)> {
        self.place(argument)
            .and_then(|place| self.existing(place))
            .filter(|(_, path)| path.is_file())
    }

    /// Where a new item would go: its shared folder and path.
    fn new_item(&self, argument: &str) -> Option<(usize, PathBuf)> {
        match self.place(argument)? {
            Place::Top => None,
            Place::Inside(root, relative) => {
                let (parent, name) = relative.rsplit_once('/').unwrap_or(("", &relative));
                self.shared
                    .roots
                    .resolve_new(root, parent, name)
                    .ok()
                    .map(|path| (root, path))
            }
        }
    }

    fn change_folder(&mut self, argument: &str) -> io::Result<()> {
        let ok = match self.place(argument) {
            Some(Place::Top) => {
                self.cwd.clear();
                true
            }
            Some(Place::Inside(root, relative)) => {
                match self.shared.roots.resolve(root, &relative) {
                    Ok(path) if path.is_dir() => {
                        let mut names = vec![self.shared.roots.roots[root].name.clone()];
                        names.extend(
                            relative
                                .split('/')
                                .filter(|name| !name.is_empty())
                                .map(str::to_string),
                        );
                        self.cwd = names;
                        true
                    }
                    _ => false,
                }
            }
            None => false,
        };
        if ok {
            self.reply(250, "Folder changed.")
        } else {
            self.reply(550, "No such folder.")
        }
    }

    fn passive(&mut self, extended: bool) -> io::Result<()> {
        let IpAddr::V4(v4) = self.local else {
            if extended {
                return self.open_passive(SocketAddr::new(self.local, 0), true);
            }
            return self.reply(425, "Use EPSV on IPv6.");
        };
        let _ = v4;
        self.open_passive(SocketAddr::new(self.local, 0), extended)
    }

    fn open_passive(&mut self, address: SocketAddr, extended: bool) -> io::Result<()> {
        let listener = match TcpListener::bind(address) {
            Ok(listener) => listener,
            Err(_) => return self.reply(425, "Couldn't open a data port."),
        };
        let port = listener.local_addr()?.port();
        self.passive = Some(listener);
        if extended {
            self.reply(229, &format!("Entering Extended Passive Mode (|||{port}|)"))
        } else {
            let IpAddr::V4(v4) = self.local else {
                return self.reply(425, "Use EPSV.");
            };
            let [a, b, c, d] = v4.octets();
            self.reply(
                227,
                &format!(
                    "Entering Passive Mode ({a},{b},{c},{d},{},{})",
                    port >> 8,
                    port & 0xff
                ),
            )
        }
    }

    /// Waits for the visitor's data connection, after a 150 reply.
    fn data(&mut self) -> io::Result<Option<Link>> {
        let Some(listener) = self.passive.take() else {
            self.reply(425, "Send PASV or EPSV first.")?;
            return Ok(None);
        };
        if !self.shared.config.ftp_plain && !self.protect_data {
            self.reply(521, "Data connections must be protected (PROT P).")?;
            return Ok(None);
        }
        self.reply(150, "Opening the data connection.")?;
        listener.set_nonblocking(true)?;
        let started = Instant::now();
        let stream = loop {
            match listener.accept() {
                // Only the visitor on the control connection may connect.
                Ok((stream, peer)) if peer.ip() == self.peer => break Some(stream),
                Ok(_) => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if started.elapsed() > DATA_WAIT || self.shared.stopping() {
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break None,
            }
        };
        let Some(stream) = stream else {
            self.reply(425, "The data connection didn't arrive.")?;
            return Ok(None);
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(DATA_TIMEOUT))?;
        stream.set_write_timeout(Some(DATA_TIMEOUT))?;
        if self.protect_data {
            let connection =
                ServerConnection::new(Arc::clone(&self.shared.tls)).map_err(io::Error::other)?;
            Ok(Some(Link::Tls(Box::new(StreamOwned::new(
                connection, stream,
            )))))
        } else {
            Ok(Some(Link::Plain(stream)))
        }
    }

    fn items(&self, place: &Place) -> Option<Vec<Item>> {
        match place {
            Place::Top => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|time| time.as_secs())
                    .unwrap_or(0);
                Some(
                    self.shared
                        .roots
                        .roots
                        .iter()
                        .map(|root| Item {
                            name: root.name.clone(),
                            dir: true,
                            size: 0,
                            modified: now,
                        })
                        .collect(),
                )
            }
            Place::Inside(root, relative) => {
                let path = self.shared.roots.resolve(*root, relative).ok()?;
                if path.is_dir() {
                    self.shared
                        .roots
                        .list(&path, LIST_LIMIT)
                        .ok()
                        .map(|(items, _)| items)
                } else {
                    self.shared.roots.describe(&path).map(|item| vec![item])
                }
            }
        }
    }

    fn list(&mut self, command: &str, argument: &str) -> io::Result<()> {
        // Clients add `ls` flags like `-la`; they don't name a path.
        let target = argument
            .split_whitespace()
            .filter(|part| !part.starts_with('-'))
            .collect::<Vec<_>>()
            .join(" ");
        let Some(items) = self.place(&target).and_then(|place| self.items(&place)) else {
            return self.reply(550, "No such folder.");
        };
        let mut text = String::new();
        for item in &items {
            match command {
                "NLST" => text.push_str(&format!("{}\r\n", item.name)),
                "MLSD" => text.push_str(&format!("{} {}\r\n", facts(item), item.name)),
                _ => text.push_str(&format!("{}\r\n", unix_line(item))),
            }
        }
        let Some(mut data) = self.data()? else {
            return Ok(());
        };
        let sent = data.write_all(text.as_bytes()).and_then(|_| data.flush());
        data.finish();
        match sent {
            Ok(()) => self.reply(226, "Listing sent."),
            Err(_) => self.reply(426, "The listing was cut off."),
        }
    }

    fn mlst(&mut self, argument: &str) -> io::Result<()> {
        let place = self.place(argument);
        let item = match &place {
            Some(Place::Top) => Some(Item {
                name: "/".into(),
                dir: true,
                size: 0,
                modified: 0,
            }),
            Some(Place::Inside(root, relative)) => self
                .shared
                .roots
                .resolve(*root, relative)
                .ok()
                .and_then(|path| self.shared.roots.describe(&path)),
            None => None,
        };
        match item {
            Some(item) => {
                let text = format!(
                    "250-Facts\r\n {} {}\r\n250 End\r\n",
                    facts(&item),
                    argument.trim()
                );
                self.control.write_all(text.as_bytes())?;
                self.control.flush()
            }
            None => self.reply(550, "No such file or folder."),
        }
    }

    fn retrieve(&mut self, argument: &str) -> io::Result<()> {
        let start = std::mem::take(&mut self.restart);
        let Some((_, path)) = self.file(argument) else {
            return self.reply(550, "No such file.");
        };
        let Ok(mut file) = File::open(&path) else {
            return self.reply(550, "That file can't be opened.");
        };
        if file.seek(SeekFrom::Start(start)).is_err() {
            return self.reply(554, "Can't restart there.");
        }
        self.shared.log(
            self.peer,
            &format!("FTP download {}", files::display_path(&path)),
        );
        let Some(mut data) = self.data()? else {
            return Ok(());
        };
        let copied = io::copy(&mut file, &mut data).and_then(|_| data.flush());
        data.finish();
        match copied {
            Ok(()) => self.reply(226, "Sent."),
            Err(_) => self.reply(426, "The transfer was cut off."),
        }
    }

    fn store(&mut self, argument: &str) -> io::Result<()> {
        if std::mem::take(&mut self.restart) > 0 {
            return self.reply(504, "Resuming uploads isn't supported.");
        }
        let Some((_, target)) = self.new_item(argument) else {
            return self.reply(553, "That name isn't allowed here.");
        };
        if target.is_dir() {
            return self.reply(553, "A folder with that name is already there.");
        }
        let Some(folder) = target.parent().map(PathBuf::from) else {
            return self.reply(553, "That folder can't be used.");
        };
        let temporary = folder.join(format!(".feather-upload-{}.part", auth::token(8)));
        let Ok(mut file) = File::create(&temporary) else {
            return self.reply(553, "Couldn't write there.");
        };
        let Some(mut data) = self.data()? else {
            let _ = std::fs::remove_file(&temporary);
            return Ok(());
        };
        let received = io::copy(&mut data, &mut file).and_then(|_| file.sync_all());
        data.finish();
        drop(file);
        match received.and_then(|_| std::fs::rename(&temporary, &target)) {
            Ok(()) => {
                self.shared.log(
                    self.peer,
                    &format!("FTP upload {}", files::display_path(&target)),
                );
                self.reply(226, "Received.")
            }
            Err(_) => {
                let _ = std::fs::remove_file(&temporary);
                self.reply(451, "The upload failed.")
            }
        }
    }

    fn delete(&mut self, argument: &str) -> io::Result<()> {
        match self.place(argument).and_then(|place| self.existing(place)) {
            Some((_, path)) if path.is_dir() && self.shared.roots.holds_blocked(&path) => {
                self.reply(550, "That folder holds a folder that isn't shared.")
            }
            Some((root, path)) if !self.shared.roots.is_root(root, &path) => {
                match files::recycle(&path) {
                    Ok(()) => {
                        self.shared.log(
                            self.peer,
                            &format!("FTP delete {}", files::display_path(&path)),
                        );
                        self.reply(250, "Moved to the Recycle Bin.")
                    }
                    Err(error) => self.reply(550, &error),
                }
            }
            _ => self.reply(550, "Can't delete that."),
        }
    }

    fn make_folder(&mut self, argument: &str) -> io::Result<()> {
        match self.new_item(argument) {
            Some((_, path)) if !path.exists() => match std::fs::create_dir(&path) {
                Ok(()) => {
                    self.shared.log(
                        self.peer,
                        &format!("FTP new folder {}", files::display_path(&path)),
                    );
                    let shown = argument.replace('"', "\"\"");
                    self.reply(257, &format!("\"{shown}\" created."))
                }
                Err(_) => self.reply(550, "Couldn't create the folder."),
            },
            _ => self.reply(550, "Can't create that."),
        }
    }

    fn rename_to(&mut self, argument: &str) -> io::Result<()> {
        let Some((from_root, from)) = self.rename_from.take() else {
            return self.reply(503, "Send RNFR first.");
        };
        // A new name above a left-out folder would expose it.
        if from.is_dir() && self.shared.roots.holds_blocked(&from) {
            return self.reply(553, "That folder holds a folder that isn't shared.");
        }
        match self.new_item(argument) {
            Some((root, target)) if root == from_root && !target.exists() => {
                match std::fs::rename(&from, &target) {
                    Ok(()) => {
                        self.shared.log(
                            self.peer,
                            &format!(
                                "FTP rename {} to {}",
                                files::display_path(&from),
                                files::display_path(&target)
                            ),
                        );
                        self.reply(250, "Renamed.")
                    }
                    Err(_) => self.reply(553, "Couldn't rename it."),
                }
            }
            _ => self.reply(553, "Can't rename to that."),
        }
    }
}

/// MLSD facts for an item.
fn facts(item: &Item) -> String {
    if item.dir {
        format!("type=dir;modify={};", mlsd_time(item.modified))
    } else {
        format!(
            "type=file;size={};modify={};",
            item.size,
            mlsd_time(item.modified)
        )
    }
}

/// `YYYYMMDDHHMMSS` in UTC.
fn mlsd_time(seconds: u64) -> String {
    let (year, month, day) = civil(seconds / 86_400);
    let rest = seconds % 86_400;
    format!(
        "{year:04}{month:02}{day:02}{:02}{:02}{:02}",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// A line like `ls -l` prints, which every FTP client understands.
fn unix_line(item: &Item) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (year, month, day) = civil(item.modified / 86_400);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .unwrap_or(0);
    let rest = item.modified % 86_400;
    // Recent files show the time; older ones the year.
    let when = if now.saturating_sub(item.modified) < 180 * 86_400 {
        format!("{:02}:{:02}", rest / 3600, rest / 60 % 60)
    } else {
        format!("{year:>5}")
    };
    let kind = if item.dir { "drwxr-xr-x" } else { "-rw-r--r--" };
    format!(
        "{kind} 1 feather feather {:>13} {} {day:>2} {when} {}",
        item.size,
        MONTHS[(month as usize).clamp(1, 12) - 1],
        item.name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listings_look_like_unix() {
        let item = Item {
            name: "photo.jpg".into(),
            dir: false,
            size: 1234,
            modified: 1_700_000_000,
        };
        let line = unix_line(&item);
        assert!(line.starts_with("-rw-r--r-- 1 feather feather          1234 Nov 14"));
        assert!(line.ends_with(" photo.jpg"));
        assert_eq!(mlsd_time(1_700_000_000), "20231114221320");
        assert_eq!(facts(&item), "type=file;size=1234;modify=20231114221320;");
    }
}
