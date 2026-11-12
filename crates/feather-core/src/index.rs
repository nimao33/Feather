use std::cmp::Ordering;
use std::io;
use std::sync::Arc;

use crate::entry::{EntrySummary, EntryView};
use crate::format::{Manifest, CHECK_STRIDE, COUNT_SIZE, FRN_SIZE, META_SIZE};
use crate::live::{JournalCursor, Overlay};
use crate::mapped::MappedFile;

/// The mapped, immutable files of one index generation. Shared by every
/// [`VolumeIndex`] snapshot built on it, so a live update never copies or
/// re-maps them.
pub(crate) struct Base {
    pub(crate) manifest: Manifest,
    pub(crate) prefix: String,
    /// Where the change journal stood when this generation was read, if
    /// the volume is tracked through one.
    pub(crate) journal: Option<JournalCursor>,
    meta: MappedFile,
    search: MappedFile,
    display: MappedFile,
    frn: MappedFile,
    check: MappedFile,
    /// Items below each folder, when the index has folder totals.
    counts: Option<MappedFile>,
}

/// One volume: its mapped base index and, optionally, the in-memory
/// overlay of changes made since the base was written.
///
/// Entry ids below [`VolumeIndex::base_count`] address the base; ids from
/// there on address entries the overlay added. A base entry the overlay
/// deleted or replaced keeps its id, so rows already shown still render,
/// but it is no longer listed by [`VolumeIndex::iter_ids`] or found by any
/// lookup.
pub struct VolumeIndex {
    pub manifest: Manifest,
    pub prefix: String,
    base: Arc<Base>,
    overlay: Option<Arc<Overlay>>,
    path_cache: std::sync::Mutex<std::collections::HashMap<u32, String>>,
}

/// A read-only view of a base plus an overlay. Every lookup that must see
/// live changes goes through this, both for searches and while the
/// overlay itself is being updated.
#[derive(Clone, Copy)]
pub(crate) struct View<'a> {
    pub(crate) base: &'a Base,
    pub(crate) overlay: Option<&'a Overlay>,
}

impl Base {
    fn open(dir: &std::path::Path) -> io::Result<Self> {
        let manifest = crate::format::read_manifest(dir)?;
        let meta = MappedFile::open(&dir.join(manifest.meta_name()))?;
        let search = MappedFile::open(&dir.join(manifest.search_name()))?;
        let display = MappedFile::open(&dir.join(manifest.display_name()))?;
        let frn = MappedFile::open(&dir.join(manifest.frn_name()))?;
        let check = MappedFile::open(&dir.join(manifest.check_name()))?;
        let counts = if manifest.has_folder_totals() {
            Some(MappedFile::open(&dir.join(manifest.count_name()))?)
        } else {
            None
        };

        let base = Self {
            manifest,
            prefix: std::fs::read_to_string(dir.join("prefix.txt"))
                .unwrap_or_else(|_| crate::path::default_prefix().to_string())
                .trim()
                .to_string(),
            journal: crate::live::read_cursor(dir),
            meta,
            search,
            display,
            frn,
            check,
            counts,
        };
        base.validate()?;
        Ok(base)
    }

    fn validate(&self) -> io::Result<()> {
        let count = self.manifest.entry_count as usize;
        if self.meta.byte_len() != count * META_SIZE
            || self.frn.byte_len() != count * FRN_SIZE
            || self.check.byte_len() != self.manifest.check_count() * 8
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "index table length mismatch",
            ));
        }
        if self
            .counts
            .as_ref()
            .is_some_and(|counts| counts.byte_len() != count * COUNT_SIZE)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "folder count table length mismatch",
            ));
        }
        if self.search.byte_len() as u64 != self.manifest.search_len
            || self.display.byte_len() as u64 != self.manifest.display_len
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "index blob length mismatch",
            ));
        }

        // Check every name once here so the hot read path can borrow it as a
        // `str` without re-validating UTF-8 on every search.
        let meta_data = self.meta.as_slice();
        let search_data = self.search.as_slice();
        let display_data = self.display.as_slice();
        for id in 0..count {
            let base = id * META_SIZE;
            let display_offset = read_u32(meta_data, base + 32) as usize;
            let search_offset = read_u32(meta_data, base + 36) as usize;
            let display_len = read_u16(meta_data, base + 40) as usize;
            let search_len = read_u16(meta_data, base + 42) as usize;
            let search_end = search_offset + search_len;
            let display_end = display_offset + display_len * 2;
            if search_end > search_data.len() || display_end > display_data.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "index name offset out of bounds",
                ));
            }
            if std::str::from_utf8(&search_data[search_offset..search_end]).is_err() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "index name is not UTF-8",
                ));
            }
        }

        // The meta table is in name order, but the FRN table is deliberately
        // sorted by file reference number so paths can resolve parents by
        // binary search. Validate that table independently: it must be sorted,
        // point at real entries, agree with each entry's FRN, and cover every
        // entry exactly once.
        let frn_data = self.frn.as_slice();
        let mut seen = vec![0u64; count.div_ceil(64)];
        let mut previous_frn = 0u64;
        for slot in 0..count {
            let base = slot * FRN_SIZE;
            let frn = read_u64(frn_data, base);
            let id = read_u32(frn_data, base + 8) as usize;
            if slot > 0 && frn < previous_frn {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "FRN index is not sorted",
                ));
            }
            if id >= count {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "FRN index entry points outside the entry table",
                ));
            }
            if read_u64(meta_data, id * META_SIZE) != frn {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "FRN index entry does not match the entry table",
                ));
            }
            let bit = 1u64 << (id % 64);
            if seen[id / 64] & bit != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "FRN index contains a duplicate entry id",
                ));
            }
            seen[id / 64] |= bit;
            previous_frn = frn;
        }
        Ok(())
    }

    pub(crate) fn count(&self) -> u32 {
        self.manifest.entry_count
    }

    /// True when folder `size` fields hold the total of everything below
    /// them. Indexes written before folder totals existed say false: their
    /// folders have size 0 until the next rebuild.
    pub(crate) fn has_folder_totals(&self) -> bool {
        self.counts.is_some()
    }

    /// The item count the base stored for entry `id`: the number of files
    /// and folders below it for a folder, 0 for a file or an index without
    /// folder totals.
    pub(crate) fn items(&self, id: u32) -> u32 {
        match &self.counts {
            Some(counts) if id < self.manifest.entry_count => {
                read_u32(counts.as_slice(), id as usize * COUNT_SIZE)
            }
            _ => 0,
        }
    }

    /// The modification time stored for base entry `id`.
    #[inline]
    fn entry_mtime(&self, id: u32) -> u64 {
        let id = id.min(self.manifest.entry_count.saturating_sub(1));
        read_u64(self.meta.as_slice(), id as usize * META_SIZE + 24)
    }

    pub(crate) fn entry(&self, id: u32) -> EntryView<'_> {
        let id = id.min(self.manifest.entry_count.saturating_sub(1));
        let base = id as usize * META_SIZE;
        let meta = self.meta.as_slice();
        let frn = read_u64(meta, base);
        let parent_frn = read_u64(meta, base + 8);
        let size = read_u64(meta, base + 16);
        let mtime = read_u64(meta, base + 24);
        let display_offset = read_u32(meta, base + 32);
        let search_offset = read_u32(meta, base + 36);
        let display_len = read_u16(meta, base + 40);
        let search_len = read_u16(meta, base + 42);
        let flags = read_u16(meta, base + 44);

        let search_bytes = self
            .search
            .as_slice()
            .get(search_offset as usize..search_offset as usize + search_len as usize)
            .unwrap_or(&[]);
        // `open` validated every name as UTF-8, so this does not re-scan it.
        let name_search = unsafe { std::str::from_utf8_unchecked(search_bytes) };
        let display_bytes = self
            .display
            .as_slice()
            .get(display_offset as usize..display_offset as usize + display_len as usize * 2)
            .unwrap_or(&[]);
        let name_display: &[u16] = if display_bytes.len() >= 2 {
            unsafe {
                std::slice::from_raw_parts(
                    display_bytes.as_ptr() as *const u16,
                    display_bytes.len() / 2,
                )
            }
        } else {
            &[]
        };

        EntryView {
            id,
            frn,
            parent_frn,
            size,
            mtime,
            flags,
            search_offset,
            search_len,
            display_offset,
            display_len,
            name_search,
            name_display,
        }
    }

    pub(crate) fn entry_summary(&self, id: u32) -> EntrySummary<'_> {
        let id = id.min(self.manifest.entry_count.saturating_sub(1));
        let base = id as usize * META_SIZE;
        let meta = self.meta.as_slice();
        let parent_frn = read_u64(meta, base + 8);
        let size = read_u64(meta, base + 16);
        let flags = read_u16(meta, base + 44);
        let search_offset = read_u32(meta, base + 36) as usize;
        let search_len = read_u16(meta, base + 42) as usize;
        let search_bytes = self
            .search
            .as_slice()
            .get(search_offset..search_offset + search_len)
            .unwrap_or(&[]);
        // Validated once by `open`.
        let name_search = unsafe { std::str::from_utf8_unchecked(search_bytes) };
        EntrySummary {
            name_search,
            parent_frn,
            size,
            flags,
        }
    }

    /// Slot `slot` of the FRN table: (reference, entry id), by reference.
    pub(crate) fn frn_slot(&self, slot: usize) -> (u64, u32) {
        let data = self.frn.as_slice();
        (
            read_u64(data, slot * FRN_SIZE),
            read_u32(data, slot * FRN_SIZE + 8),
        )
    }

    /// The first FRN table slot whose reference is not below `frn`.
    fn frn_lower_bound(&self, frn: u64) -> usize {
        let count = self.manifest.entry_count as usize;
        let data = self.frn.as_slice();
        let mut low = 0usize;
        let mut high = count;
        while low < high {
            let mid = (low + high) / 2;
            if read_u64(data, mid * FRN_SIZE) < frn {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low
    }

    pub(crate) fn lookup_frn(&self, frn: u64) -> Option<u32> {
        let low = self.frn_lower_bound(frn);
        (low < self.manifest.entry_count as usize && self.frn_slot(low).0 == frn)
            .then(|| self.frn_slot(low).1)
    }

    /// Like [`Base::lookup_frn`], preferring a folder when an older index
    /// holds the same reference more than once. `usable` filters ids out.
    fn lookup_directory_frn(&self, frn: u64, usable: impl Fn(u32) -> bool) -> Option<u32> {
        let count = self.manifest.entry_count as usize;
        let mut slot = self.frn_lower_bound(frn);
        let mut first = None;
        while slot < count {
            let (found, id) = self.frn_slot(slot);
            if found != frn {
                break;
            }
            if usable(id) {
                if self.entry(id).is_dir() {
                    return Some(id);
                }
                first.get_or_insert(id);
            }
            slot += 1;
        }
        first
    }

    /// The first id (from 1) whose search name is not below `name`.
    fn name_lower_bound(&self, name: &str) -> u32 {
        let count = self.manifest.entry_count;
        // Entry 0 is the root; the rest are sorted by search name.
        let (mut low, mut high) = (1u32, count.max(1));
        while low < high {
            let mid = low + (high - low) / 2;
            if self.entry(mid).name_search < name {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low
    }

    /// Where an entry named (`search`, `display`) would go among ids 1..:
    /// after every base entry that sorts the same or before it.
    pub(crate) fn insertion_point(&self, search: &str, display: &[u16]) -> u32 {
        let count = self.manifest.entry_count;
        let (mut low, mut high) = (1u32, count.max(1));
        while low < high {
            let mid = low + (high - low) / 2;
            let entry = self.entry(mid);
            let after = entry
                .name_search
                .cmp(search)
                .then_with(|| entry.name_display.cmp(display))
                == Ordering::Greater;
            if after {
                high = mid;
            } else {
                low = mid + 1;
            }
        }
        low
    }
}

impl<'a> View<'a> {
    fn base_count(&self) -> u32 {
        self.base.count()
    }

    /// True for an id that no longer names a live entry: a base entry the
    /// overlay deleted or replaced, or an overlay entry deleted since.
    pub(crate) fn is_removed(&self, id: u32) -> bool {
        match self.overlay {
            None => id >= self.base_count(),
            Some(overlay) => overlay.is_removed(id),
        }
    }

    /// An overlay entry, or a base entry for a base id; `None` for an id
    /// past both.
    pub(crate) fn entry_or_none(&self, id: u32) -> Option<EntryView<'a>> {
        if id < self.base_count() {
            Some(self.base.entry(id))
        } else {
            self.overlay.and_then(|overlay| overlay.entry(id))
        }
    }

    pub(crate) fn entry(&self, id: u32) -> EntryView<'a> {
        if id >= self.base_count() {
            if let Some(view) = self.overlay.and_then(|overlay| overlay.entry(id)) {
                return view;
            }
        }
        self.base.entry(id)
    }

    pub(crate) fn entry_summary(&self, id: u32) -> EntrySummary<'a> {
        if id >= self.base_count() {
            if let Some(view) = self.overlay.and_then(|overlay| overlay.entry(id)) {
                return EntrySummary {
                    name_search: view.name_search,
                    parent_frn: view.parent_frn,
                    size: view.size,
                    flags: view.flags,
                };
            }
        }
        self.base.entry_summary(id)
    }

    /// The modification time of entry `id`, from the overlay for an entry
    /// it added, without reading the rest of a base entry.
    pub(crate) fn entry_mtime(&self, id: u32) -> u64 {
        if id >= self.base_count() {
            if let Some(view) = self.overlay.and_then(|overlay| overlay.entry(id)) {
                return view.mtime;
            }
        }
        self.base.entry_mtime(id)
    }

    /// The item count stored with entry `id` when it was indexed or last
    /// changed: the base's count table for a base entry, the count an
    /// overlay entry carried over from the base. Changes below the folder
    /// since then are in [`Overlay::folder_delta`].
    pub(crate) fn stored_items(&self, id: u32) -> u32 {
        if id >= self.base_count() {
            if let Some(items) = self.overlay.and_then(|overlay| overlay.stored_items(id)) {
                return items;
            }
        }
        self.base.items(id)
    }

    /// Folder `id`'s live total size and item count: what was stored for it
    /// plus what changed below it since. `None` for a file or an index
    /// without folder totals.
    pub(crate) fn folder_totals(&self, id: u32) -> Option<(u64, u32)> {
        if !self.base.has_folder_totals() {
            return None;
        }
        let entry = self.entry_summary(id);
        if !entry.is_dir() {
            return None;
        }
        let (size, items) = (entry.size, self.stored_items(id));
        let Some(overlay) = self.overlay else {
            return Some((size, items));
        };
        let (size_delta, items_delta) = overlay.folder_delta(self.entry(id).frn);
        Some((
            size.saturating_add_signed(size_delta),
            (i64::from(items) + items_delta).clamp(0, i64::from(u32::MAX)) as u32,
        ))
    }

    /// The live entry with this reference.
    pub(crate) fn lookup_frn(&self, frn: u64) -> Option<u32> {
        match self.overlay {
            None => self.base.lookup_frn(frn),
            Some(overlay) => overlay.live_id(frn).or_else(|| {
                self.base
                    .lookup_frn(frn)
                    .filter(|&id| !overlay.is_removed(id))
            }),
        }
    }

    pub(crate) fn lookup_directory_frn(&self, frn: u64) -> Option<u32> {
        match self.overlay {
            None => self.base.lookup_directory_frn(frn, |_| true),
            Some(overlay) => overlay.live_id(frn).or_else(|| {
                self.base
                    .lookup_directory_frn(frn, |id| !overlay.is_removed(id))
            }),
        }
    }

    pub(crate) fn find_child_folder(&self, parent_frn: u64, name: &str) -> Option<u32> {
        if let Some(overlay) = self.overlay {
            if let Some(id) = overlay.find_child(parent_frn, name, true) {
                return Some(id);
            }
        }
        let count = self.base_count();
        let mut id = self.base.name_lower_bound(name);
        while id < count {
            let entry = self.base.entry(id);
            if entry.name_search != name {
                break;
            }
            if entry.parent_frn == parent_frn && entry.is_dir() && !self.is_removed(id) {
                return Some(id);
            }
            id += 1;
        }
        // An index whose names are not sorted (older synthetic data).
        (1..count).find(|&id| {
            let entry = self.base.entry(id);
            entry.parent_frn == parent_frn
                && entry.is_dir()
                && entry.name_search == name
                && !self.is_removed(id)
        })
    }

    pub(crate) fn find_folder(&self, path: &str) -> Option<u32> {
        let path = crate::path::normalize_path(path);
        let prefix = crate::path::normalize_path(&self.base.prefix);
        if path == prefix {
            return Some(0);
        }
        let rest = path.strip_prefix(&prefix)?;
        let rest = rest.strip_prefix(crate::path::SEP).unwrap_or(rest);
        let mut current = self.base.entry(0).frn;
        let mut found = 0;
        for part in rest.split(crate::path::SEP).filter(|part| !part.is_empty()) {
            found = self.find_child_folder(current, part)?;
            current = self.entry(found).frn;
        }
        Some(found)
    }

    pub(crate) fn resolve_path(&self, id: u32) -> Option<String> {
        let mut parts = Vec::new();
        let mut current = id;
        let mut found_root = false;
        for _ in 0..128 {
            let entry = self.entry(current);
            let root = current == 0 || entry.parent_frn == entry.frn;
            if !entry.name_display.is_empty() && current != 0 {
                parts.push(String::from_utf16_lossy(entry.name_display));
            }
            if root {
                found_root = true;
                break;
            }
            current = self.lookup_directory_frn(entry.parent_frn)?;
        }
        if !found_root {
            return None;
        }

        parts.reverse();
        let prefix = self.base.prefix.trim_end_matches(crate::path::SEP);
        let mut path = if prefix.is_empty() {
            crate::path::SEP.to_string()
        } else {
            format!("{prefix}{}", crate::path::SEP)
        };
        for (index, part) in parts.iter().enumerate() {
            if index > 0 {
                path.push(crate::path::SEP);
            }
            path.push_str(part);
        }
        Some(path)
    }

    /// Orders two ids the way the index is sorted: by search name, then
    /// display name; a base entry before an overlay entry that sorts the
    /// same; then by id.
    pub(crate) fn compare(&self, a: u32, b: u32) -> Ordering {
        let (left, right) = (self.entry(a), self.entry(b));
        left.name_search
            .cmp(right.name_search)
            .then_with(|| left.name_display.cmp(right.name_display))
            .then_with(|| a.cmp(&b))
    }
}

impl VolumeIndex {
    pub fn open(dir: &std::path::Path) -> io::Result<Self> {
        let base = Arc::new(Base::open(dir)?);
        Ok(Self::from_base(base, None))
    }

    fn from_base(base: Arc<Base>, overlay: Option<Arc<Overlay>>) -> Self {
        Self {
            manifest: base.manifest.clone(),
            prefix: base.prefix.clone(),
            base,
            overlay,
            path_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// The same mapped base with another overlay (or none). Cheap: nothing
    /// is copied but the small manifest and prefix.
    pub fn with_overlay(&self, overlay: Option<Arc<Overlay>>) -> Self {
        Self::from_base(Arc::clone(&self.base), overlay)
    }

    pub(crate) fn view(&self) -> View<'_> {
        View {
            base: &self.base,
            overlay: self.overlay.as_deref(),
        }
    }

    pub(crate) fn base(&self) -> &Base {
        &self.base
    }

    /// True when `other` holds the same base generation: the same mapped
    /// files, or a copy of them carried into a newer library. An overlay
    /// made for one then fits the other, since its ids depend only on the
    /// base. Bases written by different reads of a drive differ at least
    /// in where the change journal stood.
    pub fn same_base(&self, other: &VolumeIndex) -> bool {
        if Arc::ptr_eq(&self.base, &other.base) {
            return true;
        }
        let (a, b) = (&self.base.manifest, &other.base.manifest);
        self.base.journal.is_some()
            && self.base.journal == other.base.journal
            && self.base.prefix == other.base.prefix
            && (
                a.generation,
                a.entry_count,
                a.meta_len,
                a.search_len,
                a.display_len,
                a.frn_len,
                a.seq,
                a.features,
            ) == (
                b.generation,
                b.entry_count,
                b.meta_len,
                b.search_len,
                b.display_len,
                b.frn_len,
                b.seq,
                b.features,
            )
    }

    pub fn overlay(&self) -> Option<&Arc<Overlay>> {
        self.overlay.as_ref()
    }

    /// True when live changes are layered over the base.
    pub fn has_changes(&self) -> bool {
        self.overlay
            .as_ref()
            .is_some_and(|overlay| !overlay.is_empty())
    }

    /// Where the change journal stood when the base was written.
    pub fn journal(&self) -> Option<JournalCursor> {
        self.base.journal
    }

    /// Entries in the base, the root included. Overlay entries have ids
    /// from here on.
    pub fn entry_count(&self) -> u32 {
        self.base.count()
    }

    pub fn base_count(&self) -> u32 {
        self.base.count()
    }

    /// The base's searchable ids, deleted ones included; check
    /// [`VolumeIndex::is_removed`] when the volume has changes.
    pub fn base_ids(&self) -> std::ops::Range<u32> {
        1..self.base.count()
    }

    /// Every live entry but the root, in name order: entries the overlay
    /// added sit where their names sort among the base's.
    pub fn iter_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.live_runs()
            .into_iter()
            .flat_map(|(first, count)| first..first + count)
    }

    /// The overlay's live entries, sorted by name.
    pub fn overlay_ids(&self) -> &[u32] {
        self.overlay
            .as_deref()
            .map(Overlay::sorted_ids)
            .unwrap_or(&[])
    }

    /// Searchable entries, the root left out.
    pub fn item_count(&self) -> u64 {
        let base = u64::from(self.base.count().saturating_sub(1));
        match self.overlay.as_deref() {
            None => base,
            Some(overlay) => {
                base.saturating_sub(u64::from(overlay.removed_count()))
                    + overlay.sorted_ids().len() as u64
            }
        }
    }

    pub fn is_removed(&self, id: u32) -> bool {
        self.view().is_removed(id)
    }

    /// True when the overlay changed something below some folder, so a
    /// folder's `size` needs its delta added on read.
    fn folder_sizes_changed(&self) -> bool {
        self.base.has_folder_totals()
            && self
                .overlay
                .as_deref()
                .is_some_and(Overlay::has_folder_deltas)
    }

    /// Entry `id`. With folder totals, a folder's `size` is its live total:
    /// what was stored for it plus what live changes added below it.
    pub fn entry(&self, id: u32) -> EntryView<'_> {
        let view = self.view();
        let mut entry = view.entry(id);
        if entry.is_dir() && self.folder_sizes_changed() {
            if let Some((size, _)) = view.folder_totals(id) {
                entry.size = size;
            }
        }
        entry
    }

    /// The fields name filtering needs, in one metadata read. Skips the
    /// display name, modification time, and file reference number. A
    /// folder's `size` is its live total, as for [`VolumeIndex::entry`].
    pub fn entry_summary(&self, id: u32) -> EntrySummary<'_> {
        let view = self.view();
        let mut entry = view.entry_summary(id);
        if entry.is_dir() && self.folder_sizes_changed() {
            if let Some((size, _)) = view.folder_totals(id) {
                entry.size = size;
            }
        }
        entry
    }

    /// The modification time of entry `id` as a FILETIME (0 when unknown),
    /// overlay entries included. Date filters read it beside
    /// [`VolumeIndex::entry_summary`], so plain name searches never touch it.
    #[inline]
    pub fn entry_mtime(&self, id: u32) -> u64 {
        self.view().entry_mtime(id)
    }

    /// True when folder `size` fields hold the total of everything below
    /// them. Indexes written before folder totals existed say false: their
    /// folders have size 0 until the next rebuild.
    pub fn has_folder_totals(&self) -> bool {
        self.base.has_folder_totals()
    }

    /// The number of files and folders anywhere below folder `id`, live
    /// changes included, or `None` for a file or an index without folder
    /// totals.
    pub fn folder_items(&self, id: u32) -> Option<u32> {
        if id >= self.base.count() && self.view().entry_or_none(id).is_none() {
            return None;
        }
        self.view().folder_totals(id).map(|(_, items)| items)
    }

    pub fn lookup_frn(&self, frn: u64) -> Option<u32> {
        self.view().lookup_frn(frn)
    }

    pub(crate) fn lookup_directory_frn(&self, frn: u64) -> Option<u32> {
        self.view().lookup_directory_frn(frn)
    }

    /// The entry id of a folder given by its full path, or 0 for the root.
    /// Names are found by binary search over the sorted search names.
    pub fn find_folder(&self, path: &str) -> Option<u32> {
        self.view().find_folder(path)
    }

    pub fn resolve_path(&self, id: u32) -> Option<String> {
        if let Ok(cache) = self.path_cache.lock() {
            if let Some(path) = cache.get(&id) {
                return Some(path.clone());
            }
        }
        let path = self.view().resolve_path(id)?;
        if let Ok(mut cache) = self.path_cache.lock() {
            if cache.len() > 1024 {
                cache.clear();
            }
            cache.insert(id, path.clone());
        }
        Some(path)
    }

    /// Orders two ids of this volume as the index sorts them by name.
    pub fn compare_names(&self, a: u32, b: u32) -> Ordering {
        self.view().compare(a, b)
    }

    /// Every live entry but the root, in name order, as runs of
    /// consecutive ids `(first, count)`. Without changes that is one run;
    /// deleted base entries split runs, and each added entry is a run of
    /// one placed where its name sorts.
    pub fn live_runs(&self) -> Vec<(u32, u32)> {
        let count = self.base.count();
        let Some(overlay) = self.overlay.as_deref() else {
            return if count > 1 {
                vec![(1, count - 1)]
            } else {
                Vec::new()
            };
        };
        let mut runs: Vec<(u32, u32)> = Vec::new();
        let push = |runs: &mut Vec<(u32, u32)>, first: u32, end: u32| {
            if end > first {
                runs.push((first, end - first));
            }
        };
        let mut start = 1u32;
        let mut removed = overlay.removed_ids().peekable();
        for &added in overlay.sorted_ids() {
            let entry = self.entry(added);
            let at = self
                .base
                .insertion_point(entry.name_search, entry.name_display);
            // Base entries before `at`, split around deleted ones.
            while let Some(&gone) = removed.peek() {
                if gone >= at {
                    break;
                }
                removed.next();
                if gone >= start {
                    push(&mut runs, start, gone);
                    start = gone + 1;
                }
            }
            push(&mut runs, start, at.max(start));
            start = at.max(start);
            runs.push((added, 1));
        }
        for gone in removed {
            if gone >= start {
                push(&mut runs, start, gone);
                start = gone + 1;
            }
        }
        push(&mut runs, start, count);
        runs
    }

    pub fn check_offsets(&self, entry_index: usize) -> Option<(u32, u32)> {
        if !entry_index.is_multiple_of(CHECK_STRIDE) {
            return None;
        }
        let slot = entry_index / CHECK_STRIDE;
        let data = self.base.check.as_slice();
        if slot * 8 + 8 > data.len() {
            return None;
        }
        Some((read_u32(data, slot * 8), read_u32(data, slot * 8 + 4)))
    }
}

pub(crate) fn read_u16(data: &[u8], offset: usize) -> u16 {
    data.get(offset..offset + 2)
        .and_then(|b| b.try_into().ok())
        .map(u16::from_le_bytes)
        .unwrap_or(0)
}

pub(crate) fn read_u32(data: &[u8], offset: usize) -> u32 {
    data.get(offset..offset + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .unwrap_or(0)
}

pub(crate) fn read_u64(data: &[u8], offset: usize) -> u64 {
    data.get(offset..offset + 8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
        .unwrap_or(0)
}
