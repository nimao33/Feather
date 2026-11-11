//! Starting Feather when the person signs in, through the per-user `Run`
//! key. The switch in Settings reads the key itself, so it always shows
//! what Windows will actually do.

use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_SZ,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "Feather";

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The command Windows runs at sign-in: this program, in the tray.
fn command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\" --background", exe.display()))
}

/// True when the `Run` entry points at this copy of Feather.
pub fn enabled() -> bool {
    let key = wide(RUN_KEY);
    let value = wide(VALUE);
    let mut buffer = [0u16; 1024];
    let mut size = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr() as *mut _,
            &mut size,
        )
    };
    if status != 0 {
        return false;
    }
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    let stored = String::from_utf16_lossy(&buffer[..end]);
    command().is_some_and(|wanted| stored.eq_ignore_ascii_case(&wanted))
}

/// Adds or removes the `Run` entry.
pub fn set(on: bool) -> Result<(), String> {
    let key_name = wide(RUN_KEY);
    let value = wide(VALUE);
    let mut key: HKEY = std::ptr::null_mut();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            key_name.as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut key,
        )
    };
    if status != 0 {
        return Err(format!("could not open the startup list (error {status})"));
    }
    let result = if on {
        let command = command().ok_or("could not find Feather's own path")?;
        let data = wide(&command);
        unsafe {
            RegSetValueExW(
                key,
                value.as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                (data.len() * 2) as u32,
            )
        }
    } else {
        match unsafe { RegDeleteValueW(key, value.as_ptr()) } {
            2 => 0, // not there: already off
            status => status,
        }
    };
    unsafe {
        RegCloseKey(key);
    }
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "could not change the startup list (error {result})"
        ))
    }
}
