//! Building the index from inside the Windows app: the MFT reader, the
//! elevated helper, and the background thread. Library folders themselves
//! are handled in `crate::library`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;

use feather_core::mft;
use feather_core::scan::{library_dir_name, scan, write_tree, ScanOptions};

use super::helper;
pub use crate::library::{
    activate, active_library, apply_pending, cleanup, outermost_roots, root_path, staging_dir,
};
use crate::library::{carry_over, COMPLETE_MARKER};
use feather_core::{normalize_path, path_within};

pub enum IndexEvent {
    /// Started scanning this root.
    Root {
        root: String,
    },
    Done {
        staged: PathBuf,
        failures: Vec<String>,
    },
    Failed(String),
    Cancelled,
}

pub struct Indexer {
    progress: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
    events: Receiver<IndexEvent>,
    thread: Option<JoinHandle<()>>,
    /// Entries found in roots that are already finished.
    finished: Arc<AtomicU32>,
    pub roots: Vec<String>,
}

impl Indexer {
    /// Starts indexing `roots` into a staged library next to `library`.
    /// `elevate` allows one UAC prompt to read NTFS drives quickly. With
    /// places in `only`, just the roots holding them are read; the others
    /// are carried over from the library in use. Empty reads everything.
    pub fn start(
        library: &Path,
        roots: &[String],
        only: &[String],
        mut options: ScanOptions,
        elevate: bool,
    ) -> Self {
        let roots = outermost_roots(roots);
        let chosen: Vec<String> = only
            .iter()
            .filter_map(|only| {
                let key = normalize_path(only);
                roots
                    .iter()
                    .find(|root| {
                        let outer = normalize_path(root);
                        outer == key || path_within(&key, &outer)
                    })
                    .cloned()
            })
            .collect();
        let reuse = (!chosen.is_empty() && chosen.len() < roots.len())
            .then(|| (active_library(library), chosen));
        let progress = Arc::new(AtomicU32::new(0));
        let finished = Arc::new(AtomicU32::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        options.progress = Some(Arc::clone(&progress));
        options.cancel = Some(Arc::clone(&cancel));
        let (sender, events) = mpsc::channel();
        let staged = staging_dir(library);
        let thread_roots = roots.clone();
        let thread_finished = Arc::clone(&finished);
        let thread_progress = Arc::clone(&progress);
        let thread = std::thread::Builder::new()
            .name("feather-indexer".into())
            .spawn(move || {
                let event = build(
                    BuildTarget { staged, reuse },
                    &thread_roots,
                    &options,
                    &thread_progress,
                    &thread_finished,
                    elevate,
                    |root| {
                        let _ = sender.send(IndexEvent::Root {
                            root: root.to_string(),
                        });
                    },
                );
                let _ = sender.send(event);
            })
            .ok();
        Self {
            progress,
            cancel,
            events,
            thread,
            finished,
            roots,
        }
    }

    /// Entries found so far across every root.
    pub fn found(&self) -> u64 {
        u64::from(self.finished.load(Ordering::Relaxed))
            + u64::from(self.progress.load(Ordering::Relaxed))
    }

    pub fn try_event(&self) -> Option<IndexEvent> {
        match self.events.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => None,
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for Indexer {
    fn drop(&mut self) {
        self.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Where a build writes and what it can reuse: a staged library folder, and
/// optionally the active library plus the one root being read again.
struct BuildTarget {
    staged: PathBuf,
    reuse: Option<(PathBuf, Vec<String>)>,
}

fn build(
    target: BuildTarget,
    roots: &[String],
    options: &ScanOptions,
    progress: &AtomicU32,
    finished: &AtomicU32,
    elevate: bool,
    mut on_root: impl FnMut(&str),
) -> IndexEvent {
    let BuildTarget { staged, reuse } = target;
    let staged = staged.as_path();
    if staged.exists() {
        if let Err(error) = std::fs::remove_dir_all(staged) {
            return IndexEvent::Failed(format!("Could not clear {}: {error}", staged.display()));
        }
    }
    if let Err(error) = std::fs::create_dir_all(staged) {
        return IndexEvent::Failed(format!("Could not create {}: {error}", staged.display()));
    }

    // Reading one place again: every other root comes from the library in
    // use. A root that can't be carried over is read like the chosen one.
    let roots: Vec<String> = roots
        .iter()
        .filter(|root| {
            let Some((active, chosen)) = &reuse else {
                return true;
            };
            if chosen.contains(root) {
                return true;
            }
            let name = library_dir_name(&root_path(root));
            let (from, to) = (active.join(&name), staged.join(&name));
            if from.is_dir() && carry_over(&from, &to).is_ok() {
                return false;
            }
            let _ = std::fs::remove_dir_all(&to);
            true
        })
        .cloned()
        .collect();
    let roots = roots.as_slice();

    let mut failures = Vec::new();
    // NTFS drives are read from their master file table, which takes
    // seconds instead of minutes but needs admin rights. With them, the
    // table is read here; without, one elevated helper reads every such
    // root after a single UAC prompt. Everything else, or a declined
    // prompt, falls back to the folder walk.
    //
    // Each root is announced once. A failed MFT read falls back to the walk
    // below, which must not announce the same root a second time.
    let mut walk_roots: Vec<(String, bool)> = Vec::new();
    // The admin helper must not read Feather's own folders into the index.
    let mut helper_options = options.clone();
    if let Some(parent) = staged.parent() {
        helper_options
            .excluded_paths
            .push(parent.to_string_lossy().to_string());
    }
    for root in roots {
        let path = root_path(root);
        if !is_ntfs(&path) {
            walk_roots.push((root.clone(), false));
            continue;
        }
        on_root(root);
        progress.store(0, Ordering::Relaxed);
        let target = staged.join(library_dir_name(&path));
        let mut result = mft::scan(&path, &target, options);
        if elevate
            && helper::available()
            && matches!(&result, Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied)
        {
            // Through the session's admin helper: at most one prompt.
            let started = std::time::Instant::now();
            result = helper::read(&path, &helper_options)
                .and_then(|tree| write_tree(&target, tree, started));
        }
        match result {
            Ok(report) => {
                finished.fetch_add(report.entries, Ordering::Relaxed);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                let _ = std::fs::remove_dir_all(staged);
                return IndexEvent::Cancelled;
            }
            Err(_) => walk_roots.push((root.clone(), true)),
        }
        if cancelled(options) {
            let _ = std::fs::remove_dir_all(staged);
            return IndexEvent::Cancelled;
        }
    }

    for (root, announced) in &walk_roots {
        if !announced {
            on_root(root);
        }
        progress.store(0, Ordering::Relaxed);
        let path = root_path(root);
        let target = staged.join(library_dir_name(&path));
        match scan(&path, &target, options) {
            Ok(report) => {
                finished.fetch_add(report.entries, Ordering::Relaxed);
                progress.store(0, Ordering::Relaxed);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                let _ = std::fs::remove_dir_all(staged);
                return IndexEvent::Cancelled;
            }
            Err(error) => failures.push(format!("{root}: {error}")),
        }
    }
    if failures.len() == roots.len() {
        let _ = std::fs::remove_dir_all(staged);
        return IndexEvent::Failed(failures.join("; "));
    }
    if let Err(error) = std::fs::write(staged.join(COMPLETE_MARKER), b"ok") {
        return IndexEvent::Failed(format!("Could not finish the index: {error}"));
    }
    IndexEvent::Done {
        staged: staged.to_path_buf(),
        failures,
    }
}

fn cancelled(options: &ScanOptions) -> bool {
    options
        .cancel
        .as_ref()
        .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
}

/// Free and total bytes on a drive.
pub fn drive_space(root: &str) -> Option<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = format!("{}\\", root.trim().trim_end_matches('\\'))
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let (mut free, mut total) = (0u64, 0u64);
    let ok =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) };
    (ok != 0).then_some((free, total))
}

/// How Explorer names a drive: its label and letter, like `Local Disk (C:)`.
pub fn drive_label(drive: &str) -> String {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;
    let letter = drive.trim().trim_end_matches('\\');
    let root: Vec<u16> = format!("{letter}\\")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut name = [0u16; 261];
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            name.as_mut_ptr(),
            name.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    let end = name
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(name.len());
    let label = String::from_utf16_lossy(&name[..end]);
    let label = if ok == 0 {
        "Not connected"
    } else if label.trim().is_empty() {
        "Local Disk"
    } else {
        label.trim()
    };
    format!("{label} ({letter})")
}

/// True for a local drive formatted with NTFS, the only file system with a
/// master file table to read.
fn is_ntfs(path: &Path) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;
    let text = path.to_string_lossy();
    let bytes = text.as_bytes();
    if bytes.len() < 2 || bytes[1] != b':' {
        return false;
    }
    let volume: Vec<u16> = format!("{}\\", &text[..2])
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut name = [0u16; 32];
    let ok = unsafe {
        GetVolumeInformationW(
            volume.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    let end = name
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(name.len());
    ok != 0 && String::from_utf16_lossy(&name[..end]).eq_ignore_ascii_case("NTFS")
}

/// Quotes one command-line argument the way Windows programs parse them:
/// backslashes before the closing quote are doubled.
fn quote(argument: &str) -> String {
    let trailing = argument.len() - argument.trim_end_matches('\\').len();
    format!("\"{argument}{}\"", "\\".repeat(trailing))
}

/// Runs `feather index scan` for `roots` with admin rights, writing into the
/// staged library, and waits for it. Fails if the UAC prompt is declined.
pub fn run_elevated(staged: &Path, roots: &[String], options: &ScanOptions) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let exe = std::env::current_exe()?;
    let mut parameters = format!("index scan --index {}", quote(&staged.to_string_lossy()));
    for root in roots {
        parameters.push_str(" --root ");
        parameters.push_str(&quote(&root_path(root).to_string_lossy()));
    }
    for name in &options.excluded_names {
        parameters.push_str(" --exclude-name ");
        parameters.push_str(&quote(name));
    }
    for path in &options.excluded_paths {
        parameters.push_str(" --exclude-path ");
        parameters.push_str(&quote(path));
    }
    let wide = |text: &str| -> Vec<u16> { text.encode_utf16().chain(std::iter::once(0)).collect() };
    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let parameters = wide(&parameters);
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.nShow = SW_HIDE;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if info.hProcess.is_null() {
        return Err(std::io::Error::other("the indexing helper did not start"));
    }
    let mut code = 1u32;
    unsafe {
        WaitForSingleObject(info.hProcess, u32::MAX);
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
    }
    if code == 0 {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "the indexing helper failed with code {code}"
        )))
    }
}

/// The user's profile folder, the default thing to index.
pub fn profile_folder() -> Option<String> {
    std::env::var("USERPROFILE")
        .ok()
        .filter(|path| !path.trim().is_empty())
}

/// Fixed local drives, like `C:\`.
pub fn fixed_drives() -> Vec<String> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    const DRIVE_FIXED: u32 = 3;
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| format!("{}:\\", (b'A' + bit) as char))
        .filter(|root| {
            let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
            unsafe { GetDriveTypeW(wide.as_ptr()) == DRIVE_FIXED }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "feather-indexer-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    #[test]
    fn arguments_keep_trailing_backslashes() {
        assert_eq!(quote(r"C:\"), r#""C:\\""#);
        assert_eq!(quote(r"C:\My Files"), r#""C:\My Files""#);
    }

    #[test]
    fn the_system_drive_is_ntfs() {
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        assert!(is_ntfs(&root_path(&system)));
        assert!(!is_ntfs(Path::new(r"\\server\share")));
    }

    #[test]
    fn drive_roots_get_their_backslash_back() {
        assert_eq!(root_path("C:"), PathBuf::from("C:\\"));
        assert_eq!(root_path(r"D:\Photos"), PathBuf::from(r"D:\Photos"));
    }

    #[test]
    fn nested_and_duplicate_roots_are_indexed_once() {
        let roots = outermost_roots(&[
            r"C:\Users\Me\Documents".into(),
            r"C:\Users\Me".into(),
            r"c:\users\me\".into(),
            r"D:\Photos".into(),
            "  ".into(),
        ]);
        assert_eq!(
            roots,
            vec![r"D:\Photos".to_string(), r"C:\Users\Me".to_string()]
        );
    }

    #[test]
    fn builds_stages_and_installs_a_library() {
        let base = temp_dir("library");
        let tree = base.join("tree");
        std::fs::create_dir_all(tree.join("docs")).expect("tree");
        std::fs::write(tree.join("docs").join("report.txt"), b"x").expect("file");
        let library = base.join("index");
        std::fs::create_dir_all(library.join("old-volume")).expect("old library");

        let roots = vec![tree.to_string_lossy().to_string()];
        let indexer = Indexer::start(&library, &roots, &[], ScanOptions::default(), false);
        let staged = finish(&indexer).0;
        // The old library stays open while the new one is activated.
        let old = feather_core::IndexSet::open(&library);
        activate(&library, &staged).expect("activate");
        assert_eq!(active_library(&library), staged);
        let set = feather_core::IndexSet::open(&active_library(&library)).expect("open");
        assert_eq!(set.item_count(), 2);
        drop(old);
        cleanup(&library);
        assert!(!library.exists());
        assert!(staged.exists());
        drop(set);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Waits for a build: its folder, and the roots it read.
    fn finish(indexer: &Indexer) -> (PathBuf, Vec<String>) {
        let mut read = Vec::new();
        loop {
            match indexer.events.recv().expect("event") {
                IndexEvent::Done { staged, .. } => return (staged, read),
                IndexEvent::Root { root } => read.push(root),
                IndexEvent::Failed(error) => panic!("{error}"),
                IndexEvent::Cancelled => panic!("cancelled"),
            }
        }
    }

    #[test]
    fn reading_one_place_again_keeps_the_others() {
        let base = temp_dir("one-place");
        let (photos, music) = (base.join("photos"), base.join("music"));
        std::fs::create_dir_all(&photos).expect("photos");
        std::fs::create_dir_all(&music).expect("music");
        std::fs::write(photos.join("cat.jpg"), b"x").expect("file");
        std::fs::write(music.join("song.mp3"), b"x").expect("file");
        let library = base.join("index");
        let roots = vec![
            photos.to_string_lossy().to_string(),
            music.to_string_lossy().to_string(),
        ];
        let first = Indexer::start(&library, &roots, &[], ScanOptions::default(), false);
        let (staged, read) = finish(&first);
        assert_eq!(read.len(), 2);
        activate(&library, &staged).expect("activate");
        drop(first);

        // Both folders gain a file, but only one is read again.
        std::fs::write(photos.join("dog.jpg"), b"x").expect("file");
        std::fs::write(music.join("tune.mp3"), b"x").expect("file");
        let again = Indexer::start(&library, &roots, &roots[..1], ScanOptions::default(), false);
        let (staged, read) = finish(&again);
        assert_eq!(read, vec![roots[0].clone()]);
        let set = feather_core::IndexSet::open(&staged).expect("open");
        // cat, dog, and song; the volume roots are synthetic and not counted.
        assert_eq!(set.item_count(), 3);
        drop(set);
        drop(again);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn unfinished_builds_are_discarded_at_startup() {
        let base = temp_dir("pending");
        let library = base.join("index");
        let staged = staging_dir(&library);
        std::fs::create_dir_all(staged.join("vol-c")).expect("staged");
        apply_pending(&library);
        assert!(!staged.exists());
        assert_eq!(active_library(&library), library);
        let _ = std::fs::remove_dir_all(&base);
    }
}
