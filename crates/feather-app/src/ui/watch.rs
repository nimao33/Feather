//! Notices when files appear, disappear, or are renamed under the searched
//! places, so the index can refresh itself.
//!
//! Each root gets a thread running `ReadDirectoryChangesW` over the whole
//! tree (no admin rights needed). Only name changes count: a log file
//! growing does not make the index stale. Changes under ignored folders,
//! such as Feather's own index, are dropped.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use feather_core::{normalize_path, path_within};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
    FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, SetEvent, WaitForMultipleObjects, INFINITE,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

/// Counts relevant changes and remembers when the last one happened, and
/// which roots they were in.
#[derive(Default)]
pub struct Changes {
    count: AtomicU64,
    /// Milliseconds since the Unix epoch.
    last: AtomicU64,
    /// One flag per watched root, set by a change there.
    dirty: std::sync::Mutex<Vec<bool>>,
}

impl Changes {
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// How long ago the last change happened.
    pub fn quiet_for(&self) -> std::time::Duration {
        let last = self.last.load(Ordering::Relaxed);
        std::time::Duration::from_millis(now_ms().saturating_sub(last))
    }

    fn note(&self, root: usize) {
        if let Some(flag) = self.dirty.lock().unwrap().get_mut(root) {
            *flag = true;
        }
        self.last.store(now_ms(), Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// The roots that changed since the last call, by their place in the
    /// list the watcher was started with, and clears them.
    pub fn take_dirty(&self) -> Vec<usize> {
        let mut dirty = self.dirty.lock().unwrap();
        let changed = dirty
            .iter()
            .enumerate()
            .filter(|(_, flag)| **flag)
            .map(|(index, _)| index)
            .collect();
        dirty.iter_mut().for_each(|flag| *flag = false);
        changed
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

struct Stop(HANDLE);
unsafe impl Send for Stop {}
unsafe impl Sync for Stop {}

pub struct Watcher {
    pub changes: Arc<Changes>,
    stop: Arc<Stop>,
    stopping: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Watcher {
    /// Watches every root; changes inside `ignore` (or below a folder named
    /// in `ignore_names`) are not counted.
    pub fn start(roots: &[PathBuf], ignore: Vec<String>, ignore_names: Vec<String>) -> Self {
        let changes = Arc::new(Changes {
            dirty: std::sync::Mutex::new(vec![false; roots.len()]),
            ..Changes::default()
        });
        let stop = Arc::new(Stop(unsafe {
            CreateEventW(std::ptr::null(), 1, 0, std::ptr::null())
        }));
        let stopping = Arc::new(AtomicBool::new(false));
        let ignore: Arc<Vec<String>> =
            Arc::new(ignore.iter().map(|path| normalize_path(path)).collect());
        let ignore_names: Arc<Vec<String>> = Arc::new(
            ignore_names
                .iter()
                .map(|name| name.to_lowercase())
                .collect(),
        );
        let threads = roots
            .iter()
            .enumerate()
            .filter_map(|(index, root)| {
                let root = root.clone();
                let changes = Arc::clone(&changes);
                let stop = Arc::clone(&stop);
                let stopping = Arc::clone(&stopping);
                let ignore = Arc::clone(&ignore);
                let ignore_names = Arc::clone(&ignore_names);
                std::thread::Builder::new()
                    .name("feather-watch".into())
                    .spawn(move || {
                        watch(
                            &root,
                            index,
                            &changes,
                            &stop,
                            &stopping,
                            &ignore,
                            &ignore_names,
                        )
                    })
                    .ok()
            })
            .collect();
        Self {
            changes,
            stop,
            stopping,
            threads,
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        unsafe {
            SetEvent(self.stop.0);
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        unsafe {
            CloseHandle(self.stop.0);
        }
    }
}

fn watch(
    root: &Path,
    index: usize,
    changes: &Changes,
    stop: &Stop,
    stopping: &AtomicBool,
    ignore: &[String],
    ignore_names: &[String],
) {
    let wide: Vec<u16> = root
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return;
    }
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    // DWORD-aligned, as ReadDirectoryChangesW requires.
    let mut buffer = vec![0u32; 16 * 1024];
    let root_text = root.to_string_lossy().trim_end_matches('\\').to_string();
    while !stopping.load(Ordering::Relaxed) {
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event;
        let ok = unsafe {
            ReadDirectoryChangesW(
                handle,
                buffer.as_mut_ptr() as *mut _,
                (buffer.len() * 4) as u32,
                1,
                FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME,
                std::ptr::null_mut(),
                &mut overlapped,
                None,
            )
        };
        if ok == 0 {
            break;
        }
        let handles = [event, stop.0];
        let which = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
        if which != WAIT_OBJECT_0 {
            unsafe {
                CancelIoEx(handle, &overlapped);
                let mut bytes = 0;
                GetOverlappedResult(handle, &overlapped, &mut bytes, 1);
            }
            break;
        }
        let mut bytes = 0u32;
        if unsafe { GetOverlappedResult(handle, &overlapped, &mut bytes, 0) } == 0 {
            break;
        }
        if bytes == 0 {
            // Too many changes to list: count it as a change.
            changes.note(index);
            continue;
        }
        let data =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr() as *const u8, bytes as usize) };
        if relevant(data, &root_text, ignore, ignore_names) {
            changes.note(index);
        }
    }
    unsafe {
        CloseHandle(event);
        CloseHandle(handle);
    }
}

/// True when any change in a `FILE_NOTIFY_INFORMATION` list is outside the
/// ignored places.
fn relevant(data: &[u8], root: &str, ignore: &[String], ignore_names: &[String]) -> bool {
    let mut offset = 0usize;
    loop {
        if offset + 12 > data.len() {
            return false;
        }
        let read_u32 = |at: usize| {
            u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize
        };
        let next = read_u32(offset);
        let name_len = read_u32(offset + 8);
        let start = offset + 12;
        let end = (start + name_len).min(data.len());
        let units: Vec<u16> = data[start..end]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let relative = String::from_utf16_lossy(&units);
        let path = format!("{root}\\{relative}");
        let named = relative.split('\\').any(|part| {
            ignore_names
                .iter()
                .any(|name| part.eq_ignore_ascii_case(name))
        });
        if !named && !ignore.iter().any(|place| path_within(&path, place)) {
            return true;
        }
        if next == 0 {
            return false;
        }
        offset += next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, last: bool) -> Vec<u8> {
        let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut size = 12 + units.len();
        size = size.div_ceil(4) * 4;
        let mut out = Vec::new();
        out.extend_from_slice(&(if last { 0 } else { size as u32 }).to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(units.len() as u32).to_le_bytes());
        out.extend_from_slice(&units);
        out.resize(size, 0);
        out
    }

    #[test]
    fn changes_in_ignored_places_do_not_count() {
        let ignore = vec![normalize_path(r"C:\Users\Me\AppData\Local\Feather")];
        let names = vec!["node_modules".to_string()];
        let mut data = record(r"Users\Me\AppData\Local\Feather\index\x.meta", false);
        data.extend(record(r"Code\app\node_modules\left-pad\index.js", true));
        assert!(!relevant(&data, "C:", &ignore, &names));

        let mut data = record(r"Users\Me\AppData\Local\Feather\index\x.meta", false);
        data.extend(record(r"Users\Me\Desktop\new.txt", true));
        assert!(relevant(&data, "C:", &ignore, &names));
    }

    #[test]
    fn a_new_file_is_noticed() {
        let base = std::env::temp_dir().join(format!("feather-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let watcher = Watcher::start(std::slice::from_ref(&base), Vec::new(), Vec::new());
        std::thread::sleep(std::time::Duration::from_millis(200));
        std::fs::write(base.join("hello.txt"), b"x").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while watcher.changes.count() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(watcher.changes.count() > 0);
        drop(watcher);
        let _ = std::fs::remove_dir_all(&base);
    }
}
