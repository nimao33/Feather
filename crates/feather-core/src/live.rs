//! Live index updates: a small in-memory overlay of changes on top of a
//! volume's mapped base index.
//!
//! The base files never change once written. Changes read from the NTFS
//! change journal (see `usn.rs`) arrive as [`Change`]s keyed by file
//! reference number, and [`Overlay::apply`] records them: entries that are
//! new, renamed, moved or resized are added to the overlay, and the base
//! entries they replace or that were deleted are marked removed. Searches
//! read the base and the overlay together through
//! [`crate::VolumeIndex`]; the overlay never copies the base. When it grows
//! large, [`fold`] writes base and overlay out as a new base generation.
//!
//! Everything here is plain data and runs without admin rights, so it is
//! tested against synthetic indexes.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::sync::Arc;

use crate::entry::EntryView;
use crate::format::{write_manifest, Manifest, CHECK_STRIDE, FRN_SIZE, META_SIZE};
use crate::index::{Base, View};
use crate::VolumeIndex;

/// Names the NTFS reader never indexes, anywhere in the tree.
pub const ALWAYS_SKIPPED: &[&str] = &["$recycle.bin", "system volume information", "$winreagent"];

/// The file beside a volume's index that holds its journal position.
pub const CURSOR_FILE: &str = "journal.bin";
const CURSOR_MAGIC: &[u8; 8] = b"FTHRUSN1";

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// The index flags for an entry: bit 0 folder, bit 1 hidden, bit 2 system.
pub fn entry_flags(is_dir: bool, attributes: u32) -> u16 {
    u16::from(is_dir)
        | if attributes & FILE_ATTRIBUTE_HIDDEN != 0 {
            2
        } else {
            0
        }
        | if attributes & FILE_ATTRIBUTE_SYSTEM != 0 {
            4
        } else {
            0
        }
}

/// A position in a volume's change journal: which journal, and the next
/// update sequence number to read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JournalCursor {
    pub journal_id: u64,
    pub next_usn: i64,
}

/// Writes `journal.bin` into a volume's index folder. Written once with
/// the index and never changed afterwards, like the other index files.
pub fn write_cursor(dir: &Path, cursor: JournalCursor) -> io::Result<()> {
    let mut bytes = Vec::with_capacity(28);
    bytes.extend_from_slice(CURSOR_MAGIC);
    bytes.extend_from_slice(&cursor.journal_id.to_le_bytes());
    bytes.extend_from_slice(&cursor.next_usn.to_le_bytes());
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    let temp = dir.join(format!("{CURSOR_FILE}.tmp"));
    fs::write(&temp, &bytes)?;
    fs::rename(&temp, dir.join(CURSOR_FILE))
}

/// Reads `journal.bin`, or `None` when it is missing or damaged.
pub fn read_cursor(dir: &Path) -> Option<JournalCursor> {
    let bytes = fs::read(dir.join(CURSOR_FILE)).ok()?;
    if bytes.len() != 28 || &bytes[0..8] != CURSOR_MAGIC {
        return None;
    }
    let crc = u32::from_le_bytes(bytes[24..28].try_into().ok()?);
    if crc != crc32fast::hash(&bytes[0..24]) {
        return None;
    }
    Some(JournalCursor {
        journal_id: u64::from_le_bytes(bytes[8..16].try_into().ok()?),
        next_usn: i64::from_le_bytes(bytes[16..24].try_into().ok()?),
    })
}

/// What an entry looks like now, from the journal and the file itself.
#[derive(Clone, Debug, PartialEq)]
pub struct EntryState {
    pub frn: u64,
    pub parent_frn: u64,
    pub name: String,
    /// `None` when the file could not be asked; the size known so far is
    /// kept then.
    pub size: Option<u64>,
    pub mtime: u64,
    pub flags: u16,
    /// Created since the cursor, as opposed to renamed or moved here. A
    /// folder that appears without being created was moved in from a
    /// place the index does not cover, and its contents must be listed.
    pub created: bool,
}

impl EntryState {
    pub fn is_dir(&self) -> bool {
        self.flags & 1 != 0
    }
}

/// One change to apply, keyed by file reference number.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    /// Created, renamed, moved, or changed in place.
    Upsert(EntryState),
    /// Deleted, or gone from the volume.
    Remove(u64),
}

/// The changes read from a journal in one go, and where to read next.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Batch {
    pub cursor: JournalCursor,
    pub changes: Vec<Change>,
    /// More changes were waiting than one batch holds.
    pub more: bool,
}

const BATCH_MAGIC: &[u8; 4] = b"FUS1";
const UNKNOWN_SIZE: u64 = u64::MAX;

impl Batch {
    /// Writes the batch as a compact byte stream (for the helper's pipe).
    pub fn encode(&self, out: &mut impl Write) -> io::Result<()> {
        out.write_all(BATCH_MAGIC)?;
        out.write_all(&self.cursor.journal_id.to_le_bytes())?;
        out.write_all(&self.cursor.next_usn.to_le_bytes())?;
        out.write_all(&[u8::from(self.more)])?;
        out.write_all(&(self.changes.len() as u32).to_le_bytes())?;
        for change in &self.changes {
            match change {
                Change::Remove(frn) => {
                    out.write_all(&[2])?;
                    out.write_all(&frn.to_le_bytes())?;
                }
                Change::Upsert(state) => {
                    out.write_all(&[1])?;
                    out.write_all(&state.frn.to_le_bytes())?;
                    out.write_all(&state.parent_frn.to_le_bytes())?;
                    out.write_all(&state.size.unwrap_or(UNKNOWN_SIZE).to_le_bytes())?;
                    out.write_all(&state.mtime.to_le_bytes())?;
                    out.write_all(&state.flags.to_le_bytes())?;
                    out.write_all(&[u8::from(state.created)])?;
                    let name = state.name.as_bytes();
                    let len = name.len().min(u16::MAX as usize);
                    out.write_all(&(len as u16).to_le_bytes())?;
                    out.write_all(&name[..len])?;
                }
            }
        }
        Ok(())
    }

    /// Reads a stream written by [`Batch::encode`].
    pub fn decode(input: &mut impl Read) -> io::Result<Batch> {
        let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
        let mut magic = [0u8; 4];
        input.read_exact(&mut magic)?;
        if &magic != BATCH_MAGIC {
            return Err(bad("not a Feather change stream"));
        }
        let mut b8 = [0u8; 8];
        let mut b4 = [0u8; 4];
        let mut b2 = [0u8; 2];
        let mut b1 = [0u8; 1];
        input.read_exact(&mut b8)?;
        let journal_id = u64::from_le_bytes(b8);
        input.read_exact(&mut b8)?;
        let next_usn = i64::from_le_bytes(b8);
        input.read_exact(&mut b1)?;
        let more = b1[0] != 0;
        input.read_exact(&mut b4)?;
        let count = u32::from_le_bytes(b4) as usize;
        let mut changes = Vec::with_capacity(count.min(1 << 20));
        let mut name = Vec::new();
        for _ in 0..count {
            input.read_exact(&mut b1)?;
            let kind = b1[0];
            input.read_exact(&mut b8)?;
            let frn = u64::from_le_bytes(b8);
            match kind {
                2 => changes.push(Change::Remove(frn)),
                1 => {
                    input.read_exact(&mut b8)?;
                    let parent_frn = u64::from_le_bytes(b8);
                    input.read_exact(&mut b8)?;
                    let size = u64::from_le_bytes(b8);
                    input.read_exact(&mut b8)?;
                    let mtime = u64::from_le_bytes(b8);
                    input.read_exact(&mut b2)?;
                    let flags = u16::from_le_bytes(b2);
                    input.read_exact(&mut b1)?;
                    let created = b1[0] != 0;
                    input.read_exact(&mut b2)?;
                    name.resize(u16::from_le_bytes(b2) as usize, 0);
                    input.read_exact(&mut name)?;
                    changes.push(Change::Upsert(EntryState {
                        frn,
                        parent_frn,
                        name: String::from_utf8(name.clone())
                            .map_err(|_| bad("a name is not UTF-8"))?,
                        size: (size != UNKNOWN_SIZE).then_some(size),
                        mtime,
                        flags,
                        created,
                    }));
                }
                _ => return Err(bad("unknown change kind")),
            }
        }
        Ok(Batch {
            cursor: JournalCursor {
                journal_id,
                next_usn,
            },
            changes,
            more,
        })
    }
}

/// What the index leaves out, as the NTFS reader does when it builds it.
#[derive(Clone, Debug, Default)]
pub struct Rules {
    /// Lowercase names skipped anywhere in the tree.
    names: Vec<String>,
    /// Normalized folder paths skipped with everything below them.
    paths: Vec<String>,
}

impl Rules {
    pub fn new(excluded_names: &[String], excluded_paths: &[String]) -> Self {
        Self {
            names: ALWAYS_SKIPPED
                .iter()
                .map(|name| name.to_string())
                .chain(excluded_names.iter().map(|name| name.to_lowercase()))
                .collect(),
            paths: excluded_paths
                .iter()
                .filter(|path| !path.trim().is_empty())
                .map(|path| crate::path::normalize_path(path))
                .collect(),
        }
    }

    fn skips_name(&self, lower: &str) -> bool {
        self.names.iter().any(|name| name == lower)
    }

    fn skips_path(&self, path: &str) -> bool {
        let path = crate::path::normalize_path(path);
        self.paths.contains(&path)
    }
}

/// What applying changes did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ApplyReport {
    /// Something searches can see changed.
    pub changed: bool,
    /// Folders that appeared without being created: moved in from a place
    /// the index does not cover. Their contents are not in the journal and
    /// must be listed from the disk.
    pub moved_in: Vec<u64>,
}

/// An entry the overlay added. Names are shared, so cloning the overlay
/// for a new snapshot copies no strings.
#[derive(Clone, Debug)]
struct Added {
    frn: u64,
    parent_frn: u64,
    size: u64,
    mtime: u64,
    flags: u16,
    live: bool,
    name_search: Arc<str>,
    name_display: Arc<[u16]>,
}

impl Added {
    fn is_dir(&self) -> bool {
        self.flags & 1 != 0
    }
}

/// Changes on top of one volume's base index. Cloned (copy on write) for
/// each published snapshot; ids it hands out never change while the base
/// stays the same, so rows already shown stay valid.
#[derive(Clone, Debug, Default)]
pub struct Overlay {
    base_count: u32,
    /// Overlay id `base_count + i` is `added[i]`. Dead entries stay, so
    /// ids never move; they count towards folding.
    added: Vec<Added>,
    /// Live added entries by reference.
    by_frn: HashMap<u64, u32>,
    /// One bit per base id, set when that base entry is deleted or
    /// replaced. Empty until the first one.
    removed: Vec<u64>,
    removed_count: u32,
    /// Live overlay ids, sorted by name.
    order: Vec<u32>,
    /// One bit per base id: set for folders with at least one child in the
    /// base. Built the first time a base folder is deleted, so deleting
    /// an empty folder does not cost a pass over the whole volume.
    has_children: Option<Arc<Vec<u64>>>,
    /// Hook for folder totals: the change in recursive size and item count
    /// below each folder, by its reference, since the base was written.
    folder_deltas: HashMap<u64, (i64, i64)>,
    cursor: JournalCursor,
    /// Changes applied that searches could see.
    applied: u64,
}

fn bit(bits: &[u64], index: u32) -> bool {
    bits.get(index as usize / 64)
        .is_some_and(|word| word & (1 << (index % 64)) != 0)
}

impl Overlay {
    /// An empty overlay for `volume`'s base, reading on from `cursor`.
    pub fn new(volume: &VolumeIndex, cursor: JournalCursor) -> Self {
        Self {
            base_count: volume.base_count(),
            cursor,
            ..Self::default()
        }
    }

    pub fn cursor(&self) -> JournalCursor {
        self.cursor
    }

    pub fn set_cursor(&mut self, cursor: JournalCursor) {
        self.cursor = cursor;
    }

    /// No visible change.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed_count == 0
    }

    /// How much the overlay holds: added entries (dead ones too) and
    /// removed base entries. Folding is worth it once this is large.
    pub fn size(&self) -> usize {
        self.added.len() + self.removed_count as usize
    }

    /// Changes applied so far that searches could see.
    pub fn applied(&self) -> u64 {
        self.applied
    }

    /// Approximate heap bytes, for the memory report.
    pub fn bytes(&self) -> usize {
        let names: usize = self
            .added
            .iter()
            .map(|added| added.name_search.len() + added.name_display.len() * 2 + 32)
            .sum();
        self.added.capacity() * std::mem::size_of::<Added>()
            + names
            + self.by_frn.capacity() * 16
            + self.removed.capacity() * 8
            + self.order.capacity() * 4
            + self.folder_deltas.capacity() * 32
    }

    pub(crate) fn removed_count(&self) -> u32 {
        self.removed_count
    }

    pub(crate) fn sorted_ids(&self) -> &[u32] {
        &self.order
    }

    /// Removed base ids, ascending.
    pub(crate) fn removed_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.removed
            .iter()
            .enumerate()
            .filter(|(_, word)| **word != 0)
            .flat_map(|(index, &word)| {
                (0..64u32)
                    .filter(move |offset| word & (1u64 << offset) != 0)
                    .map(move |offset| index as u32 * 64 + offset)
            })
    }

    pub(crate) fn is_removed(&self, id: u32) -> bool {
        if id < self.base_count {
            bit(&self.removed, id)
        } else {
            self.added
                .get((id - self.base_count) as usize)
                .is_none_or(|added| !added.live)
        }
    }

    pub(crate) fn live_id(&self, frn: u64) -> Option<u32> {
        self.by_frn.get(&frn).copied()
    }

    pub(crate) fn entry(&self, id: u32) -> Option<EntryView<'_>> {
        let added = self.added.get(id.checked_sub(self.base_count)? as usize)?;
        Some(EntryView {
            id,
            frn: added.frn,
            parent_frn: added.parent_frn,
            size: added.size,
            mtime: added.mtime,
            flags: added.flags,
            search_offset: 0,
            search_len: added.name_search.len().min(u16::MAX as usize) as u16,
            display_offset: 0,
            display_len: added.name_display.len().min(u16::MAX as usize) as u16,
            name_search: &added.name_search,
            name_display: &added.name_display,
        })
    }

    /// A live added entry named `name` (lowercase) inside `parent_frn`.
    pub(crate) fn find_child(&self, parent_frn: u64, name: &str, folder: bool) -> Option<u32> {
        self.order.iter().copied().find(|&id| {
            let added = &self.added[(id - self.base_count) as usize];
            added.parent_frn == parent_frn
                && (!folder || added.is_dir())
                && &*added.name_search == name
        })
    }

    /// The change in recursive size and item count below the folder with
    /// reference `frn` since the base was written. This is the hook for
    /// folder totals: a base that stores each folder's total in its `size`
    /// adds this to it on read and when folding.
    pub fn folder_delta(&self, frn: u64) -> (i64, i64) {
        self.folder_deltas.get(&frn).copied().unwrap_or((0, 0))
    }

    fn hide(&mut self, id: u32) {
        if self.removed.is_empty() {
            self.removed = vec![0; (self.base_count as usize).div_ceil(64)];
        }
        let word = &mut self.removed[id as usize / 64];
        let mask = 1u64 << (id % 64);
        if *word & mask == 0 {
            *word |= mask;
            self.removed_count += 1;
        }
    }

    fn unhide(&mut self, id: u32) {
        if let Some(word) = self.removed.get_mut(id as usize / 64) {
            let mask = 1u64 << (id % 64);
            if *word & mask != 0 {
                *word &= !mask;
                self.removed_count -= 1;
            }
        }
    }

    fn kill(&mut self, id: u32) {
        let index = (id - self.base_count) as usize;
        if let Some(added) = self.added.get_mut(index) {
            if added.live {
                added.live = false;
                self.by_frn.remove(&added.frn);
            }
        }
    }

    /// Records `changes` against `volume`'s base (its own overlay, if any,
    /// is not consulted). Applying the same changes twice changes nothing
    /// the second time, so a replay from an older cursor is harmless.
    pub fn apply(
        &mut self,
        volume: &VolumeIndex,
        changes: &[Change],
        rules: &Rules,
    ) -> ApplyReport {
        let base = volume.base();
        let root_frn = base.entry(0).frn;
        let mut report = ApplyReport::default();
        let mut pending: Vec<&EntryState> = Vec::new();
        let mut prune = false;
        for change in changes {
            match change {
                Change::Remove(frn) if *frn != root_frn => {
                    prune |= self.remove(base, *frn, &mut report);
                }
                Change::Upsert(state) if state.frn != root_frn && state.parent_frn != state.frn => {
                    pending.push(state)
                }
                _ => {}
            }
        }
        // A child can come before its new parent folder in a batch: retry
        // until nothing more finds its parent.
        loop {
            let mut waiting = Vec::new();
            let before = pending.len();
            for state in pending {
                match self.upsert(base, state, rules, &mut report) {
                    Upsert::Done => {}
                    Upsert::NoParent => waiting.push(state),
                    Upsert::Excluded => prune |= self.remove(base, state.frn, &mut report),
                }
            }
            pending = waiting;
            if pending.is_empty() || pending.len() == before {
                break;
            }
        }
        // Whatever still has no parent left the indexed tree.
        for state in pending {
            prune |= self.remove(base, state.frn, &mut report);
        }
        if prune {
            self.prune_orphans(base);
        }
        if report.changed {
            self.resort();
        }
        report
    }

    fn view<'a>(&'a self, base: &'a Base) -> View<'a> {
        View {
            base,
            overlay: Some(self),
        }
    }

    /// What an entry adds to the totals of the folders above it: its size
    /// (a folder's own total plus what changed below it) and one item.
    fn contribution(&self, entry: &EntryView<'_>) -> (i64, i64) {
        let (size, count) = if entry.is_dir() {
            self.folder_delta(entry.frn)
        } else {
            (0, 0)
        };
        (entry.size as i64 + size, 1 + count)
    }

    /// Adds `delta` to every live folder from `parent_frn` up to the root.
    fn adjust_folders(&mut self, base: &Base, parent_frn: u64, delta: (i64, i64)) {
        if delta == (0, 0) {
            return;
        }
        let mut chain = Vec::new();
        {
            let view = self.view(base);
            let mut current = parent_frn;
            for _ in 0..256 {
                let Some(id) = view.lookup_directory_frn(current) else {
                    break;
                };
                chain.push(current);
                let entry = view.entry(id);
                if id == 0 || entry.parent_frn == entry.frn {
                    break;
                }
                current = entry.parent_frn;
            }
        }
        for frn in chain {
            let slot = self.folder_deltas.entry(frn).or_insert((0, 0));
            slot.0 += delta.0;
            slot.1 += delta.1;
        }
    }

    /// Removes the live entry with reference `frn`. True when it was a
    /// folder that may have had contents, which must go too.
    fn remove(&mut self, base: &Base, frn: u64, report: &mut ApplyReport) -> bool {
        let found = {
            let view = self.view(base);
            view.lookup_frn(frn).map(|id| {
                let entry = view.entry(id);
                (
                    id,
                    entry.parent_frn,
                    entry.is_dir(),
                    self.contribution(&entry),
                )
            })
        };
        let Some((id, parent, is_dir, contribution)) = found else {
            return false;
        };
        self.adjust_folders(base, parent, (-contribution.0, -contribution.1));
        self.folder_deltas.remove(&frn);
        report.changed = true;
        self.applied += 1;
        if id < self.base_count {
            self.hide(id);
        } else {
            self.kill(id);
        }
        if !is_dir {
            return false;
        }
        // Its contents may sit in the base (under the same reference, even
        // when the folder itself had moved into the overlay) or be new.
        let in_base = base
            .lookup_frn(frn)
            .is_some_and(|base_id| self.base_has_children(base, base_id));
        in_base
            || self
                .added
                .iter()
                .any(|added| added.live && added.parent_frn == frn)
    }

    fn base_has_children(&mut self, base: &Base, id: u32) -> bool {
        let bits = self.has_children.get_or_insert_with(|| {
            let mut bits = vec![0u64; (base.count() as usize).div_ceil(64)];
            for child in 1..base.count() {
                let parent_frn = base.entry_summary(child).parent_frn;
                if let Some(parent) = base.lookup_frn(parent_frn) {
                    bits[parent as usize / 64] |= 1 << (parent % 64);
                }
            }
            Arc::new(bits)
        });
        bit(bits, id)
    }

    fn upsert(
        &mut self,
        base: &Base,
        state: &EntryState,
        rules: &Rules,
        report: &mut ApplyReport,
    ) -> Upsert {
        let lower = state.name.to_lowercase();
        let display: Vec<u16> = state.name.encode_utf16().collect();
        let (current, base_id, base_same, excluded) = {
            let view = self.view(base);
            let Some(parent) = view.lookup_directory_frn(state.parent_frn) else {
                return Upsert::NoParent;
            };
            if !view.entry(parent).is_dir() {
                return Upsert::NoParent;
            }
            let excluded = rules.skips_name(&lower)
                || (state.is_dir()
                    && !rules.paths.is_empty()
                    && view.resolve_path(parent).is_some_and(|path| {
                        rules.skips_path(&format!(
                            "{}{}{}",
                            path.trim_end_matches(crate::path::SEP),
                            crate::path::SEP,
                            state.name
                        ))
                    }));
            let current = view.lookup_frn(state.frn).map(|id| {
                let entry = view.entry(id);
                (
                    id,
                    entry.parent_frn,
                    entry.size,
                    entry.mtime,
                    entry.flags,
                    entry.name_display == display.as_slice(),
                    self.contribution(&entry),
                )
            });
            let base_id = base.lookup_frn(state.frn);
            (current, base_id, base_id.map(|id| base.entry(id)), excluded)
        };
        if excluded {
            return Upsert::Excluded;
        }
        // Folders keep the size the index gave them (reserved for folder
        // totals); files take the new size, or keep theirs if unknown.
        let known_size = current
            .map(|current| current.2)
            .or(base_same.map(|e| e.size));
        let size = if state.is_dir() {
            known_size.unwrap_or(0)
        } else {
            state.size.or(known_size).unwrap_or(0)
        };
        let same_as_base = base_same.is_some_and(|entry| {
            entry.parent_frn == state.parent_frn
                && entry.flags == state.flags
                && entry.mtime == state.mtime
                && entry.size == size
                && entry.name_display == display.as_slice()
        });
        if let Some((_, parent, old_size, mtime, flags, same_name, _)) = current {
            if parent == state.parent_frn
                && old_size == size
                && mtime == state.mtime
                && flags == state.flags
                && same_name
            {
                return Upsert::Done;
            }
        }
        // Take the old version out of the folder totals, put the new one in.
        if let Some((_, parent, .., contribution)) = current {
            self.adjust_folders(base, parent, (-contribution.0, -contribution.1));
        }
        let live_added = self.by_frn.get(&state.frn).copied();
        if same_as_base {
            if let Some(id) = live_added {
                self.kill(id);
            }
            if let Some(id) = base_id {
                self.unhide(id);
            }
        } else {
            if let Some(id) = base_id {
                self.hide(id);
            }
            let added = Added {
                frn: state.frn,
                parent_frn: state.parent_frn,
                size,
                mtime: state.mtime,
                flags: state.flags,
                live: true,
                name_search: Arc::from(lower.as_str()),
                name_display: Arc::from(display.as_slice()),
            };
            match live_added {
                Some(id) => self.added[(id - self.base_count) as usize] = added,
                None => {
                    let id = self.base_count + self.added.len() as u32;
                    self.added.push(added);
                    self.by_frn.insert(state.frn, id);
                }
            }
        }
        let contribution = {
            let view = self.view(base);
            view.lookup_frn(state.frn)
                .map(|id| self.contribution(&view.entry(id)))
        };
        if let Some(contribution) = contribution {
            self.adjust_folders(base, state.parent_frn, contribution);
        }
        report.changed = true;
        self.applied += 1;
        if state.is_dir() && current.is_none() && !state.created {
            report.moved_in.push(state.frn);
        }
        Upsert::Done
    }

    /// Removes every live entry that no longer reaches the root: the
    /// contents of deleted folders, which the journal does not list.
    fn prune_orphans(&mut self, base: &Base) {
        let root_frn = base.entry(0).frn;
        let mut known: HashMap<u64, bool> = HashMap::new();
        known.insert(root_frn, true);
        let mut connected = |overlay: &Overlay, frn: u64| -> bool {
            let view = overlay.view(base);
            let mut chain = Vec::new();
            let mut current = frn;
            let answer = loop {
                if let Some(&answer) = known.get(&current) {
                    break answer;
                }
                if chain.len() > 512 {
                    break false;
                }
                chain.push(current);
                let Some(id) = view.lookup_directory_frn(current) else {
                    break false;
                };
                let entry = view.entry(id);
                if id == 0 || entry.parent_frn == entry.frn {
                    break id == 0;
                }
                current = entry.parent_frn;
            };
            for frn in chain {
                known.insert(frn, answer);
            }
            answer
        };
        let mut orphans = Vec::new();
        for id in 1..self.base_count {
            if bit(&self.removed, id) {
                continue;
            }
            let parent = base.entry_summary(id).parent_frn;
            if !connected(self, parent) {
                orphans.push(id);
            }
        }
        for id in orphans {
            self.hide(id);
            self.folder_deltas.remove(&base.entry(id).frn);
        }
        let dead: Vec<u32> = self
            .order
            .iter()
            .copied()
            .filter(|&id| {
                let parent = self.added[(id - self.base_count) as usize].parent_frn;
                !connected(self, parent)
            })
            .collect();
        for id in dead {
            let frn = self.added[(id - self.base_count) as usize].frn;
            self.folder_deltas.remove(&frn);
            self.kill(id);
        }
    }

    fn resort(&mut self) {
        let base_count = self.base_count;
        let added = &self.added;
        self.order = (0..added.len() as u32)
            .filter(|&index| added[index as usize].live)
            .map(|index| base_count + index)
            .collect();
        self.order.sort_by(|&a, &b| {
            let (left, right) = (
                &added[(a - base_count) as usize],
                &added[(b - base_count) as usize],
            );
            left.name_search
                .cmp(&right.name_search)
                .then_with(|| left.name_display.cmp(&right.name_display))
                .then_with(|| a.cmp(&b))
        });
    }
}

enum Upsert {
    Done,
    NoParent,
    Excluded,
}

/// Writes `volume` (its base with its overlay applied) as a new index in
/// `out`, without holding the volume in memory: the base is already in
/// name order, so it is merged with the sorted overlay as it is written.
/// The new index records the overlay's journal cursor. Returns the number
/// of entries written.
pub fn fold(volume: &VolumeIndex, out: &Path) -> io::Result<u32> {
    fs::create_dir_all(out)?;
    let view = volume.view();
    let base = volume.base();
    let base_count = base.count();
    let added = volume.overlay_ids();

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
    let (mut search_len, mut display_len) = (0u64, 0u64);
    let mut new_base_ids = vec![u32::MAX; base_count as usize];
    let mut new_added: Vec<(u64, u32)> = Vec::with_capacity(added.len());
    let mut written = 0u32;
    let mut record = [0u8; META_SIZE];

    let mut base_ids = (1..base_count)
        .filter(|&id| !view.is_removed(id))
        .peekable();
    let mut added_ids = added.iter().copied().peekable();
    let mut next = |first: bool| -> Option<u32> {
        if first {
            return Some(0);
        }
        match (base_ids.peek(), added_ids.peek()) {
            (Some(&a), Some(&b)) => {
                if view.compare(a, b).is_le() {
                    base_ids.next()
                } else {
                    added_ids.next()
                }
            }
            (Some(_), None) => base_ids.next(),
            (None, Some(_)) => added_ids.next(),
            (None, None) => None,
        }
    };
    while let Some(id) = next(written == 0) {
        let entry = view.entry(id);
        if search_len + entry.name_search.len() as u64 + 1 > u64::from(u32::MAX)
            || display_len + entry.name_display.len() as u64 * 2 + 2 > u64::from(u32::MAX)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "names are too long for the index format",
            ));
        }
        if (written as usize).is_multiple_of(CHECK_STRIDE) {
            check_file.write_all(&(search_len as u32).to_le_bytes())?;
            check_file.write_all(&(display_len as u32).to_le_bytes())?;
        }
        record[0..8].copy_from_slice(&entry.frn.to_le_bytes());
        record[8..16].copy_from_slice(&entry.parent_frn.to_le_bytes());
        record[16..24].copy_from_slice(&entry.size.to_le_bytes());
        record[24..32].copy_from_slice(&entry.mtime.to_le_bytes());
        record[32..36].copy_from_slice(&(display_len as u32).to_le_bytes());
        record[36..40].copy_from_slice(&(search_len as u32).to_le_bytes());
        record[40..42].copy_from_slice(&(entry.name_display.len() as u16).to_le_bytes());
        record[42..44].copy_from_slice(&(entry.name_search.len() as u16).to_le_bytes());
        record[44..46].copy_from_slice(&entry.flags.to_le_bytes());
        record[46..48].copy_from_slice(&0u16.to_le_bytes());
        meta_file.write_all(&record)?;
        search_file.write_all(entry.name_search.as_bytes())?;
        search_file.write_all(&[0])?;
        search_len += entry.name_search.len() as u64 + 1;
        for unit in entry.name_display {
            display_file.write_all(&unit.to_le_bytes())?;
        }
        display_file.write_all(&[0, 0])?;
        display_len += entry.name_display.len() as u64 * 2 + 2;
        if id < base_count {
            new_base_ids[id as usize] = written;
        } else {
            new_added.push((entry.frn, written));
        }
        written += 1;
    }

    // The FRN table: the base's, already sorted, merged with the added
    // entries' references.
    new_added.sort_unstable();
    let mut extra = new_added.into_iter().peekable();
    let mut put = |frn: u64, id: u32| -> io::Result<()> {
        frn_file.write_all(&frn.to_le_bytes())?;
        frn_file.write_all(&id.to_le_bytes())
    };
    for slot in 0..base_count as usize {
        let (frn, old) = base.frn_slot(slot);
        let new_id = new_base_ids[old as usize];
        if new_id == u32::MAX {
            continue;
        }
        while let Some(&(added_frn, added_id)) = extra.peek() {
            if (added_frn, added_id) > (frn, new_id) {
                break;
            }
            put(added_frn, added_id)?;
            extra.next();
        }
        put(frn, new_id)?;
    }
    for (frn, id) in extra {
        put(frn, id)?;
    }
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

    let manifest = Manifest {
        generation,
        entry_count: written,
        meta_len: u64::from(written) * META_SIZE as u64,
        search_len,
        display_len,
        frn_len: u64::from(written) * FRN_SIZE as u64,
        check_len: (written as usize).div_ceil(CHECK_STRIDE) as u64 * 8,
        seq: 0,
    };
    fs::write(out.join("prefix.txt"), volume.prefix.as_bytes())?;
    write_manifest(out, &manifest)?;
    let cursor = volume
        .overlay()
        .map(|overlay| overlay.cursor())
        .or(volume.journal());
    if let Some(cursor) = cursor {
        write_cursor(out, cursor)?;
    }
    let check = VolumeIndex::open(out)?;
    if check.entry_count() != written {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the folded index did not reopen with the expected entry count",
        ));
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{write_tree, Entries, ScanReport, Tree};
    use crate::IndexSet;

    const ROOT: u64 = 5 | (5 << 48);
    const DOCS: u64 = 100 | (1 << 48);
    const REPORT: u64 = 101 | (1 << 48);
    const OLD_LOG: u64 = 102 | (1 << 48);
    const PHOTOS: u64 = 200 | (1 << 48);
    const CAT: u64 = 201 | (1 << 48);
    const SUB: u64 = 202 | (1 << 48);
    const DEEP: u64 = 203 | (1 << 48);
    const CURSOR: JournalCursor = JournalCursor {
        journal_id: 0xabc,
        next_usn: 1000,
    };

    fn temp_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "feather-live-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    /// A small volume written the way the NTFS reader writes one: real
    /// file references and a journal position.
    fn volume(dir: &Path) -> VolumeIndex {
        let mut entries = Entries::default();
        let mut add = |name: &str, parent: u32, frn: u64, size: u64, dir: bool| {
            entries.push(name, parent, if dir { 0 } else { size }, 7, u16::from(dir));
            entries.frn.push(frn);
        };
        add("", 0, ROOT, 0, true);
        add("docs", 0, DOCS, 0, true);
        add("Report.txt", 1, REPORT, 10, false);
        add("old.log", 1, OLD_LOG, 20, false);
        add("photos", 0, PHOTOS, 0, true);
        add("cat.jpg", 4, CAT, 30, false);
        add("sub", 4, SUB, 0, true);
        add("deep.png", 6, DEEP, 40, false);
        let tree = Tree {
            root_display: "C:\\Feather".into(),
            entries,
            report: ScanReport::default(),
            journal: Some(CURSOR),
        };
        write_tree(dir, tree, std::time::Instant::now()).expect("write");
        VolumeIndex::open(dir).expect("open")
    }

    fn file(frn: u64, parent: u64, name: &str, size: u64) -> Change {
        Change::Upsert(EntryState {
            frn,
            parent_frn: parent,
            name: name.into(),
            size: Some(size),
            mtime: 7,
            flags: 0,
            created: true,
        })
    }

    fn folder(frn: u64, parent: u64, name: &str, created: bool) -> Change {
        Change::Upsert(EntryState {
            frn,
            parent_frn: parent,
            name: name.into(),
            size: Some(0),
            mtime: 7,
            flags: 1,
            created,
        })
    }

    /// Every live entry as (path, size), sorted.
    fn listing(volume: &VolumeIndex) -> Vec<(String, u64)> {
        let mut all: Vec<(String, u64)> = volume
            .iter_ids()
            .map(|id| {
                (
                    volume.resolve_path(id).expect("path"),
                    volume.entry(id).size,
                )
            })
            .collect();
        all.sort();
        all
    }

    fn with(base: &VolumeIndex, overlay: &Overlay) -> VolumeIndex {
        base.with_overlay(Some(Arc::new(overlay.clone())))
    }

    fn rules() -> Rules {
        Rules::new(&[], &[])
    }

    #[test]
    fn the_writer_keeps_real_references_and_the_journal_position() {
        let dir = temp_dir("refs");
        let base = volume(&dir);
        assert_eq!(base.journal(), Some(CURSOR));
        assert_eq!(base.entry(0).frn, ROOT);
        assert_eq!(base.entry(0).parent_frn, ROOT);
        let deep = base.lookup_frn(DEEP).expect("deep");
        assert_eq!(base.entry(deep).parent_frn, SUB);
        assert_eq!(
            base.resolve_path(deep).as_deref(),
            Some("C:\\Feather\\photos\\sub\\deep.png")
        );
        assert_eq!(
            base.find_folder("C:\\Feather\\photos\\sub"),
            base.lookup_frn(SUB)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_files_show_up_with_their_paths() {
        let dir = temp_dir("create");
        let base = volume(&dir);
        let mut overlay = Overlay::new(&base, CURSOR);
        let report = overlay.apply(&base, &[file(300, DOCS, "Notes.md", 5)], &rules());
        assert!(report.changed);
        let live = with(&base, &overlay);
        let id = live.lookup_frn(300).expect("added");
        assert!(id >= live.base_count());
        assert_eq!(live.entry(id).name_search, "notes.md");
        assert_eq!(
            live.resolve_path(id).as_deref(),
            Some("C:\\Feather\\docs\\Notes.md")
        );
        assert_eq!(live.item_count(), base.item_count() + 1);
        assert!(live.iter_ids().any(|found| found == id));
        // The base alone does not see it.
        assert!(base.lookup_frn(300).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn renames_and_deletes_replace_base_entries() {
        let dir = temp_dir("rename");
        let base = volume(&dir);
        let old_report = base.lookup_frn(REPORT).unwrap();
        let mut overlay = Overlay::new(&base, CURSOR);
        overlay.apply(
            &base,
            &[
                file(REPORT, PHOTOS, "final.txt", 11),
                Change::Remove(OLD_LOG),
            ],
            &rules(),
        );
        let live = with(&base, &overlay);
        assert!(live.is_removed(old_report));
        assert!(live.is_removed(base.lookup_frn(OLD_LOG).unwrap()));
        let moved = live.lookup_frn(REPORT).unwrap();
        assert_ne!(moved, old_report);
        assert_eq!(
            live.resolve_path(moved).as_deref(),
            Some("C:\\Feather\\photos\\final.txt")
        );
        assert_eq!(live.entry(moved).size, 11);
        assert!(live.lookup_frn(OLD_LOG).is_none());
        assert_eq!(live.item_count(), base.item_count() - 1);
        assert_eq!(overlay.removed_count(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_a_folder_removes_everything_in_it() {
        let dir = temp_dir("delete-folder");
        let base = volume(&dir);
        let mut overlay = Overlay::new(&base, CURSOR);
        // A file added inside first, then the whole folder goes.
        overlay.apply(&base, &[file(400, SUB, "new.png", 1)], &rules());
        overlay.apply(&base, &[Change::Remove(PHOTOS)], &rules());
        let live = with(&base, &overlay);
        let names: Vec<String> = listing(&live).into_iter().map(|(path, _)| path).collect();
        assert_eq!(
            names,
            vec![
                "C:\\Feather\\docs".to_string(),
                "C:\\Feather\\docs\\Report.txt".to_string(),
                "C:\\Feather\\docs\\old.log".to_string(),
            ]
        );
        assert_eq!(live.item_count(), 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn renaming_a_folder_moves_its_contents_along() {
        let dir = temp_dir("rename-folder");
        let base = volume(&dir);
        let mut overlay = Overlay::new(&base, CURSOR);
        let report = overlay.apply(&base, &[folder(PHOTOS, DOCS, "Pictures", false)], &rules());
        // Known before, so nothing has to be listed from the disk.
        assert!(report.moved_in.is_empty());
        let live = with(&base, &overlay);
        let deep = live.lookup_frn(DEEP).unwrap();
        assert!(deep < live.base_count(), "the child itself did not change");
        assert_eq!(
            live.resolve_path(deep).as_deref(),
            Some("C:\\Feather\\docs\\Pictures\\sub\\deep.png")
        );
        assert_eq!(
            live.find_folder("C:\\Feather\\docs\\pictures\\sub"),
            live.lookup_frn(SUB)
        );
        assert!(live.find_folder("C:\\Feather\\photos").is_none());
        // Then the renamed folder is deleted: its base contents go too.
        overlay.apply(&base, &[Change::Remove(PHOTOS)], &rules());
        let live = with(&base, &overlay);
        assert!(live.lookup_frn(DEEP).is_none());
        assert!(live.lookup_frn(CAT).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn children_may_come_before_their_new_folder() {
        let dir = temp_dir("order");
        let base = volume(&dir);
        let mut overlay = Overlay::new(&base, CURSOR);
        let report = overlay.apply(
            &base,
            &[
                file(501, 500, "inside.txt", 3),
                folder(500, DOCS, "new folder", true),
            ],
            &rules(),
        );
        assert!(report.moved_in.is_empty(), "a created folder starts empty");
        let live = with(&base, &overlay);
        let inside = live.lookup_frn(501).unwrap();
        assert_eq!(
            live.resolve_path(inside).as_deref(),
            Some("C:\\Feather\\docs\\new folder\\inside.txt")
        );
        assert_eq!(
            live.find_folder("C:\\Feather\\docs\\New Folder"),
            live.lookup_frn(500)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaving_the_indexed_tree_is_a_delete() {
        let dir = temp_dir("leave");
        let base = volume(&dir);
        let mut overlay = Overlay::new(&base, CURSOR);
        // Moved into a folder the index does not have (the recycle bin).
        overlay.apply(&base, &[folder(PHOTOS, 999, "$R0001", false)], &rules());
        let live = with(&base, &overlay);
        assert!(live.lookup_frn(PHOTOS).is_none());
        assert!(live.lookup_frn(DEEP).is_none());
        assert_eq!(live.item_count(), 3);
        // Restored: back where it was, as the base had it. The folder is
        // reported so its contents can be listed again.
        let report = overlay.apply(&base, &[folder(PHOTOS, ROOT, "photos", false)], &rules());
        assert_eq!(report.moved_in, vec![PHOTOS]);
        overlay.apply(
            &base,
            &[
                file(CAT, PHOTOS, "cat.jpg", 30),
                folder(SUB, PHOTOS, "sub", true),
                file(DEEP, SUB, "deep.png", 40),
            ],
            &rules(),
        );
        let live = with(&base, &overlay);
        assert_eq!(listing(&live), listing(&base));
        assert!(overlay.is_empty(), "everything matches the base again");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn replays_and_unchanged_entries_cost_nothing() {
        let dir = temp_dir("replay");
        let base = volume(&dir);
        let mut overlay = Overlay::new(&base, CURSOR);
        // What the base already says.
        let report = overlay.apply(&base, &[file(REPORT, DOCS, "Report.txt", 10)], &rules());
        assert!(!report.changed);
        assert_eq!(overlay.size(), 0);
        let batch = [
            file(300, DOCS, "a.txt", 1),
            file(OLD_LOG, DOCS, "old.log", 99),
        ];
        assert!(overlay.apply(&base, &batch, &rules()).changed);
        let size = overlay.size();
        assert!(!overlay.apply(&base, &batch, &rules()).changed);
        assert_eq!(overlay.size(), size);
        // Back to the base's values: the overlay copy goes away.
        overlay.apply(&base, &[file(OLD_LOG, DOCS, "old.log", 20)], &rules());
        let live = with(&base, &overlay);
        let id = live.lookup_frn(OLD_LOG).unwrap();
        assert!(id < live.base_count());
        assert_eq!(overlay.removed_count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_sizes_keep_the_old_one_and_the_root_is_left_alone() {
        let dir = temp_dir("unknown");
        let base = volume(&dir);
        let mut overlay = Overlay::new(&base, CURSOR);
        let mut state = match file(CAT, PHOTOS, "kitten.jpg", 0) {
            Change::Upsert(state) => state,
            _ => unreachable!(),
        };
        state.size = None;
        overlay.apply(
            &base,
            &[
                Change::Upsert(state),
                folder(ROOT, ROOT, "", false),
                Change::Remove(ROOT),
            ],
            &rules(),
        );
        let live = with(&base, &overlay);
        let cat = live.lookup_frn(CAT).unwrap();
        assert_eq!(live.entry(cat).name_search, "kitten.jpg");
        assert_eq!(live.entry(cat).size, 30);
        assert!(!live.is_removed(0));
        assert_eq!(live.item_count(), base.item_count());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn excluded_names_and_paths_stay_out() {
        let dir = temp_dir("rules");
        let base = volume(&dir);
        let rules = Rules::new(
            &["node_modules".into()],
            &["C:\\Feather\\docs\\private".into()],
        );
        let mut overlay = Overlay::new(&base, CURSOR);
        overlay.apply(
            &base,
            &[
                folder(600, ROOT, "$RECYCLE.BIN", true),
                folder(601, DOCS, "node_modules", true),
                folder(602, DOCS, "Private", true),
                file(603, 602, "secret.txt", 1),
                file(604, DOCS, "kept.txt", 1),
            ],
            &rules,
        );
        let live = with(&base, &overlay);
        for frn in [600, 601, 602, 603] {
            assert!(live.lookup_frn(frn).is_none(), "{frn} should be left out");
        }
        assert!(live.lookup_frn(604).is_some());
        // Renaming an indexed file into an excluded name removes it.
        overlay.apply(&base, &[file(REPORT, DOCS, "node_modules", 10)], &rules);
        assert!(with(&base, &overlay).lookup_frn(REPORT).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn runs_list_live_entries_in_name_order() {
        let dir = temp_dir("runs");
        let base = volume(&dir);
        let mut overlay = Overlay::new(&base, CURSOR);
        overlay.apply(
            &base,
            &[
                file(700, DOCS, "aaa.txt", 1),
                file(701, DOCS, "Cat.jpg", 1),
                file(702, DOCS, "zzz.txt", 1),
                file(703, DOCS, "m.txt", 1),
                Change::Remove(OLD_LOG),
                file(REPORT, DOCS, "b report.txt", 10),
            ],
            &rules(),
        );
        let live = with(&base, &overlay);
        let from_runs: Vec<u32> = live
            .live_runs()
            .into_iter()
            .flat_map(|(first, count)| first..first + count)
            .collect();
        let mut expected: Vec<u32> = live.iter_ids().collect();
        expected.sort_by(|&a, &b| live.compare_names(a, b));
        assert_eq!(from_runs, expected);
        let names: Vec<&str> = from_runs
            .iter()
            .map(|&id| live.entry(id).name_search)
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        assert_eq!(names.first(), Some(&"aaa.txt"));
        assert_eq!(names.last(), Some(&"zzz.txt"));
        // Without changes it is one run.
        assert_eq!(base.live_runs(), vec![(1, base.base_count() - 1)]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn folder_deltas_track_sizes_and_counts_below_each_folder() {
        let dir = temp_dir("deltas");
        let base = volume(&dir);
        let mut overlay = Overlay::new(&base, CURSOR);
        overlay.apply(&base, &[file(800, SUB, "big.bin", 1000)], &rules());
        assert_eq!(overlay.folder_delta(SUB), (1000, 1));
        assert_eq!(overlay.folder_delta(PHOTOS), (1000, 1));
        assert_eq!(overlay.folder_delta(ROOT), (1000, 1));
        assert_eq!(overlay.folder_delta(DOCS), (0, 0));
        // Grows, then moves to docs.
        overlay.apply(&base, &[file(800, SUB, "big.bin", 1500)], &rules());
        assert_eq!(overlay.folder_delta(PHOTOS), (1500, 1));
        overlay.apply(&base, &[file(800, DOCS, "big.bin", 1500)], &rules());
        assert_eq!(overlay.folder_delta(PHOTOS), (0, 0));
        assert_eq!(overlay.folder_delta(DOCS), (1500, 1));
        assert_eq!(overlay.folder_delta(ROOT), (1500, 1));
        // A base file goes.
        overlay.apply(&base, &[Change::Remove(REPORT)], &rules());
        assert_eq!(overlay.folder_delta(DOCS), (1490, 0));
        // A folder goes with everything that changed inside it: the root
        // loses the folder (its stored size, 0 here, plus 1490) and one
        // item more than it gained.
        overlay.apply(&base, &[Change::Remove(DOCS)], &rules());
        assert_eq!(overlay.folder_delta(ROOT), (0, -1));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn folding_writes_what_searches_saw() {
        let dir = temp_dir("fold");
        let base = volume(&dir);
        let mut overlay = Overlay::new(&base, CURSOR);
        overlay.apply(
            &base,
            &[
                folder(900, DOCS, "Älter", true),
                file(901, 900, "zeta.txt", 5),
                file(902, ROOT, "alpha.txt", 6),
                file(CAT, DOCS, "cat.jpg", 31),
                Change::Remove(SUB),
            ],
            &rules(),
        );
        overlay.set_cursor(JournalCursor {
            journal_id: 0xabc,
            next_usn: 5000,
        });
        let live = with(&base, &overlay);
        let out = temp_dir("fold-out");
        let written = fold(&live, &out).expect("fold");
        let folded = VolumeIndex::open(&out).expect("open folded");
        assert_eq!(written, folded.entry_count());
        assert_eq!(listing(&folded), listing(&live));
        assert_eq!(folded.item_count(), live.item_count());
        assert_eq!(folded.journal().map(|cursor| cursor.next_usn), Some(5000));
        // Sorted, so folder lookups by binary search still work.
        let names: Vec<&str> = folded
            .iter_ids()
            .map(|id| folded.entry(id).name_search)
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        assert_eq!(
            folded.find_folder("C:\\Feather\\docs\\älter"),
            folded.lookup_frn(900)
        );
        // And it opens as a set like any index.
        assert_eq!(
            IndexSet::open(&out).expect("set").item_count(),
            live.item_count()
        );
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn sets_share_their_base_with_overlays() {
        let dir = temp_dir("set");
        volume(&dir);
        let set = IndexSet::open(&dir).expect("set");
        let mut overlay = Overlay::new(&set.volumes()[0], CURSOR);
        overlay.apply(&set.volumes()[0], &[file(300, DOCS, "x.txt", 1)], &rules());
        let live = set.with_overlays(&[Some(Arc::new(overlay))]);
        assert_eq!(live.item_count(), set.item_count() + 1);
        let id = live.volumes()[0].lookup_frn(300).unwrap();
        let reference = crate::EntryRef::new(0, id);
        assert_eq!(
            live.resolve_path(reference).as_deref(),
            Some("C:\\Feather\\docs\\x.txt")
        );
        assert_eq!(live.entry(reference).name_search, "x.txt");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn batches_and_cursors_survive_their_formats() {
        let mut unknown = match folder(9, 8, "d", false) {
            Change::Upsert(state) => state,
            _ => unreachable!(),
        };
        unknown.size = None;
        let batch = Batch {
            cursor: CURSOR,
            changes: vec![
                file(1 << 50 | 77, DOCS, "naïve.txt", 3),
                Change::Remove(42),
                Change::Upsert(unknown),
            ],
            more: true,
        };
        let mut bytes = Vec::new();
        batch.encode(&mut bytes).unwrap();
        assert_eq!(Batch::decode(&mut bytes.as_slice()).unwrap(), batch);
        assert!(Batch::decode(&mut &b"FUS2"[..]).is_err());
        assert!(Batch::decode(&mut &bytes[..bytes.len() - 1]).is_err());

        let dir = temp_dir("cursor");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(read_cursor(&dir), None);
        write_cursor(&dir, CURSOR).unwrap();
        assert_eq!(read_cursor(&dir), Some(CURSOR));
        let mut damaged = fs::read(dir.join(CURSOR_FILE)).unwrap();
        damaged[10] ^= 1;
        fs::write(dir.join(CURSOR_FILE), damaged).unwrap();
        assert_eq!(read_cursor(&dir), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
