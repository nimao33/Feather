//! A result list in as little memory as it can take.
//!
//! "Everything" on a big disk is millions of rows. Listed in index order it
//! is just a run of ids per volume, so it is kept as a few spans: a handful
//! of bytes for millions of rows. Other lists pack each row into 4 bytes (6
//! bits of volume, 26 of entry id) instead of 8, and only fall back to full
//! references if an id ever doesn't fit.

use feather_core::EntryRef;

const ID_BITS: u32 = 26;
const ID_MASK: u32 = (1 << ID_BITS) - 1;
const MAX_PACKED_VOLUME: usize = 1 << (32 - ID_BITS);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    /// The first row this span covers.
    row: usize,
    volume: usize,
    first: u32,
    count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Store {
    Spans(Vec<Span>),
    Packed(Vec<u32>),
    Wide(Vec<EntryRef>),
}

impl Default for Store {
    fn default() -> Self {
        Store::Spans(Vec::new())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rows {
    store: Store,
    len: usize,
}

fn pack(entry: EntryRef) -> Option<u32> {
    (entry.volume() < MAX_PACKED_VOLUME && entry.id() <= ID_MASK)
        .then(|| ((entry.volume() as u32) << ID_BITS) | entry.id())
}

fn unpack(value: u32) -> EntryRef {
    EntryRef::new((value >> ID_BITS) as usize, value & ID_MASK)
}

impl Rows {
    pub fn from_refs(refs: Vec<EntryRef>) -> Self {
        let mut rows = Rows::default();
        rows.push_refs(&refs);
        rows
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Empties the list and lets go of its memory.
    pub fn clear(&mut self) {
        *self = Rows::default();
    }

    pub fn get(&self, row: usize) -> Option<EntryRef> {
        if row >= self.len {
            return None;
        }
        match &self.store {
            Store::Spans(spans) => {
                let at = spans
                    .partition_point(|span| span.row <= row)
                    .checked_sub(1)?;
                let span = spans[at];
                Some(EntryRef::new(
                    span.volume,
                    span.first + (row - span.row) as u32,
                ))
            }
            Store::Packed(values) => values.get(row).map(|value| unpack(*value)),
            Store::Wide(refs) => refs.get(row).copied(),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = EntryRef> + '_ {
        (0..self.len).filter_map(move |row| self.get(row))
    }

    pub fn to_vec(&self) -> Vec<EntryRef> {
        self.iter().collect()
    }

    /// Adds `count` entries of `volume` starting at id `first`.
    pub fn push_span(&mut self, volume: usize, first: u32, count: u32) {
        if count == 0 {
            return;
        }
        if let Store::Spans(spans) = &mut self.store {
            if let Some(last) = spans.last_mut() {
                if last.volume == volume && last.first + last.count == first {
                    last.count += count;
                    self.len += count as usize;
                    return;
                }
            }
            spans.push(Span {
                row: self.len,
                volume,
                first,
                count,
            });
            self.len += count as usize;
            return;
        }
        let refs: Vec<EntryRef> = (first..first + count)
            .map(|id| EntryRef::new(volume, id))
            .collect();
        self.push_refs(&refs);
    }

    /// Adds explicit rows.
    pub fn push_refs(&mut self, refs: &[EntryRef]) {
        if refs.is_empty() {
            return;
        }
        if matches!(self.store, Store::Spans(_)) {
            self.materialize();
        }
        if let Store::Packed(values) = &mut self.store {
            let packed: Option<Vec<u32>> = refs.iter().map(|entry| pack(*entry)).collect();
            match packed {
                Some(packed) => {
                    values.extend(packed);
                    self.len += refs.len();
                    return;
                }
                None => {
                    let wide: Vec<EntryRef> = values.iter().map(|value| unpack(*value)).collect();
                    self.store = Store::Wide(wide);
                }
            }
        }
        if let Store::Wide(wide) = &mut self.store {
            wide.extend_from_slice(refs);
            self.len += refs.len();
        }
    }

    /// Turns spans into explicit rows, packed when they fit.
    fn materialize(&mut self) {
        let Store::Spans(spans) = &self.store else {
            return;
        };
        let fits = spans.iter().all(|span| {
            span.volume < MAX_PACKED_VOLUME && span.first + span.count.saturating_sub(1) <= ID_MASK
        });
        self.store = if fits {
            let mut values = Vec::with_capacity(self.len);
            for span in spans {
                values.extend(
                    (span.first..span.first + span.count)
                        .map(|id| ((span.volume as u32) << ID_BITS) | id),
                );
            }
            Store::Packed(values)
        } else {
            let mut refs = Vec::with_capacity(self.len);
            for span in spans {
                refs.extend(
                    (span.first..span.first + span.count).map(|id| EntryRef::new(span.volume, id)),
                );
            }
            Store::Wide(refs)
        };
    }

    /// Sorts the rows; a span list becomes an explicit one first.
    pub fn sort_by(&mut self, mut compare: impl FnMut(EntryRef, EntryRef) -> std::cmp::Ordering) {
        self.materialize();
        match &mut self.store {
            Store::Packed(values) => {
                values.sort_unstable_by(|a, b| compare(unpack(*a), unpack(*b)))
            }
            Store::Wide(refs) => refs.sort_unstable_by(|a, b| compare(*a, *b)),
            Store::Spans(_) => {}
        }
    }

    /// Returns spare capacity left from growing, once a list is complete.
    pub fn shrink(&mut self) {
        match &mut self.store {
            Store::Spans(spans) => spans.shrink_to_fit(),
            Store::Packed(values) => values.shrink_to_fit(),
            Store::Wide(refs) => refs.shrink_to_fit(),
        }
    }

    /// Bytes the list holds, for tests and the memory report.
    pub fn bytes(&self) -> usize {
        match &self.store {
            Store::Spans(spans) => spans.capacity() * std::mem::size_of::<Span>(),
            Store::Packed(values) => values.capacity() * 4,
            Store::Wide(refs) => refs.capacity() * 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_cost_almost_nothing() {
        let mut rows = Rows::default();
        rows.push_span(0, 0, 2_000_000);
        rows.push_span(1, 0, 500_000);
        assert_eq!(rows.len(), 2_500_000);
        assert!(rows.bytes() < 1024);
        assert_eq!(rows.get(0), Some(EntryRef::new(0, 0)));
        assert_eq!(rows.get(1_999_999), Some(EntryRef::new(0, 1_999_999)));
        assert_eq!(rows.get(2_000_000), Some(EntryRef::new(1, 0)));
        assert_eq!(rows.get(2_500_000), None);
    }

    #[test]
    fn explicit_rows_are_packed_and_sort() {
        let mut rows = Rows::default();
        rows.push_refs(&[EntryRef::new(1, 9), EntryRef::new(0, 3)]);
        rows.push_span(2, 5, 2);
        assert_eq!(
            rows.to_vec(),
            vec![
                EntryRef::new(1, 9),
                EntryRef::new(0, 3),
                EntryRef::new(2, 5),
                EntryRef::new(2, 6)
            ]
        );
        assert_eq!(rows.bytes() / rows.len(), 4);
        rows.sort_by(|a, b| a.cmp(&b));
        assert_eq!(rows.get(0), Some(EntryRef::new(0, 3)));
    }

    #[test]
    fn ids_too_big_to_pack_still_work() {
        let mut rows = Rows::default();
        rows.push_refs(&[EntryRef::new(0, 1)]);
        rows.push_refs(&[EntryRef::new(3, u32::MAX - 1)]);
        assert_eq!(rows.get(1), Some(EntryRef::new(3, u32::MAX - 1)));
        assert_eq!(rows.get(0), Some(EntryRef::new(0, 1)));
        let mut spans = Rows::default();
        spans.push_span(0, u32::MAX - 10, 5);
        spans.sort_by(|a, b| b.cmp(&a));
        assert_eq!(spans.get(0), Some(EntryRef::new(0, u32::MAX - 6)));
    }
}
