//! A low-level keyboard hook for shortcuts `RegisterHotKey` cannot express
//! (one side of a modifier, modifiers alone, several ordinary keys) and for
//! recording a new shortcut in Settings.
//!
//! The hook runs on its own thread with nothing else to do, so a busy UI
//! thread never delays anyone's typing. It only posts messages:
//! `WM_SHORTCUT` to the main window when the shortcut is pressed, and
//! `WM_RECORD_KEY` to the Settings window while recording.

use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PostMessageW, SetWindowsHookExW, KBDLLHOOKSTRUCT, MSG,
    WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_SYSKEYDOWN,
};

use super::hotkey::{Key, Modifier, Shortcut};

/// Posted to the main window when the shortcut is pressed.
pub const WM_SHORTCUT: u32 = WM_APP + 0x51;
/// Posted to the recording window: `wparam` is the virtual key, `lparam`
/// is 1 for down and 0 for up.
pub const WM_RECORD_KEY: u32 = WM_APP + 0x52;

const LLKHF_INJECTED: u32 = 0x10;
/// An unassigned key, sent so Windows does not treat a released Win or Alt
/// as a lone tap (Start menu, menu bar) after the shortcut fired.
const VK_NONE: u16 = 0xe8;

#[derive(Default)]
struct State {
    /// Fire for this shortcut by posting to this window.
    watch: Option<(Shortcut, isize)>,
    /// Send every key to this window instead, and keep them from apps.
    record: Option<isize>,
    /// Keys down right now, side-specific.
    down: Vec<u32>,
    /// Modifier-only shortcuts: every key of it went down together and
    /// nothing else was pressed meanwhile.
    armed: bool,
    /// Key-ups to hide because their key-down was the shortcut.
    swallow_up: Vec<u32>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn ensure_thread() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("feather-keyhook".into())
            .spawn(|| unsafe {
                let module = GetModuleHandleW(std::ptr::null());
                let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), module, 0);
                if hook.is_null() {
                    return;
                }
                let mut message = MSG::default();
                while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {}
            });
    });
}

/// Watches for `shortcut` and posts `WM_SHORTCUT` to `hwnd`; `None` stops.
pub fn watch(shortcut: Option<(Shortcut, HWND)>) {
    let needed = shortcut.is_some();
    if let Ok(mut state) = state().lock() {
        state.watch = shortcut.map(|(shortcut, hwnd)| (shortcut, hwnd as isize));
        state.armed = false;
    }
    if needed {
        ensure_thread();
    }
}

/// Sends every key press to `hwnd` until `stop_recording`.
pub fn record(hwnd: HWND) {
    if let Ok(mut state) = state().lock() {
        state.record = Some(hwnd as isize);
        state.down.clear();
    }
    ensure_thread();
}

pub fn stop_recording() {
    if let Ok(mut state) = state().lock() {
        state.record = None;
        state.down.clear();
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = &*(lparam as *const KBDLLHOOKSTRUCT);
        if info.flags & LLKHF_INJECTED == 0 {
            let down = wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN;
            if on_key(info.vkCode, down) {
                return 1;
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

/// Returns true to keep the key from reaching apps.
fn on_key(vk: u32, down: bool) -> bool {
    let Ok(mut state) = state().lock() else {
        return false;
    };
    // Forget keys whose release was missed (the lock screen, say).
    state.down.retain(|&held| {
        held == vk || unsafe { GetAsyncKeyState(held as i32) } as u16 & 0x8000 != 0
    });
    let repeat = down && state.down.contains(&vk);
    if down && !repeat {
        state.down.push(vk);
    } else if !down {
        state.down.retain(|&held| held != vk);
    }

    if let Some(target) = state.record {
        if !repeat {
            unsafe {
                PostMessageW(target as HWND, WM_RECORD_KEY, vk as usize, down as isize);
            }
        }
        return true;
    }

    if !down {
        if let Some(at) = state.swallow_up.iter().position(|&held| held == vk) {
            state.swallow_up.remove(at);
            return true;
        }
    }
    let Some((shortcut, target)) = state.watch.clone() else {
        return false;
    };

    if shortcut.modifiers_only() {
        // Fire when released, if the keys were down together and alone.
        if down && !repeat {
            state.armed = shortcut.matches(&state.down);
        } else if !down && state.armed {
            state.armed = false;
            let mut before = state.down.clone();
            before.push(vk);
            if shortcut.matches(&before) {
                unsafe {
                    PostMessageW(target as HWND, WM_SHORTCUT, 0, 0);
                }
                quiet_lone_modifier(&shortcut);
            }
        }
        return false;
    }

    if down && !repeat && Modifier::of(vk).is_none() && shortcut.matches(&state.down) {
        state.swallow_up.push(vk);
        unsafe {
            PostMessageW(target as HWND, WM_SHORTCUT, 0, 0);
        }
        quiet_lone_modifier(&shortcut);
        return true;
    }
    if down && repeat && Modifier::of(vk).is_none() && state.swallow_up.contains(&vk) {
        // Holding the shortcut does not type the key either.
        return true;
    }
    false
}

/// After a shortcut with Win or Alt, a key press in between stops Windows
/// from opening Start or the menu bar when they are let go.
fn quiet_lone_modifier(shortcut: &Shortcut) {
    let uses = |wanted: Modifier| {
        shortcut.keys().iter().any(|key| match key {
            Key::Either(modifier) => *modifier == wanted,
            Key::Exact(vk) => Modifier::of(*vk) == Some(wanted),
        })
    };
    if !uses(Modifier::Win) && !uses(Modifier::Alt) {
        return;
    }
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_NONE,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [key(0), key(KEYEVENTF_KEYUP)];
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
    }
}
