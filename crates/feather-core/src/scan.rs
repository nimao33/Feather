//! The directory-walk index builder, and the index writer shared with the
//! NTFS reader in `mft.rs`.
//!
//! The walk works on any file system without admin rights. It reads folders
//! on several threads, assigns stable local file references, sorts by
//! lowercase name, and writes the same index format the mmap reader
//! consumes.

use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::format::{write_manifest, Manifest, CHECK_STRIDE, FRN_SIZE, META_SIZE};
use crate::path::{is_absolute_path, normalize_path, path_within};

const MAX_DEPTH: usize = 512;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Follow junctions and symbolic links. Off by default because a junction
    /// loop can make a walk endless.
    pub follow_links: bool,
    /// Stop after this many entries. Zero means the default cap.
    pub max_entries: u32,
    /// Absolute folder subtrees to leave out of the walk.
    pub excluded_paths: Vec<String>,
    /// Folder or file names to leave out anywhere in the tree.
    pub excluded_names: Vec<String>,
    /// Updated with the number of entries found so far, for progress.
    pub progress: Option<Arc<AtomicU32>>,
    /// Set from another thread to stop the walk; the scan then fails with
    /// `ErrorKind::Interrupted` and leaves `out` untouched.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            follow_links: false,
            max_entries: 5_000_000,
            excluded_paths: default_excluded_paths(),
            excluded_names: Vec::new(),
            progress: None,
            cancel: None,
        }
    }
}

/// Virtual and kernel-owned trees that would make a scan of `/` endless.
#[cfg(not(windows))]
fn default_excluded_paths() -> Vec<String> {
    ["/proc", "/sys", "/dev", "/run"]
        .iter()
        .map(|path| path.to_string())
        .collect()
}

#[cfg(windows)]
fn default_excluded_paths() -> Vec<String> {
    Vec::new()
}

fn stable_hash(text: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The subdirectory of a library that holds the index for `root`: `vol-c`
/// for a drive, `vol-root` for `/`, or a stable hash of the path for a folder.
pub fn library_dir_name(root: &Path) -> String {
    let text = root
        .to_string_lossy()
        .trim_end_matches(crate::path::SEP)
        .to_lowercase();
    if text.is_empty() {
        "vol-root".to_string()
    } else if cfg!(windows) && text.len() == 2 && text.ends_with(':') {
        format!("vol-{}", &text[..1])
    } else {
        format!("vol-{:016x}", stable_hash(&text))
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScanReport {
    pub entries: u32,
    pub dirs: u32,
    pub files: u32,
    pub skipped: u64,
    pub errors: u64,
    pub bytes: u64,
    pub truncated: bool,
    pub root: String,
    pub elapsed: Duration,
}

/// One file or folder before it is written. `parent` indexes the same list;
/// entry 0 is the root.
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    pub name: String,
    pub parent: u32,
    pub size: u64,
    pub mtime: u64,
    pub flags: u16,
    pub is_dir: bool,
}

/// Scanned files and folders, column by column. The names sit end to end
/// in one buffer instead of one allocation each, which halves what a big
/// drive takes while its index is built. Entry 0 is the root; `parent`
/// indexes the same list.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entries {
    names: Vec<u8>,
    /// Where each name ends in `names`.
    ends: Vec<u32>,
    pub parent: Vec<u32>,
    pub size: Vec<u64>,
    pub mtime: Vec<u64>,
    /// Bit 0: folder. Bit 1: hidden. Bit 2: system.
    pub flags: Vec<u16>,
}

impl Entries {
    pub fn with_capacity(count: usize) -> Self {
        Entries {
            names: Vec::new(),
            ends: Vec::with_capacity(count),
            parent: Vec::with_capacity(count),
            size: Vec::with_capacity(count),
            mtime: Vec::with_capacity(count),
            flags: Vec::with_capacity(count),
        }
    }

    pub fn push(&mut self, name: &str, parent: u32, size: u64, mtime: u64, flags: u16) {
        self.names.extend_from_slice(name.as_bytes());
        self.ends
            .push(u32::try_from(self.names.len()).unwrap_or(u32::MAX));
        self.parent.push(parent);
        self.size.push(size);
        self.mtime.push(mtime);
        self.flags.push(flags);
    }

    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    pub fn name(&self, index: usize) -> &str {
        let start = if index == 0 {
            0
        } else {
            self.ends[index - 1] as usize
        };
        let end = self.ends[index] as usize;
        std::str::from_utf8(self.names.get(start..end).unwrap_or(&[])).unwrap_or("")
    }

    pub fn is_dir(&self, index: usize) -> bool {
        self.flags[index] & 1 != 0
    }

    pub fn get(&self, index: usize) -> Pending {
        Pending {
            name: self.name(index).to_string(),
            parent: self.parent[index],
            size: self.size[index],
            mtime: self.mtime[index],
            flags: self.flags[index],
            is_dir: self.is_dir(index),
        }
    }

    pub fn shrink_to_fit(&mut self) {
        self.names.shrink_to_fit();
        self.ends.shrink_to_fit();
        self.parent.shrink_to_fit();
        self.size.shrink_to_fit();
        self.mtime.shrink_to_fit();
        self.flags.shrink_to_fit();
    }
}

impl From<Vec<Pending>> for Entries {
    fn from(list: Vec<Pending>) -> Self {
        let mut entries = Entries::with_capacity(list.len());
        for entry in list {
            let flags = (entry.flags & !1) | u16::from(entry.is_dir);
            entries.push(&entry.name, entry.parent, entry.size, entry.mtime, flags);
        }
        entries
    }
}

/// A scanned folder tree held in memory, ready to be written as an index.
#[derive(Clone, Debug, Default)]
pub struct Tree {
    pub root_display: String,
    pub entries: Entries,
    pub report: ScanReport,
}

const TREE_MAGIC: &[u8; 4] = b"FTR1";

impl Tree {
    /// Writes the tree as a compact byte stream (for the elevated helper's
    /// pipe).
    pub fn encode(&self, out: &mut impl Write) -> io::Result<()> {
        out.write_all(TREE_MAGIC)?;
        let root = self.root_display.as_bytes();
        out.write_all(&(root.len() as u32).to_le_bytes())?;
        out.write_all(root)?;
        out.write_all(&self.report.skipped.to_le_bytes())?;
        out.write_all(&[u8::from(self.report.truncated)])?;
        out.write_all(&(self.entries.len() as u32).to_le_bytes())?;
        let entries = &self.entries;
        for index in 0..entries.len() {
            let name = entries.name(index).as_bytes();
            let len = name.len().min(u16::MAX as usize);
            out.write_all(&entries.parent[index].to_le_bytes())?;
            out.write_all(&entries.size[index].to_le_bytes())?;
            out.write_all(&entries.mtime[index].to_le_bytes())?;
            out.write_all(&entries.flags[index].to_le_bytes())?;
            out.write_all(&(len as u16).to_le_bytes())?;
            out.write_all(&name[..len])?;
        }
        Ok(())
    }

    /// Reads a stream written by [`Tree::encode`], checking every parent
    /// reference so a bad stream cannot produce a broken index.
    pub fn decode(input: &mut impl Read) -> io::Result<Tree> {
        let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
        let mut magic = [0u8; 4];
        input.read_exact(&mut magic)?;
        if &magic != TREE_MAGIC {
            return Err(bad("not a Feather tree stream"));
        }
        let mut u32_buf = [0u8; 4];
        let mut u64_buf = [0u8; 8];
        let mut u16_buf = [0u8; 2];
        input.read_exact(&mut u32_buf)?;
        let root_len = u32::from_le_bytes(u32_buf) as usize;
        if root_len > 32 * 1024 {
            return Err(bad("root path is too long"));
        }
        let mut root = vec![0u8; root_len];
        input.read_exact(&mut root)?;
        let root_display = String::from_utf8(root).map_err(|_| bad("root path is not UTF-8"))?;
        input.read_exact(&mut u64_buf)?;
        let skipped = u64::from_le_bytes(u64_buf);
        let mut flag = [0u8; 1];
        input.read_exact(&mut flag)?;
        input.read_exact(&mut u32_buf)?;
        let count = u32::from_le_bytes(u32_buf) as usize;
        let mut entries = Entries::with_capacity(count.min(64 << 20));
        let mut name = Vec::new();
        for index in 0..count {
            input.read_exact(&mut u32_buf)?;
            let parent = u32::from_le_bytes(u32_buf);
            input.read_exact(&mut u64_buf)?;
            let size = u64::from_le_bytes(u64_buf);
            input.read_exact(&mut u64_buf)?;
            let mtime = u64::from_le_bytes(u64_buf);
            input.read_exact(&mut u16_buf)?;
            let flags = u16::from_le_bytes(u16_buf);
            input.read_exact(&mut u16_buf)?;
            name.resize(u16::from_le_bytes(u16_buf) as usize, 0);
            input.read_exact(&mut name)?;
            if index > 0 && parent as usize >= index {
                return Err(bad("an entry points at a parent that comes after it"));
            }
            entries.push(&String::from_utf8_lossy(&name), parent, size, mtime, flags);
        }
        Ok(Tree {
            root_display,
            report: ScanReport {
                root: String::new(),
                skipped,
                truncated: flag[0] != 0,
                ..ScanReport::default()
            },
            entries,
        })
    }
}

/// Writes an in-memory tree as the index in `out`.
pub fn write_tree(out: &Path, tree: Tree, started: Instant) -> io::Result<ScanReport> {
    let mut report = tree.report;
    report.root = tree.root_display.clone();
    if tree.entries.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the tree is empty",
        ));
    }
    write_entries(out, &tree.root_display, tree.entries, started, report)
}

/// Scans `root` and replaces the index files in `out`.
///
/// Folders are read on several threads at once, and each entry's size, time,
/// and attributes come from the directory listing itself, so there is no
/// extra system call per file. The output directory is excluded from the walk
/// automatically, so storing the index under the scanned root cannot feed
/// back into the next scan.
pub fn scan(root: &Path, out: &Path, options: &ScanOptions) -> io::Result<ScanReport> {
    let started = Instant::now();
    let root = absolute_path(root)?;
    if !root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("scan root is not a directory: {}", root.display()),
        ));
    }
    if !root.to_string_lossy().is_empty() && !is_absolute_path(&root.to_string_lossy()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "scan root must be an absolute path",
        ));
    }

    fs::create_dir_all(out)?;
    let out_abs = absolute_path(out).unwrap_or_else(|_| out.to_path_buf());
    let out_key = normalize_path(&out_abs.to_string_lossy());
    let root_display = display_root(&root);
    let root_meta = fs::metadata(&root)?;
    let mut report = ScanReport {
        root: root_display.clone(),
        ..ScanReport::default()
    };
    let entries = walk(
        &root,
        &out_key,
        options,
        modified_filetime(&root_meta),
        &mut report,
    )?;
    phase("walk", started);
    write_entries(out, &root_display, entries, started, report)
}

struct Job {
    path: PathBuf,
    index: u32,
    depth: usize,
}

struct Child {
    name: String,
    size: u64,
    mtime: u64,
    flags: u16,
    /// Set for folders that should be read next.
    descend: Option<PathBuf>,
}

struct Listing {
    parent: u32,
    depth: usize,
    children: Vec<Child>,
    skipped: u64,
    errors: u64,
}

#[derive(Default)]
struct Queue {
    state: Mutex<(Vec<Job>, bool)>,
    ready: Condvar,
}

impl Queue {
    fn push(&self, job: Job) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.0.push(job);
        self.ready.notify_one();
    }

    /// Waits for a job; `None` once the queue is closed.
    fn pop(&self) -> Option<Job> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        loop {
            if state.1 {
                return None;
            }
            if let Some(job) = state.0.pop() {
                return Some(job);
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.1 = true;
        self.ready.notify_all();
    }
}

/// Reads folders on worker threads. The calling thread owns the entry list:
/// it numbers each folder's children, then queues their subfolders.
fn walk(
    root: &Path,
    out_key: &str,
    options: &ScanOptions,
    root_mtime: u64,
    report: &mut ScanReport,
) -> io::Result<Entries> {
    let max_entries = if options.max_entries == 0 {
        ScanOptions::default().max_entries
    } else {
        options.max_entries
    } as usize;
    let mut entries = Entries::default();
    entries.push("", 0, 0, root_mtime, 1);
    let visited = Mutex::new(std::collections::HashSet::new());
    if options.follow_links {
        if let Ok(canonical) = fs::canonicalize(root) {
            if let Ok(mut visited) = visited.lock() {
                visited.insert(normalize_path(&canonical.to_string_lossy()));
            }
        }
    }
    let queue = Queue::default();
    queue.push(Job {
        path: root.to_path_buf(),
        index: 0,
        depth: 0,
    });
    let threads = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4)
        .clamp(2, 12);
    let (sender, receiver) = mpsc::channel::<Listing>();
    let mut cancelled = false;

    std::thread::scope(|scope| {
        for _ in 0..threads {
            let sender = sender.clone();
            let queue = &queue;
            let visited = &visited;
            scope.spawn(move || {
                while let Some(job) = queue.pop() {
                    let listing = list_dir(&job, out_key, options, visited);
                    if sender.send(listing).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);

        let mut outstanding = 1usize;
        while outstanding > 0 {
            let Ok(listing) = receiver.recv() else {
                break;
            };
            outstanding -= 1;
            report.skipped += listing.skipped;
            report.errors += listing.errors;
            for child in listing.children {
                if entries.len() >= max_entries {
                    report.truncated = true;
                    break;
                }
                let index = entries.len() as u32;
                entries.push(
                    &child.name,
                    listing.parent,
                    child.size,
                    child.mtime,
                    child.flags,
                );
                if let Some(path) = child.descend {
                    if listing.depth + 1 >= MAX_DEPTH {
                        report.skipped += 1;
                    } else {
                        queue.push(Job {
                            path,
                            index,
                            depth: listing.depth + 1,
                        });
                        outstanding += 1;
                    }
                }
            }
            if let Some(progress) = &options.progress {
                progress.store(entries.len() as u32, Ordering::Relaxed);
            }
            if options
                .cancel
                .as_ref()
                .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
            {
                cancelled = true;
                break;
            }
            if report.truncated {
                break;
            }
        }
        queue.close();
    });

    if cancelled {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "scan cancelled"));
    }
    Ok(entries)
}

fn list_dir(
    job: &Job,
    out_key: &str,
    options: &ScanOptions,
    visited: &Mutex<std::collections::HashSet<String>>,
) -> Listing {
    let mut listing = Listing {
        parent: job.index,
        depth: job.depth,
        children: Vec::new(),
        skipped: 0,
        errors: 0,
    };
    let reader = match fs::read_dir(&job.path) {
        Ok(reader) => reader,
        Err(_) => {
            listing.errors += 1;
            return listing;
        }
    };
    for child in reader {
        let child = match child {
            Ok(child) => child,
            Err(_) => {
                listing.errors += 1;
                continue;
            }
        };
        let name = child.file_name().to_string_lossy().into_owned();
        if should_skip_name(&name)
            || options
                .excluded_names
                .iter()
                .any(|skip| skip.eq_ignore_ascii_case(&name))
        {
            listing.skipped += 1;
            continue;
        }
        // On Windows this comes from the directory listing: no extra call.
        let metadata = match child.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                listing.errors += 1;
                continue;
            }
        };
        let attributes = file_attributes(&metadata);
        #[cfg(not(windows))]
        let attributes = if name.starts_with('.') {
            attributes | FILE_ATTRIBUTE_HIDDEN
        } else {
            attributes
        };
        let is_dir = attributes & FILE_ATTRIBUTE_DIRECTORY != 0 || metadata.is_dir();
        #[cfg(windows)]
        let is_link = attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        #[cfg(not(windows))]
        let is_link = child
            .file_type()
            .map(|kind| kind.is_symlink())
            .unwrap_or(false);
        // Folder links (junctions) would index things twice; file links and
        // cloud placeholders are ordinary results.
        if is_link && is_dir && !options.follow_links {
            listing.skipped += 1;
            continue;
        }

        let mut descend = None;
        if is_dir {
            let path = child.path();
            let path_key = normalize_path(&path.to_string_lossy());
            if (!out_key.is_empty() && (path_key == out_key || path_within(&path_key, out_key)))
                || options
                    .excluded_paths
                    .iter()
                    .any(|excluded| path_within(&path_key, excluded))
            {
                listing.skipped += 1;
                continue;
            }
            if is_link {
                let Ok(canonical) = fs::canonicalize(&path) else {
                    listing.errors += 1;
                    continue;
                };
                let key = normalize_path(&canonical.to_string_lossy());
                let first_visit = visited
                    .lock()
                    .map(|mut set| set.insert(key))
                    .unwrap_or(false);
                if !first_visit {
                    listing.skipped += 1;
                    continue;
                }
            }
            descend = Some(path);
        }

        let flags = (if is_dir { 1u16 } else { 0 })
            | (if attributes & FILE_ATTRIBUTE_HIDDEN != 0 {
                2u16
            } else {
                0
            })
            | (if attributes & FILE_ATTRIBUTE_SYSTEM != 0 {
                4u16
            } else {
                0
            });
        listing.children.push(Child {
            name,
            size: if is_dir { 0 } else { metadata.len() },
            mtime: modified_filetime(&metadata),
            flags,
            descend,
        });
    }
    listing
}

/// Prints how long a scan phase took when `FEATHER_SCAN_TIMING` is set.
pub(crate) fn phase(name: &str, since: Instant) {
    if std::env::var_os("FEATHER_SCAN_TIMING").is_some() {
        eprintln!("phase {name}: {} ms", since.elapsed().as_millis());
    }
}

/// Worker threads for the CPU-heavy scan phases.
pub(crate) fn workers() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4)
        .clamp(1, 16)
}

/// Splits `0..len` into about `parts` ranges whose starts are multiples of
/// `align`.
pub(crate) fn split(len: usize, parts: usize, align: usize) -> Vec<std::ops::Range<usize>> {
    let align = align.max(1);
    let per = (len.div_ceil(parts.max(1)).div_ceil(align)).max(1) * align;
    (0..len)
        .step_by(per)
        .map(|start| start..(start + per).min(len))
        .collect()
}

fn joined<T>(handle: std::thread::ScopedJoinHandle<'_, T>) -> io::Result<T> {
    handle
        .join()
        .map_err(|_| io::Error::other("an index worker thread failed"))
}

/// Merges two sorted runs into `out`.
fn merge<T: Copy>(left: &[T], right: &[T], out: &mut [T], less: impl Fn(&T, &T) -> bool) {
    let (mut i, mut j) = (0, 0);
    for slot in out.iter_mut() {
        if j >= right.len() || (i < left.len() && !less(&right[j], &left[i])) {
            *slot = left[i];
            i += 1;
        } else {
            *slot = right[j];
            j += 1;
        }
    }
}

/// Sorts on several threads: each sorts a slice, then sorted runs are
/// merged pairwise.
fn parallel_sort<T: Copy + Send + Sync>(
    items: &mut Vec<T>,
    compare: impl Fn(&T, &T) -> std::cmp::Ordering + Copy + Send + Sync,
) {
    let len = items.len();
    let mut run = len.div_ceil(workers()).max(1 << 14);
    std::thread::scope(|scope| {
        for chunk in items.chunks_mut(run) {
            scope.spawn(move || chunk.sort_unstable_by(compare));
        }
    });
    if run >= len {
        return;
    }
    let mut buffer = items.clone();
    let less = |a: &T, b: &T| compare(a, b) == std::cmp::Ordering::Less;
    while run < len {
        std::thread::scope(|scope| {
            let mut source: &[T] = items;
            let mut target: &mut [T] = &mut buffer;
            while !source.is_empty() {
                let take = (run * 2).min(source.len());
                let (pair, rest) = source.split_at(take);
                let (out, rest_out) = std::mem::take(&mut target).split_at_mut(take);
                let (left, right) = pair.split_at(run.min(take));
                scope.spawn(move || merge(left, right, out, less));
                source = rest;
                target = rest_out;
            }
        });
        std::mem::swap(items, &mut buffer);
        run *= 2;
    }
}

/// One slice of the encoded index, with offsets relative to the slice.
#[derive(Default)]
struct Encoded {
    meta: Vec<u8>,
    search: Vec<u8>,
    display: Vec<u8>,
    frn: Vec<u8>,
    check: Vec<u8>,
    dirs: u32,
    files: u32,
}

/// Sorts entries by their searchable name and writes the index files.
/// Entry 0 must be the root; parents are remapped after the sort so the
/// tree stays valid. Lowercasing, sorting, and encoding run on all cores.
pub(crate) fn write_entries(
    out: &Path,
    root_display: &str,
    mut entries: Entries,
    started: Instant,
    mut report: ScanReport,
) -> io::Result<ScanReport> {
    if entries.len() > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "volume has more entries than the index format supports",
        ));
    }
    entries.shrink_to_fit();
    let count = entries.len();
    let threads = workers();

    // Lowercased names, packed end to end: one allocation for all of them
    // instead of one per name. `key_at[i]..key_at[i + 1]` is entry i's.
    let phase_start = Instant::now();
    let (key_bytes, key_at) = std::thread::scope(|scope| {
        let handles: Vec<_> = split(count, threads, 1)
            .into_iter()
            .map(|range| {
                let entries = &entries;
                scope.spawn(move || {
                    let mut bytes = Vec::new();
                    let mut ends = Vec::with_capacity(range.len());
                    for index in range {
                        bytes.extend_from_slice(entries.name(index).to_lowercase().as_bytes());
                        ends.push(bytes.len());
                    }
                    (bytes, ends)
                })
            })
            .collect();
        let mut key_bytes = Vec::new();
        let mut key_at: Vec<u32> = Vec::with_capacity(count + 1);
        key_at.push(0);
        for handle in handles {
            let (bytes, ends) = joined(handle)?;
            let base = key_bytes.len();
            if base + bytes.len() > u32::MAX as usize {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "names are too long for the index format",
                ));
            }
            key_at.extend(ends.into_iter().map(|end| (base + end) as u32));
            key_bytes.extend_from_slice(&bytes);
        }
        key_bytes.shrink_to_fit();
        Ok::<_, io::Error>((key_bytes, key_at))
    })?;
    let key = |index: usize| -> &str {
        let bytes = &key_bytes[key_at[index] as usize..key_at[index + 1] as usize];
        // Whole lowercased names, so always valid.
        std::str::from_utf8(bytes).unwrap_or("")
    };
    phase("lowercase", phase_start);

    // Sort on the first eight bytes as a number, which settles most
    // comparisons without touching the strings; ties compare in full.
    let phase_start = Instant::now();
    let prefix = |key: &str| {
        let mut bytes = [0u8; 8];
        let take = key.len().min(8);
        bytes[..take].copy_from_slice(&key.as_bytes()[..take]);
        u64::from_be_bytes(bytes)
    };
    let mut keyed: Vec<(u64, u32)> = (1..count as u32)
        .map(|index| (prefix(key(index as usize)), index))
        .collect();
    {
        let entries = &entries;
        parallel_sort(&mut keyed, |&(prefix_a, a), &(prefix_b, b)| {
            prefix_a
                .cmp(&prefix_b)
                .then_with(|| key(a as usize).cmp(key(b as usize)))
                .then_with(|| entries.name(a as usize).cmp(entries.name(b as usize)))
                .then_with(|| a.cmp(&b))
        });
    }
    let mut order: Vec<u32> = Vec::with_capacity(count);
    order.push(0);
    order.extend(keyed.into_iter().map(|(_, index)| index));
    let mut remap = vec![0u32; count];
    for (new_id, &old_id) in order.iter().enumerate() {
        remap[old_id as usize] = new_id as u32;
    }
    phase("sort", phase_start);

    // Encode and write a slice of entries at a time, so the finished index
    // never has to sit in memory whole: the files grow as each slice is
    // done, and each slice's name offsets move by the bytes before it.
    let phase_start = Instant::now();
    fs::create_dir_all(out)?;
    cleanup(out)?;
    let generation = 1u64;
    let open = |kind: &str| -> io::Result<BufWriter<fs::File>> {
        let path = out.join(format!("base-{generation:016}.{kind}"));
        Ok(BufWriter::with_capacity(1 << 20, fs::File::create(path)?))
    };
    let mut meta_file = open("meta")?;
    let mut search_file = open("search")?;
    let mut display_file = open("display")?;
    let mut frn_file = open("frn")?;
    let mut check_file = open("check")?;
    let (mut meta_len, mut search_len, mut display_len) = (0u64, 0u64, 0u64);
    let (mut frn_len, mut check_len) = (0u64, 0u64);
    let slice = (CHECK_STRIDE * 1024).max(CHECK_STRIDE);
    let mut first = 0usize;
    while first < count {
        let last = (first + slice).min(count);
        let parts: Vec<Encoded> = std::thread::scope(|scope| {
            let handles: Vec<_> = split(last - first, threads, CHECK_STRIDE)
                .into_iter()
                .map(|range| {
                    let range = range.start + first..range.end + first;
                    let (order, entries, remap, key) = (&order, &entries, &remap, &key);
                    scope.spawn(move || {
                        let mut part = Encoded {
                            meta: Vec::with_capacity(range.len() * META_SIZE),
                            frn: Vec::with_capacity(range.len() * FRN_SIZE),
                            ..Encoded::default()
                        };
                        for new_id in range {
                            let old_id = order[new_id] as usize;
                            if entries.is_dir(old_id) {
                                part.dirs += 1;
                            } else {
                                part.files += 1;
                            }
                            let search_name = key(old_id);
                            let search_offset = part.search.len() as u32;
                            let display_offset = part.display.len() as u32;
                            if new_id.is_multiple_of(CHECK_STRIDE) {
                                part.check.extend_from_slice(&search_offset.to_le_bytes());
                                part.check.extend_from_slice(&display_offset.to_le_bytes());
                            }
                            part.search.extend_from_slice(search_name.as_bytes());
                            part.search.push(0);
                            let mut display_len = 0u16;
                            for unit in entries.name(old_id).encode_utf16() {
                                part.display.extend_from_slice(&unit.to_le_bytes());
                                display_len += 1;
                            }
                            part.display.extend_from_slice(&0u16.to_le_bytes());
                            let parent = if new_id == 0 {
                                1
                            } else {
                                remap[entries.parent[old_id] as usize] as u64 + 1
                            };
                            let file_reference = new_id as u64 + 1;
                            let meta = &mut part.meta;
                            meta.extend_from_slice(&file_reference.to_le_bytes());
                            meta.extend_from_slice(&parent.to_le_bytes());
                            meta.extend_from_slice(&entries.size[old_id].to_le_bytes());
                            meta.extend_from_slice(&entries.mtime[old_id].to_le_bytes());
                            meta.extend_from_slice(&display_offset.to_le_bytes());
                            meta.extend_from_slice(&search_offset.to_le_bytes());
                            meta.extend_from_slice(&display_len.to_le_bytes());
                            meta.extend_from_slice(&(search_name.len() as u16).to_le_bytes());
                            meta.extend_from_slice(&entries.flags[old_id].to_le_bytes());
                            meta.extend_from_slice(&0u16.to_le_bytes());
                            part.frn.extend_from_slice(&file_reference.to_le_bytes());
                            part.frn.extend_from_slice(&(new_id as u32).to_le_bytes());
                        }
                        part
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(joined)
                .collect::<io::Result<Vec<_>>>()
        })?;
        for mut part in parts {
            if search_len + part.search.len() as u64 > u32::MAX as u64
                || display_len + part.display.len() as u64 > u32::MAX as u64
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "names are too long for the index format",
                ));
            }
            let search_base = search_len as u32;
            let display_base = display_len as u32;
            for record in part.meta.as_chunks_mut::<META_SIZE>().0 {
                let display_at =
                    u32::from_le_bytes([record[32], record[33], record[34], record[35]]);
                let search_at =
                    u32::from_le_bytes([record[36], record[37], record[38], record[39]]);
                record[32..36].copy_from_slice(&(display_at + display_base).to_le_bytes());
                record[36..40].copy_from_slice(&(search_at + search_base).to_le_bytes());
            }
            for pair in part.check.as_chunks_mut::<8>().0 {
                let search_at = u32::from_le_bytes([pair[0], pair[1], pair[2], pair[3]]);
                let display_at = u32::from_le_bytes([pair[4], pair[5], pair[6], pair[7]]);
                pair[0..4].copy_from_slice(&(search_at + search_base).to_le_bytes());
                pair[4..8].copy_from_slice(&(display_at + display_base).to_le_bytes());
            }
            report.dirs += part.dirs;
            report.files += part.files;
            meta_file.write_all(&part.meta)?;
            search_file.write_all(&part.search)?;
            display_file.write_all(&part.display)?;
            frn_file.write_all(&part.frn)?;
            check_file.write_all(&part.check)?;
            meta_len += part.meta.len() as u64;
            search_len += part.search.len() as u64;
            display_len += part.display.len() as u64;
            frn_len += part.frn.len() as u64;
            check_len += part.check.len() as u64;
        }
        first = last;
    }
    drop(key_bytes);
    drop(key_at);
    drop(entries);
    for file in [
        &mut meta_file,
        &mut search_file,
        &mut display_file,
        &mut frn_file,
        &mut check_file,
    ] {
        file.flush()?;
    }
    drop((meta_file, search_file, display_file, frn_file, check_file));
    let entry_count = count as u32;
    report.entries = entry_count;
    phase("encode and write", phase_start);
    let phase_start = Instant::now();

    let manifest = Manifest {
        generation,
        entry_count,
        meta_len,
        search_len,
        display_len,
        frn_len,
        check_len,
        seq: 0,
    };
    if !manifest.validate_lengths() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "built index shape is invalid",
        ));
    }
    fs::write(out.join("prefix.txt"), root_display.as_bytes())?;
    write_manifest(out, &manifest)?;

    report.bytes = meta_len + search_len + display_len + frn_len + check_len;
    phase("write", phase_start);
    let phase_start = Instant::now();

    // Fail early if the committed manifest or offsets are unusable.
    let volume = crate::index::VolumeIndex::open(out)?;
    if volume.entry_count() != entry_count {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "built index did not reopen with the expected entry count",
        ));
    }
    phase("verify", phase_start);
    report.elapsed = started.elapsed();
    Ok(report)
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::path::absolute(path)?
    };
    let text = strip_verbatim(&absolute.to_string_lossy());
    Ok(PathBuf::from(text))
}

/// The absolute form of a path as text, for comparisons.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn absolute_display(path: &Path) -> String {
    absolute_path(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn strip_verbatim(input: &str) -> String {
    match input.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => input.strip_prefix(r"\\?\").unwrap_or(input).to_string(),
    }
}

pub(crate) fn display_root(root: &Path) -> String {
    let mut text = root
        .to_string_lossy()
        .trim_end_matches(crate::path::SEP)
        .to_string();
    if cfg!(windows) && text.len() == 2 && text.ends_with(':') {
        text.push('\\');
    }
    if text.is_empty() {
        text = root.to_string_lossy().to_string();
    }
    text
}

pub(crate) fn should_skip_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "$recycle.bin" | "system volume information" | "$winreagent"
    )
}

#[cfg(windows)]
fn file_attributes(metadata: &fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes()
}

#[cfg(not(windows))]
fn file_attributes(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::FileTypeExt;
    let kind = metadata.file_type();
    let mut attributes = 0;
    if kind.is_dir() {
        attributes |= FILE_ATTRIBUTE_DIRECTORY;
    }
    if kind.is_fifo() || kind.is_socket() || kind.is_block_device() || kind.is_char_device() {
        attributes |= FILE_ATTRIBUTE_SYSTEM;
    }
    attributes
}

#[cfg(windows)]
fn modified_filetime(metadata: &fs::Metadata) -> u64 {
    use std::os::windows::fs::MetadataExt;
    metadata.last_write_time()
}

#[cfg(not(windows))]
fn modified_filetime(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .map(system_time_to_filetime)
        .unwrap_or(0)
}

#[cfg_attr(windows, allow(dead_code))]
fn system_time_to_filetime(time: SystemTime) -> u64 {
    const WINDOWS_EPOCH_OFFSET_SECS: u64 = 11_644_473_600;
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => (duration.as_secs() + WINDOWS_EPOCH_OFFSET_SECS)
            .saturating_mul(10_000_000)
            .saturating_add(u64::from(duration.subsec_nanos() / 100)),
        Err(_) => 0,
    }
}

fn cleanup(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if (name.starts_with("base-") || name == "manifest.bin" || name == "prefix.txt")
            && entry.path().is_file()
        {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_set::{EntryRef, IndexSet};

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "feather-scan-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    #[test]
    fn scans_a_tree_and_reopens_it_by_path() {
        let root = temp_dir("tree");
        let out = temp_dir("index");
        fs::create_dir_all(root.join("Documents").join("Nested")).expect("folders");
        fs::write(root.join("Documents").join("report.pdf"), b"hello").expect("file");
        fs::write(root.join("Documents").join("Nested").join("note.txt"), b"x").expect("file");
        fs::write(root.join("photo.PNG"), b"image").expect("file");

        let report = scan(&root, &out, &ScanOptions::default()).expect("scan");
        assert_eq!(report.entries, 6);
        assert_eq!(report.dirs, 3);
        assert_eq!(report.files, 3);
        assert!(!report.truncated);

        let set = IndexSet::open(&out).expect("open");
        assert_eq!(set.volume_count(), 1);
        assert_eq!(set.item_count(), 5);
        let info = &set.infos()[0];
        assert_eq!(
            normalize_path(&info.prefix),
            normalize_path(&root.to_string_lossy())
        );

        let mut paths = Vec::new();
        for id in 1..set.volumes()[0].entry_count() {
            paths.push(set.resolve_path(EntryRef::new(0, id)).expect("path"));
        }
        assert!(paths
            .iter()
            .any(|path| path.ends_with(r"Documents\report.pdf")));
        assert!(paths
            .iter()
            .any(|path| path.ends_with(r"Documents\Nested\note.txt")));
        assert!(paths.iter().any(|path| path.ends_with("photo.PNG")));

        let pdf = set
            .volumes()
            .iter()
            .flat_map(|volume| volume.iter_ids())
            .find(|&id| set.volumes()[0].entry(id).name_search == "report.pdf")
            .expect("pdf entry");
        assert_eq!(set.volumes()[0].entry(pdf).size, 5);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn excludes_the_output_directory_from_the_walk() {
        let root = temp_dir("exclude-out");
        let out = root.join("index");
        fs::create_dir_all(&out).expect("out");
        fs::write(root.join("keep.txt"), b"x").expect("file");
        fs::write(out.join("ignore.txt"), b"x").expect("file");

        scan(&root, &out, &ScanOptions::default()).expect("scan");
        let set = IndexSet::open(&out).expect("open");
        let names: Vec<String> = set.volumes()[0]
            .iter_ids()
            .map(|id| set.volumes()[0].entry(id).name_search.to_string())
            .collect();
        assert!(names.contains(&"keep.txt".to_string()));
        assert!(!names.contains(&"ignore.txt".to_string()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn honors_excluded_paths_and_names() {
        let root = temp_dir("exclude-rules");
        let out = temp_dir("index");
        fs::create_dir_all(root.join("keep")).expect("keep");
        fs::create_dir_all(root.join("node_modules").join("pkg")).expect("modules");
        fs::create_dir_all(root.join("old").join("nested")).expect("old");
        fs::write(root.join("keep").join("good.txt"), b"x").expect("file");
        fs::write(root.join("node_modules").join("pkg").join("skip.js"), b"x").expect("file");
        fs::write(root.join("old").join("nested").join("skip.txt"), b"x").expect("file");

        let options = ScanOptions {
            excluded_paths: vec![root.join("old").to_string_lossy().to_string()],
            excluded_names: vec!["node_modules".into()],
            ..ScanOptions::default()
        };
        scan(&root, &out, &options).expect("scan");
        let set = IndexSet::open(&out).expect("open");
        let names: Vec<String> = set.volumes()[0]
            .iter_ids()
            .map(|id| set.volumes()[0].entry(id).name_search.to_string())
            .collect();
        assert!(names.contains(&"good.txt".to_string()));
        assert!(!names.contains(&"skip.js".to_string()));
        assert!(!names.contains(&"skip.txt".to_string()));
        assert!(!names.contains(&"old".to_string()));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn a_cancelled_scan_leaves_the_output_alone() {
        let root = temp_dir("cancel");
        let out = temp_dir("cancel-out");
        fs::create_dir_all(root.join("a")).expect("root");
        let cancel = Arc::new(AtomicBool::new(true));
        let progress = Arc::new(AtomicU32::new(0));
        let options = ScanOptions {
            cancel: Some(Arc::clone(&cancel)),
            progress: Some(Arc::clone(&progress)),
            ..ScanOptions::default()
        };
        let error = scan(&root, &out, &options).expect_err("cancelled");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(!out.join("manifest.bin").exists());

        cancel.store(false, Ordering::Relaxed);
        scan(&root, &out, &options).expect("scan");
        assert!(progress.load(Ordering::Relaxed) >= 1);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn library_directories_are_named_per_root() {
        assert_eq!(library_dir_name(Path::new("C:\\")), "vol-c");
        assert_eq!(library_dir_name(Path::new("D:")), "vol-d");
        let folder = library_dir_name(Path::new(r"C:\Users\Me"));
        assert!(folder.starts_with("vol-") && folder.len() == 20);
        assert_eq!(folder, library_dir_name(Path::new(r"c:\users\me\")));
    }
}

#[cfg(test)]
mod tree_tests {
    use super::*;

    #[test]
    fn trees_survive_the_pipe_format() {
        let tree = Tree {
            root_display: r"C:\".into(),
            entries: Entries::from(vec![
                Pending {
                    name: String::new(),
                    parent: 0,
                    size: 0,
                    mtime: 1,
                    flags: 1,
                    is_dir: true,
                },
                Pending {
                    name: "naïve résumé.txt".into(),
                    parent: 0,
                    size: 42,
                    mtime: 7,
                    flags: 2,
                    is_dir: false,
                },
            ]),
            report: ScanReport {
                skipped: 3,
                truncated: true,
                ..ScanReport::default()
            },
        };
        let mut bytes = Vec::new();
        tree.encode(&mut bytes).unwrap();
        let back = Tree::decode(&mut bytes.as_slice()).unwrap();
        assert_eq!(back.root_display, tree.root_display);
        assert_eq!(back.entries, tree.entries);
        assert_eq!(back.report.skipped, 3);
        assert!(back.report.truncated);

        // A parent that points forward is refused.
        let mut bad = tree.clone();
        bad.entries.parent[1] = 5;
        let mut bytes = Vec::new();
        bad.encode(&mut bytes).unwrap();
        assert!(Tree::decode(&mut bytes.as_slice()).is_err());
        assert!(Tree::decode(&mut &b"nope"[..]).is_err());
    }

    #[test]
    fn big_trees_are_written_in_slices_without_losing_anything() {
        // More entries than one write slice holds, with repeated names,
        // mixed case and names that are not ASCII.
        let count = 700_000u32;
        let mut entries = vec![Pending {
            name: String::new(),
            parent: 0,
            size: 0,
            mtime: 0,
            flags: 1,
            is_dir: true,
        }];
        let folders = 500u32;
        for index in 1..count {
            let is_dir = index <= folders;
            let name = match index % 5 {
                0 => format!("Report {}.PDF", index % 9_000),
                1 => format!("\u{00e9}t\u{00e9}-{index}.txt"),
                2 => format!("same{}", index % 50),
                3 => format!("\u{03a3}\u{03b9}\u{03c3}-{index}"),
                _ => format!("file-{index:07}.rs"),
            };
            let parent = if is_dir {
                (index - 1) / 2
            } else {
                1 + index % folders
            };
            entries.push(Pending {
                name,
                parent,
                size: u64::from(index) * 3,
                mtime: u64::from(index) * 7,
                flags: if is_dir { 1 } else { 0 },
                is_dir,
            });
        }
        let expected: Vec<(String, u32, u64, u64)> = entries
            .iter()
            .map(|entry| (entry.name.clone(), entry.parent, entry.size, entry.mtime))
            .collect();
        let dir = std::env::temp_dir().join(format!("feather-slices-{}", std::process::id()));
        let tree = Tree {
            root_display: r"C:\Slices".to_string(),
            entries: Entries::from(entries),
            report: ScanReport::default(),
        };
        let report = write_tree(&dir, tree, Instant::now()).expect("write");
        assert_eq!(report.entries, count);
        assert_eq!(report.dirs, folders + 1);

        let volume = crate::index::VolumeIndex::open(&dir).expect("open");
        assert_eq!(volume.entry_count(), count);
        // Sorted by the lowercased name, and every entry found once with
        // its own size, time and parent.
        let mut seen = vec![false; count as usize];
        let mut previous = String::new();
        for id in 1..count {
            let entry = volume.entry(id);
            assert!(entry.name_search >= previous.as_str(), "not sorted at {id}");
            previous = entry.name_search.to_string();
            let old = (entry.size / 3) as usize;
            let (name, parent, size, mtime) = &expected[old];
            assert!(!seen[old]);
            seen[old] = true;
            assert_eq!(String::from_utf16_lossy(entry.name_display), *name);
            assert_eq!(entry.name_search, name.to_lowercase());
            assert_eq!((entry.size, entry.mtime), (*size, *mtime));
            let parent_entry = volume.entry(volume.lookup_frn(entry.parent_frn).expect("parent"));
            assert_eq!(parent_entry.size / 3, u64::from(*parent));
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
