//! Comparators for the sortable results table. Kept independent of Win32 and
//! the index so sorting can be tested with plain `EntryView` values.

use std::cmp::Ordering;

use feather_core::EntryView;

use crate::text::extension;

pub const COLUMN_NAME: usize = 0;
pub const COLUMN_PATH: usize = 1;
pub const COLUMN_SIZE: usize = 2;
pub const COLUMN_MODIFIED: usize = 3;
pub const COLUMN_TYPE: usize = 4;

pub fn compare_entries(a: &EntryView<'_>, b: &EntryView<'_>, column: usize) -> Ordering {
    match column {
        COLUMN_NAME => a.name_search.cmp(b.name_search),
        COLUMN_SIZE => a.size.cmp(&b.size),
        COLUMN_MODIFIED => a.mtime.cmp(&b.mtime),
        COLUMN_TYPE => extension(a.name_search)
            .cmp(extension(b.name_search))
            .then_with(|| a.name_search.cmp(b.name_search)),
        // The path column is sorted by the caller, which can resolve names to
        // folders; entry metadata alone does not contain that string.
        _ => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &'static str, is_dir: bool, size: u64, mtime: u64) -> EntryView<'static> {
        EntryView {
            id: 0,
            frn: 0,
            parent_frn: 0,
            size,
            mtime,
            flags: u16::from(is_dir),
            search_offset: 0,
            search_len: 0,
            display_offset: 0,
            display_len: 0,
            name_search: name,
            name_display: &[],
        }
    }

    #[test]
    fn compares_names() {
        let a = entry("apple.txt", false, 0, 0);
        let b = entry("banana.txt", false, 0, 0);
        assert_eq!(compare_entries(&a, &b, COLUMN_NAME), Ordering::Less);
        assert_eq!(compare_entries(&b, &a, COLUMN_NAME), Ordering::Greater);
        assert_eq!(compare_entries(&a, &a, COLUMN_NAME), Ordering::Equal);
    }

    #[test]
    fn compares_sizes_and_dates() {
        let small = entry("a.bin", false, 10, 100);
        let large = entry("b.bin", false, 20, 200);
        assert_eq!(compare_entries(&small, &large, COLUMN_SIZE), Ordering::Less);
        assert_eq!(
            compare_entries(&large, &small, COLUMN_MODIFIED),
            Ordering::Greater
        );
    }

    #[test]
    fn compares_types_then_names() {
        let a = entry("beta.pdf", false, 0, 0);
        let b = entry("alpha.txt", false, 0, 0);
        let c = entry("alpha.pdf", false, 0, 0);
        assert_eq!(compare_entries(&a, &b, COLUMN_TYPE), Ordering::Less);
        assert_eq!(compare_entries(&a, &c, COLUMN_TYPE), Ordering::Greater);
    }

    #[test]
    fn folders_have_no_special_type_order() {
        let folder = entry("src", true, 0, 0);
        let file = entry("src.txt", false, 0, 0);
        assert_eq!(compare_entries(&folder, &file, COLUMN_TYPE), Ordering::Less);
    }
}
