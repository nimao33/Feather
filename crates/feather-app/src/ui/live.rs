//! Live index updates for whole NTFS drives.
//!
//! A background thread asks each tracked drive's change journal what
//! changed since the index was written, about once a second, and keeps the
//! answer in a small overlay per drive (`feather_core::live::Overlay`).
//! Whenever something visible changed it publishes a new `IndexSet`
//! snapshot: the same mapped files with the new overlays, so nothing is
//! copied or read again. The UI searches the snapshot like any new index.
//!
//! Reading a journal needs admin rights: the thread reads it directly when
//! Feather already runs as admin, and otherwise through the session's
//! elevated helper (one UAC prompt, shared with full rebuilds). When the
//! journal no longer reaches back to the index (it wrapped while Feather
//! was closed, or was recreated) the drive is rebuilt once as before; when
//! admin rights are declined it falls back to the folder watcher.

use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use feather_core::live::{
    entry_flags, Batch, Change, EntryState, JournalCursor, Overlay, Rules, FILE_ATTRIBUTE_DIRECTORY,
};
use feather_core::IndexSet;

use super::helper;

/// How often the journals are read.
pub const INTERVAL: Duration = Duration::from_secs(1);
/// Overlay size (added entries plus removed base entries) at which the
/// overlay is folded into a new base.
pub const FOLD_AT: usize = 50_000;
/// A non-empty overlay older than this is folded too, so the stored
/// journal position never falls far behind the journal itself.
pub const FOLD_AFTER: Duration = Duration::from_secs(6 * 60 * 60);
/// A folder moved in from outside the index is listed from the disk up to
/// this many items; a bigger one rebuilds the drive instead.
const WALK_LIMIT: usize = 100_000;
/// Batches read per drive per round, so one busy drive cannot stall the
/// others for long.
const ROUNDS: usize = 20;

/// One drive followed through its journal.
#[derive(Clone, Debug)]
pub struct Tracked {
    /// Its volume's place in the index set.
    pub volume: usize,
    /// The searched place, as Settings has it (`C:\`).
    pub root: String,
    /// `C:`.
    pub drive: String,
    /// Where its index left the journal.
    pub cursor: JournalCursor,
}

pub enum LiveEvent {
    /// The index with every change so far.
    Snapshot(Arc<IndexSet>),
    /// These places must be read again: their journal no longer covers
    /// their index, or following it failed.
    Rebuild(Vec<String>),
    /// No way to read these journals (admin rights declined): watch the
    /// folders and rebuild instead.
    Unavailable(Vec<String>),
    /// An overlay grew large; fold it into a new base when convenient.
    Fold,
}

/// Reads the changes on a drive since a cursor.
pub type ReadJournal = Box<dyn FnMut(&str, JournalCursor) -> io::Result<Batch> + Send>;
/// Lists a folder with each item's file reference number.
pub type ListDir =
    Box<dyn FnMut(&std::path::Path) -> io::Result<Vec<feather_core::usn::DirItem>> + Send>;

pub struct Live {
    stop: Arc<(Mutex<bool>, Condvar)>,
    events: Receiver<LiveEvent>,
    thread: Option<JoinHandle<()>>,
}

impl Live {
    /// Follows `tracked` drives of `base`, reading journals directly or
    /// through the helper (`elevate` allows its one prompt).
    pub fn start(base: Arc<IndexSet>, tracked: Vec<Tracked>, rules: Rules, elevate: bool) -> Self {
        Self::with_sources(
            base,
            tracked,
            rules,
            journal_reader(elevate),
            Box::new(feather_core::usn::list_dir),
            INTERVAL,
        )
    }

    /// Like `start`, with the journal and the disk supplied by the caller,
    /// so tests can follow made-up changes.
    pub fn with_sources(
        base: Arc<IndexSet>,
        tracked: Vec<Tracked>,
        rules: Rules,
        read: ReadJournal,
        list: ListDir,
        interval: Duration,
    ) -> Self {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let (sender, events) = mpsc::channel();
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("feather-live".into())
            .spawn(move || {
                let mut follower = Follower::new(base, tracked, rules, read, list);
                loop {
                    follower.round(&sender);
                    let (lock, condvar) = &*thread_stop;
                    let Ok(guard) = lock.lock() else {
                        return;
                    };
                    let Ok((guard, _)) = condvar.wait_timeout_while(guard, interval, |stop| !*stop)
                    else {
                        return;
                    };
                    if *guard || follower.tracked.is_empty() {
                        return;
                    }
                }
            })
            .ok();
        Self {
            stop,
            events,
            thread,
        }
    }

    pub fn try_event(&self) -> Option<LiveEvent> {
        self.events.try_recv().ok()
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let (lock, condvar) = &*self.stop;
        if let Ok(mut stop) = lock.lock() {
            *stop = true;
        }
        condvar.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Reads journals directly when this process may, else via the helper.
fn journal_reader(elevate: bool) -> ReadJournal {
    let mut direct = true;
    Box::new(move |drive, cursor| {
        if direct {
            match feather_core::usn::read_changes(drive, cursor, helper::JOURNAL_RECORDS) {
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => direct = false,
                other => return other,
            }
        }
        if elevate && helper::available() {
            helper::journal(drive, cursor)
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "reading the change journal needs admin rights",
            ))
        }
    })
}

/// The thread's state: the base, one overlay per tracked drive.
struct Follower {
    base: Arc<IndexSet>,
    tracked: Vec<Tracked>,
    overlays: Vec<Option<Overlay>>,
    rules: Rules,
    read: ReadJournal,
    list: ListDir,
    started: Instant,
    fold_asked: bool,
}

impl Follower {
    fn new(
        base: Arc<IndexSet>,
        tracked: Vec<Tracked>,
        rules: Rules,
        read: ReadJournal,
        list: ListDir,
    ) -> Self {
        let mut overlays: Vec<Option<Overlay>> = vec![None; base.volume_count()];
        let tracked: Vec<Tracked> = tracked
            .into_iter()
            .filter(|tracked| tracked.volume < overlays.len())
            .collect();
        for tracked in &tracked {
            overlays[tracked.volume] = Some(Overlay::new(
                &base.volumes()[tracked.volume],
                tracked.cursor,
            ));
        }
        Self {
            base,
            tracked,
            overlays,
            rules,
            read,
            list,
            started: Instant::now(),
            fold_asked: false,
        }
    }

    /// Reads every tracked journal once (more while a catch-up has more)
    /// and tells the UI what came of it.
    fn round(&mut self, sender: &Sender<LiveEvent>) {
        let mut changed = false;
        let mut rebuild = Vec::new();
        let mut unavailable = Vec::new();
        let tracked = std::mem::take(&mut self.tracked);
        for drive in tracked {
            match self.follow(&drive) {
                Ok(any) => {
                    changed |= any;
                    self.tracked.push(drive);
                }
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    unavailable.push(drive.root.clone());
                }
                Err(error) => {
                    eprintln!("live updates for {}: {error}", drive.root);
                    rebuild.push(drive.root.clone());
                }
            }
        }
        if changed {
            let _ = sender.send(LiveEvent::Snapshot(Arc::new(self.snapshot())));
        }
        if !rebuild.is_empty() {
            let _ = sender.send(LiveEvent::Rebuild(rebuild));
        }
        if !unavailable.is_empty() {
            let _ = sender.send(LiveEvent::Unavailable(unavailable));
        }
        let large = self.overlays.iter().flatten().any(|overlay| {
            overlay.size() >= FOLD_AT
                || (!overlay.is_empty() && self.started.elapsed() >= FOLD_AFTER)
        });
        if large && !self.fold_asked {
            self.fold_asked = true;
            let _ = sender.send(LiveEvent::Fold);
        }
    }

    /// The base with the current overlays, shared copy-on-write.
    fn snapshot(&self) -> IndexSet {
        let overlays: Vec<Option<Arc<Overlay>>> = self
            .overlays
            .iter()
            .map(|overlay| overlay.clone().map(Arc::new))
            .collect();
        self.base.with_overlays(&overlays)
    }

    /// Applies what changed on one drive. True when searches can see it.
    fn follow(&mut self, drive: &Tracked) -> io::Result<bool> {
        let base = Arc::clone(&self.base);
        let volume = &base.volumes()[drive.volume];
        let mut changed = false;
        for _ in 0..ROUNDS {
            let Some(overlay) = self.overlays[drive.volume].as_mut() else {
                return Ok(changed);
            };
            let batch = (self.read)(&drive.drive, overlay.cursor())?;
            let report = overlay.apply(volume, &batch.changes, &self.rules);
            overlay.set_cursor(batch.cursor);
            changed |= report.changed;
            for folder in report.moved_in {
                self.fill_folder(drive.volume, folder)?;
            }
            if !batch.more {
                break;
            }
        }
        Ok(changed)
    }

    /// Lists a folder that was moved in from outside the index, and what
    /// is inside it, into the overlay.
    fn fill_folder(&mut self, volume_index: usize, frn: u64) -> io::Result<()> {
        let base = &self.base.volumes()[volume_index];
        let Some(overlay) = self.overlays[volume_index].as_mut() else {
            return Ok(());
        };
        let view = base.with_overlay(Some(Arc::new(overlay.clone())));
        let Some(path) = view.lookup_frn(frn).and_then(|id| view.resolve_path(id)) else {
            return Ok(());
        };
        drop(view);
        let mut changes = Vec::new();
        let mut folders: Vec<(u64, PathBuf)> = vec![(frn, PathBuf::from(path))];
        while let Some((parent, path)) = folders.pop() {
            // A folder that cannot be listed is simply left empty.
            let Ok(items) = (self.list)(&path) else {
                continue;
            };
            for item in items {
                let is_dir = item.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                if is_dir {
                    folders.push((item.frn, path.join(&item.name)));
                }
                changes.push(Change::Upsert(EntryState {
                    frn: item.frn,
                    parent_frn: parent,
                    name: item.name,
                    size: Some(if is_dir { 0 } else { item.size }),
                    mtime: item.mtime,
                    flags: entry_flags(is_dir, item.attributes),
                    // Listed whole here, so not listed again.
                    created: true,
                }));
                if changes.len() > WALK_LIMIT {
                    return Err(io::Error::other("a folder too big to list was moved in"));
                }
            }
        }
        overlay.apply(base, &changes, &self.rules);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feather_core::scan::{write_tree, Entries, ScanReport, Tree};
    use feather_core::usn::DirItem;

    const ROOT: u64 = 5 | (5 << 48);
    const DOCS: u64 = 100;
    const REPORT: u64 = 101;
    const CURSOR: JournalCursor = JournalCursor {
        journal_id: 9,
        next_usn: 100,
    };

    fn library() -> (PathBuf, Arc<IndexSet>) {
        let dir = std::env::temp_dir().join(format!(
            "feather-live-ui-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let mut entries = Entries::default();
        for (name, parent, frn, dir) in [
            ("", 0, ROOT, true),
            ("docs", 0, DOCS, true),
            ("report.txt", 1, REPORT, false),
        ] {
            entries.push(name, parent, 1, 1, u16::from(dir));
            entries.frn.push(frn);
        }
        let tree = Tree {
            root_display: "C:\\".into(),
            entries,
            report: ScanReport::default(),
            journal: Some(CURSOR),
        };
        write_tree(&dir.join("vol-c"), tree, Instant::now()).expect("write");
        let set = Arc::new(IndexSet::open(&dir).expect("open"));
        (dir, set)
    }

    fn tracked() -> Vec<Tracked> {
        vec![Tracked {
            volume: 0,
            root: "C:\\".into(),
            drive: "C:".into(),
            cursor: CURSOR,
        }]
    }

    fn upsert(frn: u64, parent: u64, name: &str, dir: bool, created: bool) -> Change {
        Change::Upsert(EntryState {
            frn,
            parent_frn: parent,
            name: name.into(),
            size: Some(3),
            mtime: 1,
            flags: u16::from(dir),
            created,
        })
    }

    fn next(live: &Live) -> LiveEvent {
        let started = Instant::now();
        loop {
            if let Some(event) = live.try_event() {
                return event;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "no event");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn changes_become_snapshots_and_the_cursor_moves_on() {
        let (dir, set) = library();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&asked);
        let mut step = 0;
        let read: ReadJournal = Box::new(move |drive, cursor| {
            seen.lock()
                .unwrap()
                .push((drive.to_string(), cursor.next_usn));
            step += 1;
            let changes = match step {
                // A catch-up in two parts, then quiet.
                1 => vec![upsert(300, DOCS, "first.txt", false, true)],
                2 => vec![upsert(301, ROOT, "second.txt", false, true)],
                _ => Vec::new(),
            };
            Ok(Batch {
                cursor: JournalCursor {
                    journal_id: 9,
                    next_usn: cursor.next_usn + 10,
                },
                changes,
                more: step == 1,
            })
        });
        let live = Live::with_sources(
            Arc::clone(&set),
            tracked(),
            Rules::default(),
            read,
            Box::new(|_| Ok(Vec::new())),
            Duration::from_millis(10),
        );
        let LiveEvent::Snapshot(snapshot) = next(&live) else {
            panic!("expected a snapshot");
        };
        assert_eq!(snapshot.item_count(), set.item_count() + 2);
        let volume = &snapshot.volumes()[0];
        let first = volume.lookup_frn(300).unwrap();
        assert_eq!(
            volume.resolve_path(first).as_deref(),
            Some("C:\\docs\\first.txt")
        );
        // Quiet rounds publish nothing more, and read on from the cursor.
        std::thread::sleep(Duration::from_millis(80));
        assert!(live.try_event().is_none());
        drop(live);
        let asked = asked.lock().unwrap();
        assert_eq!(asked[0], ("C:".to_string(), 100));
        assert_eq!(asked[1].1, 110);
        assert_eq!(asked[2].1, 120);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lost_journal_asks_for_a_rebuild_and_declined_rights_fall_back() {
        let (dir, set) = library();
        let live = Live::with_sources(
            Arc::clone(&set),
            tracked(),
            Rules::default(),
            Box::new(|_, _| Err(feather_core::usn::journal_gone("wrapped"))),
            Box::new(|_| Ok(Vec::new())),
            Duration::from_millis(10),
        );
        match next(&live) {
            LiveEvent::Rebuild(roots) => assert_eq!(roots, vec!["C:\\".to_string()]),
            _ => panic!("expected a rebuild"),
        }
        drop(live);
        let live = Live::with_sources(
            set,
            tracked(),
            Rules::default(),
            Box::new(|_, _| Err(io::Error::new(io::ErrorKind::PermissionDenied, "no"))),
            Box::new(|_| Ok(Vec::new())),
            Duration::from_millis(10),
        );
        match next(&live) {
            LiveEvent::Unavailable(roots) => assert_eq!(roots, vec!["C:\\".to_string()]),
            _ => panic!("expected unavailable"),
        }
        drop(live);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folders_moved_in_are_listed_from_the_disk() {
        let (dir, set) = library();
        let mut step = 0;
        let read: ReadJournal = Box::new(move |_, cursor| {
            step += 1;
            Ok(Batch {
                cursor,
                changes: if step == 1 {
                    vec![upsert(400, ROOT, "restored", true, false)]
                } else {
                    Vec::new()
                },
                more: false,
            })
        });
        let listed = Arc::new(Mutex::new(Vec::new()));
        let paths = Arc::clone(&listed);
        let list: ListDir = Box::new(move |path| {
            paths.lock().unwrap().push(path.to_path_buf());
            let item = |frn, name: &str, attributes| DirItem {
                frn,
                name: name.into(),
                attributes,
                size: 5,
                mtime: 2,
            };
            Ok(match path.to_string_lossy().as_ref() {
                "C:\\restored" => vec![item(401, "inner", 0x10), item(402, "a.txt", 0x20)],
                "C:\\restored\\inner" => vec![item(403, "b.txt", 0x20)],
                _ => Vec::new(),
            })
        });
        let live = Live::with_sources(
            set,
            tracked(),
            Rules::default(),
            read,
            list,
            Duration::from_millis(10),
        );
        let LiveEvent::Snapshot(snapshot) = next(&live) else {
            panic!("expected a snapshot");
        };
        drop(live);
        let volume = &snapshot.volumes()[0];
        let deep = volume.lookup_frn(403).expect("listed");
        assert_eq!(
            volume.resolve_path(deep).as_deref(),
            Some("C:\\restored\\inner\\b.txt")
        );
        assert_eq!(volume.entry(deep).size, 5);
        assert_eq!(listed.lock().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_large_overlay_asks_to_be_folded_once() {
        let (dir, set) = library();
        let mut sent = false;
        let read: ReadJournal = Box::new(move |_, cursor| {
            let changes = if sent {
                Vec::new()
            } else {
                sent = true;
                (0..FOLD_AT as u64)
                    .map(|index| upsert(1000 + index, DOCS, &format!("f{index}"), false, true))
                    .collect()
            };
            Ok(Batch {
                cursor,
                changes,
                more: false,
            })
        });
        let live = Live::with_sources(
            set,
            tracked(),
            Rules::default(),
            read,
            Box::new(|_| Ok(Vec::new())),
            Duration::from_millis(10),
        );
        assert!(matches!(next(&live), LiveEvent::Snapshot(_)));
        assert!(matches!(next(&live), LiveEvent::Fold));
        std::thread::sleep(Duration::from_millis(60));
        assert!(live.try_event().is_none(), "asked once");
        drop(live);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
