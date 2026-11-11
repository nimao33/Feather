//! Windows file-type icons, looked up by extension and cached as bitmaps.
//!
//! Icons come from the system image list through `SHGetFileInfoW` with
//! `SHGFI_USEFILEATTRIBUTES`, so a lookup never touches the disk. That means
//! every `.exe` shares the generic program icon instead of its own; the
//! tradeoff keeps scrolling through 50,000 rows cheap.

use std::collections::HashMap;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Graphics::Direct2D::{ID2D1Bitmap, ID2D1RenderTarget};
use windows::Win32::Graphics::Imaging::{
    GUID_WICPixelFormat32bppPBGRA, IWICBitmap, IWICImagingFactory, WICBitmapCacheOnLoad,
    WICBitmapDitherTypeNone, WICBitmapLockRead, WICBitmapPaletteTypeMedianCut, WICRect,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES,
};
use windows::Win32::UI::Controls::{IImageList, ILD_TRANSPARENT};
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHGetImageList, SHFILEINFOW, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES,
    SHIL_JUMBO, SHIL_LARGE, SHIL_SMALL,
};
use windows::Win32::UI::WindowsAndMessaging::DestroyIcon;

use super::gfx::Rect;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IconSize {
    /// 16 px, for the sidebar.
    Small,
    /// 32 px, for result rows.
    Large,
    /// Up to 256 px, for the details pane.
    Jumbo,
}

impl IconSize {
    fn list(self) -> i32 {
        (match self {
            Self::Small => SHIL_SMALL,
            Self::Large => SHIL_LARGE,
            Self::Jumbo => SHIL_JUMBO,
        }) as i32
    }
}

struct Source {
    bitmap: IWICBitmap,
    /// The non-transparent part of the icon. Jumbo icons of types without
    /// a 256 px image are a small picture in the corner of a large frame.
    content: Rect,
}

/// The index that stands for Feather's own icon.
pub const APP_ICON: i32 = -2;

#[derive(Default)]
pub struct IconCache {
    indexes: HashMap<(String, bool), i32>,
    sources: HashMap<(IconSize, i32), Option<Source>>,
    bitmaps: HashMap<(IconSize, i32), ID2D1Bitmap>,
}

/// What a caller needs to draw one icon.
pub struct IconBitmap {
    pub bitmap: ID2D1Bitmap,
    pub content: Rect,
}

impl IconCache {
    /// Drops device bitmaps after the render target is recreated.
    pub fn reset_device(&mut self) {
        self.bitmaps.clear();
    }

    /// The system image list index for a name, keyed by its extension.
    pub fn index_for(&mut self, name: &str, is_dir: bool) -> i32 {
        let extension = if is_dir {
            String::new()
        } else {
            name.rsplit_once('.')
                .map(|(_, extension)| extension.to_ascii_lowercase())
                .unwrap_or_default()
        };
        if let Some(&index) = self.indexes.get(&(extension.clone(), is_dir)) {
            return index;
        }
        let probe = if is_dir {
            "folder".to_string()
        } else if extension.is_empty() {
            "file".to_string()
        } else {
            format!("file.{extension}")
        };
        let attributes = if is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        let index = system_icon_index(&probe, attributes, true).unwrap_or(-1);
        self.indexes.insert((extension, is_dir), index);
        index
    }

    /// The icon for a drive root such as `C:`, which needs a real lookup so
    /// Windows can return the drive's own icon. Other scopes use the folder
    /// icon.
    pub fn index_for_scope(&mut self, scope: &str) -> i32 {
        let trimmed = scope.trim_end_matches('\\');
        let is_drive = trimmed.len() == 2 && trimmed.ends_with(':');
        if !is_drive {
            return self.index_for("", true);
        }
        let key = (format!("drive:{trimmed}"), true);
        if let Some(&index) = self.indexes.get(&key) {
            return index;
        }
        let index = system_icon_index(&format!("{trimmed}\\"), FILE_FLAGS_AND_ATTRIBUTES(0), false)
            .unwrap_or_else(|| self.index_for("", true));
        self.indexes.insert(key, index);
        index
    }

    /// Returns a device bitmap for the icon, creating and caching it on first
    /// use. `cache` is false for one-off targets such as screenshots.
    pub fn bitmap(
        &mut self,
        wic: &IWICImagingFactory,
        rt: &ID2D1RenderTarget,
        index: i32,
        size: IconSize,
        cache: bool,
    ) -> Option<IconBitmap> {
        if index < 0 && index != APP_ICON {
            return None;
        }
        let key = (size, index);
        let source = self.sources.entry(key).or_insert_with(|| {
            if index == APP_ICON {
                load_app_source(wic)
            } else {
                load_source(wic, index, size)
            }
        });
        let source = source.as_ref()?;
        if cache {
            if let Some(bitmap) = self.bitmaps.get(&key) {
                return Some(IconBitmap {
                    bitmap: bitmap.clone(),
                    content: source.content,
                });
            }
        }
        let bitmap = unsafe { rt.CreateBitmapFromWicBitmap(&source.bitmap, None).ok()? };
        if cache {
            self.bitmaps.insert(key, bitmap.clone());
        }
        Some(IconBitmap {
            bitmap,
            content: source.content,
        })
    }
}

fn system_icon_index(
    probe: &str,
    attributes: FILE_FLAGS_AND_ATTRIBUTES,
    by_attributes: bool,
) -> Option<i32> {
    let mut info = SHFILEINFOW::default();
    let path = HSTRING::from(probe);
    let result = unsafe {
        SHGetFileInfoW(
            PCWSTR(path.as_ptr()),
            attributes,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            if by_attributes {
                SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES
            } else {
                SHGFI_SYSICONINDEX
            },
        )
    };
    if result == 0 {
        None
    } else {
        Some(info.iIcon)
    }
}

fn load_source(wic: &IWICImagingFactory, index: i32, size: IconSize) -> Option<Source> {
    unsafe {
        let list: IImageList = SHGetImageList(size.list()).ok()?;
        let icon = list.GetIcon(index, ILD_TRANSPARENT.0).ok()?;
        let raw = wic.CreateBitmapFromHICON(icon);
        let _ = DestroyIcon(icon);
        convert(wic, raw.ok()?)
    }
}

/// Feather's icon from the executable's resources, at 64 px so it stays
/// sharp when drawn smaller.
fn load_app_source(wic: &IWICImagingFactory) -> Option<Source> {
    let icon = super::tray::app_icon(64);
    if icon.is_null() {
        return None;
    }
    unsafe {
        let raw = wic.CreateBitmapFromHICON(windows::Win32::UI::WindowsAndMessaging::HICON(icon));
        convert(wic, raw.ok()?)
    }
}

fn convert(wic: &IWICImagingFactory, raw: IWICBitmap) -> Option<Source> {
    unsafe {
        let converter = wic.CreateFormatConverter().ok()?;
        converter
            .Initialize(
                &raw,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeMedianCut,
            )
            .ok()?;
        let bitmap = wic
            .CreateBitmapFromSource(&converter, WICBitmapCacheOnLoad)
            .ok()?;
        let content = content_bounds(&bitmap).unwrap_or_default();
        Some(Source { bitmap, content })
    }
}

/// Finds the bounding box of pixels with any alpha.
fn content_bounds(bitmap: &IWICBitmap) -> Option<Rect> {
    unsafe {
        let mut width = 0u32;
        let mut height = 0u32;
        bitmap.GetSize(&mut width, &mut height).ok()?;
        let lock = bitmap
            .Lock(
                &WICRect {
                    X: 0,
                    Y: 0,
                    Width: width as i32,
                    Height: height as i32,
                },
                WICBitmapLockRead.0 as u32,
            )
            .ok()?;
        let stride = lock.GetStride().ok()? as usize;
        let mut size = 0u32;
        let mut data = std::ptr::null_mut();
        lock.GetDataPointer(&mut size, &mut data).ok()?;
        if data.is_null() {
            return None;
        }
        let pixels = std::slice::from_raw_parts(data, size as usize);
        Some(
            alpha_bounds(pixels, width as usize, height as usize, stride).unwrap_or(Rect::new(
                0.0,
                0.0,
                width as f32,
                height as f32,
            )),
        )
    }
}

/// The bounding box of non-transparent BGRA pixels, or `None` when the image
/// is fully transparent.
pub fn alpha_bounds(pixels: &[u8], width: usize, height: usize, stride: usize) -> Option<Rect> {
    let (mut left, mut top, mut right, mut bottom) = (width, height, 0usize, 0usize);
    for y in 0..height {
        let row = pixels.get(y * stride..y * stride + width * 4)?;
        for x in 0..width {
            if row[x * 4 + 3] > 8 {
                left = left.min(x);
                right = right.max(x + 1);
                top = top.min(y);
                bottom = bottom.max(y + 1);
            }
        }
    }
    if right <= left || bottom <= top {
        return None;
    }
    Some(Rect::new(
        left as f32,
        top as f32,
        right as f32,
        bottom as f32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_bounds_finds_the_visible_area() {
        let (width, height) = (4usize, 3usize);
        let mut pixels = vec![0u8; width * height * 4];
        pixels[(width + 1) * 4 + 3] = 255;
        pixels[(2 * width + 2) * 4 + 3] = 255;
        let bounds = alpha_bounds(&pixels, width, height, width * 4).unwrap();
        assert_eq!(bounds, Rect::new(1.0, 1.0, 3.0, 3.0));
        assert!(alpha_bounds(&[0u8; 16], 2, 2, 8).is_none());
    }
}
