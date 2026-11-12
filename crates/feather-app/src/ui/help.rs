//! The Help window: a read-only reference for search syntax and shortcuts.

use std::path::Path;

use windows_sys::Win32::Foundation::HWND;

use super::form::{Action, ButtonStyle, Form, FormDialog, FormWindow, Id, Look};

const CLOSE: Id = "close";

/// One block of help: a heading followed by example rows or a paragraph.
enum Block {
    Heading(&'static str),
    Rows(&'static [(&'static str, &'static str)]),
    Text(&'static str),
}

const SYNTAX: &[(&str, &str)] = &[
    ("report", "Names containing \u{201c}report\u{201d}"),
    ("report budget", "Both words, in any order"),
    ("\"annual report\"", "The exact phrase"),
    ("!draft", "Leave out names containing \u{201c}draft\u{201d}"),
    ("*.pdf", "Wildcards: * is any text, ? is one character"),
    ("ext:pdf;docx", "Only these extensions"),
    ("type:folder", "Only folders (type:file for files)"),
    (
        "size:>10mb",
        "Size limits; also >=, <=, <, =, and ranges like 1kb..2gb",
    ),
    (
        "match:starts report",
        "Words must start or end the name: match:starts, match:ends",
    ),
    (
        "dm:today",
        "Modified today; also yesterday, thisweek, lastweek, thismonth, lastmonth, thisyear, lastyear",
    ),
    (
        "dm:last7days",
        "Modified in the last 7 days, today included; also 24hours, 30min, 2weeks",
    ),
    (
        "dm:2026-11",
        "Modified in that year, month, or day: 2026, 2026-11, 2026-11-11",
    ),
    (
        "dm:2026-01..2026-03",
        "A range, both ends included; leave a side open: dm:2026-06.., dm:..2025",
    ),
    (
        "dm:>=2026-06",
        "From June on; > is after it, < before it, <= up to its end. date: works too",
    ),
];

const KEYS: &[(&str, &str)] = &[
    ("Ctrl+F", "Go to the search box"),
    ("Esc", "Clear the search"),
    ("Up, Down", "Move through the results"),
    ("Enter", "Open the selected result"),
    ("Ctrl+Enter", "Show the selected result in its folder"),
    ("Ctrl+C", "Copy the selected result's path"),
    ("Column header", "Sort by it; click again to reverse"),
];

const BLOCKS: &[Block] = &[
    Block::Heading("Search syntax"),
    Block::Rows(SYNTAX),
    Block::Heading("Keyboard and mouse"),
    Block::Rows(KEYS),
    Block::Heading("Filters"),
    Block::Text(
        "Filters narrows results by kind, extension, size, date modified, and where typed \
         words must appear on top of what you type. The categories in the sidebar are quick \
         presets for the same filters. Use Reset in the Filters window to clear them.",
    ),
    Block::Heading("Dates"),
    Block::Text(
        "Dates are read in this computer's time zone, and weeks start on the first day of \
         the week set in Windows. Folders are matched by their own date modified, which \
         changes when items are added to or removed from them. Items whose date is unknown \
         never match a date filter. Several dm: filters must all match.",
    ),
    Block::Heading("Settings"),
    Block::Text(
        "Settings controls the theme, the result limit, which drives and folders are \
         searched, what double-click does, and what Feather remembers between sessions. \
         They are saved in your user profile.",
    ),
    Block::Heading("Not supported yet"),
    Block::Text(
        "Searching inside files and regular expressions are not available. Dates created \
         and accessed are not indexed, and !dm: is not supported.",
    ),
];

struct HelpDialog;

impl FormDialog for HelpDialog {
    fn on_button(&mut self, _form: &mut Form, _id: Id) -> Action {
        Action::Cancel
    }
}

fn build() -> Form {
    let mut form = Form::new();
    form.heading(&format!("Feather {}", env!("CARGO_PKG_VERSION")));
    form.paragraph("Type part of a name to find it. Everything below can be combined.");
    for block in BLOCKS {
        match block {
            Block::Heading(text) => form.heading(text),
            Block::Rows(rows) => form.syntax(rows),
            Block::Text(text) => form.paragraph(text),
        }
    }
    form.button(CLOSE, "Close", ButtonStyle::Primary);
    form
}

fn open(parent: HWND, look: Look) -> Result<FormWindow, String> {
    FormWindow::open(
        parent,
        "Help",
        640,
        600,
        look,
        build(),
        Box::new(HelpDialog),
    )
}

pub fn show(parent: HWND, look: Look) -> Result<(), String> {
    open(parent, look)?.run_modal();
    Ok(())
}

pub fn capture(parent: HWND, look: Look, path: &Path) -> Result<(), String> {
    open(parent, look)?.capture(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_documents_the_real_syntax() {
        let mut text = String::new();
        for block in BLOCKS {
            match block {
                Block::Heading(heading) => text.push_str(heading),
                Block::Rows(rows) => {
                    for (code, description) in rows.iter() {
                        text.push_str(code);
                        text.push_str(description);
                    }
                }
                Block::Text(body) => text.push_str(body),
            }
        }
        for expected in [
            "ext:pdf;docx",
            "type:folder",
            "size:>10mb",
            "match:starts",
            "Ctrl+F",
            "Filters",
            "Settings",
        ] {
            assert!(text.contains(expected), "missing {expected}");
        }
    }
}
