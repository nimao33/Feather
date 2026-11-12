//! Asks GitHub whether a newer Feather is out. Nothing is downloaded or
//! installed here: the caller shows the news, and the person chooses to
//! open the download.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LATEST: &str = "https://api.github.com/repos/nimao33/Feather/releases/latest";
/// The release page, for when a release has no .msi.
pub const RELEASES: &str = "https://github.com/nimao33/Feather/releases/latest";
/// The least time between two automatic checks.
pub const EVERY: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// Without the leading "v", such as "1.1.0".
    pub version: String,
    /// The Windows installer, or the release page.
    pub download: String,
}

/// The newest release, or why it couldn't be found out.
pub fn latest() -> Result<Release, String> {
    let mut command = Command::new(curl());
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
            "20",
            "--max-filesize",
            "2000000",
            "--header",
            "Accept: application/vnd.github+json",
            "--user-agent",
            "Feather-update-check",
            LATEST,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
    let output = command
        .output()
        .map_err(|error| format!("Couldn't check: {error}"))?;
    if !output.status.success() {
        return Err("Couldn't reach GitHub. Check the internet connection.".into());
    }
    parse(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| "GitHub's answer didn't name a version.".into())
}

/// The newer release found this run, for Settings to offer.
static FOUND: Mutex<Option<Release>> = Mutex::new(None);

/// A newer release found by an earlier check this run.
pub fn found() -> Option<Release> {
    FOUND.lock().ok().and_then(|found| found.clone())
}

/// Asks for the newest release and remembers it when it is newer; notes
/// the time for [`due`].
pub fn check() -> Result<Release, String> {
    let release = latest()?;
    checked();
    if is_newer(&release.version) {
        if let Ok(mut found) = FOUND.lock() {
            *found = Some(release.clone());
        }
    }
    Ok(release)
}

/// Whether `version` is newer than this Feather.
pub fn is_newer(version: &str) -> bool {
    newer_than(version, env!("CARGO_PKG_VERSION"))
}

fn newer_than(version: &str, current: &str) -> bool {
    fn parts(version: &str) -> Vec<u64> {
        version
            .trim_start_matches('v')
            .split(['-', '+'])
            .next()
            .unwrap_or("")
            .split('.')
            .map(|part| part.parse().unwrap_or(0))
            .collect()
    }
    let (mut new, mut old) = (parts(version), parts(current));
    let len = new.len().max(old.len());
    new.resize(len, 0);
    old.resize(len, 0);
    new > old
}

/// The version and installer link in GitHub's release JSON.
fn parse(json: &str) -> Option<Release> {
    let version = strings(json, "tag_name")
        .into_iter()
        .next()?
        .trim_start_matches('v')
        .to_string();
    if version.is_empty() {
        return None;
    }
    let download = strings(json, "browser_download_url")
        .into_iter()
        .find(|url| url.ends_with(".msi") && url.starts_with("https://github.com/nimao33/Feather/"))
        .unwrap_or_else(|| RELEASES.to_string());
    Some(Release { version, download })
}

/// Every string value of `"key": "…"` in `json`, unescaped enough for
/// versions and URLs.
fn strings(json: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{key}\"");
    let mut found = Vec::new();
    let mut rest = json;
    while let Some(at) = rest.find(&needle) {
        rest = &rest[at + needle.len()..];
        let after = rest.trim_start();
        let Some(after) = after.strip_prefix(':') else {
            continue;
        };
        let Some(value) = after.trim_start().strip_prefix('"') else {
            continue;
        };
        let mut out = String::new();
        let mut chars = value.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => {
                    if let Some(next) = chars.next() {
                        out.push(next);
                    }
                }
                c => out.push(c),
            }
        }
        found.push(out);
    }
    found
}

fn curl() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join("System32").join("curl.exe"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("curl.exe"))
}

/// Where the time of the last automatic check is kept.
fn stamp() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|dir| PathBuf::from(dir).join("Feather").join("update-check"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Whether an automatic check is due, a day after the last one.
pub fn due() -> bool {
    let last = stamp()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(0);
    now().saturating_sub(last) >= EVERY.as_secs()
}

/// Notes that a check ran now.
pub fn checked() {
    if let Some(path) = stamp() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, now().to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(newer_than("1.1.0", "1.0.0"));
        assert!(newer_than("v1.10.0", "1.9.3"));
        assert!(newer_than("2", "1.9.9"));
        assert!(!newer_than("1.0.0", "1.0.0"));
        assert!(!newer_than("1.0", "1.0.0"));
        assert!(!newer_than("0.9.9", "1.0.0"));
        assert!(!newer_than("1.0.0-beta", "1.0.0"));
    }

    #[test]
    fn the_installer_link_is_found() {
        let json = r#"{"tag_name": "v1.1.0", "assets": [
            {"browser_download_url": "https://github.com/nimao33/Feather/releases/download/v1.1.0/feather-1.1.0-linux-x86_64.tar.gz"},
            {"browser_download_url":"https://github.com/nimao33/Feather/releases/download/v1.1.0/Feather-1.1.0-x64.msi"}]}"#;
        let release = parse(json).unwrap();
        assert_eq!(release.version, "1.1.0");
        assert!(release.download.ends_with("Feather-1.1.0-x64.msi"));
    }

    #[test]
    #[ignore = "asks GitHub over the network"]
    fn github_names_the_newest_release() {
        let release = latest().unwrap();
        assert!(!release.version.is_empty());
        assert!(release
            .download
            .starts_with("https://github.com/nimao33/Feather/"));
        println!("{release:?}");
    }

    #[test]
    fn links_elsewhere_are_not_followed() {
        let json = r#"{"tag_name":"v9.0.0","assets":[{"browser_download_url":"https://evil.example/x.msi"}]}"#;
        assert_eq!(parse(json).unwrap().download, RELEASES);
        assert_eq!(parse(r#"{"message":"Not Found"}"#), None);
    }
}
