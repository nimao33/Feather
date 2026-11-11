use windows_sys::Win32::Foundation::{HWND, RECT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;

pub fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[allow(dead_code)]
pub fn from_wide(text: &[u16]) -> String {
    let end = text
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(text.len());
    String::from_utf16_lossy(&text[..end])
}

pub fn loword(value: WPARAM) -> u16 {
    (value & 0xffff) as u16
}

pub fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rect = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut rect) } == 0 {
        (0, 0)
    } else {
        (rect.right - rect.left, rect.bottom - rect.top)
    }
}
