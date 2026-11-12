//! The real Windows shell context menu.
//!
//! Windows 11's compact menu is owned by Explorer. A desktop app that hosts a
//! shell item through `IContextMenu` and asks for extended verbs gets the
//! classic expanded menu, including installed shell extensions. The fallback
//! menu keeps the basic actions usable when a handler fails.

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::size_of;

use windows::core::{Interface, HSTRING, PCSTR, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IContextMenu, IContextMenu2, IContextMenu3, IShellFolder, SHBindToParent, SHParseDisplayName,
    CMF_EXPLORE, CMF_EXTENDEDVERBS, CMF_NORMAL, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, PostMessageW, SetForegroundWindow,
    TrackPopupMenuEx, MF_STRING, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_DRAWITEM,
    WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR, WM_NULL,
};

const CMIC_MASK_UNICODE: u32 = 0x4000;
const ID_OPEN: usize = 1;
const ID_OPEN_FOLDER: usize = 2;
const ID_COPY_PATH: usize = 3;

struct ActiveMenu {
    menu2: Option<IContextMenu2>,
    menu3: Option<IContextMenu3>,
}

thread_local! {
    static ACTIVE_MENU: RefCell<Option<ActiveMenu>> = const { RefCell::new(None) };
}

struct MenuGuard;

impl Drop for MenuGuard {
    fn drop(&mut self) {
        ACTIVE_MENU.with(|slot| *slot.borrow_mut() = None);
    }
}

/// Shows the full shell menu at the current cursor position. When the shell
/// path cannot be used, a small native menu with the essential actions is
/// shown instead.
pub fn show(
    parent: windows_sys::Win32::Foundation::HWND,
    path: &str,
    use_shell: bool,
) -> Result<(), String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let owner = HWND(parent);
    if use_shell {
        match unsafe { show_shell(owner, path) } {
            Ok(()) => return Ok(()),
            // The menu was shown and a command was chosen: its failure is
            // reported, never followed by a second menu.
            Err(Failure::Command(error)) => return Err(error),
            Err(Failure::Menu(error)) => eprintln!("shell context menu: {error}"),
        }
    }
    unsafe { show_fallback(owner, path) }
}

/// Called from the main window procedure while a shell menu is open. Returns
/// the result to use for `WM_MENUCHAR`; other handled menu messages return 0.
pub fn handle_menu_message(message: u32, wparam: usize, lparam: isize) -> Option<isize> {
    if !matches!(
        message,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    ) {
        return None;
    }
    ACTIVE_MENU.with(|slot| {
        let mut guard = slot.borrow_mut();
        let active = guard.as_mut()?;
        let wparam = WPARAM(wparam);
        let lparam = LPARAM(lparam);
        if let Some(menu3) = &active.menu3 {
            let mut result = LRESULT(0);
            let _ = unsafe { menu3.HandleMenuMsg2(message, wparam, lparam, Some(&mut result)) };
            return Some(result.0);
        }
        if let Some(menu2) = &active.menu2 {
            let _ = unsafe { menu2.HandleMenuMsg(message, wparam, lparam) };
            return Some(0);
        }
        None
    })
}

/// Why the shell menu path ended: before the menu showed (then the small
/// menu stands in), or running the command chosen in it.
enum Failure {
    Menu(String),
    Command(String),
}

/// The first command id given to the shell menu; chosen ids are offsets
/// from it when invoked.
const FIRST_COMMAND: u32 = 1;

unsafe fn show_shell(owner: HWND, path: &str) -> Result<(), Failure> {
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    SHParseDisplayName(&HSTRING::from(path), None, &mut pidl, 0, None)
        .map_err(|error| Failure::Menu(format!("SHParseDisplayName failed: {error}")))?;
    let result = show_shell_pidl(owner, pidl);
    CoTaskMemFree(Some(pidl as *const c_void));
    result
}

unsafe fn show_shell_pidl(owner: HWND, pidl: *mut ITEMIDLIST) -> Result<(), Failure> {
    let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
    let parent: IShellFolder = SHBindToParent(pidl, Some(&mut child))
        .map_err(|error| Failure::Menu(format!("SHBindToParent failed: {error}")))?;
    let context: IContextMenu = parent
        .GetUIObjectOf(owner, &[child as *const ITEMIDLIST], None)
        .map_err(|error| Failure::Menu(format!("GetUIObjectOf(IContextMenu) failed: {error}")))?;

    let menu = CreatePopupMenu()
        .map_err(|error| Failure::Menu(format!("CreatePopupMenu failed: {error}")))?;
    context
        .QueryContextMenu(
            menu,
            0,
            FIRST_COMMAND,
            0x7fff,
            CMF_NORMAL | CMF_EXPLORE | CMF_EXTENDEDVERBS,
        )
        .ok()
        .map_err(|error| Failure::Menu(format!("QueryContextMenu failed: {error}")))?;

    let menu3 = context.cast::<IContextMenu3>().ok();
    let menu2 = context.cast::<IContextMenu2>().ok();
    ACTIVE_MENU.with(|slot| {
        *slot.borrow_mut() = Some(ActiveMenu { menu2, menu3 });
    });
    let _guard = MenuGuard;

    let mut point = POINT::default();
    let _ = GetCursorPos(&mut point);
    let _ = SetForegroundWindow(owner);
    let command = TrackPopupMenuEx(
        menu,
        TPM_RETURNCMD.0 | TPM_RIGHTBUTTON.0,
        point.x,
        point.y,
        owner,
        None,
    );
    let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
    drop(_guard);

    if command.0 != 0 {
        // The shell wants the command as an offset from the first id it was
        // given, not the id itself: the id ran the item below the one chosen.
        let offset = command.0 as usize - FIRST_COMMAND as usize;
        let info = CMINVOKECOMMANDINFOEX {
            cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: CMIC_MASK_UNICODE,
            hwnd: owner,
            lpVerb: PCSTR(offset as *const u8),
            lpVerbW: PCWSTR(offset as *const u16),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        context
            .InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO)
            .map_err(|error| Failure::Command(format!("That didn't work: {error}")))?;
    }
    Ok(())
}

unsafe fn show_fallback(owner: HWND, path: &str) -> Result<(), String> {
    let menu = CreatePopupMenu().map_err(|error| format!("CreatePopupMenu failed: {error}"))?;
    let _ = AppendMenuW(menu, MF_STRING, ID_OPEN, windows::core::w!("Open"));
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        ID_OPEN_FOLDER,
        windows::core::w!("Open containing folder"),
    );
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        ID_COPY_PATH,
        windows::core::w!("Copy full path"),
    );

    let mut point = POINT::default();
    let _ = GetCursorPos(&mut point);
    let _ = SetForegroundWindow(owner);
    let command = TrackPopupMenuEx(
        menu,
        TPM_RETURNCMD.0 | TPM_RIGHTBUTTON.0,
        point.x,
        point.y,
        owner,
        None,
    );
    let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);

    match command.0 as usize {
        ID_OPEN => open_path(owner, path),
        ID_OPEN_FOLDER => open_containing_folder(owner, path),
        ID_COPY_PATH => copy_path(owner, path),
        _ => Ok(()),
    }
}

unsafe fn open_path(owner: HWND, path: &str) -> Result<(), String> {
    shell_execute(owner, "open", path)
}

unsafe fn open_containing_folder(owner: HWND, path: &str) -> Result<(), String> {
    let parameters = HSTRING::from(format!("/select,\"{path}\""));
    let result = windows::Win32::UI::Shell::ShellExecuteW(
        Some(owner),
        windows::core::w!("open"),
        windows::core::w!("explorer.exe"),
        &parameters,
        None,
        SW_SHOWNORMAL,
    );
    if result.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!("explorer.exe returned {}", result.0 as isize))
    }
}

unsafe fn shell_execute(owner: HWND, verb: &str, path: &str) -> Result<(), String> {
    let result = windows::Win32::UI::Shell::ShellExecuteW(
        Some(owner),
        &HSTRING::from(verb),
        &HSTRING::from(path),
        None,
        None,
        SW_SHOWNORMAL,
    );
    if result.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!("ShellExecuteW returned {}", result.0 as isize))
    }
}

unsafe fn copy_path(owner: HWND, path: &str) -> Result<(), String> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    let units: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = units.len() * size_of::<u16>();
    let memory = GlobalAlloc(GMEM_MOVEABLE, bytes)
        .map_err(|error| format!("GlobalAlloc failed: {error}"))?;
    let target = GlobalLock(memory);
    if target.is_null() {
        return Err("GlobalLock failed".into());
    }
    std::ptr::copy_nonoverlapping(units.as_ptr() as *const u8, target as *mut u8, bytes);
    let _ = GlobalUnlock(memory);
    if OpenClipboard(Some(owner)).is_err() {
        return Err("could not open the clipboard".into());
    }
    let _ = EmptyClipboard();
    let ok = SetClipboardData(
        windows::Win32::System::Ole::CF_UNICODETEXT.0 as u32,
        Some(HANDLE(memory.0)),
    )
    .is_ok();
    let _ = CloseClipboard();
    if ok {
        Ok(())
    } else {
        Err("could not set the clipboard text".into())
    }
}

/// Puts text on the clipboard.
#[allow(dead_code)]
pub fn copy_text(parent: windows_sys::Win32::Foundation::HWND, text: &str) -> Result<(), String> {
    unsafe { copy_path(HWND(parent), text) }
}

/// Reads Unicode text from the clipboard, if any.
#[allow(dead_code)]
pub fn paste_text(parent: windows_sys::Win32::Foundation::HWND) -> Option<String> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    unsafe {
        OpenClipboard(Some(HWND(parent))).ok()?;
        let text = (|| {
            let handle =
                GetClipboardData(windows::Win32::System::Ole::CF_UNICODETEXT.0 as u32).ok()?;
            let memory = HGLOBAL(handle.0);
            let pointer = GlobalLock(memory) as *const u16;
            if pointer.is_null() {
                return None;
            }
            let capacity = GlobalSize(memory) / size_of::<u16>();
            let units = std::slice::from_raw_parts(pointer, capacity);
            let end = units
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(units.len());
            let text = String::from_utf16_lossy(&units[..end]);
            let _ = GlobalUnlock(memory);
            Some(text)
        })();
        let _ = CloseClipboard();
        text
    }
}

/// Opens an https address in the default browser.
pub fn open_url(url: &str) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("only https addresses open".into());
    }
    unsafe { shell_execute(HWND(std::ptr::null_mut()), "open", url) }
}

/// Opens a file or folder with its default handler.
#[allow(dead_code)]
pub fn open_item(parent: windows_sys::Win32::Foundation::HWND, path: &str) -> Result<(), String> {
    unsafe { open_path(HWND(parent), path) }
}

/// Opens Explorer with the item selected.
#[allow(dead_code)]
pub fn reveal_item(parent: windows_sys::Win32::Foundation::HWND, path: &str) -> Result<(), String> {
    unsafe { open_containing_folder(HWND(parent), path) }
}
