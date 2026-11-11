//! The first-run setup window: where to search, and how Feather looks.
//!
//! It opens over the main window the first time Feather runs (until
//! `setup_done` is saved). Choosing places starts the first index; skipping
//! leaves everything for Settings.

use std::any::Any;
use std::path::Path;

use windows_sys::Win32::Foundation::HWND;

use super::form::{option, Action, ButtonStyle, Choice, Form, FormDialog, FormWindow, Id, Look};
use super::indexer;
use super::theme;
use crate::settings::{Accent, DarkMode, Material};

const PLACES: Id = "places";
const MORE: Id = "more";
const THEME: Id = "theme";
const ACCENT: Id = "accent";
const START: Id = "start";
const SKIP: Id = "skip";

const THEMES: [DarkMode; 3] = [DarkMode::FollowSystem, DarkMode::Light, DarkMode::Full];

/// What the person chose.
#[derive(Clone, Debug, PartialEq)]
pub struct SetupChoice {
    /// Empty when setup was skipped.
    pub roots: Vec<String>,
    pub dark_mode: DarkMode,
    pub accent: Accent,
}

struct SetupDialog {
    places: Vec<String>,
    material: Material,
    result: Option<SetupChoice>,
}

impl SetupDialog {
    fn choice(&self, form: &Form, roots: Vec<String>) -> SetupChoice {
        SetupChoice {
            roots,
            dark_mode: THEMES[form.selected(THEME).min(THEMES.len() - 1)],
            accent: Accent::ALL[form.selected(ACCENT).min(Accent::ALL.len() - 1)],
        }
    }
}

impl FormDialog for SetupDialog {
    fn on_button(&mut self, form: &mut Form, id: Id) -> Action {
        let roots = if id == START {
            let mut roots: Vec<String> = self
                .places
                .iter()
                .zip(form.checked(PLACES))
                .filter(|(_, on)| *on)
                .map(|(place, _)| place.clone())
                .collect();
            roots.extend(form.path_items(MORE));
            if roots.is_empty() {
                form.set_error(Some(
                    "Choose at least one place to search, or skip for now.".into(),
                ));
                return Action::Stay;
            }
            roots
        } else {
            Vec::new()
        };
        self.result = Some(self.choice(form, roots));
        Action::Accept
    }

    fn look(&self, form: &Form) -> Option<Look> {
        let choice = self.choice(form, Vec::new());
        Some(Look {
            dark: theme::resolve_dark(choice.dark_mode),
            accent: choice.accent,
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

fn build(places: &[(String, String, bool)], look: Look, accent: Accent) -> Form {
    let mut form = Form::new();
    form.heading("Welcome to Feather");
    form.paragraph(
        "Feather finds files and folders by name as you type. First it reads the \
         names of everything in the places you choose, which takes a few seconds, \
         and then keeps them up to date by itself. Nothing leaves this computer.",
    );
    form.heading("Where should Feather search?");
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
    form.heading("How should it look?");
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
            .position(|choice| *choice == accent)
            .unwrap_or(0),
    );
    form.button(SKIP, "Skip for now", ButtonStyle::Quiet);
    form.button(START, "Start searching", ButtonStyle::Primary);
    form.settle();
    form
}

fn open(parent: HWND, look: Look) -> Result<FormWindow, String> {
    let places = places();
    let form = build(&places, look, look.accent);
    let dialog = SetupDialog {
        places: places.into_iter().map(|(path, _, _)| path).collect(),
        material: look.material,
        result: None,
    };
    FormWindow::open(
        parent,
        "Set up Feather",
        620,
        720,
        look,
        form,
        Box::new(dialog),
    )
}

/// Shows the setup window. `None` means it was closed without a choice.
pub fn show(parent: HWND, look: Look) -> Result<Option<SetupChoice>, String> {
    let outcome = open(parent, look)?.run_modal();
    Ok(outcome.and_then(|boxed| boxed.downcast::<SetupChoice>().ok().map(|boxed| *boxed)))
}

pub fn capture(parent: HWND, look: Look, path: &Path) -> Result<(), String> {
    open(parent, look)?.capture(path)
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
}
