//! Feather's icon in the notification area, used while the window is hidden.

use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_LARGE_ICON,
    NIIF_USER, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW,
    NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, GetSystemMetrics, LoadIconW,
    LoadImageW, PostMessageW, RegisterWindowMessageW, SetForegroundWindow, SetMenuDefaultItem,
    TrackPopupMenuEx, HICON, IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTCOLOR, MF_SEPARATOR, MF_STRING,
    SM_CXSMICON, SM_CYSMICON, TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP,
    WM_CONTEXTMENU, WM_NULL, WM_USER,
};

use windows_sys::core::w;

/// The message the tray icon sends to the main window.
pub const WM_TRAY: u32 = WM_APP + 0x42;
const ICON_ID: u32 = 1;
/// The resource id of the app icon in `feather.rc`.
const APP_ICON: u16 = 1;

const NIN_SELECT: u32 = WM_USER;
const NIN_KEYSELECT: u32 = WM_USER + 1;
const NIN_BALLOONHIDE: u32 = WM_USER + 3;
const NIN_BALLOONTIMEOUT: u32 = WM_USER + 4;
const NIN_BALLOONUSERCLICK: u32 = WM_USER + 5;

pub const MENU_OPEN: u32 = 1;
pub const MENU_SETTINGS: u32 = 2;
pub const MENU_QUIT: u32 = 3;

/// What a tray event asks for.
#[derive(Debug, Eq, PartialEq)]
pub enum TrayEvent {
    Open,
    Menu,
    /// The notification was clicked.
    NoteClicked,
    /// The notification went away unclicked.
    NoteGone,
    Other,
}

/// Shows a Windows notification from the icon, which must be added.
pub fn notify(hwnd: HWND, title: &str, text: &str) -> bool {
    let mut data = data(hwnd);
    data.uFlags = NIF_INFO;
    data.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON;
    data.hBalloonIcon = app_icon(32);
    for (slot, unit) in data
        .szInfoTitle
        .iter_mut()
        .take(63)
        .zip(title.encode_utf16())
    {
        *slot = unit;
    }
    for (slot, unit) in data.szInfo.iter_mut().take(255).zip(text.encode_utf16()) {
        *slot = unit;
    }
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) != 0 }
}

/// The app icon at `size` pixels, or the generic one if it is missing.
pub fn app_icon(size: i32) -> HICON {
    unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        let icon = LoadImageW(
            module,
            APP_ICON as usize as *const u16,
            IMAGE_ICON,
            size,
            size,
            LR_DEFAULTCOLOR,
        );
        if icon.is_null() {
            LoadIconW(std::ptr::null_mut(), IDI_APPLICATION)
        } else {
            icon as HICON
        }
    }
}

fn data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = ICON_ID;
    data
}

/// Adds the icon. Safe to call again after Explorer restarts.
pub fn add(hwnd: HWND) -> bool {
    let mut data = data(hwnd);
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = app_icon(unsafe { GetSystemMetrics(SM_CXSMICON) });
    for (slot, unit) in data.szTip.iter_mut().zip("Feather".encode_utf16()) {
        *slot = unit;
    }
    unsafe {
        // A stale icon from before an Explorer restart would make NIM_ADD fail.
        Shell_NotifyIconW(NIM_DELETE, &data);
        if Shell_NotifyIconW(NIM_ADD, &data) == 0 {
            return false;
        }
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &data);
    }
    true
}

pub fn remove(hwnd: HWND) {
    let data = data(hwnd);
    unsafe {
        Shell_NotifyIconW(NIM_DELETE, &data);
    }
}

/// The message Explorer broadcasts when the taskbar is recreated.
pub fn taskbar_created() -> u32 {
    unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }
}

/// Reads a `WM_TRAY` message (version 4 layout: the event is in the low
/// word of `lparam`).
pub fn event(lparam: isize) -> TrayEvent {
    match (lparam as u32) & 0xffff {
        NIN_SELECT | NIN_KEYSELECT => TrayEvent::Open,
        WM_CONTEXTMENU => TrayEvent::Menu,
        NIN_BALLOONUSERCLICK => TrayEvent::NoteClicked,
        NIN_BALLOONHIDE | NIN_BALLOONTIMEOUT => TrayEvent::NoteGone,
        _ => TrayEvent::Other,
    }
}

/// Shows the tray menu at the cursor and returns the chosen command.
pub fn show_menu(hwnd: HWND) -> Option<u32> {
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return None;
        }
        AppendMenuW(menu, MF_STRING, MENU_OPEN as usize, w!("Open Feather"));
        AppendMenuW(menu, MF_STRING, MENU_SETTINGS as usize, w!("Settings"));
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, MENU_QUIT as usize, w!("Quit Feather"));
        SetMenuDefaultItem(menu, MENU_OPEN, 0);
        let mut point = POINT { x: 0, y: 0 };
        GetCursorPos(&mut point);
        // Without this the menu stays open after clicking elsewhere.
        SetForegroundWindow(hwnd);
        let command = TrackPopupMenuEx(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            point.x,
            point.y,
            hwnd,
            std::ptr::null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        (command > 0).then_some(command as u32)
    }
}

/// Small icon size, for the window's title bar.
pub fn small_icon_size() -> i32 {
    unsafe { GetSystemMetrics(SM_CYSMICON) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_events_are_read_from_the_low_word() {
        assert_eq!(event((5 << 16) | NIN_SELECT as isize), TrayEvent::Open);
        assert_eq!(event(WM_CONTEXTMENU as isize), TrayEvent::Menu);
        assert_eq!(event(0x0200), TrayEvent::Other);
    }
}
