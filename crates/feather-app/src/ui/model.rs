//! GUI state independent of Win32. The panels render this; the search worker
//! feeds it through `apply`. Keeping the state here makes new features and
//! tests possible without creating a window.

use std::time::Duration;

#[cfg(test)]
use feather_core::EntryRef;

use crate::rows::Rows;
use crate::search::SearchEvent;

#[derive(Default)]
pub struct UiModel {
    pub ids: Rows,
    pub current_generation: u64,
    pub total: u64,
    pub capped: bool,
    pub elapsed_ms: u128,
    pub searching: bool,
    pub search_done: bool,
    /// False while the empty state is shown because the user chose not to
    /// list everything for an empty search box.
    pub started: bool,
    pub warnings: Vec<String>,
    /// The rows still shown are the previous search's: kept on screen
    /// until the new one sends rows or finishes, so typing doesn't flash
    /// an empty list.
    pub stale: bool,
}

impl UiModel {
    pub fn begin(&mut self, generation: u64, warnings: Vec<String>) {
        self.current_generation = generation;
        self.warnings = warnings;
        self.stale = true;
        self.total = 0;
        self.capped = false;
        self.elapsed_ms = 0;
        self.searching = true;
        self.search_done = false;
        self.started = true;
    }

    /// Results filtered from the previous ones, complete at once.
    pub fn narrowed(
        &mut self,
        generation: u64,
        warnings: Vec<String>,
        ids: Rows,
        elapsed: Duration,
    ) {
        self.current_generation = generation;
        self.warnings = warnings;
        self.total = ids.len() as u64;
        self.ids = ids;
        self.stale = false;
        self.capped = false;
        self.elapsed_ms = elapsed.as_millis();
        self.searching = false;
        self.search_done = true;
        self.started = true;
    }

    /// Drops the previous search's rows once the new one has its own.
    fn fresh(&mut self) {
        if self.stale {
            self.ids.clear();
            self.stale = false;
        }
    }

    /// Returns to the untitled empty state. The generation still advances so
    /// batches from the cancelled search cannot arrive as current results.
    pub fn clear(&mut self, generation: u64) {
        *self = Self {
            current_generation: generation,
            ..Self::default()
        };
    }

    /// Returns true when the event belonged to the current generation.
    pub fn apply(&mut self, event: SearchEvent) -> bool {
        match event {
            SearchEvent::Batch { generation, ids } if generation == self.current_generation => {
                self.fresh();
                self.ids.push_refs(&ids);
                true
            }
            SearchEvent::Span {
                generation,
                volume,
                first,
                count,
            } if generation == self.current_generation => {
                self.fresh();
                self.ids.push_span(volume, first, count);
                true
            }
            SearchEvent::Done {
                generation,
                total,
                capped,
                elapsed,
            } if generation == self.current_generation => {
                self.fresh();
                self.total = total;
                self.capped = capped;
                self.elapsed_ms = elapsed.as_millis();
                self.searching = false;
                self.search_done = true;
                // Spare room from growing the list goes back.
                self.ids.shrink();
                true
            }
            _ => false,
        }
    }

    pub fn is_idle(&self) -> bool {
        !self.searching && (self.search_done || !self.started)
    }

    pub fn result_count(&self) -> usize {
        self.ids.len()
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    #[allow(dead_code)]
    pub fn elapsed(&self) -> Duration {
        Duration::from_millis(self.elapsed_ms as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_events_are_ignored() {
        let mut model = UiModel::default();
        model.begin(2, Vec::new());
        assert!(!model.apply(SearchEvent::Batch {
            generation: 1,
            ids: vec![EntryRef::new(0, 1), EntryRef::new(0, 2)],
        }));
        assert!(model.ids.is_empty());

        assert!(model.apply(SearchEvent::Batch {
            generation: 2,
            ids: vec![EntryRef::new(0, 3), EntryRef::new(1, 4)],
        }));
        assert_eq!(
            model.ids.to_vec(),
            vec![EntryRef::new(0, 3), EntryRef::new(1, 4)]
        );
        assert!(!model.apply(SearchEvent::Done {
            generation: 1,
            total: 2,
            capped: false,
            elapsed: Duration::from_millis(5),
        }));
        assert!(model.searching);

        assert!(model.apply(SearchEvent::Done {
            generation: 2,
            total: 2,
            capped: false,
            elapsed: Duration::from_millis(5),
        }));
        assert!(model.is_idle());
        assert_eq!(model.total(), 2);
        assert_eq!(model.elapsed_ms, 5);
    }

    #[test]
    fn a_new_query_clears_the_previous_rows() {
        let mut model = UiModel::default();
        model.begin(1, Vec::new());
        model.apply(SearchEvent::Batch {
            generation: 1,
            ids: vec![EntryRef::new(0, 9)],
        });
        model.begin(2, Vec::new());
        // Shown until the new search sends something.
        assert_eq!(model.ids.len(), 1);
        assert!(!model.search_done);
        model.apply(SearchEvent::Batch {
            generation: 2,
            ids: vec![EntryRef::new(0, 4)],
        });
        assert_eq!(model.ids.to_vec(), vec![EntryRef::new(0, 4)]);
        assert_eq!(model.current_generation, 2);
    }

    #[test]
    fn clearing_returns_to_the_idle_empty_state() {
        let mut model = UiModel::default();
        model.begin(1, Vec::new());
        model.apply(SearchEvent::Batch {
            generation: 1,
            ids: vec![EntryRef::new(0, 9)],
        });
        model.clear(2);
        assert!(model.ids.is_empty());
        assert!(!model.started);
        assert!(model.is_idle());
        assert_eq!(model.current_generation, 2);
        assert!(!model.apply(SearchEvent::Batch {
            generation: 1,
            ids: vec![EntryRef::new(0, 10)],
        }));
    }
}
