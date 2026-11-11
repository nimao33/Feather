//! Reaching the web page from the internet through Cloudflare Tunnel
//! (`cloudflared`) or ngrok. Feather runs the tool, points it at the web
//! server on 127.0.0.1, and reads the public address from its output. The
//! server itself never listens beyond this computer.
//!
//! Nothing has to be set up by hand: when the tool is missing, Feather
//! fetches it once into its own folder. cloudflared comes from Cloudflare's
//! GitHub releases and is used only if Windows confirms Cloudflare's
//! signature on it; ngrok comes through winget, which checks the package
//! hash, or else from ngrok's download, checked for ngrok's signature.
//! Tokens go to the tool through its environment, never its command line,
//! so other programs can't read them from the process list.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::Status;
use crate::settings::Tunnel as Kind;

const CLOUDFLARED_URL: &str =
    "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-windows-amd64.exe";
const NGROK_URL: &str = "https://bin.equinox.io/c/bNyj1mQVY4c/ngrok-v3-stable-windows-amd64.zip";
/// Where ngrok users get the token ngrok needs.
pub const NGROK_TOKEN_PAGE: &str = "https://dashboard.ngrok.com/get-started/your-authtoken";

struct Running {
    child: Child,
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

// The job handle is only used to close it.
unsafe impl Send for Running {}

pub struct Tunnel {
    running: Arc<Mutex<Option<Running>>>,
    stopped: Arc<AtomicBool>,
}

fn name(kind: Kind) -> &'static str {
    match kind {
        Kind::Ngrok => "ngrok",
        _ => "cloudflared",
    }
}

fn setting_up(kind: Kind) -> String {
    match kind {
        Kind::Ngrok => "Setting up ngrok (one-time download)...".into(),
        _ => "Setting up the Cloudflare tunnel (one-time download)...".into(),
    }
}

impl Tunnel {
    /// Starts the tunnel in the background, fetching the tool first when
    /// it isn't there. Progress and problems show in `status`.
    pub fn start(
        kind: Kind,
        port: u16,
        token: &str,
        domain: &str,
        status: Arc<Mutex<Status>>,
    ) -> Result<Self, String> {
        if kind == Kind::Direct {
            return Err("A direct connection needs no tunnel.".into());
        }
        if kind == Kind::Ngrok && token.is_empty() && !ngrok_has_token() {
            return Err(
                "ngrok needs a free account token: press Set up tunnel in Settings > Sharing."
                    .into(),
            );
        }
        let running = Arc::new(Mutex::new(None));
        let stopped = Arc::new(AtomicBool::new(false));
        let tunnel = Self {
            running: Arc::clone(&running),
            stopped: Arc::clone(&stopped),
        };
        let token = token.to_string();
        let domain = clean_domain(domain);
        std::thread::Builder::new()
            .name("feather-tunnel".into())
            .spawn(move || {
                let path = match find(name(kind)) {
                    Some(path) => path,
                    None => {
                        let note = setting_up(kind);
                        status.lock().unwrap().problems.push(note.clone());
                        let installed = install(kind);
                        status.lock().unwrap().problems.retain(|line| *line != note);
                        match installed {
                            Ok(path) => path,
                            Err(error) => {
                                status.lock().unwrap().problems.push(error);
                                return;
                            }
                        }
                    }
                };
                let mut slot = running.lock().unwrap();
                if stopped.load(Ordering::SeqCst) {
                    return;
                }
                match launch(kind, &path, port, &token, &domain, &status, &stopped) {
                    Ok(started) => *slot = Some(started),
                    Err(error) => status.lock().unwrap().problems.push(error),
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(tunnel)
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let mut slot = self.running.lock().unwrap();
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(mut running) = slot.take() {
            let _ = running.child.kill();
            let _ = running.child.wait();
            #[cfg(windows)]
            if !running.job.is_null() {
                unsafe {
                    windows_sys::Win32::Foundation::CloseHandle(running.job);
                }
            }
        }
    }
}

/// A domain as typed, without the scheme or anything after the host, and
/// only when it is a plain host name.
fn clean_domain(domain: &str) -> String {
    let host = domain
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split(['/', ':'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let plain = host.contains('.')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    if plain {
        host
    } else {
        String::new()
    }
}

fn launch(
    kind: Kind,
    path: &Path,
    port: u16,
    token: &str,
    domain: &str,
    status: &Arc<Mutex<Status>>,
    stopped: &Arc<AtomicBool>,
) -> Result<Running, String> {
    let local = format!("https://localhost:{port}");
    let (args, variable): (Vec<String>, Option<&str>) = match kind {
        Kind::Cloudflare if token.is_empty() => (
            vec![
                "tunnel".into(),
                "--no-autoupdate".into(),
                "--url".into(),
                local,
                // The server's certificate is its own; the tunnel only
                // runs on this computer.
                "--no-tls-verify".into(),
            ],
            None,
        ),
        Kind::Cloudflare => (
            vec!["tunnel".into(), "--no-autoupdate".into(), "run".into()],
            Some("TUNNEL_TOKEN"),
        ),
        _ => {
            let mut args = vec![
                "http".to_string(),
                local,
                "--log".into(),
                "stdout".into(),
                "--log-format".into(),
                "logfmt".into(),
            ];
            // The account's static domain: the same address every time.
            if !domain.is_empty() {
                args.push(format!("--url=https://{domain}"));
            }
            (args, (!token.is_empty()).then_some("NGROK_AUTHTOKEN"))
        }
    };
    let mut command = Command::new(path);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(variable) = variable {
        command.env(variable, token);
    }
    hide_window(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("Couldn't start {}: {error}", name(kind)))?;
    for stream in [
        child
            .stdout
            .take()
            .map(|out| Box::new(out) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|err| Box::new(err) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let (status, stopped) = (Arc::clone(status), Arc::clone(stopped));
        let named = kind == Kind::Cloudflare && !token.is_empty();
        let _ = std::thread::Builder::new()
            .name("feather-tunnel-log".into())
            .spawn(move || watch(stream, kind, named, status, stopped));
    }
    Ok(Running {
        #[cfg(windows)]
        job: kill_with_feather(&child),
        child,
    })
}

fn hide_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window for the tool.
        command.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = command;
}

/// Reads the tool's log for its public address and for errors.
fn watch(
    stream: Box<dyn Read + Send>,
    kind: Kind,
    named: bool,
    status: Arc<Mutex<Status>>,
    stopped: Arc<AtomicBool>,
) {
    let mut last_error = String::new();
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else {
            break;
        };
        if let Some(address) = public_address(kind, &line) {
            let mut status = status.lock().unwrap();
            if !status.addresses.contains(&address) {
                status.addresses.insert(0, address);
            }
        } else if named && line.contains("Registered tunnel connection") {
            let mut status = status.lock().unwrap();
            let note = "your Cloudflare hostname".to_string();
            if !status.addresses.contains(&note) {
                status.addresses.insert(0, note);
            }
        } else if line.contains("lvl=eror") || line.contains(" ERR ") || line.contains("ERROR") {
            last_error = line.chars().take(240).collect();
        }
    }
    if !stopped.load(Ordering::SeqCst) {
        let mut status = status.lock().unwrap();
        status.addresses.retain(|address| {
            !address.contains("trycloudflare")
                && !address.contains("ngrok")
                && !address.contains("Cloudflare")
        });
        let problem = if last_error.is_empty() {
            format!("The {} tunnel stopped.", name(kind))
        } else {
            format!("The {} tunnel stopped: {last_error}", name(kind))
        };
        if !status.problems.contains(&problem) {
            status.problems.push(problem);
        }
    }
}

/// The public `https://` address in one log line, if there is one. A line
/// can name the local address too, so every address on it is checked.
fn public_address(kind: Kind, line: &str) -> Option<String> {
    if kind == Kind::Ngrok {
        if !line.contains("started tunnel") {
            return None;
        }
        let start = line.find("url=https://")? + 4;
        let address: String = line[start..]
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '"')
            .collect();
        return Some(address.trim_end_matches('/').to_string());
    }
    line.match_indices("https://").find_map(|(start, _)| {
        let address: String = line[start..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '/' | '.' | '-'))
            .collect();
        let host = address
            .trim_start_matches("https://")
            .split(['/', ':'])
            .next()
            .unwrap_or("");
        let ours = match kind {
            Kind::Cloudflare => host.ends_with(".trycloudflare.com") && !host.starts_with("api."),
            Kind::Ngrok => {
                host.ends_with(".ngrok-free.app")
                    || host.ends_with(".ngrok-free.dev")
                    || host.ends_with(".ngrok.app")
                    || host.ends_with(".ngrok.dev")
                    || host.ends_with(".ngrok.io")
            }
            Kind::Direct => false,
        };
        ours.then_some(address)
    })
}

/// Feather's own folder for the tunnel tools.
fn tools_folder() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_DATA_HOME"))
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
        })?;
    Some(base.join("Feather").join("tools"))
}

/// Looks for the tool in Feather's folder, on PATH, and where installers
/// put it.
fn find(program: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) {
        format!("{program}.exe")
    } else {
        program.to_string()
    };
    let mut places: Vec<PathBuf> = tools_folder().into_iter().collect();
    places.extend(
        std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .unwrap_or_default(),
    );
    let env = |name: &str| std::env::var_os(name).map(PathBuf::from);
    if let Some(local) = env("LOCALAPPDATA") {
        places.push(local.join("Microsoft").join("WinGet").join("Links"));
        places.push(local.join("ngrok"));
        places.push(local.join("cloudflared"));
    }
    for base in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(folder) = env(base) {
            places.push(folder.join("cloudflared"));
            places.push(folder.join("ngrok"));
        }
    }
    if let Some(home) = env("USERPROFILE") {
        places.push(home.join("scoop").join("shims"));
    }
    if let Some(data) = env("ProgramData") {
        places.push(data.join("chocolatey").join("bin"));
    }
    places
        .into_iter()
        .map(|folder| folder.join(&file))
        .find(|path| path.is_file())
}

/// True when ngrok already has an authtoken in its own configuration.
fn ngrok_has_token() -> bool {
    let env = |name: &str| std::env::var_os(name).map(PathBuf::from);
    let configs = [
        env("LOCALAPPDATA").map(|base| base.join("ngrok").join("ngrok.yml")),
        env("USERPROFILE").map(|home| home.join(".config").join("ngrok").join("ngrok.yml")),
        env("USERPROFILE").map(|home| home.join(".ngrok2").join("ngrok.yml")),
        env("HOME").map(|home| home.join(".config").join("ngrok").join("ngrok.yml")),
    ];
    configs.into_iter().flatten().any(|path| {
        std::fs::read_to_string(path)
            .map(|text| text.contains("authtoken:"))
            .unwrap_or(false)
    })
}

/// Fetches the tool for `kind` once. Returns where it is.
pub fn install(kind: Kind) -> Result<PathBuf, String> {
    if let Some(path) = find(name(kind)) {
        return Ok(path);
    }
    if !cfg!(windows) {
        return Err(format!(
            "Install {} with your package manager, then save Settings again.",
            name(kind)
        ));
    }
    let folder = tools_folder().ok_or("Could not find Feather's folder.")?;
    std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    let part = folder.join(format!("download-{}.part", super::auth::token(6)));
    let result = match kind {
        Kind::Cloudflare => {
            let target = folder.join("cloudflared.exe");
            download(CLOUDFLARED_URL, &part)
                .and_then(|()| verified(&part, "Cloudflare"))
                .and_then(|()| std::fs::rename(&part, &target).map_err(|error| error.to_string()))
                .map(|()| target)
        }
        _ => install_ngrok(&folder, &part),
    };
    let _ = std::fs::remove_file(&part);
    result
}

fn install_ngrok(folder: &Path, part: &Path) -> Result<PathBuf, String> {
    // winget checks the package against its published hash.
    if let Some(winget) = find("winget") {
        let mut command = Command::new(winget);
        command.args([
            "install",
            "--id",
            "Ngrok.Ngrok",
            "--exact",
            "--silent",
            "--disable-interactivity",
            "--accept-source-agreements",
            "--accept-package-agreements",
        ]);
        hide_window(&mut command);
        let _ = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Some(path) = find("ngrok") {
            return Ok(path);
        }
    }
    let unpacked = folder.join("ngrok-unpacked");
    let _ = std::fs::remove_dir_all(&unpacked);
    std::fs::create_dir_all(&unpacked).map_err(|error| error.to_string())?;
    download(NGROK_URL, part)?;
    let mut tar = Command::new(system_tool("tar.exe"));
    tar.arg("-xf").arg(part).arg("-C").arg(&unpacked);
    hide_window(&mut tar);
    let unpacked_ok = tar.status().map(|status| status.success()).unwrap_or(false);
    let inside = unpacked.join("ngrok.exe");
    if !unpacked_ok || !inside.is_file() {
        return Err("The ngrok download couldn't be unpacked.".into());
    }
    verified(&inside, "ngrok")?;
    let target = folder.join("ngrok.exe");
    std::fs::rename(&inside, &target).map_err(|error| error.to_string())?;
    let _ = std::fs::remove_dir_all(&unpacked);
    Ok(target)
}

fn system_tool(file: &str) -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join("System32").join(file))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(file))
}

/// Downloads `url` over HTTPS only, with Windows' own curl.
fn download(url: &str, to: &Path) -> Result<(), String> {
    let mut command = Command::new(system_tool("curl.exe"));
    command
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--tlsv1.2",
            "--max-time",
            "900",
            "--max-filesize",
            "300000000",
            "--output",
        ])
        .arg(to)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    hide_window(&mut command);
    let output = command
        .output()
        .map_err(|error| format!("Couldn't start the download: {error}"))?;
    if output.status.success() && to.is_file() {
        Ok(())
    } else {
        let reason = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!(
            "The download failed{}. Check the internet connection and save Settings again.",
            if reason.is_empty() {
                String::new()
            } else {
                format!(" ({reason})")
            }
        ))
    }
}

/// Accepts a program only with a valid Authenticode signature from `who`.
fn verified(path: &Path, who: &str) -> Result<(), String> {
    let literal = path.to_string_lossy().replace('\'', "''");
    let script = format!(
        "$s = Get-AuthenticodeSignature -LiteralPath '{literal}'; \
         if ($s.Status -eq 'Valid' -and $s.SignerCertificate.Subject -match 'O=\"?{who}') {{ exit 0 }} else {{ exit 1 }}"
    );
    let mut command = Command::new(system_tool("WindowsPowerShell\\v1.0\\powershell.exe"));
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
        ])
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hide_window(&mut command);
    let ok = command
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err(format!(
            "The downloaded {} wasn't signed by {who}, so Feather didn't use it.",
            path.file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default()
        ))
    }
}

/// Puts the tool in a job that ends with Feather, so it never outlives it.
#[cfg(windows)]
fn kill_with_feather(child: &Child) -> windows_sys::Win32::Foundation::HANDLE {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return job;
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        AssignProcessToJobObject(job, child.as_raw_handle() as _);
        job
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_addresses_are_read_from_the_logs() {
        let cloudflare =
            "2026-11-10T08:00:00Z INF |  https://quiet-river-1234.trycloudflare.com  |";
        assert_eq!(
            public_address(Kind::Cloudflare, cloudflare),
            Some("https://quiet-river-1234.trycloudflare.com".into())
        );
        assert_eq!(
            public_address(
                Kind::Cloudflare,
                "INF see https://developers.cloudflare.com/x"
            ),
            None
        );
        let ngrok = "t=2026 lvl=info msg=\"started tunnel\" obj=tunnels name=command_line addr=https://localhost:8443 url=https://ab12-34.ngrok-free.app";
        assert_eq!(
            public_address(Kind::Ngrok, ngrok),
            Some("https://ab12-34.ngrok-free.app".into())
        );
        assert_eq!(
            public_address(Kind::Ngrok, "lvl=info url=https://x.ngrok-free.app"),
            None
        );
    }

    #[test]
    fn stable_domains_are_cleaned() {
        assert_eq!(
            clean_domain(" https://Quiet-River.ngrok-free.app/ "),
            "quiet-river.ngrok-free.app"
        );
        assert_eq!(clean_domain("files.example.com:443"), "files.example.com");
        assert_eq!(clean_domain("not a domain"), "");
        assert_eq!(clean_domain(""), "");
        let custom = "lvl=info msg=\"started tunnel\" url=https://files.example.com";
        assert_eq!(
            public_address(Kind::Ngrok, custom),
            Some("https://files.example.com".into())
        );
    }

    #[test]
    fn downloads_come_only_from_the_publishers() {
        assert!(CLOUDFLARED_URL.starts_with("https://github.com/cloudflare/cloudflared/"));
        assert!(NGROK_URL.starts_with("https://bin.equinox.io/"));
    }
}
