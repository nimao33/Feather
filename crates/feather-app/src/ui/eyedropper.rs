//! Picking a color from anywhere on screen. The screen is photographed
//! once, a see-through window covers every monitor so the click lands on
//! it and not on whatever is underneath, and a loupe follows the pointer:
//! the pixels around it, magnified, with the color under it and its code.
//! Esc or a right click cancels.

use std::cell::Cell;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreateRoundRectRgn,
    CreateSolidBrush, DeleteDC, DeleteObject, FillRect, FrameRect, GetDC, GetPixel, ReleaseDC,
    SelectObject, SetBkMode, SetStretchBltMode, SetTextColor, SetWindowRgn, StretchBlt, TextOutW,
    CLR_INVALID, COLORONCOLOR, HDC, SRCCOPY, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
    GetSystemMetrics, LoadCursorW, PostMessageW, RegisterClassW, SetCursor, SetForegroundWindow,
    SetLayeredWindowAttributes, SetWindowPos, ShowWindow, TranslateMessage, HWND_TOPMOST,
    IDC_CROSS, LWA_ALPHA, MSG, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_SHOW, WM_APP, WM_KEYDOWN, WM_KILLFOCUS,
    WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_SETCURSOR, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

const WM_DONE: u32 = WM_APP + 0x61;
/// Pixels shown across the loupe, and how many times bigger each is drawn.
const CELLS: i32 = 11;
const ZOOM: i32 = 12;

/// What the overlay's window procedure needs while a pick runs.
#[derive(Clone, Copy)]
struct Session {
    /// The photograph of the screen, and where its top left is on screen.
    shot: HDC,
    origin: (i32, i32),
    size: (i32, i32),
    loupe: HWND,
}

thread_local! {
    /// The pick: `None` while choosing, then `Some(color)` or `Some(None)`.
    static RESULT: Cell<Option<Option<u32>>> = const { Cell::new(None) };
    static SESSION: Cell<Option<Session>> = const { Cell::new(None) };
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn pointer() -> POINT {
    let mut at = POINT { x: 0, y: 0 };
    unsafe {
        GetCursorPos(&mut at);
    }
    at
}

/// A `COLORREF` (`0x00BBGGRR`) as `0xRRGGBB`.
fn rgb(pixel: u32) -> u32 {
    ((pixel & 0xff) << 16) | (pixel & 0xff00) | ((pixel >> 16) & 0xff)
}

/// The color in the photograph under the pointer.
fn under_pointer(session: &Session) -> Option<u32> {
    let at = pointer();
    let pixel = unsafe {
        GetPixel(
            session.shot,
            at.x - session.origin.0,
            at.y - session.origin.1,
        )
    };
    (pixel != CLR_INVALID).then(|| rgb(pixel))
}

/// Moves the loupe beside the pointer and redraws it.
unsafe fn show_loupe(session: &Session) {
    let at = pointer();
    let scale = (GetDpiForWindow(session.loupe).max(96) as f32 / 96.0)
        .round()
        .max(1.0) as i32;
    let side = CELLS * ZOOM * scale;
    let label_h = 26 * scale;
    let (w, h) = (side, side + label_h);
    // Below and to the right, or the other way near the screen's edges.
    let gap = 22 * scale;
    let (right, bottom) = (
        session.origin.0 + session.size.0,
        session.origin.1 + session.size.1,
    );
    let x = if at.x + gap + w > right {
        at.x - gap - w
    } else {
        at.x + gap
    };
    let y = if at.y + gap + h > bottom {
        at.y - gap - h
    } else {
        at.y + gap
    };
    SetWindowPos(
        session.loupe,
        HWND_TOPMOST,
        x,
        y,
        w,
        h,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );

    let dc = GetDC(session.loupe);
    SetStretchBltMode(dc, COLORONCOLOR);
    StretchBlt(
        dc,
        0,
        0,
        side,
        side,
        session.shot,
        at.x - session.origin.0 - CELLS / 2,
        at.y - session.origin.1 - CELLS / 2,
        CELLS,
        CELLS,
        SRCCOPY,
    );
    let pixel = GetPixel(
        session.shot,
        at.x - session.origin.0,
        at.y - session.origin.1,
    );
    let pixel = if pixel == CLR_INVALID { 0 } else { pixel };
    let (r, g, b) = (pixel & 0xff, (pixel >> 8) & 0xff, (pixel >> 16) & 0xff);
    let light = r * 299 + g * 587 + b * 114 > 140_000;
    let frame = |rect: RECT, color: u32| {
        let brush = CreateSolidBrush(color);
        FrameRect(dc, &rect, brush);
        DeleteObject(brush);
    };
    // The pixel that will be picked, ringed in white on black.
    let cell = ZOOM * scale;
    let middle = (CELLS / 2) * cell;
    let ring = RECT {
        left: middle - 1,
        top: middle - 1,
        right: middle + cell + 1,
        bottom: middle + cell + 1,
    };
    frame(ring, 0x00ff_ffff);
    frame(
        RECT {
            left: ring.left - 1,
            top: ring.top - 1,
            right: ring.right + 1,
            bottom: ring.bottom + 1,
        },
        0x0000_0000,
    );
    // The color itself and its code.
    let label = RECT {
        left: 0,
        top: side,
        right: w,
        bottom: h,
    };
    let brush = CreateSolidBrush(pixel);
    FillRect(dc, &label, brush);
    DeleteObject(brush);
    let face = wide("Segoe UI");
    let font = CreateFontW(
        -14 * scale,
        0,
        0,
        0,
        600,
        0,
        0,
        0,
        0,
        0,
        0,
        5,
        0,
        face.as_ptr(),
    );
    let before = SelectObject(dc, font);
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, if light { 0x0016_1a1d } else { 0x00ff_ffff });
    let code: Vec<u16> = format!("#{:06X}", rgb(pixel)).encode_utf16().collect();
    TextOutW(
        dc,
        10 * scale,
        side + 4 * scale,
        code.as_ptr(),
        code.len() as i32,
    );
    SelectObject(dc, before);
    DeleteObject(font);
    frame(
        RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        },
        if light { 0x0040_4040 } else { 0x00a0_a0a0 },
    );
    ReleaseDC(session.loupe, dc);
}

unsafe extern "system" fn overlay_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let finish = |color: Option<u32>| {
        RESULT.with(|result| result.set(Some(color)));
        PostMessageW(hwnd, WM_DONE, 0, 0);
    };
    let session = SESSION.with(|session| session.get());
    match message {
        WM_SETCURSOR => {
            SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_CROSS));
            1
        }
        WM_MOUSEMOVE => {
            if let Some(session) = &session {
                show_loupe(session);
            }
            0
        }
        WM_LBUTTONDOWN => {
            finish(session.as_ref().and_then(under_pointer));
            0
        }
        WM_RBUTTONDOWN => {
            finish(None);
            0
        }
        WM_KEYDOWN if wparam as u16 == VK_ESCAPE => {
            finish(None);
            0
        }
        WM_KILLFOCUS => {
            // Another window took over (Alt+Tab): stop picking.
            if RESULT.with(|result| result.get()).is_none() {
                finish(None);
            }
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe extern "system" fn loupe_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, message, wparam, lparam)
}

/// Lets the person click anywhere on screen; returns that color, or `None`
/// when they cancel. Runs its own message loop until then.
pub fn pick(owner: HWND) -> Option<u32> {
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let register =
            |name: &[u16],
             proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT| {
                let mut window_class: WNDCLASSW = std::mem::zeroed();
                window_class.lpfnWndProc = Some(proc);
                window_class.hInstance = instance;
                window_class.lpszClassName = name.as_ptr();
                window_class.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_CROSS);
                RegisterClassW(&window_class);
            };
        let overlay_class = wide("FeatherEyedropper");
        let loupe_class = wide("FeatherEyedropperLoupe");
        register(&overlay_class, overlay_proc);
        register(&loupe_class, loupe_proc);
        let (x, y) = (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        );
        let (w, h) = (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        );

        // The screen as it is now; colors are read from this, so neither
        // window here can tint what is picked.
        let screen = GetDC(std::ptr::null_mut());
        let shot = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, w, h);
        let before = SelectObject(shot, bitmap);
        BitBlt(shot, 0, 0, w, h, screen, x, y, SRCCOPY);
        ReleaseDC(std::ptr::null_mut(), screen);
        let release = || {
            SelectObject(shot, before);
            DeleteObject(bitmap);
            DeleteDC(shot);
        };

        let overlay = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            overlay_class.as_ptr(),
            overlay_class.as_ptr(),
            WS_POPUP,
            x,
            y,
            w,
            h,
            owner,
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if overlay.is_null() {
            release();
            return None;
        }
        // Next to invisible, but there, so clicks reach it.
        SetLayeredWindowAttributes(overlay, 0, 1, LWA_ALPHA);
        // The loupe never takes clicks or focus.
        let loupe = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED,
            loupe_class.as_ptr(),
            loupe_class.as_ptr(),
            WS_POPUP,
            0,
            0,
            CELLS * ZOOM,
            CELLS * ZOOM + 26,
            overlay,
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if !loupe.is_null() {
            SetLayeredWindowAttributes(loupe, 0, 255, LWA_ALPHA);
            let scale = (GetDpiForWindow(loupe).max(96) as f32 / 96.0)
                .round()
                .max(1.0) as i32;
            let side = CELLS * ZOOM * scale;
            SetWindowRgn(
                loupe,
                CreateRoundRectRgn(
                    0,
                    0,
                    side + 1,
                    side + 26 * scale + 1,
                    14 * scale,
                    14 * scale,
                ),
                0,
            );
        }
        let session = Session {
            shot,
            origin: (x, y),
            size: (w, h),
            loupe,
        };
        SESSION.with(|slot| slot.set(Some(session)));
        RESULT.with(|result| result.set(None));
        ShowWindow(overlay, SW_SHOW);
        SetForegroundWindow(overlay);
        SetFocus(overlay);
        if !loupe.is_null() {
            show_loupe(&session);
        }
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            if message.hwnd == overlay && message.message == WM_DONE {
                break;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        SESSION.with(|slot| slot.set(None));
        if !loupe.is_null() {
            DestroyWindow(loupe);
        }
        DestroyWindow(overlay);
        release();
        SetForegroundWindow(owner);
        RESULT.with(|result| result.take()).flatten()
    }
}
