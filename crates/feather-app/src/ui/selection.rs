//! Which results are selected, Explorer style: click, Ctrl+click,
//! Shift+click, Shift+arrows and Ctrl+A.
//!
//! Items are kept by entry, not by row, so a selection survives the list
//! being sorted, narrowed or changed by live updates. Ctrl+A doesn't list
//! every result: it is a flag with the few items taken out of it, so
//! selecting a million results costs nothing.

use std::collections::HashSet;

use feather_core::EntryRef;

#[derive(Default)]
pub struct Selection {
    /// Every result is selected, except those in `picked`.
    all: bool,
    /// The selected items, or with `all` the ones left out.
    picked: HashSet<EntryRef>,
    /// Where Shift+click and Shift+arrows ranges start.
    pub anchor: Option<usize>,
    /// Bumped on every change, for what is worked out from the selection.
    pub version: u64,
}

impl Selection {
    /// Whether more than the focused row is in play: with nothing marked,
    /// the focused row alone is the selection.
    pub fn is_marked(&self) -> bool {
        self.all || !self.picked.is_empty()
    }

    pub fn contains(&self, entry: EntryRef) -> bool {
        self.all != self.picked.contains(&entry)
    }

    pub fn clear(&mut self) {
        if self.is_marked() {
            self.all = false;
            self.picked = HashSet::new();
            self.version += 1;
        }
    }

    pub fn select_all(&mut self) {
        self.all = true;
        self.picked = HashSet::new();
        self.version += 1;
    }

    /// Just this item.
    pub fn only(&mut self, entry: EntryRef) {
        self.all = false;
        self.picked = HashSet::from([entry]);
        self.version += 1;
    }

    /// Ctrl+click: in or out.
    pub fn toggle(&mut self, entry: EntryRef) {
        if !self.picked.remove(&entry) {
            self.picked.insert(entry);
        }
        self.version += 1;
    }

    /// Shift+click: these items, after what was there when `add` (Ctrl).
    pub fn range(&mut self, entries: impl Iterator<Item = EntryRef>, add: bool) {
        if !add {
            self.all = false;
            self.picked.clear();
        }
        for entry in entries {
            if self.all {
                self.picked.remove(&entry);
            } else {
                self.picked.insert(entry);
            }
        }
        self.version += 1;
    }

    /// Before a new search replaces the results: "everything" meant
    /// everything in these results, so it becomes the items themselves
    /// when that's a reasonable number, and is dropped otherwise.
    pub fn pin_all(&mut self, count: usize, listed: impl Iterator<Item = EntryRef>) {
        if !self.all {
            return;
        }
        const MOST: usize = 200_000;
        let left_out = std::mem::take(&mut self.picked);
        self.all = false;
        if count <= MOST {
            self.picked = listed.filter(|entry| !left_out.contains(entry)).collect();
        }
        self.version += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: u32) -> EntryRef {
        EntryRef::new(0, id)
    }

    #[test]
    fn clicks_ctrl_clicks_and_ranges() {
        let mut selection = Selection::default();
        assert!(!selection.is_marked());
        selection.only(e(1));
        selection.toggle(e(3));
        assert!(selection.contains(e(1)) && selection.contains(e(3)));
        selection.toggle(e(1));
        assert!(!selection.contains(e(1)));
        selection.range([e(5), e(6), e(7)].into_iter(), false);
        assert!(selection.contains(e(6)) && !selection.contains(e(3)));
        selection.range([e(1)].into_iter(), true);
        assert!(selection.contains(e(1)) && selection.contains(e(7)));
    }

    #[test]
    fn all_is_cheap_and_takes_exceptions() {
        let mut selection = Selection::default();
        selection.select_all();
        assert!(selection.contains(e(999_999)));
        selection.toggle(e(2));
        assert!(!selection.contains(e(2)) && selection.contains(e(9)));
    }

    #[test]
    fn a_new_search_keeps_what_all_covered() {
        let mut all = Selection::default();
        all.select_all();
        all.toggle(e(2));
        all.pin_all(3, [e(1), e(2), e(3)].into_iter());
        assert!(all.is_marked());
        assert!(all.contains(e(1)) && !all.contains(e(2)) && all.contains(e(3)));
    }
}
