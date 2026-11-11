//! Index libraries on disk, the same on every system.
//!
//! Each build writes a complete new library in its own folder beside the
//! base path (`index.g<time>` next to `index`) and marks it complete. The
//! open index stays usable the whole time. When the build finishes, the app
//! opens the new folder, records it in `index.current`, and deletes the
//! older folders once nothing has them open. Nothing is renamed, so a
//! mapped index never blocks an update.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use feather_core::scan::{library_dir_name, scan, ScanOptions};
use feather_core::{normalize_path, path_within};

/// Marks a staged library as complete, so a crash mid-scan is never
/// mistaken for a finished index.
pub const COMPLETE_MARKER: &str = "complete.txt";

/// Where a library's next version is built: a new folder each time.
pub fn staging_dir(library: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    sibling(library, &format!("g{nanos:x}"))
}

/// The file beside the base path naming the library folder in use.
fn pointer(library: &Path) -> PathBuf {
    sibling(library, "current")
}

/// The library folder to open: the newest finished build, or the base path.
pub fn active_library(library: &Path) -> PathBuf {
    std::fs::read_to_string(pointer(library))
        .ok()
        .map(|name| library.with_file_name(name.trim()))
        .filter(|path| path.join(COMPLETE_MARKER).is_file())
        .unwrap_or_else(|| library.to_path_buf())
}

/// Records `staged` as the library to open from now on.
pub fn activate(library: &Path, staged: &Path) -> std::io::Result<()> {
    if !staged.join(COMPLETE_MARKER).is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "the new index is not complete",
        ));
    }
    let name = staged
        .file_name()
        .ok_or_else(|| std::io::Error::other("the new index has no folder name"))?;
    let target = pointer(library);
    let temp = sibling(library, "current.tmp");
    std::fs::write(&temp, name.to_string_lossy().as_bytes())?;
    std::fs::rename(&temp, &target)
}

/// Deletes library folders other than the active one. Folders still open
/// fail to delete and are tried again next time.
pub fn cleanup(library: &Path) {
    let active = active_library(library);
    let Some(parent) = library.parent() else {
        return;
    };
    let Some(base) = library
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
    else {
        return;
    };
    let Ok(listing) = std::fs::read_dir(parent) else {
        return;
    };
    for item in listing.flatten() {
        let name = item.file_name().to_string_lossy().to_string();
        let ours = name == base
            || name
                .strip_prefix(&format!("{base}."))
                .is_some_and(|rest| rest == "new" || rest == "old" || rest.starts_with('g'));
        let path = item.path();
        if ours && path != active && path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// The library a path belongs to. Given one of its built versions
/// (`index.g<time>`, marked complete), that is the base path beside it:
/// treating a version as a library of its own would build the next version
/// next to it and then delete it as out of date.
pub fn library_of(path: PathBuf) -> PathBuf {
    let version = path.join(COMPLETE_MARKER).is_file();
    let base = path.file_name().and_then(|name| {
        let name = name.to_str()?;
        let (base, stamp) = name.rsplit_once('.')?;
        let hex = stamp.strip_prefix('g')?;
        (!base.is_empty() && !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| base.to_string())
    });
    match base {
        Some(base) if version => path.with_file_name(base),
        _ => path,
    }
}

fn sibling(library: &Path, suffix: &str) -> PathBuf {
    let name = library
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "index".to_string());
    library.with_file_name(format!("{name}.{suffix}"))
}

/// Settings store drives without the trailing backslash (`C:`), but `C:`
/// alone means the current folder on drive C, so add it back.
pub fn root_path(root: &str) -> PathBuf {
    let root = root.trim();
    if root.len() == 2 && root.ends_with(':') {
        PathBuf::from(format!("{root}\\"))
    } else {
        PathBuf::from(root)
    }
}

/// Drops roots that sit inside another root, and duplicates, so nothing is
/// indexed twice.
pub fn outermost_roots(roots: &[String]) -> Vec<String> {
    let mut keys: Vec<(String, String)> = roots
        .iter()
        .map(|root| root.trim())
        .filter(|root| !root.is_empty())
        .map(|root| (normalize_path(root), root.to_string()))
        .collect();
    keys.sort_by(|a, b| a.0.len().cmp(&b.0.len()).then_with(|| a.0.cmp(&b.0)));
    let mut kept: Vec<(String, String)> = Vec::new();
    for (key, root) in keys {
        if !kept
            .iter()
            .any(|(outer, _)| *outer == key || path_within(&key, outer))
        {
            kept.push((key, root));
        }
    }
    kept.into_iter().map(|(_, root)| root).collect()
}

/// Puts a finished root's files into the new library without reading the
/// disk again. Index files never change once written, so a hard link does;
/// a copy is the fallback.
pub fn carry_over(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for item in std::fs::read_dir(from)? {
            let item = item?;
            carry_over(&item.path(), &to.join(item.file_name()))?;
        }
        return Ok(());
    }
    if std::fs::hard_link(from, to).is_err() {
        std::fs::copy(from, to)?;
    }
    Ok(())
}

/// At startup: an older version's finished `index.new` becomes the active
/// library, and unfinished or replaced builds are deleted.
pub fn apply_pending(library: &Path) {
    let legacy = sibling(library, "new");
    if legacy.join(COMPLETE_MARKER).is_file() {
        let _ = activate(library, &legacy);
    }
    cleanup(library);
}

/// Builds a library for `roots` by walking their folders, the way every
/// system can. `reuse` names roots to carry over from the library in use
/// instead of reading them again. Returns the staged folder, ready for
/// `activate`, and the roots that couldn't be read.
pub fn build_walk(
    library: &Path,
    roots: &[String],
    reuse: &[String],
    options: &ScanOptions,
    cancel: &Arc<AtomicBool>,
) -> Result<(PathBuf, Vec<String>), String> {
    let staged = staging_dir(library);
    std::fs::create_dir_all(&staged)
        .map_err(|error| format!("Could not create {}: {error}", staged.display()))?;
    let active = active_library(library);
    let roots = outermost_roots(roots);
    let mut failures = Vec::new();
    for root in &roots {
        if cancel.load(Ordering::Relaxed) {
            let _ = std::fs::remove_dir_all(&staged);
            return Err("cancelled".into());
        }
        let path = root_path(root);
        let name = library_dir_name(&path);
        let target = staged.join(&name);
        if reuse.contains(root) && carry_over(&active.join(&name), &target).is_ok() {
            continue;
        }
        let _ = std::fs::remove_dir_all(&target);
        if let Err(error) = scan(&path, &target, options) {
            failures.push(format!("{root}: {error}"));
        }
    }
    if !roots.is_empty() && failures.len() == roots.len() {
        let _ = std::fs::remove_dir_all(&staged);
        return Err(failures.join("; "));
    }
    std::fs::write(staged.join(COMPLETE_MARKER), b"ok")
        .map_err(|error| format!("Could not finish the index: {error}"))?;
    Ok((staged, failures))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_built_version_points_back_to_its_library() {
        let root = std::env::temp_dir().join(format!("feather-version-{}", std::process::id()));
        let version = root.join("index.g18e6f72174c3a94c");
        std::fs::create_dir_all(&version).unwrap();
        // Without the marker it is just a folder with an odd name.
        assert_eq!(library_of(version.clone()), version);
        std::fs::write(version.join(COMPLETE_MARKER), "1").unwrap();
        assert_eq!(library_of(version.clone()), root.join("index"));
        assert_eq!(library_of(root.join("index")), root.join("index"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
