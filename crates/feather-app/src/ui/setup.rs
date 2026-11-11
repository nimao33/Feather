//! The first-run guide: what Feather is, where to search, how it looks, how
//! to call it up, and a few search tips, one step at a time.
//!
//! It opens over the main window the first time Feather runs (until
//! `setup_done` is saved). Finishing starts the first index; skipping
//! leaves everything for Settings.

use std::any::Any;
use std::path::Path;

use windows_sys::Win32::Foundation::HWND;

use super::form::{option, Action, ButtonStyle, Choice, Form, FormDialog, FormWindow, Id, Look};
use super::glyphs::PageIcon;
use super::hotkey;
use super::indexer;
use super::theme;
use crate::settings::{Accent, DarkMode, Material};

const PLACES: Id = "places";
const MORE: Id = "more";
const THEME: Id = "theme";
const ACCENT: Id = "accent";
const HOTKEY: Id = "hotkey";
const START_AT_LOGIN: Id = "start_at_login";
const CLOSE_TO_TRAY: Id = "close_to_tray";
const NEXT: Id = "next";
const BACK: Id = "back";
const SKIP: Id = "skip";

const STEP_WELCOME: usize = 0;
const STEP_PLACES: usize = 1;
const STEP_SHORTCUT: usize = 3;
const STEPS: usize = 5;

const THEMES: [DarkMode; 3] = [DarkMode::FollowSystem, DarkMode::Light, DarkMode::Full];

/// What the person chose.
#[derive(Clone, Debug, PartialEq)]
pub struct SetupChoice {
    /// Skipped: only `setup_done` is saved; nothing else changes.
    pub skipped: bool,
    pub roots: Vec<String>,
    pub dark_mode: DarkMode,
    pub accent: Accent,
    /// Normalized, or [`hotkey::OFF`].
    pub hotkey: String,
    pub start_at_login: bool,
    pub close_to_tray: bool,
}

/// What the guide starts from.
#[derive(Clone, Debug)]
pub struct SetupStart {
    pub hotkey: String,
    pub start_at_login: bool,
    pub close_to_tray: bool,
}

struct SetupDialog {
    places: Vec<String>,
    material: Material,
    result: Option<SetupChoice>,
}

impl SetupDialog {
    fn roots(&self, form: &Form) -> Vec<String> {
        let mut roots: Vec<String> = self
            .places
            .iter()
            .zip(form.checked(PLACES))
            .filter(|(_, on)| *on)
            .map(|(place, _)| place.clone())
            .collect();
        roots.extend(form.path_items(MORE));
        roots
    }

    fn dark_mode(form: &Form) -> DarkMode {
        THEMES[form.selected(THEME).min(THEMES.len() - 1)]
    }

    fn accent(form: &Form) -> Accent {
        Accent::ALL[form.selected(ACCENT).min(Accent::ALL.len() - 1)]
    }

    /// The step's problem, if it can't be left forward yet.
    fn check(&self, form: &Form, step: usize) -> Result<(), String> {
        match step {
            STEP_PLACES if self.roots(form).is_empty() => {
                Err("Choose at least one place to search, or skip for now.".into())
            }
            STEP_SHORTCUT => hotkey::normalize(&form.text(HOTKEY)).map(|_| ()),
            _ => Ok(()),
        }
    }

    fn finish(&mut self, form: &mut Form) -> Action {
        for step in 0..STEPS {
            if let Err(error) = self.check(form, step) {
                go_to(form, step);
                form.set_error(Some(error));
                return Action::Stay;
            }
        }
        let hotkey = hotkey::normalize(&form.text(HOTKEY))
            .ok()
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| hotkey::OFF.to_string());
        self.result = Some(SetupChoice {
            skipped: false,
            roots: self.roots(form),
            dark_mode: Self::dark_mode(form),
            accent: Self::accent(form),
            hotkey,
            start_at_login: form.is_on(START_AT_LOGIN),
            close_to_tray: form.is_on(CLOSE_TO_TRAY),
        });
        Action::Accept
    }
}

/// Shows `step` with the footer it needs: no Back on the first step, and
/// Start searching instead of Next on the last.
fn go_to(form: &mut Form, step: usize) {
    form.set_page(step);
    label_footer(form, step);
}

fn label_footer(form: &mut Form, step: usize) {
    form.set_button_hidden(BACK, step == STEP_WELCOME);
    let last = step + 1 >= STEPS;
    form.set_button_label(NEXT, if last { "Start searching" } else { "Next" });
    form.set_error(None);
}

impl FormDialog for SetupDialog {
    fn on_button(&mut self, form: &mut Form, id: Id) -> Action {
        let step = form.current_page();
        match id {
            SKIP => {
                self.result = Some(SetupChoice {
                    skipped: true,
                    roots: Vec::new(),
                    dark_mode: Self::dark_mode(form),
                    accent: Self::accent(form),
                    hotkey: String::new(),
                    start_at_login: false,
                    close_to_tray: false,
                });
                Action::Accept
            }
            BACK => {
                go_to(form, step.saturating_sub(1));
                Action::Stay
            }
            _ if step + 1 >= STEPS => self.finish(form),
            _ => {
                if let Err(error) = self.check(form, step) {
                    form.set_error(Some(error));
                    return Action::Stay;
                }
                go_to(form, step + 1);
                Action::Stay
            }
        }
    }

    fn on_page(&mut self, form: &mut Form, page: usize) {
        label_footer(form, page);
    }

    fn look(&self, form: &Form) -> Option<Look> {
        Some(Look {
            dark: theme::resolve_dark(Self::dark_mode(form)),
            accent: Self::accent(form),
            material: self.material,
        })
    }

    fn take_outcome(&mut self) -> Option<Box<dyn Any>> {
        self.result
            .take()
            .map(|choice| Box::new(choice) as Box<dyn Any>)
    }
}

/// The places offered as check boxes: the profile folder first, checked,
/// then every fixed drive, unchecked.
fn places() -> Vec<(String, String, bool)> {
    let mut places = Vec::new();
    if let Some(profile) = indexer::profile_folder() {
        places.push((profile.clone(), format!("Your files ({profile})"), true));
    }
    for drive in indexer::fixed_drives() {
        places.push((drive.clone(), format!("The whole {drive} drive"), false));
    }
    places
}

fn build(places: &[(String, String, bool)], look: Look, start: &SetupStart) -> Form {
    let mut form = Form::new();

    form.page("Welcome", PageIcon::Window);
    form.heading("Find any file as you type");
    form.paragraph(
        "Feather finds files and folders by name the moment you type. It reads \
         the names of everything in the places you choose once, which takes a \
         few seconds, and then keeps them up to date by itself.",
    );
    form.heading("Why it feels different");
    form.paragraph(
        "Instant results. Type part of a name and the list fills in as you type, \
         with the matching part highlighted, even across millions of files.",
    );
    form.paragraph(
        "Light on your PC. The list of names lives in files Windows can page \
         out, so Feather uses only a few dozen megabytes while it waits.",
    );
    form.paragraph(
        "Private. Nothing leaves this computer unless you turn on sharing in \
         Settings.",
    );
    form.paragraph("This takes under a minute. Press Next to begin.");

    form.page("Where to search", PageIcon::Places);
    form.heading("Choose the places to read");
    form.paragraph(
        "Whole NTFS drives are read straight from the drive's file table, which \
         is very fast; Windows asks once for permission to do that.",
    );
    form.checks(
        PLACES,
        "",
        places
            .iter()
            .map(|(_, label, on)| (label.clone(), *on))
            .collect(),
    );
    form.paths(
        MORE,
        "Other folders",
        Some("A network folder, a second drive, or anywhere else."),
        Vec::new(),
    );

    form.page("How it looks", PageIcon::Appearance);
    form.heading("Theme and accent");
    form.paragraph("You can change these, and make your own theme, in Settings any time.");
    form.segments(
        THEME,
        "Theme",
        None,
        vec![option("Match Windows"), option("Light"), option("Dark")],
        0,
    );
    form.segments(
        ACCENT,
        "Accent color",
        None,
        Accent::ALL
            .iter()
            .map(|choice| Choice {
                label: choice.label().to_string(),
                swatch: Some(theme::accent_color(*choice, look.dark)),
            })
            .collect(),
        Accent::ALL
            .iter()
            .position(|choice| *choice == look.accent)
            .unwrap_or(0),
    );

    form.page("Shortcut", PageIcon::Shortcut);
    form.heading("Open Feather from anywhere");
    let shortcut = if start.hotkey.eq_ignore_ascii_case(hotkey::OFF) {
        ""
    } else {
        start.hotkey.as_str()
    };
    form.shortcut(
        HOTKEY,
        "Shortcut",
        Some("Press it in any app to bring Feather up, ready to type. Click the box and press keys to change it."),
        shortcut,
        "None",
    );
    form.heading("In the background");
    form.switch(
        START_AT_LOGIN,
        "Start when you sign in",
        Some("So the shortcut works right away and the index stays current."),
        start.start_at_login,
    );
    form.switch(
        CLOSE_TO_TRAY,
        "Keep running in the tray when closed",
        Some("Closing the window hides Feather next to the clock instead of quitting."),
        start.close_to_tray,
    );

    form.page("Ready", PageIcon::Sidebar);
    form.heading("Search tips");
    form.paragraph(
        "Press Start searching and Feather reads the places you chose. You can \
         search while it works. A few things worth knowing:",
    );
    form.syntax(&[
        ("report pdf", "names with both words, in any order"),
        ("\"annual report\"", "the exact phrase"),
        ("!temp", "leave out names with temp"),
        ("*.mp4", "wildcards"),
        ("docs\\report", "report inside a folder ending in docs"),
        ("ext:jpg;png", "only these file types"),
        ("size:>100mb", "by size"),
    ]);
    form.paragraph(
        "Double-click a result to open it, or right-click for the usual Explorer \
         menu. The sidebar narrows results to documents, images, video and more. \
         Sharing folders to your phone, themes and everything else are in Settings.",
    );

    form.button(SKIP, "Skip for now", ButtonStyle::Quiet);
    form.button(BACK, "Back", ButtonStyle::Secondary);
    form.button(NEXT, "Next", ButtonStyle::Primary);
    label_footer(&mut form, STEP_WELCOME);
    form.settle();
    form
}

fn open(parent: HWND, look: Look, start: &SetupStart, step: usize) -> Result<FormWindow, String> {
    let places = places();
    let mut form = build(&places, look, start);
    go_to(&mut form, step.min(STEPS - 1));
    form.settle();
    let dialog = SetupDialog {
        places: places.into_iter().map(|(path, _, _)| path).collect(),
        material: look.material,
        result: None,
    };
    FormWindow::open(
        parent,
        "Get started",
        820,
        600,
        look,
        form,
        Box::new(dialog),
    )
}

/// Shows the guide. `None` means it was closed without a choice.
pub fn show(parent: HWND, look: Look, start: &SetupStart) -> Result<Option<SetupChoice>, String> {
    let outcome = open(parent, look, start, STEP_WELCOME)?.run_modal();
    Ok(outcome.and_then(|boxed| boxed.downcast::<SetupChoice>().ok().map(|boxed| *boxed)))
}

/// A screenshot of one step, for the UI test.
pub fn capture(
    parent: HWND,
    look: Look,
    start: &SetupStart,
    step: usize,
    path: &Path,
) -> Result<(), String> {
    open(parent, look, start, step)?.capture(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_folder_is_offered_first_and_checked() {
        let places = places();
        if let Some(profile) = indexer::profile_folder() {
            assert_eq!(places[0].0, profile);
            assert!(places[0].2);
        }
        assert!(places.iter().skip(1).all(|(_, _, on)| !on));
    }

    #[test]
    fn the_footer_follows_the_step() {
        let start = SetupStart {
            hotkey: hotkey::DEFAULT.to_string(),
            start_at_login: true,
            close_to_tray: true,
        };
        let look = Look {
            dark: true,
            accent: Accent::ALL[0],
            material: Material::Solid,
        };
        let mut form = build(&places(), look, &start);
        assert_eq!(form.current_page(), STEP_WELCOME);
        go_to(&mut form, STEPS - 1);
        assert_eq!(form.current_page(), STEPS - 1);
    }
}
