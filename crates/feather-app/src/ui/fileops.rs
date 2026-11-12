//! File management from the results list, done the way Explorer does it.
//!
//! Renaming, deleting, and pasting go through the shell's `IFileOperation`,
//! so Windows shows its own progress, conflict, and elevation dialogs, and
//! the Recycle Bin and undo work as they do in Explorer. Copy and cut put
//! the items on the clipboard as files (`CF_HDROP` plus "Preferred
//! DropEffect"), so pasting in Explorer copies or moves them.

use std::ffi::c_void;

use windows::core::HSTRING;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    DragQueryFileW, FileOperation, IFileOperation, IFileOperationProgressSink, IShellItem,
    IShellItemArray, SHCreateItemFromParsingName, SHCreateShellItemArrayFromIDLists,
    SHGetSetSettings, SHParseDisplayName, FILEOPERATION_FLAGS, FOFX_RECYCLEONDELETE, FOF_ALLOWUNDO,
    FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_RENAMEONCOLLISION, FOF_SILENT, FOF_WANTNUKEWARNING,
    HDROP, SHELLSTATEA, SSF_NOCONFIRMRECYCLE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_YESNO,
};

/// `DROPEFFECT_COPY | DROPEFFECT_LINK`, what Explorer's Copy offers.
pub const EFFECT_COPY: u32 = 1 | 4;
/// `DROPEFFECT_MOVE`, what Explorer's Cut offers.
pub const EFFECT_MOVE: u32 = 2;
/// Size of the `DROPFILES` header in front of a `CF_HDROP` file list.
const DROPFILES_SIZE: usize = 20;

// ----- Names --------------------------------------------------------------

/// How much of a name to select when renaming starts: the part before the
/// extension for a file (Explorer does the same), all of it for a folder or
/// for a name that only has a leading dot. In UTF-16 units.
pub fn rename_selection(name: &[u16], is_dir: bool) -> usize {
    if is_dir {
        return name.len();
    }
    match name.iter().rposition(|&unit| unit == u16::from(b'.')) {
        Some(0) | None => name.len(),
        Some(dot) => dot,
    }
}

/// What Explorer says when a name has a character it can't hold.
pub const FORBIDDEN_MESSAGE: &str =
    "A file name can't contain any of these characters: \\ / : * ? \" < > |";

/// Characters Windows does not allow in a file or folder name.
pub fn is_forbidden_char(c: char) -> bool {
    matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control()
}

/// Checks a new name typed for a file or folder. Returns the name to use,
/// with surrounding spaces and trailing dots removed as Windows would, or
/// a message saying what is wrong with it.
pub fn validate_new_name(name: &str) -> Result<String, String> {
    let name = name.trim_matches(' ').trim_end_matches(['.', ' ']);
    if name.is_empty() {
        return Err("A name can't be empty.".into());
    }
    if name.chars().any(is_forbidden_char) {
        return Err(FORBIDDEN_MESSAGE.into());
    }
    if name.encode_utf16().count() > 255 {
        return Err("That name is too long.".into());
    }
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    let upper = stem.to_ascii_uppercase();
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.len() == 4
            && matches!(upper.as_bytes()[3], b'1'..=b'9'));
    if reserved {
        return Err(format!("{stem} is a name Windows keeps for itself."));
    }
    Ok(name.to_string())
}

/// The folder pasted items go into: the selected folder itself, or the
/// folder holding the selected file.
pub fn paste_folder(path: &str, is_dir: bool) -> String {
    if is_dir {
        path.to_string()
    } else {
        crate::text::parent_path(path)
    }
}

/// True when a "Preferred DropEffect" asks for a move (a Cut).
pub fn effect_is_move(effect: u32) -> bool {
    effect & EFFECT_MOVE != 0 && effect & 1 == 0
}

// ----- Clipboard ------------------------------------------------------------

/// The bytes of a `CF_HDROP` block: a `DROPFILES` header, then each path as
/// UTF-16 with a terminating NUL, and one more NUL at the end.
pub fn encode_hdrop(paths: &[String]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(DROPFILES_SIZE + paths.len() * 64);
    bytes.extend_from_slice(&(DROPFILES_SIZE as u32).to_le_bytes()); // pFiles
    bytes.extend_from_slice(&0i32.to_le_bytes()); // pt.x
    bytes.extend_from_slice(&0i32.to_le_bytes()); // pt.y
    bytes.extend_from_slice(&0i32.to_le_bytes()); // fNC
    bytes.extend_from_slice(&1i32.to_le_bytes()); // fWide
    for path in paths {
        for unit in path.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0u16.to_le_bytes());
    }
    bytes.extend_from_slice(&0u16.to_le_bytes());
    if paths.is_empty() {
        // An empty list still ends in two NULs.
        bytes.extend_from_slice(&0u16.to_le_bytes());
    }
    bytes
}

/// Reads the paths out of a `CF_HDROP` block, wide or not. The app reads
/// the clipboard with `DragQueryFileW`; tests use this to check the bytes.
#[cfg(test)]
pub fn decode_hdrop(bytes: &[u8]) -> Vec<String> {
    let read_u32 = |at: usize| {
        bytes
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let (Some(start), Some(wide)) = (read_u32(0), read_u32(16)) else {
        return Vec::new();
    };
    let Some(list) = bytes.get(start as usize..) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    if wide != 0 {
        let units: Vec<u16> = list
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_le_bytes(pair))
            .collect();
        for part in units.split(|&unit| unit == 0) {
            if part.is_empty() {
                break;
            }
            paths.push(String::from_utf16_lossy(part));
        }
    } else {
        for part in list.split(|&byte| byte == 0) {
            if part.is_empty() {
                break;
            }
            paths.push(String::from_utf8_lossy(part).to_string());
        }
    }
    paths
}

/// What Copy or Cut left on the clipboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardFiles {
    pub paths: Vec<String>,
    /// Cut rather than copied: pasting moves them.
    pub cut: bool,
}

fn preferred_effect_format() -> u32 {
    unsafe { RegisterClipboardFormatW(windows::core::w!("Preferred DropEffect")) }
}

unsafe fn global_bytes(bytes: &[u8]) -> Result<windows::Win32::Foundation::HGLOBAL, String> {
    let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))
        .map_err(|error| format!("GlobalAlloc failed: {error}"))?;
    let target = GlobalLock(memory);
    if target.is_null() {
        let _ = GlobalFree(Some(memory));
        return Err("GlobalLock failed".into());
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), target as *mut u8, bytes.len());
    let _ = GlobalUnlock(memory);
    Ok(memory)
}

/// Puts files on the clipboard for Explorer (and anything else) to paste,
/// with their paths as text too so pasting into a text box gives the path.
pub fn set_clipboard_files(
    owner: windows_sys::Win32::Foundation::HWND,
    paths: &[String],
    cut: bool,
) -> Result<(), String> {
    let effect = if cut { EFFECT_MOVE } else { EFFECT_COPY };
    let text: Vec<u16> = paths
        .join("\r\n")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let text_bytes: Vec<u8> = text.iter().flat_map(|unit| unit.to_le_bytes()).collect();
    let blocks = [
        (u32::from(CF_HDROP.0), encode_hdrop(paths)),
        (preferred_effect_format(), effect.to_le_bytes().to_vec()),
        (u32::from(CF_UNICODETEXT.0), text_bytes),
    ];
    unsafe {
        OpenClipboard(Some(HWND(owner)))
            .map_err(|_| "Could not open the clipboard.".to_string())?;
        let _ = EmptyClipboard();
        let mut result = Ok(());
        for (format, bytes) in blocks {
            match global_bytes(&bytes) {
                Ok(memory) => {
                    if SetClipboardData(format, Some(HANDLE(memory.0))).is_err() {
                        let _ = GlobalFree(Some(memory));
                        result = Err("Could not put the files on the clipboard.".to_string());
                    }
                }
                Err(error) => result = Err(error),
            }
        }
        let _ = CloseClipboard();
        result
    }
}

/// The files on the clipboard, if it holds any.
pub fn clipboard_files(owner: windows_sys::Win32::Foundation::HWND) -> Option<ClipboardFiles> {
    unsafe {
        IsClipboardFormatAvailable(u32::from(CF_HDROP.0)).ok()?;
        OpenClipboard(Some(HWND(owner))).ok()?;
        let read = (|| {
            let handle = GetClipboardData(u32::from(CF_HDROP.0)).ok()?;
            let drop = HDROP(handle.0);
            let count = DragQueryFileW(drop, u32::MAX, None);
            let mut paths = Vec::with_capacity(count as usize);
            for index in 0..count {
                let len = DragQueryFileW(drop, index, None) as usize;
                let mut buffer = vec![0u16; len + 1];
                let copied = DragQueryFileW(drop, index, Some(&mut buffer)) as usize;
                paths.push(String::from_utf16_lossy(&buffer[..copied.min(len)]));
            }
            let mut cut = false;
            if let Ok(handle) = GetClipboardData(preferred_effect_format()) {
                let memory = windows::Win32::Foundation::HGLOBAL(handle.0);
                let pointer = GlobalLock(memory) as *const u8;
                if !pointer.is_null() {
                    if GlobalSize(memory) >= 4 {
                        let effect = std::ptr::read_unaligned(pointer as *const u32);
                        cut = effect_is_move(effect);
                    }
                    let _ = GlobalUnlock(memory);
                }
            }
            (!paths.is_empty()).then_some(ClipboardFiles { paths, cut })
        })();
        let _ = CloseClipboard();
        read
    }
}

/// Empties the clipboard after cut files were moved, as Explorer does, so
/// they cannot be pasted a second time from where they no longer are.
pub fn clear_clipboard(owner: windows_sys::Win32::Foundation::HWND) {
    unsafe {
        if OpenClipboard(Some(HWND(owner))).is_ok() {
            let _ = EmptyClipboard();
            let _ = CloseClipboard();
        }
    }
}

// ----- Shell file operations ---------------------------------------------

/// How a shell operation ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// The person cancelled it, or skipped some items, in Windows' dialogs.
    Aborted,
}

/// Who a file operation's dialogs belong to, and whether to show them.
#[derive(Clone, Copy, Debug)]
pub struct Shell {
    pub owner: windows_sys::Win32::Foundation::HWND,
    /// No progress, confirmation, or error dialogs (tests).
    pub silent: bool,
}

impl Shell {
    pub fn new(owner: windows_sys::Win32::Foundation::HWND) -> Self {
        Self {
            owner,
            silent: false,
        }
    }

    unsafe fn operation(&self, flags: FILEOPERATION_FLAGS) -> Result<IFileOperation, String> {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let operation: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)
            .map_err(|error| format!("could not start the file operation: {error}"))?;
        let mut flags = flags;
        if self.silent {
            flags = FILEOPERATION_FLAGS(
                flags.0 | FOF_SILENT.0 | FOF_NOCONFIRMATION.0 | FOF_NOERRORUI.0,
            );
        }
        operation
            .SetOperationFlags(flags)
            .map_err(|error| error.message())?;
        if !self.owner.is_null() {
            let _ = operation.SetOwnerWindow(HWND(self.owner));
        }
        Ok(operation)
    }

    unsafe fn run(&self, operation: &IFileOperation) -> Result<Outcome, String> {
        let performed = operation.PerformOperations();
        let aborted = operation
            .GetAnyOperationsAborted()
            .map(|aborted| aborted.as_bool())
            .unwrap_or(false);
        match performed {
            Ok(()) if !aborted => Ok(Outcome::Done),
            // Cancelling in Windows' dialog is not an error to report.
            _ if aborted => Ok(Outcome::Aborted),
            Err(error) => Err(error.message()),
            Ok(()) => Ok(Outcome::Done),
        }
    }

    /// Renames one file or folder in place. Windows asks what to do if the
    /// new name is taken.
    pub fn rename(&self, path: &str, new_name: &str) -> Result<Outcome, String> {
        unsafe {
            let operation = self.operation(FOF_ALLOWUNDO)?;
            let item = shell_item(path)?;
            operation
                .RenameItem(
                    &item,
                    &HSTRING::from(new_name),
                    None::<&IFileOperationProgressSink>,
                )
                .map_err(|error| error.message())?;
            self.run(&operation)
        }
    }

    /// Moves items to the Recycle Bin. Asks first only when the person
    /// kept Explorer's "Display delete confirmation dialog" setting on.
    /// Where there is no Recycle Bin (network shares, many USB drives) or an
    /// item is too big for it, Windows warns that it will be deleted for
    /// good, even with confirmations off, as Explorer does.
    pub fn recycle(&self, paths: &[String]) -> Result<Outcome, String> {
        let mut flags = FOF_ALLOWUNDO.0 | FOFX_RECYCLEONDELETE.0 | FOF_WANTNUKEWARNING.0;
        if !confirms_recycling() {
            flags |= FOF_NOCONFIRMATION.0;
        }
        unsafe {
            let operation = self.operation(FILEOPERATION_FLAGS(flags))?;
            let items = shell_items(paths)?;
            operation
                .DeleteItems(&items)
                .map_err(|error| error.message())?;
            self.run(&operation)
        }
    }

    /// Deletes items for good, without the Recycle Bin. The caller asks
    /// first with [`confirm_permanent_delete`].
    pub fn delete_permanently(&self, paths: &[String]) -> Result<Outcome, String> {
        unsafe {
            let operation = self.operation(FOF_NOCONFIRMATION)?;
            let items = shell_items(paths)?;
            operation
                .DeleteItems(&items)
                .map_err(|error| error.message())?;
            self.run(&operation)
        }
    }

    /// Copies or moves items into `folder`. Windows shows its progress and
    /// asks about name conflicts. Copying into the folder the items are
    /// already in makes "name - Copy" files, as Explorer does.
    pub fn paste(&self, paths: &[String], folder: &str, cut: bool) -> Result<Outcome, String> {
        let same_folder = paths.iter().all(|path| {
            crate::text::parent_path(path).eq_ignore_ascii_case(folder.trim_end_matches('\\'))
        });
        if cut && same_folder {
            // Moving things to where they already are does nothing.
            return Ok(Outcome::Done);
        }
        let mut flags = FOF_ALLOWUNDO.0;
        if !cut && same_folder {
            flags |= FOF_RENAMEONCOLLISION.0;
        }
        unsafe {
            let operation = self.operation(FILEOPERATION_FLAGS(flags))?;
            let items = shell_items(paths)?;
            let destination = shell_item(folder)?;
            if cut {
                operation.MoveItems(&items, &destination)
            } else {
                operation.CopyItems(&items, &destination)
            }
            .map_err(|error| error.message())?;
            self.run(&operation)
        }
    }
}

/// Asks before deleting for good. True to go ahead.
pub fn confirm_permanent_delete(
    owner: windows_sys::Win32::Foundation::HWND,
    paths: &[String],
) -> bool {
    let message = match paths {
        [one] => format!(
            "Permanently delete \u{201c}{}\u{201d}?\n\nIt will not go to the Recycle Bin. This can't be undone.",
            one.rsplit(['\\', '/']).next().unwrap_or(one)
        ),
        many => format!(
            "Permanently delete these {} items?\n\nThey will not go to the Recycle Bin. This can't be undone.",
            many.len()
        ),
    };
    let answer = unsafe {
        MessageBoxW(
            Some(HWND(owner)),
            &HSTRING::from(message),
            windows::core::w!("Delete permanently"),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
        )
    };
    answer == IDYES
}

/// Explorer's "Display delete confirmation dialog" setting.
fn confirms_recycling() -> bool {
    let mut state = SHELLSTATEA::default();
    unsafe {
        SHGetSetSettings(Some(&mut state), SSF_NOCONFIRMRECYCLE, false);
    }
    // fNoConfirmRecycle is the third bit of the first bitfield.
    state._bitfield1 & (1 << 2) == 0
}

unsafe fn shell_item(path: &str) -> Result<IShellItem, String> {
    SHCreateItemFromParsingName(&HSTRING::from(path), None)
        .map_err(|error| format!("{path}: {}", error.message()))
}

unsafe fn shell_items(paths: &[String]) -> Result<IShellItemArray, String> {
    let mut lists: Vec<*mut ITEMIDLIST> = Vec::with_capacity(paths.len());
    let mut failure = None;
    for path in paths {
        let mut list: *mut ITEMIDLIST = std::ptr::null_mut();
        match SHParseDisplayName(&HSTRING::from(path.as_str()), None, &mut list, 0, None) {
            Ok(()) => lists.push(list),
            Err(error) => {
                failure = Some(format!("{path}: {}", error.message()));
                break;
            }
        }
    }
    let result = match failure {
        Some(error) => Err(error),
        None => {
            let borrowed: Vec<*const ITEMIDLIST> = lists
                .iter()
                .map(|&list| list as *const ITEMIDLIST)
                .collect();
            SHCreateShellItemArrayFromIDLists(&borrowed).map_err(|error| error.message())
        }
    };
    for list in lists {
        CoTaskMemFree(Some(list as *const c_void));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn rename_selects_the_name_without_its_extension() {
        assert_eq!(rename_selection(&wide("report.final.pdf"), false), 12);
        assert_eq!(rename_selection(&wide("README"), false), 6);
        assert_eq!(rename_selection(&wide(".gitignore"), false), 10);
        assert_eq!(rename_selection(&wide("photos.2024"), true), 11);
    }

    #[test]
    fn new_names_are_checked_like_windows_does() {
        assert_eq!(validate_new_name("  notes.txt "), Ok("notes.txt".into()));
        assert_eq!(validate_new_name("draft..."), Ok("draft".into()));
        assert!(validate_new_name("").is_err());
        assert!(validate_new_name("   ").is_err());
        assert!(validate_new_name("...").is_err());
        assert!(validate_new_name("a/b").is_err());
        assert!(validate_new_name("what?").is_err());
        assert!(validate_new_name("tab\there").is_err());
        assert!(validate_new_name("CON").is_err());
        assert!(validate_new_name("con.txt").is_err());
        assert!(validate_new_name("LPT1.log").is_err());
        assert!(validate_new_name("COM0").is_ok());
        assert!(validate_new_name("console.txt").is_ok());
        assert!(validate_new_name(&"x".repeat(256)).is_err());
        assert!(validate_new_name("\u{00e9}t\u{00e9} \u{2014} final.docx").is_ok());
    }

    #[test]
    fn pasting_goes_into_the_selected_folder_or_the_files_folder() {
        assert_eq!(paste_folder(r"C:\Data\Photos", true), r"C:\Data\Photos");
        assert_eq!(paste_folder(r"C:\Data\notes.txt", false), r"C:\Data");
    }

    #[test]
    fn drop_effects_tell_cut_from_copy() {
        assert!(effect_is_move(EFFECT_MOVE));
        assert!(!effect_is_move(EFFECT_COPY));
        assert!(!effect_is_move(1));
        assert!(!effect_is_move(3));
    }

    #[test]
    fn file_lists_round_trip_through_the_hdrop_format() {
        let paths = vec![
            r"C:\Data\notes.txt".to_string(),
            "D:\\\u{00e9}t\u{00e9}\\photo \u{1f600}.jpg".to_string(),
            r"\\server\share\folder".to_string(),
        ];
        let bytes = encode_hdrop(&paths);
        assert_eq!(&bytes[0..4], &20u32.to_le_bytes());
        assert_eq!(&bytes[16..20], &1u32.to_le_bytes());
        assert_eq!(&bytes[bytes.len() - 4..], &[0, 0, 0, 0]);
        assert_eq!(decode_hdrop(&bytes), paths);
        assert!(decode_hdrop(&encode_hdrop(&[])).is_empty());
        assert!(decode_hdrop(&[1, 2, 3]).is_empty());

        // Windows' own reader agrees, without going near the real clipboard.
        unsafe {
            let memory = global_bytes(&bytes).expect("memory");
            let drop = HDROP(memory.0);
            assert_eq!(DragQueryFileW(drop, u32::MAX, None) as usize, paths.len());
            for (index, path) in paths.iter().enumerate() {
                let len = DragQueryFileW(drop, index as u32, None) as usize;
                let mut buffer = vec![0u16; len + 1];
                DragQueryFileW(drop, index as u32, Some(&mut buffer));
                assert_eq!(String::from_utf16_lossy(&buffer[..len]), *path);
            }
            let _ = GlobalFree(Some(memory));
        }

        // An ANSI list from an old program.
        let mut ansi = bytes[..20].to_vec();
        ansi[16..20].copy_from_slice(&0u32.to_le_bytes());
        ansi.extend_from_slice(b"C:\\a.txt\0C:\\b.txt\0\0");
        assert_eq!(decode_hdrop(&ansi), vec![r"C:\a.txt", r"C:\b.txt"]);
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "feather-fileops-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn shell_operations_rename_copy_move_and_delete() {
        let dir = temp_dir("shell");
        let shell = Shell {
            owner: std::ptr::null_mut(),
            silent: true,
        };
        let from = dir.join("from");
        let to = dir.join("to");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        let file = from.join("draft.txt");
        std::fs::write(&file, b"hello").unwrap();
        let text = |path: &std::path::Path| path.to_string_lossy().to_string();

        assert_eq!(shell.rename(&text(&file), "final.txt"), Ok(Outcome::Done));
        let renamed = from.join("final.txt");
        assert!(renamed.is_file() && !file.exists());

        let pasted = vec![text(&renamed)];
        assert_eq!(shell.paste(&pasted, &text(&to), false), Ok(Outcome::Done));
        assert!(to.join("final.txt").is_file() && renamed.is_file());
        // Copying into the same folder makes a second copy beside it.
        assert_eq!(shell.paste(&pasted, &text(&from), false), Ok(Outcome::Done));
        assert_eq!(std::fs::read_dir(&from).unwrap().count(), 2);
        // Cut and paste moves.
        let moved = vec![text(&to.join("final.txt"))];
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(shell.paste(&moved, &text(&sub), true), Ok(Outcome::Done));
        assert!(sub.join("final.txt").is_file() && !to.join("final.txt").exists());

        assert_eq!(shell.delete_permanently(&[text(&sub)]), Ok(Outcome::Done));
        assert!(!sub.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
