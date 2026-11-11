//! Animation frames paced by the display.
//!
//! A `SetTimer` interval is rounded up to the system timer tick (15.6 ms), so
//! a 16 ms timer fires every 31 ms and animations run at 32 frames a second.
//! Instead, a window that is animating registers here, and the message loop
//! renders a frame and then waits in `DwmFlush` for the next composition
//! pass. Frames therefore arrive once per monitor refresh, whatever the
//! refresh rate is.
//!
//! Each registered window receives `WM_ANIM_FRAME`, advances its animations,
//! and invalidates what moved; the loop then paints it with `UpdateWindow`
//! before waiting. A window unregisters with [`stop`] when it has settled.

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Dwm::DwmFlush;
use windows_sys::Win32::Graphics::Gdi::UpdateWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetMessageW, IsWindow, PeekMessageW, SendMessageW, MSG, PM_REMOVE, WM_APP, WM_QUIT,
};

/// Sent to an animating window once per display refresh.
pub const WM_ANIM_FRAME: u32 = WM_APP + 0x40;

/// After a frame, input is handled for at most this long before the next
/// frame is drawn, so a flood of mouse moves cannot stall an animation.
const INPUT_BUDGET: Duration = Duration::from_millis(6);

thread_local! {
    static WINDOWS: RefCell<Vec<HWND>> = const { RefCell::new(Vec::new()) };
    static LAST_FRAME: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Asks for a frame on every refresh until [`stop`] is called.
pub fn request(hwnd: HWND) {
    WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        if !windows.contains(&hwnd) {
            windows.push(hwnd);
        }
    });
}

pub fn stop(hwnd: HWND) {
    WINDOWS.with(|windows| windows.borrow_mut().retain(|candidate| *candidate != hwnd));
}

fn wanted() -> bool {
    WINDOWS.with(|windows| !windows.borrow().is_empty())
}

/// Draws one frame for every animating window, then waits for the next
/// composition pass.
fn render() {
    let windows: Vec<HWND> = WINDOWS.with(|windows| windows.borrow().clone());
    for hwnd in windows {
        unsafe {
            if IsWindow(hwnd) == 0 {
                stop(hwnd);
                continue;
            }
            SendMessageW(hwnd, WM_ANIM_FRAME, 0, 0);
            UpdateWindow(hwnd);
        }
    }
    unsafe {
        DwmFlush();
    }
    LAST_FRAME.with(|last| last.set(Some(Instant::now())));
}

/// Like `GetMessageW`, but keeps drawing animation frames while any window
/// wants them. Returns false on `WM_QUIT` (or an error), like `GetMessageW`.
pub fn next_message(message: &mut MSG) -> bool {
    loop {
        if !wanted() {
            return unsafe { GetMessageW(message, std::ptr::null_mut(), 0, 0) } > 0;
        }
        let since = LAST_FRAME.with(|last| last.get().map(|at| at.elapsed()));
        let input_time = since.is_some_and(|elapsed| elapsed < INPUT_BUDGET);
        if input_time
            && unsafe { PeekMessageW(message, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0
        {
            return message.message != WM_QUIT;
        }
        render();
    }
}
