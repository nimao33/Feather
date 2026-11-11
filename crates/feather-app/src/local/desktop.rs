//! Fitting into the desktop, whichever it is: opening the window, files,
//! and folders, and the applications-menu and autostart entries. Everything
//! goes through freedesktop.org standards (`xdg-open`, `.desktop` files,
//! the FileManager1 D-Bus interface) that GNOME, KDE, Xfce, Cinnamon, MATE,
//! LXQt, and window managers with a launcher all follow.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const ICON_SVG: &str = include_str!("../share/web/icon.svg");

/// The user's home folder.
pub fn home() -> Option<String> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .filter(|home| !home.is_empty())
}

fn on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|folder| folder.join(program))
        .find(|candidate| candidate.is_file())
}

/// Starts a program without waiting for it and without its output.
fn launch(program: &Path, args: &[&str]) -> Result<(), String> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Couldn't start {}: {error}", program.display()))
}

/// Opens Feather's window: an app window of a Chromium-family browser when
/// one is installed (no tabs or address bar, its own taskbar entry), else
/// the default browser. `FEATHER_BROWSER` picks one by hand.
pub fn open_browser(url: &str) -> Result<(), String> {
    if let Ok(choice) = std::env::var("FEATHER_BROWSER") {
        let mut parts = choice.split_whitespace();
        if let Some(program) = parts.next() {
            let mut args: Vec<String> = parts.map(str::to_string).collect();
            args.push(url.to_string());
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            return launch(Path::new(program), &args);
        }
    }
    if cfg!(windows) {
        return windows_open(url);
    }
    if cfg!(target_os = "macos") {
        return launch(Path::new("open"), &[url]);
    }
    let app_flag = format!("--app={url}");
    for browser in [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "brave-browser",
        "brave",
        "microsoft-edge",
        "microsoft-edge-stable",
        "vivaldi",
        "vivaldi-stable",
    ] {
        if let Some(path) = on_path(browser) {
            return launch(&path, &[&app_flag, "--class=Feather", "--name=Feather"]);
        }
    }
    for opener in ["xdg-open", "gio", "sensible-browser", "firefox"] {
        if let Some(path) = on_path(opener) {
            let args: &[&str] = if opener == "gio" {
                &["open", url]
            } else {
                &[url]
            };
            return launch(&path, args);
        }
    }
    Err(format!("No browser was found. Open {url} by hand."))
}

#[cfg(windows)]
fn windows_open(target: &str) -> Result<(), String> {
    crate::ui::context_menu::open_item(std::ptr::null_mut(), target)
}

#[cfg(not(windows))]
fn windows_open(_target: &str) -> Result<(), String> {
    Err("not on Windows".into())
}

/// Opens a file or folder with its default app.
pub fn open_path(path: &Path) -> Result<(), String> {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        return windows_open(&text);
    }
    if cfg!(target_os = "macos") {
        return launch(Path::new("open"), &[&text]);
    }
    for opener in ["xdg-open", "gio"] {
        if let Some(program) = on_path(opener) {
            let args: Vec<&str> = if opener == "gio" {
                vec!["open", &text]
            } else {
                vec![&text]
            };
            return launch(&program, &args);
        }
    }
    Err("Install xdg-utils to open files from Feather.".into())
}

/// A path as a `file://` address, percent-encoded.
fn file_url(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut out = String::from("file://");
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Shows an item in the file manager, selected when it can be.
pub fn reveal(path: &Path) -> Result<(), String> {
    if cfg!(windows) {
        #[cfg(windows)]
        return crate::ui::context_menu::reveal_item(std::ptr::null_mut(), &path.to_string_lossy());
    }
    if cfg!(target_os = "macos") {
        return launch(Path::new("open"), &["-R", &path.to_string_lossy()]);
    }
    // Nautilus, Dolphin, Nemo, Caja, Thunar and others answer this.
    if let Some(dbus) = on_path("dbus-send") {
        let item = format!("array:string:{}", file_url(path));
        let shown = Command::new(dbus)
            .args([
                "--session",
                "--print-reply",
                "--dest=org.freedesktop.FileManager1",
                "--type=method_call",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItems",
                &item,
                "string:",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if shown {
            return Ok(());
        }
    }
    let folder = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().map(Path::to_path_buf).unwrap_or_default()
    };
    open_path(&folder)
}

fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home().map(|home| PathBuf::from(home).join(".local").join("share")))
}

fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home().map(|home| PathBuf::from(home).join(".config")))
}

fn menu_file() -> Option<PathBuf> {
    Some(data_home()?.join("applications").join("feather.desktop"))
}

fn autostart_file() -> Option<PathBuf> {
    Some(config_home()?.join("autostart").join("feather.desktop"))
}

fn icon_file() -> Option<PathBuf> {
    Some(
        data_home()?
            .join("icons")
            .join("hicolor")
            .join("scalable")
            .join("apps")
            .join("feather.svg"),
    )
}

/// The `.desktop` entry, starting this copy of Feather.
fn desktop_entry(background: bool) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    // Exec quoting per the Desktop Entry spec.
    let quoted = format!(
        "\"{}\"",
        exe.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('`', "\\`")
            .replace('$', "\\$")
    );
    let exec = if background {
        format!("{quoted} --background")
    } else {
        quoted
    };
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Feather\n\
         GenericName=File Search\n\
         Comment=Find files and folders by name as you type\n\
         Exec={exec}\n\
         Icon=feather\n\
         Terminal=false\n\
         Categories=Utility;FileTools;\n\
         Keywords=search;find;files;folders;everything;\n\
         StartupWMClass=Feather\n\
         {}",
        if background {
            "X-GNOME-Autostart-enabled=true\nNoDisplay=true\n"
        } else {
            ""
        }
    ))
}

fn write_file(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(path, text)
        .map_err(|error| format!("Couldn't write {}: {error}", path.display()))
}

pub fn menu_entry_exists() -> bool {
    menu_file().is_some_and(|path| path.is_file())
}

/// Adds Feather to the applications menu (and its icon), or removes it.
pub fn set_menu_entry(on: bool) -> Result<(), String> {
    let path = menu_file().ok_or("Couldn't find the applications folder.")?;
    if !on {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    if let Some(icon) = icon_file() {
        write_file(&icon, ICON_SVG)?;
    }
    write_file(&path, &desktop_entry(false)?)?;
    // Menus pick up new entries faster when told.
    if let Some(update) = on_path("update-desktop-database") {
        if let Some(folder) = path.parent() {
            let _ = launch(&update, &[&folder.to_string_lossy()]);
        }
    }
    Ok(())
}

pub fn autostart_enabled() -> bool {
    autostart_file().is_some_and(|path| path.is_file())
}

/// Starts Feather in the background at sign-in, or stops doing so.
pub fn set_autostart(on: bool) -> Result<(), String> {
    let path = autostart_file().ok_or("Couldn't find the autostart folder.")?;
    if on {
        write_file(&path, &desktop_entry(true)?)
    } else {
        let _ = std::fs::remove_file(&path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_and_addresses() {
        let entry = desktop_entry(true).unwrap();
        assert!(entry.contains("Exec=\""));
        assert!(entry.ends_with("NoDisplay=true\n"));
        assert!(entry.contains("StartupWMClass=Feather"));
        assert_eq!(
            file_url(Path::new("/home/me/My file.txt")),
            "file:///home/me/My%20file.txt"
        );
    }
}
