//! The shared folders as the servers see them. Every path from a visitor is
//! a root number and a relative path, checked one name at a time and then
//! resolved on disk, so `..`, links, junctions, and odd Windows names can't
//! reach anything outside a shared folder.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use feather_core::{is_absolute_path, normalize_path, path_within};

/// Why a path was refused. Visitors only ever see "not found" for these,
/// so they can't tell a hidden file from a missing one.
#[derive(Debug, PartialEq, Eq)]
pub enum Denied {
    /// The path itself is malformed.
    BadName,
    /// Missing, outside the shared folders, hidden, or left out.
    NotFound,
}

pub struct Root {
    /// What visitors see: the folder's own name.
    pub name: String,
    /// The folder as Windows writes it, like `D:\Photos`.
    pub display: String,
    /// The resolved folder on disk.
    real: PathBuf,
}

/// One entry of a folder listing.
pub struct Item {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    /// Seconds since 1970.
    pub modified: u64,
}

pub struct Roots {
    pub roots: Vec<Root>,
    /// Folders never served: the "leave out" list and Feather's own data.
    blocked: Vec<String>,
    show_hidden: bool,
    show_system: bool,
}

impl Roots {
    /// The shared folders that exist. `errors` names any that don't.
    pub fn new(
        folders: &[String],
        excluded: &[String],
        show_hidden: bool,
        show_system: bool,
    ) -> (Self, Vec<String>) {
        let mut roots: Vec<Root> = Vec::new();
        let mut errors = Vec::new();
        for folder in folders {
            let real = match std::fs::canonicalize(folder) {
                Ok(real) if real.is_dir() => real,
                _ => {
                    errors.push(format!("{folder} is not a folder Feather can open."));
                    continue;
                }
            };
            let display = display_path(&real);
            let base = display
                .trim_end_matches(['\\', '/'])
                .rsplit(['\\', '/'])
                .next()
                .unwrap_or(&display)
                .trim_end_matches(':')
                .to_string();
            let base = if base.is_empty() {
                "Shared".into()
            } else {
                base
            };
            // Two folders with the same name get a number.
            let mut name = base.clone();
            let mut count = 2;
            while roots
                .iter()
                .any(|root| root.name.eq_ignore_ascii_case(&name))
            {
                name = format!("{base} ({count})");
                count += 1;
            }
            roots.push(Root {
                name,
                display,
                real,
            });
        }
        let mut blocked: Vec<String> = Vec::new();
        let feather = feather_folders()
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned());
        for folder in excluded.iter().cloned().chain(feather) {
            let mut forms = vec![normalize_path(&folder)];
            // Served paths are resolved, so a folder named through a link
            // or junction is also blocked where it really is.
            if is_absolute_path(&folder) {
                if let Ok(real) = std::fs::canonicalize(&folder) {
                    forms.push(normalize_path(&display_path(&real)));
                }
            }
            for form in forms {
                if !blocked.contains(&form) {
                    blocked.push(form);
                }
            }
        }
        (
            Self {
                roots,
                blocked,
                show_hidden,
                show_system,
            },
            errors,
        )
    }

    /// An existing file or folder inside shared folder `root`.
    pub fn resolve(&self, root: usize, relative: &str) -> Result<PathBuf, Denied> {
        let root = self.roots.get(root).ok_or(Denied::NotFound)?;
        let mut path = root.real.clone();
        for part in parts(relative) {
            check_name(part)?;
            path.push(part);
            // Every folder on the way must be one visitors may see.
            if !self.visible(&path) {
                return Err(Denied::NotFound);
            }
        }
        let real = std::fs::canonicalize(&path).map_err(|_| Denied::NotFound)?;
        if !inside(&root.real, &real) || self.is_blocked(&real) {
            return Err(Denied::NotFound);
        }
        Ok(real)
    }

    /// Where a new item called `name` would go in the shared folder at
    /// `parent`. The item may not exist yet.
    pub fn resolve_new(&self, root: usize, parent: &str, name: &str) -> Result<PathBuf, Denied> {
        check_name(name)?;
        if super::thumbs::risky_name(name) {
            return Err(Denied::BadName);
        }
        let folder = self.resolve(root, parent)?;
        if !folder.is_dir() {
            return Err(Denied::NotFound);
        }
        let path = folder.join(name);
        if self.is_blocked(&path) {
            return Err(Denied::NotFound);
        }
        // An item that is there but hidden from visitors can't be replaced,
        // and the answer doesn't tell it is there.
        if path.symlink_metadata().is_ok() && !self.visible(&path) {
            return Err(Denied::NotFound);
        }
        Ok(path)
    }

    /// True for the shared folder itself, which can't be renamed or deleted.
    pub fn is_root(&self, root: usize, path: &Path) -> bool {
        self.roots
            .get(root)
            .is_some_and(|root| same_path(&root.real, path))
    }

    /// The visible entries of a folder, folders first, then by name.
    pub fn list(&self, folder: &Path, limit: usize) -> Result<(Vec<Item>, bool), Denied> {
        let listing = std::fs::read_dir(folder).map_err(|_| Denied::NotFound)?;
        let mut items = Vec::new();
        let mut truncated = false;
        for entry in listing.flatten() {
            let path = entry.path();
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if !self.visible(&path) || self.is_blocked(&path) {
                continue;
            }
            // A link or junction is only listed when it stays inside.
            if entry
                .file_type()
                .map(|kind| kind.is_symlink())
                .unwrap_or(true)
                || is_reparse(&metadata)
            {
                let inside_root = std::fs::canonicalize(&path)
                    .ok()
                    .is_some_and(|real| self.roots.iter().any(|root| inside(&root.real, &real)));
                if !inside_root {
                    continue;
                }
            }
            if items.len() >= limit {
                truncated = true;
                break;
            }
            items.push(item(
                entry.file_name().to_string_lossy().to_string(),
                &metadata,
            ));
        }
        items.sort_by(|a, b| {
            b.dir
                .cmp(&a.dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok((items, truncated))
    }

    /// The shared folder and relative path of an indexed path, when it is
    /// shared and visible.
    pub fn locate(&self, path: &str) -> Option<(usize, String)> {
        let (index, _, relative) = self
            .roots
            .iter()
            .enumerate()
            .filter_map(|(index, root)| {
                relative_to(path, &root.display).map(|relative| (index, root, relative))
            })
            .max_by_key(|(_, root, _)| root.display.len())?;
        let real = self.resolve(index, &relative).ok()?;
        self.visible(&real).then_some((index, relative))
    }

    /// The metadata of a resolved path as a listing entry.
    pub fn describe(&self, path: &Path) -> Option<Item> {
        let metadata = std::fs::metadata(path).ok()?;
        let name = path.file_name()?.to_string_lossy().to_string();
        Some(item(name, &metadata))
    }

    /// True when `folder` is, or holds, a left-out folder: moving it would
    /// carry that folder somewhere it is no longer left out.
    pub fn holds_blocked(&self, folder: &Path) -> bool {
        let display = display_path(folder);
        self.blocked
            .iter()
            .any(|blocked| path_within(blocked, &display) || path_within(&display, blocked))
    }

    pub(crate) fn is_blocked(&self, path: &Path) -> bool {
        let display = display_path(path);
        self.blocked
            .iter()
            .any(|blocked| path_within(&display, blocked))
    }

    /// Hidden and system items follow the "What to show" settings.
    pub(crate) fn visible(&self, path: &Path) -> bool {
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return true;
        };
        let (hidden, system) = attributes(path, &metadata);
        (self.show_hidden || !hidden) && (self.show_system || !system)
    }
}

fn item(name: String, metadata: &std::fs::Metadata) -> Item {
    Item {
        name,
        dir: metadata.is_dir(),
        size: if metadata.is_dir() { 0 } else { metadata.len() },
        modified: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|time| time.as_secs())
            .unwrap_or(0),
    }
}

#[cfg(windows)]
fn attributes(_path: &Path, metadata: &std::fs::Metadata) -> (bool, bool) {
    use std::os::windows::fs::MetadataExt;
    let flags = metadata.file_attributes();
    (flags & 0x2 != 0, flags & 0x4 != 0)
}

#[cfg(not(windows))]
fn attributes(path: &Path, _metadata: &std::fs::Metadata) -> (bool, bool) {
    let hidden = path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with('.'));
    (hidden, false)
}

#[cfg(windows)]
pub(crate) fn is_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
pub(crate) fn is_reparse(_metadata: &std::fs::Metadata) -> bool {
    false
}

/// `name` inside the relative folder `parent`.
pub fn join_relative(parent: &str, name: &str) -> String {
    let parent = parent.trim_matches(['/', '\\']);
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

/// The names in a relative path, either slash.
fn parts(relative: &str) -> impl Iterator<Item = &str> {
    relative.split(['/', '\\']).filter(|part| !part.is_empty())
}

/// One file or folder name from a visitor. Refuses anything Windows would
/// read as something else: `..`, drive letters and streams (`:`),
/// wildcards, device names like `CON`, and trailing dots or spaces.
pub fn check_name(name: &str) -> Result<(), Denied> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 255
        || name.ends_with('.')
        || name.ends_with(' ')
        || name.chars().any(|c| {
            c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        })
    {
        return Err(Denied::BadName);
    }
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    let reserved = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];
    let numbered = ["COM", "LPT"];
    let upper = stem.to_ascii_uppercase();
    if reserved.contains(&upper.as_str())
        || numbered.iter().any(|prefix| {
            upper.len() == 4
                && upper.starts_with(prefix)
                && upper[3..]
                    .chars()
                    .all(|c| c.is_ascii_digit() || "¹²³".contains(c))
        })
    {
        return Err(Denied::BadName);
    }
    Ok(())
}

/// `real` is `root` or inside it. Both are resolved paths.
#[cfg(windows)]
pub(crate) fn inside(root: &Path, real: &Path) -> bool {
    let root = root.to_string_lossy().to_lowercase();
    let real = real.to_string_lossy().to_lowercase();
    let root = root.trim_end_matches(['\\', '/']);
    real == root
        || real
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('\\') || rest.starts_with('/'))
}

/// `real` is `root` or inside it. Both are resolved paths. Names are case
/// sensitive here: `/srv/Share` is not inside `/srv/share`, so a link to a
/// folder that differs only in case stays outside.
#[cfg(not(windows))]
pub(crate) fn inside(root: &Path, real: &Path) -> bool {
    real.starts_with(root)
}

/// The path below shared folder `root`, with forward slashes, for an
/// indexed path.
#[cfg(windows)]
fn relative_to(path: &str, root: &str) -> Option<String> {
    if !path_within(path, root) {
        return None;
    }
    Some(
        path.get(root.trim_end_matches('\\').len()..)?
            .trim_start_matches('\\')
            .replace('\\', "/"),
    )
}

/// The path below shared folder `root` for an indexed path, compared the
/// way the file system does: one name at a time and case sensitive.
#[cfg(not(windows))]
fn relative_to(path: &str, root: &str) -> Option<String> {
    let rest = Path::new(path).strip_prefix(root).ok()?.to_str()?;
    // A backslash is part of a name here, and no visitor path can name it.
    if rest.contains('\\') {
        return None;
    }
    Some(rest.to_string())
}

pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .eq_ignore_ascii_case(b.to_string_lossy().trim_end_matches(['\\', '/']))
}

/// Moves a file or folder to the Recycle Bin, so a visitor's delete can be
/// undone.
#[cfg(windows)]
pub fn recycle(path: &Path) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE,
        SHFILEOPSTRUCTW,
    };
    // The shell wants a plain path, ended by two nulls.
    let wide: Vec<u16> = display_path(path).encode_utf16().chain([0, 0]).collect();
    let mut operation: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    operation.wFunc = FO_DELETE;
    operation.pFrom = wide.as_ptr();
    operation.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT) as u16;
    let result = unsafe { SHFileOperationW(&mut operation) };
    if result == 0 && operation.fAnyOperationsAborted == 0 {
        Ok(())
    } else {
        Err(format!(
            "Windows couldn't move it to the Recycle Bin (code {result})."
        ))
    }
}

/// Moves a file or folder to the desktop's trash (the freedesktop.org
/// trash in `~/.local/share/Trash`), so a visitor's delete can be undone.
/// Something on another drive goes to that drive's own trash; when
/// neither can take it, the page asks whether to delete it for good.
#[cfg(not(windows))]
pub fn recycle(path: &Path) -> Result<(), String> {
    // The home trash, then the drive's own `.Trash-<uid>` for something on
    // another drive, which can't be moved home.
    let mut trashes: Vec<PathBuf> = trash_folder().into_iter().collect();
    trashes.extend(drive_trash(path));
    for trash in trashes {
        if into_trash(&trash, path) {
            return Ok(());
        }
    }
    Err("It can't be moved to the trash on the computer running Feather.".into())
}

/// The trash at the top of the drive holding `path` (freedesktop.org's
/// `$topdir/.Trash-$uid`).
#[cfg(not(windows))]
fn drive_trash(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;
    let device = std::fs::symlink_metadata(path).ok()?.dev();
    let uid = std::fs::metadata("/proc/self").ok()?.uid();
    let mut top = path.parent()?;
    while let Some(parent) = top.parent() {
        match std::fs::metadata(parent) {
            Ok(metadata) if metadata.dev() == device => top = parent,
            _ => break,
        }
    }
    Some(top.join(format!(".Trash-{uid}")))
}

/// Moves `path` into `trash` with its note; false when it can't go there.
#[cfg(not(windows))]
fn into_trash(trash: &Path, path: &Path) -> bool {
    use std::io::Write as _;
    use std::os::unix::fs::DirBuilderExt as _;
    let (files, info) = (trash.join("files"), trash.join("info"));
    for folder in [trash, files.as_path(), info.as_path()] {
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(folder);
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let Some(name) = name else {
        return false;
    };
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name.as_str(), ""),
    };
    for number in 1..10_000 {
        let candidate = if number == 1 {
            name.clone()
        } else {
            format!("{stem}.{number}{extension}")
        };
        let note = info.join(format!("{candidate}.trashinfo"));
        let Ok(mut file) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&note)
        else {
            continue;
        };
        if std::fs::symlink_metadata(files.join(&candidate)).is_ok() {
            drop(file);
            let _ = std::fs::remove_file(&note);
            continue;
        }
        let written = write!(
            file,
            "[Trash Info]
Path={}
DeletionDate={}
",
            trash_path(path),
            super::timestamp().replace(' ', "T")
        );
        drop(file);
        if written.is_ok() && std::fs::rename(path, files.join(&candidate)).is_ok() {
            return true;
        }
        let _ = std::fs::remove_file(&note);
        return false;
    }
    false
}

/// The user's trash folder.
#[cfg(not(windows))]
fn trash_folder() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".local").join("share"))
        })?;
    Some(data.join("Trash"))
}

/// A path as a `.trashinfo` file writes it: percent-encoded bytes.
#[cfg(not(windows))]
fn trash_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut out = String::new();
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-_.~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Deletes a file or folder for good, when the person said so after the
/// trash couldn't take it.
pub fn remove(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    let result = if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    result.map_err(|error| format!("It couldn't be deleted: {error}"))
}

/// A name beside `taken` that nothing has yet, like `photo (2).jpg` for
/// `photo.jpg`, or `None` after many tries.
pub fn free_name(taken: &Path) -> Option<PathBuf> {
    let folder = taken.parent()?;
    let name = taken.file_name()?.to_str()?;
    // A folder's dots are part of its name; a file keeps its extension.
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 && !taken.is_dir() => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    (2..10_000).find_map(|number| {
        let candidate = format!("{stem} ({number}){extension}");
        let path = folder.join(&candidate);
        (check_name(&candidate).is_ok() && std::fs::symlink_metadata(&path).is_err())
            .then_some(path)
    })
}

/// A resolved path the way Windows shows it: without `\\?\`.
pub fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text.to_string()
    }
}

/// Feather's settings, certificate, and index folders, never served even
/// when a shared folder contains them.
fn feather_folders() -> Vec<PathBuf> {
    let mut folders = Vec::new();
    // Settings and the sharing certificate, password pepper, and keys.
    if let Some(settings) = crate::settings::file_path() {
        if let Some(folder) = settings.parent() {
            folders.push(folder.to_path_buf());
        }
    }
    folders.extend(data_folders());
    folders
}

#[cfg(windows)]
fn data_folders() -> Vec<PathBuf> {
    ["APPDATA", "LOCALAPPDATA"]
        .into_iter()
        .filter_map(std::env::var_os)
        .filter(|base| !base.is_empty())
        .map(|base| PathBuf::from(base).join("Feather"))
        .collect()
}

/// The index and tunnel tools (`~/.local/share/feather`, also spelled
/// `Feather`, which blocking matches regardless of case) and the running
/// window's port and key (`$XDG_RUNTIME_DIR/feather` or `~/.cache/feather`).
#[cfg(not(windows))]
fn data_folders() -> Vec<PathBuf> {
    let env = |name: &str| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let mut bases = Vec::new();
    bases.extend(env("XDG_DATA_HOME"));
    bases.extend(env("XDG_RUNTIME_DIR"));
    bases.extend(env("LOCALAPPDATA"));
    if let Some(home) = env("HOME") {
        bases.push(home.join(".local").join("share"));
        bases.push(home.join(".cache"));
    }
    let mut folders: Vec<PathBuf> = bases.into_iter().map(|base| base.join("feather")).collect();
    // What was deleted, from the share or anywhere else, stays deleted.
    folders.extend(trash_folder());
    folders
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::MAIN_SEPARATOR as SEP;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("feather-share-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn names_that_mean_something_else_are_refused() {
        for name in [
            "..",
            ".",
            "a:b",
            "C:",
            "file.txt:stream",
            "CON",
            "con.txt",
            "COM1",
            "lpt9.log",
            "x*",
            "a?",
            "tail.",
            "tail ",
            "a/b",
            "a\\b",
            "",
            "nul",
        ] {
            assert_eq!(check_name(name), Err(Denied::BadName), "{name}");
        }
        for name in [
            "report.txt",
            "COMPUTER",
            "Console notes",
            "..hidden",
            "a.b.c",
        ] {
            assert_eq!(check_name(name), Ok(()), "{name}");
        }
    }

    #[test]
    fn paths_stay_inside_the_shared_folder() {
        let base = temp("inside");
        let shared = base.join("shared");
        let secret = base.join("secret");
        std::fs::create_dir_all(shared.join("docs")).unwrap();
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(shared.join("docs").join("a.txt"), b"a").unwrap();
        std::fs::write(secret.join("key.txt"), b"k").unwrap();
        let (roots, errors) = Roots::new(&[shared.to_string_lossy().to_string()], &[], true, true);
        assert!(errors.is_empty());
        assert!(roots.resolve(0, "docs/a.txt").is_ok());
        assert!(roots.resolve(0, "").is_ok());
        assert_eq!(roots.resolve(0, "../secret/key.txt"), Err(Denied::BadName));
        assert_eq!(roots.resolve(0, "docs/../../secret"), Err(Denied::BadName));
        assert!(roots.resolve(0, "missing.txt").is_err());
        assert!(roots.resolve(1, "").is_err());
        let (items, truncated) = roots.list(&roots.resolve(0, "docs").unwrap(), 100).unwrap();
        assert_eq!(items.len(), 1);
        assert!(!truncated);
        let display = format!("{}{SEP}docs{SEP}a.txt", roots.roots[0].display);
        assert_eq!(roots.locate(&display), Some((0, "docs/a.txt".into())));
        let outside = format!(
            "{}{SEP}key.txt",
            display_path(&std::fs::canonicalize(&secret).unwrap())
        );
        assert_eq!(roots.locate(&outside), None);
        // A sibling whose name starts with the shared folder's is outside.
        let sibling = format!("{}-not{SEP}a.txt", roots.roots[0].display);
        assert_eq!(roots.locate(&sibling), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn left_out_folders_are_not_served() {
        let base = temp("blocked");
        std::fs::create_dir_all(base.join("private")).unwrap();
        std::fs::write(base.join("private").join("p.txt"), b"p").unwrap();
        let real = display_path(&std::fs::canonicalize(&base).unwrap());
        let (roots, _) = Roots::new(
            &[base.to_string_lossy().to_string()],
            &[format!("{real}{SEP}private")],
            true,
            true,
        );
        assert!(roots.resolve(0, "private/p.txt").is_err());
        assert!(roots.resolve(0, "private").is_err());
        let (items, _) = roots.list(&roots.resolve(0, "").unwrap(), 100).unwrap();
        assert!(items.is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn feathers_own_folders_are_always_left_out() {
        let (roots, _) = Roots::new(&[], &[], true, true);
        let settings = crate::settings::file_path().expect("settings path");
        let folder = normalize_path(&settings.parent().unwrap().to_string_lossy());
        assert!(roots.blocked.contains(&folder), "{:?}", roots.blocked);
    }

    /// On a case-sensitive file system `share` and `SHARE` are different
    /// folders; a link from one to the other must not reach outside.
    #[cfg(unix)]
    #[test]
    fn links_to_a_folder_that_differs_only_in_case_stay_outside() {
        let base = temp("case");
        let shared = base.join("share");
        let other = base.join("SHARE");
        std::fs::create_dir_all(&shared).unwrap();
        if std::fs::create_dir(&other).is_err() {
            // A case-insensitive file system: there is no second folder.
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        std::fs::write(other.join("secret.txt"), b"s").unwrap();
        std::os::unix::fs::symlink(&other, shared.join("link")).unwrap();
        std::os::unix::fs::symlink(other.join("secret.txt"), shared.join("file")).unwrap();
        let (roots, errors) = Roots::new(&[shared.to_string_lossy().to_string()], &[], true, true);
        assert!(errors.is_empty());
        assert_eq!(roots.resolve(0, "link/secret.txt"), Err(Denied::NotFound));
        assert_eq!(roots.resolve(0, "file"), Err(Denied::NotFound));
        let (items, _) = roots.list(&roots.resolve(0, "").unwrap(), 100).unwrap();
        assert!(items.is_empty(), "links to outside were listed");
        let real_other = display_path(&std::fs::canonicalize(&other).unwrap());
        assert_eq!(roots.locate(&format!("{real_other}/secret.txt")), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A left-out folder named through a link is blocked where it really is.
    #[cfg(unix)]
    #[test]
    fn left_out_folders_named_through_a_link_are_not_served() {
        let base = temp("blocked-link");
        std::fs::create_dir_all(base.join("shared").join("private")).unwrap();
        std::fs::write(base.join("shared").join("private").join("p.txt"), b"p").unwrap();
        let alias = base.join("alias");
        std::os::unix::fs::symlink(base.join("shared"), &alias).unwrap();
        let (roots, _) = Roots::new(
            &[base.join("shared").to_string_lossy().to_string()],
            &[alias.join("private").to_string_lossy().to_string()],
            true,
            true,
        );
        assert!(roots.resolve(0, "private/p.txt").is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
