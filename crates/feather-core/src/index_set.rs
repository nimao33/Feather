//! A set of volume indexes searched together.
//!
//! A library directory contains one subdirectory per volume, each with its own
//! manifest. A plain index directory is accepted as a one-volume set, so the
//! single-volume commands and tests keep working.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::entry::EntryView;
use crate::index::VolumeIndex;
use crate::path::path_within;

/// Volumes beyond this count cannot fit in the search selection mask.
pub const MAX_VOLUMES: usize = 64;

/// A stable reference to one entry across all open volumes. The high 32 bits
/// identify the volume and the low 32 bits are the entry id inside it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct EntryRef(u64);

impl EntryRef {
    pub fn new(volume: usize, id: u32) -> Self {
        Self(((volume as u64) << 32) | u64::from(id))
    }

    pub fn volume(self) -> usize {
        (self.0 >> 32) as usize
    }

    pub fn id(self) -> u32 {
        self.0 as u32
    }

    pub fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug)]
pub struct VolumeInfo {
    pub index: usize,
    pub prefix: String,
    pub entry_count: u32,
    pub generation: u64,
    pub source: PathBuf,
}

impl VolumeInfo {
    /// Entries shown in the UI: the synthetic volume root is not a result.
    pub fn item_count(&self) -> u32 {
        self.entry_count.saturating_sub(1)
    }
}

pub struct IndexSet {
    volumes: Vec<VolumeIndex>,
    sources: Vec<PathBuf>,
}

impl IndexSet {
    /// Opens one index directory, or a library directory containing volume
    /// subdirectories. Volume indexes are ordered by their display prefix.
    pub fn open(path: &Path) -> io::Result<Self> {
        if path.join("manifest.bin").is_file() {
            return Self::from_paths(vec![path.to_path_buf()]);
        }
        if !path.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("index directory not found: {}", path.display()),
            ));
        }

        let mut paths = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let child = entry.path();
            if child.is_dir() && child.join("manifest.bin").is_file() {
                paths.push(child);
            }
        }
        if paths.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "no index found in {}; run `feather index scan` first",
                    path.display()
                ),
            ));
        }
        paths.sort();
        Self::from_paths(paths)
    }

    /// A set with no volumes, used before anything has been indexed. Every
    /// search over it finds nothing.
    pub fn empty() -> Self {
        Self {
            volumes: Vec::new(),
            sources: Vec::new(),
        }
    }

    pub fn from_paths(paths: Vec<PathBuf>) -> io::Result<Self> {
        if paths.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "at least one index path is required",
            ));
        }
        if paths.len() > MAX_VOLUMES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("at most {MAX_VOLUMES} volumes can be opened"),
            ));
        }
        let mut volumes = Vec::with_capacity(paths.len());
        for path in &paths {
            volumes.push(VolumeIndex::open(path)?);
        }
        let mut paired: Vec<(VolumeIndex, PathBuf)> = volumes.into_iter().zip(paths).collect();
        paired.sort_by(|a, b| a.0.prefix.cmp(&b.0.prefix));
        let (volumes, sources) = paired.into_iter().unzip();
        Ok(Self { volumes, sources })
    }

    pub fn volumes(&self) -> &[VolumeIndex] {
        &self.volumes
    }

    pub fn volume_count(&self) -> usize {
        self.volumes.len()
    }

    pub fn infos(&self) -> Vec<VolumeInfo> {
        self.volumes
            .iter()
            .enumerate()
            .map(|(index, volume)| VolumeInfo {
                index,
                prefix: volume.prefix.clone(),
                entry_count: volume.entry_count(),
                generation: volume.manifest.generation,
                source: self.sources[index].clone(),
            })
            .collect()
    }

    pub fn entry(&self, entry: EntryRef) -> EntryView<'_> {
        self.volumes[entry.volume()].entry(entry.id())
    }

    pub fn resolve_path(&self, entry: EntryRef) -> Option<String> {
        self.volumes[entry.volume()].resolve_path(entry.id())
    }

    pub fn item_count(&self) -> u64 {
        self.volumes
            .iter()
            .map(|volume| u64::from(volume.entry_count().saturating_sub(1)))
            .sum()
    }

    /// Builds the search mask from the volume prefixes the user excluded.
    /// A missing volume is simply not in the mask. Newly mounted drives are
    /// included by default because the exclusion list is normally empty.
    pub fn volume_mask(&self, excluded_prefixes: &[String]) -> u64 {
        let mut mask = 0u64;
        for (index, volume) in self.volumes.iter().enumerate() {
            if index >= MAX_VOLUMES {
                break;
            }
            let excluded = excluded_prefixes
                .iter()
                .any(|prefix| path_within(&volume.prefix, prefix));
            if !excluded {
                mask |= 1u64 << index;
            }
        }
        mask
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthetic::generate;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "feather-index-set-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    #[test]
    fn an_empty_set_has_nothing_to_search() {
        let set = IndexSet::empty();
        assert_eq!(set.volume_count(), 0);
        assert_eq!(set.item_count(), 0);
        assert_eq!(set.volume_mask(&[]), 0);
        assert!(set.infos().is_empty());
    }

    #[test]
    fn opens_a_single_volume_directory() {
        let dir = temp_dir("single");
        generate(&dir, 500, r"C:\FeatherTest").expect("generate");
        let set = IndexSet::open(&dir).expect("open");
        assert_eq!(set.volume_count(), 1);
        assert_eq!(set.infos()[0].prefix, r"C:\FeatherTest");
        assert_eq!(set.item_count(), 499);
        let reference = EntryRef::new(0, 1);
        assert!(set.resolve_path(reference).is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    // Uses drive letters and backslashes: Windows paths.
    #[cfg(windows)]
    #[test]
    fn discovers_volume_subdirectories_and_filters_the_mask() {
        let root = temp_dir("library");
        let c = root.join("c");
        let d = root.join("d");
        generate(&c, 300, r"C:\FeatherTest").expect("generate c");
        generate(&d, 200, r"D:\FeatherTest").expect("generate d");

        let set = IndexSet::open(&root).expect("open library");
        assert_eq!(set.volume_count(), 2);
        assert_eq!(set.item_count(), 498);
        assert_eq!(set.volume_mask(&[]), 0b11);
        assert_eq!(set.volume_mask(&["c:".into()]), 0b10);
        assert_eq!(set.volume_mask(&[r"C:\FeatherTest".into(), "d:".into()]), 0);

        let second = EntryRef::new(1, 1);
        assert!(set
            .resolve_path(second)
            .unwrap()
            .starts_with(r"D:\FeatherTest"));
        let _ = fs::remove_dir_all(&root);
    }
}
