//! Telling when a game or another full-screen app has the screen, so the
//! shortcut can stand aside: Windows hands a registered shortcut to Feather
//! even inside a game, which then loses those keys and gets Feather on top.

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONULL,
};
use windows_sys::Win32::UI::Shell::{
    SHQueryUserNotificationState, QUERY_USER_NOTIFICATION_STATE, QUNS_BUSY, QUNS_PRESENTATION_MODE,
    QUNS_RUNNING_D3D_FULL_SCREEN,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, IsWindowVisible,
    IsZoomed, GWL_STYLE, WS_CAPTION, WS_THICKFRAME,
};

/// True when the window in front fills its whole monitor (a game, a video
/// played full screen, a presentation), other than Feather's own `own`
/// window or the desktop itself.
pub fn app_in_front(own: HWND) -> bool {
    // Windows' own answer covers exclusive full-screen games and
    // presentation mode.
    let mut state: QUERY_USER_NOTIFICATION_STATE = 0;
    if unsafe { SHQueryUserNotificationState(&mut state) } == 0
        && matches!(
            state,
            QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_BUSY | QUNS_PRESENTATION_MODE
        )
    {
        return true;
    }
    // Borderless full-screen games look like ordinary windows to it, so
    // compare the front window with its monitor.
    let front = unsafe { GetForegroundWindow() };
    if front.is_null() || front == own || unsafe { IsWindowVisible(front) } == 0 {
        return false;
    }
    if is_shell(front) {
        return false;
    }
    // A maximized window overhangs its monitor by its border, so with the
    // taskbar hidden or on another monitor it would look full screen. Real
    // full-screen apps are neither maximized nor framed with a title bar.
    let style = unsafe { GetWindowLongPtrW(front, GWL_STYLE) } as u32;
    if unsafe { IsZoomed(front) } != 0 || !looks_borderless(style) {
        return false;
    }
    let mut window = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { GetWindowRect(front, &mut window) } == 0 {
        return false;
    }
    let monitor = unsafe { MonitorFromWindow(front, MONITOR_DEFAULTTONULL) };
    if monitor.is_null() {
        return false;
    }
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return false;
    }
    covers(&window, &info.rcMonitor)
}

/// The desktop and taskbar fill the screen too, but aren't apps.
fn is_shell(hwnd: HWND) -> bool {
    let mut class = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) };
    let name = String::from_utf16_lossy(&class[..len.max(0) as usize]);
    matches!(
        name.as_str(),
        "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
    )
}

/// True for a window style without a title bar or sizing border, as games
/// and full-screen players use.
fn looks_borderless(style: u32) -> bool {
    style & WS_CAPTION != WS_CAPTION && style & WS_THICKFRAME == 0
}

/// True when `window` covers all of `monitor`.
fn covers(window: &RECT, monitor: &RECT) -> bool {
    window.left <= monitor.left
        && window.top <= monitor.top
        && window.right >= monitor.right
        && window.bottom >= monitor.bottom
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn framed_windows_are_not_full_screen_apps() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
        };
        // A browser or editor: title bar and sizing border.
        assert!(!looks_borderless(WS_OVERLAPPEDWINDOW | WS_VISIBLE));
        // A borderless full-screen game.
        assert!(looks_borderless(WS_POPUP | WS_VISIBLE));
        // A fixed-size window with a title bar.
        assert!(!looks_borderless(WS_CAPTION | WS_VISIBLE));
    }

    #[test]
    fn only_windows_covering_the_whole_monitor_count() {
        let monitor = rect(0, 0, 1920, 1080);
        assert!(covers(&rect(0, 0, 1920, 1080), &monitor));
        // Exclusive modes and some games overshoot by a few pixels.
        assert!(covers(&rect(-8, -8, 1928, 1088), &monitor));
        // A maximized window stops at the taskbar.
        assert!(!covers(&rect(0, 0, 1920, 1040), &monitor));
        assert!(!covers(&rect(100, 100, 900, 700), &monitor));
        // On the second monitor.
        assert!(covers(
            &rect(1920, 0, 4480, 1440),
            &rect(1920, 0, 4480, 1440)
        ));
    }
}
