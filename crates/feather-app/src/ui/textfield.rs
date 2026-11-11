//! Editing state for the custom-drawn search field.
//!
//! Text is stored as UTF-16 so caret positions match DirectWrite hit-test
//! positions directly. Positions never split a surrogate pair.

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TextField {
    text: Vec<u16>,
    caret: usize,
    anchor: usize,
    /// A password: shown as dots and never copied.
    secret: bool,
    /// A password shown as typed, after its eye button was pressed.
    revealed: bool,
}

impl TextField {
    pub fn text(&self) -> String {
        String::from_utf16_lossy(&self.text)
    }

    pub fn wide(&self) -> &[u16] {
        &self.text
    }

    pub fn set_secret(&mut self, secret: bool) {
        self.secret = secret;
    }

    pub fn is_secret(&self) -> bool {
        self.secret
    }

    pub fn is_revealed(&self) -> bool {
        self.revealed
    }

    /// Shows a password as typed, or as dots again.
    pub fn toggle_reveal(&mut self) {
        self.revealed = !self.revealed;
    }

    /// What to draw: the text, or one dot per unit for a password, so caret
    /// positions line up either way.
    pub fn shown(&self) -> std::borrow::Cow<'_, [u16]> {
        if self.secret && !self.revealed {
            std::borrow::Cow::Owned(vec![0x2022; self.text.len()])
        } else {
            std::borrow::Cow::Borrowed(&self.text)
        }
    }

    pub fn caret(&self) -> usize {
        self.caret
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The selected range as `start..end`; empty when nothing is selected.
    pub fn selection(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }

    pub fn has_selection(&self) -> bool {
        self.caret != self.anchor
    }

    pub fn selected_text(&self) -> String {
        if self.secret {
            return String::new();
        }
        let (start, end) = self.selection();
        String::from_utf16_lossy(&self.text[start..end])
    }

    /// Replaces all text and puts the caret at the end.
    pub fn set_text(&mut self, text: &str) {
        self.text = text.encode_utf16().collect();
        self.caret = self.text.len();
        self.anchor = self.caret;
    }

    /// Inserts text over the selection. Line breaks become spaces because the
    /// field is a single line.
    pub fn insert(&mut self, text: &str) -> bool {
        let units: Vec<u16> = text
            .chars()
            .map(|c| {
                if c == '\r' || c == '\n' || c == '\t' {
                    ' '
                } else {
                    c
                }
            })
            .filter(|c| !c.is_control())
            .collect::<String>()
            .encode_utf16()
            .collect();
        if units.is_empty() && !self.has_selection() {
            return false;
        }
        self.insert_units(&units);
        true
    }

    /// Inserts raw UTF-16 units, such as the halves of a surrogate pair that
    /// arrive in separate `WM_CHAR` messages.
    pub fn insert_units(&mut self, units: &[u16]) {
        let (start, end) = self.selection();
        self.text.splice(start..end, units.iter().copied());
        self.caret = start + units.len();
        self.anchor = self.caret;
    }

    pub fn backspace(&mut self, word: bool) -> bool {
        if self.has_selection() {
            return self.delete_selection();
        }
        if self.caret == 0 {
            return false;
        }
        let start = if word {
            self.word_left(self.caret)
        } else {
            self.prev(self.caret)
        };
        self.text.drain(start..self.caret);
        self.caret = start;
        self.anchor = start;
        true
    }

    pub fn delete(&mut self, word: bool) -> bool {
        if self.has_selection() {
            return self.delete_selection();
        }
        if self.caret >= self.text.len() {
            return false;
        }
        let end = if word {
            self.word_right(self.caret)
        } else {
            self.next(self.caret)
        };
        self.text.drain(self.caret..end);
        true
    }

    pub fn delete_selection(&mut self) -> bool {
        let (start, end) = self.selection();
        if start == end {
            return false;
        }
        self.text.drain(start..end);
        self.caret = start;
        self.anchor = start;
        true
    }

    pub fn move_left(&mut self, select: bool, word: bool) {
        let target = if !select && self.has_selection() && !word {
            self.selection().0
        } else if word {
            self.word_left(self.caret)
        } else {
            self.prev(self.caret)
        };
        self.move_to(target, select);
    }

    pub fn move_right(&mut self, select: bool, word: bool) {
        let target = if !select && self.has_selection() && !word {
            self.selection().1
        } else if word {
            self.word_right(self.caret)
        } else {
            self.next(self.caret)
        };
        self.move_to(target, select);
    }

    pub fn home(&mut self, select: bool) {
        self.move_to(0, select);
    }

    pub fn end(&mut self, select: bool) {
        self.move_to(self.text.len(), select);
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    /// Moves the caret to a position from a hit test, keeping the anchor when
    /// extending a selection.
    pub fn move_to(&mut self, position: usize, select: bool) {
        let position = self.snap(position.min(self.text.len()));
        self.caret = position;
        if !select {
            self.anchor = position;
        }
    }

    /// Selects the word around a position, for double-click.
    pub fn select_word_at(&mut self, position: usize) {
        let position = position.min(self.text.len());
        let is_word = |unit: u16| !is_space(unit);
        let mut start = position;
        while start > 0 && is_word(self.text[start - 1]) {
            start -= 1;
        }
        let mut end = position;
        while end < self.text.len() && is_word(self.text[end]) {
            end += 1;
        }
        self.anchor = start;
        self.caret = end;
    }

    fn prev(&self, position: usize) -> usize {
        if position == 0 {
            return 0;
        }
        let mut position = position - 1;
        if position > 0
            && is_low_surrogate(self.text[position])
            && is_high_surrogate(self.text[position - 1])
        {
            position -= 1;
        }
        position
    }

    fn next(&self, position: usize) -> usize {
        if position >= self.text.len() {
            return self.text.len();
        }
        let mut position = position + 1;
        if position < self.text.len()
            && is_low_surrogate(self.text[position])
            && is_high_surrogate(self.text[position - 1])
        {
            position += 1;
        }
        position
    }

    fn snap(&self, position: usize) -> usize {
        if position > 0
            && position < self.text.len()
            && is_low_surrogate(self.text[position])
            && is_high_surrogate(self.text[position - 1])
        {
            position - 1
        } else {
            position
        }
    }

    fn word_left(&self, mut position: usize) -> usize {
        while position > 0 && is_space(self.text[position - 1]) {
            position -= 1;
        }
        while position > 0 && !is_space(self.text[position - 1]) {
            position -= 1;
        }
        position
    }

    fn word_right(&self, mut position: usize) -> usize {
        let len = self.text.len();
        while position < len && !is_space(self.text[position]) {
            position += 1;
        }
        while position < len && is_space(self.text[position]) {
            position += 1;
        }
        position
    }
}

fn is_space(unit: u16) -> bool {
    unit == b' ' as u16 || unit == 0x3000 || unit == 0x00a0
}

fn is_high_surrogate(unit: u16) -> bool {
    (0xd800..0xdc00).contains(&unit)
}

fn is_low_surrogate(unit: u16) -> bool {
    (0xdc00..0xe000).contains(&unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(text: &str) -> TextField {
        let mut field = TextField::default();
        field.set_text(text);
        field
    }

    #[test]
    fn typing_replaces_the_selection() {
        let mut f = field("annual report");
        f.select_all();
        assert!(f.insert("invoice"));
        assert_eq!(f.text(), "invoice");
        assert_eq!(f.caret(), 7);
        f.move_left(true, true);
        assert_eq!(f.selected_text(), "invoice");
        f.insert("bill\r\nto");
        assert_eq!(f.text(), "bill  to");
    }

    #[test]
    fn word_motion_and_deletion() {
        let mut f = field("ext:pdf  annual report");
        f.backspace(true);
        assert_eq!(f.text(), "ext:pdf  annual ");
        f.home(false);
        f.move_right(false, true);
        assert_eq!(f.caret(), 9);
        f.delete(true);
        assert_eq!(f.text(), "ext:pdf  ");
        f.move_left(true, false);
        assert_eq!(f.selection(), (8, 9));
        f.move_left(false, false);
        assert_eq!(f.caret(), 8);
    }

    #[test]
    fn surrogate_pairs_are_never_split() {
        let mut f = field("a😀b");
        assert_eq!(f.wide().len(), 4);
        f.move_left(false, false);
        f.move_left(false, false);
        assert_eq!(f.caret(), 1);
        f.move_to(2, false);
        assert_eq!(f.caret(), 1);
        f.delete(false);
        assert_eq!(f.text(), "ab");
    }

    #[test]
    fn double_click_selects_a_word() {
        let mut f = field("annual report 2025");
        f.select_word_at(9);
        assert_eq!(f.selected_text(), "report");
        assert!(f.backspace(false));
        assert_eq!(f.text(), "annual  2025");
    }
}
