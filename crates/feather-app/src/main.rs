// A window app: Windows opens no console window for it.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
#[cfg(windows)]
use windows_sys::Win32::System::Console::{
    AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
    STD_OUTPUT_HANDLE,
};

#[cfg(windows)]
fn missing(handle: HANDLE) -> bool {
    handle.is_null() || handle == INVALID_HANDLE_VALUE
}

/// Commands such as `feather config list` still print when run from a
/// terminal: borrow the terminal that started us. Output that is already
/// piped or redirected to a file is left alone.
#[cfg(windows)]
fn use_parent_console() {
    unsafe {
        let out = missing(GetStdHandle(STD_OUTPUT_HANDLE));
        let err = missing(GetStdHandle(STD_ERROR_HANDLE));
        if !(out || err) || AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
        let console = CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if missing(console) {
            return;
        }
        if out {
            SetStdHandle(STD_OUTPUT_HANDLE, console);
        }
        if err {
            SetStdHandle(STD_ERROR_HANDLE, console);
        }
    }
}

#[cfg(not(windows))]
fn use_parent_console() {}

fn main() {
    use_parent_console();
    let code = feather_app::cli::run(std::env::args().skip(1));
    std::process::exit(code);
}
