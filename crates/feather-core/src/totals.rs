//! Folder totals: how many bytes and items sit below each folder.
//!
//! The index stores a folder's total in its `size` field (files keep their
//! own size) and the number of items below it in the `.count` table. Both
//! are worked out once, when an index is written, from the parent links
//! alone: no folder is read from disk again.
//!
//! Anything that changes the tree later (the live update overlay, for
//! example) can keep the totals right by applying the same rule to every
//! ancestor of a changed entry: a file of `n` bytes adds `n` bytes and one
//! item to each folder above it, and a folder adds one item (its own files
//! are counted as they are added).

/// Turns per-entry sizes into folder totals, in place.
///
/// `parent[i]` is the index of entry `i`'s folder; entry 0 is the root and
/// its parent is ignored. `flags` bit 0 marks a folder. On return every
/// folder's `size` is the sum of the sizes of all files anywhere below it,
/// and the returned vector holds, for every folder, the number of files and
/// folders anywhere below it (0 for files). A folder's own size before the
/// call is ignored.
///
/// Parents usually come before their children (both the directory walk and
/// the NTFS reader write them that way), and then this is one pass from the
/// end of the list with no extra memory but the counts. Any other order
/// still works: each file is added to its ancestors one by one. Links that
/// point outside the list, or loop, stop the climb instead of hanging.
pub fn fold_folder_totals(parent: &[u32], flags: &[u16], size: &mut [u64]) -> Vec<u32> {
    let count = parent.len().min(flags.len()).min(size.len());
    let mut items = vec![0u32; count];
    if count == 0 {
        return items;
    }
    let is_dir = |index: usize| flags[index] & 1 != 0;
    for (bytes, &flag) in size[..count].iter_mut().zip(&flags[..count]) {
        if flag & 1 != 0 {
            *bytes = 0;
        }
    }

    let parents_first = (1..count).all(|index| (parent[index] as usize) < index);
    if parents_first {
        // Every child sits after its folder, so walking backwards finishes
        // each folder before the folder above it reads it.
        for index in (1..count).rev() {
            let up = parent[index] as usize;
            size[up] = size[up].saturating_add(size[index]);
            items[up] = items[up].saturating_add(items[index]).saturating_add(1);
        }
        return items;
    }

    // Any order: climb from each entry to the root. A folder's own size is
    // zero here, so only files carry bytes up.
    for index in 1..count {
        let bytes = if is_dir(index) { 0 } else { size[index] };
        let mut up = parent[index] as usize;
        let mut steps = 0usize;
        while up < count && steps < count {
            size[up] = size[up].saturating_add(bytes);
            items[up] = items[up].saturating_add(1);
            if up == 0 {
                break;
            }
            up = parent[up] as usize;
            steps += 1;
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    /// root
    /// ├─ a/            (dir 1)
    /// │  ├─ b/         (dir 2)
    /// │  │  ├─ x 100   (3)
    /// │  │  └─ y 20    (4)
    /// │  └─ z 3        (5)
    /// ├─ empty/        (6)
    /// └─ w 7           (7)
    fn sample() -> (Vec<u32>, Vec<u16>, Vec<u64>) {
        let parent = vec![0, 0, 1, 2, 2, 1, 0, 0];
        let flags = vec![1, 1, 1, 0, 0, 0, 1, 0];
        let size = vec![0, 0, 0, 100, 20, 3, 0, 7];
        (parent, flags, size)
    }

    fn check(size: &[u64], items: &[u32]) {
        assert_eq!(size[0], 130);
        assert_eq!(items[0], 7);
        assert_eq!((size[1], items[1]), (123, 4));
        assert_eq!((size[2], items[2]), (120, 2));
        assert_eq!((size[6], items[6]), (0, 0));
        // Files keep their size and count nothing.
        assert_eq!((size[3], items[3]), (100, 0));
        assert_eq!((size[7], items[7]), (7, 0));
    }

    #[test]
    fn totals_nested_folders_with_parents_first() {
        let (parent, flags, mut size) = sample();
        let items = fold_folder_totals(&parent, &flags, &mut size);
        check(&size, &items);
    }

    #[test]
    fn totals_nested_folders_in_any_order() {
        // The same tree with children listed before their folders.
        // Old index -> new index: 0->0, 7->1, 6->2, 5->3, 4->4, 3->5, 2->6, 1->7
        let map = [0usize, 7, 6, 5, 4, 3, 2, 1];
        let (parent, flags, size) = sample();
        let mut p = vec![0u32; 8];
        let mut f = vec![0u16; 8];
        let mut s = vec![0u64; 8];
        for old in 0..8 {
            p[map[old]] = map[parent[old] as usize] as u32;
            f[map[old]] = flags[old];
            s[map[old]] = size[old];
        }
        let items = fold_folder_totals(&p, &f, &mut s);
        let mut back_size = vec![0u64; 8];
        let mut back_items = vec![0u32; 8];
        for old in 0..8 {
            back_size[old] = s[map[old]];
            back_items[old] = items[map[old]];
        }
        check(&back_size, &back_items);
    }

    #[test]
    fn a_folders_own_size_is_ignored() {
        let (parent, flags, mut size) = sample();
        size[1] = 4096;
        size[6] = 4096;
        let items = fold_folder_totals(&parent, &flags, &mut size);
        check(&size, &items);
    }

    #[test]
    fn loops_and_bad_links_do_not_hang() {
        // 1 and 2 point at each other; 3 points past the end.
        let parent = vec![0, 2, 1, 99];
        let flags = vec![1, 1, 1, 0];
        let mut size = vec![0, 0, 0, 5];
        let items = fold_folder_totals(&parent, &flags, &mut size);
        assert_eq!(items.len(), 4);
        assert_eq!(size[3], 5);
    }

    #[test]
    fn an_empty_list_is_fine() {
        assert!(fold_folder_totals(&[], &[], &mut []).is_empty());
    }
}
