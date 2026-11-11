//! Getting Direct2D frames onto a window, and into PNG files.
//!
//! Windows draw into an off-screen 32-bit bitmap with a DC render target,
//! then copy it to the window. Copying a premultiplied 32-bit bitmap keeps
//! its alpha, so translucent pixels show the Mica or Acrylic backdrop.

use std::mem::size_of;
use std::path::Path;

use windows::core::Interface;
use windows::Win32::Graphics::Direct2D::{ID2D1DCRenderTarget, ID2D1Factory, ID2D1RenderTarget};
use windows::Win32::Graphics::Imaging::{
    GUID_WICPixelFormat32bppPBGRA, IWICImagingFactory, WICBitmapCacheOnLoad, WICBitmapLockRead,
    WICRect,
};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
};

use super::gfx;
use super::screenshot;

/// `D2DERR_RECREATE_TARGET`: the device was lost and the target must be rebuilt.
pub const D2DERR_RECREATE_TARGET: windows::core::HRESULT =
    windows::core::HRESULT(0x8899_000c_u32 as i32);

/// A top-down 32-bit DIB selected into a memory DC.
pub struct Surface {
    pub dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    pub width: i32,
    pub height: i32,
}

impl Surface {
    pub fn new(width: i32, height: i32) -> Option<Self> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        unsafe {
            let dc = CreateCompatibleDC(std::ptr::null_mut());
            if dc.is_null() {
                return None;
            }
            let mut bits = std::ptr::null_mut();
            let bitmap = CreateDIBSection(
                dc,
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            );
            if bitmap.is_null() {
                DeleteDC(dc);
                return None;
            }
            let old = SelectObject(dc, bitmap as HGDIOBJ);
            Some(Self {
                dc,
                bitmap,
                old,
                width,
                height,
            })
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bitmap as HGDIOBJ);
            DeleteDC(self.dc);
        }
    }
}

/// The render target and bitmap behind one window.
#[derive(Default)]
pub struct Canvas {
    target: Option<ID2D1DCRenderTarget>,
    surface: Option<Surface>,
}

pub enum Presented {
    Done,
    /// The device was lost; the caller should repaint and reset any
    /// device-dependent resources.
    Lost,
    Failed,
}

impl Canvas {
    /// Draws a frame with `draw` and copies it to `hdc`. The bitmap is freed
    /// afterwards unless `keep` is set (while animating, every frame needs it).
    pub fn present(
        &mut self,
        d2d: &ID2D1Factory,
        hdc: HDC,
        width: i32,
        height: i32,
        keep: bool,
        draw: impl FnOnce(&ID2D1RenderTarget) -> windows::core::Result<()>,
    ) -> Presented {
        if width <= 0 || height <= 0 {
            return Presented::Failed;
        }
        let fits = self
            .surface
            .as_ref()
            .is_some_and(|surface| surface.width == width && surface.height == height);
        if !fits {
            self.surface = None;
            match Surface::new(width, height) {
                Some(surface) => self.surface = Some(surface),
                None => return Presented::Failed,
            }
        }
        if self.target.is_none() {
            match unsafe { d2d.CreateDCRenderTarget(&gfx::render_target_properties()) } {
                Ok(target) => self.target = Some(target),
                Err(_) => return Presented::Failed,
            }
        }
        let (Some(target), Some(surface)) = (self.target.clone(), self.surface.as_ref()) else {
            return Presented::Failed;
        };
        let memory = surface.dc;
        let bound = unsafe {
            target.BindDC(
                windows::Win32::Graphics::Gdi::HDC(memory),
                &windows::Win32::Foundation::RECT {
                    left: 0,
                    top: 0,
                    right: width,
                    bottom: height,
                },
            )
        };
        if bound.is_err() {
            return Presented::Failed;
        }
        let Ok(rt) = target.cast::<ID2D1RenderTarget>() else {
            return Presented::Failed;
        };
        match draw(&rt) {
            Ok(()) => {
                unsafe {
                    BitBlt(hdc, 0, 0, width, height, memory, 0, 0, SRCCOPY);
                }
                if !keep {
                    self.surface = None;
                }
                Presented::Done
            }
            Err(error) if error.code() == D2DERR_RECREATE_TARGET => {
                self.target = None;
                Presented::Lost
            }
            Err(_) => Presented::Failed,
        }
    }
}

/// Renders a frame into a PNG with an opaque background. A live window
/// composites with the desktop backdrop, which a screenshot of its client
/// area cannot capture faithfully.
pub fn capture_png(
    d2d: &ID2D1Factory,
    wic: &IWICImagingFactory,
    width: u32,
    height: u32,
    path: &Path,
    draw: impl FnOnce(&ID2D1RenderTarget) -> windows::core::Result<()>,
) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("window has no client area".into());
    }
    unsafe {
        let bitmap = wic
            .CreateBitmap(
                width,
                height,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapCacheOnLoad,
            )
            .map_err(|error| error.to_string())?;
        let rt = d2d
            .CreateWicBitmapRenderTarget(&bitmap, &gfx::render_target_properties())
            .map_err(|error| error.to_string())?;
        draw(&rt).map_err(|error| error.to_string())?;
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
            .map_err(|error| error.to_string())?;
        let stride = lock.GetStride().map_err(|error| error.to_string())? as usize;
        let mut size = 0u32;
        let mut data = std::ptr::null_mut();
        lock.GetDataPointer(&mut size, &mut data)
            .map_err(|error| error.to_string())?;
        let source = std::slice::from_raw_parts(data, size as usize);
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height as usize {
            let row = &source[y * stride..y * stride + width as usize * 4];
            for chunk in row.as_chunks::<4>().0 {
                rgba.extend_from_slice(&[chunk[2], chunk[1], chunk[0], 255]);
            }
        }
        drop(lock);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        screenshot::write_png(path, width, height, &rgba)
    }
}
