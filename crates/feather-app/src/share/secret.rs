//! Secrets kept on disk (the tunnel token, the server's private key),
//! encrypted with Windows DPAPI so only this Windows account can read them.

/// Encrypts `data` for the signed-in Windows account.
#[cfg(windows)]
pub fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err("Windows could not encrypt the secret".into());
    }
    Ok(take(output))
}

#[cfg(windows)]
pub fn unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err("Windows could not decrypt the secret".into());
    }
    Ok(take(output))
}

/// Copies a DPAPI result out and frees it.
#[cfg(windows)]
fn take(blob: windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB) -> Vec<u8> {
    let bytes = unsafe { std::slice::from_raw_parts(blob.pbData, blob.cbData as usize) }.to_vec();
    unsafe {
        windows_sys::Win32::Foundation::LocalFree(blob.pbData as _);
    }
    bytes
}

/// Elsewhere the file's permissions are the protection.
#[cfg(not(windows))]
pub fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
    Ok(data.to_vec())
}

#[cfg(not(windows))]
pub fn unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    Ok(data.to_vec())
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// A secret as text for the settings file: encrypted, then hex.
pub fn seal(secret: &str) -> Result<String, String> {
    if secret.is_empty() {
        return Ok(String::new());
    }
    let sealed = protect(secret.as_bytes())?;
    Ok(sealed
        .iter()
        .flat_map(|byte| {
            [
                HEX[(byte >> 4) as usize] as char,
                HEX[(byte & 15) as usize] as char,
            ]
        })
        .collect())
}

/// The secret back from `seal`; empty when missing or unreadable.
pub fn open(sealed: &str) -> String {
    if sealed.is_empty() || !sealed.len().is_multiple_of(2) {
        return String::new();
    }
    let bytes: Option<Vec<u8>> = (0..sealed.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(sealed.get(at..at + 2)?, 16).ok())
        .collect();
    bytes
        .and_then(|bytes| unprotect(&bytes).ok())
        .and_then(|plain| String::from_utf8(plain).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_round_trip() {
        let sealed = seal("token-123").expect("seal");
        assert!(!sealed.contains("token-123"));
        assert_eq!(open(&sealed), "token-123");
        assert_eq!(open(""), "");
        assert_eq!(open("zz"), "");
        assert_eq!(seal("").unwrap(), "");
    }
}
