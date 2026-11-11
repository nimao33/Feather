//! The global shortcut that brings Feather forward from anywhere.
//!
//! A shortcut is any set of keys, stored as text such as `Ctrl + Space`,
//! `Ctrl + Shift + Right Shift`, or `Caps Lock + F`; `None` turns it off.
//! `Ctrl`, `Alt`, `Shift`, and `Win` mean either side; `Left Shift` and
//! `Right Shift` mean that side only.
//!
//! Shortcuts Windows can register itself (side-less modifiers plus one key)
//! use `RegisterHotKey`, which also works over admin windows. Anything else
//! goes through the keyboard hook in `keyhook.rs`.

use std::fmt;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey};

const MOD_ALT: u32 = 0x1;
const MOD_CONTROL: u32 = 0x2;
const MOD_SHIFT: u32 = 0x4;
const MOD_WIN: u32 = 0x8;
const MOD_NOREPEAT: u32 = 0x4000;

pub const DEFAULT: &str = "Ctrl + Space";
pub const OFF: &str = "None";

/// A modifier on either side of the keyboard.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Modifier {
    Ctrl,
    Alt,
    Shift,
    Win,
}

impl Modifier {
    const ALL: [Modifier; 4] = [
        Modifier::Ctrl,
        Modifier::Alt,
        Modifier::Shift,
        Modifier::Win,
    ];

    /// The left and right virtual keys.
    pub fn sides(self) -> (u32, u32) {
        match self {
            Modifier::Ctrl => (0xa2, 0xa3),
            Modifier::Alt => (0xa4, 0xa5),
            Modifier::Shift => (0xa0, 0xa1),
            Modifier::Win => (0x5b, 0x5c),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Modifier::Ctrl => "Ctrl",
            Modifier::Alt => "Alt",
            Modifier::Shift => "Shift",
            Modifier::Win => "Win",
        }
    }

    fn flag(self) -> u32 {
        match self {
            Modifier::Ctrl => MOD_CONTROL,
            Modifier::Alt => MOD_ALT,
            Modifier::Shift => MOD_SHIFT,
            Modifier::Win => MOD_WIN,
        }
    }

    /// The modifier a side-specific key belongs to.
    pub fn of(key: u32) -> Option<Modifier> {
        Modifier::ALL.into_iter().find(|modifier| {
            let (left, right) = modifier.sides();
            key == left || key == right
        })
    }
}

/// One key of a shortcut.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Key {
    /// Either side.
    Either(Modifier),
    /// A specific virtual key, including one side of a modifier.
    Exact(u32),
}

impl Key {
    fn is_modifier(self) -> bool {
        match self {
            Key::Either(_) => true,
            Key::Exact(vk) => Modifier::of(vk).is_some(),
        }
    }

    /// Sorting: side-less modifiers, then sided ones, then other keys.
    fn rank(self) -> (u8, u32) {
        match self {
            Key::Either(modifier) => (0, modifier as u32),
            Key::Exact(vk) if Modifier::of(vk).is_some() => (1, vk),
            Key::Exact(vk) => (2, vk),
        }
    }
}

/// Named keys. Letters, digits, number-pad digits, and F1 to F24 are
/// handled separately; anything else is written `Key 0x..`.
const NAMED: &[(u32, &str)] = &[
    (0x03, "Break"),
    (0x08, "Backspace"),
    (0x09, "Tab"),
    (0x0c, "Clear"),
    (0x0d, "Enter"),
    (0x13, "Pause"),
    (0x14, "Caps Lock"),
    (0x1b, "Esc"),
    (0x20, "Space"),
    (0x21, "Page Up"),
    (0x22, "Page Down"),
    (0x23, "End"),
    (0x24, "Home"),
    (0x25, "Left"),
    (0x26, "Up"),
    (0x27, "Right"),
    (0x28, "Down"),
    (0x2c, "Print Screen"),
    (0x2d, "Insert"),
    (0x2e, "Delete"),
    (0x5b, "Left Win"),
    (0x5c, "Right Win"),
    (0x5d, "Menu"),
    (0x6a, "Num *"),
    (0x6b, "Num Plus"),
    (0x6d, "Num -"),
    (0x6e, "Num ."),
    (0x6f, "Num /"),
    (0x90, "Num Lock"),
    (0x91, "Scroll Lock"),
    (0xa0, "Left Shift"),
    (0xa1, "Right Shift"),
    (0xa2, "Left Ctrl"),
    (0xa3, "Right Ctrl"),
    (0xa4, "Left Alt"),
    (0xa5, "Right Alt"),
    (0xa6, "Browser Back"),
    (0xa7, "Browser Forward"),
    (0xad, "Mute"),
    (0xae, "Volume Down"),
    (0xaf, "Volume Up"),
    (0xb0, "Next Track"),
    (0xb1, "Previous Track"),
    (0xb3, "Play/Pause"),
    (0xba, ";"),
    (0xbb, "="),
    (0xbc, ","),
    (0xbd, "-"),
    (0xbe, "."),
    (0xbf, "/"),
    (0xc0, "`"),
    (0xdb, "["),
    (0xdc, "\\"),
    (0xdd, "]"),
    (0xde, "'"),
    (0xe2, "<"),
];

pub fn key_name(vk: u32) -> String {
    match vk {
        0x30..=0x39 | 0x41..=0x5a => (vk as u8 as char).to_string(),
        0x60..=0x69 => format!("Num {}", vk - 0x60),
        0x70..=0x87 => format!("F{}", vk - 0x6f),
        _ => NAMED
            .iter()
            .find(|(code, _)| *code == vk)
            .map(|(_, name)| (*name).to_string())
            .unwrap_or_else(|| format!("Key 0x{vk:02X}")),
    }
}

fn key_code(name: &str) -> Option<u32> {
    let upper = name.trim().to_ascii_uppercase();
    if let [byte] = upper.as_bytes() {
        if byte.is_ascii_alphanumeric() {
            return Some(*byte as u32);
        }
    }
    let compact: String = upper.chars().filter(|c| !c.is_whitespace()).collect();
    if let Some(digit) = compact
        .strip_prefix("NUM")
        .and_then(|rest| rest.parse::<u32>().ok())
    {
        if digit <= 9 {
            return Some(0x60 + digit);
        }
    }
    if let Some(number) = compact
        .strip_prefix('F')
        .and_then(|rest| rest.parse::<u32>().ok())
    {
        if (1..=24).contains(&number) {
            return Some(0x6f + number);
        }
    }
    if let Some(hex) = compact.strip_prefix("KEY0X") {
        return u32::from_str_radix(hex, 16)
            .ok()
            .filter(|vk| (1..=0xfe).contains(vk));
    }
    let squash = |text: &str| -> String {
        text.chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase()
    };
    let wanted = squash(name);
    NAMED
        .iter()
        .find(|(_, candidate)| squash(candidate) == wanted)
        .map(|(code, _)| *code)
        .or(match wanted.as_str() {
            "escape" => Some(0x1b),
            "return" => Some(0x0d),
            "capslock" | "caps" => Some(0x14),
            "pgup" => Some(0x21),
            "pgdn" => Some(0x22),
            "del" => Some(0x2e),
            "ins" => Some(0x2d),
            "apps" => Some(0x5d),
            _ => None,
        })
}

/// Keys that type or move the caret: on their own (or with Shift) they
/// would stop working everywhere else.
fn is_typing_key(vk: u32) -> bool {
    matches!(vk,
        0x08 | 0x09 | 0x0d | 0x1b | 0x20..=0x28 | 0x2e | 0x30..=0x39 | 0x41..=0x5a
        | 0x60..=0x6f | 0xba..=0xc0 | 0xdb..=0xde | 0xe2)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Shortcut {
    keys: Vec<Key>,
}

impl Shortcut {
    fn new(mut keys: Vec<Key>) -> Self {
        keys.sort_by_key(|key| key.rank());
        keys.dedup();
        Shortcut { keys }
    }

    pub fn keys(&self) -> &[Key] {
        &self.keys
    }

    /// True when every key is a modifier: these fire when released, so
    /// `Ctrl + Shift` does not get in the way of `Ctrl + Shift + T`.
    pub fn modifiers_only(&self) -> bool {
        self.keys.iter().all(|key| key.is_modifier())
    }

    /// Parses stored text. `Ok(None)` means the shortcut is off.
    pub fn parse(text: &str) -> Result<Option<Shortcut>, String> {
        let text = text.trim();
        if text.is_empty() || text.eq_ignore_ascii_case(OFF) || text.eq_ignore_ascii_case("off") {
            return Ok(None);
        }
        let mut keys = Vec::new();
        for part in split_parts(text) {
            let either = Modifier::ALL.into_iter().find(|modifier| {
                part.eq_ignore_ascii_case(modifier.name())
                    || (*modifier == Modifier::Ctrl && part.eq_ignore_ascii_case("control"))
                    || (*modifier == Modifier::Win && part.eq_ignore_ascii_case("windows"))
            });
            match either {
                Some(modifier) => keys.push(Key::Either(modifier)),
                None => match key_code(&part) {
                    Some(vk) => keys.push(Key::Exact(vk)),
                    None => return Err(format!("{part} is not a key Feather knows")),
                },
            }
        }
        let shortcut = Shortcut::new(keys);
        shortcut.check()?;
        Ok(Some(shortcut))
    }

    /// The shortcut for the keys held down together while recording.
    /// Holding both sides of a modifier records one side-less and one
    /// right-side key, so `Shift` and `Right Shift` both must be down.
    pub fn from_held(held: &[u32]) -> Option<Shortcut> {
        let mut keys = Vec::new();
        for &vk in held {
            match Modifier::of(vk) {
                Some(modifier) => {
                    let (left, right) = modifier.sides();
                    let both = held.contains(&left) && held.contains(&right);
                    if vk == left {
                        keys.push(Key::Either(modifier));
                    } else if both {
                        keys.push(Key::Exact(right));
                    } else {
                        keys.push(Key::Exact(vk));
                    }
                }
                None => keys.push(Key::Exact(vk)),
            }
        }
        if keys.is_empty() {
            return None;
        }
        Some(Shortcut::new(keys))
    }

    /// Refuses shortcuts that would take over ordinary typing.
    pub fn check(&self) -> Result<(), String> {
        let has_command = self.keys.iter().any(|key| {
            matches!(key, Key::Either(Modifier::Ctrl | Modifier::Alt | Modifier::Win))
                || matches!(key, Key::Exact(vk) if matches!(Modifier::of(*vk), Some(Modifier::Ctrl | Modifier::Alt | Modifier::Win)))
        });
        let others: Vec<u32> = self
            .keys
            .iter()
            .filter_map(|key| match key {
                Key::Exact(vk) if Modifier::of(*vk).is_none() => Some(*vk),
                _ => None,
            })
            .collect();
        let typing_alone = others.len() == 1 && is_typing_key(others[0]);
        if typing_alone && !has_command {
            return Err(format!(
                "{self} would stop that key from typing. Add Ctrl, Alt, or Win."
            ));
        }
        Ok(())
    }

    /// The `RegisterHotKey` form, when Windows can watch for it directly.
    pub fn registrable(&self) -> Option<(u32, u32)> {
        let mut flags = 0;
        let mut key = None;
        for item in &self.keys {
            match item {
                Key::Either(modifier) => flags |= modifier.flag(),
                Key::Exact(vk) if Modifier::of(*vk).is_none() && key.is_none() => key = Some(*vk),
                _ => return None,
            }
        }
        let key = key?;
        (flags != 0 || (0x70..=0x87).contains(&key)).then_some((flags, key))
    }

    /// True when exactly these keys are down. `down` holds side-specific
    /// virtual keys.
    pub fn matches(&self, down: &[u32]) -> bool {
        let mut used = vec![false; down.len()];
        for key in &self.keys {
            if let Key::Exact(vk) = key {
                match down.iter().position(|held| held == vk) {
                    Some(at) => used[at] = true,
                    None => return false,
                }
            }
        }
        for key in &self.keys {
            if let Key::Either(modifier) = key {
                let (left, right) = modifier.sides();
                let mut found = false;
                for (at, held) in down.iter().enumerate() {
                    if (*held == left || *held == right) && !used[at] {
                        used[at] = true;
                        found = true;
                    }
                }
                // Both sides may be held for a side-less modifier, but a
                // side already claimed by an exact key does not count.
                if !found {
                    return false;
                }
            }
        }
        used.iter().all(|used| *used)
    }
}

/// Splits on `+`, keeping `+` itself when it is the key (as in `Ctrl + +`).
fn split_parts(text: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    for raw in text.split('+') {
        let part = raw.trim();
        if part.is_empty() {
            if parts.last().map(|last| last != "+").unwrap_or(true) {
                parts.push("+".into());
            }
            continue;
        }
        parts.push(part.to_string());
    }
    parts.retain(|part| part != "+");
    parts
}

impl fmt::Display for Shortcut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<String> = self.names();
        f.write_str(&names.join(" + "))
    }
}

impl Shortcut {
    /// Each key's label, in order, for drawing as key caps.
    pub fn names(&self) -> Vec<String> {
        self.keys
            .iter()
            .map(|key| match key {
                Key::Either(modifier) => modifier.name().to_string(),
                Key::Exact(vk) => key_name(*vk),
            })
            .collect()
    }
}

/// Labels for keys held right now, as the recorder shows them.
pub fn held_names(held: &[u32]) -> Vec<String> {
    Shortcut::from_held(held)
        .map(|shortcut| shortcut.names())
        .unwrap_or_default()
}

/// Text to store for a setting: the canonical spelling, or `None`.
pub fn normalize(text: &str) -> Result<String, String> {
    Ok(match Shortcut::parse(text)? {
        Some(shortcut) => shortcut.to_string(),
        None => OFF.to_string(),
    })
}

/// Registers `shortcut` for `hwnd`, which then receives `WM_HOTKEY`. Fails
/// when another app already owns it.
pub fn register(hwnd: HWND, id: i32, modifiers: u32, key: u32) -> bool {
    unsafe { RegisterHotKey(hwnd, id, modifiers | MOD_NOREPEAT, key) != 0 }
}

pub fn unregister(hwnd: HWND, id: i32) {
    unsafe {
        UnregisterHotKey(hwnd, id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LCTRL: u32 = 0xa2;
    const LSHIFT: u32 = 0xa0;
    const RSHIFT: u32 = 0xa1;
    const RCTRL: u32 = 0xa3;
    const SPACE: u32 = 0x20;

    #[test]
    fn shortcuts_round_trip_in_a_fixed_order() {
        assert_eq!(
            normalize("shift + alt + ctrl + f").unwrap(),
            "Ctrl + Alt + Shift + F"
        );
        assert_eq!(normalize("ctrl+space").unwrap(), DEFAULT);
        assert_eq!(normalize("Ctrl+Space").unwrap(), DEFAULT);
        assert_eq!(normalize("Win+`").unwrap(), "Win + `");
        assert_eq!(normalize("F8").unwrap(), "F8");
        assert_eq!(normalize("space + caps lock").unwrap(), "Caps Lock + Space");
        assert_eq!(
            normalize("Ctrl + Shift + Right Shift").unwrap(),
            "Ctrl + Shift + Right Shift"
        );
        assert_eq!(normalize("Right Ctrl").unwrap(), "Right Ctrl");
        assert_eq!(normalize("Ctrl + Key 0xE8").unwrap(), "Ctrl + Key 0xE8");
    }

    #[test]
    fn none_turns_the_shortcut_off() {
        assert_eq!(Shortcut::parse("None").unwrap(), None);
        assert_eq!(Shortcut::parse("").unwrap(), None);
        assert_eq!(normalize("off").unwrap(), OFF);
    }

    #[test]
    fn shortcuts_that_block_typing_are_refused() {
        assert!(Shortcut::parse("A").is_err());
        assert!(Shortcut::parse("Shift+A").is_err());
        assert!(Shortcut::parse("Space").is_err());
        assert!(Shortcut::parse("Ctrl+Nope").is_err());
        assert!(Shortcut::parse("Right Shift").is_ok());
        assert!(Shortcut::parse("Caps Lock").is_ok());
        assert!(Shortcut::parse("Right Alt + A").is_ok());
    }

    #[test]
    fn held_keys_become_shortcuts() {
        let shortcut = Shortcut::from_held(&[LCTRL, LSHIFT, RSHIFT]).unwrap();
        assert_eq!(shortcut.to_string(), "Ctrl + Shift + Right Shift");
        let right = Shortcut::from_held(&[RCTRL]).unwrap();
        assert_eq!(right.to_string(), "Right Ctrl");
        assert_eq!(held_names(&[LCTRL, SPACE]), vec!["Ctrl", "Space"]);
    }

    #[test]
    fn matching_needs_exactly_the_keys() {
        let combo = Shortcut::parse("Ctrl + Shift + Right Shift")
            .unwrap()
            .unwrap();
        assert!(combo.matches(&[LCTRL, LSHIFT, RSHIFT]));
        assert!(!combo.matches(&[LCTRL, RSHIFT]));
        assert!(!combo.matches(&[LCTRL, LSHIFT, RSHIFT, SPACE]));
        assert!(combo.modifiers_only());

        let space = Shortcut::parse(DEFAULT).unwrap().unwrap();
        assert!(space.matches(&[RCTRL, SPACE]));
        assert!(space.matches(&[LCTRL, RCTRL, SPACE]));
        assert!(!space.matches(&[LCTRL, LSHIFT, SPACE]));
        assert!(!space.modifiers_only());
    }

    #[test]
    fn only_plain_combinations_use_register_hot_key() {
        let space = Shortcut::parse(DEFAULT).unwrap().unwrap();
        assert_eq!(space.registrable(), Some((MOD_CONTROL, SPACE)));
        let sided = Shortcut::parse("Right Ctrl + Space").unwrap().unwrap();
        assert_eq!(sided.registrable(), None);
        let chord = Shortcut::parse("Caps Lock + Space").unwrap().unwrap();
        assert_eq!(chord.registrable(), None);
    }
}
