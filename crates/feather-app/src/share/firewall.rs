//! Letting phones and other computers on the network through Windows
//! Firewall. Windows asks about a new listening program once; a "Cancel"
//! there leaves a rule that blocks Feather, and block rules beat allow rules.
//! So this removes Feather's old inbound rules and adds one that allows it,
//! after Windows asks for administrator permission. The servers themselves
//! still refuse anyone the "Reachable from" setting doesn't allow.

/// Runs the rule change elevated; Windows shows its permission prompt.
#[cfg(windows)]
pub fn allow() -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SEE_MASK_NO_CONSOLE, SHELLEXECUTEINFOW,
    };
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let exe = exe.to_string_lossy();
    if exe.contains('"') {
        return Err("Feather's path can't be used in a firewall rule.".into());
    }
    let parameters = format!(
        "/c netsh advfirewall firewall delete rule name=all dir=in program=\"{exe}\" & \
         netsh advfirewall firewall add rule name=\"Feather\" dir=in action=allow \
         program=\"{exe}\" enable=yes profile=any protocol=tcp"
    );
    let wide = |text: &str| -> Vec<u16> { text.encode_utf16().chain(std::iter::once(0)).collect() };
    let (verb, file, parameters) = (wide("runas"), wide("cmd.exe"), wide(&parameters));
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NO_CONSOLE;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.nShow = 0;
    if unsafe { ShellExecuteExW(&mut info) } == 0 || info.hProcess.is_null() {
        return Err("Windows didn't allow it (the permission prompt was declined).".into());
    }
    let mut code = 1u32;
    unsafe {
        WaitForSingleObject(info.hProcess, 60_000);
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
    }
    if code == 0 {
        Ok(())
    } else {
        Err(format!(
            "Windows Firewall didn't take the rule (code {code})."
        ))
    }
}

#[cfg(not(windows))]
pub fn allow() -> Result<(), String> {
    Err(
        "If a firewall is on, allow Feather's ports in it (for example `sudo ufw allow 8443/tcp`)."
            .into(),
    )
}
