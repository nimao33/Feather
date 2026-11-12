//! How big shared folders are. Adding up a folder can take long, so it is
//! never done while a listing waits: the page asks for the sizes after it
//! has shown the folder, one worker thread adds them up (the folders asked
//! for last first), and the page asks again until all are there. Each
//! folder gets a time and entry budget; past it the total is a lower bound.
//! Only what a visitor may see is counted, so a total can't reveal hidden
//! or left-out files. Totals are kept a few minutes, and forgotten whenever
//! anything is changed through the share.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::files::{self, Roots};

/// How long one folder may take, and how many entries it may read.
const TIME: Duration = Duration::from_secs(10);
const ENTRIES: u64 = 300_000;
/// How long a total is kept.
const KEEP: Duration = Duration::from_secs(180);
/// Folders waiting, at most; the oldest requests are dropped first.
const QUEUE: usize = 2_000;
const DEPTH: usize = 64;

/// A folder's total.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Total {
    /// Bytes in every file inside, at any depth.
    pub bytes: u64,
    /// Files and folders directly inside.
    pub items: u64,
    /// The budget ran out: the real total is bigger.
    pub partial: bool,
}

#[derive(Default)]
struct Inner {
    done: HashMap<PathBuf, (Total, Instant)>,
    queue: VecDeque<PathBuf>,
    queued: HashSet<PathBuf>,
    working: bool,
}

#[derive(Default)]
pub struct Sizes {
    inner: Mutex<Inner>,
}

impl Sizes {
    /// The totals known for `folders` (resolved paths), starting work on
    /// the others. Never waits for any folder to be added up.
    pub fn lookup(self: &Arc<Self>, roots: &Arc<Roots>, folders: &[PathBuf]) -> Vec<Option<Total>> {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        inner
            .done
            .retain(|_, (_, at)| now.duration_since(*at) < KEEP);
        let mut found = Vec::with_capacity(folders.len());
        // The folders asked for now go first, in their order.
        for folder in folders.iter().rev() {
            if inner.done.contains_key(folder) {
                continue;
            }
            if inner.queued.contains(folder) {
                inner.queue.retain(|queued| queued != folder);
            } else {
                inner.queued.insert(folder.clone());
            }
            inner.queue.push_front(folder.clone());
        }
        while inner.queue.len() > QUEUE {
            if let Some(dropped) = inner.queue.pop_back() {
                inner.queued.remove(&dropped);
            }
        }
        for folder in folders {
            found.push(inner.done.get(folder).map(|(total, _)| *total));
        }
        if !inner.queue.is_empty() && !inner.working {
            inner.working = true;
            let (sizes, roots) = (Arc::clone(self), Arc::clone(roots));
            let started = std::thread::Builder::new()
                .name("feather-sizes".into())
                .spawn(move || sizes.work(&roots));
            if started.is_err() {
                inner.working = false;
            }
        }
        found
    }

    /// Something changed: every total may be wrong now.
    pub fn forget(&self) {
        self.inner.lock().unwrap().done.clear();
    }

    fn work(&self, roots: &Roots) {
        loop {
            let folder = {
                let mut inner = self.inner.lock().unwrap();
                match inner.queue.pop_front() {
                    Some(folder) => folder,
                    None => {
                        inner.working = false;
                        return;
                    }
                }
            };
            let total = add_up(roots, &folder);
            let mut inner = self.inner.lock().unwrap();
            inner.queued.remove(&folder);
            inner.done.insert(folder, (total, Instant::now()));
        }
    }
}

/// Adds up what a visitor may see in `folder`. Links and junctions aren't
/// followed: what they lead to is counted where it really is, if shared.
pub fn add_up(roots: &Roots, folder: &Path) -> Total {
    let started = Instant::now();
    let mut total = Total::default();
    let mut read = 0u64;
    let mut pending = vec![(folder.to_path_buf(), 0usize)];
    while let Some((at, depth)) = pending.pop() {
        let Ok(listing) = std::fs::read_dir(&at) else {
            continue;
        };
        for entry in listing.flatten() {
            read += 1;
            if read > ENTRIES || (read.is_multiple_of(256) && started.elapsed() > TIME) {
                total.partial = true;
                return total;
            }
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !roots.visible(&path) || roots.is_blocked(&path) {
                continue;
            }
            let link = metadata.file_type().is_symlink() || files::is_reparse(&metadata);
            if depth == 0 {
                total.items += 1;
            }
            if link {
                continue;
            }
            if metadata.is_dir() {
                if depth + 1 < DEPTH {
                    pending.push((path, depth + 1));
                } else {
                    total.partial = true;
                }
            } else {
                total.bytes += metadata.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_add_up_without_what_is_left_out() {
        let base = std::env::temp_dir().join(format!("feather-sizes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("a").join("deep")).unwrap();
        std::fs::create_dir_all(base.join("private")).unwrap();
        std::fs::write(base.join("a").join("one.txt"), [0u8; 100]).unwrap();
        std::fs::write(base.join("a").join("deep").join("two.txt"), [0u8; 50]).unwrap();
        std::fs::write(base.join("private").join("secret.txt"), [0u8; 1000]).unwrap();
        let real = files::display_path(&std::fs::canonicalize(&base).unwrap());
        let sep = std::path::MAIN_SEPARATOR;
        let (roots, _) = Roots::new(
            &[base.to_string_lossy().to_string()],
            &[format!("{real}{sep}private")],
            true,
            true,
        );
        let roots = Arc::new(roots);
        let top = roots.resolve(0, "").unwrap();
        assert_eq!(
            add_up(&roots, &top),
            Total {
                bytes: 150,
                items: 1,
                partial: false
            }
        );
        let a = roots.resolve(0, "a").unwrap();
        assert_eq!(add_up(&roots, &a).items, 2);
        let sizes = Arc::new(Sizes::default());
        let mut answer = sizes.lookup(&roots, std::slice::from_ref(&a));
        for _ in 0..200 {
            if answer[0].is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            answer = sizes.lookup(&roots, std::slice::from_ref(&a));
        }
        assert_eq!(answer[0].map(|total| total.bytes), Some(150));
        sizes.forget();
        assert!(sizes.inner.lock().unwrap().done.is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }
}
