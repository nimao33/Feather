//! Shortcuts to Feather in the Start menu and on the desktop, for the
//! person signed in. The switches in Settings look for the files
//! themselves, so they always show what is really there.

use std::path::PathBuf;

use windows::core::{Interface, HSTRING};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Programs, IShellLinkW, SHGetKnownFolderPath, ShellLink,
    KF_FLAG_DEFAULT,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Place {
    StartMenu,
    Desktop,
}

/// The folder the shortcut goes in: the Start menu's Programs folder or the
/// desktop.
fn folder(place: Place) -> Option<PathBuf> {
    let id = match place {
        Place::StartMenu => &FOLDERID_Programs,
        Place::Desktop => &FOLDERID_Desktop,
    };
    unsafe {
        let path = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as *const _));
        text.map(PathBuf::from)
    }
}

fn link(place: Place) -> Option<PathBuf> {
    Some(folder(place)?.join("Feather.lnk"))
}

/// True when the shortcut is there.
pub fn exists(place: Place) -> bool {
    link(place).is_some_and(|path| path.is_file())
}

/// Adds the shortcut, pointing at this copy of Feather, or removes it.
pub fn set(place: Place, on: bool) -> Result<(), String> {
    let path = link(place).ok_or("could not find the folder")?;
    if !on {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        };
    }
    let exe = std::env::current_exe().map_err(|_| "could not find Feather's own path")?;
    write_link(&path, &exe)
}

/// Writes a shortcut file at `path` that starts `exe`.
fn write_link(path: &std::path::Path, exe: &std::path::Path) -> Result<(), String> {
    let fail = |error: windows::core::Error| error.message();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let shortcut: IShellLinkW =
            CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(fail)?;
        shortcut
            .SetPath(&HSTRING::from(exe.as_os_str()))
            .map_err(fail)?;
        if let Some(dir) = exe.parent() {
            shortcut
                .SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))
                .map_err(fail)?;
        }
        shortcut
            .SetDescription(&HSTRING::from("Find files and folders by name"))
            .map_err(fail)?;
        shortcut
            .SetIconLocation(&HSTRING::from(exe.as_os_str()), 0)
            .map_err(fail)?;
        let file: IPersistFile = shortcut.cast().map_err(fail)?;
        file.Save(&HSTRING::from(path.as_os_str()), true)
            .map_err(fail)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_shortcut_file() {
        let dir = std::env::temp_dir().join(format!("feather-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("Feather.lnk");
        let exe = std::env::current_exe().expect("exe");
        write_link(&path, &exe).expect("write");
        // A shell link starts with its 0x4C header size.
        let bytes = std::fs::read(&path).expect("read");
        assert_eq!(&bytes[..4], &[0x4c, 0, 0, 0]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
