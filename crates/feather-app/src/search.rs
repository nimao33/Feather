//! Search orchestration. The UI owns the controller; a single worker thread
//! scans the mapped indexes and streams batches. Every request carries a
//! generation so a slow search can never overwrite a newer one.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use feather_core::{EntryRef, IndexSet, Query};

pub const RESULT_CAP: usize = 50_000;
const BATCH_SIZE: usize = 1_000;
const CHECK_MASK: u32 = 0x3ff;

#[derive(Clone, Debug)]
pub struct SearchOptions {
    pub show_hidden: bool,
    pub show_system: bool,
    pub excluded_paths: Vec<String>,
    /// One bit per open volume. Zero disables every volume.
    pub volume_mask: u64,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            show_hidden: true,
            show_system: true,
            excluded_paths: Vec::new(),
            volume_mask: u64::MAX,
        }
    }
}

impl SearchOptions {
    /// Keeps only volumes that are actually open and clears paths that would
    /// exclude everything by accident.
    pub fn for_index(mut self, index: &IndexSet) -> Self {
        let available = index.volume_mask(&[]);
        self.volume_mask &= available;
        self
    }

    /// `None` when an excluded folder covers the whole volume at `prefix`;
    /// otherwise whether any excluded folder lies inside it.
    fn volume_excludes(&self, prefix: &str) -> Option<bool> {
        if prefix.trim().is_empty() {
            return Some(!self.excluded_paths.is_empty());
        }
        let mut inside = false;
        for excluded in &self.excluded_paths {
            if feather_core::path_within(prefix, excluded) {
                return None;
            }
            inside |= feather_core::path_within(excluded, prefix);
        }
        Some(inside)
    }

    fn excludes_path(&self, path: &str) -> bool {
        self.excluded_paths
            .iter()
            .any(|excluded| feather_core::path_within(path, excluded))
    }
}

#[derive(Clone)]
struct SearchRequest {
    generation: u64,
    query: Query,
    cap: usize,
    options: SearchOptions,
}

#[derive(Debug)]
pub enum SearchEvent {
    Batch {
        generation: u64,
        ids: Vec<EntryRef>,
    },
    /// `count` entries of `volume` in a row, from id `first`: what a search
    /// that matches everything sends instead of millions of ids.
    Span {
        generation: u64,
        volume: usize,
        first: u32,
        count: u32,
    },
    Done {
        generation: u64,
        total: u64,
        capped: bool,
        elapsed: Duration,
    },
}

/// Volumes with fewer entries than this are scanned on one thread: for
/// them, starting threads costs more than it saves.
const PARALLEL_MIN: usize = 200_000;

/// What scanning one volume needs, shared by the threads scanning its parts.
struct Scan<'a> {
    index: &'a IndexSet,
    volume_index: usize,
    query: &'a Query,
    filter: &'a feather_core::query::VolumeFilter,
    options: &'a SearchOptions,
    scoped: bool,
    excluded: bool,
    cancelled: &'a (dyn Fn() -> bool + Sync),
}

impl Scan<'_> {
    /// The matches among `ids`, in order, at most `limit` of them; `None`
    /// when the search was cancelled.
    fn run(&self, ids: impl Iterator<Item = u32>, limit: usize) -> Option<Vec<EntryRef>> {
        let volume = &self.index.volumes()[self.volume_index];
        let query = self.query;
        let mut paths = feather_core::PathCheck::new(query);
        let mut found = Vec::new();
        // Live changes: base entries deleted since are skipped.
        let changed = volume.has_changes();
        for (step, id) in ids.enumerate() {
            if step as u32 & CHECK_MASK == 0 && (self.cancelled)() {
                return None;
            }
            if changed && volume.is_removed(id) {
                continue;
            }
            let entry = volume.entry_summary(id);
            if (!self.options.show_hidden && entry.flags & 2 != 0)
                || (!self.options.show_system && entry.flags & 4 != 0)
                || !query.matches_name(entry.name_search, entry.size, entry.flags)
                || !query.matches_folder_parts(self.filter, entry.parent_frn, entry.name_search)
            {
                continue;
            }
            let reference = EntryRef::new(self.volume_index, id);
            if self.excluded {
                let Some(path) = self.index.resolve_path(reference) else {
                    continue;
                };
                if !query.matches_path(&path) || self.options.excludes_path(&path) {
                    continue;
                }
            } else if self.scoped
                && !paths.matches(volume, entry.parent_frn, entry.name_search, || {
                    self.index.resolve_path(reference)
                })
            {
                continue;
            }
            found.push(reference);
            if found.len() >= limit {
                break;
            }
        }
        Some(found)
    }
}

pub struct SearchController {
    pending: Arc<(Mutex<Option<SearchRequest>>, Condvar)>,
    generation: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    events: Receiver<SearchEvent>,
    thread: Option<JoinHandle<()>>,
}

impl SearchController {
    pub fn new(index: Arc<IndexSet>) -> Self {
        let pending = Arc::new((Mutex::new(None::<SearchRequest>), Condvar::new()));
        let generation = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, events) = mpsc::channel();

        let worker_pending = Arc::clone(&pending);
        let worker_generation = Arc::clone(&generation);
        let worker_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("feather-search".into())
            .stack_size(256 * 1024)
            .spawn(move || loop {
                let request = {
                    let (lock, condvar) = &*worker_pending;
                    let mut guard = lock.lock().unwrap();
                    while guard.is_none() && !worker_stop.load(Ordering::SeqCst) {
                        guard = condvar.wait(guard).unwrap();
                    }
                    if worker_stop.load(Ordering::SeqCst) {
                        break;
                    }
                    match guard.take() {
                        Some(request) => request,
                        None => break,
                    }
                };

                let started = Instant::now();
                let workers = std::thread::available_parallelism()
                    .map_or(1, |count| count.get())
                    .clamp(1, 8) as u32;
                let cap = request.cap.max(1);
                let mut batch = Vec::with_capacity(BATCH_SIZE);
                let mut total = 0u64;
                let mut capped = false;
                let mut aborted = false;
                let show_hidden = request.options.show_hidden;
                let show_system = request.options.show_system;
                let query = &request.query;
                let options = &request.options;

                'volumes: for (volume_index, volume) in index.volumes().iter().enumerate() {
                    if options.volume_mask & (1u64 << volume_index) == 0 {
                        continue;
                    }
                    // Volumes a scope rules out are skipped; on a volume a
                    // scope covers, paths aren't built at all.
                    let Some(scoped) = query.volume_paths(&volume.prefix) else {
                        continue;
                    };
                    let Some(excluded) = options.volume_excludes(&volume.prefix) else {
                        continue;
                    };
                    let needs_path = scoped || excluded;
                    // Everything on this volume matches: send it as runs,
                    // one for an unchanged volume. Live changes split it
                    // around deleted entries and put added ones in place.
                    if !needs_path && show_hidden && show_system && query.matches_everything() {
                        if !batch.is_empty() {
                            let _ = sender.send(SearchEvent::Batch {
                                generation: request.generation,
                                ids: std::mem::take(&mut batch),
                            });
                        }
                        for (first, run) in volume.live_runs() {
                            let left = cap.saturating_sub(total as usize);
                            let count = run.min(left as u32);
                            if count > 0 {
                                let _ = sender.send(SearchEvent::Span {
                                    generation: request.generation,
                                    volume: volume_index,
                                    first,
                                    count,
                                });
                            }
                            total += u64::from(count);
                            if count < run {
                                capped = true;
                                break 'volumes;
                            }
                        }
                        continue;
                    }
                    let filter = query.volume_filter(volume);
                    if !filter.may_match() {
                        continue;
                    }
                    let cancelled = || {
                        worker_stop.load(Ordering::SeqCst)
                            || worker_generation.load(Ordering::SeqCst) != request.generation
                    };
                    let scan = Scan {
                        index: &index,
                        volume_index,
                        query,
                        filter: &filter,
                        options,
                        scoped,
                        excluded,
                        cancelled: &cancelled,
                    };
                    // Big volumes are split across cores; each part keeps its
                    // matches in order and the parts are sent in order.
                    let ids = volume.base_ids();
                    let threads = if ids.len() >= PARALLEL_MIN {
                        workers
                    } else {
                        1
                    };
                    let step = ids.len().div_ceil(threads as usize) as u32;
                    let ranges: Vec<_> = (0..threads)
                        .map(|part| {
                            let first = ids.start + part * step;
                            first.min(ids.end)..(first + step).min(ids.end)
                        })
                        .collect();
                    let left = cap.saturating_sub(total as usize);
                    let mut parts: Vec<Option<Vec<EntryRef>>> = if threads == 1 {
                        vec![scan.run(ranges[0].clone(), left)]
                    } else {
                        std::thread::scope(|scope| {
                            let handles: Vec<_> = ranges
                                .iter()
                                .map(|range| {
                                    let scan = &scan;
                                    let range = range.clone();
                                    std::thread::Builder::new()
                                        .stack_size(256 * 1024)
                                        .spawn_scoped(scope, move || scan.run(range, left))
                                })
                                .collect();
                            handles
                                .into_iter()
                                .map(|handle| {
                                    handle.ok().and_then(|handle| handle.join().ok().flatten())
                                })
                                .collect()
                        })
                    };
                    if parts.iter().any(Option::is_none) {
                        aborted = true;
                        break 'volumes;
                    }
                    // Entries added since the base was written, merged
                    // into the base's matches in name order.
                    let added = volume.overlay_ids();
                    if !added.is_empty() {
                        let Some(extra) = scan.run(added.iter().copied(), left) else {
                            aborted = true;
                            break 'volumes;
                        };
                        if !extra.is_empty() {
                            let found: Vec<EntryRef> = parts
                                .iter_mut()
                                .flat_map(|part| part.take().unwrap_or_default())
                                .collect();
                            parts = vec![Some(merge_by_name(volume, found, extra))];
                        }
                    }
                    for part in parts.iter_mut() {
                        let mut found = part.take().unwrap_or_default();
                        let room = cap.saturating_sub(total as usize);
                        if found.len() >= room {
                            found.truncate(room);
                            capped = true;
                        }
                        total += found.len() as u64;
                        if !found.is_empty() {
                            if !batch.is_empty() {
                                let _ = sender.send(SearchEvent::Batch {
                                    generation: request.generation,
                                    ids: std::mem::take(&mut batch),
                                });
                            }
                            let _ = sender.send(SearchEvent::Batch {
                                generation: request.generation,
                                ids: found,
                            });
                        }
                        if capped {
                            break 'volumes;
                        }
                    }
                }

                if aborted || worker_stop.load(Ordering::SeqCst) {
                    continue;
                }
                if worker_generation.load(Ordering::SeqCst) != request.generation {
                    continue;
                }
                if !batch.is_empty() {
                    let _ = sender.send(SearchEvent::Batch {
                        generation: request.generation,
                        ids: batch,
                    });
                }
                let _ = sender.send(SearchEvent::Done {
                    generation: request.generation,
                    total,
                    capped,
                    elapsed: started.elapsed(),
                });
            })
            .expect("spawn search worker");

        Self {
            pending,
            generation,
            stop,
            events,
            thread: Some(thread),
        }
    }

    pub fn submit(&self, query: Query, cap: usize, options: SearchOptions) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let (lock, condvar) = &*self.pending;
        let mut guard = lock.lock().unwrap();
        *guard = Some(SearchRequest {
            generation,
            query,
            cap,
            options,
        });
        condvar.notify_one();
        generation
    }

    /// Drops any pending request and invalidates a running search without
    /// starting another one. The returned generation is safe to store in the
    /// UI model so late events from the old search are ignored.
    pub fn invalidate(&self) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let (lock, _) = &*self.pending;
        *lock.lock().unwrap() = None;
        generation
    }

    pub fn try_recv(&self) -> Result<SearchEvent, TryRecvError> {
        self.events.try_recv()
    }
}

/// Merges two lists of one volume's entries, each in name order, into one.
fn merge_by_name(
    volume: &feather_core::VolumeIndex,
    left: Vec<EntryRef>,
    right: Vec<EntryRef>,
) -> Vec<EntryRef> {
    let mut merged = Vec::with_capacity(left.len() + right.len());
    let mut right = right.into_iter().peekable();
    for entry in left {
        while let Some(&other) = right.peek() {
            if volume.compare_names(other.id(), entry.id()).is_lt() {
                merged.push(other);
                right.next();
            } else {
                break;
            }
        }
        merged.push(entry);
    }
    merged.extend(right);
    merged
}

/// The rows of `previous` that `query` matches, in the same order, or
/// `None` when more than `cap` do. Only valid when `query` narrows the query
/// `previous` came from, with the same options: then nothing outside it can
/// match, and typing further costs a pass over the shown rows instead of a
/// pass over the whole index.
pub fn narrow(
    index: &IndexSet,
    previous: &crate::rows::Rows,
    query: &Query,
    cap: usize,
) -> Option<crate::rows::Rows> {
    let volumes = index.volumes();
    let mut paths: Vec<_> = volumes
        .iter()
        .map(|_| feather_core::PathCheck::new(query))
        .collect();
    let needs_path = query.needs_path();
    let mut rows = crate::rows::Rows::default();
    let mut chunk = Vec::with_capacity(BATCH_SIZE);
    let mut count = 0usize;
    for reference in previous.iter() {
        let Some(volume) = volumes.get(reference.volume()) else {
            continue;
        };
        if volume.is_removed(reference.id()) {
            continue;
        }
        let entry = volume.entry_summary(reference.id());
        if !query.matches_name(entry.name_search, entry.size, entry.flags) {
            continue;
        }
        if needs_path
            && !paths[reference.volume()].matches(
                volume,
                entry.parent_frn,
                entry.name_search,
                || index.resolve_path(reference),
            )
        {
            continue;
        }
        count += 1;
        if count > cap {
            return None;
        }
        chunk.push(reference);
        if chunk.len() == BATCH_SIZE {
            rows.push_refs(&chunk);
            chunk.clear();
        }
    }
    rows.push_refs(&chunk);
    rows.shrink();
    Some(rows)
}

impl Drop for SearchController {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
        {
            let (lock, condvar) = &*self.pending;
            let _guard = lock.lock().unwrap();
            condvar.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
