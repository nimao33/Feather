use std::io;

use crate::entry::{EntrySummary, EntryView};
use crate::format::{Manifest, CHECK_STRIDE, COUNT_SIZE, FRN_SIZE, META_SIZE};
use crate::mapped::MappedFile;

pub struct VolumeIndex {
    pub manifest: Manifest,
    pub prefix: String,
    meta: MappedFile,
    search: MappedFile,
    display: MappedFile,
    frn: MappedFile,
    check: MappedFile,
    /// Items below each folder, when the index has folder totals.
    counts: Option<MappedFile>,
    path_cache: std::sync::Mutex<std::collections::HashMap<u32, String>>,
}

impl VolumeIndex {
    pub fn open(dir: &std::path::Path) -> io::Result<Self> {
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

        let index = Self {
            manifest,
            prefix: std::fs::read_to_string(dir.join("prefix.txt"))
                .unwrap_or_else(|_| crate::path::default_prefix().to_string())
                .trim()
                .to_string(),
            meta,
            search,
            display,
            frn,
            check,
            counts,
            path_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        };
        index.validate()?;
        Ok(index)
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

    pub fn entry_count(&self) -> u32 {
        self.manifest.entry_count
    }

    /// True when folder `size` fields hold the total of everything below
    /// them. Indexes written before folder totals existed say false: their
    /// folders have size 0 until the next rebuild.
    pub fn has_folder_totals(&self) -> bool {
        self.counts.is_some()
    }

    /// The number of files and folders anywhere below folder `id`, or
    /// `None` for a file or an index without folder totals.
    pub fn folder_items(&self, id: u32) -> Option<u32> {
        let counts = self.counts.as_ref()?;
        if id >= self.manifest.entry_count || !self.entry_summary(id).is_dir() {
            return None;
        }
        Some(read_u32(counts.as_slice(), id as usize * COUNT_SIZE))
    }

    pub fn iter_ids(&self) -> std::ops::Range<u32> {
        1..self.manifest.entry_count
    }

    pub fn entry(&self, id: u32) -> EntryView<'_> {
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

    /// The fields name filtering needs, in one metadata read. Skips the
    /// display name, modification time, and file reference number.
    pub fn entry_summary(&self, id: u32) -> EntrySummary<'_> {
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

    /// The modification time of entry `id` as a FILETIME (0 when unknown).
    /// Date filters read it beside [`VolumeIndex::entry_summary`], so plain
    /// name searches never touch it.
    #[inline]
    pub fn entry_mtime(&self, id: u32) -> u64 {
        let id = id.min(self.manifest.entry_count.saturating_sub(1));
        read_u64(self.meta.as_slice(), id as usize * META_SIZE + 24)
    }

    pub fn lookup_frn(&self, frn: u64) -> Option<u32> {
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
        if low < count && read_u64(data, low * FRN_SIZE) == frn {
            Some(read_u32(data, low * FRN_SIZE + 8))
        } else {
            None
        }
    }

    pub(crate) fn lookup_directory_frn(&self, frn: u64) -> Option<u32> {
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
        let mut first = None;
        while low < count && read_u64(data, low * FRN_SIZE) == frn {
            let id = read_u32(data, low * FRN_SIZE + 8);
            if self.entry(id).is_dir() {
                return Some(id);
            }
            first.get_or_insert(id);
            low += 1;
        }
        first
    }

    /// The entry id of a folder given by its full path, or 0 for the root.
    /// Names are found by binary search over the sorted search names.
    pub fn find_folder(&self, path: &str) -> Option<u32> {
        let path = crate::path::normalize_path(path);
        let prefix = crate::path::normalize_path(&self.prefix);
        if path == prefix {
            return Some(0);
        }
        let rest = path.strip_prefix(&prefix)?;
        let rest = rest.strip_prefix(crate::path::SEP).unwrap_or(rest);
        let mut current = self.entry(0).frn;
        let mut found = 0;
        for part in rest.split(crate::path::SEP).filter(|part| !part.is_empty()) {
            found = self.find_child_folder(current, part)?;
            current = self.entry(found).frn;
        }
        Some(found)
    }

    fn find_child_folder(&self, parent_frn: u64, name: &str) -> Option<u32> {
        let count = self.manifest.entry_count;
        // Entry 0 is the root; the rest are sorted by search name.
        let (mut low, mut high) = (1u32, count);
        while low < high {
            let mid = low + (high - low) / 2;
            if self.entry(mid).name_search < name {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        let mut id = low;
        while id < count {
            let entry = self.entry(id);
            if entry.name_search != name {
                break;
            }
            if entry.parent_frn == parent_frn && entry.is_dir() {
                return Some(id);
            }
            id += 1;
        }
        // An index whose names are not sorted (older synthetic data).
        (1..count).find(|&id| {
            let entry = self.entry(id);
            entry.parent_frn == parent_frn && entry.is_dir() && entry.name_search == name
        })
    }

    pub fn resolve_path(&self, id: u32) -> Option<String> {
        if let Ok(cache) = self.path_cache.lock() {
            if let Some(path) = cache.get(&id) {
                return Some(path.clone());
            }
        }

        let mut parts = Vec::new();
        let mut current = id;
        let mut found_root = false;
        for _ in 0..128 {
            let entry = self.entry(current);
            if !entry.name_display.is_empty() {
                parts.push(String::from_utf16_lossy(entry.name_display));
            }
            if entry.id == 0 || entry.parent_frn == entry.frn {
                found_root = true;
                break;
            }
            current = self.lookup_directory_frn(entry.parent_frn)?;
        }
        if !found_root {
            return None;
        }

        parts.reverse();
        let prefix = self.prefix.trim_end_matches(crate::path::SEP);
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

        if let Ok(mut cache) = self.path_cache.lock() {
            if cache.len() > 1024 {
                cache.clear();
            }
            cache.insert(id, path.clone());
        }
        Some(path)
    }

    pub fn check_offsets(&self, entry_index: usize) -> Option<(u32, u32)> {
        if !entry_index.is_multiple_of(CHECK_STRIDE) {
            return None;
        }
        let slot = entry_index / CHECK_STRIDE;
        let data = self.check.as_slice();
        if slot * 8 + 8 > data.len() {
            return None;
        }
        Some((read_u32(data, slot * 8), read_u32(data, slot * 8 + 4)))
    }
}

fn read_u16(data: &[u8], offset: usize) -> u16 {
    data.get(offset..offset + 2)
        .and_then(|b| b.try_into().ok())
        .map(u16::from_le_bytes)
        .unwrap_or(0)
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    data.get(offset..offset + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .unwrap_or(0)
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    data.get(offset..offset + 8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
        .unwrap_or(0)
}
