//! The modern shell folder picker used by the search-scope row.

use std::ffi::c_void;

use windows::core::{Error, HRESULT};
use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
    SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

/// Shows the modern folder picker. Returns `None` when the user cancels.
pub fn pick_folder(parent: windows_sys::Win32::Foundation::HWND) -> Result<Option<String>, String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| format!("could not create the folder picker: {error}"))?;
        dialog
            .SetOptions(FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
            .map_err(|error| format!("could not configure the folder picker: {error}"))?;

        let owner = if parent.is_null() {
            GetForegroundWindow()
        } else {
            HWND(parent)
        };
        match dialog.Show(Some(owner)) {
            Ok(()) => {}
            Err(error) if is_cancelled(&error) => return Ok(None),
            Err(error) => return Err(format!("folder picker failed: {error}")),
        }

        let item = dialog
            .GetResult()
            .map_err(|error| format!("could not read the chosen folder: {error}"))?;
        let name = item
            .GetDisplayName(SIGDN_FILESYSPATH)
            .map_err(|error| format!("could not read the folder path: {error}"))?;
        let path = name
            .to_string()
            .map_err(|error| format!("the folder path is not valid Unicode: {error}"));
        CoTaskMemFree(Some(name.0 as *const c_void));
        let path = path?;
        if path.is_empty() {
            Ok(None)
        } else {
            Ok(Some(path))
        }
    }
}

/// Shows the file picker for a picture. Returns `None` when cancelled.
pub fn pick_image(parent: windows_sys::Win32::Foundation::HWND) -> Result<Option<String>, String> {
    use windows::core::w;
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| format!("could not create the file picker: {error}"))?;
        let filters = [
            COMDLG_FILTERSPEC {
                pszName: w!("Pictures"),
                pszSpec: w!("*.png;*.jpg;*.jpeg;*.bmp;*.gif;*.webp;*.tif;*.tiff;*.heic"),
            },
            COMDLG_FILTERSPEC {
                pszName: w!("All files"),
                pszSpec: w!("*.*"),
            },
        ];
        let _ = dialog.SetFileTypes(&filters);
        let _ = dialog.SetTitle(w!("Choose a background picture"));
        dialog
            .SetOptions(FOS_FORCEFILESYSTEM | FOS_FILEMUSTEXIST)
            .map_err(|error| format!("could not configure the file picker: {error}"))?;
        let owner = if parent.is_null() {
            GetForegroundWindow()
        } else {
            HWND(parent)
        };
        match dialog.Show(Some(owner)) {
            Ok(()) => {}
            Err(error) if is_cancelled(&error) => return Ok(None),
            Err(error) => return Err(format!("file picker failed: {error}")),
        }
        let item = dialog
            .GetResult()
            .map_err(|error| format!("could not read the chosen file: {error}"))?;
        let name = item
            .GetDisplayName(SIGDN_FILESYSPATH)
            .map_err(|error| format!("could not read the file path: {error}"))?;
        let path = name
            .to_string()
            .map_err(|error| format!("the file path is not valid Unicode: {error}"));
        CoTaskMemFree(Some(name.0 as *const c_void));
        let path = path?;
        Ok((!path.is_empty()).then_some(path))
    }
}

fn is_cancelled(error: &Error) -> bool {
    error.code() == HRESULT::from_win32(ERROR_CANCELLED.0)
}
