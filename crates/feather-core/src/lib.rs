//! Core index, query, and search types for Feather.

pub mod entry;
pub mod format;
pub mod index;
pub mod index_set;
pub mod live;
pub mod mapped;
#[cfg(windows)]
pub mod mft;
pub mod path;
pub mod query;
pub mod scan;
pub mod synthetic;
pub mod usn;

pub use entry::{EntrySummary, EntryView};
pub use index::VolumeIndex;
pub use index_set::{EntryRef, IndexSet, VolumeInfo, MAX_VOLUMES};
pub use path::{is_absolute_path, normalize_path, path_within};
pub use query::{glob_match, Kind, MatchMode, PathCheck, Query};

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
